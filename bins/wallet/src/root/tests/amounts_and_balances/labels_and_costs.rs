use super::*;

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
    assert_eq!(native_wrapped_output_labels(137), Some(("POL", "WPOL")));
    assert_eq!(native_wrapped_output_labels(42161), Some(("ETH", "WETH")));
    assert_eq!(
        native_wrapped_output_labels(11_155_111),
        Some(("ETH", "WETH"))
    );
    assert_eq!(native_wrapped_output_labels(999_999), None);
}

#[test]
fn parse_address_enforces_checksum_only_on_mixed_case() {
    let address = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";
    let expected = address.parse::<Address>().ok();
    for accepted in [
        address,
        "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
        "0xD8DA6BF26964AF9D7EED9E03E53415D37AA96045",
        "0Xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
        "0Xd8da6bf26964af9d7eed9e03e53415d37aa96045",
    ] {
        assert_eq!(parse_address(accepted), expected, "{accepted}");
    }
    // One letter's case flipped: still mixed case, checksum no longer matches.
    assert_eq!(
        parse_address("0xd8DA6BF26964aF9D7eEd9e03E53415D37aA96045"),
        None
    );
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
    assert_eq!(display.action_amount(display.recipient_amount), "3,217 DAI");
    assert_eq!(display.fee_display().fee_amount(), "0.16334 DAI");
}

#[test]
fn public_broadcaster_cost_display_filters_only_redundant_usd_values() {
    let dai = address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");
    let candidate = wallet_ops::public_broadcaster_candidates_for_asset(
        &[fee_row(1, dai, "usd-values")],
        1,
        dai,
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
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
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
        display
            .fee_display()
            .broadcaster_fee_value_with_usd(&breakdown, &cache),
    ];
    assert!(stable_token_values.iter().all(|value| !value.contains('$')));
    assert!(
        stable_token_values
            .iter()
            .all(|value| !value.contains("USD unavailable"))
    );
    assert!(
        display
            .fee_display()
            .native_gas_cost_value_with_usd(&breakdown, &cache)
            .contains('$')
    );

    let outside_peg_cache = TokenAnchorRateCache::new();
    outside_peg_cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    outside_peg_cache.store_rate(1, dai, uint!(1_500_000_000_000_000_000_000_U256));
    assert!(
        display
            .action_amount_with_usd(uint!(2_000_000_000_000_000_000_U256), &outside_peg_cache,)
            .contains('$')
    );
    assert!(
        display
            .fee_display()
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
