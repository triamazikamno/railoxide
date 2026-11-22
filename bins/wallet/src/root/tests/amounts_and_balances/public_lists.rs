use super::*;

#[test]
fn public_list_totals_keep_partial_prices_and_exclude_inactive_accounts() {
    use crate::root::public_balances::{public_active_usd_total, public_balances_usd_total};
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    let mut snapshot = public_balance_snapshot_for_test(1);
    snapshot.accounts[0].balances[0].amount =
        PublicBalanceAmount::Available(uint!(1_000_000_000_000_000_000_U256));
    let priced = public_balances_usd_total(&snapshot.accounts[0].balances, 1, Some(&cache));
    assert_eq!(priced.value, Some(uint!(3_000_000_000_U256)));
    assert!(!priced.partial);
    snapshot.accounts[0].balances.push(PublicBalanceEntry {
        asset: PublicBalanceAsset {
            id: PublicAssetId::Erc20(Address::repeat_byte(0x77)),
            symbol: "UNKNOWN".into(),
            decimals: 18,
        },
        amount: PublicBalanceAmount::Available(U256::from(1)),
    });
    let mut inactive = snapshot.accounts[0].clone();
    inactive.account.public_account_uuid = "inactive".into();
    inactive.account.status = PublicAccountStatus::Inactive;
    snapshot.accounts.push(inactive);
    let accounts = snapshot
        .accounts
        .iter()
        .map(|entry| entry.account.clone())
        .collect::<Vec<_>>();
    let total = public_active_usd_total(Some(&snapshot), &accounts, 1, Some(&cache));
    assert_eq!(total.value, priced.value);
    assert!(total.partial);
    let unpriced = public_balances_usd_total(&snapshot.accounts[0].balances, 1, None);
    assert_eq!(unpriced.value, None);
    assert!(unpriced.partial);
    assert_eq!(
        public_active_usd_total(Some(&snapshot), &accounts, 56, Some(&cache)).value,
        None
    );
}

#[test]
fn public_list_amounts_keep_dust_visible_and_abbreviate_to_tile_width() {
    use crate::root::public_balances::public_balance_tile_amount;
    let mut snapshot = public_balance_snapshot_for_test(1);
    for (amount, decimals, expected) in [
        (U256::ZERO, 18, "0"),
        (U256::from(1), 18, "<0.0001"),
        (uint!(100_000_000_000_000_U256), 18, "0.0001"),
        (uint!(1_234_567_890_U256), 6, "1,234.567"),
        (uint!(123_456_789_U256), 0, "1.234e8"),
    ] {
        let amount = PublicBalanceAmount::Available(amount);
        assert_eq!(public_balance_tile_amount(&amount, decimals), expected);
        snapshot.accounts[0].balances[0].amount = amount;
        assert_eq!(
            public_account_visible_balances_for_chain(
                Some(&snapshot),
                1,
                "public-account",
                PublicAccountStatus::Active
            )
            .is_empty(),
            expected == "0"
        );
    }
}

#[test]
fn public_account_sort_orders_value_name_and_added_with_unpriced_fallback() {
    use crate::root::public_account::list::sort_public_accounts;
    use wallet_ops::settings::PublicAccountSort;
    let mut accounts = vec![
        public_account_for_search_with_uuid("empty", Some("Alpha"), Address::ZERO),
        public_account_for_search_with_uuid("small", Some("beta"), Address::ZERO),
        public_account_for_search_with_uuid("large", Some("Beta"), Address::ZERO),
        public_account_for_search_with_uuid("unnamed", None, Address::ZERO),
        // Not read on the chain: the snapshot below has no row for it.
        public_account_for_search_with_uuid("unread", Some("Gamma"), Address::ZERO),
    ];
    for (order, account) in accounts.iter_mut().enumerate() {
        account.display_order = u32::try_from(order).unwrap();
    }
    let snapshot = public_native_balance_snapshot_for_test(
        1,
        accounts
            .iter()
            .cloned()
            .zip(
                [
                    U256::ZERO,
                    uint!(1_000_000_000_000_000_000_U256),
                    uint!(2_000_000_000_000_000_000_U256),
                    U256::from(1),
                ]
                .map(PublicBalanceAmount::Available),
            )
            .collect(),
    );
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    for (sort, pricing, expected) in [
        (
            PublicAccountSort::Value,
            true,
            ["large", "small", "unnamed", "empty", "unread"],
        ),
        (
            PublicAccountSort::Name,
            true,
            ["empty", "small", "large", "unread", "unnamed"],
        ),
        (
            PublicAccountSort::Value,
            false,
            ["empty", "small", "large", "unread", "unnamed"],
        ),
        (
            PublicAccountSort::Added,
            true,
            ["empty", "small", "large", "unnamed", "unread"],
        ),
    ] {
        sort_public_accounts(
            &mut accounts,
            sort,
            Some(&snapshot),
            1,
            pricing,
            Some(&cache),
        );
        assert_eq!(
            accounts
                .iter()
                .map(|account| account.public_account_uuid.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
#[allow(clippy::float_cmp)] // Whole-pixel geometry must be exact at the default scale.
fn public_tiles_reserve_overflow_and_follow_window_and_sidebar_width() {
    use crate::root::public_account::list::{balance_column_width, tiles_that_fit};
    assert_eq!(tiles_that_fit(0.0, 0), 0);
    assert_eq!(tiles_that_fit(0.0, 4), 1);
    assert_eq!(tiles_that_fit(162.0, 1), 1);
    assert_eq!(tiles_that_fit(224.0, 4), 1);
    assert_eq!(tiles_that_fit(560.0, 6), 3);
    assert_eq!(tiles_that_fit(666.0, 4), 4);
    assert_eq!(balance_column_width(1224.0, false, true, 1.0), 586.0);
    assert_eq!(balance_column_width(980.0, false, true, 1.0), 342.0);
    assert_eq!(balance_column_width(760.0, true, true, 1.0), 294.0);
    assert_eq!(
        tiles_that_fit(balance_column_width(760.0, true, true, 1.0), 6),
        1
    );
    assert!(
        balance_column_width(980.0, false, false, 1.0)
            > balance_column_width(980.0, false, true, 1.0)
    );
}
