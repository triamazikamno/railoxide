use std::sync::Arc;

use alloy::primitives::B256;
use alloy::rpc::types::TransactionRequest;
use eyre::{Result, eyre};
use tokio::sync::watch;

use super::ExecutorOwner;
use crate::WalletSyncTip;
use crate::vault::ExecutorOperationId;

/// Links transport attempts to an already persisted signed execution payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorTransactionIdentity {
    operation: ExecutorOperationId,
    payload: B256,
}

impl ExecutorTransactionIdentity {
    #[must_use]
    pub const fn operation(self) -> ExecutorOperationId {
        self.operation
    }

    #[must_use]
    pub const fn payload(self) -> B256 {
        self.payload
    }
}

impl ExecutorOwner {
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(super) fn notify_change(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(crate) fn transaction_identity(
        &self,
        transaction: &TransactionRequest,
    ) -> Result<Option<ExecutorTransactionIdentity>> {
        self.ensure_active()?;
        let Some(address) = transaction.to.as_ref().and_then(|to| to.to()) else {
            return Ok(None);
        };
        for record in self.store.records()? {
            if record.address().as_ref() != Some(address) {
                continue;
            }
            let payload = record
                .issued()
                .iter()
                .find(|payload| transaction.input.input() == Some(payload.context().calldata()))
                .ok_or_else(|| {
                    eyre!("executor transaction does not match a durable issued payload")
                })?;
            return Ok(Some(ExecutorTransactionIdentity {
                operation: record.operation(),
                payload: payload.hash(),
            }));
        }
        Ok(None)
    }

    pub(crate) fn record_submission(
        &self,
        identity: ExecutorTransactionIdentity,
        transaction: B256,
    ) -> Result<()> {
        self.ensure_active()?;
        self.store
            .record_submission(identity.operation, identity.payload, transaction)?;
        // A handoff establishes issuance; the next successful history check
        // supplies its conservative head bound without additional RPC.
        self.submission_blocks
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .entry(identity.operation)
            .or_default()
            .entry(identity.payload)
            .or_insert(None);
        self.notify_change();
        Ok(())
    }

    /// Observe the existing sync feed only to invalidate local preparation data.
    /// This task must never issue RPC requests for an executor account.
    pub(crate) fn start_tip_observation(self: &Arc<Self>, mut tip: watch::Receiver<WalletSyncTip>) {
        let mut join = self
            .tip_observation_join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if join.is_some() || self.ensure_active().is_err() {
            return;
        }
        let owner = Arc::clone(self);
        *join = Some(tokio::spawn(async move {
            let mut closed = owner.closed.subscribe();
            loop {
                if owner
                    .unused
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .observe_head(tip.borrow().head_block)
                {
                    owner.notify_change();
                }
                tokio::select! {
                    biased;
                    _ = closed.wait_for(|closed| *closed) => break,
                    changed = tip.changed() => if changed.is_err() { break; },
                }
            }
        }));
    }
}
