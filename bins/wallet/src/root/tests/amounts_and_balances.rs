use super::*;
use crate::root::chain_load::SyncStatusContext;
use crate::root::public_action::public_action_progress_steps_for_source;
use crate::root::public_balances::{
    public_account_usd_total_label_for_chain, public_balance_entry_for_chain,
};
use crate::root::shell::{balance_sync_data_source, balance_sync_source_label};
use wallet_ops::{
    PublicBroadcasterFeeBreakdown, PublicScanSource, SyncProgressStage, SyncProgressUpdate,
};

#[test]
fn balance_sync_source_labels_cover_every_public_scan_source() {
    assert_eq!(
        balance_sync_source_label(PublicScanSource::CachedCoverage),
        "Local cache"
    );
    assert_eq!(
        balance_sync_source_label(PublicScanSource::IndexedArtifacts),
        "Verified artifacts"
    );
    assert_eq!(
        balance_sync_source_label(PublicScanSource::Squid),
        "Squid index"
    );
    assert_eq!(balance_sync_source_label(PublicScanSource::Rpc), "RPC");
    assert_eq!(
        balance_sync_source_label(PublicScanSource::ArchiveRpc),
        "Archive RPC"
    );
}

#[test]
fn balance_sync_data_source_is_active_and_progress_scoped() {
    let sourced_progress = SyncProgressUpdate::new(SyncProgressStage::IndexingUtxos, 100, 150, 200)
        .with_source(PublicScanSource::Rpc);
    let source_free_progress =
        SyncProgressUpdate::artifact_preparation(SyncProgressStage::PreparingUtxoIndex, 1, 10);

    assert_eq!(
        balance_sync_data_source(Some(SyncStatusContext::Syncing), Some(sourced_progress)),
        Some(PublicScanSource::Rpc)
    );
    assert_eq!(
        balance_sync_data_source(Some(SyncStatusContext::Loading), Some(source_free_progress)),
        None
    );
    assert_eq!(balance_sync_data_source(None, Some(sourced_progress)), None);
    assert_eq!(
        balance_sync_data_source(Some(SyncStatusContext::Syncing), None),
        None
    );
}

#[test]
fn private_action_metrics_hide_values_matching_total() {
    let token = Address::from([0x11; 20]);
    let mut asset = UnshieldAsset {
        chain_id: 1,
        token,
        label: "WETH".to_string(),
        decimals: Some(18),
        total: uint!(10_U256),
        poi_verified_total: uint!(10_U256),
        max_batched: uint!(10_U256),
        icon_path: None,
    };

    assert_eq!(
        private_action_metrics(&asset),
        vec![PrivateActionMetric {
            label: "Total private balance",
            amount: uint!(10_U256),
        }]
    );

    asset.poi_verified_total = uint!(7_U256);
    assert_eq!(
        private_action_metrics(&asset),
        vec![
            PrivateActionMetric {
                label: "Total private balance",
                amount: uint!(10_U256),
            },
            PrivateActionMetric {
                label: "POI-verified balance",
                amount: uint!(7_U256),
            },
        ]
    );

    asset.poi_verified_total = asset.total;
    asset.max_batched = uint!(8_U256);
    assert_eq!(
        private_action_metrics(&asset),
        vec![
            PrivateActionMetric {
                label: "Total private balance",
                amount: uint!(10_U256),
            },
            PrivateActionMetric {
                label: "Max batched transaction",
                amount: uint!(8_U256),
            },
        ]
    );
}

#[test]
fn private_action_metric_display_amount_uses_compact_precision() {
    assert_eq!(
        private_action_metric_display_amount(uint!(10_447_680_100_412_055_662_U256), Some(18)),
        "10.45"
    );
    assert_eq!(
        private_action_metric_display_amount(uint!(10_437_705_100_412_055_662_U256), Some(18)),
        "10.44"
    );
    assert_eq!(
        private_action_metric_display_amount(uint!(42_U256), None),
        "42"
    );
}

#[test]
fn native_wrapped_output_labels_are_chain_specific() {
    assert_eq!(native_wrapped_output_labels(1), Some(("ETH", "WETH")));
    assert_eq!(native_wrapped_output_labels(56), Some(("BNB", "WBNB")));
    assert_eq!(native_wrapped_output_labels(137), Some(("MATIC", "WMATIC")));
    assert_eq!(native_wrapped_output_labels(42161), Some(("ETH", "WETH")));
    assert_eq!(native_wrapped_output_labels(999_999), None);
}

#[test]
fn native_gas_cost_display_uses_base_token_label() {
    assert_eq!(native_token_display_label(1), "ETH");
    assert_eq!(native_token_display_label(999_999), "base token");
    assert_eq!(
        format_native_token_amount_for_display(1, uint!(1_500_000_000_000_000_U256)),
        "0.0015 ETH"
    );
}

#[test]
fn public_account_default_label_number_uses_account_count() {
    assert_eq!(next_public_account_label_number(0), 1);
    assert_eq!(next_public_account_label_number(2), 3);
    assert_eq!(next_public_account_label_number(usize::MAX), u32::MAX);
}

#[test]
fn public_broadcaster_fee_margin_display_is_signed_fee_token_amount() {
    let usdc = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        .parse::<Address>()
        .expect("usdc address");

    assert_eq!(
        format_public_broadcaster_fee_margin(
            1,
            usdc,
            PublicBroadcasterFeeMargin::Positive(uint!(123_456_U256)),
            None,
        ),
        "0.12346 USDC"
    );
    assert_eq!(
        format_public_broadcaster_fee_margin(
            1,
            usdc,
            PublicBroadcasterFeeMargin::Negative(uint!(42_U256)),
            None,
        ),
        "-0.000042 USDC"
    );
    assert_eq!(
        format_public_broadcaster_fee_margin(1, usdc, PublicBroadcasterFeeMargin::Zero, None),
        "0 USDC"
    );
}

#[test]
fn public_broadcaster_cost_display_uses_shared_amount_precision() {
    let dai = address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");
    let candidate = wallet_ops::public_broadcaster_candidates_for_asset(
        &[fee_row(1, dai, "formatted-amounts")],
        1,
        dai,
        None,
        BroadcasterFeePolicy::default(),
        None,
    )
    .expect("candidate")
    .remove(0);
    let mut estimate = public_broadcaster_cost_estimate(candidate);
    estimate.action_token = dai;
    estimate.fee_token = dai;
    estimate.recipient_amount = uint!(3_217_175_912_188_463_386_625_U256);
    estimate.fee_amount = uint!(163_340_146_818_210_787_U256);
    estimate.protocol_fee_amount = uint!(8_063_097_524_281_863_124_U256);
    estimate.protocol_fee_bps = uint!(25_U256);

    let display = PublicBroadcasterCostDisplay::from_estimate_chain(1, &estimate, None, None);
    assert_eq!(display.action_amount(display.recipient_amount), "3217 DAI");
    assert_eq!(display.fee_amount(), "0.16334 DAI");
}

#[test]
fn public_broadcaster_cost_display_filters_only_redundant_usd_values() {
    let dai = address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");
    let candidate = wallet_ops::public_broadcaster_candidates_for_asset(
        &[fee_row(1, dai, "usd-values")],
        1,
        dai,
        None,
        BroadcasterFeePolicy::default(),
        None,
    )
    .expect("candidate")
    .remove(0);
    let mut estimate = public_broadcaster_cost_estimate(candidate);
    estimate.action_token = dai;
    estimate.fee_token = dai;
    estimate.fee_amount = uint!(1_000_000_000_000_000_000_U256);
    estimate.protocol_fee_amount = uint!(100_000_000_000_000_000_U256);
    estimate.protocol_fee_bps = uint!(25_U256);
    let display = PublicBroadcasterCostDisplay::from_estimate_chain(1, &estimate, None, None);
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    cache.store_rate(1, dai, uint!(3_000_000_000_000_000_000_000_U256));
    let breakdown = PublicBroadcasterFeeBreakdown {
        native_gas_cost: uint!(1_000_000_000_000_000_U256),
        fee_token_gas_cost: Some(uint!(250_000_000_000_000_000_U256)),
        broadcaster_fee: Some(PublicBroadcasterFeeMargin::Negative(uint!(
            500_000_000_000_000_000_U256
        ))),
    };

    let stable_token_values = [
        display.action_amount_with_usd(uint!(2_000_000_000_000_000_000_U256), &cache),
        display.fee_amount_with_usd(&cache),
        display.protocol_fee_value_with_usd(&cache),
        display.broadcaster_fee_value_with_usd(&breakdown, &cache),
    ];
    assert!(stable_token_values.iter().all(|value| !value.contains('$')));
    assert!(
        stable_token_values
            .iter()
            .all(|value| !value.contains("USD unavailable"))
    );
    assert!(
        display
            .native_gas_cost_value_with_usd(&breakdown, &cache)
            .contains('$')
    );

    let outside_peg_cache = TokenAnchorRateCache::new();
    outside_peg_cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    outside_peg_cache.store_rate(1, dai, uint!(1_500_000_000_000_000_000_000_U256));
    assert!(
        display
            .action_amount_with_usd(uint!(2_000_000_000_000_000_000_U256), &outside_peg_cache,)
            .contains('$')
    );
    assert!(
        display
            .broadcaster_fee_value_with_usd(&breakdown, &outside_peg_cache)
            .contains("-$")
    );
    assert!(
        display
            .fee_amount_with_usd(&TokenAnchorRateCache::new())
            .contains("USD unavailable")
    );
}

#[test]
fn public_broadcaster_cost_display_uses_effective_token_metadata() {
    let token = Address::from([0x88; 20]);
    let mut settings = WalletSettings::default();
    settings.tokens.custom_tokens.push(CustomTokenSettings {
        chain_id: 1,
        token_address: token.to_string(),
        symbol: "TST".to_string(),
        decimals: 4,
        icon_path: None,
        price_anchor: None,
    });
    let registry = build_effective_token_registry(&settings).expect("effective registry");
    let candidate = wallet_ops::public_broadcaster_candidates_for_asset(
        &[fee_row(1, token, "custom-token-formatting")],
        1,
        token,
        None,
        BroadcasterFeePolicy::default(),
        None,
    )
    .expect("candidate")
    .remove(0);
    let mut estimate = public_broadcaster_cost_estimate(candidate);
    estimate.action_token = token;
    estimate.fee_token = token;
    estimate.entered_amount = uint!(123_456_U256);

    let display =
        PublicBroadcasterCostDisplay::from_estimate_chain(1, &estimate, None, Some(&registry));
    assert_eq!(display.action_amount(display.entered_amount), "12.35 TST");
}

#[test]
fn native_top_up_plan_display_and_request_use_fixed_native_amount() {
    let token = Address::from([0x41; 20]);
    let candidate = wallet_ops::public_broadcaster_candidates_for_asset(
        &[fee_row(1, token, "native-top-up")],
        1,
        token,
        None,
        BroadcasterFeePolicy::default(),
        None,
    )
    .expect("candidate")
    .remove(0);
    let mut estimate = public_broadcaster_cost_estimate(candidate);
    estimate.native_top_up = Some(DesktopNativeTopUpPlan {
        recipient: Address::from([0x42; 20]),
        wrapped_native_token: token,
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: wallet_ops::native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    });

    let display = PublicBroadcasterCostDisplay::from_estimate_chain(1, &estimate, None, None);
    assert_eq!(
        display.native_top_up_recipient_suffix(),
        Some("+ 0.003 ETH (gas top-up)".to_string())
    );
    assert_eq!(
        estimate
            .native_top_up
            .as_ref()
            .map(|top_up| top_up.wrapped_native_amount),
        Some(uint!(3_007_518_796_992_481_U256))
    );

    assert_eq!(
        native_top_up_request_from_plan(estimate.native_top_up.as_ref()),
        Some(wallet_ops::DesktopNativeTopUpRequest)
    );

    let result = PublicBroadcasterSubmissionResult {
        broadcaster: estimate.broadcaster.clone(),
        action_token: estimate.action_token,
        fee_token: estimate.fee_token,
        entered_amount: estimate.entered_amount,
        receiver_amount: estimate.receiver_amount,
        recipient_amount: estimate.recipient_amount,
        total_private_spend: estimate.total_private_spend,
        fee_amount: estimate.fee_amount,
        protocol_fee_amount: estimate.protocol_fee_amount,
        protocol_fee_bps: estimate.protocol_fee_bps,
        fee_mode: estimate.fee_mode,
        gas_limit: estimate.gas_limit,
        min_gas_price: estimate.min_gas_price,
        transaction_count: estimate.transaction_count,
        input_count: estimate.input_count,
        private_output_count: estimate.private_output_count,
        public_output_count: estimate.public_output_count,
        relay_call_count: estimate.relay_call_count,
        uses_relay_adapt: estimate.uses_relay_adapt,
        result: PublicBroadcasterResultKind::Submitted {
            tx_hash: "0xabc".to_string(),
        },
        native_top_up: estimate.native_top_up.clone(),
    };
    let display = PublicBroadcasterCostDisplay::from_result(&result, None, None);
    assert_eq!(
        display.native_top_up_recipient_suffix(),
        Some("+ 0.003 ETH (gas top-up)".to_string())
    );
}

#[test]
fn native_top_up_eligibility_accepts_known_and_arbitrary_recipients_but_stays_explicit() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let recipient = Address::from([0x42; 20]);
    let private_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![unshield_utxo_output(weth, 3_007_518_796_992_481, 0, 1)],
        totals: Vec::new(),
    };
    let state = unshield_native_top_up_state_from_inputs(
        1,
        usdc,
        false,
        recipient,
        uint!(100_000_000_U256),
        wallet_ops::FeeHandlingMode::DeductFromAmount,
        Some(&private_snapshot),
        Some(weth),
    );
    let plan = state.plan.expect("eligible top-up plan");
    assert_eq!(plan.recipient, recipient);
    assert_eq!(plan.native_amount, uint!(3_000_000_000_000_000_U256));
    assert_eq!(
        plan.wrapped_native_amount,
        uint!(3_007_518_796_992_481_U256)
    );
    assert_eq!(
        enabled_native_top_up_plan(false, Some(&plan)),
        None,
        "eligibility must not auto-select the top-up"
    );
    assert_eq!(
        enabled_native_top_up_plan(true, Some(&plan)),
        Some(plan.clone())
    );

    let arbitrary = unshield_native_top_up_state_from_inputs(
        1,
        usdc,
        false,
        Address::from([0x43; 20]),
        uint!(100_000_000_U256),
        wallet_ops::FeeHandlingMode::DeductFromAmount,
        Some(&private_snapshot),
        Some(weth),
    );
    assert!(arbitrary.plan.is_some());
}

#[test]
fn native_top_up_eligibility_uses_add_on_top_gross_amount() {
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let recipient = Address::from([0x42; 20]);
    let entered_amount = uint!(1_000_000_U256);
    let top_up_amount = uint!(3_000_000_000_000_000_U256);
    let old_required =
        entered_amount + wallet_ops::native_top_up_wrapped_native_amount(top_up_amount);
    let gross_required = wallet_ops::native_top_up_required_wrapped_native_amount_for_fee_mode(
        weth,
        weth,
        entered_amount,
        wallet_ops::FeeHandlingMode::AddToAmount,
        top_up_amount,
    );
    assert!(gross_required > old_required);
    let private_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![unshield_utxo_output(weth, old_required.to::<u64>(), 0, 1)],
        totals: Vec::new(),
    };
    let state = unshield_native_top_up_state_from_inputs(
        1,
        weth,
        false,
        recipient,
        entered_amount,
        wallet_ops::FeeHandlingMode::AddToAmount,
        Some(&private_snapshot),
        Some(weth),
    );

    assert!(state.plan.is_none());
}

#[test]
fn native_top_up_passive_eligibility_refresh_does_not_cancel_estimate() {
    assert!(!native_top_up_refresh_invalidates_estimate(
        false, false, true
    ));
    assert!(!native_top_up_refresh_invalidates_estimate(
        false, true, true
    ));
    assert!(native_top_up_refresh_invalidates_estimate(true, true, true));
    assert!(native_top_up_refresh_invalidates_estimate(
        true, false, false
    ));
}

#[test]
fn max_unshield_amount_from_snapshot_uses_batched_top_chunks() {
    let token = Address::from([0x11; 20]);
    let other = Address::from([0x22; 20]);
    let mut utxos = (0..20)
        .map(|position| unshield_utxo_output(token, 1, 0, position))
        .collect::<Vec<_>>();
    utxos.extend((0..5).map(|position| unshield_utxo_output(token, 3, 1, position)));
    utxos.push(unshield_utxo_output(other, 100, 1, 99));
    let mut unknown = unshield_utxo_output(token, 100, 2, 1);
    unknown.poi_statuses.clear();
    unknown.poi_spendable = false;
    utxos.push(unknown);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: utxos.len(),
        unspent_count: utxos.len(),
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos,
        totals: Vec::new(),
    };

    assert_eq!(
        max_unshield_amount_from_snapshot(&snapshot, token),
        uint!(35_U256)
    );
}

#[test]
fn refreshed_form_asset_tracks_new_utxos() {
    let token = Address::from([0x11; 20]);
    let original_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![unshield_utxo_output(token, 5, 0, 1)],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "5".to_string(),
            poi_verified_total: "5".to_string(),
        }],
    };
    let original_row = format_private_asset_rows(1, &original_snapshot.totals, None, None)
        .pop()
        .expect("formatted row");
    let original_asset =
        build_unshield_asset(&original_snapshot, &original_row).expect("original asset");
    let updated_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(token, 5, 0, 1),
            unshield_utxo_output(token, 3, 0, 2),
        ],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "8".to_string(),
            poi_verified_total: "8".to_string(),
        }],
    };

    let updated = refresh_form_asset_from_snapshot(&updated_snapshot, &original_asset, false, None);

    assert_eq!(updated.total, uint!(8_U256));
    assert_eq!(updated.poi_verified_total, uint!(8_U256));
    assert_eq!(updated.max_batched, uint!(8_U256));
}

#[test]
fn refreshed_form_asset_tracks_spent_out_token() {
    let token = Address::from([0x11; 20]);
    let original_asset = UnshieldAsset {
        chain_id: 1,
        token,
        label: "WETH".to_string(),
        decimals: Some(18),
        total: uint!(5_U256),
        poi_verified_total: uint!(5_U256),
        max_batched: uint!(5_U256),
        icon_path: None,
    };
    let mut spent = unshield_utxo_output(token, 5, 0, 1);
    spent.is_spent = true;
    spent.poi_spendable = false;
    spent.spent_tx_hash =
        Some("0x2222222222222222222222222222222222222222222222222222222222222222".to_string());
    spent.spent_block_number = Some(21);
    let updated_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 0,
        spent_count: 1,
        local_pending_spent_count: 0,
        utxos: vec![spent],
        totals: Vec::new(),
    };

    let updated = refresh_form_asset_from_snapshot(&updated_snapshot, &original_asset, false, None);

    assert_eq!(updated.label, "WETH");
    assert_eq!(updated.decimals, Some(18));
    assert_eq!(updated.total, U256::ZERO);
    assert_eq!(updated.poi_verified_total, U256::ZERO);
    assert_eq!(updated.max_batched, U256::ZERO);
}

#[test]
fn max_send_amount_from_snapshot_uses_batched_top_chunks() {
    let token = Address::from([0x12; 20]);
    let other = Address::from([0x22; 20]);
    let mut utxos = (0..20)
        .map(|position| unshield_utxo_output(token, 1, 0, position))
        .collect::<Vec<_>>();
    utxos.extend((0..5).map(|position| unshield_utxo_output(token, 3, 1, position)));
    utxos.push(unshield_utxo_output(other, 100, 1, 99));
    let mut unknown = unshield_utxo_output(token, 100, 2, 1);
    unknown.poi_statuses.clear();
    unknown.poi_spendable = false;
    utxos.push(unknown);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: utxos.len(),
        unspent_count: utxos.len(),
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos,
        totals: Vec::new(),
    };

    assert_eq!(
        max_send_amount_from_snapshot(&snapshot, token),
        uint!(35_U256)
    );
}

#[test]
fn build_unshield_asset_includes_max_batched_transaction() {
    let token = Address::from([0x33; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(token, 5, 0, 1),
            unshield_utxo_output(token, 7, 0, 2),
        ],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "12".to_string(),
            poi_verified_total: "12".to_string(),
        }],
    };
    let row = format_private_asset_rows(1, &snapshot.totals, None, None)
        .into_iter()
        .next()
        .expect("asset row");

    let asset = build_unshield_asset(&snapshot, &row).expect("unshield asset");

    assert_eq!(asset.total, uint!(12_U256));
    assert_eq!(asset.max_batched, uint!(12_U256));
}

#[test]
fn build_send_asset_includes_max_batched_transaction() {
    let token = Address::from([0x34; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(token, 5, 0, 1),
            unshield_utxo_output(token, 7, 0, 2),
        ],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "12".to_string(),
            poi_verified_total: "12".to_string(),
        }],
    };
    let row = format_private_asset_rows(1, &snapshot.totals, None, None)
        .into_iter()
        .next()
        .expect("asset row");

    let asset = build_send_asset(&snapshot, &row).expect("send asset");

    assert_eq!(asset.total, uint!(12_U256));
    assert_eq!(asset.max_batched, uint!(12_U256));
}

#[test]
fn private_action_assets_from_snapshot_include_only_switchable_assets() {
    let weth = Address::from([0x35; 20]);
    let dai = Address::from([0x36; 20]);
    let stale = Address::from([0x37; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(weth, 5, 0, 1),
            unshield_utxo_output(dai, 7, 0, 2),
        ],
        totals: vec![
            wallet_ops::TokenTotal {
                token: weth.to_checksum(None),
                total: "5".to_string(),
                poi_verified_total: "5".to_string(),
            },
            wallet_ops::TokenTotal {
                token: stale.to_checksum(None),
                total: "3".to_string(),
                poi_verified_total: "3".to_string(),
            },
            wallet_ops::TokenTotal {
                token: dai.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
        ],
    };

    let send_assets =
        private_action_assets_from_snapshot(DeliveryFormKind::Send, &snapshot, None, None);
    let unshield_assets =
        private_action_assets_from_snapshot(DeliveryFormKind::Unshield, &snapshot, None, None);

    assert_eq!(
        send_assets
            .iter()
            .map(|asset| asset.token)
            .collect::<Vec<_>>(),
        [weth, dai]
    );
    assert_eq!(
        unshield_assets
            .iter()
            .map(|asset| asset.token)
            .collect::<Vec<_>>(),
        [weth, dai]
    );
}

#[test]
fn unshield_key_matches_only_selected_asset() {
    let token = Address::from([0x44; 20]);
    let other = Address::from([0x45; 20]);
    let rows = format_private_asset_rows(
        1,
        &[
            wallet_ops::TokenTotal {
                token: token.to_checksum(None),
                total: "5".to_string(),
                poi_verified_total: "5".to_string(),
            },
            wallet_ops::TokenTotal {
                token: other.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
        ],
        None,
        None,
    );
    let key = UnshieldAssetKey::new(1, token);

    assert_eq!(unshield_asset_key_from_formatted(&rows[0]), Some(key));
    assert!(unshield_key_matches_asset(key, &rows[0]));
    assert!(!unshield_key_matches_asset(key, &rows[1]));
}

#[test]
fn send_key_matches_only_selected_asset() {
    let token = Address::from([0x46; 20]);
    let other = Address::from([0x47; 20]);
    let rows = format_private_asset_rows(
        1,
        &[
            wallet_ops::TokenTotal {
                token: token.to_checksum(None),
                total: "5".to_string(),
                poi_verified_total: "5".to_string(),
            },
            wallet_ops::TokenTotal {
                token: other.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
        ],
        None,
        None,
    );
    let key = UnshieldAssetKey::new(1, token);

    assert_eq!(send_asset_key_from_formatted(&rows[0]), Some(key));
    assert!(send_key_matches_asset(key, &rows[0]));
    assert!(!send_key_matches_asset(key, &rows[1]));
}

#[test]
fn unshield_element_ids_are_asset_scoped() {
    let first = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let second = UnshieldAssetKey::new(1, Address::from([0x22; 20]));

    assert_ne!(
        unshield_element_id(first, "cancel").as_ref(),
        unshield_element_id(second, "cancel").as_ref()
    );
    assert_ne!(
        unshield_element_id(first, "copy-to").as_ref(),
        unshield_element_id(first, "copy-data").as_ref()
    );
}

fn public_account_for_search(label: Option<&str>, address: Address) -> PublicAccountMetadata {
    public_account_for_search_with_uuid("public-account", label, address)
}

fn public_account_for_search_with_uuid(
    uuid: &str,
    label: Option<&str>,
    address: Address,
) -> PublicAccountMetadata {
    PublicAccountMetadata {
        public_account_uuid: uuid.to_string(),
        address,
        label: label.map(str::to_string),
        source: PublicAccountSource::Imported,
        scope: PublicAccountScope::Global,
        derivation_index: None,
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    }
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
fn self_broadcast_native_balance_label_uses_snapshot_or_unavailable() {
    let snapshot = public_balance_snapshot_for_test(1);
    let expected =
        public_balance_amount_label(&PublicBalanceAmount::Available(U256::from(5_u64)), 18);

    assert_eq!(
        self_broadcast_native_balance_label(Some(&snapshot), 1, "public-account"),
        expected
    );
    assert_eq!(
        self_broadcast_native_balance_label(Some(&snapshot), 56, "public-account"),
        "unavailable"
    );
    assert_eq!(
        self_broadcast_native_balance_label(Some(&snapshot), 1, "missing"),
        "unavailable"
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

#[test]
fn eip1559_gas_fee_helpers_parse_and_format_gwei() {
    assert_eq!(parse_gwei_to_wei("1"), Ok(1_000_000_000));
    assert_eq!(parse_gwei_to_wei("1.25"), Ok(1_250_000_000));
    assert_eq!(parse_gwei_to_wei("0.000000001"), Ok(1));
    assert!(parse_gwei_to_wei("0.0000000001").is_err());
    assert_eq!(format_gwei(1_250_000_000), "1.25");
    assert_eq!(format_gwei(1), "0.000000001");
}

#[test]
fn eip1559_custom_gas_fee_validation_rejects_invalid_caps() {
    assert!(validate_custom_gas_fee(1, 0).is_ok());
    assert!(validate_custom_gas_fee(1, 1).is_ok());
    assert!(validate_custom_gas_fee(0, 0).is_err());
    assert!(validate_custom_gas_fee(1, 2).is_err());
}

#[test]
fn public_address_qr_payload_is_plain_address() {
    let address = Address::from([0xab; 20]);
    let payload = public_address_qr_payload(address);

    assert_eq!(payload, format!("{address:#x}"));
    assert!(!payload.starts_with("ethereum:"));
}

#[test]
fn public_address_qr_payload_fits_qr_with_quiet_zone() {
    let address = Address::from([0x42; 20]);
    let payload = public_address_qr_payload(address);
    let qr = qrcodegen::QrCode::encode_text(&payload, qrcodegen::QrCodeEcc::Medium)
        .expect("public address should fit in QR code");
    let module_range = public_address_qr_module_range(qr.size());

    assert!(qr.size() > 0);
    assert_eq!(PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES, 4);
    assert_eq!(
        module_range.clone().count(),
        usize::try_from(qr.size() + 8).unwrap()
    );
    assert!(module_range.contains(&-PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES));
    assert!(module_range.contains(&qr.size()));
}

#[test]
fn public_account_identicon_pattern_is_deterministic_and_symmetric() {
    let address = Address::from([0x42; 20]);
    let pattern = public_account_identicon_pattern(&address);

    assert_eq!(pattern, public_account_identicon_pattern(&address));
    assert!(pattern.iter().any(|active| *active));
    for row in pattern.chunks_exact(PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE) {
        assert_eq!(row[0], row[4]);
        assert_eq!(row[1], row[3]);
    }
}

#[test]
fn public_account_identicon_differs_for_different_addresses() {
    let first = Address::from([0x11; 20]);
    let second = Address::from([0x22; 20]);

    assert_ne!(
        public_account_identicon_pattern(&first),
        public_account_identicon_pattern(&second),
    );
    assert_ne!(
        public_account_identicon_color(&first),
        public_account_identicon_color(&second),
    );
}

#[test]
fn public_account_identicon_zero_address_is_not_blank() {
    let pattern = public_account_identicon_pattern(&Address::from([0; 20]));
    let active_count = pattern.iter().filter(|active| **active).count();

    assert_eq!(active_count, 1);
    assert!(pattern[PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT / 2]);
}

fn public_balance_snapshot_for_test(chain_id: u64) -> PublicBalanceSnapshot {
    let account = public_account_for_search(Some("Main account"), Address::from([0x11; 20]));
    PublicBalanceSnapshot {
        chain_id,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: vec![PublicAccountBalance {
            account,
            balances: vec![PublicBalanceEntry {
                asset: PublicBalanceAsset {
                    id: PublicAssetId::Native,
                    symbol: "ETH".to_string(),
                    decimals: 18,
                },
                amount: PublicBalanceAmount::Available(U256::from(5_u64)),
            }],
        }],
    }
}

fn public_native_balance_snapshot_for_test(
    chain_id: u64,
    accounts: Vec<(PublicAccountMetadata, PublicBalanceAmount)>,
) -> PublicBalanceSnapshot {
    PublicBalanceSnapshot {
        chain_id,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: accounts
            .into_iter()
            .map(|(account, amount)| PublicAccountBalance {
                account,
                balances: vec![PublicBalanceEntry {
                    asset: PublicBalanceAsset {
                        id: PublicAssetId::Native,
                        symbol: "ETH".to_string(),
                        decimals: 18,
                    },
                    amount,
                }],
            })
            .collect(),
    }
}

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
    assert!(
        public_account_visible_balances_for_chain(
            Some(&snapshot),
            56,
            "public-account",
            PublicAccountStatus::Active,
        )
        .is_empty(),
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
    assert!(
        public_account_visible_balances_for_chain(
            Some(&snapshot),
            1,
            "public-account",
            PublicAccountStatus::Inactive,
        )
        .is_empty(),
    );
}

#[test]
fn public_balance_usd_label_prices_native_and_erc20_balances() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
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
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));

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
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));
    let snapshot = PublicBalanceSnapshot {
        chain_id: 1,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: vec![PublicAccountBalance {
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
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
    let snapshot = PublicBalanceSnapshot {
        chain_id: 1,
        refreshed_at: SystemTime::UNIX_EPOCH,
        accounts: vec![PublicAccountBalance {
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

#[test]
fn public_balance_merge_preserves_other_account_status_group() {
    let active = public_balance_snapshot_for_test(1);
    let mut inactive = public_balance_snapshot_for_test(1);
    inactive.accounts[0].account.public_account_uuid = "inactive-account".to_string();
    inactive.accounts[0].account.status = PublicAccountStatus::Inactive;

    let merged =
        merge_public_balance_snapshot(Some(&active), inactive, PublicAccountStatus::Inactive);

    assert!(merged.accounts.iter().any(|account| {
        account.account.public_account_uuid == "public-account"
            && account.account.status == PublicAccountStatus::Active
    }));
    assert!(merged.accounts.iter().any(|account| {
        account.account.public_account_uuid == "inactive-account"
            && account.account.status == PublicAccountStatus::Inactive
    }));
}

#[test]
fn public_action_native_max_subtracts_estimated_gas_reserve() {
    assert_eq!(
        public_action_max_amount_after_reserve(U256::from(100_u64), U256::from(40_u64)),
        Some(U256::from(60_u64)),
    );
    assert_eq!(
        public_action_max_amount_after_reserve(U256::from(100_u64), U256::from(100_u64)),
        None,
    );
    assert_eq!(
        public_action_max_amount_after_reserve(U256::from(100_u64), U256::from(101_u64)),
        None,
    );
}

#[test]
fn public_action_max_label_notes_native_gas_estimate() {
    let native = PublicBalanceEntry {
        asset: PublicBalanceAsset {
            id: PublicAssetId::Native,
            symbol: "ETH".to_string(),
            decimals: 18,
        },
        amount: PublicBalanceAmount::Available(U256::from(1_000_000_000_000_000_000_u128)),
    };
    let token = PublicBalanceEntry {
        asset: PublicBalanceAsset {
            id: PublicAssetId::Erc20(Address::from([0x22; 20])),
            symbol: "USDC".to_string(),
            decimals: 6,
        },
        amount: PublicBalanceAmount::Available(U256::from(1_500_000_u64)),
    };

    assert_eq!(
        public_action_max_label(&native, Some(U256::from(286_838_782_386_224_461_u128)),),
        Some("0.7132 ETH after est. gas".to_string()),
    );
    assert_eq!(
        public_action_max_label(&token, None),
        Some("1.5 USDC".to_string()),
    );
    assert_eq!(public_action_max_label(&native, None), None);
}

#[test]
fn public_action_progress_steps_use_single_send_step() {
    assert_eq!(
        public_action_progress_steps(PublicActionMode::Send, PublicAssetId::Native),
        vec![PublicActionProgressStep::Send],
    );
}

#[test]
fn public_action_closed_active_step_uses_pending_step() {
    let steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Wrap,
            status: PublicActionStepStatus::Error,
            tx_hash: None,
            message: Some(Arc::from("wrap failed")),
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::Pending,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];

    assert_eq!(
        public_action_closed_active_step(&steps).map(|step| step.step),
        Some(PublicActionProgressStep::Approve),
    );
    assert!(steps.iter().all(|step| step.interval.is_none()));
}

#[test]
fn public_action_closed_active_step_uses_error_without_pending() {
    let steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Wrap,
            status: PublicActionStepStatus::Done,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::Error,
            tx_hash: None,
            message: Some(Arc::from("approve failed")),
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Shield,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];

    assert_eq!(
        public_action_closed_active_step(&steps).map(|step| step.step),
        Some(PublicActionProgressStep::Approve),
    );
}

#[test]
fn public_action_closed_active_step_ignores_inactive_steps() {
    let steps = vec![
        PublicActionStepState {
            step: PublicActionProgressStep::Wrap,
            status: PublicActionStepStatus::Done,
            tx_hash: None,
            message: None,
            interval: None,
        },
        PublicActionStepState {
            step: PublicActionProgressStep::Approve,
            status: PublicActionStepStatus::NotStarted,
            tx_hash: None,
            message: None,
            interval: None,
        },
    ];

    assert!(public_action_closed_active_step(&steps).is_none());
}

#[test]
fn public_action_progress_steps_use_single_transaction_for_native_shield() {
    assert_eq!(
        public_action_progress_steps(PublicActionMode::Shield, PublicAssetId::Native),
        vec![PublicActionProgressStep::Shield],
    );
}

#[test]
fn public_action_progress_steps_skip_wrap_for_erc20_shield() {
    assert_eq!(
        public_action_progress_steps(
            PublicActionMode::Shield,
            PublicAssetId::Erc20(Address::from([0xef; 20])),
        ),
        vec![
            PublicActionProgressStep::Approve,
            PublicActionProgressStep::Shield,
        ],
    );
}

#[test]
fn hardware_public_shield_progress_steps_start_with_shield_key_authorization() {
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Shield,
            PublicAssetId::Erc20(Address::from([0xef; 20])),
            wallet_ops::vault::PublicAccountSource::HardwareDerived,
        ),
        vec![
            PublicActionProgressStep::ShieldKey,
            PublicActionProgressStep::Approve,
            PublicActionProgressStep::Shield,
        ],
    );
    assert_eq!(
        public_action_progress_steps_for_source(
            PublicActionMode::Shield,
            PublicAssetId::Native,
            wallet_ops::vault::PublicAccountSource::HardwareDerived,
        ),
        vec![
            PublicActionProgressStep::ShieldKey,
            PublicActionProgressStep::Shield,
        ],
    );
}

#[test]
fn public_action_error_summary_explains_wrap_gas_estimate() {
    assert_eq!(
        public_action_error_summary(
            PublicActionProgressStep::Wrap,
            Some("public-shield-wrap: estimate gas"),
            "ETH",
        ),
        "Could not estimate gas to wrap ETH. Check amount and gas balance.",
    );
}

#[test]
fn public_action_asset_label_uses_native_symbol() {
    assert_eq!(
        public_action_asset_label(1, PublicAssetId::Native, None),
        "ETH"
    );
}

#[test]
fn public_action_error_details_hide_duplicate_summary() {
    let summary = "Could not send publicly.";

    assert_eq!(public_action_error_details(summary, Some(summary)), None);
    assert_eq!(
        public_action_error_details(summary, Some("public-send: estimate gas")),
        Some("public-send: estimate gas".to_string()),
    );
}

#[test]
fn public_action_error_copy_value_includes_context_and_details() {
    assert_eq!(
        public_action_error_copy_value(
            PublicActionProgressStep::Wrap,
            "ETH",
            "Could not estimate gas to wrap ETH.",
            Some("public-shield-wrap: estimate gas: insufficient funds"),
        ),
        "Step: Wrap\nAsset: ETH\nSummary: Could not estimate gas to wrap ETH.\nDetails: public-shield-wrap: estimate gas: insufficient funds",
    );
}

#[test]
fn send_element_ids_are_asset_scoped() {
    let first = UnshieldAssetKey::new(1, Address::from([0x11; 20]));
    let second = UnshieldAssetKey::new(1, Address::from([0x22; 20]));

    assert_ne!(
        send_element_id(first, "cancel").as_ref(),
        send_element_id(second, "cancel").as_ref()
    );
    assert_ne!(
        send_element_id(first, "copy-to").as_ref(),
        send_element_id(first, "copy-data").as_ref()
    );
}
