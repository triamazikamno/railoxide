use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use alloy::primitives::U256;
use rand::RngExt as _;
use rand::rngs::StdRng;

use super::{PublicAssetId, PublicBalanceAsset, PublicBalanceSnapshot};
use crate::vault::PublicAccountMetadata;
use crate::{RpcBroker, RpcChainRoute};

/// Captured wallet and network authority for one chain's public observations.
#[derive(Clone)]
pub struct PublicBalanceScope {
    wallet_uuid: String,
    active_wallet_generation: u64,
    broker: Arc<RpcBroker>,
    route: RpcChainRoute,
}

impl PublicBalanceScope {
    #[must_use]
    pub const fn new(
        wallet_uuid: String,
        active_wallet_generation: u64,
        broker: Arc<RpcBroker>,
        route: RpcChainRoute,
    ) -> Self {
        Self {
            wallet_uuid,
            active_wallet_generation,
            broker,
            route,
        }
    }

    #[must_use]
    pub const fn chain_id(&self) -> u64 {
        self.route.chain_id()
    }

    pub(crate) fn wallet_uuid(&self) -> &str {
        &self.wallet_uuid
    }

    fn matches(&self, other: &Self) -> bool {
        self.wallet_uuid == other.wallet_uuid
            && self.active_wallet_generation == other.active_wallet_generation
            && Arc::ptr_eq(&self.broker, &other.broker)
            && self.route == other.route
    }
}

/// Consume every ticket with `finish_refresh`, including failed refreshes.
pub struct PublicBalanceRefreshTicket {
    scope: PublicBalanceScope,
    generation: u64,
    identity: Arc<()>,
    /// Exactly one account id.
    accounts: BTreeSet<String>,
    minimum: Option<u64>,
}

impl PublicBalanceRefreshTicket {
    #[must_use]
    pub const fn scope(&self) -> &PublicBalanceScope {
        &self.scope
    }

    #[must_use]
    pub fn includes_account(&self, public_account_uuid: &str) -> bool {
        self.accounts.contains(public_account_uuid)
    }

    #[must_use]
    pub const fn minimum_block(&self) -> Option<u64> {
        self.minimum
    }
}

pub struct PublicBalanceRefreshCompletion {
    /// The ticket still matched the scope and generation, even if its RPC failed.
    pub accepted: bool,
    /// Already admitted; refresh the current accounts among its account ids.
    pub follow_up: Option<PublicBalanceRefreshTicket>,
}

struct Namespace {
    identity: Arc<()>,
    scope: PublicBalanceScope,
    generation: u64,
    snapshot: Option<PublicBalanceSnapshot>,
    pending_accounts: BTreeSet<String>,
    /// The in-flight ticket's identity and account id.
    active: Option<(Arc<()>, BTreeSet<String>)>,
    queued: BTreeSet<String>,
    minimum: Option<u64>,
    /// Ids whose latest accepted refresh failed and has not since returned their row.
    failed: BTreeSet<String>,
}

impl Namespace {
    /// Serve one queued id, chosen uniformly at random.
    fn start_queued(&mut self, rng: &mut StdRng) -> PublicBalanceRefreshTicket {
        let id = pick(&self.queued, rng);
        self.queued.remove(&id);
        self.start(id)
    }

    fn start(&mut self, id: String) -> PublicBalanceRefreshTicket {
        let accounts = BTreeSet::from([id]);
        let identity = Arc::new(());
        self.active = Some((identity.clone(), accounts.clone()));
        PublicBalanceRefreshTicket {
            scope: self.scope.clone(),
            generation: self.generation,
            identity,
            accounts,
            minimum: self.minimum,
        }
    }
}

/// Requires a non-empty `ids`.
fn pick(ids: &BTreeSet<String>, rng: &mut StdRng) -> String {
    let index = rng.random_range(0..ids.len());
    ids.iter().nth(index).cloned().expect("index within ids")
}

/// Shared by desktop refreshes and gateway lookups, independently of UI selection.
#[derive(Clone)]
pub struct PublicBalanceCache {
    namespaces: Arc<Mutex<BTreeMap<u64, Namespace>>>,
    /// Picks each ticket's account. Locked only while `namespaces` is held.
    rng: Arc<Mutex<StdRng>>,
}

impl Default for PublicBalanceCache {
    fn default() -> Self {
        Self::with_rng(rand::make_rng())
    }
}

impl PublicBalanceCache {
    /// `rng` picks the order in which queued accounts are read; tests pass a seeded one.
    #[must_use]
    pub fn with_rng(rng: StdRng) -> Self {
        Self {
            namespaces: Arc::default(),
            rng: Arc::new(Mutex::new(rng)),
        }
    }

    fn rng(&self) -> MutexGuard<'_, StdRng> {
        self.rng.lock().expect("public balance cache rng poisoned")
    }

    /// Activate only from the current wallet owner, never from a captured request.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub fn set_scope(&self, scope: PublicBalanceScope) {
        let mut namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        if namespaces
            .get(&scope.chain_id())
            .is_some_and(|entry| entry.scope.matches(&scope))
        {
            return;
        }
        namespaces.insert(
            scope.chain_id(),
            Namespace {
                identity: Arc::new(()),
                scope,
                generation: 0,
                snapshot: None,
                pending_accounts: BTreeSet::new(),
                active: None,
                queued: BTreeSet::new(),
                minimum: None,
                failed: BTreeSet::new(),
            },
        );
    }

    /// Preserve unchanged authority and remove namespaces no longer enabled.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub fn reconcile_scopes(&self, scopes: Vec<PublicBalanceScope>) {
        let mut namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        namespaces.retain(|chain_id, entry| {
            scopes
                .iter()
                .any(|scope| scope.chain_id() == *chain_id && entry.scope.matches(scope))
        });
        for scope in scopes {
            namespaces
                .entry(scope.chain_id())
                .or_insert_with(|| Namespace {
                    identity: Arc::new(()),
                    scope,
                    generation: 0,
                    snapshot: None,
                    pending_accounts: BTreeSet::new(),
                    active: None,
                    queued: BTreeSet::new(),
                    minimum: None,
                    failed: BTreeSet::new(),
                });
        }
    }

    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn is_current_scope(&self, scope: &PublicBalanceScope) -> bool {
        self.namespaces
            .lock()
            .expect("public balance cache poisoned")
            .get(&scope.chain_id())
            .is_some_and(|entry| entry.scope.matches(scope))
    }

    pub(crate) fn scope_identity(&self, scope: &PublicBalanceScope) -> Option<Arc<()>> {
        let namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        let entry = namespaces.get(&scope.chain_id())?;
        entry.scope.matches(scope).then(|| entry.identity.clone())
    }

    /// Invalidate all captured tickets and observations on wallet/account reset.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    pub fn clear(&self) {
        self.namespaces
            .lock()
            .expect("public balance cache poisoned")
            .clear();
    }

    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn snapshot(&self, scope: &PublicBalanceScope) -> Option<PublicBalanceSnapshot> {
        let namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        let entry = namespaces.get(&scope.chain_id())?;
        if entry.scope.matches(scope) {
            entry.snapshot.clone()
        } else {
            None
        }
    }

    /// Authorization and eligible RPC-shape checks remain the gateway's responsibility.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn eligible_balance(
        &self,
        scope: &PublicBalanceScope,
        approved_account: &PublicAccountMetadata,
        asset: PublicAssetId,
        now: Instant,
    ) -> Option<U256> {
        self.eligible_balance_entry(scope, approved_account, asset, None, now)
            .map(|(amount, _, _)| amount)
    }

    pub(crate) fn eligible_balance_state(
        &self,
        scope: &PublicBalanceScope,
        approved_account: &PublicAccountMetadata,
        asset: &PublicBalanceAsset,
        now: Instant,
    ) -> Option<(U256, u64, Arc<()>)> {
        self.eligible_balance_entry(scope, approved_account, asset.id, Some(asset), now)
    }

    fn eligible_balance_entry(
        &self,
        scope: &PublicBalanceScope,
        approved_account: &PublicAccountMetadata,
        asset: PublicAssetId,
        metadata: Option<&PublicBalanceAsset>,
        now: Instant,
    ) -> Option<(U256, u64, Arc<()>)> {
        let namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        let entry = namespaces.get(&scope.chain_id())?;
        if !entry.scope.matches(scope)
            || !approved_account.is_scoped_to_wallet(&scope.wallet_uuid)
            || entry
                .pending_accounts
                .contains(&approved_account.public_account_uuid)
        {
            return None;
        }
        let account = entry
            .snapshot
            .as_ref()?
            .accounts
            .iter()
            .find(|account| account.account == *approved_account)?;
        let age = now.checked_duration_since(account.observed_at?)?;
        if age > Duration::from_secs(30) || account.observed_block.is_none() {
            return None;
        }
        let balance = account.balances.iter().find(|balance| {
            balance.asset.id == asset && metadata.is_none_or(|metadata| balance.asset == *metadata)
        })?;
        Some((
            balance.amount.amount()?,
            entry.generation,
            Arc::clone(&entry.identity),
        ))
    }

    /// When none is in flight, start a ticket for one of `accounts`, chosen at random, and
    /// queue the rest for its follow-ups. Otherwise queue `accounts` behind the in-flight
    /// ticket. An empty request starts nothing.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn begin_refresh<I, S>(
        &self,
        scope: &PublicBalanceScope,
        accounts: I,
    ) -> Option<PublicBalanceRefreshTicket>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        let entry = namespaces.get_mut(&scope.chain_id())?;
        if !entry.scope.matches(scope) {
            return None;
        }
        let accounts = accounts.into_iter().map(Into::into);
        if entry.active.is_some() {
            entry.queued.extend(accounts);
            return None;
        }
        let accounts: BTreeSet<String> = accounts.collect();
        if accounts.is_empty() {
            return None;
        }
        let id = pick(&accounts, &mut self.rng());
        entry.queued.extend(accounts);
        entry.queued.remove(&id);
        Some(entry.start(id))
    }

    /// Account ids in the in-flight ticket or queued behind it. Ids queued while no ticket
    /// is in flight are not reported.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn refreshing_accounts(&self, scope: &PublicBalanceScope) -> BTreeSet<String> {
        let namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        namespaces
            .get(&scope.chain_id())
            .filter(|entry| entry.scope.matches(scope))
            .and_then(|entry| {
                let (_, ids) = entry.active.as_ref()?;
                Some(ids.iter().chain(&entry.queued).cloned().collect())
            })
            .unwrap_or_default()
    }

    /// Account ids whose latest accepted refresh failed without returning their row since.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn failed_accounts(&self, scope: &PublicBalanceScope) -> BTreeSet<String> {
        let namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        namespaces
            .get(&scope.chain_id())
            .filter(|entry| entry.scope.matches(scope))
            .map(|entry| entry.failed.clone())
            .unwrap_or_default()
    }

    /// Keep only listed accounts' rows, refresh their embedded metadata, and advance
    /// every namespace's fence so in-flight tickets cannot write back old metadata.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned or a generation counter is exhausted.
    pub fn retain_accounts(&self, accounts: &[PublicAccountMetadata]) {
        let mut namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        for entry in namespaces.values_mut() {
            entry.generation = entry
                .generation
                .checked_add(1)
                .expect("balance generation exhausted");
            entry.failed.retain(|id| {
                accounts
                    .iter()
                    .any(|account| account.public_account_uuid == *id)
            });
            if let Some(snapshot) = &mut entry.snapshot {
                snapshot.accounts.retain_mut(|row| {
                    let Some(listed) = accounts.iter().find(|account| {
                        account.public_account_uuid == row.account.public_account_uuid
                    }) else {
                        return false;
                    };
                    row.account = listed.clone();
                    true
                });
            }
        }
    }

    /// Admit one queued account's refresh per idle namespace after reconciling current scopes.
    /// Active refreshes retain their queued account ids for `finish_refresh`.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn take_queued_refreshes(&self) -> Vec<PublicBalanceRefreshTicket> {
        let mut namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        namespaces
            .values_mut()
            .filter_map(|entry| {
                if entry.active.is_some() || entry.queued.is_empty() {
                    return None;
                }
                Some(entry.start_queued(&mut self.rng()))
            })
            .collect()
    }

    /// Fence the account as `fence_account` does, then queue its refresh. While no ticket
    /// is in flight the id stays queued until `take_queued_refreshes` or a ticket's follow-up
    /// admits it. `pending` is the caller's current account-chain aggregate pending state.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned or the generation counter is exhausted.
    pub fn invalidate_account(
        &self,
        scope: &PublicBalanceScope,
        account: &PublicAccountMetadata,
        pending: bool,
        inclusion: Option<u64>,
    ) {
        self.fence(scope, account, pending, inclusion, true);
    }

    /// Advance the fence, set the account's pending state, raise the inclusion minimum and
    /// clear its observation without queuing a refresh. Already queued ids stay queued.
    /// `pending` is the caller's current account-chain aggregate pending state.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned or the generation counter is exhausted.
    pub fn fence_account(
        &self,
        scope: &PublicBalanceScope,
        account: &PublicAccountMetadata,
        pending: bool,
        inclusion: Option<u64>,
    ) {
        self.fence(scope, account, pending, inclusion, false);
    }

    fn fence(
        &self,
        scope: &PublicBalanceScope,
        account: &PublicAccountMetadata,
        pending: bool,
        inclusion: Option<u64>,
        queue: bool,
    ) {
        let mut namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        let Some(entry) = namespaces.get_mut(&scope.chain_id()) else {
            return;
        };
        if !entry.scope.matches(scope) || !account.is_scoped_to_wallet(&scope.wallet_uuid) {
            return;
        }
        entry.generation = entry
            .generation
            .checked_add(1)
            .expect("balance generation exhausted");
        if pending {
            entry
                .pending_accounts
                .insert(account.public_account_uuid.clone());
        } else {
            entry.pending_accounts.remove(&account.public_account_uuid);
        }
        if let Some(snapshot) = &mut entry.snapshot {
            for balance in &mut snapshot.accounts {
                if balance.account.public_account_uuid == account.public_account_uuid {
                    balance.observed_at = None;
                    balance.observed_block = None;
                }
            }
        }
        entry.minimum = entry.minimum.max(inclusion);
        if queue {
            entry.queued.insert(account.public_account_uuid.clone());
        }
    }

    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn finish_refresh(
        &self,
        ticket: PublicBalanceRefreshTicket,
        refreshed: Option<PublicBalanceSnapshot>,
    ) -> PublicBalanceRefreshCompletion {
        let PublicBalanceRefreshTicket {
            scope,
            generation,
            identity,
            accounts,
            minimum,
        } = ticket;
        let rejected = || PublicBalanceRefreshCompletion {
            accepted: false,
            follow_up: None,
        };
        let mut namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        let Some(entry) = namespaces.get_mut(&scope.chain_id()) else {
            return rejected();
        };
        if !entry.scope.matches(&scope)
            || !entry
                .active
                .as_ref()
                .is_some_and(|(active, _)| Arc::ptr_eq(active, &identity))
        {
            return rejected();
        }
        entry.active = None;
        let accepted = entry.generation == generation;
        if accepted {
            if let Some(mut refreshed) =
                refreshed.filter(|snapshot| snapshot.chain_id == scope.chain_id())
            {
                refreshed.accounts.retain(|account| {
                    accounts.contains(&account.account.public_account_uuid)
                        && account.account.is_scoped_to_wallet(&scope.wallet_uuid)
                });
                for account in &mut refreshed.accounts {
                    let complete = !account.balances.is_empty()
                        && account
                            .balances
                            .iter()
                            .all(|balance| balance.amount.amount().is_some())
                        && account
                            .observed_block
                            .is_some_and(|block| minimum.is_none_or(|minimum| block >= minimum));
                    if !complete {
                        account.observed_at = None;
                        account.observed_block = None;
                    }
                }
                let refreshed_ids: BTreeSet<_> = refreshed
                    .accounts
                    .iter()
                    .map(|account| account.account.public_account_uuid.clone())
                    .collect();
                entry.failed.retain(|id| !refreshed_ids.contains(id));
                if let Some(current) = &entry.snapshot {
                    refreshed.accounts.extend(
                        current
                            .accounts
                            .iter()
                            .filter(|account| {
                                !refreshed_ids.contains(&account.account.public_account_uuid)
                            })
                            .cloned(),
                    );
                }
                entry.snapshot = Some(refreshed);
            } else {
                entry.failed.extend(accounts.iter().cloned());
                if let Some(snapshot) = &mut entry.snapshot {
                    for account in &mut snapshot.accounts {
                        if accounts.contains(&account.account.public_account_uuid) {
                            account.observed_at = None;
                            account.observed_block = None;
                        }
                    }
                }
            }
        } else {
            entry.queued.extend(accounts);
        }
        let follow_up = (!entry.queued.is_empty()).then(|| entry.start_queued(&mut self.rng()));
        PublicBalanceRefreshCompletion {
            accepted,
            follow_up,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use alloy::primitives::Address;
    use rand::SeedableRng as _;

    use super::*;
    use crate::HttpContext;
    use crate::public_wallet::{
        PublicAccountBalance, PublicBalanceAmount, PublicBalanceAsset, PublicBalanceEntry,
    };
    use crate::vault::{PublicAccountScope, PublicAccountSource, PublicAccountStatus};

    fn account(id: &str, status: PublicAccountStatus) -> PublicAccountMetadata {
        PublicAccountMetadata {
            public_account_uuid: id.into(),
            address: Address::ZERO,
            label: None,
            source: PublicAccountSource::Derived,
            scope: PublicAccountScope::PrivateWallet {
                wallet_uuid: "wallet".into(),
            },
            derivation_index: Some(0),
            hardware_descriptor: None,
            status,
            display_order: 0,
        }
    }

    fn observation(
        chain_id: u64,
        account: &PublicAccountMetadata,
        time: Instant,
        block: u64,
    ) -> PublicBalanceSnapshot {
        PublicBalanceSnapshot {
            chain_id,
            refreshed_at: SystemTime::now(),
            accounts: vec![PublicAccountBalance {
                account: account.clone(),
                balances: vec![PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Native,
                        symbol: "ETH".into(),
                        decimals: 18,
                    },
                    amount: PublicBalanceAmount::Available(U256::from(5)),
                }],
                observed_at: Some(time),
                observed_block: Some(block),
            }],
        }
    }

    #[tokio::test]
    async fn independent_chains_coalesce_inclusion_and_reject_obsolete_observations() {
        let http = HttpContext::direct_for_tests();
        let scope = |chain_id| {
            PublicBalanceScope::new(
                "wallet".into(),
                1,
                http.rpc_broker(),
                RpcChainRoute::new(chain_id, Vec::<url::Url>::new()),
            )
        };
        let first = scope(1);
        let second = scope(137);
        let cache = PublicBalanceCache::default();
        cache.set_scope(first.clone());
        cache.set_scope(second.clone());
        let active = account("active", PublicAccountStatus::Active);
        let now = Instant::now();
        let before = 10;
        let inclusion = 11;
        let old = cache.begin_refresh(&first, ["active"]).unwrap();
        let other = cache.begin_refresh(&second, ["active"]).unwrap();
        assert!(cache.begin_refresh(&first, ["inactive"]).is_none());
        cache.invalidate_account(&first, &active, true, Some(inclusion));
        cache.invalidate_account(&first, &active, true, Some(inclusion));
        let other_done = cache.finish_refresh(other, Some(observation(137, &active, now, before)));
        assert!(other_done.accepted);
        assert!(other_done.follow_up.is_none());
        assert_eq!(
            cache.eligible_balance(&second, &active, PublicAssetId::Native, now),
            Some(U256::from(5))
        );

        let done = cache.finish_refresh(old, Some(observation(1, &active, now, before)));
        assert!(!done.accepted);
        assert!(cache.snapshot(&first).is_none());
        // The rejected account and the queued one are read next, one per ticket.
        let mut follow_up = done.follow_up;
        let mut served = BTreeSet::new();
        while let Some(ticket) = follow_up {
            assert_eq!(ticket.minimum_block(), Some(inclusion));
            let id = if ticket.includes_account("active") {
                "active"
            } else {
                "inactive"
            };
            assert!(served.insert(id.to_owned()));
            let done = cache.finish_refresh(ticket, Some(observation(1, &active, now, inclusion)));
            assert!(done.accepted);
            follow_up = done.follow_up;
        }
        assert_eq!(served, ids(&["active", "inactive"]));
        assert!(
            cache
                .eligible_balance(&first, &active, PublicAssetId::Native, now)
                .is_none()
        );
        cache.invalidate_account(&first, &active, false, Some(inclusion));
        // A result below the inclusion block is kept without an observation; one at it is current.
        for (block, eligible) in [(before, None), (inclusion, Some(U256::from(5)))] {
            let refresh = cache.begin_refresh(&first, ["active"]).unwrap();
            let done = cache.finish_refresh(refresh, Some(observation(1, &active, now, block)));
            assert!(done.accepted);
            assert_eq!(
                cache.eligible_balance(&first, &active, PublicAssetId::Native, now),
                eligible
            );
        }

        let obsolete = cache.begin_refresh(&first, ["active"]).unwrap();
        let changed =
            PublicBalanceScope::new("wallet".into(), 2, http.rpc_broker(), first.route.clone());
        cache.set_scope(changed.clone());
        assert!(cache.snapshot(&first).is_none());
        assert!(
            !cache
                .finish_refresh(obsolete, Some(observation(1, &active, now, inclusion)))
                .accepted
        );
        assert!(cache.snapshot(&changed).is_none());
    }

    #[tokio::test]
    async fn account_merges_preserve_account_age_and_failed_observations_disable_reuse() {
        let http = HttpContext::direct_for_tests();
        let scope = PublicBalanceScope::new(
            "wallet".into(),
            1,
            http.rpc_broker(),
            RpcChainRoute::new(1, Vec::<url::Url>::new()),
        );
        let cache = PublicBalanceCache::default();
        cache.set_scope(scope.clone());
        let active = account("active", PublicAccountStatus::Active);
        let inactive = account("inactive", PublicAccountStatus::Inactive);
        let observed = Instant::now();
        let block = 10;
        let inactive_observed = observed + Duration::from_secs(31);
        let inactive_block = 11;
        let active_snapshot = observation(1, &active, observed, block);
        let inactive_snapshot = observation(1, &inactive, inactive_observed, inactive_block);
        let active_balances = active_snapshot.accounts[0].balances.clone();
        let inactive_balances = inactive_snapshot.accounts[0].balances.clone();
        let refresh = cache.begin_refresh(&scope, ["active"]).unwrap();
        let _ = cache.finish_refresh(refresh, Some(active_snapshot));
        let refresh = cache.begin_refresh(&scope, ["inactive"]).unwrap();
        let _ = cache.finish_refresh(refresh, Some(inactive_snapshot));
        let merged = cache.snapshot(&scope).unwrap();
        assert!(merged.accounts.iter().any(|account| {
            account.account.public_account_uuid == active.public_account_uuid
                && account.account.status == PublicAccountStatus::Active
                && account.balances == active_balances
                && account.observed_at == Some(observed)
                && account.observed_block == Some(block)
        }));
        assert!(merged.accounts.iter().any(|account| {
            account.account.public_account_uuid == inactive.public_account_uuid
                && account.account.status == PublicAccountStatus::Inactive
                && account.balances == inactive_balances
                && account.observed_at == Some(inactive_observed)
                && account.observed_block == Some(inactive_block)
        }));
        assert_eq!(
            cache.eligible_balance(
                &scope,
                &active,
                PublicAssetId::Native,
                observed + Duration::from_secs(30)
            ),
            Some(U256::from(5))
        );
        assert!(
            cache
                .eligible_balance(
                    &scope,
                    &active,
                    PublicAssetId::Native,
                    observed + Duration::from_secs(31)
                )
                .is_none()
        );
        let mut changed = active.clone();
        changed.address = Address::repeat_byte(1);
        assert!(
            cache
                .eligible_balance(&scope, &changed, PublicAssetId::Native, observed)
                .is_none()
        );
        let refresh = cache.begin_refresh(&scope, ["active"]).unwrap();
        let mut partial = observation(1, &active, observed, block);
        partial.accounts[0].balances[0].amount = PublicBalanceAmount::Unavailable;
        let _ = cache.finish_refresh(refresh, Some(partial));
        assert!(
            cache
                .eligible_balance(&scope, &active, PublicAssetId::Native, observed)
                .is_none()
        );
        let refresh = cache.begin_refresh(&scope, ["inactive"]).unwrap();
        let _ = cache.finish_refresh(refresh, None);
        assert!(
            cache
                .eligible_balance(
                    &scope,
                    &inactive,
                    PublicAssetId::Native,
                    observed + Duration::from_secs(31)
                )
                .is_none()
        );
    }

    fn row(
        cache: &PublicBalanceCache,
        scope: &PublicBalanceScope,
        id: &str,
    ) -> Option<PublicAccountBalance> {
        cache
            .snapshot(scope)?
            .accounts
            .into_iter()
            .find(|row| row.account.public_account_uuid == id)
    }

    /// Refreshes `accounts` through accepted tickets, one per account.
    fn refresh_all(
        cache: &PublicBalanceCache,
        scope: &PublicBalanceScope,
        accounts: &[&PublicAccountMetadata],
        time: Instant,
        block: u64,
    ) {
        let mut ticket = cache.begin_refresh(
            scope,
            accounts
                .iter()
                .map(|account| account.public_account_uuid.clone()),
        );
        let mut result = observation(scope.chain_id(), accounts[0], time, block);
        for account in &accounts[1..] {
            result
                .accounts
                .extend(observation(scope.chain_id(), account, time, block).accounts);
        }
        while let Some(current) = ticket {
            let done = cache.finish_refresh(current, Some(result.clone()));
            assert!(done.accepted);
            ticket = done.follow_up;
        }
    }

    fn ids(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[tokio::test]
    async fn account_tickets_refresh_and_requeue_only_their_ids() {
        let http = HttpContext::direct_for_tests();
        let scope = PublicBalanceScope::new(
            "wallet".into(),
            1,
            http.rpc_broker(),
            RpcChainRoute::new(1, Vec::<url::Url>::new()),
        );
        let cache = PublicBalanceCache::default();
        cache.set_scope(scope.clone());
        let a = account("a", PublicAccountStatus::Active);
        let b = account("b", PublicAccountStatus::Active);
        let seeded = Instant::now();
        let seeded_block = 10;
        refresh_all(&cache, &scope, &[&a, &b], seeded, seeded_block);

        let only_a = cache.begin_refresh(&scope, ["a"]).unwrap();
        // Re-requesting the in-flight account queues a follow-up read, as does a second account.
        assert!(cache.begin_refresh(&scope, ["a"]).is_none());
        assert!(cache.begin_refresh(&scope, ["c"]).is_none());
        assert_eq!(cache.refreshing_accounts(&scope), ids(&["a", "c"]));
        let later = seeded + Duration::from_secs(5);
        let later_block = 11;
        // Rows outside the ticket are ignored even if the read returned them.
        let mut result = observation(1, &a, later, later_block);
        result
            .accounts
            .extend(observation(1, &b, later, later_block).accounts);
        let done = cache.finish_refresh(only_a, Some(result));
        assert!(done.accepted);
        let follow_up = done.follow_up.unwrap();
        assert_ne!(
            follow_up.includes_account("a"),
            follow_up.includes_account("c")
        );
        assert!(!follow_up.includes_account("b"));
        assert_eq!(row(&cache, &scope, "a").unwrap().observed_at, Some(later));
        let untouched = row(&cache, &scope, "b").unwrap();
        assert_eq!(untouched.observed_at, Some(seeded));
        assert_eq!(untouched.observed_block, Some(seeded_block));

        // Inclusion queues only its account, and the fenced ticket re-queues its id.
        // A rejected failure records nothing as failed.
        cache.invalidate_account(&scope, &b, false, None);
        let done = cache.finish_refresh(follow_up, None);
        assert!(!done.accepted);
        assert!(cache.failed_accounts(&scope).is_empty());
        let mut retry = done.follow_up;
        assert_eq!(cache.refreshing_accounts(&scope), ids(&["a", "b", "c"]));
        // An accepted failure clears its account's observation and records it as failed,
        // in this chain's namespace only.
        let other = PublicBalanceScope::new(
            "wallet".into(),
            1,
            http.rpc_broker(),
            RpcChainRoute::new(137, Vec::<url::Url>::new()),
        );
        cache.set_scope(other.clone());
        while let Some(ticket) = retry {
            let done = cache.finish_refresh(ticket, None);
            assert!(done.accepted);
            retry = done.follow_up;
        }
        assert_eq!(row(&cache, &scope, "a").unwrap().observed_at, None);
        assert!(cache.refreshing_accounts(&scope).is_empty());
        assert_eq!(cache.failed_accounts(&scope), ids(&["a", "b", "c"]));
        assert!(cache.failed_accounts(&other).is_empty());
        // A success clears only the ids whose rows it returned.
        let ticket = cache.begin_refresh(&scope, ["a"]).unwrap();
        let done = cache.finish_refresh(ticket, Some(observation(1, &a, later, later_block)));
        assert!(done.accepted);
        assert_eq!(cache.failed_accounts(&scope), ids(&["b", "c"]));

        // An idle queue is not reported as refreshing, and another account's ticket
        // neither reads it nor drains it.
        cache.invalidate_account(&scope, &a, false, None);
        assert!(cache.refreshing_accounts(&scope).is_empty());
        assert!(cache.begin_refresh(&scope, Vec::<String>::new()).is_none());
        let only_b = cache.begin_refresh(&scope, ["b"]).unwrap();
        assert!(only_b.includes_account("b") && !only_b.includes_account("a"));
        assert_eq!(cache.refreshing_accounts(&scope), ids(&["a", "b"]));
        let done = cache.finish_refresh(only_b, Some(observation(1, &b, later, later_block)));
        assert!(done.accepted);
        let follow_up = done.follow_up.unwrap();
        assert!(follow_up.includes_account("a") && !follow_up.includes_account("b"));
    }

    #[tokio::test]
    async fn tickets_serve_one_random_account_and_a_fence_requeues_only_its_account() {
        let http = HttpContext::direct_for_tests();
        let scope = PublicBalanceScope::new(
            "wallet".into(),
            1,
            http.rpc_broker(),
            RpcChainRoute::new(1, Vec::<url::Url>::new()),
        );
        let requested = ["a", "b", "c"];
        let mut orders = Vec::new();
        for seed in 0..8 {
            let cache = PublicBalanceCache::with_rng(StdRng::seed_from_u64(seed));
            cache.set_scope(scope.clone());
            let mut ticket = cache.begin_refresh(&scope, requested);
            let mut order = Vec::new();
            while let Some(current) = ticket {
                let unserved = requested
                    .into_iter()
                    .filter(|id| !order.contains(id))
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>();
                assert_eq!(cache.refreshing_accounts(&scope), unserved);
                let served = requested
                    .into_iter()
                    .filter(|id| current.includes_account(id))
                    .collect::<Vec<_>>();
                assert_eq!(served.len(), 1, "each ticket reads one account");
                order.extend(served);
                ticket = cache.finish_refresh(current, None).follow_up;
            }
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, requested, "every account is read once");
            orders.push(order);
        }
        assert!(
            orders.iter().any(|order| order != &requested),
            "the read order does not follow id order"
        );

        // A fence while the second account is read rejects only that account's work.
        let cache = PublicBalanceCache::default();
        cache.set_scope(scope.clone());
        let accounts = requested.map(|id| account(id, PublicAccountStatus::Active));
        let served = |ticket: &PublicBalanceRefreshTicket| {
            accounts
                .iter()
                .find(|account| ticket.includes_account(&account.public_account_uuid))
                .unwrap()
        };
        let now = Instant::now();
        let first = cache.begin_refresh(&scope, requested).unwrap();
        let first_account = served(&first);
        let done = cache.finish_refresh(first, Some(observation(1, first_account, now, 10)));
        assert!(done.accepted);
        let second = done.follow_up.unwrap();
        let second_account = served(&second);
        cache.fence_account(
            &scope,
            &account("d", PublicAccountStatus::Active),
            false,
            None,
        );
        let done = cache.finish_refresh(second, Some(observation(1, second_account, now, 10)));
        assert!(!done.accepted);
        let first_id = &first_account.public_account_uuid;
        assert_eq!(
            row(&cache, &scope, first_id).unwrap().observed_at,
            Some(now)
        );
        let mut unread = ids(&requested);
        unread.remove(first_id);
        assert_eq!(cache.refreshing_accounts(&scope), unread);
    }

    #[tokio::test]
    async fn retained_accounts_keep_rows_and_reject_refreshes_of_changed_accounts() {
        let http = HttpContext::direct_for_tests();
        let scope = PublicBalanceScope::new(
            "wallet".into(),
            1,
            http.rpc_broker(),
            RpcChainRoute::new(1, Vec::<url::Url>::new()),
        );
        let cache = PublicBalanceCache::default();
        cache.set_scope(scope.clone());
        let a = account("a", PublicAccountStatus::Active);
        let b = account("b", PublicAccountStatus::Active);
        let c = account("c", PublicAccountStatus::Active);
        let observed = Instant::now();
        let block = 10;
        refresh_all(&cache, &scope, &[&a, &b], observed, block);
        let before = cache.snapshot(&scope).unwrap().accounts;

        cache.retain_accounts(&[a.clone(), b.clone(), c.clone()]);
        assert_eq!(cache.snapshot(&scope).unwrap().accounts, before);

        let held = cache.begin_refresh(&scope, ["a"]).unwrap();
        let mut deactivated = a.clone();
        deactivated.status = PublicAccountStatus::Inactive;
        cache.retain_accounts(&[deactivated.clone(), b.clone(), c.clone()]);
        let later = observed + Duration::from_secs(5);
        let done = cache.finish_refresh(held, Some(observation(1, &a, later, block)));
        assert!(!done.accepted);
        let kept = row(&cache, &scope, "a").unwrap();
        assert_eq!(kept.account, deactivated);
        assert_eq!(kept.observed_at, Some(observed));

        // The re-queued follow-up is also obsoleted when its account is deleted.
        let held = done.follow_up.unwrap();
        cache.retain_accounts(&[b, c]);
        let done = cache.finish_refresh(held, Some(observation(1, &deactivated, later, block)));
        assert!(!done.accepted);
        assert!(row(&cache, &scope, "a").is_none());
        assert_eq!(
            row(&cache, &scope, "b").unwrap().observed_at,
            Some(observed)
        );
    }
}
