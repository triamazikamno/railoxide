use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::eips::BlockNumHash;
use alloy::primitives::B256;
use eyre::{Result, eyre};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::{PublicBalanceCache, PublicBalanceScope};
use crate::block_observer::BlockObserver;
use crate::vault::PublicAccountMetadata;

/// Classification survives observer failure and replacement, independently of permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicTransactionLookup {
    Untracked,
    Pending,
    /// Observation is unavailable, or another attempt in this family was included.
    /// This does not prove absence and must never enable an exact-hash RPC fallback.
    Unavailable,
    /// Retained background discovery exhausted its providers. This remains latched for
    /// this chain/hash and permits exact-hash forwarding only for authorized dapp reads.
    DiscoveryExhausted,
    Included {
        block: BlockNumHash,
        transaction_index: u64,
    },
}

#[derive(Default)]
struct Registry {
    hashes: HashMap<(u64, B256), PublicTransactionLookup>,
    families: Vec<Family>,
}

struct Family {
    scope: PublicBalanceScope,
    account: PublicAccountMetadata,
    cache: PublicBalanceCache,
    hashes: Vec<B256>,
    inclusion: Option<BlockNumHash>,
    cache_scope_identity: Option<Arc<()>>,
}

/// Root-owned local classification. Stores only ownership and small block locations.
#[derive(Clone)]
pub struct PublicTransactionTracker {
    registry: Arc<Mutex<Registry>>,
    changes: watch::Sender<()>,
    jobs: Arc<Mutex<ObserverJobs>>,
}

#[derive(Default)]
struct ObserverJobs {
    closed: bool,
    tasks: JoinSet<()>,
}

impl Default for PublicTransactionTracker {
    fn default() -> Self {
        Self {
            registry: Arc::default(),
            changes: watch::channel(()).0,
            jobs: Arc::default(),
        }
    }
}

/// Captured submission ownership. Each observer creates a separate replacement family.
#[derive(Clone)]
pub struct PublicTransactionTrackingContext {
    tracker: PublicTransactionTracker,
    scope: PublicBalanceScope,
    account: PublicAccountMetadata,
    cache: PublicBalanceCache,
}

impl PublicTransactionTracker {
    /// Whether an unresolved submitted transaction still needs this chain's observation route.
    ///
    /// # Panics
    /// Panics if the transaction registry mutex is poisoned.
    #[must_use]
    pub fn has_pending_observation(&self, chain_id: u64) -> bool {
        self.registry
            .lock()
            .expect("public transaction registry poisoned")
            .hashes
            .iter()
            .any(|((chain, _), state)| {
                *chain == chain_id
                    && matches!(
                        state,
                        PublicTransactionLookup::Pending | PublicTransactionLookup::Unavailable
                    )
            })
    }

    fn retain_observer(&self, mut observer: BlockObserver, runtime: &Handle) -> Result<()> {
        let mut jobs = self.jobs.lock().expect("public transaction jobs poisoned");
        if jobs.closed {
            drop(jobs);
            return Err(eyre!("public transaction observation is closed"));
        }
        if !observer.needs_observation() {
            drop(jobs);
            return Ok(());
        }
        while jobs.tasks.try_join_next().is_some() {}
        jobs.tasks.spawn_on(
            async move {
                loop {
                    match observer.poll().await {
                        Ok(observation) if observation.receipt.is_some() => break,
                        Ok(_) => tokio::time::sleep(Duration::from_secs(3)).await,
                        Err(_) => {
                            observer.discovery_exhausted();
                            break;
                        }
                    }
                }
            },
            runtime,
        );
        Ok(())
    }

    /// Close this admission owner and prepare a fresh owner sharing known hashes and wakeups.
    /// Captured contexts remain attached to the closed owner. Before installing or using the
    /// successor, the root must stop and await old submission jobs and await this owner's
    /// `shutdown`. Ordinary wallet or cache scope changes should keep the existing owner.
    ///
    /// # Panics
    /// Panics if the observer-jobs mutex is poisoned.
    #[must_use]
    pub fn successor(&self) -> Self {
        self.close();
        Self {
            registry: Arc::clone(&self.registry),
            changes: self.changes.clone(),
            jobs: Arc::default(),
        }
    }

    /// Reject new observer admission and abort background observers.
    /// The root must also close, abort, and await its submission jobs before `shutdown`.
    /// Browser disconnect alone must not close accepted submission work.
    ///
    /// # Panics
    /// Panics if the observer-jobs mutex is poisoned.
    pub fn close(&self) {
        let mut jobs = self.jobs.lock().expect("public transaction jobs poisoned");
        jobs.closed = true;
        jobs.tasks.abort_all();
    }

    /// Await observer cleanup after the root has stopped and awaited submission jobs.
    /// Classification remains available after shutdown. This tracker cannot be reopened.
    ///
    /// # Panics
    /// Panics if the observer-jobs mutex is poisoned.
    pub async fn shutdown(&self) {
        self.close();
        let mut tasks = {
            let mut jobs = self.jobs.lock().expect("public transaction jobs poisoned");
            std::mem::take(&mut jobs.tasks)
        };
        while tasks.join_next().await.is_some() {}
    }

    /// # Panics
    /// Panics if the registry mutex is poisoned.
    #[must_use]
    pub fn lookup(&self, chain_id: u64, hash: B256) -> PublicTransactionLookup {
        self.registry
            .lock()
            .expect("public transaction registry poisoned")
            .hashes
            .get(&(chain_id, hash))
            .copied()
            .unwrap_or(PublicTransactionLookup::Untracked)
    }

    /// Coalesced wakeups; the owner reconciles current scopes before queuing refreshes.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }

    #[must_use]
    pub fn context(
        &self,
        scope: PublicBalanceScope,
        account: PublicAccountMetadata,
        cache: PublicBalanceCache,
    ) -> PublicTransactionTrackingContext {
        PublicTransactionTrackingContext {
            tracker: self.clone(),
            scope,
            account,
            cache,
        }
    }

    /// Reapply outstanding work after scope changes, before refresh or local reuse.
    /// Only the current wallet owner may supply scopes here. No notification is emitted.
    ///
    /// # Panics
    /// Panics if a registry or balance-cache mutex is poisoned.
    pub fn reconcile_scope(
        &self,
        scope: &PublicBalanceScope,
        accounts: &[PublicAccountMetadata],
        cache: &PublicBalanceCache,
    ) {
        let mut registry = self
            .registry
            .lock()
            .expect("public transaction registry poisoned");
        let Some(identity) = cache.scope_identity(scope) else {
            return;
        };
        for account in accounts {
            let mut changed = false;
            for family in &mut registry.families {
                if !family.matches_account(scope, account) {
                    continue;
                }
                if family
                    .cache_scope_identity
                    .as_ref()
                    .is_none_or(|previous| !Arc::ptr_eq(previous, &identity))
                    || family.account != *account
                {
                    family.scope = scope.clone();
                    family.account = account.clone();
                    family.cache = cache.clone();
                    family.cache_scope_identity = Some(identity.clone());
                    changed = true;
                }
            }
            if changed {
                registry.invalidate(scope, account, cache);
            }
        }
    }
}

impl Registry {
    fn invalidate(
        &self,
        scope: &PublicBalanceScope,
        account: &PublicAccountMetadata,
        cache: &PublicBalanceCache,
    ) {
        let mut found = false;
        let mut pending = false;
        let mut minimum: Option<BlockNumHash> = None;
        for family in &self.families {
            if !family.matches_account(scope, account) || family.hashes.is_empty() {
                continue;
            }
            found = true;
            pending |= family.inclusion.is_none();
            if let Some(block) = family.inclusion
                && minimum.is_none_or(|previous| block.number >= previous.number)
            {
                minimum = Some(block);
            }
        }
        if found {
            cache.invalidate_account(scope, account, pending, minimum);
        }
    }
}

impl Family {
    fn matches_account(&self, scope: &PublicBalanceScope, account: &PublicAccountMetadata) -> bool {
        self.scope.chain_id() == scope.chain_id()
            && self.account.public_account_uuid == account.public_account_uuid
            && self.account.address == account.address
            && self.account.scope == account.scope
            && account.is_scoped_to_wallet(scope.wallet_uuid())
    }
}

impl PublicTransactionTrackingContext {
    /// Check immediately before handoff in the root-owned managed submission job.
    pub(crate) fn ensure_open(&self) -> Result<()> {
        if self
            .tracker
            .jobs
            .lock()
            .expect("public transaction jobs poisoned")
            .closed
        {
            Err(eyre!("public transaction observation is closed"))
        } else {
            Ok(())
        }
    }

    /// Acquire before raw-send handoff. The caller owns and drains the submission job.
    pub(crate) fn admit_observer(
        &self,
        observer: BlockObserver,
    ) -> Result<PublicTransactionObservationGuard> {
        PublicTransactionObservationGuard::new(observer, Some(self))
    }

    pub(crate) fn start_family(&self) -> PublicTransactionFamily {
        let mut registry = self
            .tracker
            .registry
            .lock()
            .expect("public transaction registry poisoned");
        let id = registry.families.len();
        registry.families.push(Family {
            scope: self.scope.clone(),
            account: self.account.clone(),
            cache: self.cache.clone(),
            hashes: Vec::new(),
            inclusion: None,
            cache_scope_identity: self.cache.scope_identity(&self.scope),
        });
        PublicTransactionFamily {
            tracker: self.tracker.clone(),
            id,
        }
    }
}

/// Keeps observation owned across an in-flight broadcast, including ambiguous errors.
pub(crate) struct PublicTransactionObservationGuard {
    owner: Option<(PublicTransactionTracker, Handle)>,
    observer: Option<BlockObserver>,
}

impl PublicTransactionObservationGuard {
    /// Wrap an established observer without starting another poller. Dropping an unresolved
    /// tracked owner transfers this same observer to the tracker unless admission is closed.
    pub(crate) fn new(
        observer: BlockObserver,
        context: Option<&PublicTransactionTrackingContext>,
    ) -> Result<Self> {
        if let Some(context) = context {
            context.ensure_open()?;
            let runtime = Handle::try_current()
                .map_err(|_| eyre!("public transaction observation requires a runtime"))?;
            Ok(Self {
                owner: Some((context.tracker.clone(), runtime)),
                observer: Some(observer.with_tracking(context)),
            })
        } else {
            Ok(Self {
                owner: None,
                observer: Some(observer),
            })
        }
    }

    /// Check immediately before the irreversible RPC, inside the root-owned submission job.
    pub(crate) fn ensure_open(&self) -> Result<()> {
        if self.owner.as_ref().is_some_and(|(tracker, _)| {
            tracker
                .jobs
                .lock()
                .expect("public transaction jobs poisoned")
                .closed
        }) {
            Err(eyre!("public transaction observation is closed"))
        } else {
            Ok(())
        }
    }

    pub(crate) const fn observer_mut(&mut self) -> &mut BlockObserver {
        self.observer
            .as_mut()
            .expect("observation guard already handed off")
    }

    /// Transfer after attempting broadcast, even when its outcome is ambiguous.
    pub(crate) fn handoff(mut self) -> Result<()> {
        self.transfer()
    }

    fn transfer(&mut self) -> Result<()> {
        let Some(observer) = self.observer.take() else {
            return Ok(());
        };
        if let Some((tracker, runtime)) = &self.owner {
            tracker.retain_observer(observer, runtime)
        } else {
            Ok(())
        }
    }
}

impl Deref for PublicTransactionObservationGuard {
    type Target = BlockObserver;

    fn deref(&self) -> &Self::Target {
        self.observer
            .as_ref()
            .expect("observation guard already handed off")
    }
}

impl DerefMut for PublicTransactionObservationGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.observer_mut()
    }
}

impl Drop for PublicTransactionObservationGuard {
    fn drop(&mut self) {
        let _ = self.transfer();
    }
}

pub(crate) struct PublicTransactionFamily {
    tracker: PublicTransactionTracker,
    id: usize,
}

impl PublicTransactionFamily {
    pub(crate) fn is_included(&self) -> bool {
        self.tracker
            .registry
            .lock()
            .expect("public transaction registry poisoned")
            .families[self.id]
            .inclusion
            .is_some()
    }

    pub(crate) fn register(&self, hash: B256) {
        let mut registry = self
            .tracker
            .registry
            .lock()
            .expect("public transaction registry poisoned");
        let family = &mut registry.families[self.id];
        if family.hashes.contains(&hash) {
            return;
        }
        family.hashes.push(hash);
        family.inclusion = None;
        let chain_id = family.scope.chain_id();
        registry
            .hashes
            .entry((chain_id, hash))
            .or_insert(PublicTransactionLookup::Pending);
        self.notify(&registry);
    }

    pub(crate) fn included(&self, hash: B256, block: BlockNumHash, transaction_index: u64) {
        self.update(
            Some((hash, block, transaction_index)),
            PublicTransactionLookup::Unavailable,
        );
    }

    pub(crate) fn pending(&self) {
        self.update(None, PublicTransactionLookup::Pending);
    }

    pub(crate) fn recovered(&self) {
        let included = self
            .tracker
            .registry
            .lock()
            .expect("public transaction registry poisoned")
            .families[self.id]
            .inclusion
            .is_some();
        if !included {
            self.pending();
        }
    }

    pub(crate) fn unavailable(&self) {
        self.update(None, PublicTransactionLookup::Unavailable);
    }

    pub(crate) fn discovery_exhausted(&self) {
        self.update(None, PublicTransactionLookup::DiscoveryExhausted);
    }

    fn update(&self, included: Option<(B256, BlockNumHash, u64)>, other: PublicTransactionLookup) {
        let mut registry = self
            .tracker
            .registry
            .lock()
            .expect("public transaction registry poisoned");
        let Registry { hashes, families } = &mut *registry;
        let family = &mut families[self.id];
        let next_inclusion = included.map(|(_, block, _)| block);
        let mut changed = family.inclusion != next_inclusion;
        family.inclusion = next_inclusion;
        for hash in &family.hashes {
            let state = match included {
                Some((winner, block, transaction_index)) if winner == *hash => {
                    PublicTransactionLookup::Included {
                        block,
                        transaction_index,
                    }
                }
                _ => other,
            };
            let key = (family.scope.chain_id(), *hash);
            // Preserve explicit dapp forwarding eligibility across family updates and drops.
            if hashes.get(&key) != Some(&PublicTransactionLookup::DiscoveryExhausted) {
                changed |= hashes.insert(key, state) != Some(state);
            }
        }
        if changed {
            self.notify(&registry);
        }
    }

    fn notify(&self, registry: &Registry) {
        let family = &registry.families[self.id];
        registry.invalidate(&family.scope, &family.account, &family.cache);
        self.tracker.changes.send_replace(());
    }
}

impl Drop for PublicTransactionFamily {
    fn drop(&mut self) {
        let included = self
            .tracker
            .registry
            .lock()
            .expect("public transaction registry poisoned")
            .families[self.id]
            .inclusion
            .is_some();
        if !included {
            self.unavailable();
        }
    }
}

#[cfg(test)]
pub(crate) fn test_tracking_context() -> (PublicTransactionTracker, PublicTransactionTrackingContext)
{
    use crate::vault::{PublicAccountScope, PublicAccountSource, PublicAccountStatus};
    use alloy::primitives::Address;

    let tracker = PublicTransactionTracker::default();
    let http = crate::HttpContext::direct_for_tests();
    let scope = PublicBalanceScope::new(
        "wallet".into(),
        1,
        http.rpc_broker(),
        crate::RpcChainRoute::new(1, Vec::<url::Url>::new()),
    );
    let account = PublicAccountMetadata {
        public_account_uuid: "account".into(),
        address: Address::ZERO,
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet".into(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let cache = PublicBalanceCache::default();
    cache.set_scope(scope.clone());
    let context = tracker.context(scope, account, cache);
    (tracker, context)
}

#[cfg(test)]
mod tests {
    use std::time::{Instant, SystemTime};

    use alloy::primitives::U256;

    use super::*;
    use crate::public_wallet::{
        PublicAccountBalance, PublicAssetId, PublicBalanceAmount, PublicBalanceAsset,
        PublicBalanceEntry, PublicBalanceSnapshot,
    };

    fn observation(
        context: &PublicTransactionTrackingContext,
        block: BlockNumHash,
    ) -> PublicBalanceSnapshot {
        PublicBalanceSnapshot {
            chain_id: context.scope.chain_id(),
            refreshed_at: SystemTime::now(),
            accounts: vec![PublicAccountBalance {
                account: context.account.clone(),
                balances: vec![PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Native,
                        symbol: "ETH".into(),
                        decimals: 18,
                    },
                    amount: PublicBalanceAmount::Available(U256::from(5)),
                }],
                observed_at: Some(Instant::now()),
                observed_block: Some(block),
            }],
        }
    }

    #[tokio::test]
    async fn families_fence_recreated_scopes_until_every_send_is_included() {
        let (tracker, context) = test_tracking_context();
        let first = context.start_family();
        let second = context.start_family();
        let first_hash = B256::repeat_byte(1);
        let second_hash = B256::repeat_byte(2);
        let block = BlockNumHash::new(12, B256::repeat_byte(3));
        first.register(first_hash);
        second.register(second_hash);
        // Even recreation with the same scope needs outstanding work reapplied.
        context.cache.clear();
        context.cache.set_scope(context.scope.clone());
        tracker.reconcile_scope(
            &context.scope,
            std::slice::from_ref(&context.account),
            &context.cache,
        );
        let mut queued = context.cache.take_queued_refreshes();
        assert_eq!(queued.len(), 1);
        let old = queued.pop().unwrap();
        assert!(context.cache.take_queued_refreshes().is_empty());
        first.included(first_hash, block, 0);
        assert!(context.cache.take_queued_refreshes().is_empty());
        let completion = context
            .cache
            .finish_refresh(old, Some(observation(&context, block)));
        assert!(!completion.accepted);
        let current = completion.follow_up.unwrap();
        assert_eq!(current.minimum_block(), Some(block));
        assert!(
            context
                .cache
                .finish_refresh(current, Some(observation(&context, block)))
                .accepted
        );
        assert!(
            context
                .cache
                .eligible_balance(
                    &context.scope,
                    &context.account,
                    PublicAssetId::Native,
                    Instant::now()
                )
                .is_none()
        );

        // Rebinding also makes old owners fence a new authority synchronously.
        let new_scope = PublicBalanceScope::new(
            "wallet".into(),
            2,
            crate::HttpContext::direct_for_tests().rpc_broker(),
            crate::RpcChainRoute::new(1, Vec::<url::Url>::new()),
        );
        context.cache.set_scope(new_scope.clone());
        tracker.reconcile_scope(
            &new_scope,
            std::slice::from_ref(&context.account),
            &context.cache,
        );
        let old = context
            .cache
            .begin_refresh(&new_scope, context.account.status)
            .unwrap();
        second.included(second_hash, block, 1);
        let completion = context
            .cache
            .finish_refresh(old, Some(observation(&context, block)));
        assert!(!completion.accepted);
        let current = completion.follow_up.unwrap();
        // Repeated reconciliation must not obsolete otherwise current refreshes.
        tracker.reconcile_scope(
            &new_scope,
            std::slice::from_ref(&context.account),
            &context.cache,
        );
        assert!(
            context
                .cache
                .finish_refresh(current, Some(observation(&context, block)))
                .accepted
        );
        assert_eq!(
            context.cache.eligible_balance(
                &new_scope,
                &context.account,
                PublicAssetId::Native,
                Instant::now()
            ),
            Some(U256::from(5))
        );
        drop(first);
        drop(second);
        assert!(matches!(
            tracker.lookup(1, first_hash),
            PublicTransactionLookup::Included { .. }
        ));

        let scoped_pending = context.start_family();
        scoped_pending.register(B256::repeat_byte(6));
        let other_scope = PublicBalanceScope::new(
            "other-wallet".into(),
            3,
            crate::HttpContext::direct_for_tests().rpc_broker(),
            crate::RpcChainRoute::new(1, Vec::<url::Url>::new()),
        );
        let mut other = context.clone();
        other.scope = other_scope;
        other.account.scope = crate::vault::PublicAccountScope::PrivateWallet {
            wallet_uuid: "other-wallet".into(),
        };
        other.cache.set_scope(other.scope.clone());
        // An identical account ID/address cannot borrow another private wallet's pending work.
        tracker.reconcile_scope(
            &other.scope,
            std::slice::from_ref(&other.account),
            &other.cache,
        );
        assert!(other.cache.take_queued_refreshes().is_empty());
        let current = other
            .cache
            .begin_refresh(&other.scope, other.account.status)
            .unwrap();
        assert!(
            other
                .cache
                .finish_refresh(current, Some(observation(&other, block)))
                .accepted
        );
        assert_eq!(
            other.cache.eligible_balance(
                &other.scope,
                &other.account,
                PublicAssetId::Native,
                Instant::now()
            ),
            Some(U256::from(5))
        );

        let mut global = context;
        global.account.scope = crate::vault::PublicAccountScope::Global;
        let global_pending = global.start_family();
        let global_hash = B256::repeat_byte(7);
        global_pending.register(global_hash);
        // The global account keeps its pending work when the active private wallet changes.
        global.scope = other.scope;
        tracker.reconcile_scope(
            &global.scope,
            std::slice::from_ref(&global.account),
            &global.cache,
        );
        let current = global.cache.take_queued_refreshes().pop().unwrap();
        assert!(
            global
                .cache
                .finish_refresh(current, Some(observation(&global, block)))
                .accepted
        );
        assert!(
            global
                .cache
                .eligible_balance(
                    &global.scope,
                    &global.account,
                    PublicAssetId::Native,
                    Instant::now()
                )
                .is_none()
        );
        let old = global
            .cache
            .begin_refresh(&global.scope, global.account.status)
            .unwrap();
        global_pending.included(global_hash, block, 2);
        let done = global
            .cache
            .finish_refresh(old, Some(observation(&global, block)));
        assert!(!done.accepted);
        assert!(
            global
                .cache
                .finish_refresh(done.follow_up.unwrap(), Some(observation(&global, block)))
                .accepted
        );
        assert_eq!(
            global.cache.eligible_balance(
                &global.scope,
                &global.account,
                PublicAssetId::Native,
                Instant::now()
            ),
            Some(U256::from(5))
        );
    }

    #[tokio::test]
    async fn duplicate_hash_families_refresh_balances_after_both_are_included() {
        let (tracker, context) = test_tracking_context();
        let first = context.start_family();
        let second = context.start_family();
        let hash = B256::repeat_byte(1);
        let block = BlockNumHash::new(12, B256::repeat_byte(3));
        first.register(hash);
        second.register(hash);

        first.included(hash, block, 0);
        let current = context.cache.take_queued_refreshes().pop().unwrap();
        assert!(
            context
                .cache
                .finish_refresh(current, Some(observation(&context, block)))
                .accepted
        );
        assert!(
            context
                .cache
                .eligible_balance(
                    &context.scope,
                    &context.account,
                    PublicAssetId::Native,
                    Instant::now()
                )
                .is_none()
        );
        assert!(context.cache.take_queued_refreshes().is_empty());

        second.included(hash, block, 0);
        let current = context.cache.take_queued_refreshes().pop().unwrap();
        assert!(
            context
                .cache
                .finish_refresh(current, Some(observation(&context, block)))
                .accepted
        );
        assert_eq!(
            context.cache.eligible_balance(
                &context.scope,
                &context.account,
                PublicAssetId::Native,
                Instant::now()
            ),
            Some(U256::from(5))
        );
        assert_eq!(
            tracker.lookup(context.scope.chain_id(), hash),
            PublicTransactionLookup::Included {
                block,
                transaction_index: 0,
            }
        );
    }

    #[tokio::test]
    async fn failed_and_dropped_observers_keep_hashes_known() {
        let (tracker, context) = test_tracking_context();
        let family = context.start_family();
        let hash = B256::repeat_byte(4);
        family.register(hash);
        family.unavailable();
        assert_eq!(
            tracker.lookup(1, hash),
            PublicTransactionLookup::Unavailable
        );
        family.recovered();
        assert_eq!(tracker.lookup(1, hash), PublicTransactionLookup::Pending);
        drop(family);
        assert_eq!(
            tracker.lookup(1, hash),
            PublicTransactionLookup::Unavailable
        );
        assert_eq!(tracker.lookup(2, hash), PublicTransactionLookup::Untracked);

        let successor = tracker.successor();
        tracker.shutdown().await;
        let current = successor.context(
            context.scope.clone(),
            context.account.clone(),
            context.cache.clone(),
        );
        current.ensure_open().unwrap();
        assert!(context.ensure_open().is_err());
        assert_eq!(
            successor.lookup(1, hash),
            PublicTransactionLookup::Unavailable
        );
        let mut notifications = tracker.subscribe();
        let family = current.start_family();
        let new_hash = B256::repeat_byte(5);
        family.register(new_hash);
        notifications.changed().await.unwrap();
        assert_eq!(
            tracker.lookup(1, new_hash),
            PublicTransactionLookup::Pending
        );
        assert!(context.ensure_open().is_err());
    }
}
