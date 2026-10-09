use std::sync::Arc;
use std::time::Duration;

use broadcaster_core::contracts::cow::OrderUid;
use eyre::Result;
use sync_service::{WalletCurrentSnapshot, WalletHandle};
use tokio::sync::watch;

use super::ExecutorOwner;
use super::attribution::synced_resolution;
use crate::WalletSyncTip;
use crate::vault::{ExecutorOperationId, ExecutorRecord};

impl ExecutorOwner {
    /// Resume submitted executions from the private actor's durable receive/spend
    /// locations, and settle resolved nonces that private sync has scanned past. It reads
    /// the private snapshot and local records only: no chain read starts here.
    pub(crate) fn start_confirmation_observation(
        self: &Arc<Self>,
        wallet: WalletHandle,
        mut tip: watch::Receiver<WalletSyncTip>,
    ) {
        let mut join = self
            .confirmation_observation_join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Checked under this lock so `close` cannot run between the check and the store.
        let mut synced = self
            .synced_observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if join.is_some() || self.ensure_active().is_err() {
            return;
        }
        *synced = Some((wallet.clone(), tip.clone()));
        drop(synced);
        let owner = Arc::clone(self);
        *join = Some(tokio::spawn(async move {
            let mut closed = owner.closed.subscribe();
            let mut observations = wallet.subscribe_observation();
            let mut changes = owner.subscribe();
            loop {
                let safe_head = tip.borrow_and_update().safe_head_block;
                observations.borrow_and_update();
                changes.borrow_and_update();
                // A failed pass leaves the records untouched. Fast chain tips must not
                // turn failures into a continuous retry loop.
                let result = owner
                    .while_active(Box::pin(owner.confirm_synced_history(&wallet, safe_head)))
                    .await;
                if result.is_err() {
                    tracing::debug!(
                        target: "executor_observation",
                        retry_after_secs = 15,
                        "confirmation observation unavailable; delaying retry"
                    );
                    tokio::select! {
                        biased;
                        _ = closed.wait_for(|closed| *closed) => break,
                        () = tokio::time::sleep(Duration::from_secs(15)) => {},
                    }
                    continue;
                }
                tokio::select! {
                    biased;
                    _ = closed.wait_for(|closed| *closed) => break,
                    changed = observations.changed() => if changed.is_err() { break; },
                    changed = changes.changed() => if changed.is_err() { break; },
                    changed = tip.changed() => if changed.is_err() { break; },
                }
            }
        }));
    }

    async fn confirm_synced_history(
        &self,
        wallet: &WalletHandle,
        safe_head: Option<u64>,
    ) -> Result<()> {
        self.ensure_active()?;
        let Some(snapshot) = wallet.current_snapshot() else {
            return Ok(());
        };
        let current = || wallet.current_snapshot();
        for record in self.store.records()? {
            if !self
                .confirm_synced_record(&snapshot, &current, safe_head, record)
                .await?
            {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Foreground counterpart of the confirmation observer for one operation, run
    /// before an account read. It only records what private sync shows; the account
    /// read still evaluates the nonce and grants no admission from sync alone.
    pub(super) async fn confirm_synced_operation(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<()> {
        let synced = self
            .synced_observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some((wallet, tip)) = synced else {
            return Ok(());
        };
        let safe_head = tip.borrow().safe_head_block;
        let Some(snapshot) = wallet.current_snapshot() else {
            return Ok(());
        };
        let Some(record) = self
            .store
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
        else {
            return Ok(());
        };
        let current = || wallet.current_snapshot();
        self.while_active(Box::pin(
            self.confirm_synced_record(&snapshot, &current, safe_head, record),
        ))
        .await?;
        Ok(())
    }

    /// Private sync's spend location of the inputs unshielded by the pre-hook of `record`'s
    /// order `uid`. Like the orderbook's trade block, this is an untrusted hint: only the
    /// settlement receipts at that block establish the trade.
    #[must_use]
    pub fn synced_settlement_block(&self, record: &ExecutorRecord, uid: OrderUid) -> Option<u64> {
        let wallet = self
            .synced_observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|(wallet, _)| wallet.clone())?;
        let snapshot = wallet.current_snapshot()?;
        let order = record
            .swap()?
            .orders()
            .iter()
            .find(|order| order.uid() == uid)?;
        let payload = record
            .issued()
            .iter()
            .find(|payload| payload.hash() == order.pre_hook().payload())?;
        let inputs = payload.context().inputs();
        snapshot
            .utxos
            .iter()
            .filter(|utxo| inputs.iter().any(|input| input.matches(&utxo.utxo)))
            .filter_map(|utxo| utxo.spent.as_ref().map(|spent| spent.block_number))
            .min()
    }

    /// Record what `snapshot` shows about `record`, with no chain read: a pending payload
    /// that private sync shows executed at or below the safe head moves the watermark, and
    /// nonces resolved at a block sync has scanned are settled. `current` is the private
    /// snapshot at the time of the write. Returns false once the private snapshot was
    /// reset, so callers stop using it.
    pub(crate) async fn confirm_synced_record(
        &self,
        snapshot: &WalletCurrentSnapshot,
        current: &impl Fn() -> Option<Arc<WalletCurrentSnapshot>>,
        safe_head: Option<u64>,
        record: ExecutorRecord,
    ) -> Result<bool> {
        let resolved = self.chain.railgun.as_ref().and_then(|railgun| {
            synced_resolution(&record, railgun.deployment.contract, snapshot, safe_head)
        });
        let mut synced = record.clone();
        synced.apply_synced(resolved, snapshot.last_scanned);
        if synced == record {
            return Ok(true);
        }
        let _guard = self.lock_activity().await;
        self.require_record_unchanged(&record)?;
        if current().is_none_or(|latest| latest.reset_generation != snapshot.reset_generation) {
            return Ok(false);
        }
        self.store
            .record_synced(record.operation(), resolved, snapshot.last_scanned)?;
        self.notify_change();
        Ok(true)
    }
}
