use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use alloy::eips::eip7702::Authorization;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Signature, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::RelayAdapt7702;
use eyre::{Result, WrapErr, eyre};
use railgun_wallet::tx::ExecutorContext;
use railgun_wallet::{TransactionCall, Utxo};

use super::{
    ExecutorOwner, ExecutorRecoveryExecution, ExecutorRecoveryFunding, PreparedExecutorRecovery,
};
use crate::desktop::executor_discovery::{inspect_for_signing, matches_executor_delegation};
use crate::settings::{EffectiveChainConfig, ExecutorProfile};
use crate::vault::{
    ExecutorInputIdentity, ExecutorOperationId, ExecutorPayloadContext, ExecutorPayloadPurpose,
    IssuedExecutorPayload,
};
use crate::{ExecutorActivity, ExecutorAsset, ExecutorInspection, PublicBroadcasterCandidate};

/// Funding selection precedes allocation. Existing sponsored Send and Unshield
/// select their legacy route before reaching this owner.
#[derive(Clone)]
pub enum ExecutorDelivery {
    PublicBroadcaster(Box<PublicBroadcasterCandidate>),
    SelfBroadcast { sender: Address, sponsored: bool },
    ManualExport,
}

impl ExecutorDelivery {
    /// Admit this delivery for an operation needing `profile`'s capability. Returns the
    /// profile its delegation authorization targets: a broadcaster's advertised delegate.
    pub fn admit(&self, profile: ExecutorProfile) -> Result<ExecutorProfile> {
        match self {
            Self::PublicBroadcaster(candidate) => {
                let target = candidate
                    .relay_adapt_7702
                    .and_then(|advertised| profile.interchangeable(advertised))
                    .filter(|_| {
                        candidate.chain_id == profile.chain_id()
                            && candidate.available_wallets != 0
                            && candidate.fee_expiration > std::time::SystemTime::now()
                    })
                    .ok_or_else(|| {
                        eyre!("selected broadcaster does not support this executor operation")
                    })?;
                candidate.parsed_required_poi_list_keys()?;
                Ok(target)
            }
            Self::SelfBroadcast {
                sponsored: false, ..
            } => Ok(profile),
            Self::SelfBroadcast {
                sponsored: true, ..
            } => Err(eyre!(
                "builder sponsorship is unavailable for executor operations"
            )),
            Self::ManualExport => Err(eyre!(
                "manual calldata export cannot represent executor delegation authorizations"
            )),
        }
    }

    const fn sender(&self) -> Option<Address> {
        match self {
            Self::SelfBroadcast { sender, .. } => Some(*sender),
            Self::PublicBroadcaster(_) | Self::ManualExport => None,
        }
    }
}

/// Outside private sponsorship, unwrap-to-native and native top-up Unshields run only
/// through the executor; there is no legacy `RelayAdapt` fallback.
#[must_use]
pub const fn unshield_requires_executor(
    unwrap: bool,
    native_top_up: bool,
    sponsored: bool,
) -> bool {
    (unwrap || native_top_up) && !sponsored
}

/// Why an Unshield that requires the executor cannot use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecutorRouteUnavailable {
    #[error(
        "Unwrap and gas top-up need this wallet's stealth account, which this session doesn't have."
    )]
    NoExecutorOwner,
    #[error("Unwrap and gas top-up need the accepted RelayAdapt7702 contract on this network.")]
    NoAcceptedProfile,
}

/// The executor route for an Unshield that requires it.
pub fn unshield_executor_route(
    has_executor_owner: bool,
    chain: &EffectiveChainConfig,
) -> std::result::Result<ExecutorProfile, ExecutorRouteUnavailable> {
    if !has_executor_owner {
        return Err(ExecutorRouteUnavailable::NoExecutorOwner);
    }
    chain
        .accepted_executor_profile()
        .ok_or(ExecutorRouteUnavailable::NoAcceptedProfile)
}

/// A newly reserved account that was found used before it issued anything. It is retired.
#[derive(Debug, thiserror::Error)]
#[error("reserved executor has prior activity; it remains available for recovery")]
pub struct ExecutorAccountUsed {
    operation: ExecutorOperationId,
}

impl ExecutorAccountUsed {
    /// The retired account's operation.
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
}

/// Native preparation identity carried through quotes and proof generation.
/// It contains no signing key and cannot be constructed from browser input.
pub struct PreparedExecutorOperation {
    operation: ExecutorOperationId,
    generation: u64,
    wallet_id: String,
    owner: tokio::sync::watch::Sender<bool>,
    context: ExecutorContext,
    delivery: ExecutorDelivery,
    inspection_assets: Vec<ExecutorAsset>,
    recovery: Option<std::sync::Arc<PreparedExecutorRecovery>>,
}

impl PreparedExecutorOperation {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }

    #[must_use]
    pub const fn context(&self) -> ExecutorContext {
        self.context
    }

    #[must_use]
    pub const fn delivery(&self) -> &ExecutorDelivery {
        &self.delivery
    }

    /// A retained executor's recovery, including a swap's early cancellation.
    pub(crate) const fn is_recovery(&self) -> bool {
        self.recovery.is_some()
    }

    pub(crate) fn require_broadcaster(&self, candidate: &PublicBroadcasterCandidate) -> Result<()> {
        let ExecutorDelivery::PublicBroadcaster(prepared) = &self.delivery else {
            return Err(eyre!("executor preparation selected another funding route"));
        };
        if prepared.chain_id != candidate.chain_id
            || prepared.railgun_address != candidate.railgun_address
            || prepared.fees_id != candidate.fees_id
            || prepared.fee != candidate.fee
            || prepared.token != candidate.token
            || prepared.fee_expiration != candidate.fee_expiration
            || prepared.viewing_public_key != candidate.viewing_public_key
            || prepared.required_poi_list_keys != candidate.required_poi_list_keys
        {
            return Err(eyre!(
                "executor broadcaster selection changed; refresh preparation"
            ));
        }
        Ok(())
    }
}

/// What a prepared operation's reservation stores beside its account.
pub(super) enum OperationReservation<'a> {
    /// An ordinary reservation, or a new swap's, whose record is created holding
    /// `swap_approval` and the link to its destination stealth account.
    Operation {
        purpose_summary: Option<&'a str>,
        swap_approval: Option<&'a crate::vault::SwapApproval>,
        destination_operation: Option<ExecutorOperationId>,
    },
    /// A private Bridge swap's destination stealth account on this chain.
    SwapDestination(crate::vault::SwapDestinationRecord),
}

/// Returning this value is signed-data handoff: its payload is already durable.
pub struct IssuedExecutorTransaction {
    operation: ExecutorOperationId,
    payload_hash: B256,
    transaction: TransactionRequest,
}

/// The caller-specific part of one execute payload: the hash the executor signs,
/// the transaction value, the private inputs it spends, and how the signature
/// becomes the signed call.
pub(super) struct PayloadToSign<'a> {
    pub(super) hash: B256,
    pub(super) value: U256,
    pub(super) inputs: Vec<ExecutorInputIdentity>,
    /// Sign an authorization even when this delegate is installed.
    pub(super) authorize_delegation: bool,
    pub(super) authorize: Box<dyn FnOnce(Signature) -> Result<TransactionCall> + Send + 'a>,
}

/// Chain evidence that one paid fee-convergence loop reuses to sign repriced
/// variants of its own payload. The loop owns this value and drops it before
/// broadcaster submission, on error, and on exhaustion, so it never outlives
/// one signing approval.
///
/// Reuse is safe because every variant signs the same execution nonce, so at
/// most one of them can execute. A fresh read would only notice a third party
/// executing an exposed variant a few seconds earlier. Persistence, input
/// reservations, authorization, and the durable nonce observation are
/// unchanged. The context holds no signing material.
#[derive(Default)]
pub(crate) struct IssueRetry {
    seed: Option<IssueRetrySeed>,
}

struct IssueRetrySeed {
    operation: ExecutorOperationId,
    execution_nonce: U256,
    /// The unused-account inspection behind the first payload, including its
    /// nonce observation.
    checked: Arc<super::spare::CheckedExecutor>,
    /// Every payload this context issued. Any other payload blocks reuse.
    issued: BTreeSet<B256>,
}

impl IssueRetry {
    /// Returns the saved inspection only while the durable record shows nothing
    /// but this context's own unsent payloads under the same observation. History
    /// reconciliation can replace that observation without an unused-cache change.
    fn reusable(
        &self,
        prepared: &PreparedExecutorOperation,
        record: &crate::vault::ExecutorRecord,
    ) -> Option<Arc<super::spare::CheckedExecutor>> {
        let seed = self.seed.as_ref()?;
        (prepared.recovery.is_none()
            && seed.operation == prepared.operation
            && seed.execution_nonce == prepared.context.execution_nonce
            && record.issued().iter().all(|payload| {
                seed.issued.contains(&payload.hash())
                    && payload.transaction_hashes().is_empty()
                    && !record.nonce_resolved(payload.nonce())
            })
            && seed.checked.ensure_valid().is_ok()
            && record.nonce_observation() == Some(seed.checked.observed))
        .then(|| Arc::clone(&seed.checked))
    }
}

fn require_unfinished_operation(record: &crate::vault::ExecutorRecord) -> Result<()> {
    if record
        .issued()
        .iter()
        .any(|payload| record.nonce_resolved(payload.nonce()))
    {
        return Err(eyre!(
            "this executor operation has already completed; start a new action"
        ));
    }
    Ok(())
}

impl IssuedExecutorTransaction {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }

    #[must_use]
    pub const fn payload_hash(&self) -> B256 {
        self.payload_hash
    }

    #[must_use]
    pub const fn transaction(&self) -> &TransactionRequest {
        &self.transaction
    }
}

impl ExecutorOwner {
    /// Prepare with the same software authorization used by the private spend.
    pub async fn prepare_authorized_operation(
        &self,
        operation: ExecutorOperationId,
        delivery: ExecutorDelivery,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        assets: &[ExecutorAsset],
        purpose_summary: Option<&str>,
    ) -> Result<PreparedExecutorOperation> {
        self.prepare_operation(operation, delivery, authorization, assets, purpose_summary)
            .await
    }

    /// Recheck native ownership before quoting or proving an existing preparation.
    pub fn validate_preparation(
        &self,
        prepared: &PreparedExecutorOperation,
    ) -> Result<crate::vault::ExecutorRecord> {
        self.ensure_active()?;
        if let Some(recovery) = &prepared.recovery {
            let record = self.validate_recovery(recovery)?;
            if !self.closed.same_channel(&prepared.owner)
                || prepared.generation != self.generation
                || prepared.wallet_id != self.view.wallet_id()
                || prepared.operation != record.operation()
                || prepared.context.executor != recovery.source()
                || prepared.context.chain_id != self.chain.chain_id
                || prepared.context.delegate != record.delegate()
                || recovery.execution()
                    != (ExecutorRecoveryExecution::PaidExecute {
                        nonce: prepared.context.execution_nonce,
                    })
            {
                return Err(eyre!(
                    "recovery execution preparation no longer matches its owner"
                ));
            }
            return Ok(record);
        }
        let profile = self
            .chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        let profile = prepared.delivery.admit(profile)?;
        if !self.closed.same_channel(&prepared.owner)
            || prepared.generation != self.generation
            || prepared.wallet_id != self.view.wallet_id()
            || prepared.context.chain_id != profile.chain_id()
            || prepared.context.delegate != profile.delegate()
        {
            return Err(eyre!(
                "executor preparation belongs to a different wallet session"
            ));
        }
        let record = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == prepared.operation)
            .ok_or_else(|| eyre!("executor reservation is unavailable"))?;
        if record.is_retired()
            || record.address() != Some(prepared.context.executor)
            || record.delegate() != prepared.context.delegate
        {
            return Err(eyre!(
                "executor preparation no longer matches its reservation"
            ));
        }
        require_unfinished_operation(&record)?;
        Ok(record)
    }

    /// Bind paid recovery to the retained identity without allocating or exposing
    /// another account. Proof and delivery code carry this same preparation.
    pub fn prepare_broadcaster_recovery_execution(
        &self,
        recovery: std::sync::Arc<PreparedExecutorRecovery>,
    ) -> Result<PreparedExecutorOperation> {
        let record = self.validate_recovery(&recovery)?;
        let ExecutorRecoveryFunding::PublicBroadcaster { candidate, .. } = recovery.funding()
        else {
            return Err(eyre!("this recovery selected another funding route"));
        };
        let ExecutorRecoveryExecution::PaidExecute { nonce } = recovery.execution() else {
            return Err(eyre!(
                "broadcaster recovery requires the current execute wrapper"
            ));
        };
        Ok(PreparedExecutorOperation {
            operation: record.operation(),
            generation: self.generation,
            wallet_id: self.view.wallet_id().to_owned(),
            owner: self.closed.clone(),
            context: ExecutorContext {
                chain_id: self.chain.chain_id,
                executor: recovery.source(),
                delegate: record.delegate(),
                execution_nonce: nonce,
            },
            delivery: ExecutorDelivery::PublicBroadcaster(candidate.clone()),
            inspection_assets: Vec::new(),
            recovery: Some(recovery),
        })
    }

    /// Reserve durably before deriving or querying the address. Failed and used
    /// reservations remain visible for recovery and never enter the free pool.
    pub async fn prepare_operation(
        &self,
        operation: ExecutorOperationId,
        delivery: ExecutorDelivery,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        assets: &[ExecutorAsset],
        purpose_summary: Option<&str>,
    ) -> Result<PreparedExecutorOperation> {
        self.prepare_reserved_operation(
            operation,
            delivery,
            authorization,
            assets,
            OperationReservation::Operation {
                purpose_summary,
                swap_approval: None,
                destination_operation: None,
            },
        )
        .await
    }

    fn reserve_operation(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        assets: &[ExecutorAsset],
        reservation: &OperationReservation<'_>,
    ) -> Result<crate::vault::ExecutorRecord> {
        Ok(match reservation {
            OperationReservation::Operation {
                purpose_summary,
                swap_approval,
                destination_operation,
            } => self.store.reserve_with_swap_approval(
                operation,
                delegate,
                *purpose_summary,
                assets,
                swap_approval.cloned(),
                *destination_operation,
            )?,
            OperationReservation::SwapDestination(destination) => self
                .store
                .reserve_swap_destination(operation, delegate, *destination)?,
        })
    }

    /// [`Self::prepare_operation`] with the reservation `reservation` describes. A reservation
    /// that already exists keeps its own approval.
    pub(super) async fn prepare_reserved_operation(
        &self,
        operation: ExecutorOperationId,
        delivery: ExecutorDelivery,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        assets: &[ExecutorAsset],
        reservation: OperationReservation<'_>,
    ) -> Result<PreparedExecutorOperation> {
        self.require_executor_authorization(
            authorization,
            &super::HardwareExecutorAction::Execute(operation),
        )?;
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "activity_lock", "started");
        let guard = self.lock_activity().await;
        tracing::info!(
            target: "executor_preparation",
            step = "activity_lock",
            elapsed_ms = started.elapsed().as_millis(),
            "finished"
        );
        self.ensure_active()?;
        let profile = self
            .chain
            .accepted_executor_profile()
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        let profile = delivery.admit(profile)?;
        if let ExecutorDelivery::SelfBroadcast { sender, .. } = &delivery
            && self
                .vault
                .list_public_accounts_for_session(&self.view, true)?
                .iter()
                .any(|account| {
                    account.address == *sender
                        && account.source == crate::vault::PublicAccountSource::HardwareDerived
                })
        {
            return Err(eyre!(
                "Hardware-native Public accounts cannot pay gas for executor actions. Select a software or imported payer, or a broadcaster."
            ));
        }
        if let (
            crate::DesktopPrivateSpendAuthorization::HardwareExecutor(hardware),
            ExecutorDelivery::SelfBroadcast { sender, .. },
        ) = (authorization, &delivery)
        {
            hardware.require_gas_payer(*sender)?;
        }
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "reserve_and_derive", "started");
        let record = self.reserve_operation(operation, profile.delegate(), assets, &reservation)?;
        require_unfinished_operation(&record)?;
        if record.is_retired() {
            return Err(eyre!(
                "executor reservation is retired; select it through recovery"
            ));
        }
        let spare = self
            .store
            .reserve_spare(profile.delegate())
            .inspect_err(|_| {
                tracing::debug!(
                    "executor spare allocation deferred until the next authorized action"
                );
            })
            .ok();
        let spare_index = spare
            .as_ref()
            .and_then(|spare| spare.address().is_none().then_some(spare.index()));
        let (address, spare_address) = self.authorized_executor_addresses(
            authorization,
            operation,
            record.index(),
            spare_index,
        )?;
        self.ensure_active()?;
        self.store.bind_address(operation, address)?;
        if let Some((index, address)) = spare_index.zip(spare_address) {
            self.store.bind_spare_address(index, address)?;
        }
        self.notify_change();
        tracing::info!(
            target: "executor_preparation",
            step = "reserve_and_derive",
            elapsed_ms = started.elapsed().as_millis(),
            "finished"
        );
        drop(guard);
        let started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "chain_inspection", "started");
        let result = if record.issued().is_empty() {
            self.unused_inspection(record.index(), address, assets)
                .await
        } else {
            self.while_active(inspect_for_signing(
                &self.chain,
                &self.http,
                address,
                assets,
            ))
            .await
            .map(|(inspection, observed)| {
                std::sync::Arc::new(super::spare::CheckedExecutor {
                    inspection,
                    observed,
                    revision: None,
                })
            })
        };
        tracing::info!(
            target: "executor_preparation",
            step = "chain_inspection",
            elapsed_ms = started.elapsed().as_millis(),
            success = result.is_ok(),
            "finished"
        );
        let checked = result?;
        let inspection = &checked.inspection;
        let observed = checked.observed;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        checked.ensure_valid()?;
        let record = self.reserve_operation(operation, profile.delegate(), assets, &reservation)?;
        require_unfinished_operation(&record)?;
        if record.is_retired() {
            return Err(eyre!(
                "executor reservation is retired; select it through recovery"
            ));
        }
        if record.issued().is_empty()
            && (inspection.activity() != ExecutorActivity::NoObservedActivity
                || observed.nonce() != U256::ZERO)
        {
            self.store.retire(operation)?;
            return Err(ExecutorAccountUsed { operation }.into());
        }
        Ok(PreparedExecutorOperation {
            operation,
            generation: self.generation,
            wallet_id: self.view.wallet_id().to_owned(),
            owner: self.closed.clone(),
            context: ExecutorContext {
                chain_id: profile.chain_id(),
                executor: address,
                delegate: profile.delegate(),
                execution_nonce: observed.nonce(),
            },
            delivery,
            inspection_assets: assets.to_vec(),
            recovery: None,
        })
    }

    /// Sign the final proved call under current spend authorization. The shared
    /// context checks proof binding and hashes all transactions and action data.
    pub async fn issue_operation(
        &self,
        prepared: &PreparedExecutorOperation,
        call: &TransactionCall,
        inputs: &[Utxo],
        authorization: &crate::DesktopPrivateSpendAuthorization,
    ) -> Result<IssuedExecutorTransaction> {
        self.issue_operation_with(prepared, call, inputs, authorization, None)
            .await
    }

    /// Issue one round of a paid fee-convergence loop. `retry` belongs to that
    /// loop; see [`IssueRetry`] for when a repriced round skips the chain reads.
    pub(crate) async fn issue_operation_retrying(
        &self,
        prepared: &PreparedExecutorOperation,
        call: &TransactionCall,
        inputs: &[Utxo],
        authorization: &crate::DesktopPrivateSpendAuthorization,
        retry: &mut IssueRetry,
    ) -> Result<IssuedExecutorTransaction> {
        self.issue_operation_with(prepared, call, inputs, authorization, Some(retry))
            .await
    }

    async fn issue_operation_with(
        &self,
        prepared: &PreparedExecutorOperation,
        call: &TransactionCall,
        inputs: &[Utxo],
        authorization: &crate::DesktopPrivateSpendAuthorization,
        retry: Option<&mut IssueRetry>,
    ) -> Result<IssuedExecutorTransaction> {
        self.issue_payload(prepared, authorization, retry, |_| {
            let hash = prepared.context.signing_hash(call)?;
            // The durable reservation must cover precisely the private inputs whose
            // nullifiers the signed execute can spend, including every inner transaction.
            let decoded = RelayAdapt7702::executeCall::abi_decode(&call.data)?;
            if let Some(recovery) = &prepared.recovery
                && (decoded._actionData.minGasLimit != U256::ZERO
                    || decoded._actionData.calls.len() != recovery.calls().len()
                    || decoded._actionData.calls.iter().zip(recovery.calls()).any(
                        |(actual, expected)| {
                            actual.to != expected.to
                                || actual.value != expected.value
                                || actual.data != expected.data
                        },
                    ))
            {
                return Err(eyre!(
                    "proved recovery calls differ from the reviewed actions"
                ));
            }
            let mut expected = decoded
                ._transactions
                .iter()
                .flat_map(|transaction| {
                    transaction.nullifiers.iter().map(|nullifier| {
                        (u32::from(transaction.boundParams.treeNumber), *nullifier)
                    })
                })
                .collect::<Vec<_>>();
            let nullifying_key = self.view.scan_keys().nullifying_key;
            let mut supplied = inputs
                .iter()
                .map(|input| (input.tree, B256::from(input.nullifier(nullifying_key))))
                .collect::<Vec<_>>();
            expected.sort_unstable();
            supplied.sort_unstable();
            if expected != supplied || supplied.is_empty() {
                return Err(eyre!(
                    "executor input reservations do not match the proved transaction"
                ));
            }
            if self.records()?.iter().any(|other| {
                other.operation() != prepared.operation
                    && other
                        .reserved_inputs()
                        .iter()
                        .any(|reserved| inputs.iter().any(|input| reserved.matches(input)))
            }) {
                return Err(eyre!(
                    "private inputs are reserved by another executor operation"
                ));
            }
            Ok(PayloadToSign {
                hash,
                value: U256::ZERO,
                inputs: inputs
                    .iter()
                    .map(ExecutorInputIdentity::from_utxo)
                    .collect(),
                authorize_delegation: false,
                authorize: Box::new(move |signature| {
                    Ok(prepared.context.authorize_call(call, signature)?)
                }),
            })
        })
        .await
    }

    /// Issue one execute payload under the shared executor admission, inspection,
    /// signing and persistence rules. `payload` runs under the activity lock right
    /// after the preparation is validated and before any chain read.
    pub(super) async fn issue_payload<'a>(
        &self,
        prepared: &PreparedExecutorOperation,
        authorization: &crate::DesktopPrivateSpendAuthorization,
        mut retry: Option<&mut IssueRetry>,
        payload: impl FnOnce(&crate::vault::ExecutorRecord) -> Result<PayloadToSign<'a>>,
    ) -> Result<IssuedExecutorTransaction> {
        self.ensure_active()?;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self.validate_preparation(prepared)?;
        let chain = if prepared.recovery.is_some() {
            let mut chain = self
                .chain_for_delegate(record.delegate())
                .ok_or_else(|| eyre!("chain does not support Railgun"))?;
            chain.enabled = true;
            chain
        } else {
            self.chain.clone()
        };
        let chain = Box::new(chain);
        let profile = ExecutorProfile::accepted(chain.chain_id, record.delegate())
            .ok_or_else(|| eyre!("executor execution is unavailable for this configuration"))?;
        let PayloadToSign {
            hash,
            value,
            inputs,
            authorize_delegation: require_authorization,
            authorize,
        } = payload(&record)?;
        let initially_unissued = prepared.recovery.is_none() && record.issued().is_empty();
        let reused = match retry.as_deref_mut() {
            Some(retry) if !initially_unissued => {
                let reused = retry.reusable(prepared, &record);
                if reused.is_none() {
                    // Once this loop needs fresh evidence, later rounds need it too.
                    retry.seed = None;
                }
                reused
            }
            _ => None,
        };
        let checked = if let Some(checked) = &reused {
            Arc::clone(checked)
        } else if initially_unissued {
            self.unused_inspection(
                record.index(),
                prepared.context.executor,
                &prepared.inspection_assets,
            )
            .await?
        } else {
            let (inspection, observed) = if let Some(recovery) = &prepared.recovery {
                // Keep this larger branch out of the setup/signing future, including
                // operations that never take the recovery path.
                self.while_active(Box::pin(
                    crate::desktop::executor_discovery::inspect_for_recovery_batch(
                        &chain,
                        &self.http,
                        prepared.context.executor,
                        &[recovery.asset()],
                    ),
                ))
                .await?
            } else {
                self.while_active(inspect_for_signing(
                    &chain,
                    &self.http,
                    prepared.context.executor,
                    &[],
                ))
                .await?
            };
            std::sync::Arc::new(super::spare::CheckedExecutor {
                inspection,
                observed,
                revision: None,
            })
        };
        let inspection = &checked.inspection;
        let observed = checked.observed;
        checked.ensure_valid()?;
        if observed.nonce() != prepared.context.execution_nonce {
            return Err(eyre!(
                "executor nonce changed; rebuild the approved operation"
            ));
        }
        if initially_unissued
            && (inspection.activity() != ExecutorActivity::NoObservedActivity
                || !observed.nonce().is_zero())
        {
            self.store.retire(prepared.operation)?;
            return Err(ExecutorAccountUsed {
                operation: prepared.operation,
            }
            .into());
        }
        if let Some(recovery) = &prepared.recovery {
            super::recovery::recovery_funding_admission(
                inspection,
                recovery.asset(),
                recovery.amount(),
                recovery.funding(),
                recovery.gas_limits(),
            )?;
            recovery.validate_inspection(inspection)?;
        }
        // A retry with issued payloads reads the account again at the inspection's block.
        // An unresolved earlier nonce keeps future-nonce signing blocked. A reused round
        // keeps the durable observation its evidence matched.
        if reused.is_none() {
            let reconciled = self.admit_signing_read(&record, &chain, observed).await?;
            if prepared.recovery.is_none() {
                require_unfinished_operation(&reconciled)?;
            }
        }
        let action = if let Some(recovery) = &prepared.recovery {
            self.recovery_authorization_action(authorization, recovery)?
        } else {
            super::HardwareExecutorAction::Execute(record.operation())
        };
        let signer = self.authorized_executor_signer(
            authorization,
            &action,
            record.operation(),
            record.index(),
        )?;
        if signer.address() != prepared.context.executor {
            return Err(eyre!(
                "executor signing identity does not match preparation"
            ));
        }
        self.ensure_active()?;
        let signed_call = authorize(signer.sign_hash_sync(&hash)?)?;
        let mut transaction = TransactionRequest::default()
            .to(signed_call.to)
            .input(signed_call.data.clone().into())
            .value(value);
        transaction.chain_id = Some(profile.chain_id());
        transaction.from = prepared.delivery.sender();
        authorize_delegation(
            &mut transaction,
            profile,
            inspection,
            &signer,
            prepared.recovery.is_some(),
            matches!(prepared.delivery, ExecutorDelivery::PublicBroadcaster(_))
                || require_authorization,
        )?;
        self.ensure_active()?;
        checked.ensure_valid()?;
        let payload_context = ExecutorPayloadContext::new(signed_call.data, observed, inputs);
        let payload = IssuedExecutorPayload::new(
            observed.nonce(),
            profile.delegate(),
            hash,
            if prepared.recovery.is_some() {
                ExecutorPayloadPurpose::Recovery
            } else {
                ExecutorPayloadPurpose::Operation
            },
            payload_context,
        );
        if let Some(recovery) = &prepared.recovery {
            self.store.record_recovery_issued(
                prepared.operation,
                recovery.expected_active_use,
                payload,
            )?;
        } else {
            self.store.record_issued(prepared.operation, payload)?;
        }
        self.unused
            .lock()
            .map_err(|_| eyre!("executor preparation is unavailable"))?
            .remove(record.index());
        if let Some(retry) = retry {
            if initially_unissued {
                retry.seed = Some(IssueRetrySeed {
                    operation: prepared.operation,
                    execution_nonce: prepared.context.execution_nonce,
                    checked: Arc::clone(&checked),
                    issued: BTreeSet::from([hash]),
                });
            } else if reused.is_some()
                && let Some(seed) = &mut retry.seed
            {
                seed.issued.insert(hash);
            }
        }
        self.ensure_active()?;
        self.notify_change();
        Ok(IssuedExecutorTransaction {
            operation: prepared.operation,
            payload_hash: hash,
            transaction,
        })
    }
}

/// Execution and recovery share the Ethereum authorization nonce rules. The
/// contract execution nonce is signed separately into their respective calls.
pub(super) fn authorize_delegation(
    transaction: &mut TransactionRequest,
    profile: ExecutorProfile,
    inspection: &ExecutorInspection,
    signer: &PrivateKeySigner,
    recovery: bool,
    require_authorization: bool,
) -> Result<()> {
    let code = inspection
        .code()
        .ok_or_else(|| eyre!("executor delegation is unavailable"))?;
    let installed = crate::desktop::executor_discovery::executor_delegation(code);
    let delegated = installed == Some(profile.delegate());
    // SDK broadcaster TX7702 requests require an authorization even when this
    // delegate is already installed. Self-broadcast execution reuses an installed
    // delegate without one unless its payload requires it, as a native shield does.
    // An account on another delegate of this capability moves to the delivery's delegate.
    if code.is_empty()
        || !delegated
            && (recovery && installed.is_some() || matches_executor_delegation(code, profile))
        || require_authorization && delegated
    {
        let account_nonce = inspection
            .account_nonce()
            .ok_or_else(|| eyre!("executor account nonce is unavailable"))?;
        let nonce = if transaction.from == Some(signer.address()) {
            account_nonce
                .checked_add(1)
                .ok_or_else(|| eyre!("executor account nonce is exhausted"))?
        } else {
            account_nonce
        };
        let authorization = Authorization {
            chain_id: U256::from(profile.chain_id()),
            address: profile.delegate(),
            nonce,
        };
        let signature = signer.sign_hash_sync(&authorization.signature_hash())?;
        transaction.authorization_list = Some(vec![authorization.into_signed(signature)]);
        transaction.transaction_type = Some(4);
    } else if !delegated {
        return Err(eyre!("executor has an unrecognized delegation"));
    }
    if transaction.from == Some(signer.address()) {
        transaction.nonce = inspection.account_nonce();
    }
    Ok(())
}

/// `request`'s gas estimate. A type-4 request is checked against RPCs that silently ignore
/// authorizations and simulate an empty account, which estimates only intrinsic and calldata
/// gas and leaves the executor's execution out of the limit.
pub(crate) async fn estimate_delegated_gas(
    provider: &impl Provider,
    request: &TransactionRequest,
) -> Result<u64> {
    let delegated = request.transaction_type == Some(4);
    if delegated {
        // Require the executor's getter to return ABI data through this delegation. The
        // getter takes no value.
        let nonce_request = request
            .clone()
            .with_value(U256::ZERO)
            .with_input(RelayAdapt7702::nonceCall {}.abi_encode());
        let nonce_result = provider
            .call(nonce_request)
            .pending()
            .await
            .wrap_err("simulate executor delegation")?;
        RelayAdapt7702::nonceCall::abi_decode_returns(&nonce_result)
            .wrap_err("RPC did not simulate executor delegation")?;
    }
    let estimated_gas = provider
        .estimate_gas(request.clone())
        .await
        .wrap_err("estimate gas")?;
    if delegated {
        // eth_call and eth_estimateGas can have different authorization support.
        // Check the estimate before a buffer can conceal an execution shortfall.
        provider
            .call(request.clone().with_gas_limit(estimated_gas))
            .pending()
            .await
            .wrap_err("simulate transaction at estimated gas")?;
    }
    Ok(estimated_gas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{WalletSettings, build_effective_chain_configs};

    #[test]
    fn unwrap_and_top_up_route_through_the_executor_or_are_unavailable() {
        let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let accepted = chains.get(1).unwrap().clone();
        let profile = accepted.accepted_executor_profile().unwrap();
        let mut unaccepted = accepted.clone();
        unaccepted
            .railgun
            .as_mut()
            .unwrap()
            .deployment
            .relay_adapt_7702_contract = Address::repeat_byte(7);
        let mut disabled = accepted.clone();
        disabled.enabled = false;
        // (unwrap, top-up, sponsored, executor owner, chain, route when required)
        let cases = [
            (true, false, false, true, &accepted, Some(Ok(profile))),
            (false, true, false, true, &accepted, Some(Ok(profile))),
            (
                true,
                false,
                false,
                false,
                &accepted,
                Some(Err(ExecutorRouteUnavailable::NoExecutorOwner)),
            ),
            (
                false,
                true,
                false,
                true,
                &unaccepted,
                Some(Err(ExecutorRouteUnavailable::NoAcceptedProfile)),
            ),
            (
                true,
                false,
                false,
                true,
                &disabled,
                Some(Err(ExecutorRouteUnavailable::NoAcceptedProfile)),
            ),
            (true, true, true, false, &unaccepted, None),
            (false, false, false, true, &accepted, None),
        ];
        for (unwrap, top_up, sponsored, owner, chain, expected) in cases {
            let required = unshield_requires_executor(unwrap, top_up, sponsored);
            assert_eq!(required, expected.is_some());
            if let Some(route) = expected {
                assert_eq!(unshield_executor_route(owner, chain), route);
            }
        }
    }

    /// RPCs that ignore authorizations simulate an empty account: `eth_call` returns no data
    /// and `eth_estimateGas` leaves out the executor's execution. Neither estimate is used.
    #[tokio::test]
    async fn delegated_gas_rejects_rpcs_that_ignore_authorizations() {
        use alloy::primitives::Bytes;
        use alloy::providers::{Provider, ProviderBuilder};
        use alloy::transports::mock::Asserter;

        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        let delegated = TransactionRequest {
            transaction_type: Some(4),
            ..Default::default()
        };
        let nonce = Bytes::from(RelayAdapt7702::nonceCall::abi_encode_returns(&U256::ZERO));

        // eth_call ignores the authorization.
        responses.push_success(&Bytes::new());
        assert!(estimate_delegated_gas(&provider, &delegated).await.is_err());

        // eth_estimateGas ignores it, so the request fails at the estimate.
        responses.push_success(&nonce);
        responses.push_success(&"0x5352");
        responses.push_failure_msg("out of gas");
        assert!(estimate_delegated_gas(&provider, &delegated).await.is_err());

        responses.push_success(&nonce);
        responses.push_success(&"0xbea0");
        responses.push_success(&Bytes::new());
        assert_eq!(
            estimate_delegated_gas(&provider, &delegated).await.unwrap(),
            48_800
        );

        // Other requests are estimated as they are.
        responses.push_success(&"0x5208");
        assert_eq!(
            estimate_delegated_gas(&provider, &TransactionRequest::default())
                .await
                .unwrap(),
            21_000
        );
        assert!(responses.read_q().is_empty());
    }

    #[tokio::test]
    async fn delegation_authorization_moves_an_account_to_the_delivery_delegate() {
        use alloy::eips::BlockNumHash;
        use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
        use alloy::primitives::Bytes;
        use alloy::providers::{Provider, ProviderBuilder};
        use alloy::transports::mock::Asserter;

        let chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
        let primary = chain.accepted_executor_profile().unwrap();
        let earlier = broadcaster_core::deployment::RailgunDeployment::for_chain(1)
            .unwrap()
            .relay_adapt_7702_delegates[0]
            .address;
        let earlier = ExecutorProfile::accepted(1, earlier).unwrap();
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(7)).unwrap();
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        // (installed delegate, delivery's target, broadcaster delivery, authorized delegate)
        for (installed, target, broadcaster, authorized) in [
            // Self-broadcast moves an account on the earlier delegate to the primary.
            (earlier.delegate(), primary, false, Some(primary.delegate())),
            (primary.delegate(), primary, false, None),
            // A broadcaster advertising the earlier delegate moves the account to it.
            (primary.delegate(), earlier, true, Some(earlier.delegate())),
            (Address::repeat_byte(2), primary, false, None),
        ] {
            let listed = installed != Address::repeat_byte(2);
            responses.push_success(&"0x2");
            responses.push_success(&Bytes::from(
                [
                    EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                    installed.as_slice(),
                ]
                .concat(),
            ));
            responses.push_success(&"0x0");
            if listed {
                responses.push_success(&Bytes::from(
                    RelayAdapt7702::nonceCall::abi_encode_returns(&U256::ZERO),
                ));
            }
            let inspection = crate::desktop::executor_discovery::inspect_at(
                &provider,
                &chain,
                signer.address(),
                &[],
                BlockNumHash::new(10, B256::repeat_byte(10)),
                false,
            )
            .await;
            let mut transaction = TransactionRequest::default();
            let result = authorize_delegation(
                &mut transaction,
                target,
                &inspection,
                &signer,
                false,
                broadcaster,
            );
            // An unlisted delegation is never authorized away outside recovery.
            assert_eq!(result.is_ok(), listed);
            assert_eq!(
                transaction
                    .authorization_list
                    .map(|list| list[0].inner().address),
                authorized
            );
        }
    }
}
