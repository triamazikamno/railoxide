use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use eyre::Result;
use sync_service::WalletHandle;
use tokio::sync::watch;

use super::ExecutorOwner;
use crate::WalletSyncTip;
use crate::desktop::executor_observation::observe_synced_executor_history;

impl ExecutorOwner {
    /// Resume submitted executions from the private actor's durable receive/spend
    /// locations. Only block-scoped RPC is used; no account or transaction queries.
    pub(crate) fn start_confirmation_observation(
        self: &Arc<Self>,
        wallet: WalletHandle,
        mut tip: watch::Receiver<WalletSyncTip>,
    ) {
        let mut join = self
            .confirmation_observation_join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if join.is_some() || self.ensure_active().is_err() {
            return;
        }
        let owner = Arc::clone(self);
        *join = Some(tokio::spawn(async move {
            let mut closed = owner.closed.subscribe();
            let mut observations = wallet.subscribe_observation();
            let mut changes = owner.subscribe();
            loop {
                let safe_head = tip.borrow_and_update().safe_head_block;
                observations.borrow_and_update();
                changes.borrow_and_update();
                // Unavailable RPC leaves the durable history untouched. Retry on
                // the next chain, private projection, or submission update.
                let _ = owner
                    .while_active(owner.confirm_synced_history(&wallet, safe_head))
                    .await;
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
        let _guard = self.lock_activity().await;
        self.ensure_active()?;
        let Some(snapshot) = wallet.current_snapshot() else {
            return Ok(());
        };
        let locations = snapshot
            .utxos
            .iter()
            .flat_map(|utxo| std::iter::once(&utxo.utxo.source).chain(utxo.spent.iter()))
            .map(|source| (source.tx_hash, source.block_number))
            .collect::<BTreeMap<_, _>>();
        for mut record in self.store.records()? {
            let numbers = record
                .issued()
                .iter()
                .filter(|payload| payload.inclusion().is_none())
                .flat_map(crate::vault::IssuedExecutorPayload::transaction_hashes)
                .filter_map(|hash| locations.get(hash).copied())
                .filter(|number| safe_head.is_none_or(|head| *number <= head))
                .collect::<BTreeSet<_>>();
            for number in numbers {
                let observed =
                    observe_synced_executor_history(&self.chain, &self.http, &record, number)
                        .await?;
                self.ensure_active()?;
                if wallet
                    .current_snapshot()
                    .is_none_or(|current| current.reset_generation != snapshot.reset_generation)
                {
                    return Ok(());
                }
                let unchanged = record.issued().iter().all(|payload| {
                    payload.inclusion()
                        == observed
                            .inclusions
                            .iter()
                            .find(|(hash, _)| *hash == payload.hash())
                            .map(|(_, inclusion)| *inclusion)
                });
                if !unchanged {
                    record = self.store.record_history(
                        record.operation(),
                        observed.block,
                        &observed.inclusions,
                    )?;
                    self.notify_change();
                }
            }
        }
        Ok(())
    }
}
