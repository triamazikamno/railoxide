use super::*;
use crate::root::private_action::{
    RecipientSuggestionDirection, filtered_recipient_options,
    hardware_wallet_recipient_source_from_metadata, recipient_option_display_address,
    recipient_query_is_valid, recipient_suggestion_filter_query,
    recipient_suggestion_index_after_move,
};

#[test]
fn display_rows_use_known_token_metadata() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![utxo_output(
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            "1234567",
            false,
        )],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", true);
    assert_eq!(rows[0].token, "USDC");
    assert_eq!(rows[0].amount, "1.23");
    assert_eq!(rows[0].tree_position, "0/7");
    assert_eq!(rows[0].poi_status, "Valid");
    assert!(rows[0].poi_spendable);
    assert_eq!(rows[0].source_block_timestamp, 1_700_000_011);
    assert!(rows[0].token_icon_path.is_some());
    assert!(!rows[0].is_spent);
}

#[test]
fn display_rows_fall_back_for_unknown_token_metadata() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![utxo_output(
            "0x1111111111111111111111111111111111111111",
            "42",
            false,
        )],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", true);
    assert_eq!(rows[0].amount, "42");
    assert_eq!(
        rows[0].token_address,
        "0x1111111111111111111111111111111111111111"
    );
    assert_eq!(rows[0].token_icon_path, None);
}

#[test]
fn totals_format_known_token_amount() {
    let total = wallet_ops::TokenTotal {
        token: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(),
        total: "1234567".to_string(),
        poi_verified_total: "1234567".to_string(),
    };

    assert_eq!(format_total(1, &total), "USDC 1.23");
}

#[test]
fn form_error_formats_broadcaster_max_in_token_units() {
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::ZERO,
        label: "USDC".to_string(),
        decimals: Some(6),
        total: U256::ZERO,
        poi_verified_total: U256::ZERO,
        max_batched: U256::ZERO,
        icon_path: None,
    };

    let formatted = format_form_error_for_asset(
        "build public broadcaster send proof: public broadcaster max entered amount: 388585770",
        &asset,
        asset.token,
        None,
    );

    assert_eq!(
        formatted,
        "Max POI-verified entered amount for public broadcaster: 388.58577 USDC. Try a smaller amount or switch fee mode."
    );
}

#[test]
fn self_broadcast_software_gas_payers_require_password() {
    assert!(self_broadcast_requires_software_gas_payer_password(
        DeliveryMode::SelfBroadcast,
        Some(wallet_ops::vault::PublicAccountSource::Derived),
    ));
    assert!(self_broadcast_requires_software_gas_payer_password(
        DeliveryMode::SelfBroadcast,
        Some(wallet_ops::vault::PublicAccountSource::Imported),
    ));
    assert!(!self_broadcast_requires_software_gas_payer_password(
        DeliveryMode::SelfBroadcast,
        Some(wallet_ops::vault::PublicAccountSource::HardwareDerived),
    ));
    assert!(!self_broadcast_requires_software_gas_payer_password(
        DeliveryMode::PublicBroadcaster,
        Some(wallet_ops::vault::PublicAccountSource::Derived),
    ));
}

#[test]
fn report_chain_preserves_wrapped_public_broadcaster_error() {
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::ZERO,
        label: "USDC".to_string(),
        decimals: Some(6),
        total: U256::ZERO,
        poi_verified_total: U256::ZERO,
        max_batched: U256::ZERO,
        icon_path: None,
    };
    let error = eyre::eyre!("public broadcaster max entered amount: 388585770")
        .wrap_err("build public broadcaster send proof");

    let chain = format_report_chain(&error);

    assert_eq!(
        chain,
        "build public broadcaster send proof: public broadcaster max entered amount: 388585770"
    );
    assert_eq!(
        format_form_error_for_asset(chain.as_str(), &asset, asset.token, None),
        "Max POI-verified entered amount for public broadcaster: 388.58577 USDC. Try a smaller amount or switch fee mode."
    );
}

#[test]
fn form_error_formats_fee_token_balance_in_selected_fee_token_units() {
    let weth = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse::<Address>()
        .expect("weth address");
    let usdc = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        .parse::<Address>()
        .expect("usdc address");
    let asset = UnshieldAsset {
        chain_id: 1,
        token: weth,
        label: "WETH".to_string(),
        decimals: Some(18),
        total: U256::ZERO,
        poi_verified_total: U256::ZERO,
        max_batched: U256::ZERO,
        icon_path: None,
    };

    let formatted = format_form_error_for_asset(
        "build public broadcaster unshield proof: public broadcaster fee-token max spendable: 388585770; required fee: 400000000",
        &asset,
        usdc,
        None,
    );

    assert_eq!(
        formatted,
        "Transaction fee exceeds available fee-token balance. Required fee: 400 USDC; available: 388.58577 USDC; short by: 11.41423 USDC. Choose a fee token with more spendable balance or a lower-fee broadcaster."
    );
}

#[test]
fn form_error_formats_legacy_fee_token_balance_without_required_fee() {
    let usdc = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        .parse::<Address>()
        .expect("usdc address");
    let asset = UnshieldAsset {
        chain_id: 1,
        token: usdc,
        label: "USDC".to_string(),
        decimals: Some(6),
        total: U256::ZERO,
        poi_verified_total: U256::ZERO,
        max_batched: U256::ZERO,
        icon_path: None,
    };

    let formatted = format_form_error_for_asset(
        "build public broadcaster send proof: public broadcaster fee-token max spendable: 388585770",
        &asset,
        usdc,
        None,
    );

    assert_eq!(
        formatted,
        "Transaction fee exceeds available fee-token balance: 388.58577 USDC. Choose a fee token with more spendable balance or a lower-fee broadcaster."
    );
}

#[test]
fn public_broadcaster_amount_display_is_exact() {
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::ZERO,
        label: "USDC".to_string(),
        decimals: Some(6),
        total: U256::ZERO,
        poi_verified_total: U256::ZERO,
        max_batched: U256::ZERO,
        icon_path: None,
    };

    assert_eq!(
        format_exact_asset_amount_for_display(uint!(388_429_885_U256), &asset),
        "388.429885 USDC"
    );
    assert_eq!(
        format_exact_asset_amount_for_display(uint!(14_390_115_U256), &asset),
        "14.390115 USDC"
    );
}

#[test]
fn public_broadcaster_estimate_hides_duplicate_amount_rows() {
    let entered = uint!(388_429_885_U256);

    assert!(!should_show_distinct_amount(entered, entered));
    assert!(should_show_distinct_amount(
        entered,
        entered + uint!(1_U256)
    ));
}

#[test]
fn public_broadcaster_cost_preview_hides_on_form_error() {
    assert!(should_render_public_broadcaster_cost_preview(
        DeliveryMode::PublicBroadcaster,
        false,
        false,
    ));
    assert!(!should_render_public_broadcaster_cost_preview(
        DeliveryMode::PublicBroadcaster,
        false,
        true,
    ));
    assert!(!should_render_public_broadcaster_cost_preview(
        DeliveryMode::PublicBroadcaster,
        true,
        false,
    ));
    assert!(!should_render_public_broadcaster_cost_preview(
        DeliveryMode::ManualCalldata,
        false,
        false,
    ));
}

#[test]
fn spend_authorization_errors_preserve_estimate() {
    assert!(!form_error_clears_public_broadcaster_cost_estimate(
        DeliveryFormKind::Send,
        SEND_AUTHORIZATION_FAILED_ERROR,
    ));
    assert!(!form_error_clears_public_broadcaster_cost_estimate(
        DeliveryFormKind::Unshield,
        UNSHIELD_AUTHORIZATION_FAILED_ERROR,
    ));
    assert!(form_error_clears_public_broadcaster_cost_estimate(
        DeliveryFormKind::Send,
        "invalid recipient 0zk address",
    ));
    assert!(form_error_clears_public_broadcaster_cost_estimate(
        DeliveryFormKind::Unshield,
        "amount exceeds balance",
    ));
}

#[test]
fn spend_authorization_failure_detection_is_specific() {
    assert!(is_spend_authorization_failure_error(
        SEND_AUTHORIZATION_FAILED_ERROR,
    ));
    assert!(is_spend_authorization_failure_error(
        UNSHIELD_AUTHORIZATION_FAILED_ERROR,
    ));
    assert!(is_spend_authorization_failure_error(
        "authorize public account spend: unlock failed",
    ));
    assert!(!is_spend_authorization_failure_error(
        "Unlock failed. Check the password and try again.",
    ));
    assert!(!is_spend_authorization_failure_error(
        "invalid recipient 0zk address",
    ));
}

#[test]
fn remembered_spend_authorization_expires_by_lifetime() {
    assert!(!remembered_spend_authorization_valid_for_test(
        SpendAuthorizationLifetime::Once,
        Duration::ZERO,
    ));
    assert!(remembered_spend_authorization_valid_for_test(
        SpendAuthorizationLifetime::FiveMinutes,
        Duration::from_mins(1),
    ));
    assert!(!remembered_spend_authorization_valid_for_test(
        SpendAuthorizationLifetime::FiveMinutes,
        Duration::from_mins(5),
    ));
    assert!(remembered_spend_authorization_valid_for_test(
        SpendAuthorizationLifetime::UntilVaultLock,
        Duration::from_hours(24),
    ));
}

#[test]
fn reusable_spend_authorization_warning_matches_lifetime() {
    assert!(!SpendAuthorizationLifetime::Once.requires_reusable_authorization_warning());
    assert!(SpendAuthorizationLifetime::FiveMinutes.requires_reusable_authorization_warning());
    assert!(SpendAuthorizationLifetime::FifteenMinutes.requires_reusable_authorization_warning());
    assert!(SpendAuthorizationLifetime::UntilVaultLock.requires_reusable_authorization_warning());
}

#[test]
fn hardware_spend_authorization_copy_uses_concise_device_language() {
    let copy = crate::root::spend_authorization::hardware_spend_authorization_instruction("Ledger");

    assert!(copy.contains("Ledger"));
    assert!(copy.contains("Railgun derivation request"));
    assert!(!copy.contains("passphrase wallet"));
    assert!(!copy.contains("Remembered spend authorization"));
}

#[test]
fn spend_authorization_recipient_display_shortens_long_recipients() {
    let recipient = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";

    assert_eq!(
        crate::root::spend_authorization::spend_authorization_recipient_display(recipient),
        "0zk1qy4v...g5p6e9mp",
    );
    assert_eq!(
        crate::root::spend_authorization::spend_authorization_recipient_display("0x1234"),
        "0x1234",
    );
}

#[test]
fn public_broadcaster_estimate_validation_reports_invalid_send_recipient() {
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::ZERO,
        label: "DAI".to_string(),
        decimals: Some(18),
        total: uint!(10_000_000_000_000_000_000_U256),
        poi_verified_total: uint!(10_000_000_000_000_000_000_U256),
        max_batched: uint!(10_000_000_000_000_000_000_U256),
        icon_path: None,
    };

    let error = send_public_broadcaster_estimate_input_error("not-0zk", "1", &asset)
        .expect("invalid recipient should be reported");

    assert!(error.contains("invalid recipient 0zk address"));
}

#[test]
fn public_broadcaster_estimate_validation_reports_invalid_unshield_recipient() {
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::ZERO,
        label: "DAI".to_string(),
        decimals: Some(18),
        total: uint!(10_000_000_000_000_000_000_U256),
        poi_verified_total: uint!(10_000_000_000_000_000_000_U256),
        max_batched: uint!(10_000_000_000_000_000_000_U256),
        icon_path: None,
    };

    assert_eq!(
        unshield_public_broadcaster_estimate_input_error("not-0x", "1", &asset),
        Some("Enter a valid public EVM recipient address".to_string())
    );
}

#[test]
fn public_broadcaster_estimate_validation_allows_empty_recipient_prompt_state() {
    let asset = UnshieldAsset {
        chain_id: 1,
        token: Address::ZERO,
        label: "DAI".to_string(),
        decimals: Some(18),
        total: uint!(10_000_000_000_000_000_000_U256),
        poi_verified_total: uint!(10_000_000_000_000_000_000_U256),
        max_batched: uint!(10_000_000_000_000_000_000_U256),
        icon_path: None,
    };

    assert_eq!(
        send_public_broadcaster_estimate_input_error("", "1", &asset),
        None
    );
    assert_eq!(
        unshield_public_broadcaster_estimate_input_error("", "1", &asset),
        None
    );
}

#[test]
fn recipient_options_filter_unavailable_app_accounts_and_include_address_books() {
    let private_wallets = vec![
        PrivateWalletRecipientSource {
            label: Arc::from("Active private"),
            address: Arc::from("0zk1active"),
            active: true,
        },
        PrivateWalletRecipientSource {
            label: Arc::from("Inactive private"),
            address: Arc::from("0zk1inactive"),
            active: false,
        },
    ];
    let private_address_book = vec![PrivateAddressBookEntry {
        entry_uuid: "private-book".to_string(),
        label: "Saved private".to_string(),
        address: "0zk1saved".to_string(),
        display_order: 0,
    }];

    let private_options = private_send_recipient_options(&private_wallets, &private_address_book);

    assert_eq!(private_options.len(), 2);
    assert!(private_options.iter().any(|option| {
        option.label.as_ref() == "Active private"
            && option.source == RecipientOptionSource::PrivateWallet
    }));
    assert!(private_options.iter().any(|option| {
        option.label.as_ref() == "Saved private"
            && option.source == RecipientOptionSource::PrivateAddressBook
    }));
    assert!(
        private_options
            .iter()
            .all(|option| option.label.as_ref() != "Inactive private")
    );

    let mut public_accounts = vec![
        public_account_metadata(
            "active-public",
            "0x1111111111111111111111111111111111111111",
            Some("Active public"),
            wallet_ops::vault::PublicAccountStatus::Active,
        ),
        public_account_metadata(
            "inactive-public",
            "0x2222222222222222222222222222222222222222",
            Some("Inactive public"),
            wallet_ops::vault::PublicAccountStatus::Inactive,
        ),
    ];
    let stealth_accounts = [
        (1, "0x4444444444444444444444444444444444444444"),
        (137, "0x5555555555555555555555555555555555555555"),
    ];
    for (chain_id, address) in stealth_accounts {
        let mut account = public_account_metadata(
            address,
            address,
            Some("Stealth account 0"),
            wallet_ops::vault::PublicAccountStatus::Active,
        );
        account.source = serde_json::from_value(serde_json::json!({
            "ExecutorDerived": {
                "chain_id": chain_id,
                "index": 0,
                "operation": wallet_ops::vault::ExecutorOperationId::random().expect("operation"),
            }
        }))
        .expect("stealth account source");
        account.scope = wallet_ops::vault::PublicAccountScope::PrivateWallet {
            wallet_uuid: "wallet".to_string(),
        };
        public_accounts.push(account);
    }
    let public_address_book = vec![PublicAddressBookEntry {
        entry_uuid: "public-book".to_string(),
        label: "Saved public".to_string(),
        address: "0x3333333333333333333333333333333333333333"
            .parse()
            .expect("address"),
        display_order: 0,
    }];

    for (chain_id, stealth_address) in stealth_accounts {
        let public_options =
            private_unshield_recipient_options(&public_accounts, &public_address_book, chain_id);

        assert_eq!(public_options.len(), 3);
        assert!(public_options.iter().any(|option| {
            option.label.as_ref() == "Active public"
                && option.source == RecipientOptionSource::PublicAccount
        }));
        assert!(public_options.iter().any(|option| {
            option.label.as_ref() == "Saved public"
                && option.source == RecipientOptionSource::PublicAddressBook
        }));
        assert!(public_options.iter().any(|option| {
            option.address.as_ref() == stealth_address
                && option.source == RecipientOptionSource::PublicAccount
        }));
        assert!(
            public_options
                .iter()
                .all(|option| option.label.as_ref() != "Inactive public")
        );
    }
}

#[test]
fn hardware_wallet_recipient_source_uses_stored_receive_address() {
    use wallet_ops::hardware::{
        HardwareDerivationDescriptor, HardwareWalletSyncIntent, parse_bip32_path,
    };
    use wallet_ops::vault::{HardwareProfileMetadata, HardwareRailgunAccountMetadata};

    const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";

    let descriptor = HardwareDerivationDescriptor::ledger_eip1024_v1(
        parse_bip32_path("m/44'/60'/0'/0/0").expect("valid path"),
        0,
        "ledger-profile".to_string(),
        HardwareWalletSyncIntent::CreateNew,
    );
    let profile = HardwareProfileMetadata::from_descriptor(&descriptor);
    let mut metadata = wallet_metadata(
        "hardware-wallet",
        "Ledger hardware",
        WalletSource::LedgerDerived,
        WalletStatus::Active,
        0,
    );
    metadata.hardware_account = Some(
        HardwareRailgunAccountMetadata::synthetic_software_v1(
            profile.profile_id,
            0,
            "Ledger hardware",
            descriptor,
            wallet_ops::vault::HardwareRailgunAccountIdentity {
                spending_public_key: [[0; 32]; 2],
                viewing_public_key: [0; 32],
            },
        )
        .with_receive_address(RAILGUN_ADDRESS),
    );

    let source = hardware_wallet_recipient_source_from_metadata(&metadata)
        .expect("hardware recipient source");
    let options = private_send_recipient_options(std::slice::from_ref(&source), &[]);

    assert_eq!(source.label.as_ref(), "Ledger hardware");
    assert_eq!(source.address.as_ref(), RAILGUN_ADDRESS);
    assert!(source.active);
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].source, RecipientOptionSource::PrivateWallet);

    let mut inactive_metadata = metadata.clone();
    inactive_metadata.status = WalletStatus::Inactive;
    let inactive_source = hardware_wallet_recipient_source_from_metadata(&inactive_metadata)
        .expect("inactive hardware recipient source");
    assert!(private_send_recipient_options(std::slice::from_ref(&inactive_source), &[]).is_empty());

    let mut missing_address_metadata = metadata;
    missing_address_metadata
        .hardware_account
        .as_mut()
        .expect("hardware account")
        .receive_address = None;
    assert!(hardware_wallet_recipient_source_from_metadata(&missing_address_metadata).is_none());
}

#[test]
fn recipient_option_search_matches_label_and_address_case_insensitively() {
    let option = RecipientOption {
        label: Arc::from("Alice Cold Wallet"),
        address: Arc::from("0xAbCdEf0000000000000000000000000000000000"),
        source: RecipientOptionSource::PublicAddressBook,
    };

    assert!(recipient_option_matches_search(&option, "cold"));
    assert!(recipient_option_matches_search(&option, "COLD"));
    assert!(recipient_option_matches_search(&option, "abcdef"));
    assert!(!recipient_option_matches_search(&option, "bob"));
}

#[test]
fn recipient_combobox_helpers_filter_validate_and_move_selection() {
    const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";
    let options = vec![
        RecipientOption {
            label: Arc::from("Alice Cold Wallet"),
            address: Arc::from("0x1111111111111111111111111111111111111111"),
            source: RecipientOptionSource::PublicAddressBook,
        },
        RecipientOption {
            label: Arc::from("Alice Hot Wallet"),
            address: Arc::from("0x2222222222222222222222222222222222222222"),
            source: RecipientOptionSource::PublicAccount,
        },
        RecipientOption {
            label: Arc::from("Bob"),
            address: Arc::from("0x3333333333333333333333333333333333333333"),
            source: RecipientOptionSource::PublicAddressBook,
        },
    ];

    assert_eq!(
        filtered_recipient_options(&options, "alice")
            .iter()
            .map(|option| option.address.as_ref())
            .collect::<Vec<_>>(),
        vec![
            "0x1111111111111111111111111111111111111111",
            "0x2222222222222222222222222222222222222222"
        ]
    );
    assert!(filtered_recipient_options(&options, "carol").is_empty());
    assert!(recipient_query_is_valid(
        DeliveryFormKind::Unshield,
        "0x1111111111111111111111111111111111111111"
    ));
    assert!(!recipient_query_is_valid(
        DeliveryFormKind::Unshield,
        "alice"
    ));
    assert_eq!(
        recipient_suggestion_filter_query(
            DeliveryFormKind::Unshield,
            "0x1111111111111111111111111111111111111111"
        ),
        ""
    );
    assert_eq!(
        recipient_suggestion_filter_query(DeliveryFormKind::Unshield, "alice"),
        "alice"
    );
    assert_eq!(
        filtered_recipient_options(
            &options,
            &recipient_suggestion_filter_query(
                DeliveryFormKind::Unshield,
                "0x1111111111111111111111111111111111111111"
            )
        )
        .len(),
        3
    );
    assert_eq!(
        recipient_suggestion_filter_query(DeliveryFormKind::Send, RAILGUN_ADDRESS),
        ""
    );
    assert_eq!(
        recipient_suggestion_index_after_move(None, 3, RecipientSuggestionDirection::Next),
        Some(0)
    );
    assert_eq!(
        recipient_suggestion_index_after_move(Some(2), 3, RecipientSuggestionDirection::Next),
        Some(0)
    );
    assert_eq!(
        recipient_suggestion_index_after_move(None, 3, RecipientSuggestionDirection::Previous),
        Some(2)
    );
    assert_eq!(
        recipient_suggestion_index_after_move(Some(0), 3, RecipientSuggestionDirection::Previous),
        Some(2)
    );
}

#[test]
fn recipient_option_display_shortens_private_addresses_but_searches_full_address() {
    const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";
    let private_option = RecipientOption {
        label: Arc::from("Alice Private"),
        address: Arc::from(RAILGUN_ADDRESS),
        source: RecipientOptionSource::PrivateAddressBook,
    };
    let public_option = RecipientOption {
        label: Arc::from("Alice Public"),
        address: Arc::from("0x1111111111111111111111111111111111111111"),
        source: RecipientOptionSource::PublicAddressBook,
    };

    assert_eq!(
        recipient_option_display_address(&private_option),
        "0zk1qy4v...p6e9mp"
    );
    assert_eq!(
        recipient_option_display_address(&public_option),
        "0x1111111111111111111111111111111111111111"
    );
    assert!(recipient_option_matches_search(
        &private_option,
        "rv7j6fe3z53"
    ));
}

#[test]
fn recipient_save_visibility_requires_valid_unmatched_addresses() {
    const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";
    let private_options = vec![RecipientOption {
        label: Arc::from("Existing private"),
        address: Arc::from(RAILGUN_ADDRESS),
        source: RecipientOptionSource::PrivateAddressBook,
    }];

    assert!(can_save_private_recipient(RAILGUN_ADDRESS, &[]));
    assert!(!can_save_private_recipient("not-0zk", &[]));
    assert!(!can_save_private_recipient(
        RAILGUN_ADDRESS,
        &private_options
    ));

    let public_options = vec![RecipientOption {
        label: Arc::from("Existing public"),
        address: Arc::from("0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa"),
        source: RecipientOptionSource::PublicAddressBook,
    }];

    assert!(can_save_public_recipient(
        "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        &public_options,
    ));
    assert!(!can_save_public_recipient("not-0x", &public_options));
    assert!(!can_save_public_recipient(
        "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        &public_options,
    ));
}

#[test]
fn recipient_selection_and_save_label_helpers_preserve_expected_values() {
    let option = RecipientOption {
        label: Arc::from("Saved recipient"),
        address: Arc::from("0x4444444444444444444444444444444444444444"),
        source: RecipientOptionSource::PublicAddressBook,
    };

    assert_eq!(
        selected_recipient_address(&option),
        "0x4444444444444444444444444444444444444444"
    );
    assert_eq!(
        normalized_address_book_save_label("  Alice  ").as_deref(),
        Some("Alice")
    );
    assert_eq!(normalized_address_book_save_label("   "), None);
}

#[test]
fn public_broadcaster_cost_status_separates_pending_from_estimating() {
    assert_eq!(public_broadcaster_cost_status(true, false), None);
    assert_eq!(
        public_broadcaster_cost_status(false, true),
        Some(CostEstimateStatus::Estimating)
    );
    assert_eq!(public_broadcaster_cost_status(true, true), None);
    assert_eq!(public_broadcaster_cost_status(false, false), None);
    assert_eq!(
        public_broadcaster_cost_status_text(CostEstimateStatus::Estimating).0,
        "Estimating public broadcaster cost..."
    );
}

fn public_account_metadata(
    public_account_uuid: &str,
    address: &str,
    label: Option<&str>,
    status: wallet_ops::vault::PublicAccountStatus,
) -> PublicAccountMetadata {
    PublicAccountMetadata {
        public_account_uuid: public_account_uuid.to_string(),
        address: address.parse().expect("public account address"),
        label: label.map(str::to_owned),
        source: wallet_ops::vault::PublicAccountSource::Imported,
        scope: wallet_ops::vault::PublicAccountScope::Global,
        derivation_index: None,
        hardware_descriptor: None,
        status,
        display_order: 0,
    }
}
