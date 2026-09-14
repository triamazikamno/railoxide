use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::eips::BlockNumHash;
use alloy::primitives::U256;

use super::{PublicAssetId, PublicBalanceAsset, PublicBalanceSnapshot};
use crate::vault::{PublicAccountMetadata, PublicAccountStatus};
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

#[derive(Clone, Copy, Default)]
struct RefreshStatuses {
    active: bool,
    inactive: bool,
}

impl RefreshStatuses {
    const fn insert(&mut self, status: PublicAccountStatus) {
        match status {
            PublicAccountStatus::Active => self.active = true,
            PublicAccountStatus::Inactive => self.inactive = true,
        }
    }

    const fn contains(self, status: PublicAccountStatus) -> bool {
        match status {
            PublicAccountStatus::Active => self.active,
            PublicAccountStatus::Inactive => self.inactive,
        }
    }
}

/// Consume every ticket with `finish_refresh`, including failed refreshes.
pub struct PublicBalanceRefreshTicket {
    scope: PublicBalanceScope,
    generation: u64,
    identity: Arc<()>,
    statuses: RefreshStatuses,
    minimum: Option<BlockNumHash>,
}

impl PublicBalanceRefreshTicket {
    #[must_use]
    pub const fn scope(&self) -> &PublicBalanceScope {
        &self.scope
    }

    #[must_use]
    pub const fn includes_status(&self, status: PublicAccountStatus) -> bool {
        self.statuses.contains(status)
    }

    #[must_use]
    pub const fn minimum_block(&self) -> Option<BlockNumHash> {
        self.minimum
    }
}

pub struct PublicBalanceRefreshCompletion {
    /// The ticket still matched the scope and generation, even if its RPC failed.
    pub accepted: bool,
    /// Already admitted; refresh the current accounts in its selected statuses.
    pub follow_up: Option<PublicBalanceRefreshTicket>,
}

struct Namespace {
    identity: Arc<()>,
    scope: PublicBalanceScope,
    generation: u64,
    snapshot: Option<PublicBalanceSnapshot>,
    pending_accounts: BTreeSet<String>,
    active: Option<Arc<()>>,
    queued: RefreshStatuses,
    minimum: Option<BlockNumHash>,
}

impl Namespace {
    fn start(&mut self, statuses: RefreshStatuses) -> PublicBalanceRefreshTicket {
        let identity = Arc::new(());
        self.active = Some(identity.clone());
        PublicBalanceRefreshTicket {
            scope: self.scope.clone(),
            generation: self.generation,
            identity,
            statuses,
            minimum: self.minimum,
        }
    }
}

/// Shared by desktop refreshes and gateway lookups, independently of UI selection.
#[derive(Clone, Default)]
pub struct PublicBalanceCache {
    namespaces: Arc<Mutex<BTreeMap<u64, Namespace>>>,
}

impl PublicBalanceCache {
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
                queued: RefreshStatuses::default(),
                minimum: None,
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
                    queued: RefreshStatuses::default(),
                    minimum: None,
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

    /// # Panics
    /// Panics if the cache mutex is poisoned.
    #[must_use]
    pub fn begin_refresh(
        &self,
        scope: &PublicBalanceScope,
        status: PublicAccountStatus,
    ) -> Option<PublicBalanceRefreshTicket> {
        let mut namespaces = self
            .namespaces
            .lock()
            .expect("public balance cache poisoned");
        let entry = namespaces.get_mut(&scope.chain_id())?;
        if !entry.scope.matches(scope) {
            return None;
        }
        entry.queued.insert(status);
        if entry.active.is_some() {
            return None;
        }
        let statuses = std::mem::take(&mut entry.queued);
        Some(entry.start(statuses))
    }

    /// Admit queued refreshes for idle namespaces after reconciling current scopes.
    /// Active refreshes retain their queued statuses for `finish_refresh`.
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
                if entry.active.is_some() || !(entry.queued.active || entry.queued.inactive) {
                    return None;
                }
                let statuses = std::mem::take(&mut entry.queued);
                Some(entry.start(statuses))
            })
            .collect()
    }

    /// Advance the fence before invalidating an account or queuing its refresh.
    /// `pending` is the caller's current account-chain aggregate pending state.
    ///
    /// # Panics
    /// Panics if the cache mutex is poisoned or the generation counter is exhausted.
    pub fn invalidate_account(
        &self,
        scope: &PublicBalanceScope,
        account: &PublicAccountMetadata,
        pending: bool,
        inclusion: Option<BlockNumHash>,
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
        if let Some(inclusion) = inclusion
            && entry
                .minimum
                .is_none_or(|minimum| inclusion.number >= minimum.number)
        {
            entry.minimum = Some(inclusion);
        }
        entry.queued.insert(account.status);
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
            statuses,
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
                .is_some_and(|active| Arc::ptr_eq(active, &identity))
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
                    statuses.contains(account.account.status)
                        && account.account.is_scoped_to_wallet(&scope.wallet_uuid)
                });
                for account in &mut refreshed.accounts {
                    let complete = !account.balances.is_empty()
                        && account
                            .balances
                            .iter()
                            .all(|balance| balance.amount.amount().is_some())
                        && account.observed_block.is_some_and(|block| {
                            minimum.is_none_or(|minimum| {
                                block.number > minimum.number || block == minimum
                            })
                        });
                    if !complete {
                        account.observed_at = None;
                        account.observed_block = None;
                    }
                }
                if let Some(current) = &entry.snapshot {
                    let refreshed_ids: BTreeSet<_> = refreshed
                        .accounts
                        .iter()
                        .map(|account| account.account.public_account_uuid.clone())
                        .collect();
                    refreshed.accounts.extend(
                        current
                            .accounts
                            .iter()
                            .filter(|account| {
                                !statuses.contains(account.account.status)
                                    && !refreshed_ids.contains(&account.account.public_account_uuid)
                            })
                            .cloned(),
                    );
                }
                entry.snapshot = Some(refreshed);
            } else if let Some(snapshot) = &mut entry.snapshot {
                for account in &mut snapshot.accounts {
                    if statuses.contains(account.account.status) {
                        account.observed_at = None;
                        account.observed_block = None;
                    }
                }
            }
        } else {
            // The namespace fence also obsoletes unaffected accounts in this batch.
            entry.queued.active = entry.queued.active || statuses.active;
            entry.queued.inactive = entry.queued.inactive || statuses.inactive;
        }
        let follow_up = if entry.queued.active || entry.queued.inactive {
            let statuses = std::mem::take(&mut entry.queued);
            Some(entry.start(statuses))
        } else {
            None
        };
        PublicBalanceRefreshCompletion {
            accepted,
            follow_up,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use alloy::primitives::{Address, B256};

    use super::*;
    use crate::HttpContext;
    use crate::public_wallet::{
        PublicAccountBalance, PublicBalanceAmount, PublicBalanceAsset, PublicBalanceEntry,
    };
    use crate::vault::{PublicAccountScope, PublicAccountSource};

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
        block: BlockNumHash,
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
        let inactive = account("inactive", PublicAccountStatus::Inactive);
        let now = Instant::now();
        let before = BlockNumHash::new(10, B256::ZERO);
        let inclusion = BlockNumHash::new(11, B256::repeat_byte(1));
        let old = cache.begin_refresh(&first, active.status).unwrap();
        let other = cache.begin_refresh(&second, active.status).unwrap();
        assert!(cache.begin_refresh(&first, inactive.status).is_none());
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
        let follow_up = done.follow_up.unwrap();
        assert!(follow_up.includes_status(active.status));
        assert!(follow_up.includes_status(inactive.status));
        assert_eq!(follow_up.minimum_block(), Some(inclusion));
        let done = cache.finish_refresh(follow_up, Some(observation(1, &active, now, inclusion)));
        assert!(done.accepted);
        assert!(done.follow_up.is_none());
        assert!(
            cache
                .eligible_balance(&first, &active, PublicAssetId::Native, now)
                .is_none()
        );
        cache.invalidate_account(&first, &active, false, Some(inclusion));
        let refresh = cache.begin_refresh(&first, active.status).unwrap();
        let _ = cache.finish_refresh(refresh, Some(observation(1, &active, now, inclusion)));
        assert_eq!(
            cache.eligible_balance(&first, &active, PublicAssetId::Native, now),
            Some(U256::from(5))
        );

        let obsolete = cache.begin_refresh(&first, active.status).unwrap();
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
    async fn status_merges_preserve_account_age_and_failed_observations_disable_reuse() {
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
        let block = BlockNumHash::new(10, B256::ZERO);
        let inactive_observed = observed + Duration::from_secs(31);
        let inactive_block = BlockNumHash::new(11, B256::repeat_byte(1));
        let active_snapshot = observation(1, &active, observed, block);
        let inactive_snapshot = observation(1, &inactive, inactive_observed, inactive_block);
        let active_balances = active_snapshot.accounts[0].balances.clone();
        let inactive_balances = inactive_snapshot.accounts[0].balances.clone();
        let refresh = cache.begin_refresh(&scope, active.status).unwrap();
        let _ = cache.finish_refresh(refresh, Some(active_snapshot));
        let refresh = cache.begin_refresh(&scope, inactive.status).unwrap();
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
        let refresh = cache.begin_refresh(&scope, active.status).unwrap();
        let mut partial = observation(1, &active, observed, block);
        partial.accounts[0].balances[0].amount = PublicBalanceAmount::Unavailable;
        let _ = cache.finish_refresh(refresh, Some(partial));
        assert!(
            cache
                .eligible_balance(&scope, &active, PublicAssetId::Native, observed)
                .is_none()
        );
        let refresh = cache.begin_refresh(&scope, inactive.status).unwrap();
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
}
