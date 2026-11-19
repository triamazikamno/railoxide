use super::*;

#[test]
fn settings_discard_restores_saved_snapshot() {
    let mut saved = WalletSettings::default();
    saved.network.mode = NetworkModeSetting::Direct;
    let mut draft = saved.clone();
    draft.broadcaster.response_timeout_secs += 1;

    assert_ne!(draft, saved);
    assert_eq!(settings_draft_after_discard(&saved), saved);
}

#[test]
fn token_settings_display_includes_built_in_defaults() {
    let settings = WalletSettings::default();
    let entries = display_token_entries(&settings);

    let weth = entries
        .iter()
        .find(|entry| {
            entry.chain_id == 1
                && entry
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default token");
    assert!(weth.built_in);
    assert_eq!(weth.symbol, "WETH");
    assert_eq!(weth.decimals, 18);
}

#[test]
fn price_anchor_settings_display_includes_built_in_defaults() {
    let settings = WalletSettings::default();
    let entries = display_price_anchor_entries(&settings);

    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");
    assert!(weth.built_in_default);
    assert_eq!(
        weth.price_anchor,
        PriceAnchorSettings::Fixed {
            rate: "1000000000000000000".to_string(),
        }
    );
    assert_eq!(
        settings.tokens.price_anchors,
        [] as [TokenPriceAnchorOverride; 0]
    );
}

#[test]
fn price_anchor_settings_display_overrides_built_in_defaults() {
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(TokenPriceAnchorOverride {
            key: TokenKey {
                chain_id: 1,
                token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
            },
            price_anchor: PriceAnchorSettings::Fixed {
                rate: "2000000000000000000".to_string(),
            },
        });

    let entries = display_price_anchor_entries(&settings);
    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH price anchor");

    assert!(!weth.built_in_default);
    assert_eq!(
        weth.price_anchor,
        PriceAnchorSettings::Fixed {
            rate: "2000000000000000000".to_string(),
        }
    );

    settings.tokens.price_anchors.clear();
    let entries = display_price_anchor_entries(&settings);
    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");
    assert!(weth.built_in_default);
}

#[test]
fn price_anchor_view_uses_token_symbol_when_available() {
    let settings = WalletSettings::default();
    let entries = display_price_anchor_entries(&settings);

    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");

    assert_eq!(weth.token_symbol.as_deref(), Some("WETH"));
    assert_eq!(price_anchor_token_primary_label(weth), "WETH");
}

#[test]
fn price_anchor_view_falls_back_to_short_address_without_symbol() {
    let token = Address::from([0x22; 20]);
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(TokenPriceAnchorOverride {
            key: TokenKey {
                chain_id: 1,
                token_address: token.to_string(),
            },
            price_anchor: PriceAnchorSettings::Fixed {
                rate: "1".to_string(),
            },
        });

    let entries = display_price_anchor_entries(&settings);
    let entry = entries
        .iter()
        .find(|entry| {
            entry
                .key
                .token_address
                .eq_ignore_ascii_case(&token.to_string())
        })
        .expect("unknown token price anchor");

    assert_eq!(entry.token_symbol, None);
    assert_eq!(
        price_anchor_token_primary_label(entry),
        railgun_ui::short_address(&token)
    );
}

#[test]
fn price_anchor_edit_prefills_dialog_values() {
    let settings = WalletSettings::default();
    let entries = display_price_anchor_entries(&settings);
    let weth = entries
        .iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");

    let values = price_anchor_dialog_values_from_entry(weth);

    assert_eq!(values.chain_id, 1);
    assert_eq!(values.token_address, weth.key.token_address);
    assert_eq!(values.anchor_type, "fixed");
    assert_eq!(values.fixed_rate, "1000000000000000000");
}

#[test]
fn price_anchor_edit_builtin_default_creates_sparse_override() {
    let mut settings = WalletSettings::default();
    let entry = display_price_anchor_entries(&settings)
        .into_iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH default price anchor");
    let edited = TokenPriceAnchorOverride {
        key: entry.key.clone(),
        price_anchor: PriceAnchorSettings::Fixed {
            rate: "3000000000000000000".to_string(),
        },
    };

    set_price_anchor_override(&mut settings, &entry, edited);

    assert_eq!(settings.tokens.price_anchors.len(), 1);
    let updated = display_price_anchor_entries(&settings)
        .into_iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH edited price anchor");
    assert!(!updated.built_in_default);
    assert_eq!(
        updated.price_anchor,
        PriceAnchorSettings::Fixed {
            rate: "3000000000000000000".to_string(),
        }
    );
}

#[test]
fn price_anchor_edit_override_replaces_existing_override() {
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(TokenPriceAnchorOverride {
            key: TokenKey {
                chain_id: 1,
                token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
            },
            price_anchor: PriceAnchorSettings::Fixed {
                rate: "2000000000000000000".to_string(),
            },
        });
    let entry = display_price_anchor_entries(&settings)
        .into_iter()
        .find(|entry| {
            entry.key.chain_id == 1
                && entry
                    .key
                    .token_address
                    .eq_ignore_ascii_case("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
        })
        .expect("ethereum WETH override price anchor");

    set_price_anchor_override(
        &mut settings,
        &entry,
        TokenPriceAnchorOverride {
            key: entry.key.clone(),
            price_anchor: PriceAnchorSettings::Fixed {
                rate: "4000000000000000000".to_string(),
            },
        },
    );

    assert_eq!(settings.tokens.price_anchors.len(), 1);
    assert_eq!(
        settings.tokens.price_anchors[0].price_anchor,
        PriceAnchorSettings::Fixed {
            rate: "4000000000000000000".to_string(),
        }
    );
}

#[test]
fn price_anchor_add_dialog_values_create_override_without_mutating_settings() {
    let settings = WalletSettings::default();

    let anchor = price_anchor_override_from_dialog_values(&PriceAnchorDialogValues {
        chain_id: 42161,
        token_address: " 0x0000000000000000000000000000000000000002 ".to_string(),
        anchor_type: "oracle",
        fixed_rate: "1000000000000000000".to_string(),
        oracle_chain_id: 1,
        oracle_address: " 0x0000000000000000000000000000000000000003 ".to_string(),
        oracle_token_decimals: "6".to_string(),
        oracle_decimals: "8".to_string(),
        oracle_is_inversed: true,
        twap_pool_address: Address::ZERO.to_string(),
        twap_base_token_address: Address::ZERO.to_string(),
        twap_quote_token_address: Address::ZERO.to_string(),
        twap_base_token_decimals: "18".to_string(),
        twap_window_seconds: "1800".to_string(),
        product_scale_decimals: "18".to_string(),
        product_components: test_product_anchor_components(),
    })
    .expect("valid add price anchor dialog values");

    assert_eq!(
        settings.tokens.price_anchors,
        [] as [TokenPriceAnchorOverride; 0]
    );
    assert_eq!(anchor.key.chain_id, 42161);
    assert_eq!(
        anchor.key.token_address,
        "0x0000000000000000000000000000000000000002"
    );
    assert!(matches!(
        anchor.price_anchor,
        PriceAnchorSettings::Oracle {
            chain_id: 1,
            token_decimals: 6,
            oracle_decimals: 8,
            is_inversed: true,
            ref oracle_address,
            ..
        } if oracle_address == "0x0000000000000000000000000000000000000003"
    ));
}

#[test]
fn price_anchor_twap_dialog_round_trip_preserves_all_fields() {
    let twap = PriceAnchorSettings::UniswapV3Twap {
        pool_address: "0x0000000000000000000000000000000000000400".to_string(),
        base_token_address: "0x0000000000000000000000000000000000000401".to_string(),
        quote_token_address: "0x0000000000000000000000000000000000000402".to_string(),
        base_token_decimals: 8,
        window_seconds: 777,
    };
    let override_settings = TokenPriceAnchorOverride {
        key: TokenKey {
            chain_id: 1,
            token_address: "0x0000000000000000000000000000000000000002".to_string(),
        },
        price_anchor: twap.clone(),
    };
    let values = price_anchor_dialog_values_from_override(&override_settings);
    assert_eq!(values.anchor_type, "uniswap-v3-twap");
    assert_eq!(
        values.twap_pool_address,
        "0x0000000000000000000000000000000000000400"
    );
    assert_eq!(
        values.twap_base_token_address,
        "0x0000000000000000000000000000000000000401"
    );
    assert_eq!(
        values.twap_quote_token_address,
        "0x0000000000000000000000000000000000000402"
    );
    assert_eq!(values.twap_base_token_decimals, "8");
    assert_eq!(values.twap_window_seconds, "777");
    let round_trip = price_anchor_override_from_dialog_values(&values).expect("TWAP round trip");
    assert_eq!(round_trip.price_anchor, twap);
}

#[test]
fn price_anchor_add_dialog_values_create_product_override() {
    let twap_component = PriceAnchorSettings::UniswapV3Twap {
        pool_address: "0x0000000000000000000000000000000000000400".to_string(),
        base_token_address: "0x0000000000000000000000000000000000000401".to_string(),
        quote_token_address: "0x0000000000000000000000000000000000000402".to_string(),
        base_token_decimals: 6,
        window_seconds: 900,
    };
    let twap_component_values = price_anchor_component_dialog_values_from_anchor(&twap_component);
    let anchor = price_anchor_override_from_dialog_values(&PriceAnchorDialogValues {
        chain_id: 1,
        token_address: "0x0000000000000000000000000000000000000002".to_string(),
        anchor_type: "product",
        fixed_rate: "1000000000000000000".to_string(),
        oracle_chain_id: 1,
        oracle_address: "0x0000000000000000000000000000000000000003".to_string(),
        oracle_token_decimals: "18".to_string(),
        oracle_decimals: "8".to_string(),
        oracle_is_inversed: false,
        twap_pool_address: Address::ZERO.to_string(),
        twap_base_token_address: Address::ZERO.to_string(),
        twap_quote_token_address: Address::ZERO.to_string(),
        twap_base_token_decimals: "18".to_string(),
        twap_window_seconds: "1800".to_string(),
        product_scale_decimals: "12".to_string(),
        product_components: vec![
            PriceAnchorComponentDialogValues {
                anchor_type: "oracle",
                fixed_rate: "1000000000000000000".to_string(),
                oracle_chain_id: 42161,
                oracle_address: "0x0000000000000000000000000000000000000004".to_string(),
                oracle_token_decimals: "18".to_string(),
                oracle_decimals: "8".to_string(),
                oracle_is_inversed: false,
                twap_pool_address: Address::ZERO.to_string(),
                twap_base_token_address: Address::ZERO.to_string(),
                twap_quote_token_address: Address::ZERO.to_string(),
                twap_base_token_decimals: "18".to_string(),
                twap_window_seconds: "1800".to_string(),
            },
            twap_component_values,
        ],
    })
    .expect("valid product price anchor dialog values");

    assert!(matches!(
        anchor.price_anchor,
        PriceAnchorSettings::Product {
            scale_decimals: 12,
            ref components,
        } if matches!(
            components.as_slice(),
            [
                PriceAnchorSettings::Oracle {
                    chain_id: 42161,
                    oracle_decimals: 8,
                    is_inversed: false,
                    ..
                },
                PriceAnchorSettings::UniswapV3Twap {
                    pool_address,
                    base_token_address,
                    quote_token_address,
                    base_token_decimals: 6,
                    window_seconds: 900,
                },
            ]
            if pool_address == "0x0000000000000000000000000000000000000400"
                && base_token_address
                    == "0x0000000000000000000000000000000000000401"
                && quote_token_address
                    == "0x0000000000000000000000000000000000000402"
        )
    ));
}

#[test]
fn price_anchor_add_dialog_values_reject_invalid_anchor_type() {
    let err = price_anchor_override_from_dialog_values(&PriceAnchorDialogValues {
        chain_id: 1,
        token_address: "0x0000000000000000000000000000000000000002".to_string(),
        anchor_type: "bad",
        fixed_rate: "1000000000000000000".to_string(),
        oracle_chain_id: 1,
        oracle_address: "0x0000000000000000000000000000000000000003".to_string(),
        oracle_token_decimals: "18".to_string(),
        oracle_decimals: "8".to_string(),
        oracle_is_inversed: false,
        twap_pool_address: Address::ZERO.to_string(),
        twap_base_token_address: Address::ZERO.to_string(),
        twap_quote_token_address: Address::ZERO.to_string(),
        twap_base_token_decimals: "18".to_string(),
        twap_window_seconds: "1800".to_string(),
        product_scale_decimals: "18".to_string(),
        product_components: test_product_anchor_components(),
    })
    .expect_err("bad anchor type rejected");

    assert!(err.contains("fixed, oracle, or product"));
}

fn test_product_anchor_components() -> Vec<PriceAnchorComponentDialogValues> {
    vec![
        PriceAnchorComponentDialogValues {
            anchor_type: "oracle",
            fixed_rate: "1000000000000000000".to_string(),
            oracle_chain_id: 1,
            oracle_address: "0x0000000000000000000000000000000000000003".to_string(),
            oracle_token_decimals: "18".to_string(),
            oracle_decimals: "8".to_string(),
            oracle_is_inversed: false,
            twap_pool_address: Address::ZERO.to_string(),
            twap_base_token_address: Address::ZERO.to_string(),
            twap_quote_token_address: Address::ZERO.to_string(),
            twap_base_token_decimals: "18".to_string(),
            twap_window_seconds: "1800".to_string(),
        },
        PriceAnchorComponentDialogValues {
            anchor_type: "oracle",
            fixed_rate: "1000000000000000000".to_string(),
            oracle_chain_id: 1,
            oracle_address: "0x0000000000000000000000000000000000000004".to_string(),
            oracle_token_decimals: "18".to_string(),
            oracle_decimals: "8".to_string(),
            oracle_is_inversed: true,
            twap_pool_address: Address::ZERO.to_string(),
            twap_base_token_address: Address::ZERO.to_string(),
            twap_quote_token_address: Address::ZERO.to_string(),
            twap_base_token_decimals: "18".to_string(),
            twap_window_seconds: "1800".to_string(),
        },
    ]
}

#[test]
fn token_settings_display_applies_builtin_overrides_and_custom_tokens() {
    let custom = Address::from([0x77; 20]);
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .built_in_overrides
        .push(BuiltInTokenOverride {
            key: TokenKey {
                chain_id: 1,
                token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
            },
            symbol: Some("WETHx".to_string()),
            decimals: Some(17),
            icon_path: None,
            price_anchor: None,
        });
    settings.tokens.custom_tokens.push(CustomTokenSettings {
        chain_id: 1,
        token_address: custom.to_string(),
        symbol: "TST".to_string(),
        decimals: 4,
        icon_path: None,
        price_anchor: None,
    });

    let entries = display_token_entries(&settings);

    let overridden = entries
        .iter()
        .find(|entry| entry.chain_id == 1 && entry.symbol == "WETHx")
        .expect("overridden built-in token");
    assert!(overridden.built_in);
    assert_eq!(overridden.decimals, 17);

    let custom = entries
        .iter()
        .find(|entry| entry.chain_id == 1 && entry.symbol == "TST")
        .expect("custom token");
    assert!(!custom.built_in);
    assert_eq!(custom.decimals, 4);
}
