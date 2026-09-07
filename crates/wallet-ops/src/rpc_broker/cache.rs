use super::model::{HEAD_FRESHNESS_LEASE, ReadIdentity, RpcBrokerError, RpcResult};
use alloy::eips::{BlockId, BlockNumberOrTag};
use std::collections::{HashMap, VecDeque};
use tokio::time::Instant;

const READ_CACHE_ENTRY_LIMIT: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct LatestEpoch {
    pub(super) block_number: u64,
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HeadObservation {
    pub(super) block_number: u64,
    pub(super) observed_at: Instant,
    pub(super) usable: bool,
    generation: Option<u64>,
}

impl HeadObservation {
    fn latest_epoch(&self) -> Option<LatestEpoch> {
        self.generation
            .filter(|_| self.usable)
            .map(|generation| LatestEpoch {
                block_number: self.block_number,
                generation,
            })
    }
}

/// Owns completed read results and the head observations used to validate latest reads.
#[derive(Default)]
pub(super) struct ReadCache {
    pub(super) entries: HashMap<ReadIdentity, (Option<u64>, Result<RpcResult, RpcBrokerError>)>,
    order: VecDeque<ReadIdentity>,
    pub(super) current_blocks: HashMap<u64, HeadObservation>,
}

impl ReadCache {
    pub(super) fn lookup(
        &self,
        identity: &ReadIdentity,
        expected_epoch: Option<u64>,
    ) -> Option<Result<RpcResult, RpcBrokerError>> {
        self.entries
            .get(identity)
            .filter(|(observed, _)| *observed == expected_epoch)
            .map(|(_, value)| value.clone())
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(super) fn contains_key(&self, identity: &ReadIdentity) -> bool {
        self.entries.contains_key(identity)
    }

    pub(super) fn reconcile_latest_cache_epoch(&mut self, chain_id: u64) -> Option<LatestEpoch> {
        if self
            .current_blocks
            .get(&chain_id)
            .is_some_and(|observation| observation.observed_at.elapsed() > HEAD_FRESHNESS_LEASE)
        {
            self.invalidate_head(chain_id);
        }
        self.current_blocks
            .get(&chain_id)
            .and_then(HeadObservation::latest_epoch)
    }

    /// Records a first, newer, or equal head and reports that it may trigger a flush.
    ///
    /// Lower (stale) heads are ignored completely and return `false`; they do not
    /// change cache state or the batching window.
    pub(super) fn observe_head(&mut self, chain_id: u64, block_number: u64, now: Instant) -> bool {
        let advanced = self
            .current_blocks
            .get(&chain_id)
            .is_none_or(|old| block_number > old.block_number);
        if advanced {
            self.current_blocks.insert(
                chain_id,
                HeadObservation {
                    block_number,
                    observed_at: now,
                    usable: true,
                    generation: Some(0),
                },
            );
            self.evict_latest(chain_id);
            true
        } else {
            if self.current_blocks.get(&chain_id).is_some_and(|old| {
                block_number == old.block_number
                    && now.duration_since(old.observed_at) > HEAD_FRESHNESS_LEASE
            }) {
                self.invalidate_head(chain_id);
            }
            self.renew_head(chain_id, block_number, now)
        }
    }

    fn renew_head(&mut self, chain_id: u64, block_number: u64, now: Instant) -> bool {
        if let Some(observation) = self.current_blocks.get_mut(&chain_id)
            && block_number == observation.block_number
        {
            observation.observed_at = now;
            observation.usable = observation.generation.is_some();
            true
        } else {
            false
        }
    }

    pub(super) fn invalidate_head(&mut self, chain_id: u64) {
        if let Some(observation) = self.current_blocks.get_mut(&chain_id)
            && observation.usable
        {
            observation.usable = false;
            // Exhaustion disables this height until a newer head supplies a distinct token.
            observation.generation = observation
                .generation
                .and_then(|value| value.checked_add(1));
        }
        self.evict_latest(chain_id);
    }

    pub(super) fn evict_latest(&mut self, chain_id: u64) {
        self.entries.retain(|identity, _| {
            identity.chain_id() != chain_id
                || !matches!(
                    identity.block(),
                    Some(BlockId::Number(BlockNumberOrTag::Latest))
                )
        });
        self.order.retain(|identity| {
            identity.chain_id() != chain_id
                || !matches!(
                    identity.block(),
                    Some(BlockId::Number(BlockNumberOrTag::Latest))
                )
        });
    }

    pub(super) fn insert(
        &mut self,
        identity: ReadIdentity,
        observed: Option<u64>,
        value: Result<RpcResult, RpcBrokerError>,
    ) {
        self.order.retain(|existing| existing != &identity);
        self.entries.insert(identity.clone(), (observed, value));
        self.order.push_back(identity);
        while self.entries.len() > READ_CACHE_ENTRY_LIMIT {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    pub(super) fn latest_is_current(&self, chain_id: u64, epoch: Option<LatestEpoch>) -> bool {
        let Some(head) = self.current_blocks.get(&chain_id) else {
            return false;
        };
        epoch.is_some()
            && epoch == head.latest_epoch()
            && head.observed_at.elapsed() <= HEAD_FRESHNESS_LEASE
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{RpcChainRoute, RpcRead, RpcRoute};
    use super::*;
    use crate::rpc_broker::tests::data_result;
    use alloy::primitives::{Address, Bytes};
    use url::Url;

    fn identity(block: BlockId) -> ReadIdentity {
        let route = RpcRoute::from(RpcChainRoute::new(
            1,
            vec![Url::parse("https://cache.test").unwrap()],
        ));
        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"cache"))
            .with_test_block(block)
            .identity_for_route(&route)
    }

    #[test]
    fn lookup_requires_epoch_and_head_advance_evicts_only_latest() {
        let mut cache = ReadCache::default();
        let latest = identity(BlockId::Number(BlockNumberOrTag::Latest));
        let numbered = identity(BlockId::Number(BlockNumberOrTag::Number(7)));
        cache.insert(
            latest.clone(),
            Some(7),
            Ok(data_result(Bytes::from_static(b"latest"))),
        );
        cache.insert(
            numbered.clone(),
            Some(7),
            Ok(data_result(Bytes::from_static(b"numbered"))),
        );
        assert_eq!(
            cache.lookup(&latest, Some(7)),
            Some(Ok(data_result(Bytes::from_static(b"latest"))))
        );
        assert_eq!(cache.lookup(&latest, Some(8)), None);
        cache.observe_head(1, 8, Instant::now());
        assert!(!cache.contains_key(&latest));
        assert!(cache.contains_key(&numbered));
    }

    #[test]
    fn insert_bounds_entry_count_and_reinsertion_refreshes_eviction_order() {
        let limit = u64::try_from(READ_CACHE_ENTRY_LIMIT).expect("cache limit fits a u64");
        let mut cache = ReadCache::default();
        let numbered = |block: u64| identity(BlockId::Number(BlockNumberOrTag::Number(block)));
        for block in 0..=limit {
            cache.insert(
                numbered(block),
                Some(block),
                Ok(data_result(Bytes::from(block.to_be_bytes().to_vec()))),
            );
        }
        assert_eq!(cache.len(), READ_CACHE_ENTRY_LIMIT);
        assert!(!cache.contains_key(&numbered(0)));
        assert!(cache.contains_key(&numbered(1)));
        assert!(cache.contains_key(&numbered(limit)));

        // Re-inserting an existing key must move it to the back of the eviction order, so the
        // next admission evicts the entry behind it rather than the refreshed one.
        cache.insert(
            numbered(1),
            Some(1),
            Ok(data_result(Bytes::from_static(b"refreshed"))),
        );
        cache.insert(
            numbered(limit + 1),
            Some(limit + 1),
            Ok(data_result(Bytes::from_static(b"newest"))),
        );
        assert_eq!(cache.len(), READ_CACHE_ENTRY_LIMIT);
        assert!(!cache.contains_key(&numbered(2)));
        assert!(cache.contains_key(&numbered(limit + 1)));
        assert_eq!(
            cache.lookup(&numbered(1), Some(1)),
            Some(Ok(data_result(Bytes::from_static(b"refreshed"))))
        );
    }
}
