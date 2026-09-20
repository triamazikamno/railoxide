use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::ops::Range;
use std::sync::{Arc, Mutex as StdMutex};

use alloy::primitives::B256;
use eyre::{Result, eyre};
use tokio::sync::{Mutex, MutexGuard, watch};

use crate::settings::EffectiveChainConfig;
use crate::vault::{
    DesktopVaultStore, DesktopViewSession, ExecutorOperationId, ExecutorRecord, ExecutorStore,
};
use crate::{ExecutorAsset, ExecutorInspection, HttpContext, inspect_executor};

mod confirmations;
mod discovery;
mod execution;
mod observation;
mod public_account;
mod recovery;
mod spare;
mod status;
pub use discovery::ExecutorDiscoveryReport;
pub use execution::*;
pub use observation::ExecutorTransactionIdentity;
pub(crate) use public_account::ExecutorPublicSigningGuard;
pub use recovery::*;
pub use status::{ExecutorAccountOutcome, ExecutorAccountStatus};

pub struct ExecutorReconciliationReport {
    record: ExecutorRecord,
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
    closed: watch::Sender<bool>,
    activity: Arc<Mutex<()>>,
    reconciled: StdMutex<BTreeSet<ExecutorOperationId>>,
    history_coverage: StdMutex<BTreeMap<ExecutorOperationId, status::HistoryCoverage>>,
    submission_blocks: StdMutex<BTreeMap<ExecutorOperationId, BTreeMap<B256, Option<u64>>>>,
    tip_observation_join: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    confirmation_observation_join: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    unused: StdMutex<spare::UnusedInspections>,
    changes: watch::Sender<u64>,
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
            chain,
            http,
            closed: watch::channel(false).0,
            activity: Arc::new(Mutex::new(())),
            reconciled: StdMutex::new(BTreeSet::new()),
            history_coverage: StdMutex::new(BTreeMap::new()),
            submission_blocks: StdMutex::new(BTreeMap::new()),
            tip_observation_join: StdMutex::new(None),
            confirmation_observation_join: StdMutex::new(None),
            unused: StdMutex::new(spare::UnusedInspections::default()),
            changes: watch::channel(0).0,
        })
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn close(&self) {
        self.closed.send_replace(true);
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

    async fn lock_activity(&self) -> MutexGuard<'_, ()> {
        self.activity.lock().await
    }

    pub fn records(&self) -> Result<Vec<ExecutorRecord>> {
        self.ensure_active()?;
        let reconciled = self
            .reconciled
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?;
        let mut records = self.store.records()?;
        for record in &mut records {
            if !reconciled.contains(&record.operation()) {
                record.require_reconciliation();
            }
        }
        Ok(records)
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
        let mut inputs = inputs;
        // A later preparation must not reuse inputs spent by an earlier winner
        // during the interval before private sync publishes its nullifiers.
        inputs.retain(|input| {
            !record.issued().iter().any(|payload| {
                record.payload_status(payload.hash())
                    == Some(crate::vault::ExecutorPayloadStatus::Executed)
                    && payload
                        .context()
                        .inputs()
                        .iter()
                        .any(|spent| spent.matches(input))
            })
        });
        self.filter_reserved_inputs(inputs, Some(prepared.operation()))
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
        let _guard = self.lock_activity().await;
        self.reconcile_history_admitted(operation, range).await
    }

    async fn reconcile_history_admitted(
        &self,
        operation: ExecutorOperationId,
        range: Range<u64>,
    ) -> Result<ExecutorReconciliationReport> {
        self.ensure_active()?;
        let previous = self.begin_history_reconciliation(operation)?;
        let observed = self.read_history(&previous, range.clone()).await?;
        self.apply_history_reconciliation(operation, range, &observed)
    }

    // Callers hold activity while invalidating or applying local projections.
    fn begin_history_reconciliation(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord> {
        self.reconciled
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .remove(&operation);
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
            &historical_chain,
            &self.http,
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
        self.reconciled
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .insert(operation);
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
        Ok(ExecutorReconciliationReport { record })
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
