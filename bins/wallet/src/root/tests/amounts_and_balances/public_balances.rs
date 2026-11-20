use super::*;

#[test]
fn public_balance_helpers_ignore_stale_chain_snapshot() {
    let snapshot = public_balance_snapshot_for_test(1);

    assert_eq!(
        public_account_visible_balances_for_chain(
            Some(&snapshot),
            1,
            "public-account",
            PublicAccountStatus::Active,
        )
        .len(),
        1,
    );
    assert!(
        public_balance_entry_for_chain(
            Some(&snapshot),
            1,
            "public-account",
            PublicAssetId::Native,
            PublicAccountStatus::Active,
        )
        .is_some(),
    );
    assert_eq!(
        public_account_visible_balances_for_chain(
            Some(&snapshot),
            56,
            "public-account",
            PublicAccountStatus::Active,
        ),
        [] as [PublicBalanceEntry; 0],
    );
    assert!(
        public_balance_entry_for_chain(
            Some(&snapshot),
            56,
            "public-account",
            PublicAssetId::Native,
            PublicAccountStatus::Active,
        )
        .is_none(),
    );
    assert_eq!(
        public_account_visible_balances_for_chain(
            Some(&snapshot),
            1,
            "public-account",
            PublicAccountStatus::Inactive,
        ),
        [] as [PublicBalanceEntry; 0],
    );
}

#[test]
fn public_balance_usd_label_prices_native_and_erc20_balances() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));

    assert_eq!(
        public_balance_usd_label(
            1,
            PublicAssetId::Native,
            &PublicBalanceAmount::Available(uint!(2_000_000_000_000_000_000_U256)),
            Some(&cache),
        )
        .as_deref(),
        Some("$6,000.00")
    );
    assert_eq!(
        public_balance_usd_label(
            1,
            PublicAssetId::Erc20(usdc),
            &PublicBalanceAmount::Available(uint!(1_234_567_U256)),
            Some(&cache),
        )
        .as_deref(),
        Some("$1.23")
    );
}

#[test]
fn public_balance_usd_label_omits_unpriced_and_unavailable_balances() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let unknown = Address::from([0x77; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);

    assert_eq!(
        public_balance_usd_label(
            1,
            PublicAssetId::Erc20(usdc),
            &PublicBalanceAmount::Available(uint!(1_234_567_U256)),
            Some(&cache),
        ),
        None
    );
    assert_eq!(
        public_balance_usd_label(
            1,
            PublicAssetId::Erc20(unknown),
            &PublicBalanceAmount::Available(uint!(1_234_567_U256)),
            Some(&cache),
        ),
        None
    );
    assert_eq!(
        public_balance_usd_label(
            1,
            PublicAssetId::Native,
            &PublicBalanceAmount::Unavailable,
            Some(&cache),
        ),
        None
    );
    assert_eq!(
        public_balance_usd_label(
            56,
            PublicAssetId::Native,
            &PublicBalanceAmount::Available(uint!(1_000_000_000_000_000_000_U256)),
            Some(&cache),
        ),
        None
    );
}

#[test]
fn public_account_usd_total_label_sums_priced_balances() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let account = public_account_for_search(Some("Main account"), Address::from([0x11; 20]));
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));
    let snapshot = PublicBalanceSnapshot {
        chain_id: 1,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: vec![PublicAccountBalance {
            observed_at: None,
            observed_block: None,
            account,
            balances: vec![
                PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Native,
                        symbol: "ETH".to_string(),
                        decimals: 18,
                    },
                    amount: PublicBalanceAmount::Available(uint!(2_000_000_000_000_000_000_U256)),
                },
                PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Erc20(usdc),
                        symbol: "USDC".to_string(),
                        decimals: 6,
                    },
                    amount: PublicBalanceAmount::Available(uint!(1_234_567_U256)),
                },
            ],
        }],
    };

    assert_eq!(
        public_account_usd_total_label_for_chain(
            Some(&snapshot),
            1,
            "public-account",
            PublicAccountStatus::Active,
            Some(&cache),
        )
        .as_deref(),
        Some("$6,001.23")
    );
}

#[test]
fn public_account_usd_total_label_omits_unpriced_and_unavailable_balances() {
    let account = public_account_for_search(Some("Main account"), Address::from([0x11; 20]));
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    let snapshot = PublicBalanceSnapshot {
        chain_id: 1,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: vec![PublicAccountBalance {
            observed_at: None,
            observed_block: None,
            account,
            balances: vec![
                PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Native,
                        symbol: "ETH".to_string(),
                        decimals: 18,
                    },
                    amount: PublicBalanceAmount::Unavailable,
                },
                PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Erc20(Address::from([0x77; 20])),
                        symbol: "UNKNOWN".to_string(),
                        decimals: 18,
                    },
                    amount: PublicBalanceAmount::Available(uint!(1_234_567_U256)),
                },
            ],
        }],
    };

    assert_eq!(
        public_account_usd_total_label_for_chain(
            Some(&snapshot),
            1,
            "public-account",
            PublicAccountStatus::Active,
            Some(&cache),
        ),
        None
    );
}
