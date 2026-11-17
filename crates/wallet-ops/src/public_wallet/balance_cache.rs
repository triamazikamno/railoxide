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
mod tests;
