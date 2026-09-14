use super::*;

#[test]
fn effective_token_registry_formats_private_and_public_assets() {
    let token = Address::from([0x88; 20]);
    let icon = "/tmp/custom-token.png";
    let mut settings = WalletSettings::default();
    settings.tokens.custom_tokens.push(CustomTokenSettings {
        chain_id: 1,
        token_address: token.to_string(),
        symbol: "TST".to_string(),
        decimals: 4,
        icon_path: Some(icon.to_string()),
        price_anchor: None,
    });
    let registry = build_effective_token_registry(&settings).expect("effective registry");
    let totals = [wallet_ops::TokenTotal {
        token: token.to_checksum(None),
        total: "12345".to_string(),
        poi_verified_total: "12345".to_string(),
    }];

    let rows = format_private_asset_rows(1, &totals, Some(&registry), None);

    assert_eq!(rows[0].label, "TST");
    assert_eq!(rows[0].amount, "1.23");
    assert_eq!(rows[0].decimals, Some(4));
    let browser = crate::root::gateway_private_view::private_asset_presentation(&rows[0]);
    assert_eq!(browser.amount, "1.23");
    assert!(browser.usd.is_none());
    assert!(browser.icon.is_none());
    assert!(browser.pending_verification.is_none());
    assert!(!serde_json::to_string(&browser).unwrap().contains(icon));
    assert_eq!(
        rows[0]
            .icon_path
            .as_ref()
            .and_then(|path| path.as_file_path()),
        Some(std::path::Path::new(icon))
    );
    assert_eq!(
        public_asset_label(1, PublicAssetId::Erc20(token), Some(&registry)),
        "TST"
    );
    assert_eq!(
        public_asset_decimals(1, PublicAssetId::Erc20(token), Some(&registry)),
        Some(4)
    );
    assert_eq!(
        public_asset_icon_path(1, PublicAssetId::Erc20(token), Some(&registry))
            .as_ref()
            .and_then(|path| path.as_file_path()),
        Some(std::path::Path::new(icon))
    );
}

#[test]
fn private_asset_rows_use_totals_formatting() {
    let totals = [wallet_ops::TokenTotal {
        token: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(),
        total: "1234567".to_string(),
        poi_verified_total: "1000000".to_string(),
    }];

    let rows = format_private_asset_rows(1, &totals, None, None);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "USDC");
    assert_eq!(rows[0].amount, "1.23");
    assert_eq!(rows[0].pending_poi_amount, "0.23457");
    assert_eq!(rows[0].pending_poi_total, Some(uint!(234_567_U256)));
    assert!(should_show_pending_poi_amount(rows[0].pending_poi_total));
    assert!(rows[0].icon_path.is_some());
    let browser = crate::root::gateway_private_view::private_asset_presentation(&rows[0]);
    assert_eq!(browser.pending_verification.as_deref(), Some("0.23457"));
    assert!(browser.pending_incoming.is_none());
    assert!(browser.pending_outgoing.is_none());
    assert!(browser.icon.as_ref().unwrap().starts_with("railgun-ui/"));
}

#[test]
fn pending_shield_waits_group_by_token_and_use_latest_source() {
    let token = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let mut first = utxo_output(&token.to_checksum(None), "1000000", false);
    first.commitment_kind = "Shield".to_string();
    first.activity_classification = "Shield".to_string();
    first.ppoi_state = UtxoPpoiState::Missing;
    first.poi_spendable = false;
    first.source_block_timestamp = 1_000;
    let mut latest = first.clone();
    latest.position = 8;
    latest.source_block_timestamp = 4_500;
    let mut private_output = first.clone();
    private_output.position = 9;
    private_output.commitment_kind = "Transact".to_string();
    private_output.activity_classification = "Private Output".to_string();
    private_output.source_block_timestamp = 1_300;
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 3,
        unspent_count: 3,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![first, latest, private_output],
        totals: Vec::new(),
    };

    let waits = pending_shield_waits_by_token(&snapshot, 5_000);
    let wait = waits.get(&token).expect("USDC Shield wait");

    assert_eq!(wait.output_count, 2);
    assert_eq!(wait.latest_source_block_timestamp, 4_500);
    assert_eq!(wait.total_value, Some(uint!(2_000_000_U256)));
    assert!(wait.has_delayed);
    assert!(pending_shield_wait_matches_total(
        *wait,
        Some(uint!(2_000_000_U256))
    ));
    assert!(!pending_shield_wait_matches_total(
        *wait,
        Some(uint!(3_000_000_U256))
    ));
    let mut receive_snapshot = snapshot;
    receive_snapshot.utxos.truncate(2);
    receive_snapshot.utxo_count = 2;
    receive_snapshot.unspent_count = 2;
    for output in &mut receive_snapshot.utxos {
        output.source_block_timestamp = crate::root::utxo::now_epoch_secs().saturating_sub(90);
    }
    receive_snapshot.totals = vec![wallet_ops::TokenTotal {
        token: token.to_checksum(None),
        total: "2000000".into(),
        poi_verified_total: "0".into(),
    }];
    let assets = format_private_asset_rows_from_snapshot(&receive_snapshot, None, None);
    let summary =
        private_pending_summary(&assets, &receive_snapshot, Vec::new(), false, None).unwrap();
    let presentation = crate::root::private_assets::private_pending_presentation(&summary);
    assert_eq!(presentation.categories.len(), 1);
    assert_eq!(presentation.categories[0].title, "Not yet spendable");
    assert!(presentation.categories[0].assets[0].shield_wait.is_some());
}

#[test]
fn private_action_tooltips_distinguish_syncing_and_ready() {
    assert_eq!(
        private_send_action_tooltip(true, true, true, "No spendable private balance"),
        "Open private send form while wallet sync finishes"
    );
    assert_eq!(
        private_send_action_tooltip(true, true, false, "No spendable private balance"),
        "Prepare private send calldata"
    );
    assert_eq!(
        private_unshield_action_tooltip(true, true, true, "No unshieldable private balance"),
        "Open unshield form while wallet sync finishes"
    );
    assert_eq!(
        private_unshield_action_tooltip(false, true, false, "No unshieldable private balance"),
        "No unshieldable private balance"
    );
    assert_eq!(
        private_send_action_tooltip(false, false, false, "No spendable private balance"),
        "Available after wallet session starts"
    );
}

#[test]
fn private_asset_rows_include_usd_when_cache_has_rates() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));
    let totals = [
        wallet_ops::TokenTotal {
            token: usdc.to_checksum(None),
            total: "1234567".to_string(),
            poi_verified_total: "1234567".to_string(),
        },
        wallet_ops::TokenTotal {
            token: weth.to_checksum(None),
            total: "500000000000000000".to_string(),
            poi_verified_total: "500000000000000000".to_string(),
        },
    ];

    let rows = format_private_asset_rows(1, &totals, None, Some(&cache));

    assert_eq!(rows[0].usd_amount.as_deref(), Some("$1.23"));
    assert_eq!(rows[1].usd_amount.as_deref(), Some("$1,500.00"));
}

#[test]
fn private_asset_rows_from_snapshot_sort_by_usd_descending() {
    let rail = address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D");
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 0,
        unspent_count: 0,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: Vec::new(),
        totals: vec![
            wallet_ops::TokenTotal {
                token: rail.to_checksum(None),
                total: "1000000000000000000".to_string(),
                poi_verified_total: "1000000000000000000".to_string(),
            },
            wallet_ops::TokenTotal {
                token: usdc.to_checksum(None),
                total: "1234567".to_string(),
                poi_verified_total: "1234567".to_string(),
            },
            wallet_ops::TokenTotal {
                token: weth.to_checksum(None),
                total: "500000000000000000".to_string(),
                poi_verified_total: "500000000000000000".to_string(),
            },
        ],
    };

    let rows = format_private_asset_rows_from_snapshot(&snapshot, None, Some(&cache));

    assert_eq!(
        rows.iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        ["WETH", "USDC", "RAIL"]
    );
    assert_eq!(rows[0].usd_amount.as_deref(), Some("$1,500.00"));
    assert_eq!(rows[1].usd_amount.as_deref(), Some("$1.23"));
    assert_eq!(rows[2].usd_amount, None);
}

#[test]
fn private_asset_rows_from_snapshot_preserve_equal_and_unpriced_order() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let rail = address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D");
    let unknown = Address::from([0x99; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 0,
        unspent_count: 0,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: Vec::new(),
        totals: vec![
            wallet_ops::TokenTotal {
                token: usdc.to_checksum(None),
                total: "1500000000".to_string(),
                poi_verified_total: "1500000000".to_string(),
            },
            wallet_ops::TokenTotal {
                token: weth.to_checksum(None),
                total: "500000000000000000".to_string(),
                poi_verified_total: "500000000000000000".to_string(),
            },
            wallet_ops::TokenTotal {
                token: rail.to_checksum(None),
                total: "1000000000000000000".to_string(),
                poi_verified_total: "1000000000000000000".to_string(),
            },
            wallet_ops::TokenTotal {
                token: unknown.to_checksum(None),
                total: "1".to_string(),
                poi_verified_total: "1".to_string(),
            },
        ],
    };

    let rows = format_private_asset_rows_from_snapshot(&snapshot, None, Some(&cache));

    assert_eq!(
        rows.iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        ["USDC", "WETH", "RAIL", "0x9999…9999"]
    );
    assert_eq!(rows[0].usd_amount.as_deref(), Some("$1,500.00"));
    assert_eq!(rows[1].usd_amount.as_deref(), Some("$1,500.00"));
    assert_eq!(rows[2].usd_amount, None);
    assert_eq!(rows[3].usd_amount, None);
}

#[test]
fn private_total_balance_sums_priced_assets() {
    let rail = address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D");
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 0,
        unspent_count: 0,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: Vec::new(),
        totals: vec![
            wallet_ops::TokenTotal {
                token: usdc.to_checksum(None),
                total: "1234567".to_string(),
                poi_verified_total: "1234567".to_string(),
            },
            wallet_ops::TokenTotal {
                token: weth.to_checksum(None),
                total: "500000000000000000".to_string(),
                poi_verified_total: "500000000000000000".to_string(),
            },
            wallet_ops::TokenTotal {
                token: rail.to_checksum(None),
                total: "1000000000000000000".to_string(),
                poi_verified_total: "1000000000000000000".to_string(),
            },
        ],
    };

    let rows = format_private_asset_rows_from_snapshot(&snapshot, None, Some(&cache));

    assert_eq!(
        total_private_balance_usd_amount(&rows).as_deref(),
        Some("$1,501.23")
    );
}

#[test]
fn private_total_balance_absent_without_priced_assets() {
    let rail = address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D");
    let rows = format_private_asset_rows(
        1,
        &[wallet_ops::TokenTotal {
            token: rail.to_checksum(None),
            total: "1000000000000000000".to_string(),
            poi_verified_total: "1000000000000000000".to_string(),
        }],
        None,
        None,
    );

    assert_eq!(total_private_balance_usd_amount(&rows), None);
}

#[test]
fn private_asset_display_amounts_prioritize_usd_when_available() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));
    let totals = [wallet_ops::TokenTotal {
        token: usdc.to_checksum(None),
        total: "1234567".to_string(),
        poi_verified_total: "1234567".to_string(),
    }];
    let rows = format_private_asset_rows(1, &totals, None, Some(&cache));

    assert_eq!(
        private_asset_display_amounts(&rows[0]),
        ("$1.23".to_string(), Some("1.23 USDC".to_string()))
    );
}

#[test]
fn private_asset_display_amounts_keep_token_primary_when_unpriced() {
    let totals = [wallet_ops::TokenTotal {
        token: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(),
        total: "1234567".to_string(),
        poi_verified_total: "1234567".to_string(),
    }];
    let rows = format_private_asset_rows(1, &totals, None, None);

    assert_eq!(
        private_asset_display_amounts(&rows[0]),
        ("1.23".to_string(), None)
    );
}

#[test]
fn private_asset_rows_omit_usd_without_required_pricing() {
    let rail = address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D");
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let unknown = Address::from([0x99; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    let totals = [
        wallet_ops::TokenTotal {
            token: rail.to_checksum(None),
            total: "1000000000000000000".to_string(),
            poi_verified_total: "1000000000000000000".to_string(),
        },
        wallet_ops::TokenTotal {
            token: usdc.to_checksum(None),
            total: "1234567".to_string(),
            poi_verified_total: "1234567".to_string(),
        },
        wallet_ops::TokenTotal {
            token: unknown.to_checksum(None),
            total: "1234567".to_string(),
            poi_verified_total: "1234567".to_string(),
        },
    ];

    let rows = format_private_asset_rows(1, &totals, None, Some(&cache));

    assert_eq!(rows[0].label, "RAIL");
    assert_eq!(rows[0].usd_amount, None);
    assert_eq!(rows[1].label, "USDC");
    assert_eq!(rows[1].usd_amount, None);
    assert_eq!(rows[2].token, Some(unknown));
    assert_eq!(rows[2].usd_amount, None);
}

#[test]
fn private_asset_rows_hide_zero_pending_poi() {
    let totals = [wallet_ops::TokenTotal {
        token: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(),
        total: "1234567".to_string(),
        poi_verified_total: "1234567".to_string(),
    }];

    let rows = format_private_asset_rows(1, &totals, None, None);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].pending_poi_amount, "0");
    assert_eq!(rows[0].pending_poi_total, Some(U256::ZERO));
    assert!(!should_show_pending_poi_amount(rows[0].pending_poi_total));
}

#[test]
fn private_pending_retry_labels_use_submission_copy() {
    assert_eq!(retry_poi_label(1, false), "Retry");
    assert_eq!(retry_poi_label(2, false), "Retry (2)");
    assert_eq!(retry_poi_label(0, true), "Queue PPOI retry");
}

#[test]
fn private_pending_banner_collapses_pending_states_to_one_line() {
    let token = Address::from([0x11; 20]);
    let mut incoming = unshield_utxo_output(token, 7, 0, 2);
    incoming.pending_new = true;
    let incoming_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "incoming".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![incoming],
        totals: Vec::new(),
    };
    let incoming_assets = format_private_asset_rows_from_snapshot(&incoming_snapshot, None, None);
    let incoming_summary = private_pending_summary(
        &incoming_assets,
        &incoming_snapshot,
        Vec::new(),
        false,
        None,
    )
    .expect("incoming summary");

    assert_eq!(
        private_pending_summary_title(&incoming_summary),
        "1 asset not yet spendable"
    );
    assert_eq!(private_pending_summary_detail(&incoming_summary), None);
    let incoming_view =
        crate::root::private_assets::private_pending_presentation(&incoming_summary);
    assert_eq!(incoming_view.categories.len(), 1);
    assert_eq!(incoming_view.categories[0].title, "Pending incoming");
    assert_eq!(
        incoming_view.categories[0].assets[0].amount,
        format!("+{}", incoming_assets[0].pending_incoming_amount)
    );

    let mut outgoing = unshield_utxo_output(token, 5, 0, 1);
    outgoing.pending_spent = true;
    let outgoing_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "outgoing".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![outgoing],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "5".to_string(),
            poi_verified_total: "5".to_string(),
        }],
    };
    let outgoing_assets = format_private_asset_rows_from_snapshot(&outgoing_snapshot, None, None);
    let outgoing_summary = private_pending_summary(
        &outgoing_assets,
        &outgoing_snapshot,
        Vec::new(),
        false,
        None,
    )
    .expect("outgoing summary");

    assert_eq!(
        private_pending_summary_title(&outgoing_summary),
        "1 asset awaiting confirmation"
    );
    assert_eq!(private_pending_summary_detail(&outgoing_summary), None);
    let outgoing_view =
        crate::root::private_assets::private_pending_presentation(&outgoing_summary);
    assert_eq!(outgoing_view.categories.len(), 1);
    assert_eq!(outgoing_view.categories[0].title, "Pending outgoing");
    assert_eq!(
        outgoing_view.categories[0].assets[0].amount,
        format!("-{}", outgoing_assets[0].pending_outgoing_amount)
    );
}

#[test]
fn private_pending_summary_surfaces_sender_recovery_without_assets() {
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "sender-recovery".to_string(),
        utxo_count: 0,
        unspent_count: 0,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: Vec::new(),
        totals: Vec::new(),
    };
    let summary = private_pending_summary_with_workflow(
        &[],
        &snapshot,
        Vec::new(),
        wallet_ops::WalletPpoiWorkflowStatus {
            awaiting_recovery: 1,
            awaiting_public_txid_data: 1,
            ..wallet_ops::WalletPpoiWorkflowStatus::default()
        },
        false,
        None,
    )
    .expect("sender workflow summary");

    assert_eq!(
        private_pending_summary_title(&summary),
        "Waiting for public transaction proof data"
    );
}

#[test]
fn private_asset_rows_show_separate_pending_amounts() {
    let token = Address::from([0x11; 20]);
    let mut pending_in = unshield_utxo_output(token, 7, 0, 2);
    pending_in.pending_new = true;
    pending_in.poi_spendable = false;
    let mut pending_out = unshield_utxo_output(token, 5, 0, 1);
    pending_out.pending_spent = true;
    pending_out.poi_spendable = false;
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![pending_out, pending_in],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "5".to_string(),
            poi_verified_total: "5".to_string(),
        }],
    };

    let rows = format_private_asset_rows_from_snapshot(&snapshot, None, None);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].total, Some(uint!(5_U256)));
    assert_eq!(rows[0].pending_incoming_total, Some(uint!(7_U256)));
    assert_eq!(rows[0].pending_outgoing_total, Some(uint!(5_U256)));
    assert!(should_show_pending_amount(rows[0].pending_incoming_total));
    assert!(should_show_pending_amount(rows[0].pending_outgoing_total));
}

#[test]
fn private_asset_rows_include_local_pending_outgoing_amount() {
    let token = Address::from([0x11; 20]);
    let mut local_pending_out = unshield_utxo_output(token, 5, 0, 1);
    local_pending_out.local_pending_spent = true;
    local_pending_out.poi_spendable = false;
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 1,
        utxos: vec![local_pending_out],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "5".to_string(),
            poi_verified_total: "5".to_string(),
        }],
    };

    let rows = format_private_asset_rows_from_snapshot(&snapshot, None, None);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].pending_outgoing_total, Some(uint!(5_U256)));
    assert!(should_show_pending_amount(rows[0].pending_outgoing_total));
}

#[test]
fn unshield_amount_input_formats_exact_token_units() {
    assert_eq!(
        format_unshield_amount_input(uint!(1_230_000_U256), Some(6)),
        "1.23"
    );
    assert_eq!(
        format_unshield_amount_input(uint!(1_000_000_U256), Some(6)),
        "1"
    );
    assert_eq!(format_unshield_amount_input(uint!(42_U256), None), "42");
    assert_eq!(
        format_unshield_amount_input(uint!(3_225_402_349_859_634_605_360_U256), Some(18),),
        "3225.40234985963460536"
    );
}

#[test]
fn send_amount_input_formats_exact_token_units() {
    assert_eq!(
        format_send_amount_input(uint!(1_230_000_U256), Some(6)),
        "1.23"
    );
    assert_eq!(
        format_send_amount_input(uint!(1_000_000_U256), Some(6)),
        "1"
    );
    assert_eq!(format_send_amount_input(uint!(42_U256), None), "42");
}

#[test]
fn transaction_generation_stage_text_is_specific() {
    assert_eq!(
        TransactionGenerationStage::SelectingPrivateNotes.label(),
        "Selecting private notes"
    );
    assert_eq!(
        TransactionGenerationStage::ProvingTransaction.detail(),
        "Generating the zero-knowledge proof. This is usually the slowest step."
    );
    assert_eq!(
        TransactionGenerationStage::PublishingToBroadcaster.label(),
        "Publishing to broadcaster"
    );
    assert_eq!(
        TransactionGenerationStage::WaitingForBroadcasterResponse.detail(),
        "Waiting for the selected broadcaster to respond."
    );
}
