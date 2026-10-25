use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use alloy::primitives::Address;
use eyre::{Result, eyre};
use tokio::sync::watch;

use super::ExecutorOwner;
use crate::desktop::executor_discovery::inspect_for_signing;
use crate::vault::ExecutorNonceObservation;
use crate::{ExecutorAsset, ExecutorInspection, OperationHttpClient};

pub(super) struct CheckedExecutor {
    pub(super) inspection: ExecutorInspection,
    pub(super) observed: ExecutorNonceObservation,
    pub(super) revision: Option<(watch::Receiver<u64>, u64)>,
}

impl CheckedExecutor {
    pub(super) fn ensure_valid(&self) -> Result<()> {
        if self
            .revision
            .as_ref()
            .is_some_and(|(revision, expected)| *revision.borrow() != *expected)
        {
            return Err(eyre!("executor observation invalidated; retry preparation"));
        }
        Ok(())
    }
}

struct UnusedRead {
    address: Address,
    assets: BTreeSet<ExecutorAsset>,
    checked: Arc<CheckedExecutor>,
}

/// Completed observations that only accounts without issued payloads can consume. A pending
/// read belongs to its caller and its route. No TTL or poll.
pub(super) struct UnusedInspections {
    reads: BTreeMap<u32, UnusedRead>,
    head: Option<u64>,
    revision: watch::Sender<u64>,
}

impl Default for UnusedInspections {
    fn default() -> Self {
        Self {
            reads: BTreeMap::new(),
            head: None,
            revision: watch::channel(0).0,
        }
    }
}

impl UnusedInspections {
    pub(super) fn observe_head(&mut self, head: Option<u64>) -> bool {
        let mut invalidated = false;
        if let Some(head) = head {
            if self.head.is_some_and(|previous| head < previous) {
                self.invalidate();
                invalidated = true;
            }
            self.head = Some(head);
        }
        invalidated
    }

    pub(super) fn invalidate(&mut self) {
        self.reads.clear();
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub(super) fn remove(&mut self, index: u32) {
        self.reads.remove(&index);
    }
}

impl ExecutorOwner {
    pub(super) async fn unused_inspection(
        &self,
        index: u32,
        address: Address,
        assets: &[ExecutorAsset],
        client: &OperationHttpClient,
    ) -> Result<Arc<CheckedExecutor>> {
        let (assets, mut revision) = {
            let unused = self
                .unused
                .lock()
                .map_err(|_| eyre!("executor preparation is unavailable"))?;
            self.ensure_active()?;
            let mut assets = assets.iter().copied().collect::<BTreeSet<_>>();
            assets.insert(ExecutorAsset::Native);
            if let Some(existing) = unused.reads.get(&index)
                && existing.address == address
            {
                if assets.is_subset(&existing.assets) {
                    return Ok(existing.checked.clone());
                }
                assets.extend(&existing.assets);
            }
            (assets, unused.revision.subscribe())
        };
        let requested = assets.iter().copied().collect::<Vec<_>>();
        let mut closed = self.closed.subscribe();
        let initial_revision = *revision.borrow();
        let validity = revision.clone();
        let (inspection, observed) = tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => return Err(eyre!("executor wallet session has ended")),
            _ = revision.wait_for(|revision| *revision != initial_revision) => return Err(eyre!("executor observation invalidated; retry preparation")),
            result = inspect_for_signing(&self.chain, &self.http, client, address, &requested) => result?,
        };
        self.ensure_active()?;
        let checked = Arc::new(CheckedExecutor {
            inspection,
            observed,
            revision: Some((validity, initial_revision)),
        });
        let mut unused = self
            .unused
            .lock()
            .map_err(|_| eyre!("executor preparation is unavailable"))?;
        // A read that finished after an invalidation must not restore the evidence it cleared.
        if *unused.revision.borrow() == initial_revision {
            unused.reads.insert(
                index,
                UnusedRead {
                    address,
                    assets,
                    checked: checked.clone(),
                },
            );
        }
        Ok(checked)
    }
}
