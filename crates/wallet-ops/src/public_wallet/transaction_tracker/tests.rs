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
            observed_block: Some(block.number),
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
    // Reconciliation fences without queuing work, so the refresh starts explicitly.
    assert!(context.cache.take_queued_refreshes().is_empty());
    assert!(context.cache.refreshing_accounts(&context.scope).is_empty());
    let old = context
        .cache
        .begin_refresh(
            &context.scope,
            [context.account.public_account_uuid.as_str()],
        )
        .unwrap();
    first.included(first_hash, block, 0);
    assert!(context.cache.take_queued_refreshes().is_empty());
    let completion = context
        .cache
        .finish_refresh(old, Some(observation(&context, block)));
    assert!(!completion.accepted);
    let current = completion.follow_up.unwrap();
    assert_eq!(current.minimum_block(), Some(block.number));
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
    // The new namespace regains the inclusion minimum and pending restriction unqueued.
    assert!(context.cache.take_queued_refreshes().is_empty());
    let probe = context
        .cache
        .begin_refresh(&new_scope, [context.account.public_account_uuid.as_str()])
        .unwrap();
    assert_eq!(probe.minimum_block(), Some(block.number));
    assert!(
        context
            .cache
            .finish_refresh(probe, Some(observation(&context, block)))
            .accepted
    );
    assert!(
        context
            .cache
            .eligible_balance(
                &new_scope,
                &context.account,
                PublicAssetId::Native,
                Instant::now()
            )
            .is_none()
    );
    let old = context
        .cache
        .begin_refresh(&new_scope, [context.account.public_account_uuid.as_str()])
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
    // An idle relabel or deactivation keeps the row's observation and queues nothing.
    let mut relabeled = context.account.clone();
    relabeled.label = Some("renamed".into());
    relabeled.status = crate::vault::PublicAccountStatus::Inactive;
    tracker.reconcile_scope(&new_scope, std::slice::from_ref(&relabeled), &context.cache);
    assert!(context.cache.take_queued_refreshes().is_empty());
    assert!(context.cache.refreshing_accounts(&new_scope).is_empty());
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
        .begin_refresh(&other.scope, [other.account.public_account_uuid.as_str()])
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
    assert!(global.cache.take_queued_refreshes().is_empty());
    let current = global
        .cache
        .begin_refresh(&global.scope, [global.account.public_account_uuid.as_str()])
        .unwrap();
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
        .begin_refresh(&global.scope, [global.account.public_account_uuid.as_str()])
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
