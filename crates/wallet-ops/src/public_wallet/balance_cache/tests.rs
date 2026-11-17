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
