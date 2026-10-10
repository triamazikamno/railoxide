use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::panic::Location;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes};
use eyre::{Result, eyre};
use sync_service::WalletHandle;
use tokio::sync::{Mutex, MutexGuard, watch};

use super::executor_observation::{ExecutorAccountRead, read_executor_account, trace_step};
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    DesktopVaultStore, DesktopViewSession, ExecutorNonceObservation, ExecutorOperationId,
    ExecutorRecord, ExecutorStore, SwapUseId,
};
use crate::{ExecutorAsset, ExecutorInspection, HttpContext, WalletSyncTip, inspect_executor};

mod attribution;
pub use attribution::{
    ExecutorAttributedAction, ExecutorAttribution, ExecutorAttributionEvidence,
    ExecutorPayloadOutcome, ExecutorSignedAction, InvalidatedSwapOrders, attributed_action,
    payload_outcome, signed_actions,
};
mod authorization;
pub use authorization::{
    HardwareExecutorAction, HardwareExecutorAuthorization, HardwareExecutorAuthorizationRequest,
};
mod confirmations;
mod discovery;
mod execution;
mod locks;
mod observation;
mod public_account;
mod recovery;
mod spare;
mod status;
mod swap;
pub use discovery::ExecutorDiscoveryReport;
pub use execution::*;
pub use locks::{
    ExecutorInputLock, ExecutorInputLockKind, ExecutorInputLockReason, LockedNotes, WalletNoteLocks,
};
pub use observation::ExecutorTransactionIdentity;
pub(crate) use public_account::ExecutorPublicSigningGuard;
pub use recovery::*;
pub use status::{ExecutorAccountOutcome, ExecutorAccountStatus};
pub(crate) use swap::Transfer;
pub use swap::is_swap_destination_record;
#[cfg(test)]
pub(crate) use swap::submit_swap_pair_setups_with;
pub use swap::{
    AuthorizedPublicSwapSource, PUBLIC_ACROSS_DEPOSIT_GAS_UNITS,
    PUBLIC_PROXY_DEPLOYING_WITHDRAWAL_GAS_UNITS, PUBLIC_PROXY_WITHDRAWAL_GAS_UNITS,
    PublicSwapApprovalPlan, PublicSwapApprovalsOutcome, PublicSwapGasPlan, PublicSwapPermitPlan,
    PublicSwapPermitTerms, PublicSwapSource, PublicSwapTransactionOutcome,
    PublicSwapWithdrawalReview, public_swap_gas_plan,
};
pub use swap::{
    BridgeLegPrice, DelegatedSwapExecutor, SwapAccountCandidate, SwapAmountPlan, SwapAmountRequest,
    SwapBridgeClients, SwapBridgeQuote, SwapBridgeRoute, SwapDestinationContext, SwapExecutor,
    SwapInputPlan, SwapOrderOutcome, SwapOrderRequest, SwapOrderState, SwapPrice,
    SwapPrivateBridgeQuote, SwapReview, SwapReviewChange, SwapReviewRequest, SwapSetupRequest,
    SwapSetupStatus, SwapUseClaim, is_swap_record, swap_order_state, swap_setup_recorded_executed,
    swap_setup_status, swap_submission_outcome,
};
pub use swap::{
    PreparedSwapPair, SwapPairPreparation, SwapPairSetupResults, SwapPairSide, prepare_swap_pair,
    submit_swap_pair_setups,
};
pub use swap::{
    PublicSwapBatchTerms, PublicSwapOrderOutcome, PublicSwapOrderRequest, PublicSwapReview,
    PublicSwapReviewRequest, PublicSwapUnavailable, new_public_swap_batch_nonce,
    public_swap_batch_terms,
};
pub use swap::{
    PublicSwapDelivery, PublicSwapDeliveryQuote, PublicSwapDeliverySigning, PublicSwapUseClaim,
    SwapShieldNotes,
};
pub use swap::{PublicSwapOrderState, public_swap_order_state};
pub use swap::{PublicSwapProgress, PublicSwapTracking};
#[cfg(test)]
pub(crate) use swap::{
    SwapDestinationSigning, SwapOrderSigning, SwapOutputPoiSink, notes_of_shield, plan_swap_inputs,
    price_swap_review, reusable_swap_proof, swap_cancellation_admitted, swap_invalidation,
    swap_recovery_calls,
};

pub struct ExecutorReconciliationReport {
    record: ExecutorRecord,
    /// The executor's code and the confirmed canonical block it was read at.
    code: (BlockNumHash, Bytes),
}

/// The account changed while work used an earlier snapshot. Background observers may
/// discard the result and read again; signing and other explicit actions still fail.
#[derive(Debug, thiserror::Error)]
#[error("The stealth account changed during preparation. Review the operation again.")]
pub struct ExecutorRecordChanged;

impl ExecutorReconciliationReport {
    #[must_use]
    pub const fn record(&self) -> &ExecutorRecord {
        &self.record
    }
}

/// One wallet-chain's executor work. Closing stops admission immediately; shutdown
/// also waits for the active operation to release its capabilities and network work.
pub struct ExecutorOwner {
    generation: u64,
    view: Arc<DesktopViewSession>,
    vault: DesktopVaultStore,
    store: ExecutorStore,
    chain: EffectiveChainConfig,
    http: HttpContext,
    endpoints: super::executor_observation::ObservationEndpoints,
    closed: watch::Sender<bool>,
    activity: Arc<Mutex<()>>,
    /// The latest confirmed read of each account's execution nonce applied in this session.
    account_reads: StdMutex<BTreeMap<ExecutorOperationId, ExecutorNonceObservation>>,
    submission_blocks: StdMutex<BTreeMap<ExecutorOperationId, BTreeMap<B256, Option<u64>>>>,
    /// What this session found of each sold token's permit, by chain and token: its typed-data
    /// domain, or that it has none. A read that failed is not kept.
    permit_support: StdMutex<BTreeMap<(u64, Address), swap::PermitSupport>>,
    /// The swaps whose open order's signed permit was used up while the allowance is short.
    permit_used_up: StdMutex<BTreeSet<SwapUseId>>,
    tip_observation_join: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    confirmation_observation_join: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    /// Confirmation observer inputs, reused by foreground account reads until close.
    synced_observation: StdMutex<Option<(WalletHandle, watch::Receiver<WalletSyncTip>)>>,
    unused: StdMutex<spare::UnusedInspections>,
    changes: watch::Sender<u64>,
    /// Notes that UI tests plan swaps from in place of the session's, which doesn't sync.
    #[cfg(feature = "test-support")]
    swap_notes_for_tests: StdMutex<Vec<railgun_wallet::Utxo>>,
}

struct ExecutorActivityGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    acquired: Instant,
    caller: &'static Location<'static>,
    span: tracing::Span,
}

impl Drop for ExecutorActivityGuard<'_> {
    fn drop(&mut self) {
        let held = self.acquired.elapsed();
        if held >= Duration::from_millis(250) {
            self.span.in_scope(|| {
                tracing::debug!(target: "executor_observation", step = "activity_hold",
                    elapsed_ms = held.as_millis(), caller = %self.caller, "finished");
            });
        }
    }
}

impl ExecutorOwner {
    pub(crate) fn new(
        generation: u64,
        db: Arc<local_db::DbStore>,
        view: Arc<DesktopViewSession>,
        chain: EffectiveChainConfig,
        http: HttpContext,
    ) -> Result<Self> {
        let store = ExecutorStore::new(db.clone(), view.clone(), chain.chain_id)?;
        // Settle destination accounts and retire orphaned reservations before admitting work.
        // A failure leaves orphan cleanup for the next load.
        if store.reconcile_swap_destinations_on_load().is_err() {
            tracing::debug!(target: "executor_observation", step = "swap_destinations", "failed");
        }
        Ok(Self {
            generation,
            view,
            vault: DesktopVaultStore::from_db(db),
            store,
            endpoints: super::executor_observation::ObservationEndpoints::new(&chain, &http),
            chain,
            http,
            closed: watch::channel(false).0,
            activity: Arc::new(Mutex::new(())),
            account_reads: StdMutex::new(BTreeMap::new()),
            submission_blocks: StdMutex::new(BTreeMap::new()),
            permit_support: StdMutex::new(BTreeMap::new()),
            permit_used_up: StdMutex::new(BTreeSet::new()),
            tip_observation_join: StdMutex::new(None),
            confirmation_observation_join: StdMutex::new(None),
            synced_observation: StdMutex::new(None),
            unused: StdMutex::new(spare::UnusedInspections::default()),
            changes: watch::channel(0).0,
            #[cfg(feature = "test-support")]
            swap_notes_for_tests: StdMutex::new(Vec::new()),
        })
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    /// Configure reads of a recorded account for its delegate's nonce layout.
    fn chain_for_delegate(&self, delegate: Address) -> Option<EffectiveChainConfig> {
        let mut chain = self.chain.clone();
        chain.railgun.as_mut()?.deployment.relay_adapt_7702_contract = delegate;
        Some(chain)
    }

    /// Also releases the observation endpoints, so their background admission stops.
    pub(crate) fn close(&self) {
        self.closed.send_replace(true);
        self.endpoints.release();
        self.synced_observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    pub(crate) async fn shutdown(&self) {
        self.close();
        let join = self
            .tip_observation_join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(join) = join {
            let _ = join.await;
        }
        let join = self
            .confirmation_observation_join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(join) = join {
            let _ = join.await;
        }
        self.unused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate();
        let _guard = self.activity.lock().await;
    }

    #[track_caller]
    fn lock_activity(&self) -> impl Future<Output = ExecutorActivityGuard<'_>> {
        let caller = Location::caller();
        async move {
            // Routine polling stays quiet unless it waits or holds the lock for a while.
            let guard = if let Ok(guard) = self.activity.try_lock() {
                guard
            } else {
                let started = Instant::now();
                tracing::debug!(target: "executor_observation", step = "activity_lock",
                    caller = %caller, "started");
                let guard = self.activity.lock().await;
                tracing::debug!(target: "executor_observation", step = "activity_lock",
                    elapsed_ms = started.elapsed().as_millis(), caller = %caller, "finished");
                guard
            };
            ExecutorActivityGuard {
                _guard: guard,
                acquired: Instant::now(),
                caller,
                span: tracing::Span::current(),
            }
        }
    }

    /// Read durable history, including observations at head minus the configured finality
    /// depth. Restart does not invalidate these facts or their input-reservation decisions.
    /// Signing and recovery admission still inspect and reconcile the selected account.
    pub fn records(&self) -> Result<Vec<ExecutorRecord>> {
        self.ensure_active()?;
        Ok(self.store.records()?)
    }

    /// Called on the destination chain's owner. Records what became of each account's shield
    /// payload from its origin swap, preserving unlinked reservations still being prepared.
    /// Returns whether any record changed.
    pub fn reconcile_swap_destinations(&self) -> Result<bool> {
        self.ensure_active()?;
        let changed = self.store.reconcile_swap_destinations()?;
        if changed {
            self.notify_change();
        }
        Ok(changed)
    }

    pub fn set_hidden(&self, operation: ExecutorOperationId, hidden: bool) -> Result<()> {
        self.ensure_active()?;
        self.store.set_hidden(operation, hidden)?;
        self.notify_change();
        Ok(())
    }

    pub(crate) fn available_inputs(
        &self,
        inputs: Vec<railgun_wallet::Utxo>,
    ) -> Result<Vec<railgun_wallet::Utxo>> {
        self.filter_reserved_inputs(inputs, None)
    }

    pub(crate) fn inputs_for_preparation(
        &self,
        inputs: Vec<railgun_wallet::Utxo>,
        prepared: &PreparedExecutorOperation,
    ) -> Result<Vec<railgun_wallet::Utxo>> {
        let record = self.validate_preparation(prepared)?;
        if prepared.is_recovery() {
            self.inputs_for_recovery(inputs, &record)
        } else {
            self.inputs_for_record(inputs, &record)
        }
    }

    /// Fee inputs of a recovery, including a swap's early cancellation. It competes with the
    /// swap's own pre-hook for its nonce, so it never spends the notes that pre-hook reserves.
    pub(crate) fn inputs_for_recovery(
        &self,
        inputs: Vec<railgun_wallet::Utxo>,
        record: &ExecutorRecord,
    ) -> Result<Vec<railgun_wallet::Utxo>> {
        let mut inputs = self.inputs_for_record(inputs, record)?;
        let reserved = record.swap_reserved_inputs();
        inputs.retain(|input| !reserved.iter().any(|reserved| reserved.matches(input)));
        Ok(inputs)
    }

    /// Inputs this operation may spend: the reservation of its own pending payloads stays
    /// available.
    pub(crate) fn inputs_for_record(
        &self,
        mut inputs: Vec<railgun_wallet::Utxo>,
        record: &ExecutorRecord,
    ) -> Result<Vec<railgun_wallet::Utxo>> {
        // Whatever ran at a resolved nonce may have spent its inputs before private sync
        // publishes their nullifiers, so a later preparation leaves the inputs of every
        // payload at that nonce alone until the nonce is settled.
        inputs.retain(|input| {
            !record.issued().iter().any(|payload| {
                let held = if record.nonce_resolved(payload.nonce()) {
                    !record.nonce_settled(payload.nonce())
                } else {
                    // A hook runs inside a settlement, which the swap can observe before
                    // any account read resolves the hook's nonce.
                    record.swap().is_some_and(|swap| {
                        swap.orders().iter().any(|order| {
                            order.pre_hook().payload() == payload.hash()
                                && (order.observations().pre_hook_executed.is_some()
                                    || order.observations().delivered.is_some())
                        })
                    })
                };
                held && payload
                    .context()
                    .inputs()
                    .iter()
                    .any(|spent| spent.matches(input))
            })
        });
        self.filter_reserved_inputs(inputs, Some(record.operation()))
    }

    fn filter_reserved_inputs(
        &self,
        mut inputs: Vec<railgun_wallet::Utxo>,
        operation: Option<ExecutorOperationId>,
    ) -> Result<Vec<railgun_wallet::Utxo>> {
        let reserved = self
            .records()?
            .iter()
            .filter(|record| Some(record.operation()) != operation)
            .flat_map(ExecutorRecord::reserved_inputs)
            .collect::<Vec<_>>();
        inputs.retain(|input| !reserved.iter().any(|reserved| reserved.matches(input)));
        Ok(inputs)
    }

    /// Read the account's code and execution nonce at the confirmed tip and apply the nonce
    /// to its record, so every outcome that follows from it is current. No block contents,
    /// receipts or logs are read. A failed read is an error and leaves the record without
    /// a nonce observation, so nothing is admitted on an earlier one. Only a user action or
    /// an active operation calls this.
    ///
    /// The read is a fact about the chain, so it is applied to the record as it is by then.
    /// A caller that decides from the returned record checks it unchanged under activity.
    pub async fn reconcile_account(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorReconciliationReport> {
        self.ensure_active()?;
        // Private sync can resolve a payload with no chain read. Only owner closure is
        // fatal here.
        if trace_step(
            "account_synced_location",
            self.confirm_synced_operation(operation),
        )
        .await
        .is_err()
        {
            self.ensure_active()?;
        }
        let previous = {
            let _guard = self.lock_activity().await;
            self.ensure_active()?;
            self.begin_account_reconciliation(operation)?
        };
        let read = trace_step("account_read", self.read_account_state(&previous, None)).await?;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        self.apply_account_state(operation, read)
    }

    /// Network/proof work uses a snapshot. Call under activity before applying its result.
    fn require_record_unchanged(&self, previous: &ExecutorRecord) -> Result<()> {
        self.ensure_active()?;
        if self
            .store
            .records()?
            .iter()
            .find(|record| record.operation() == previous.operation())
            != Some(previous)
        {
            return Err(ExecutorRecordChanged.into());
        }
        Ok(())
    }

    /// [`Self::reconcile_account`] for a caller that holds activity through its signing
    /// guard.
    async fn reconcile_account_admitted(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorReconciliationReport> {
        self.ensure_active()?;
        let previous = self.begin_account_reconciliation(operation)?;
        let read = trace_step("account_read", self.read_account_state(&previous, None)).await?;
        self.apply_account_state(operation, read)
    }

    // Callers hold activity while invalidating or applying local projections.
    fn begin_account_reconciliation(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord> {
        let previous = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
            .ok_or_else(|| eyre!("executor operation is unavailable"))?;
        self.store.invalidate_observation(operation)?;
        Ok(previous)
    }

    /// One read of `record`'s account at `requested`, or at the confirmed tip, under its
    /// delegate's nonce layout.
    async fn read_account_state(
        &self,
        record: &ExecutorRecord,
        requested: Option<u64>,
    ) -> Result<ExecutorAccountRead> {
        let address = record
            .address()
            .ok_or_else(|| eyre!("executor address is unavailable"))?;
        let mut chain = self
            .chain_for_delegate(record.delegate())
            .ok_or_else(|| eyre!("chain does not support Railgun"))?;
        chain.enabled = true;
        self.while_active(read_executor_account(
            &self.endpoints,
            &chain,
            address,
            requested,
        ))
        .await
    }

    /// Callers hold activity. Code the execution nonce is not read under leaves the record
    /// without a nonce observation.
    fn apply_account_state(
        &self,
        operation: ExecutorOperationId,
        read: ExecutorAccountRead,
    ) -> Result<ExecutorReconciliationReport> {
        let record = if let Some(observed) = read.nonce {
            self.apply_account_read(operation, observed)?
        } else {
            self.date_submissions(operation, read.block.number)?;
            self.notify_change();
            self.store
                .records()?
                .into_iter()
                .find(|record| record.operation() == operation)
                .ok_or_else(|| eyre!("executor operation is unavailable"))?
        };
        Ok(ExecutorReconciliationReport {
            record,
            code: (read.block, read.code),
        })
    }

    /// Signing admission's account state, for a caller that holds activity. `observed` is
    /// the execution nonce the signing inspection read at its confirmed block. A record
    /// with no issued payload reuses it with no further read. Otherwise the account's code
    /// and nonce are read at that block, and the inspection's nonce stands only while the
    /// block is still canonical there. No block contents are read. A failed read admits
    /// nothing and leaves the record without a nonce observation.
    async fn admit_signing_read(
        &self,
        record: &ExecutorRecord,
        chain: &EffectiveChainConfig,
        observed: ExecutorNonceObservation,
    ) -> Result<ExecutorRecord> {
        let operation = record.operation();
        self.store.invalidate_observation(operation)?;
        if !record.issued().is_empty() {
            let address = record
                .address()
                .ok_or_else(|| eyre!("executor address is unavailable"))?;
            let read = self
                .while_active(read_executor_account(
                    &self.endpoints,
                    chain,
                    address,
                    Some(observed.block().number),
                ))
                .await?;
            if read.block != observed.block() {
                return Err(eyre!("executor signing block is no longer canonical"));
            }
            // Recovery reads the stored nonce of an account under another delegation, a
            // layout this read leaves undecoded. The inspection's nonce stands then.
            if read.nonce.is_some_and(|read| read != observed) {
                return Err(eyre!(
                    "executor chain observation changed; retry preparation"
                ));
            }
        }
        self.apply_account_read(operation, observed)
    }

    /// Date an observed submission from the first canonical head read after handoff,
    /// `confirmed` being the confirmed block of that read. Prefetch and stale session-tip
    /// heights cannot make it overdue.
    fn date_submissions(&self, operation: ExecutorOperationId, confirmed: u64) -> Result<()> {
        if let Some(submissions) = self
            .submission_blocks
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .get_mut(&operation)
        {
            for submitted in submissions.values_mut() {
                submitted.get_or_insert(confirmed.saturating_add(self.chain.finality_depth));
            }
        }
        Ok(())
    }

    /// Remember a confirmed read of an account's execution nonce that this session applied
    /// to its record. Account status reads it to say whether the account was read this
    /// session and whether a submission is overdue. A read older than the one held is left
    /// out.
    fn note_account_read(
        &self,
        operation: ExecutorOperationId,
        observed: ExecutorNonceObservation,
    ) -> Result<()> {
        let mut reads = self
            .account_reads
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?;
        if reads
            .get(&operation)
            .is_none_or(|held| held.block().number <= observed.block().number)
        {
            reads.insert(operation, observed);
        }
        Ok(())
    }

    /// Apply a confirmed read of an account's execution nonce to its record, date the
    /// submissions that waited for one, and keep the read for this session's status. Callers
    /// hold activity.
    fn apply_account_read(
        &self,
        operation: ExecutorOperationId,
        observed: ExecutorNonceObservation,
    ) -> Result<ExecutorRecord> {
        let record = self.store.record_account_read(operation, observed)?;
        self.date_submissions(operation, observed.block().number)?;
        self.note_account_read(operation, observed)?;
        self.notify_change();
        Ok(record)
    }

    /// One read of an account's code and execution nonce at the confirmed tip, applied to
    /// its record as it is by then, so every outcome that follows from the nonce is current.
    /// No block, receipt or log is read. Only a user action or an active operation calls
    /// this. Code the execution nonce is not read under leaves the record as it is.
    pub(super) async fn read_account(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord> {
        self.ensure_active()?;
        let record = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
            .ok_or_else(|| eyre!("stealth account is unavailable"))?;
        let read = trace_step("account_read", self.read_account_state(&record, None)).await?;
        let Some(observed) = read.nonce else {
            return Ok(record);
        };
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        self.apply_account_read(operation, observed)
    }

    /// Check balance, the account's status action: the balances of
    /// [`Self::inspect_record`], and one read of the account's execution nonce at the
    /// confirmed block, which updates the outcomes of everything signed for the account. A
    /// failed nonce read leaves those outcomes as they were and the balances stand.
    pub async fn check_record(
        &self,
        operation: ExecutorOperationId,
        assets: &[ExecutorAsset],
    ) -> Result<ExecutorInspection> {
        let inspection = self.inspect_record(operation, assets).await?;
        if self.read_account(operation).await.is_err() {
            self.ensure_active()?;
        }
        Ok(inspection)
    }

    /// Persisted addresses can be inspected with view access, without another key derivation.
    pub async fn inspect_record(
        &self,
        operation: ExecutorOperationId,
        assets: &[ExecutorAsset],
    ) -> Result<ExecutorInspection> {
        self.ensure_active()?;
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        let record = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
            .ok_or_else(|| eyre!("executor record is unavailable"))?;
        let address = record.address().ok_or_else(|| {
            eyre!("executor address is unavailable; authorize its derivation first")
        })?;
        let mut historical_chain = self
            .chain_for_delegate(record.delegate())
            .ok_or_else(|| eyre!("chain does not support Railgun"))?;
        historical_chain.enabled = true;
        self.while_active(inspect_executor(
            &historical_chain,
            &self.http,
            address,
            assets,
        ))
        .await
    }

    fn ensure_active(&self) -> Result<()> {
        if *self.closed.borrow() {
            Err(eyre!("executor wallet session has ended"))
        } else {
            Ok(())
        }
    }

    async fn while_active<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        let mut closed = self.closed.subscribe();
        self.ensure_active()?;
        tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => Err(eyre!("executor wallet session has ended")),
            result = work => {
                self.ensure_active()?;
                result
            }
        }
    }
}
