use super::*;

#[test]
fn public_account_selection_restores_per_wallet_and_falls_back_when_unavailable() {
    use crate::root::public_account::restored_public_account_selection;

    let mut accounts: Vec<_> = ["first", "second", "third"]
        .into_iter()
        .map(|uuid| public_account_for_search_with_uuid(uuid, None, Address::ZERO))
        .collect();
    let ui_state = WalletUiState {
        last_public_accounts: std::collections::BTreeMap::from([
            ("wallet-a".to_owned(), "second".to_owned()),
            ("wallet-b".to_owned(), "third".to_owned()),
        ]),
        ..WalletUiState::default()
    };
    // Lock and wallet replacement clear the runtime selection. Each unlocked wallet
    // must recover its own saved choice even when all public accounts are global.
    assert_eq!(
        restored_public_account_selection(&accounts, None, &ui_state, "wallet-a").as_deref(),
        Some("second")
    );
    assert_eq!(
        restored_public_account_selection(&accounts, None, &ui_state, "wallet-b").as_deref(),
        Some("third")
    );
    assert_eq!(
        restored_public_account_selection(&accounts, None, &ui_state, "new-wallet").as_deref(),
        Some("first")
    );
    // A live selection takes precedence during a reload or transport reconnect.
    assert_eq!(
        restored_public_account_selection(&accounts, Some("third"), &ui_state, "wallet-a")
            .as_deref(),
        Some("third")
    );

    accounts[1].status = PublicAccountStatus::Inactive;
    assert_eq!(
        restored_public_account_selection(&accounts, None, &ui_state, "wallet-a").as_deref(),
        Some("first")
    );
    assert_eq!(
        restored_public_account_selection(&accounts, Some("second"), &ui_state, "wallet-a")
            .as_deref(),
        Some("second")
    );
    accounts[1].status = PublicAccountStatus::Active;
    accounts[1].scope = PublicAccountScope::PrivateWallet {
        wallet_uuid: "wallet-b".to_owned(),
    };
    assert_eq!(
        restored_public_account_selection(&accounts, None, &ui_state, "wallet-a").as_deref(),
        Some("first")
    );
    accounts.remove(1);
    assert_eq!(
        restored_public_account_selection(&accounts, None, &ui_state, "wallet-a").as_deref(),
        Some("first")
    );
    for account in &mut accounts {
        account.status = PublicAccountStatus::Inactive;
    }
    assert!(restored_public_account_selection(&accounts, None, &ui_state, "wallet-a").is_none());
}

#[test]
fn public_account_search_matches_empty_query() {
    let account = public_account_for_search(Some("Main account"), Address::from([0x11; 20]));

    assert!(public_account_matches_search(&account, ""));
    assert!(public_account_matches_search(&account, "   "));
}

#[test]
fn public_account_search_matches_label_partial_case_insensitive() {
    let account = public_account_for_search(Some("Primary Spending"), Address::from([0x22; 20]));

    assert!(public_account_matches_search(&account, "spend"));
    assert!(public_account_matches_search(&account, "PRIMARY"));
}

#[test]
fn public_account_search_matches_address_partial_case_insensitive() {
    let account = public_account_for_search(None, Address::from([0xab; 20]));

    assert!(public_account_matches_search(&account, "0xabab"));
    assert!(public_account_matches_search(&account, "ABABAB"));
}

#[test]
fn public_account_search_rejects_non_matches() {
    let account = public_account_for_search(Some("Primary"), Address::from([0xcd; 20]));

    assert!(!public_account_matches_search(&account, "savings"));
}

#[test]
fn self_broadcast_gas_payer_defaulting_requires_explicit_multiple_choice() {
    let first =
        public_account_for_search_with_uuid("public-1", Some("Main"), Address::from([0x11; 20]));
    let second =
        public_account_for_search_with_uuid("public-2", Some("Backup"), Address::from([0x22; 20]));

    assert_eq!(default_self_broadcast_gas_payer_uuid(&[]), None);
    assert_eq!(
        default_self_broadcast_gas_payer_uuid(std::slice::from_ref(&first)).as_deref(),
        Some("public-1")
    );
    assert_eq!(
        default_self_broadcast_gas_payer_uuid(&[first, second]),
        None
    );
}

#[test]
fn self_broadcast_gas_payer_search_matches_label_and_addresses() {
    let account = public_account_for_search_with_uuid(
        "public-1",
        Some("Private gas payer"),
        Address::from([0xab; 20]),
    );

    assert!(self_broadcast_gas_payer_matches_search(&account, "gas"));
    assert!(self_broadcast_gas_payer_matches_search(&account, "0xabab"));
    assert!(self_broadcast_gas_payer_matches_search(&account, "ABAB"));
    assert!(self_broadcast_gas_payer_matches_search(
        &account,
        &railgun_ui::short_address(&account.address),
    ));
    assert!(!self_broadcast_gas_payer_matches_search(
        &account, "savings"
    ));
}

#[test]
fn walletconnect_account_selector_defaulting_preserves_valid_selection() {
    let first =
        public_account_for_search_with_uuid("public-1", Some("Main"), Address::from([0x11; 20]));
    let second =
        public_account_for_search_with_uuid("public-2", Some("Backup"), Address::from([0x22; 20]));
    let selected = Arc::<str>::from("public-2");

    assert_eq!(normalized_walletconnect_account_uuid(None, &[]), None);
    assert_eq!(
        normalized_walletconnect_account_uuid(None, &[first.clone(), second.clone()]).as_deref(),
        Some("public-1")
    );
    assert_eq!(
        normalized_walletconnect_account_uuid(Some(&selected), &[first, second]).as_deref(),
        Some("public-2")
    );
}

#[test]
fn walletconnect_account_selector_replaces_invalid_selection() {
    let first =
        public_account_for_search_with_uuid("public-1", Some("Main"), Address::from([0x11; 20]));
    let selected = Arc::<str>::from("missing-public");

    assert_eq!(
        normalized_walletconnect_account_uuid(Some(&selected), std::slice::from_ref(&first))
            .as_deref(),
        Some("public-1")
    );
}

#[test]
fn walletconnect_account_selector_matches_label_address_and_uuid() {
    let account = public_account_for_search_with_uuid(
        "public-walletconnect-1",
        Some("DeFi spending"),
        Address::from([0xab; 20]),
    );
    let items = walletconnect_account_select_items(&[account], None, 1, None);
    let item = &items[0];

    assert!(walletconnect_account_matches_search(item, "defi"));
    assert!(walletconnect_account_matches_search(item, "0xabab"));
    assert!(walletconnect_account_matches_search(
        item,
        "walletconnect-1"
    ));
    assert!(!walletconnect_account_matches_search(item, "imported"));
    assert!(item.matches("ABAB"));
    assert!(!walletconnect_account_matches_search(item, "savings"));
}

#[test]
fn random_self_broadcast_gas_payer_returns_eligible_account_uuid() {
    let first =
        public_account_for_search_with_uuid("public-1", Some("Main"), Address::from([0x11; 20]));
    let second =
        public_account_for_search_with_uuid("public-2", Some("Backup"), Address::from([0x22; 20]));

    assert_eq!(
        random_self_broadcast_gas_payer_uuid(&[], None, 1, None),
        None
    );
    assert_eq!(
        random_self_broadcast_gas_payer_uuid(
            std::slice::from_ref(&first),
            Some("public-1"),
            1,
            None
        ),
        None
    );
    assert_eq!(
        random_self_broadcast_gas_payer_uuid(std::slice::from_ref(&first), None, 1, None)
            .as_deref(),
        Some("public-1")
    );
    assert_eq!(
        random_self_broadcast_gas_payer_uuid(
            &[first.clone(), second.clone()],
            Some("public-1"),
            1,
            None
        )
        .as_deref(),
        Some("public-2")
    );
    let selected = random_self_broadcast_gas_payer_uuid(&[first, second], None, 1, None)
        .expect("random account selected");
    assert!(matches!(selected.as_ref(), "public-1" | "public-2"));
}

#[test]
fn random_self_broadcast_gas_payer_skips_known_zero_native_balance() {
    let first =
        public_account_for_search_with_uuid("public-1", Some("Empty"), Address::from([0x11; 20]));
    let second =
        public_account_for_search_with_uuid("public-2", Some("Funded"), Address::from([0x22; 20]));
    let snapshot = public_native_balance_snapshot_for_test(
        1,
        vec![
            (first.clone(), PublicBalanceAmount::Available(U256::ZERO)),
            (
                second.clone(),
                PublicBalanceAmount::Available(U256::from(5_u64)),
            ),
        ],
    );

    assert_eq!(
        random_self_broadcast_gas_payer_uuid(
            &[first.clone(), second.clone()],
            None,
            1,
            Some(&snapshot)
        )
        .as_deref(),
        Some("public-2")
    );
    assert_eq!(
        random_self_broadcast_gas_payer_uuid(
            &[first.clone(), second],
            Some("public-2"),
            1,
            Some(&snapshot)
        ),
        None
    );
    assert_eq!(
        random_self_broadcast_gas_payer_uuid(
            std::slice::from_ref(&first),
            None,
            56,
            Some(&snapshot)
        )
        .as_deref(),
        Some("public-1")
    );
}

#[test]
fn self_broadcast_native_balance_label_separates_not_fetched_failed_and_amount() {
    let funded =
        public_account_for_search_with_uuid("public-1", Some("Funded"), Address::from([0x11; 20]));
    let failed =
        public_account_for_search_with_uuid("public-2", Some("Failed"), Address::from([0x22; 20]));
    let snapshot = public_native_balance_snapshot_for_test(
        1,
        vec![
            (funded, PublicBalanceAmount::Available(U256::from(5_u64))),
            (failed, PublicBalanceAmount::Unavailable),
        ],
    );
    let amount =
        public_balance_amount_label(&PublicBalanceAmount::Available(U256::from(5_u64)), 18);

    assert_eq!(
        self_broadcast_native_balance_label(Some(&snapshot), 1, "public-1"),
        format!("{amount} ETH")
    );
    assert_eq!(
        self_broadcast_native_balance_label(Some(&snapshot), 1, "public-2"),
        "unavailable"
    );
    assert_eq!(
        self_broadcast_native_balance_label(Some(&snapshot), 56, "public-1"),
        "Not fetched"
    );
    assert_eq!(
        self_broadcast_native_balance_label(Some(&snapshot), 1, "missing"),
        "Not fetched"
    );
}

#[test]
fn self_broadcast_native_balance_state_distinguishes_zero_positive_and_unknown() {
    let empty =
        public_account_for_search_with_uuid("public-1", Some("Empty"), Address::from([0x11; 20]));
    let funded =
        public_account_for_search_with_uuid("public-2", Some("Funded"), Address::from([0x22; 20]));
    let unavailable = public_account_for_search_with_uuid(
        "public-3",
        Some("Unavailable"),
        Address::from([0x33; 20]),
    );
    let snapshot = public_native_balance_snapshot_for_test(
        1,
        vec![
            (empty, PublicBalanceAmount::Available(U256::ZERO)),
            (funded, PublicBalanceAmount::Available(U256::from(5_u64))),
            (unavailable, PublicBalanceAmount::Unavailable),
        ],
    );

    assert_eq!(
        self_broadcast_native_balance_state(Some(&snapshot), 1, "public-1"),
        SelfBroadcastNativeBalanceState::Zero
    );
    assert_eq!(
        self_broadcast_native_balance_state(Some(&snapshot), 1, "public-2"),
        SelfBroadcastNativeBalanceState::Positive
    );
    assert_eq!(
        self_broadcast_native_balance_state(Some(&snapshot), 1, "public-3"),
        SelfBroadcastNativeBalanceState::Unknown
    );
    assert_eq!(
        self_broadcast_native_balance_state(Some(&snapshot), 56, "public-1"),
        SelfBroadcastNativeBalanceState::Unknown
    );
    assert_eq!(
        self_broadcast_native_balance_state(Some(&snapshot), 1, "missing"),
        SelfBroadcastNativeBalanceState::Unknown
    );
}
