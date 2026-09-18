use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use alloy::primitives::Address;
use eyre::{Result, eyre};
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use tokio::sync::watch;

use super::ExecutorOwner;
use crate::desktop::executor_discovery::inspect_for_signing;
use crate::vault::ExecutorNonceObservation;
use crate::{ExecutorAsset, ExecutorInspection};

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

type InspectionRead = Shared<BoxFuture<'static, Result<Arc<CheckedExecutor>, Arc<str>>>>;

struct UnusedRead {
    address: Address,
    assets: BTreeSet<ExecutorAsset>,
    read: InspectionRead,
}

/// Only accounts without issued payloads can consume these observations. No TTL or poll.
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

    pub(super) fn history_start(
        &self,
        observed: ExecutorNonceObservation,
        finality_depth: u64,
    ) -> u64 {
        let observed = observed.block().number;
        self.head.map_or(observed, |head| {
            head.saturating_sub(finality_depth).max(observed)
        })
    }
}

impl ExecutorOwner {
    fn start_unused_inspection(
        &self,
        index: u32,
        address: Address,
        assets: &[ExecutorAsset],
    ) -> Result<InspectionRead> {
        let mut unused = self
            .unused
            .lock()
            .map_err(|_| eyre!("executor preparation is unavailable"))?;
        self.ensure_active()?;
        let mut assets = assets.iter().copied().collect::<BTreeSet<_>>();
        assets.insert(ExecutorAsset::Native);
        if let Some(existing) = unused.reads.get(&index)
            && existing.address == address
        {
            if assets.is_subset(&existing.assets)
                && !existing.read.peek().is_some_and(Result::is_err)
            {
                return Ok(existing.read.clone());
            }
            assets.extend(&existing.assets);
        }
        let chain = self.chain.clone();
        let http = self.http.clone();
        let requested = assets.iter().copied().collect::<Vec<_>>();
        let mut closed = self.closed.subscribe();
        let mut revision = unused.revision.subscribe();
        let initial_revision = *revision.borrow();
        let validity = revision.clone();
        let read = async move {
            tokio::select! {
                biased;
                _ = closed.wait_for(|closed| *closed) => Err(Arc::from("executor wallet session has ended")),
                _ = revision.wait_for(|revision| *revision != initial_revision) => Err(Arc::from("executor observation invalidated; retry preparation")),
                result = inspect_for_signing(&chain, &http, address, &requested) => {
                    result.map(|(inspection, observed)| Arc::new(CheckedExecutor { inspection, observed, revision: Some((validity, initial_revision)) }))
                        .map_err(|error| Arc::from(error.to_string()))
                }
            }
        }.boxed().shared();
        unused.reads.insert(
            index,
            UnusedRead {
                address,
                assets,
                read: read.clone(),
            },
        );
        Ok(read)
    }

    pub(super) async fn unused_inspection(
        &self,
        index: u32,
        address: Address,
        assets: &[ExecutorAsset],
    ) -> Result<Arc<CheckedExecutor>> {
        let read = self.start_unused_inspection(index, address, assets)?;
        self.while_active(async { read.await.map_err(|error| eyre!(error.to_string())) })
            .await
    }
}
