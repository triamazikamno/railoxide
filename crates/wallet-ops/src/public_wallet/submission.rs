use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::network::TransactionBuilder as _;
use alloy::primitives::{Address, FixedBytes, TxKind, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::transports::{RpcError, TransportError, TransportErrorKind};
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool};
use eyre::{Result, WrapErr, eyre};

use super::gas::resolve_public_action_gas_fee;
use super::signer::VaultedPublicSigner;
use super::types::{
    PublicActionAttemptInfo, PublicActionCommand, PublicActionCommandReceiver,
    PublicActionGasFeeQuote, PublicActionGasFeeSelection, PublicActionGasLimitStrategy,
    PublicActionProgressStatus, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicActionSessionEvent, PublicActionSessionEventSender, PublicActionStepFeePolicy,
    PublicAssetId, PublicShieldTransactionProfile,
};
use crate::block_observer::BlockObserver;
use crate::settings::EffectiveChainGasSettings;
use crate::{
    ClassRoute, HttpContext, SelfBroadcastResolvedGasFee, TxReceiptOutput, report_chain_string,
    self_broadcast_replacement_bumped_fee,
    self_broadcast_send_raw_transaction_to_rpc_pool_with_logging,
};

pub(super) struct PublicActionStepOutcome {
    pub(super) receipt: TxReceiptOutput,
    pub(super) next_nonce: u64,
    pub(super) gas_fee: PublicActionGasFeeSelection,
}

/// A model-based funding check before estimation, plus fees reserved for a later Shield.
#[derive(Clone, Copy)]
pub(super) struct PublicShieldFundingBudget {
    pub(super) gas_units: u64,
    pub(super) remaining_shield_gas_units: u64,
    // Railway Auto still refreshes the later Shield under its captured ceiling after an
    // approval retry. Other flows carry the current approval fee into that Shield.
    pub(super) remaining_max_fee_per_gas: Option<u128>,
}

impl PublicShieldFundingBudget {
    fn remaining_reserve(self, max_fee_per_gas: u128) -> U256 {
        U256::from(self.remaining_shield_gas_units)
            * U256::from(self.remaining_max_fee_per_gas.unwrap_or(max_fee_per_gas))
    }
}

pub(super) struct PublicActionPreflight {
    tx_req: TransactionRequest,
    nonce: u64,
    gas_limit: u64,
    rpc_gas_price: u128,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    estimated_native_gas_cost: U256,
    live_native_balance: U256,
}

#[derive(Debug)]
pub(super) enum PublicActionPreflightError {
    FeeAuthorizationRequired {
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        message: String,
    },
    InsufficientShieldFunding(eyre::Report),
    Other(eyre::Report),
}

impl From<eyre::Report> for PublicActionPreflightError {
    fn from(error: eyre::Report) -> Self {
        Self::Other(error)
    }
}

impl std::fmt::Display for PublicActionPreflightError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FeeAuthorizationRequired { message, .. } => formatter.write_str(message),
            Self::InsufficientShieldFunding(error) | Self::Other(error) => {
                std::fmt::Display::fmt(error, formatter)
            }
        }
    }
}

impl std::error::Error for PublicActionPreflightError {}

impl PublicActionPreflightError {
    pub(super) fn into_report(self) -> eyre::Report {
        match self {
            Self::FeeAuthorizationRequired { message, .. } => eyre!("{}", message),
            Self::InsufficientShieldFunding(error) | Self::Other(error) => error,
        }
    }
}

#[derive(Clone)]
pub(super) struct SubmittedPublicActionAttempt {
    pub(super) tx_hash: FixedBytes<32>,
    pub(super) info: PublicActionAttemptInfo,
    rpc_gas_price: u128,
    estimated_native_gas_cost: U256,
    live_native_balance: U256,
}

/// Persists an attempt before its raw send: its hash, the signed request and the signed raw
/// transaction.
pub(super) type PublicActionHandoff<'a> =
    dyn FnMut(FixedBytes<32>, &TransactionRequest, &[u8]) -> Result<()> + Send + 'a;

/// Where a step session ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicActionStepEnd {
    /// Once a receipt of a handed-off attempt is observed.
    Receipt,
    /// As [`Self::Receipt`], except that a first attempt that was handed off but that no RPC
    /// accepted ends the session without a receipt. The durable hand-off's owner follows that
    /// attempt from there. Only a session with a hand-off ends this way.
    HandOff,
    /// As [`Self::Receipt`], except that a successful receipt doesn't complete the step: the
    /// caller does once it verified the transaction's effect.
    Evidence,
}

/// What a step session ended with.
pub(super) enum PublicActionStepResult {
    Receipt(PublicActionStepOutcome),
    /// The first attempt was handed off and no RPC accepted it, as
    /// [`PublicActionStepEnd::HandOff`] allows.
    HandedOff,
}

impl PublicActionStepResult {
    /// The receipt a [`PublicActionStepEnd::Receipt`] session ends with.
    pub(super) fn into_receipt(self) -> Result<PublicActionStepOutcome> {
        match self {
            Self::Receipt(outcome) => Ok(outcome),
            Self::HandedOff => Err(eyre!(
                "the public action was handed off without waiting for its receipt"
            )),
        }
    }
}

/// The routes of a Public action. Callers resolve them before the transaction is signed, so a
/// route without a Sensitive or Submit endpoint fails before that signature.
pub(super) struct PublicActionPools {
    /// The Sensitive route preflights and estimates read through.
    pub(super) route: ClassRoute,
    /// A pool over `route`, for preflight fallbacks and block observation.
    pub(super) observe: Arc<QueryRpcPool>,
    /// The Submit endpoints a signed transaction fans out to.
    pub(super) submit: Arc<QueryRpcPool>,
}

pub(super) fn public_action_pools(
    route: &crate::RpcChainRoute,
    http: &HttpContext,
) -> Result<PublicActionPools> {
    let sensitive = route.for_class(crate::RpcTrafficClass::Sensitive)?;
    let submit = route.for_class(crate::RpcTrafficClass::Submission)?;
    Ok(PublicActionPools {
        observe: crate::query_rpc_pool_for_route(&sensitive, http),
        submit: crate::query_rpc_pool_for_route(&submit, http),
        route: sensitive,
    })
}

/// Each preflight, including a replacement's, quotes fees from its own candidates on the
/// Sensitive route of `pools`, including its nonce, estimate and balance reads. Observation
/// continues on that class and broadcast goes to the Submit pool.
pub(super) async fn submit_public_action_step_session(
    step: PublicActionProgressStep,
    base_tx_req: TransactionRequest,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    signer: &VaultedPublicSigner,
    label: &str,
    pools: &PublicActionPools,
    finality_depth: u64,
    http: &HttpContext,
    transaction_tracking: Option<&crate::PublicTransactionTrackingContext>,
    chain_id: u64,
    from_address: Address,
    gas: &EffectiveChainGasSettings,
    funding_budget: Option<PublicShieldFundingBudget>,
    authorized_gas_limit: Option<u64>,
    mut nonce: Option<u64>,
    gas_fee: PublicActionGasFeeSelection,
    fee_policy: PublicActionStepFeePolicy,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    command_rx: &mut Option<PublicActionCommandReceiver>,
    event_tx: Option<&PublicActionSessionEventSender>,
    mut handoff: Option<&mut PublicActionHandoff<'_>>,
    end: PublicActionStepEnd,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<PublicActionStepResult> {
    let (query_rpc_pool, submit_pool, rpc_route) =
        (&pools.observe, pools.submit.as_ref(), &pools.route);
    let private_handoff = handoff.is_some();
    if end == PublicActionStepEnd::HandOff && !private_handoff {
        return Err(eyre!(
            "only a durable hand-off ends a public action at its hand-off"
        ));
    }
    let mut railway_auto = fee_policy == PublicActionStepFeePolicy::RefreshRailwayStandard;
    let mut next_gas_fee =
        public_action_step_initial_gas_fee_selection(profile, fee_policy, gas_fee);
    let authorized_gas_fee = authorized_gas_limit.map(|_| gas_fee);
    let authorized_fee_ceiling = railway_auto.then_some(authorized_fee_ceiling).flatten();
    let mut submitted_attempts = Vec::new();
    let mut observer = None;

    loop {
        progress(public_action_progress_update(
            step,
            PublicActionProgressStatus::Pending,
            None,
            None,
        ));

        let preflight = match public_action_preflight_from_rpc_pool(
            query_rpc_pool.as_ref(),
            http,
            rpc_route,
            chain_id,
            from_address,
            base_tx_req.clone(),
            next_gas_fee,
            gas,
            profile,
            asset,
            gas_limit_strategy,
            authorized_gas_limit,
            nonce,
            None,
            authorized_fee_ceiling,
            railway_auto,
            funding_budget,
        )
        .await
        {
            Ok(preflight) => preflight,
            Err(PublicActionPreflightError::FeeAuthorizationRequired {
                max_fee_per_gas,
                max_priority_fee_per_gas,
                message,
            }) => {
                progress(public_action_progress_update(
                    step,
                    PublicActionProgressStatus::Pending,
                    None,
                    Some(message.clone()),
                ));
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::FeeAuthorizationRequired {
                        step,
                        max_fee_per_gas,
                        max_priority_fee_per_gas,
                        message,
                    },
                );
                let Some(command) = recv_public_action_command(command_rx).await else {
                    return Err(eyre!(
                        "Railway fee authorization was required but no review command was received"
                    ));
                };
                railway_auto = false;
                next_gas_fee = command.gas_fee;
                continue;
            }
            Err(error) => {
                let error = error.into_report();
                let message = report_chain_string(&error);
                progress(public_action_progress_update(
                    step,
                    PublicActionProgressStatus::Error,
                    None,
                    Some(message.clone()),
                ));
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::StepFailed { step, message },
                );
                if authorized_gas_limit.is_some() {
                    return Err(error).wrap_err(
                        "advanced transaction requires a refreshed estimate and authorization",
                    );
                }
                // A prepared native shield's reviewed gas and fee bounds cannot be widened by a
                // retry; it needs fresh preparation and review on the same stealth account.
                if matches!(
                    gas_limit_strategy,
                    PublicActionGasLimitStrategy::NativeShieldPrepared { .. }
                ) {
                    return Err(error)
                        .wrap_err("native shield requires refreshed preparation and review");
                }
                let Some(command) = recv_public_action_command(command_rx).await else {
                    return Err(error);
                };
                railway_auto = false;
                next_gas_fee = command.gas_fee;
                continue;
            }
        };
        nonce = Some(preflight.nonce);

        if observer.is_none() {
            let established = BlockObserver::establish(
                Arc::clone(query_rpc_pool),
                finality_depth,
                http.rpc_broker(),
                chain_id,
            )
            .await?;
            observer = Some(
                crate::public_wallet::PublicTransactionObservationGuard::new(
                    established,
                    transaction_tracking,
                )?,
            );
        }

        if let Some(context) = transaction_tracking {
            context.ensure_open()?;
        }
        emit_public_action_event(event_tx, PublicActionSessionEvent::AttemptHandoff { step });
        let attempted_request = preflight.tx_req.clone();
        let result = submit_public_action_attempt(
            step,
            preflight,
            submit_pool,
            http.network_mode(),
            signer,
            label,
            event_tx,
            None,
            !private_handoff,
            &mut |attempt, signed| {
                if let Some(context) = transaction_tracking {
                    context.ensure_open()?;
                }
                if let Some(handoff) = handoff.as_deref_mut() {
                    handoff(attempt.tx_hash, &attempted_request, signed)?;
                }
                retain_public_action_attempt(
                    observer
                        .as_mut()
                        .expect("public action observer established"),
                    &mut submitted_attempts,
                    attempt,
                );
                Ok(())
            },
        )
        .await;
        match result {
            Ok(attempt) => progress(public_action_progress_update(
                step,
                PublicActionProgressStatus::Pending,
                Some(attempt.info.tx_hash),
                None,
            )),
            Err(
                PublicActionAttemptError::Signing(error) | PublicActionAttemptError::Sending(error),
            ) => {
                let message = report_chain_string(&error);
                progress(public_action_progress_update(
                    step,
                    PublicActionProgressStatus::Error,
                    None,
                    Some(message.clone()),
                ));
                emit_public_action_event(
                    event_tx,
                    PublicActionSessionEvent::StepFailed { step, message },
                );
                // A failed raw-send response can still mean the transaction was accepted.
                // Keep observing every handed-off attempt while accepting explicit replacements.
                if submitted_attempts.is_empty() {
                    let Some(command) = recv_public_action_command(command_rx).await else {
                        return Err(error);
                    };
                    ensure_public_action_command_gas_fee_authorized(
                        authorized_gas_fee,
                        command.gas_fee,
                    )?;
                    railway_auto = false;
                    next_gas_fee = command.gas_fee;
                    continue;
                }
                // No RPC accepted the handed-off attempt. The durable hand-off's owner
                // follows it from here rather than waiting for a receipt that may not come.
                if end == PublicActionStepEnd::HandOff {
                    return Ok(PublicActionStepResult::HandedOff);
                }
            }
        }
        loop {
            let receipt = if command_rx.is_some() {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(3)) => {
                        observer
                            .as_mut()
                            .expect("public action observer established")
                            .poll()
                            .await?
                            .receipt
                    }
                    command = recv_public_action_command(command_rx) => {
                        let Some(command) = command else {
                            *command_rx = None;
                            continue;
                        };
                        if let Err(error) = ensure_public_action_command_gas_fee_authorized(
                            authorized_gas_fee,
                            command.gas_fee,
                        ) {
                            emit_public_action_event(
                                event_tx,
                                PublicActionSessionEvent::AttemptRejected {
                                    step,
                                    message: report_chain_string(&error),
                                },
                            );
                            continue;
                        }
                        let Some(nonce) = nonce else {
                            railway_auto = false;
                            next_gas_fee = command.gas_fee;
                            break;
                        };
                        railway_auto = false;
                        let gas_limit = submitted_attempts
                            .last()
                            .map_or(0, |attempt| attempt.info.gas_limit);
                        let replacement = match public_action_preflight_from_rpc_pool(
                            query_rpc_pool.as_ref(),
                            http,
                            rpc_route,
                            chain_id,
                            from_address,
                            base_tx_req.clone(),
                            command.gas_fee,
                            gas,
                            profile,
                            asset,
                            gas_limit_strategy,
                            authorized_gas_limit,
                            Some(nonce),
                            Some(gas_limit),
                            authorized_fee_ceiling,
                            railway_auto,
                            funding_budget,
                        )
                        .await
                        {
                            Ok(preflight) => preflight,
                            Err(error) => {
                                emit_public_action_event(
                                    event_tx,
                                    PublicActionSessionEvent::AttemptRejected {
                                        step,
                                        message: error.to_string(),
                                    },
                                );
                                continue;
                            }
                        };
                        if let Some(context) = transaction_tracking {
                            context.ensure_open()?;
                        }
                        emit_public_action_event(
                            event_tx,
                            PublicActionSessionEvent::AttemptHandoff { step },
                        );
                        let replacement_request = replacement.tx_req.clone();
                        match submit_public_action_attempt(
                            step,
                            replacement,
                            submit_pool,
                            http.network_mode(),
                            signer,
                            label,
                            event_tx,
                            None,
                            !private_handoff,
                            &mut |attempt, signed| {
                                if let Some(context) = transaction_tracking {
                                    context.ensure_open()?;
                                }
                                // A replacement is handed off as its first attempt was.
                                if let Some(handoff) = handoff.as_deref_mut() {
                                    handoff(attempt.tx_hash, &replacement_request, signed)?;
                                }
                                retain_public_action_attempt(
                                    observer.as_mut().expect("public action observer established"),
                                    &mut submitted_attempts,
                                    attempt,
                                );
                                Ok(())
                            },
                        )
                        .await
                        {
                            Ok(attempt) => {
                                progress(public_action_progress_update(
                                    step,
                                    PublicActionProgressStatus::Pending,
                                    Some(attempt.info.tx_hash.clone()),
                                    None,
                                ));
                            }
                            Err(error) => emit_public_action_event(
                                event_tx,
                                PublicActionSessionEvent::AttemptRejected {
                                    step,
                                    message: error.message(),
                                },
                            ),
                        }
                        continue;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_secs(3)).await;
                observer
                    .as_mut()
                    .expect("public action observer established")
                    .poll()
                    .await?
                    .receipt
            };

            if let Some((winner_index, receipt)) = receipt {
                let winner = &submitted_attempts[winner_index];
                if !private_handoff {
                    tracing::info!(
                        step = ?step,
                        tx_hash = %receipt.tx_hash,
                        rpc_gas_price = winner.rpc_gas_price,
                        estimated_native_gas_cost = %winner.estimated_native_gas_cost,
                        live_native_balance = %winner.live_native_balance,
                        "public action receipt confirmed from submitted attempts"
                    );
                }
                if receipt.status {
                    if end != PublicActionStepEnd::Evidence {
                        progress(public_action_progress_update(
                            step,
                            PublicActionProgressStatus::Done,
                            Some(receipt.tx_hash.clone()),
                            None,
                        ));
                    }
                } else {
                    let message = "Transaction reverted".to_string();
                    progress(public_action_progress_update(
                        step,
                        PublicActionProgressStatus::Error,
                        Some(receipt.tx_hash.clone()),
                        Some(message.clone()),
                    ));
                    emit_public_action_event(
                        event_tx,
                        PublicActionSessionEvent::StepFailed { step, message },
                    );
                    let gas_fee = public_action_winner_gas_fee(&submitted_attempts, winner_index);
                    // A prepared native retry needs fresh preparation and review on the same
                    // stealth account, never a new payer nonce for the same signed payload.
                    let prepared_native = matches!(
                        gas_limit_strategy,
                        PublicActionGasLimitStrategy::NativeShieldPrepared { .. }
                    );
                    let command = if prepared_native {
                        None
                    } else {
                        recv_public_action_command(command_rx).await
                    };
                    let Some(command) = command else {
                        return Ok(PublicActionStepResult::Receipt(PublicActionStepOutcome {
                            receipt,
                            next_nonce: winner.info.nonce.saturating_add(1),
                            gas_fee,
                        }));
                    };
                    ensure_public_action_command_gas_fee_authorized(
                        authorized_gas_fee,
                        command.gas_fee,
                    )?;
                    nonce = Some(winner.info.nonce.saturating_add(1));
                    next_gas_fee = command.gas_fee;
                    submitted_attempts.clear();
                    observer = None;
                    break;
                }
                let gas_fee = public_action_winner_gas_fee(&submitted_attempts, winner_index);
                return Ok(PublicActionStepResult::Receipt(PublicActionStepOutcome {
                    receipt,
                    next_nonce: winner.info.nonce.saturating_add(1),
                    gas_fee,
                }));
            }
        }
    }
}

fn retain_public_action_attempt(
    observer: &mut BlockObserver,
    attempts: &mut Vec<SubmittedPublicActionAttempt>,
    attempt: &SubmittedPublicActionAttempt,
) {
    let attempt_id = attempts.len();
    attempts.push(attempt.clone());
    observer.register(attempt.tx_hash, attempt_id);
}

const fn public_action_winner_gas_fee(
    attempts: &[SubmittedPublicActionAttempt],
    winner_index: usize,
) -> PublicActionGasFeeSelection {
    let winner = &attempts[winner_index];
    PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: winner.info.max_fee_per_gas,
        max_priority_fee_per_gas: winner.info.max_priority_fee_per_gas,
    }
}

pub(super) fn ensure_public_action_command_gas_fee_authorized(
    authorized_gas_fee: Option<PublicActionGasFeeSelection>,
    requested_gas_fee: PublicActionGasFeeSelection,
) -> Result<()> {
    if authorized_gas_fee.is_some_and(|authorized| authorized != requested_gas_fee) {
        return Err(eyre!(
            "advanced transaction fee changed after authorization; refresh the estimate and authorize again"
        ));
    }
    Ok(())
}

pub(super) fn public_action_step_initial_gas_fee_selection(
    profile: PublicShieldTransactionProfile,
    fee_policy: PublicActionStepFeePolicy,
    authorized_fee: PublicActionGasFeeSelection,
) -> PublicActionGasFeeSelection {
    if profile == PublicShieldTransactionProfile::Railway
        && fee_policy == PublicActionStepFeePolicy::RefreshRailwayStandard
    {
        PublicActionGasFeeSelection::Auto
    } else {
        authorized_fee
    }
}

pub(super) const fn railway_auto_fee_within_authorized_ceiling(
    chain_id: u64,
    asset: PublicAssetId,
    authorized_fee: PublicActionGasFeeSelection,
    resolved_fee: &SelfBroadcastResolvedGasFee,
) -> bool {
    let PublicActionGasFeeSelection::Custom {
        max_fee_per_gas,
        max_priority_fee_per_gas,
    } = authorized_fee
    else {
        return false;
    };
    resolved_fee.max_fee_per_gas <= max_fee_per_gas
        && (PublicShieldTransactionProfile::Railway.uses_legacy_envelope(chain_id, asset)
            || resolved_fee.max_priority_fee_per_gas <= max_priority_fee_per_gas)
}

pub(super) async fn submit_public_action_attempt(
    step: PublicActionProgressStep,
    preflight: PublicActionPreflight,
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    signer: &VaultedPublicSigner,
    label: &str,
    event_tx: Option<&PublicActionSessionEventSender>,
    expiry_timestamp: Option<u64>,
    log_transaction_details: bool,
    before_broadcast: &mut (impl FnMut(&SubmittedPublicActionAttempt, &[u8]) -> Result<()> + Send),
) -> Result<SubmittedPublicActionAttempt, PublicActionAttemptError> {
    let mut handed_off_attempt = None;
    sign_send_public_action_transaction(
        query_rpc_pool,
        network_mode,
        signer,
        preflight.tx_req,
        label,
        event_tx,
        expiry_timestamp,
        log_transaction_details,
        &mut |tx_hash, signed| {
            let attempt = SubmittedPublicActionAttempt {
                tx_hash,
                info: PublicActionAttemptInfo {
                    tx_hash: alloy::hex::encode_prefixed(tx_hash),
                    nonce: preflight.nonce,
                    gas_limit: preflight.gas_limit,
                    max_fee_per_gas: preflight.max_fee_per_gas,
                    max_priority_fee_per_gas: preflight.max_priority_fee_per_gas,
                },
                rpc_gas_price: preflight.rpc_gas_price,
                estimated_native_gas_cost: preflight.estimated_native_gas_cost,
                live_native_balance: preflight.live_native_balance,
            };
            before_broadcast(&attempt, signed)?;
            handed_off_attempt = Some(attempt);
            Ok(())
        },
    )
    .await?;
    let attempt = handed_off_attempt.expect("successful broadcast has a registered attempt");
    emit_public_action_event(
        event_tx,
        PublicActionSessionEvent::AttemptSubmitted {
            step,
            attempt: attempt.info.clone(),
        },
    );
    Ok(attempt)
}

pub(super) enum PublicActionAttemptError {
    Signing(eyre::Report),
    Sending(eyre::Report),
}

impl PublicActionAttemptError {
    pub(super) fn into_report(self) -> eyre::Report {
        match self {
            Self::Signing(error) | Self::Sending(error) => error,
        }
    }

    pub(super) fn message(&self) -> String {
        match self {
            Self::Signing(error) | Self::Sending(error) => report_chain_string(error),
        }
    }
}

pub(super) async fn public_action_preflight_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    http: &HttpContext,
    rpc_route: &ClassRoute,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
    funding_budget: Option<PublicShieldFundingBudget>,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    public_action_preflight_from_rpc_pool_with_reads(
        query_rpc_pool,
        http,
        rpc_route,
        chain_id,
        from,
        base_tx_req,
        gas_fee,
        gas,
        profile,
        asset,
        gas_limit_strategy,
        authorized_gas_limit,
        nonce,
        gas_limit,
        authorized_fee_ceiling,
        railway_auto,
        None,
        funding_budget,
    )
    .await
}

/// Quotes fees, when needed, from candidates on `rpc_route` that this preflight alone owns, then
/// reads the nonce, estimate and balance on one candidate per native attempt. Admitted dapp
/// reads retain their original pool and gateway admission.
pub(super) async fn public_action_preflight_from_rpc_pool_with_reads(
    query_rpc_pool: &QueryRpcPool,
    http: &HttpContext,
    rpc_route: &ClassRoute,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
    rpc_reads: Option<&super::DappRpcReadClient>,
    funding_budget: Option<PublicShieldFundingBudget>,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    if rpc_reads.is_some() && profile != PublicShieldTransactionProfile::Railoxide {
        return Err(eyre!("admitted dapp reads require the Railoxide transaction profile").into());
    }
    let candidates = super::gas::public_fee_candidates(http, rpc_route, rpc_reads)
        .await
        .wrap_err("select public action query RPC candidates")?;
    let quote = if profile == PublicShieldTransactionProfile::Railoxide
        || (matches!(gas_fee, PublicActionGasFeeSelection::Auto) && railway_auto)
    {
        Some(
            match rpc_reads {
                Some(reads) => {
                    super::gas::public_action_gas_fee_quote_from_candidates_with_reads(
                        &candidates,
                        http.network_mode(),
                        chain_id,
                        Some(reads),
                    )
                    .await
                }
                None => {
                    super::gas::public_action_gas_fee_quote_from_candidates_with_profile(
                        &candidates,
                        http.network_mode(),
                        chain_id,
                        profile,
                        asset,
                    )
                    .await
                }
            }
            .wrap_err("fetch public action gas price")?,
        )
    } else {
        None
    };
    let mut last_error = None;
    let mut tried = HashSet::new();
    let attempts = if rpc_reads.is_some() {
        query_rpc_pool.len()
    } else {
        candidates.len()
    };
    for attempt in 1..=attempts {
        let provider_handle = if rpc_reads.is_some() {
            query_rpc_pool.random_provider()
        } else {
            candidates.next_untried(&mut tried).await
        };
        let Some(provider_handle) = provider_handle else {
            break;
        };
        let rpc_index = rpc_route
            .endpoints()
            .iter()
            .position(|endpoint| endpoint.expose_url() == &provider_handle.url);
        let endpoint = crate::http::redact_url_for_display(&provider_handle.url);
        let started = std::time::Instant::now();
        let mut stage = "fee_resolution";
        match public_action_preflight(
            provider_handle,
            chain_id,
            from,
            base_tx_req.clone(),
            gas_fee,
            quote,
            gas,
            profile,
            asset,
            gas_limit_strategy,
            authorized_gas_limit,
            nonce,
            gas_limit,
            authorized_fee_ceiling,
            railway_auto,
            rpc_reads,
            &mut stage,
            funding_budget,
        )
        .await
        {
            Ok(preflight) => return Ok(preflight),
            Err(error @ PublicActionPreflightError::FeeAuthorizationRequired { .. }) => {
                return Err(error);
            }
            Err(error) => {
                let insufficient_funding = matches!(
                    &error,
                    PublicActionPreflightError::InsufficientShieldFunding(_)
                );
                let error = error.into_report();
                if rpc_reads.is_some() {
                    if error
                        .downcast_ref::<crate::rpc_broker::RpcBrokerError>()
                        .is_some_and(super::stops_dapp_read_retries)
                    {
                        return Err(PublicActionPreflightError::Other(error));
                    }
                } else {
                    // Error messages and bodies can contain RPC credentials or transaction data.
                    let transport_error = error.downcast_ref::<TransportError>();
                    let rpc_code = transport_error
                        .and_then(RpcError::as_error_resp)
                        .map(|payload| payload.code);
                    let transport = transport_error.and_then(RpcError::as_transport_err);
                    let http_status = transport
                        .and_then(TransportErrorKind::as_http_error)
                        .map(|error| error.status);
                    let request_error = transport
                        .and_then(TransportErrorKind::as_custom)
                        .and_then(|error| error.downcast_ref::<reqwest::Error>());
                    let failure = match transport_error {
                        Some(RpcError::Transport(_)) if http_status.is_some() => "http",
                        Some(RpcError::Transport(_))
                            if request_error.is_some_and(reqwest::Error::is_timeout) =>
                        {
                            "timeout"
                        }
                        Some(RpcError::Transport(_))
                            if request_error.is_some_and(reqwest::Error::is_connect) =>
                        {
                            "connect"
                        }
                        Some(RpcError::Transport(_)) => "transport",
                        Some(RpcError::NullResp) => "missing_response",
                        Some(RpcError::DeserError { .. }) => "decode",
                        Some(RpcError::ErrorResp(_)) => "rpc_error",
                        Some(_) => "rpc_client",
                        None => "validation",
                    };
                    tracing::warn!(
                        chain_id,
                        rpc_index,
                        endpoint = %endpoint,
                        attempt,
                        attempts,
                        stage,
                        elapsed_ms = started.elapsed().as_millis(),
                        failure,
                        rpc_code,
                        http_status,
                        "public action preflight failed"
                    );
                }
                if insufficient_funding {
                    return Err(PublicActionPreflightError::InsufficientShieldFunding(error));
                }
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(PublicActionPreflightError::Other(
            error.wrap_err("all public action query RPC attempts failed"),
        ))
    } else {
        Err(PublicActionPreflightError::Other(eyre!(
            "no healthy query RPC available"
        )))
    }
}

async fn public_action_preflight(
    provider_handle: ProviderHandle,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
    rpc_reads: Option<&super::DappRpcReadClient>,
    stage: &mut &'static str,
    funding_budget: Option<PublicShieldFundingBudget>,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    let provider = &provider_handle.provider;
    let resolved = resolve_public_action_gas_fee(chain_id, profile, asset, gas_fee, quote)?;
    if railway_auto {
        *stage = "fee_authorization";
        let within_ceiling = railway_auto_fee_within_authorized_ceiling(
            chain_id,
            asset,
            authorized_fee_ceiling
                .ok_or_else(|| eyre!("missing Railway fee authorization ceiling"))?,
            &resolved,
        );
        if !within_ceiling {
            return Err(PublicActionPreflightError::FeeAuthorizationRequired {
                max_fee_per_gas: resolved.max_fee_per_gas,
                max_priority_fee_per_gas: resolved.max_priority_fee_per_gas,
                message:
                    "Network fees changed while approval confirmed. Review the updated fee to continue."
                        .to_string(),
            });
        }
    }
    let remaining_reserve = funding_budget.map_or(U256::ZERO, |budget| {
        budget.remaining_reserve(resolved.max_fee_per_gas)
    });
    let funding_balance = if let Some(budget) = funding_budget {
        *stage = "balance";
        let balance = match rpc_reads {
            Some(reads) => reads
                .get_balance(provider_handle.url.clone().into(), chain_id, from)
                .await
                .map_err(eyre::Report::new),
            None => provider.get_balance(from).await.map_err(eyre::Report::new),
        }
        .wrap_err("fetch public Shield native balance")?;
        *stage = "shield_funding_validation";
        // Replacements already have a signed limit; use the model only before a limit is known.
        let funding_gas_limit = authorized_gas_limit
            .or(gas_limit)
            .unwrap_or(budget.gas_units);
        let estimated_cost = public_action_native_exposure(
            base_tx_req.value.unwrap_or_default(),
            funding_gas_limit,
            resolved.max_fee_per_gas,
        )
        .saturating_add(remaining_reserve);
        if balance < estimated_cost {
            return Err(PublicActionPreflightError::InsufficientShieldFunding(
                eyre!(
                    "insufficient native balance for estimated Shield fees and transaction value; include fees for any required approval and the remaining Shield"
                ),
            ));
        }
        Some(balance)
    } else {
        None
    };
    *stage = "nonce";
    let nonce = if let Some(nonce) = nonce {
        nonce
    } else {
        match rpc_reads {
            Some(reads) => reads
                .get_transaction_count(provider_handle.url.clone().into(), chain_id, from)
                .await
                .map_err(eyre::Report::new),
            None => provider
                .get_transaction_count(from)
                .await
                .map_err(eyre::Report::new),
        }
        .wrap_err("fetch public action nonce")?
    };
    let legacy_envelope =
        super::gas::public_action_uses_legacy_envelope(chain_id, profile, asset, quote);
    ensure_native_shield_envelope(gas_limit_strategy, legacy_envelope)?;
    let tx_req = if legacy_envelope {
        public_action_legacy_transaction_request(
            base_tx_req,
            chain_id,
            from,
            resolved.max_fee_per_gas,
            nonce,
        )
    } else {
        public_action_eip1559_transaction_request(
            base_tx_req,
            chain_id,
            from,
            resolved.max_fee_per_gas,
            resolved.max_priority_fee_per_gas,
            nonce,
        )
    };
    let max_fee_per_gas = tx_req
        .max_fee_per_gas
        .or(tx_req.gas_price)
        .unwrap_or(resolved.max_fee_per_gas);
    let max_priority_fee_per_gas = tx_req.max_priority_fee_per_gas.unwrap_or_else(|| {
        if tx_req.gas_price.is_some() {
            0
        } else {
            resolved.max_priority_fee_per_gas
        }
    });
    *stage = "estimate";
    let gas_limit = if let Some(authorized_gas_limit) = authorized_gas_limit {
        let estimated_gas =
            public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                .await
                .wrap_err("re-estimate authorized advanced public transaction gas")?;
        *stage = "gas_limit_authorization";
        ensure_advanced_gas_estimate_authorized(estimated_gas, authorized_gas_limit)?;
        authorized_gas_limit
    } else if let Some(gas_limit) = gas_limit {
        gas_limit
    } else {
        match gas_limit_strategy {
            PublicActionGasLimitStrategy::NativeShieldPrepared {
                reviewed_gas_limit,
                profile,
            } => {
                // The complete type-4 request, sender, value and authorization included.
                let raw_gas_limit =
                    public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                        .await
                        .wrap_err("estimate native shield gas")?;
                *stage = "gas_limit_authorization";
                native_shield_signing_gas_limit(
                    profile,
                    gas.gas_limit_buffer,
                    raw_gas_limit,
                    reviewed_gas_limit,
                )?
            }
            PublicActionGasLimitStrategy::ChainBuffer => {
                public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                    .await
                    .wrap_err("estimate public action gas")?
                    .saturating_add(gas.gas_limit_buffer)
            }
            PublicActionGasLimitStrategy::RailwayEstimate120 => super::gas::railway_gas_limit(
                public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                    .await
                    .wrap_err("estimate public action gas")?,
            ),
        }
    };
    let estimated_native_gas_cost =
        public_action_native_exposure(tx_req.value.unwrap_or_default(), gas_limit, max_fee_per_gas);
    *stage = "balance";
    let live_native_balance = if let Some(balance) = funding_balance {
        balance
    } else {
        match rpc_reads {
            Some(reads) => reads
                .get_balance(provider_handle.url.clone().into(), chain_id, from)
                .await
                .map_err(eyre::Report::new),
            None => provider.get_balance(from).await.map_err(eyre::Report::new),
        }
        .wrap_err("fetch public action native balance")?
    };
    *stage = "native_balance_validation";
    let required_native_balance = estimated_native_gas_cost.saturating_add(remaining_reserve);
    if live_native_balance < required_native_balance {
        let action = if authorized_gas_limit.is_some() {
            "advanced public transaction"
        } else {
            "public action"
        };
        let error = eyre!(
            "insufficient native balance for {action}: live balance {live_native_balance}, required value plus maximum gas cost {required_native_balance}"
        );
        return Err(if funding_budget.is_some() {
            PublicActionPreflightError::InsufficientShieldFunding(error)
        } else {
            PublicActionPreflightError::Other(error)
        });
    }
    Ok(PublicActionPreflight {
        tx_req: tx_req.with_gas_limit(gas_limit),
        nonce,
        gas_limit,
        rpc_gas_price: resolved.rpc_gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        estimated_native_gas_cost,
        live_native_balance,
    })
}

async fn public_action_estimate_gas(
    provider: &ProviderHandle,
    chain_id: u64,
    tx: &TransactionRequest,
    rpc_reads: Option<&super::DappRpcReadClient>,
) -> Result<u64> {
    match rpc_reads {
        Some(reads) => reads
            .estimate_gas(provider.url.clone().into(), chain_id, tx)
            .await
            .map_err(eyre::Report::new),
        None => provider
            .provider
            .estimate_gas(tx.clone())
            .await
            .map_err(eyre::Report::new),
    }
}

pub(super) fn public_action_eip1559_transaction_request(
    tx_req: TransactionRequest,
    chain_id: u64,
    from: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    nonce: u64,
) -> TransactionRequest {
    tx_req
        .with_chain_id(chain_id)
        .with_from(from)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
        .with_nonce(nonce)
}

pub(super) fn public_action_legacy_transaction_request(
    tx_req: TransactionRequest,
    chain_id: u64,
    from: Address,
    gas_price: u128,
    nonce: u64,
) -> TransactionRequest {
    tx_req
        .with_chain_id(chain_id)
        .with_from(from)
        .with_gas_price(gas_price)
        .with_nonce(nonce)
}

pub fn sanitize_walletconnect_transaction_request(
    tx_req: TransactionRequest,
    chain_id: u64,
    from: Address,
) -> TransactionRequest {
    TransactionRequest {
        chain_id: Some(chain_id),
        from: Some(from),
        to: tx_req.to,
        value: tx_req.value,
        input: tx_req.input,
        access_list: tx_req.access_list,
        ..Default::default()
    }
}

#[must_use]
pub fn walletconnect_transaction_payload_fingerprint(
    chain_id: u64,
    from: Address,
    tx_req: &TransactionRequest,
) -> FixedBytes<32> {
    let mut encoded = b"railoxide:walletconnect-transaction:v1".to_vec();
    encoded.extend_from_slice(&chain_id.to_be_bytes());
    encoded.extend_from_slice(from.as_slice());
    match tx_req.to {
        Some(TxKind::Call(to)) => {
            encoded.push(1);
            encoded.extend_from_slice(to.as_slice());
        }
        Some(TxKind::Create) | None => encoded.push(0),
    }
    encoded.extend_from_slice(&tx_req.value.unwrap_or_default().to_be_bytes::<32>());
    let input = tx_req.input.input().map_or(&[][..], |input| input.as_ref());
    encoded.extend_from_slice(&(input.len() as u64).to_be_bytes());
    encoded.extend_from_slice(input);
    if let Some(access_list) = tx_req.access_list.as_ref() {
        encoded.push(1);
        encoded.extend_from_slice(&serde_json::to_vec(access_list).unwrap_or_default());
    } else {
        encoded.push(0);
    }
    keccak256(encoded)
}

pub fn validate_walletconnect_reviewed_transaction(
    chain_id: u64,
    from: Address,
    tx_req: &TransactionRequest,
    payload_fingerprint: FixedBytes<32>,
    gas_limit: u64,
) -> Result<()> {
    if gas_limit == 0 {
        return Err(eyre!(
            "reviewed WalletConnect gas limit must be greater than zero"
        ));
    }
    if walletconnect_transaction_payload_fingerprint(chain_id, from, tx_req) != payload_fingerprint
    {
        return Err(eyre!(
            "WalletConnect transaction changed after simulation review"
        ));
    }
    Ok(())
}

pub(super) fn public_action_native_exposure(
    value: U256,
    gas_limit: u64,
    max_fee_per_gas: u128,
) -> U256 {
    value + (U256::from(gas_limit) * U256::from(max_fee_per_gas))
}

pub(super) fn ensure_advanced_gas_estimate_authorized(
    estimated_gas: u64,
    authorized_gas_limit: u64,
) -> Result<()> {
    if estimated_gas > authorized_gas_limit {
        return Err(eyre!(
            "advanced transaction gas estimate {estimated_gas} exceeds authorized limit {authorized_gas_limit}; refresh the estimate and authorize again"
        ));
    }
    Ok(())
}

/// A native shield signs its profile's buffer over its final raw estimate, at most its
/// reviewed limit. The final shield key can move the estimate slightly from the prepared one,
/// so only a raw estimate above the reviewed limit is refused.
pub(super) fn native_shield_signing_gas_limit(
    profile: PublicShieldTransactionProfile,
    gas_limit_buffer: u64,
    raw_gas_limit: u64,
    reviewed_gas_limit: u64,
) -> Result<u64> {
    if raw_gas_limit > reviewed_gas_limit {
        return Err(eyre!(
            "native shield gas estimate {raw_gas_limit} exceeds its reviewed signing limit {reviewed_gas_limit}; prepare and review the shield again"
        ));
    }
    let gas_limit = super::gas::native_shield_gas_limit(profile, raw_gas_limit, gas_limit_buffer);
    Ok(gas_limit.min(reviewed_gas_limit))
}

/// A prepared native shield is type 4, which a network without a base fee can't carry.
pub(super) fn ensure_native_shield_envelope(
    gas_limit_strategy: PublicActionGasLimitStrategy,
    legacy_envelope: bool,
) -> Result<()> {
    let prepared_native = matches!(
        gas_limit_strategy,
        PublicActionGasLimitStrategy::NativeShieldPrepared { .. }
    );
    if prepared_native && legacy_envelope {
        return Err(eyre!(
            "this network has no base fee, so the type-4 native shield is unavailable; prepare the shield again"
        ));
    }
    Ok(())
}

async fn sign_send_public_action_transaction(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    signer: &VaultedPublicSigner,
    tx_req: TransactionRequest,
    label: &str,
    event_tx: Option<&PublicActionSessionEventSender>,
    expiry_timestamp: Option<u64>,
    log_transaction_details: bool,
    before_broadcast: &mut (impl FnMut(FixedBytes<32>, &[u8]) -> Result<()> + Send),
) -> Result<(), PublicActionAttemptError> {
    if log_transaction_details {
        tracing::info!(
            from = %tx_req.from.unwrap_or_default(),
            to = ?tx_req.to,
            gas = ?tx_req.gas,
            label,
            "signing and sending public action transaction",
        );
    }
    let signed_tx = signer
        .sign_transaction_request(tx_req, label)
        .await
        .map_err(PublicActionAttemptError::Signing)?;
    emit_refreshed_public_action_hardware_session(event_tx, signer);
    // Stop/abort requested during synchronous hardware approval is observed here before RPC broadcast.
    public_action_before_raw_broadcast_checkpoint().await;
    signer
        .ensure_active()
        .map_err(PublicActionAttemptError::Sending)?;
    broadcast_signed_public_action_transaction(
        query_rpc_pool,
        network_mode,
        signed_tx,
        label,
        expiry_timestamp,
        log_transaction_details,
        before_broadcast,
    )
    .await
}

async fn broadcast_signed_public_action_transaction(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    signed_tx: Vec<u8>,
    label: &str,
    expiry_timestamp: Option<u64>,
    log_transaction_details: bool,
    before_broadcast: &mut (impl FnMut(FixedBytes<32>, &[u8]) -> Result<()> + Send),
) -> Result<(), PublicActionAttemptError> {
    ensure_public_action_broadcast_not_expired(expiry_timestamp, label)
        .map_err(PublicActionAttemptError::Sending)?;
    let tx_hash = keccak256(&signed_tx);
    before_broadcast(tx_hash, &signed_tx).map_err(PublicActionAttemptError::Sending)?;
    let provider_handles = self_broadcast_send_raw_transaction_to_rpc_pool_with_logging(
        query_rpc_pool,
        network_mode,
        signed_tx,
        tx_hash,
        log_transaction_details,
    )
    .await
    .wrap_err_with(|| format!("{label}: send"))
    .map_err(PublicActionAttemptError::Sending)?;
    if log_transaction_details {
        tracing::info!(%tx_hash, providers = provider_handles.len(), label, "sent public action transaction");
    }
    Ok(())
}

pub(super) fn ensure_public_action_broadcast_not_expired(
    expiry_timestamp: Option<u64>,
    label: &str,
) -> Result<()> {
    let Some(expiry_timestamp) = expiry_timestamp else {
        return Ok(());
    };
    if public_action_current_unix_seconds() >= expiry_timestamp {
        return Err(eyre!(
            "{label}: request expired before transaction broadcast"
        ));
    }
    Ok(())
}

pub(super) fn public_action_current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

pub(super) async fn public_action_before_raw_broadcast_checkpoint() {
    tokio::task::yield_now().await;
}

pub(super) fn emit_public_action_event(
    event_tx: Option<&PublicActionSessionEventSender>,
    event: PublicActionSessionEvent,
) {
    if let Some(event_tx) = event_tx {
        let _ = event_tx.send(event);
    }
}

pub(super) fn emit_refreshed_public_action_hardware_session(
    event_tx: Option<&PublicActionSessionEventSender>,
    signer: &VaultedPublicSigner,
) {
    match signer.refreshed_hardware_session() {
        Ok(Some(session)) => emit_public_action_event(
            event_tx,
            PublicActionSessionEvent::HardwareProfileSessionRefreshed { session },
        ),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, "failed to read refreshed hardware public signer session");
        }
    }
}

pub(super) async fn recv_public_action_command(
    command_rx: &mut Option<PublicActionCommandReceiver>,
) -> Option<PublicActionCommand> {
    let command_rx = command_rx.as_mut()?;
    command_rx.recv().await
}

#[must_use]
pub const fn public_action_replacement_bumped_fee(value: u128) -> u128 {
    self_broadcast_replacement_bumped_fee(value)
}

pub(super) const fn public_action_progress_update(
    step: PublicActionProgressStep,
    status: PublicActionProgressStatus,
    tx_hash: Option<String>,
    message: Option<String>,
) -> PublicActionProgressUpdate {
    PublicActionProgressUpdate {
        step,
        status,
        tx_hash,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc_broker::tests::general;
    use crate::{PublicTransactionLookup, RpcChainRoute, RpcTrafficClass};
    use serde_json::json;
    use std::sync::Mutex;

    #[tokio::test]
    async fn custom_native_preparation_signs_observed_envelope_with_reviewed_bounds() {
        use crate::settings::{
            CustomChainSettings, NativeCurrency, WalletSettings, build_effective_chain_configs,
        };
        use alloy::consensus::{Transaction, TxEnvelope};
        use alloy::eips::Decodable2718;

        const CHAIN: u64 = 9_007_199_254_740_993;
        let mut settings = WalletSettings::default();
        settings.chains.custom.insert(
            CHAIN,
            CustomChainSettings {
                native_usd_pricing: crate::settings::NativeUsdPricing::Default,
                name: "Custom EVM".into(),
                native_currency: NativeCurrency {
                    name: "Custom".into(),
                    symbol: "CSTM".into(),
                    decimals: 6,
                },
                rpc_endpoints: vec!["http://127.0.0.1:9".to_owned().into()],
                explorer_urls: vec![],
                enabled: true,
                contracts: crate::settings::ChainContractSettings::default(),
                finality_depth: None,
                gas: crate::settings::ChainGasSettings::default(),
            },
        );
        let chain = build_effective_chain_configs(&settings)
            .unwrap()
            .get(CHAIN)
            .cloned()
            .unwrap();
        let amount =
            crate::parse_send_amount("1.25", Some(chain.native_currency.decimals)).unwrap();
        assert_eq!(chain.native_currency.format_amount(amount), "1.25 CSTM");
        assert!(chain.railgun.is_none() && chain.wrapped_native_token.is_none());
        let http = HttpContext::direct_for_tests();
        let route = chain
            .rpc_route
            .for_class(RpcTrafficClass::Sensitive)
            .unwrap();
        let pool = crate::query_rpc_pool_for_route(&route, &http);
        let signer = VaultedPublicSigner::Software(
            crate::signer::SoftwareEvmSigner::from_private_key([7; 32]).unwrap(),
        );
        let from = signer.address();
        for (base_fee, failed_header, estimated_gas) in [
            (None, false, 42_000),
            (Some(5), false, 42_000),
            (None, true, 42_000),
            (Some(5), false, 50_001),
        ] {
            let reads = crate::DappRpcReadClient::new(move |_, read| {
                Box::pin(async move {
                    Ok(match read.method() {
                        "eth_getBlockByNumber" if failed_header => {
                            return Err(crate::RpcBrokerError::InvalidResponse);
                        }
                        "eth_getBlockByNumber" => {
                            let mut block: alloy::rpc::types::Block =
                                alloy::rpc::types::Block::default();
                            block.header.inner.base_fee_per_gas = base_fee;
                            serde_json::to_value(block).unwrap()
                        }
                        "eth_gasPrice" => json!("0x64"),
                        "eth_maxPriorityFeePerGas" => json!("0x2"),
                        "eth_feeHistory" => return Err(crate::RpcBrokerError::InvalidResponse),
                        "eth_getTransactionCount" => json!("0x7"),
                        "eth_getBalance" => json!("0x100000000"),
                        "eth_estimateGas" => json!(format!("0x{estimated_gas:x}")),
                        method => panic!("unexpected read {method}"),
                    })
                })
            });
            let prepared = public_action_preflight_from_rpc_pool_with_reads(
                &pool,
                &http,
                &route,
                CHAIN,
                from,
                TransactionRequest::default()
                    .with_to(Address::repeat_byte(2))
                    .with_value(amount),
                PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas: 100,
                    max_priority_fee_per_gas: 2,
                },
                &chain.gas,
                PublicShieldTransactionProfile::Railoxide,
                PublicAssetId::Native,
                PublicActionGasLimitStrategy::ChainBuffer,
                Some(50_000),
                None,
                None,
                None,
                false,
                Some(&reads),
                None,
            )
            .await;
            if failed_header || estimated_gas > 50_000 {
                assert!(
                    prepared.is_err(),
                    "failed observation or exceeded review must prevent signing"
                );
                continue;
            }
            let prepared = prepared.unwrap();
            let exposure = prepared.estimated_native_gas_cost;
            let signed = signer
                .sign_transaction_request(prepared.tx_req, "custom native test")
                .await
                .unwrap();
            let envelope = TxEnvelope::decode_2718(&mut signed.as_slice()).unwrap();
            assert_eq!(envelope.chain_id(), Some(CHAIN));
            assert_eq!(envelope.value(), U256::from(1_250_000));
            assert_eq!(envelope.gas_limit(), 50_000);
            assert_eq!(envelope.max_fee_per_gas(), 100);
            assert_eq!(
                envelope.value()
                    + U256::from(envelope.gas_limit()) * U256::from(envelope.max_fee_per_gas()),
                exposure
            );
            assert_eq!(
                matches!(envelope, TxEnvelope::Legacy(_)),
                base_fee.is_none()
            );
            assert_eq!(envelope.max_priority_fee_per_gas(), base_fee.map(|_| 2));
        }
    }

    #[tokio::test]
    async fn broadcast_handoff_retains_ambiguous_attempt_before_replacement() {
        let (tracker, context) = crate::public_wallet::test_tracking_context();
        let observed_tracker = tracker.clone();
        let original = vec![1_u8];
        let replacement = vec![2_u8];
        let original_hash = keccak256(&original);
        let replacement_hash = keccak256(&replacement);
        let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request| {
                if request["method"] == "eth_blockNumber" {
                    return json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"});
                }
                assert_eq!(request["method"], "eth_sendRawTransaction");
                let bytes = alloy::hex::decode(request["params"][0].as_str().unwrap()).unwrap();
                let hash = keccak256(bytes);
                assert_eq!(
                    observed_tracker.lookup(1, hash),
                    PublicTransactionLookup::Pending
                );
                if hash == original_hash {
                    json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                        "code": -32000, "message": "submission outcome unavailable"
                    }})
                } else {
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": hash})
                }
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let http = HttpContext::direct_for_tests();
        let pool = crate::query_rpc_pool_for_route(
            &general(&RpcChainRoute::new(1, vec![endpoint.clone()])),
            &http,
        );
        let mut observer = BlockObserver::establish(pool, 1, http.rpc_broker(), 1)
            .await
            .unwrap()
            .with_tracking(&context);
        let mut attempts = Vec::new();
        for (signed, fee, expiry) in [
            (original.clone(), 10, Some(0)),
            (original, 10, None),
            (replacement, 20, None),
        ] {
            let pool = crate::query_rpc_pool_for_route(
                &general(&RpcChainRoute::new(1, vec![endpoint.clone()])),
                &http,
            );
            let result = broadcast_signed_public_action_transaction(
                &pool,
                http.network_mode(),
                signed,
                "test broadcast",
                expiry,
                true,
                &mut |tx_hash, _| {
                    retain_public_action_attempt(
                        &mut observer,
                        &mut attempts,
                        &SubmittedPublicActionAttempt {
                            tx_hash,
                            info: PublicActionAttemptInfo {
                                tx_hash: alloy::hex::encode_prefixed(tx_hash),
                                nonce: 7,
                                gas_limit: 21_000,
                                max_fee_per_gas: fee,
                                max_priority_fee_per_gas: fee / 2,
                            },
                            rpc_gas_price: fee,
                            estimated_native_gas_cost: U256::ZERO,
                            live_native_balance: U256::ZERO,
                        },
                    );
                    Ok(())
                },
            )
            .await;
            if expiry.is_some() {
                assert!(matches!(result, Err(PublicActionAttemptError::Sending(_))));
                assert!(attempts.is_empty());
                assert_eq!(
                    tracker.lookup(1, original_hash),
                    PublicTransactionLookup::Untracked
                );
            } else if fee == 10 {
                assert!(matches!(result, Err(PublicActionAttemptError::Sending(_))));
                assert_eq!(attempts.len(), 1);
            } else {
                assert!(result.is_ok());
            }
        }
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].tx_hash, original_hash);
        assert_eq!(attempts[1].tx_hash, replacement_hash);
        for (winner, fee) in [(0, 10), (1, 20)] {
            assert_eq!(
                public_action_winner_gas_fee(&attempts, winner),
                PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas: fee,
                    max_priority_fee_per_gas: fee / 2,
                },
            );
        }
        server.abort();
    }

    /// A chain-1 endpoint quoting fixed fees and estimating `estimate` gas. It logs each method
    /// and passes each raw broadcast's hash to `on_send` before accepting it.
    async fn session_endpoint(
        estimate: u64,
        on_send: impl Fn(FixedBytes<32>) + Send + Sync + 'static,
    ) -> (
        url::Url,
        tokio::task::JoinHandle<()>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let methods = Arc::new(Mutex::new(Vec::new()));
        let logged = methods.clone();
        let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
            Arc::new(move |request: serde_json::Value| {
                let method = request["method"].as_str().unwrap_or_default().to_owned();
                let result = match method.as_str() {
                    "eth_sendRawTransaction" => {
                        let raw = request["params"][0].as_str().unwrap();
                        let hash = keccak256(alloy::hex::decode(raw).unwrap());
                        on_send(hash);
                        json!(hash)
                    }
                    "eth_getBlockByNumber" => {
                        let mut block: alloy::rpc::types::Block =
                            alloy::rpc::types::Block::default();
                        block.header.inner.base_fee_per_gas = Some(5);
                        serde_json::to_value(block).unwrap()
                    }
                    "eth_gasPrice" => json!("0x64"),
                    "eth_maxPriorityFeePerGas" => json!("0x2"),
                    "eth_feeHistory" => json!({
                        "oldestBlock": "0x1",
                        "baseFeePerGas": ["0x5", "0x5"],
                        "gasUsedRatio": [0.5],
                        "reward": [["0x1", "0x2", "0x3", "0x4"]]
                    }),
                    "eth_estimateGas" => json!(format!("0x{estimate:x}")),
                    "eth_getTransactionCount" => json!("0x7"),
                    "eth_getBalance" => json!("0x3635c9adc5dea00000"),
                    "eth_getTransactionReceipt" => serde_json::Value::Null,
                    _ => json!("0x1"),
                };
                logged.lock().unwrap().push(method);
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        (endpoint, server, methods)
    }

    /// Runs a chain-1 native shield step session on `route`'s Public action pools with a
    /// software signer.
    async fn shield_session(
        route: RpcChainRoute,
        gas_limit_strategy: PublicActionGasLimitStrategy,
        gas_fee: PublicActionGasFeeSelection,
        command_rx: &mut Option<PublicActionCommandReceiver>,
        event_tx: Option<&PublicActionSessionEventSender>,
        handoff: &mut PublicActionHandoff<'_>,
    ) -> Result<PublicActionStepResult> {
        let http = HttpContext::direct_for_tests();
        let chain = crate::settings::build_effective_chain_configs(
            &crate::settings::WalletSettings::default(),
        )
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
        let pools = public_action_pools(&route, &http).expect("route has every class");
        let signer = VaultedPublicSigner::Software(
            crate::signer::SoftwareEvmSigner::from_private_key([7; 32]).unwrap(),
        );
        submit_public_action_step_session(
            PublicActionProgressStep::Shield,
            TransactionRequest::default()
                .with_to(Address::repeat_byte(2))
                .with_value(U256::from(1)),
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            gas_limit_strategy,
            &signer,
            "public action session test",
            &pools,
            1,
            &http,
            None,
            1,
            signer.address(),
            &chain.gas,
            None,
            None,
            None,
            gas_fee,
            PublicActionStepFeePolicy::Custom,
            None,
            command_rx,
            event_tx,
            Some(handoff),
            PublicActionStepEnd::Receipt,
            &mut |_| {},
        )
        .await
    }

    #[tokio::test]
    async fn handoff_session_records_each_replacement_before_its_broadcast() {
        use crate::public_wallet::tests::LogWriter;
        use tracing::instrument::WithSubscriber;

        let handed_off = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (endpoint, server, _) = session_endpoint(21_000, {
            let handed_off = handed_off.clone();
            let sent = sent.clone();
            move |hash| {
                let recorded = handed_off
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(handed_off, _)| *handed_off == hash);
                sent.lock().unwrap().push((hash, recorded));
            }
        })
        .await;
        let fee = |max_fee_per_gas| PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas: 2,
        };
        // The first replacement's hand-off fails; the second's succeeds.
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        for max_fee_per_gas in [200, 300] {
            command_tx
                .send(PublicActionCommand {
                    kind: crate::PublicActionCommandKind::Replacement,
                    gas_fee: fee(max_fee_per_gas),
                })
                .unwrap();
        }
        let mut command_rx = Some(command_rx);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut handoff = {
            let handed_off = handed_off.clone();
            move |tx_hash: FixedBytes<32>, request: &TransactionRequest, _: &[u8]| -> Result<()> {
                let mut handed_off = handed_off.lock().unwrap();
                handed_off.push((tx_hash, request.max_fee_per_gas.or(request.gas_price)));
                if handed_off.len() == 2 {
                    return Err(eyre!("replacement hand-off failed"));
                }
                Ok(())
            }
        };
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let mut events = Vec::new();
        let mut submitted = Vec::new();
        let session = Box::pin(shield_session(
            RpcChainRoute::new(1, vec![endpoint]),
            PublicActionGasLimitStrategy::ChainBuffer,
            fee(100),
            &mut command_rx,
            Some(&event_tx),
            &mut handoff,
        ));
        let observe = async {
            while submitted.len() < 2 {
                let event = event_rx.recv().await.unwrap();
                if let PublicActionSessionEvent::AttemptSubmitted { attempt, .. } = &event {
                    submitted.push(attempt.clone());
                }
                events.push(event);
            }
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            tokio::select! {
                result = session => panic!("session ended early: {:?}", result.err()),
                () = observe => {}
            }
        })
        .with_subscriber(subscriber)
        .await
        .expect("both accepted attempts are submitted");

        let handed_off = handed_off.lock().unwrap().clone();
        assert_eq!(handed_off.len(), 3);
        // Each broadcast attempt was recorded first; the failed replacement was never sent.
        assert_eq!(
            *sent.lock().unwrap(),
            vec![(handed_off[0].0, true), (handed_off[2].0, true)]
        );
        for (attempt, (tx_hash, max_fee_per_gas)) in
            submitted.iter().zip([handed_off[0], handed_off[2]])
        {
            assert_eq!(attempt.tx_hash, alloy::hex::encode_prefixed(tx_hash));
            assert_eq!(Some(attempt.max_fee_per_gas), max_fee_per_gas);
        }
        assert_ne!(handed_off[0].1, handed_off[2].1);
        // The rejection leaves the session observing and accepting the next replacement.
        let rejected = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    PublicActionSessionEvent::AttemptRejected { message, .. }
                        if message.contains("replacement hand-off failed")
                )
            })
            .expect("failed replacement hand-off is rejected");
        let accepted = events
            .iter()
            .rposition(|event| matches!(event, PublicActionSessionEvent::AttemptSubmitted { .. }))
            .unwrap();
        assert!(rejected < accepted);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(!logs.contains("public action transaction"), "{logs}");
        assert!(!logs.contains("self-broadcast tx"), "{logs}");
        drop(command_tx);
        server.abort();
    }

    #[tokio::test]
    async fn native_shield_preflight_failure_ends_session_despite_queued_retry() {
        const REVIEWED_GAS_LIMIT: u64 = 50_000;
        let (endpoint, server, methods) = session_endpoint(REVIEWED_GAS_LIMIT + 1, |_| {}).await;
        let fee = PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
        };
        // The sender stays alive so only the native-shield rule can end the session.
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        command_tx
            .send(PublicActionCommand {
                kind: crate::PublicActionCommandKind::Retry,
                gas_fee: fee,
            })
            .unwrap();
        let mut command_rx = Some(command_rx);
        let handed_off = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handoff = {
            let handed_off = handed_off.clone();
            move |_: FixedBytes<32>, _: &TransactionRequest, _: &[u8]| -> Result<()> {
                handed_off.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        };
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            Box::pin(shield_session(
                RpcChainRoute::new(1, vec![endpoint]),
                PublicActionGasLimitStrategy::NativeShieldPrepared {
                    reviewed_gas_limit: REVIEWED_GAS_LIMIT,
                    profile: PublicShieldTransactionProfile::Railoxide,
                },
                fee,
                &mut command_rx,
                None,
                &mut handoff,
            )),
        )
        .await
        .expect("native shield preflight failure ends the session");

        let error = result.err().expect("oversized native estimate fails");
        assert!(
            report_chain_string(&error).contains("refreshed preparation"),
            "{error:?}"
        );
        let methods = methods.lock().unwrap().clone();
        assert!(methods.iter().any(|method| method == "eth_estimateGas"));
        assert!(
            !methods
                .iter()
                .any(|method| method == "eth_sendRawTransaction")
        );
        assert!(!handed_off.load(std::sync::atomic::Ordering::SeqCst));
        drop(command_tx);
        server.abort();
    }

    #[tokio::test]
    async fn public_action_reads_go_to_sensitive_endpoints_and_broadcast_to_submit_ones() {
        let (general_only, general_server, general_methods) =
            session_endpoint(21_000, |_| {}).await;
        let (reader, reader_server, reader_methods) = session_endpoint(21_000, |_| {}).await;
        let (submitter, submitter_server, submitter_methods) =
            session_endpoint(21_000, |_| {}).await;
        let roles = |general, sensitive, submit| crate::RpcEndpointRoles {
            general,
            sensitive,
            submit,
        };
        let route = RpcChainRoute::new_with_transports(
            1,
            vec![
                (
                    general_only.into(),
                    crate::RpcTransport::Wallet,
                    roles(true, false, false),
                ),
                (
                    reader.into(),
                    crate::RpcTransport::Wallet,
                    roles(false, true, false),
                ),
                (
                    submitter.into(),
                    crate::RpcTransport::Wallet,
                    roles(false, false, true),
                ),
            ],
        );
        let mut command_rx = None;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut handoff =
            |_: FixedBytes<32>, _: &TransactionRequest, _: &[u8]| -> Result<()> { Ok(()) };
        let session = Box::pin(shield_session(
            route,
            PublicActionGasLimitStrategy::ChainBuffer,
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 2,
            },
            &mut command_rx,
            Some(&event_tx),
            &mut handoff,
        ));
        let submitted = async {
            while !matches!(
                event_rx.recv().await.unwrap(),
                PublicActionSessionEvent::AttemptSubmitted { .. }
            ) {}
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            tokio::select! {
                result = session => panic!("session ended early: {:?}", result.err()),
                () = submitted => {}
            }
        })
        .await
        .expect("the attempt is submitted");

        assert!(general_methods.lock().unwrap().is_empty());
        let reader_methods = reader_methods.lock().unwrap().clone();
        let read = |method: &str| reader_methods.iter().any(|read| read == method);
        assert!(
            ["eth_gasPrice", "eth_maxPriorityFeePerGas", "eth_feeHistory"]
                .into_iter()
                .any(read)
        );
        assert!(read("eth_getTransactionCount") && read("eth_estimateGas"));
        assert!(
            !reader_methods
                .iter()
                .any(|read| read == "eth_sendRawTransaction")
        );
        assert_eq!(
            *submitter_methods.lock().unwrap(),
            vec!["eth_sendRawTransaction".to_owned()]
        );
        general_server.abort();
        reader_server.abort();
        submitter_server.abort();
    }
}
