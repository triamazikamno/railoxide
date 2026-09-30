use std::collections::BTreeMap;
use std::future::Future;
use std::ops::Range;
use std::panic::Location;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use alloy::eips::BlockNumHash;
use alloy::primitives::{B256, Bytes};
use eyre::{Result, eyre};
use sync_service::WalletHandle;
use tokio::sync::{Mutex, MutexGuard, watch};

use super::executor_observation::trace_step;
use crate::settings::EffectiveChainConfig;
use crate::vault::{
    DesktopVaultStore, DesktopViewSession, ExecutorOperationId, ExecutorRecord, ExecutorStore,
};
use crate::{ExecutorAsset, ExecutorInspection, HttpContext, WalletSyncTip, inspect_executor};

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
pub use swap::{
    BridgeLegPrice, DelegatedSwapExecutor, SwapAccountCandidate, SwapAmountPlan, SwapAmountRequest,
    SwapBridgeClients, SwapBridgeQuote, SwapBridgeRoute, SwapExecutor, SwapInputPlan,
    SwapOrderOutcome, SwapOrderRequest, SwapOrderState, SwapPrice, SwapReview, SwapReviewChange,
    SwapReviewRequest, SwapSetupRequest, SwapSetupStatus, is_swap_record, swap_order_state,
    swap_setup_recorded_executed, swap_setup_status, swap_submission_outcome,
};
#[cfg(test)]
pub(crate) use swap::{
    SwapOrderSigning, SwapOutputPoiSink, plan_swap_inputs, price_swap_review, reusable_swap_proof,
    swap_cancellation_admitted, swap_invalidation, swap_recovery_calls,
};

pub struct ExecutorReconciliationReport {
    record: ExecutorRecord,
    /// The executor's code and the canonical block it was read at, if the history read it.
    code: Option<(BlockNumHash, Bytes)>,
}

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
    history_coverage: StdMutex<BTreeMap<ExecutorOperationId, status::HistoryCoverage>>,
    submission_blocks: StdMutex<BTreeMap<ExecutorOperationId, BTreeMap<B256, Option<u64>>>>,
    tip_observation_join: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    confirmation_observation_join: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    /// Confirmation observer inputs, reused by foreground history reads until close.
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
            history_coverage: StdMutex::new(BTreeMap::new()),
            submission_blocks: StdMutex::new(BTreeMap::new()),
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

    /// Inputs this operation may spend: its own reservation stays available.
    pub(crate) fn inputs_for_record(
        &self,
        mut inputs: Vec<railgun_wallet::Utxo>,
        record: &ExecutorRecord,
    ) -> Result<Vec<railgun_wallet::Utxo>> {
        // A later preparation must not reuse inputs spent by an earlier winner
        // during the interval before private sync publishes its nullifiers.
        inputs.retain(|input| {
            !record.issued().iter().any(|payload| {
                let executed = record.payload_status(payload.hash())
                    == Some(crate::vault::ExecutorPayloadStatus::Executed)
                    // Hooks inside settlements have no direct-call receipt.
                    || record.swap().is_some_and(|swap| {
                        swap.orders().iter().any(|order| {
                            order.pre_hook().payload() == payload.hash()
                                && (order.observations().pre_hook_executed.is_some()
                                    || order.observations().delivered.is_some())
                        })
                    });
                executed
                    && payload
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

    /// Reconcile an explicit block page and revalidate any previously observed
    /// inclusions. Partial coverage does not imply that every payload was resolved.
    pub async fn reconcile_history(
        &self,
        operation: ExecutorOperationId,
        range: Range<u64>,
    ) -> Result<ExecutorReconciliationReport> {
        self.ensure_active()?;
        // A synced location can record an inclusion from one block before the page
        // read below. Only owner closure is fatal; the page read reloads the record.
        if trace_step(
            "history_synced_location",
            self.confirm_synced_operation(operation),
        )
        .await
        .is_err()
        {
            self.ensure_active()?;
        }
        let guard = self.lock_activity().await;
        self.ensure_active()?;
        let (previous, pending) = trace_step("history_load", async {
            let previous = self.begin_history_reconciliation(operation)?;
            let pending = self
                .store
                .records()?
                .into_iter()
                .find(|record| record.operation() == operation)
                .ok_or_else(|| eyre!("executor operation is unavailable"))?;
            Ok::<_, eyre::Report>((previous, pending))
        })
        .await?;
        drop(guard);
        let observed =
            trace_step("history_read", self.read_history(&previous, range.clone())).await?;
        let _guard = self.lock_activity().await;
        self.require_record_unchanged(&pending)?;
        trace_step("history_apply", async {
            self.apply_history_reconciliation(operation, range, &observed)
        })
        .await
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
            return Err(eyre!(
                "The stealth account changed during preparation. Review the operation again."
            ));
        }
        Ok(())
    }

    async fn reconcile_history_admitted(
        &self,
        operation: ExecutorOperationId,
        range: Range<u64>,
    ) -> Result<ExecutorReconciliationReport> {
        self.ensure_active()?;
        let previous = trace_step("history_load", async {
            self.begin_history_reconciliation(operation)
        })
        .await?;
        let observed =
            trace_step("history_read", self.read_history(&previous, range.clone())).await?;
        trace_step("history_apply", async {
            self.apply_history_reconciliation(operation, range, &observed)
        })
        .await
    }

    // Callers hold activity while invalidating or applying local projections.
    fn begin_history_reconciliation(
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

    async fn read_history(
        &self,
        previous: &ExecutorRecord,
        range: Range<u64>,
    ) -> Result<super::executor_observation::ExecutorHistoryObservation> {
        let mut historical_chain = self.chain.clone();
        historical_chain
            .railgun
            .as_mut()
            .ok_or_else(|| eyre!("chain does not support Railgun"))?
            .deployment
            .relay_adapt_7702_contract = previous.delegate();
        historical_chain.enabled = true;
        self.while_active(super::executor_observation::observe_executor_history(
            &self.endpoints,
            &historical_chain,
            previous,
            range,
            None,
        ))
        .await
    }

    fn apply_history_reconciliation(
        &self,
        operation: ExecutorOperationId,
        range: Range<u64>,
        observed: &super::executor_observation::ExecutorHistoryObservation,
    ) -> Result<ExecutorReconciliationReport> {
        if let Some(nonce) = observed.nonce {
            self.store
                .reconcile(operation, nonce, &observed.inclusions)?;
        }
        let record = self.store.reconcile_recovery(
            operation,
            observed.block,
            &observed.recovery_inclusions,
        )?;
        // Date an observed submission from the first canonical head read after
        // handoff. Prefetch and stale session-tip heights cannot make it overdue.
        if let Some(submissions) = self
            .submission_blocks
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .get_mut(&operation)
        {
            for submitted in submissions.values_mut() {
                submitted.get_or_insert(
                    observed
                        .block
                        .number
                        .saturating_add(self.chain.finality_depth),
                );
            }
        }
        let mut coverage = self
            .history_coverage
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?;
        let start = coverage
            .get(&operation)
            .filter(|previous| {
                previous.range.end >= range.start
                    && previous.range.start <= range.start
                    && previous.observed.number <= observed.block.number
            })
            .map_or(range.start, |previous| previous.range.start);
        coverage.insert(
            operation,
            status::HistoryCoverage {
                range: start..range.end,
                observed: observed.block,
            },
        );
        drop(coverage);
        self.notify_change();
        Ok(ExecutorReconciliationReport {
            record,
            code: observed.code.clone().map(|code| (observed.block, code)),
        })
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
        let mut historical_chain = self.chain.clone();
        historical_chain
            .railgun
            .as_mut()
            .ok_or_else(|| eyre!("chain does not support Railgun"))?
            .deployment
            .relay_adapt_7702_contract = record.delegate();
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
