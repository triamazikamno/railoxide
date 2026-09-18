use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::network::TransactionBuilder as _;
use alloy::primitives::{Address, FixedBytes, TxKind, U256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool};
use eyre::{Result, WrapErr, eyre};

use super::gas::resolve_public_action_gas_fee;
use super::signer::VaultedPublicSigner;
use super::types::{
    PublicActionAttemptInfo, PublicActionCommand, PublicActionCommandReceiver,
    PublicActionGasFeeQuote, PublicActionGasFeeSelection, PublicActionGasLimitStrategy,
    PublicActionProgressStatus, PublicActionProgressStep, PublicActionProgressUpdate,
    PublicActionSessionEvent, PublicActionSessionEventSender, PublicActionStepFeePolicy,
    PublicShieldTransactionProfile,
};
use crate::block_observer::BlockObserver;
use crate::settings::EffectiveChainGasSettings;
use crate::{
    HttpContext, SelfBroadcastResolvedGasFee, TxReceiptOutput, report_chain_string,
    self_broadcast_replacement_bumped_fee,
    self_broadcast_send_raw_transaction_to_rpc_pool_with_logging,
};

pub(super) struct PublicActionStepOutcome {
    pub(super) receipt: TxReceiptOutput,
    pub(super) next_nonce: u64,
    pub(super) gas_fee: PublicActionGasFeeSelection,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicActionPreflightMode {
    Managed,
}

impl PublicActionPreflightMode {
    const fn needs_fee_quote(self, gas_fee: PublicActionGasFeeSelection) -> bool {
        matches!(self, Self::Managed) && matches!(gas_fee, PublicActionGasFeeSelection::Auto)
    }
}

#[derive(Debug)]
pub(super) enum PublicActionPreflightError {
    FeeAuthorizationRequired {
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        message: String,
    },
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
            Self::Other(error) => std::fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for PublicActionPreflightError {}

impl PublicActionPreflightError {
    pub(super) fn into_report(self) -> eyre::Report {
        match self {
            Self::FeeAuthorizationRequired { message, .. } => eyre!("{}", message),
            Self::Other(error) => error,
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

pub(super) type PublicActionHandoff<'a> =
    dyn FnMut(FixedBytes<32>, &TransactionRequest) -> Result<()> + Send + 'a;

pub(super) async fn submit_public_action_step_session(
    step: PublicActionProgressStep,
    base_tx_req: TransactionRequest,
    profile: PublicShieldTransactionProfile,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    signer: &VaultedPublicSigner,
    label: &str,
    query_rpc_pool: Arc<QueryRpcPool>,
    finality_depth: u64,
    http: &HttpContext,
    transaction_tracking: Option<&crate::PublicTransactionTrackingContext>,
    chain_id: u64,
    from_address: Address,
    gas: &EffectiveChainGasSettings,
    authorized_gas_limit: Option<u64>,
    mut nonce: Option<u64>,
    gas_fee: PublicActionGasFeeSelection,
    fee_policy: PublicActionStepFeePolicy,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    command_rx: &mut Option<PublicActionCommandReceiver>,
    event_tx: Option<&PublicActionSessionEventSender>,
    mut handoff: Option<&mut PublicActionHandoff<'_>>,
    progress: &mut (impl FnMut(PublicActionProgressUpdate) + Send),
) -> Result<PublicActionStepOutcome> {
    let private_handoff = handoff.is_some();
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
            http.network_mode(),
            chain_id,
            from_address,
            base_tx_req.clone(),
            next_gas_fee,
            gas,
            profile,
            gas_limit_strategy,
            authorized_gas_limit,
            nonce,
            None,
            authorized_fee_ceiling,
            railway_auto,
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
                Arc::clone(&query_rpc_pool),
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
            query_rpc_pool.as_ref(),
            http.network_mode(),
            signer,
            label,
            event_tx,
            None,
            !private_handoff,
            &mut |attempt| {
                if let Some(context) = transaction_tracking {
                    context.ensure_open()?;
                }
                if let Some(handoff) = handoff.as_deref_mut() {
                    handoff(attempt.tx_hash, &attempted_request)?;
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
                            http.network_mode(),
                            chain_id,
                            from_address,
                            base_tx_req.clone(),
                            command.gas_fee,
                            gas,
                            profile,
                            gas_limit_strategy,
                            authorized_gas_limit,
                            Some(nonce),
                            Some(gas_limit),
                            authorized_fee_ceiling,
                            railway_auto,
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
                        match submit_public_action_attempt(
                            step,
                            replacement,
                            query_rpc_pool.as_ref(),
                            http.network_mode(),
                            signer,
                            label,
                            event_tx,
                            None,
                            true,
                            &mut |attempt| {
                                if let Some(context) = transaction_tracking {
                                    context.ensure_open()?;
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
                    progress(public_action_progress_update(
                        step,
                        PublicActionProgressStatus::Done,
                        Some(receipt.tx_hash.clone()),
                        None,
                    ));
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
                    let Some(command) = recv_public_action_command(command_rx).await else {
                        return Ok(PublicActionStepOutcome {
                            receipt,
                            next_nonce: winner.info.nonce.saturating_add(1),
                            gas_fee,
                        });
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
                return Ok(PublicActionStepOutcome {
                    receipt,
                    next_nonce: winner.info.nonce.saturating_add(1),
                    gas_fee,
                });
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

fn public_action_winner_gas_fee(
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
        && (PublicShieldTransactionProfile::Railway.uses_legacy_envelope(chain_id)
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
    before_broadcast: &mut (impl FnMut(&SubmittedPublicActionAttempt) -> Result<()> + Send),
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
        &mut |tx_hash| {
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
            before_broadcast(&attempt)?;
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

async fn public_action_preflight_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    public_action_preflight_from_rpc_pool_with_mode(
        query_rpc_pool,
        network_mode,
        chain_id,
        from,
        base_tx_req,
        gas_fee,
        gas,
        profile,
        gas_limit_strategy,
        authorized_gas_limit,
        nonce,
        gas_limit,
        PublicActionPreflightMode::Managed,
        authorized_fee_ceiling,
        railway_auto,
    )
    .await
}

pub(super) async fn public_action_preflight_from_rpc_pool_with_mode(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    mode: PublicActionPreflightMode,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    public_action_preflight_from_rpc_pool_with_mode_and_reads(
        query_rpc_pool,
        network_mode,
        chain_id,
        from,
        base_tx_req,
        gas_fee,
        gas,
        profile,
        gas_limit_strategy,
        authorized_gas_limit,
        nonce,
        gas_limit,
        mode,
        authorized_fee_ceiling,
        railway_auto,
        None,
    )
    .await
}

pub(super) async fn public_action_preflight_from_rpc_pool_with_mode_and_reads(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    mode: PublicActionPreflightMode,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
    rpc_reads: Option<&super::DappRpcReadClient>,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    if rpc_reads.is_some() && profile != PublicShieldTransactionProfile::Railoxide {
        return Err(eyre!("admitted dapp reads require the Railoxide transaction profile").into());
    }
    let quote = if mode.needs_fee_quote(gas_fee)
        && (profile != PublicShieldTransactionProfile::Railway || railway_auto)
    {
        Some(
            match rpc_reads {
                Some(reads) => {
                    crate::self_broadcast_gas_fee_quote_from_rpc_pool_with_reads(
                        query_rpc_pool,
                        network_mode,
                        super::gas::public_action_tip_fallback(chain_id),
                        Some((reads, chain_id)),
                    )
                    .await
                }
                None => {
                    super::gas::public_action_gas_fee_quote_from_rpc_pool_with_profile(
                        query_rpc_pool,
                        network_mode,
                        chain_id,
                        profile,
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
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        match public_action_preflight(
            provider_handle,
            chain_id,
            from,
            base_tx_req.clone(),
            gas_fee,
            quote,
            gas,
            profile,
            gas_limit_strategy,
            authorized_gas_limit,
            nonce,
            gas_limit,
            authorized_fee_ceiling,
            railway_auto,
            rpc_reads,
        )
        .await
        {
            Ok(preflight) => return Ok(preflight),
            Err(error @ PublicActionPreflightError::FeeAuthorizationRequired { .. }) => {
                return Err(error);
            }
            Err(PublicActionPreflightError::Other(error)) => {
                if rpc_reads.is_some() {
                    if error
                        .downcast_ref::<crate::rpc_broker::RpcBrokerError>()
                        .is_some_and(super::stops_dapp_read_retries)
                    {
                        return Err(PublicActionPreflightError::Other(error));
                    }
                } else {
                    tracing::warn!("public action preflight failed");
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
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
    rpc_reads: Option<&super::DappRpcReadClient>,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    let provider = &provider_handle.provider;
    let resolved = resolve_public_action_gas_fee(chain_id, profile, gas_fee, quote)?;
    if railway_auto {
        let within_ceiling = railway_auto_fee_within_authorized_ceiling(
            chain_id,
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
    let tx_req = if profile.uses_legacy_envelope(chain_id) {
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
    let gas_limit = if let Some(authorized_gas_limit) = authorized_gas_limit {
        let estimated_gas =
            public_action_estimate_gas(&provider_handle, chain_id, &tx_req, rpc_reads)
                .await
                .wrap_err("re-estimate authorized advanced public transaction gas")?;
        ensure_advanced_gas_estimate_authorized(estimated_gas, authorized_gas_limit)?;
        authorized_gas_limit
    } else if let Some(gas_limit) = gas_limit {
        gas_limit
    } else {
        match gas_limit_strategy {
            PublicActionGasLimitStrategy::RailwayNativeFixed => 6_000_000,
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
    let live_native_balance = match rpc_reads {
        Some(reads) => reads
            .get_balance(provider_handle.url.clone().into(), chain_id, from)
            .await
            .map_err(eyre::Report::new),
        None => provider.get_balance(from).await.map_err(eyre::Report::new),
    }
    .wrap_err("fetch public action native balance")?;
    if live_native_balance < estimated_native_gas_cost {
        let action = if authorized_gas_limit.is_some() {
            "advanced public transaction"
        } else {
            "public action"
        };
        return Err(PublicActionPreflightError::Other(eyre!(
            "insufficient native balance for {action}: live balance {live_native_balance}, required value plus maximum gas cost {estimated_native_gas_cost}"
        )));
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

async fn sign_send_public_action_transaction(
    query_rpc_pool: &QueryRpcPool,
    network_mode: crate::WalletNetworkMode,
    signer: &VaultedPublicSigner,
    tx_req: TransactionRequest,
    label: &str,
    event_tx: Option<&PublicActionSessionEventSender>,
    expiry_timestamp: Option<u64>,
    log_transaction_details: bool,
    before_broadcast: &mut (impl FnMut(FixedBytes<32>) -> Result<()> + Send),
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
    before_broadcast: &mut (impl FnMut(FixedBytes<32>) -> Result<()> + Send),
) -> Result<(), PublicActionAttemptError> {
    ensure_public_action_broadcast_not_expired(expiry_timestamp, label)
        .map_err(PublicActionAttemptError::Sending)?;
    let tx_hash = keccak256(&signed_tx);
    before_broadcast(tx_hash).map_err(PublicActionAttemptError::Sending)?;
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
    use crate::PublicTransactionLookup;
    use serde_json::json;

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
        let pool = crate::query_rpc_pool_with_http_client(vec![endpoint.clone()], &http);
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
            let pool = crate::query_rpc_pool_with_http_client(vec![endpoint.clone()], &http);
            let result = broadcast_signed_public_action_transaction(
                &pool,
                http.network_mode(),
                signed,
                "test broadcast",
                expiry,
                true,
                &mut |tx_hash| {
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
}
