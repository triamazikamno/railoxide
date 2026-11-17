use super::*;

#[test]
fn format_handles_zero_decimals() {
    assert_eq!(format_token_amount(uint!(123_U256), 0), "123");
}

#[test]
fn format_inclusive_thresholds_pick_expected_precision() {
    assert_eq!(display_precision(uint!(100_000_000_U256), 6), 0);
    assert_eq!(display_precision(uint!(1_000_000_U256), 6), 2);
    assert_eq!(display_precision(uint!(500_000_U256), 6), 4);
    assert_eq!(display_precision(uint!(100_000_U256), 6), 5);
    assert_eq!(display_precision(uint!(99_999_U256), 6), 6);
    assert_eq!(display_precision(uint!(99_999_999_U256), 6), 2);
}

#[test]
fn format_trims_trailing_zeros_after_rounding() {
    assert_eq!(format_token_amount(uint!(1_000_000_U256), 6), "1");
    assert_eq!(format_token_amount(uint!(1_500_000_U256), 6), "1.5");
}

#[test]
fn format_rounds_large_values_to_whole_numbers() {
    assert_eq!(
        format_token_amount(uint!(19_232_527_572_893_U256), 9),
        "19233"
    );
}

#[test]
fn format_uses_two_decimals_between_one_and_hundred() {
    assert_eq!(format_token_amount(uint!(12_345_600_U256), 6), "12.35");
}

#[test]
fn format_uses_four_decimals_between_half_and_one() {
    assert_eq!(format_token_amount(uint!(543_250_U256), 6), "0.5433");
}

#[test]
fn format_uses_five_decimals_between_tenth_and_half() {
    assert_eq!(format_token_amount(uint!(123_456_789_U256), 9), "0.12346");
}

#[test]
fn format_uses_six_decimals_below_tenth() {
    assert_eq!(format_token_amount(uint!(12_345_U256), 6), "0.012345");
}

#[test]
fn precision_caps_to_available_token_decimals() {
    assert_eq!(display_precision(uint!(54_U256), 2), 2);
    assert_eq!(format_token_amount(uint!(54_U256), 2), "0.54");
}

#[test]
fn format_zero_amount() {
    assert_eq!(format_token_amount(U256::ZERO, 18), "0");
    assert_eq!(format_token_amount(U256::ZERO, 0), "0");
}

#[test]
fn format_upper_bound_never_rounds_below_raw_value() {
    assert_eq!(format_token_amount_ceiling(uint!(1_004_U256), 1), "101");
    assert_eq!(format_token_amount_ceiling(uint!(12_341_U256), 3), "12.35");
    assert_eq!(format_token_amount_ceiling(uint!(12_340_U256), 3), "12.34");
    assert_eq!(format_token_amount_ceiling(uint!(123_U256), 0), "123");
}

#[test]
fn lookup_hits_ethereum_weth() {
    let addr = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let info = lookup_token(1, &addr).expect("WETH on Ethereum should be known");
    assert_eq!(info.symbol, "WETH");
    assert_eq!(info.decimals, 18);
    assert_eq!(
        info.anchor_sources,
        &[TokenAnchorSource::Fixed {
            token_fee_per_unit_gas: WRAPPED_NATIVE_FEE_RATE,
        }]
    );
    assert!(token_icon_path(1, &addr).is_some_and(|path| {
        path.ends_with("assets/tokens/1-0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2.png")
    }));
}

#[test]
fn lookup_exposes_oracle_anchor_sources() {
    let addr = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let info = lookup_token(1, &addr).expect("USDC on Ethereum should be known");

    assert_eq!(info.symbol, "USDC");
    assert_eq!(info.decimals, 6);
    assert_eq!(
        info.anchor_sources,
        &[TokenAnchorSource::ChainlinkOracle {
            addr: address!("0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419"),
            token_decimals: 6,
            oracle_decimals: 8,
            is_inversed: false,
        }]
    );
}

#[test]
fn usd_value_helpers_price_tokens_and_native_assets() {
    let native_usd = uint!(3_000_000_000_U256);

    assert_eq!(
        token_usd_micro_value(uint!(1_500_000_U256), uint!(3_000_000_000_U256), native_usd),
        Some(uint!(1_500_000_U256))
    );
    assert_eq!(
        token_usd_micro_value(
            uint!(500_000_000_000_000_000_U256),
            WRAPPED_NATIVE_FEE_RATE,
            native_usd,
        ),
        Some(uint!(1_500_000_000_U256))
    );
    assert_eq!(
        native_usd_micro_value(uint!(2_000_000_000_000_000_000_U256), native_usd, 18),
        Some(uint!(6_000_000_000_U256))
    );
}

#[test]
fn usd_value_helpers_skip_missing_or_invalid_rates() {
    assert_eq!(
        token_usd_micro_value(uint!(1_U256), U256::ZERO, uint!(1_U256)),
        None
    );
    assert_eq!(
        token_usd_micro_value(uint!(1_U256), uint!(1_U256), U256::ZERO),
        None
    );
    assert_eq!(
        token_usd_micro_value(U256::MAX, uint!(1_U256), uint!(2_U256)),
        None
    );
}

#[test]
fn redundant_usd_value_filter_uses_inclusive_two_percent_bounds() {
    let amount = uint!(100_000_000_U256);

    assert_eq!(
        non_redundant_usd_micro_value(amount, 6, uint!(98_000_000_U256)),
        None
    );
    assert_eq!(
        non_redundant_usd_micro_value(amount, 6, uint!(102_000_000_U256)),
        None
    );
    assert_eq!(
        non_redundant_usd_micro_value(amount, 6, uint!(97_999_999_U256)),
        Some(uint!(97_999_999_U256))
    );
    assert_eq!(
        non_redundant_usd_micro_value(amount, 6, uint!(102_000_001_U256)),
        Some(uint!(102_000_001_U256))
    );
}

#[test]
fn redundant_usd_value_filter_handles_decimals_dust_and_overflow() {
    assert_eq!(
        non_redundant_usd_micro_value(
            uint!(2_000_000_000_000_000_000_U256),
            18,
            uint!(2_000_000_U256),
        ),
        None
    );
    assert_eq!(
        non_redundant_usd_micro_value(uint!(1_U256), 18, U256::ZERO),
        Some(U256::ZERO)
    );
    assert_eq!(
        non_redundant_usd_micro_value(U256::MAX, 18, U256::MAX),
        Some(U256::MAX)
    );
}

#[test]
fn usd_value_formatter_rounds_to_cents() {
    assert_eq!(format_usd_micro_value(U256::ZERO), "$0.00");
    assert_eq!(format_usd_micro_value(uint!(12_344_U256)), "$0.01");
    assert_eq!(format_usd_micro_value(uint!(12_345_U256)), "$0.01");
    assert_eq!(format_usd_micro_value(uint!(123_454_999_U256)), "$123.45");
    assert_eq!(format_usd_micro_value(uint!(123_455_000_U256)), "$123.46");
    assert_eq!(
        format_usd_micro_value(uint!(1_234_560_000_U256)),
        "$1,234.56"
    );
    assert_eq!(
        format_usd_micro_value(uint!(12_345_678_900_000_U256)),
        "$12,345,678.90"
    );
}

#[test]
fn lookup_rail_exposes_builtin_uniswap_v3_twap_source() {
    let addr = address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D");
    let info = lookup_token(1, &addr).expect("RAIL on Ethereum should be known");

    assert_eq!(info.symbol, "RAIL");
    assert_eq!(info.decimals, 18);
    assert_eq!(
        info.anchor_sources,
        &[TokenAnchorSource::UniswapV3Twap {
            pool: address!("0x2837809FD68e4a4104af76bbec5b622b6146B2cb"),
            base_token: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            quote_token: addr,
            base_token_decimals: 18,
            window_seconds: 1_800,
        }]
    );
}

#[test]
fn lookup_exposes_composite_anchor_sources() {
    let addr = address!("0x912ce59144191c1204e64559fe8253a0e49e6548");
    let info = lookup_token(42161, &addr).expect("ARB on Arbitrum should be known");

    assert_eq!(info.symbol, "ARB");
    assert_eq!(info.decimals, 18);
    let [
        TokenAnchorSource::Product {
            sources,
            scale_decimals,
        },
    ] = info.anchor_sources
    else {
        panic!("ARB should use a composite anchor source");
    };
    assert_eq!(*scale_decimals, 18);
    assert_eq!(sources.len(), 2);
}

#[test]
fn lookup_disambiguates_native_usdc_across_chains() {
    // Native Arbitrum USDC uses 6 token decimals in the example config.
    let arb_usdc = address!("0xaf88d065e77c8cc2239327c5edb3a432268e5831");
    let info = lookup_token(42161, &arb_usdc).expect("Arbitrum USDC present");
    assert_eq!(info.symbol, "USDC");
    assert_eq!(info.decimals, 6);

    // Same chain_id with a different address should miss.
    let bogus = address!("0x0000000000000000000000000000000000000001");
    assert!(lookup_token(42161, &bogus).is_none());
}

#[test]
fn tron_registry_lists_its_four_tokens() {
    let symbols = known_tokens_for_chain(TRON_CHAIN_ID)
        .map(|token| token.symbol)
        .collect::<Vec<_>>();
    assert_eq!(symbols, ["USDT", "USDD", "USD1", "WTRX"]);
    let usdt = lookup_token(
        TRON_CHAIN_ID,
        &address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c"),
    )
    .expect("Tron USDT is known");
    assert_eq!(usdt.decimals, 6);
    assert!(!crate::chains::is_built_in_chain(TRON_CHAIN_ID));

    // WTRX is 1:1 with 6-decimal TRX, so its anchor is one whole TRX in raw units.
    let wtrx = lookup_token(
        TRON_CHAIN_ID,
        &address!("0x891cdb91d149f23b1a45d9c5ca78a88d0cb44c18"),
    )
    .expect("Tron WTRX is known");
    let [
        TokenAnchorSource::Fixed {
            token_fee_per_unit_gas,
        },
    ] = wtrx.anchor_sources
    else {
        panic!("WTRX has one fixed anchor");
    };
    let trx_usd_micro = uint!(250_000_U256);
    let one_trx = uint!(1_000_000_U256);
    assert_eq!(
        token_usd_micro_value(one_trx, *token_fee_per_unit_gas, trx_usd_micro),
        native_usd_micro_value(one_trx, trx_usd_micro, 6),
    );
    assert_eq!(
        native_usd_micro_value(one_trx, trx_usd_micro, 6),
        Some(trx_usd_micro)
    );
}

#[test]
fn lookup_misses_unknown_chain() {
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    // This chain has no built-in token metadata.
    assert!(lookup_token(u64::MAX, &weth).is_none());
    assert_eq!(token_icon_path(u64::MAX, &weth), None);
}

#[test]
fn token_icon_path_uses_svg_for_vendored_svg_icons() {
    let fxusd = address!("0x085780639CC2cACd35E474e71f4d000e2405d8f6");
    assert!(token_icon_path(1, &fxusd).is_some_and(|path| {
        path.ends_with("assets/tokens/1-0x085780639cc2cacd35e474e71f4d000e2405d8f6.svg")
    }));

    let tusd = address!("0x4D15a3A2286D883AF0AA1B3f21367843FAc63E07");
    assert!(token_icon_path(42161, &tusd).is_some_and(|path| {
        path.ends_with("assets/tokens/42161-0x4d15a3a2286d883af0aa1b3f21367843fac63e07.svg")
    }));
}

#[test]
fn every_token_row_resolves_its_vendored_icon() {
    for (chain_id, token, symbol, _, _, icon) in TOKENS {
        let path = token_icon_path(*chain_id, token);
        match icon {
            TokenIcon::Own | TokenIcon::Shared(_) => {
                let path = path.unwrap_or_else(|| {
                    panic!("{symbol} on chain {chain_id} should resolve an icon path")
                });
                assert!(
                    path.exists(),
                    "{symbol} on chain {chain_id} is missing {}",
                    path.display()
                );
            }
            TokenIcon::None => assert_eq!(
                path, None,
                "{symbol} on chain {chain_id} should not resolve an icon path"
            ),
        }
    }
}

#[test]
fn token_rows_are_unique_per_chain_and_address() {
    let mut seen = std::collections::HashSet::new();
    for (chain_id, token, symbol, _, _, _) in TOKENS {
        // `{token:#x}` is lower-case hex, so this compares addresses case-insensitively.
        assert!(
            seen.insert((*chain_id, format!("{token:#x}"))),
            "{symbol} duplicates an existing row on chain {chain_id}"
        );
    }
}

#[test]
fn short_address_preserves_prefix_and_suffix() {
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    assert_eq!(short_address(&weth), "0xc02a…6cc2");
}

#[test]
fn broadcaster_address_label_matches_monitor_style() {
    let address = "0zk1abcdefghijklmnopqrstuvwxyz";
    assert_eq!(
        format_broadcaster_address_label(address, None),
        "0zk...wxyz"
    );
    assert_eq!(
        format_broadcaster_address_label(address, Some("node")),
        "0zk...wxyz (node)"
    );
}

#[test]
fn chain_name_covers_default_set_and_misses_others() {
    use crate::chains::chain_name;

    assert_eq!(chain_name(1), Some("Ethereum"));
    assert_eq!(chain_name(56), Some("BSC"));
    assert_eq!(chain_name(137), Some("Polygon"));
    assert_eq!(chain_name(42161), Some("Arbitrum"));
    assert_eq!(chain_name(8453), Some("Base"));
    assert_eq!(chain_name(0), None);
}

#[test]
fn chain_icon_path_covers_default_set_and_misses_others() {
    use crate::chains::chain_icon_path;

    assert!(chain_icon_path(1).is_some_and(|path| path.ends_with("assets/chains/ethereum.svg")));
    assert!(chain_icon_path(56).is_some_and(|path| path.ends_with("assets/chains/bsc.svg")));
    assert!(chain_icon_path(137).is_some_and(|path| path.ends_with("assets/chains/polygon.svg")));
    assert!(
        chain_icon_path(42161).is_some_and(|path| path.ends_with("assets/chains/arbitrum.svg"))
    );
    assert_eq!(chain_icon_path(0), None);
    for chain_id in crate::chains::built_in_chain_ids() {
        assert!(
            chain_icon_path(chain_id).is_some_and(|path| path.exists()),
            "built-in chain {chain_id} ships an icon file"
        );
    }
}
