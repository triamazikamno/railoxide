use super::*;

#[test]
fn effective_token_registry_applies_overrides_tombstones_and_custom_tokens() {
    let mut settings = WalletSettings::default();
    let weth = crate::settings::TokenKey {
        chain_id: 1,
        token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
    };
    settings
        .tokens
        .built_in_overrides
        .push(crate::settings::BuiltInTokenOverride {
            key: weth,
            symbol: Some("WETHx".to_string()),
            decimals: Some(18),
            icon_path: None,
            price_anchor: Some(crate::settings::PriceAnchorSettings::Fixed {
                rate: "2000000000000000000".to_string(),
            }),
        });
    settings
        .tokens
        .built_in_tombstones
        .push(crate::settings::TokenKey {
            chain_id: 1,
            token_address: "0xdAC17F958D2ee523a2206206994597C13D831ec7".to_string(),
        });
    settings
        .tokens
        .custom_tokens
        .push(crate::settings::CustomTokenSettings {
            chain_id: 1,
            token_address: "0x0000000000000000000000000000000000000002".to_string(),
            symbol: "CSTM".to_string(),
            decimals: 9,
            icon_path: None,
            price_anchor: None,
        });

    let registry = build_effective_token_registry(&settings).expect("build token registry");
    let weth = registry
        .tokens
        .get(&(1, "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string()))
        .expect("weth token");
    assert_eq!(weth.symbol, "WETHx");
    assert!(weth.price_anchor.is_some());
    assert!(
        !registry
            .tokens
            .contains_key(&(1, "0xdac17f958d2ee523a2206206994597c13d831ec7".to_string()))
    );
    let custom = registry
        .tokens
        .get(&(1, "0x0000000000000000000000000000000000000002".to_string()))
        .expect("custom token");
    assert!(!custom.built_in);
    assert_eq!(custom.decimals, 9);
}

#[test]
fn tron_tokens_join_the_registry_while_unknown_evm_chains_stay_rejected() {
    let tron = crate::tron::TRON_CHAIN_ID;
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .custom_tokens
        .push(crate::settings::CustomTokenSettings {
            chain_id: tron,
            token_address: "0x0000000000000000000000000000000000000003".to_string(),
            symbol: "TCST".to_string(),
            decimals: 6,
            ..crate::settings::CustomTokenSettings::default()
        });

    let registry = build_effective_token_registry(&settings).expect("Tron custom token is valid");
    let custom = registry
        .tokens
        .get(&(
            tron,
            "0x0000000000000000000000000000000000000003".to_string(),
        ))
        .expect("custom Tron token");
    assert!(!custom.built_in);
    let usdt = registry
        .tokens
        .get(&(
            tron,
            "0xa614f803b6fd780986a42c78ec9c7f77e6ded13c".to_string(),
        ))
        .expect("built-in Tron USDT");
    assert_eq!(usdt.symbol, "USDT");

    settings.tokens.custom_tokens[0].chain_id = 999_999_999;
    let err = build_effective_token_registry(&settings).expect_err("unknown chain rejected");
    assert!(
        err.messages
            .iter()
            .any(|message| message == "tokens.custom_tokens[0].chain_id is not supported")
    );
}

#[test]
fn effective_token_registry_includes_static_price_anchor_defaults() {
    let settings = WalletSettings::default();

    let registry = build_effective_token_registry(&settings).expect("build token registry");

    let weth = registry
        .tokens
        .get(&(1, "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string()))
        .expect("weth token");
    assert_eq!(
        weth.price_anchor,
        Some(crate::settings::PriceAnchorSettings::Fixed {
            rate: "1000000000000000000".to_string(),
        })
    );

    let usdt = registry
        .tokens
        .get(&(1, "0xdac17f958d2ee523a2206206994597c13d831ec7".to_string()))
        .expect("usdt token");
    assert!(matches!(
        usdt.price_anchor,
        Some(crate::settings::PriceAnchorSettings::Oracle {
            chain_id: 1,
            token_decimals: 6,
            oracle_decimals: 8,
            is_inversed: false,
            ..
        })
    ));
}

#[test]
fn broadcaster_settings_build_fee_policy_and_validate_thresholds() {
    let mut settings = WalletSettings::default();
    settings.broadcaster.min_anchor_bps = 9_000;
    settings.broadcaster.max_anchor_bps = 11_000;
    settings
        .broadcaster
        .allow_suspicious_broadcasters_by_default = true;
    settings.broadcaster.response_timeout_secs = 45;

    settings.validate().expect("broadcaster settings valid");
    let policy = settings.broadcaster.fee_policy();
    assert_eq!(policy.min_anchor_bps, 9_000);
    assert_eq!(policy.max_anchor_bps, 11_000);
    assert!(policy.allow_suspicious_broadcasters);

    settings.broadcaster.min_anchor_bps = 12_000;
    let err = settings
        .validate()
        .expect_err("invalid thresholds rejected");
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("min_anchor_bps"))
    );
}

#[test]
fn price_anchor_validation_covers_oracle_and_product_metadata() {
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(crate::settings::TokenPriceAnchorOverride {
            key: crate::settings::TokenKey {
                chain_id: 1,
                token_address: "0x0000000000000000000000000000000000000002".to_string(),
            },
            price_anchor: crate::settings::PriceAnchorSettings::Product {
                scale_decimals: 18,
                components: vec![crate::settings::PriceAnchorSettings::Oracle {
                    chain_id: 1,
                    oracle_address: "0x0000000000000000000000000000000000000003".to_string(),
                    token_decimals: 18,
                    oracle_decimals: 8,
                    is_inversed: false,
                }],
            },
        });

    settings.validate().expect("anchor metadata valid");

    let crate::settings::PriceAnchorSettings::Product { components, .. } = &mut settings
        .tokens
        .price_anchors
        .first_mut()
        .expect("price anchor")
        .price_anchor
    else {
        panic!("expected product anchor");
    };
    let crate::settings::PriceAnchorSettings::Oracle {
        oracle_decimals, ..
    } = &mut components[0]
    else {
        panic!("expected oracle anchor");
    };
    *oracle_decimals = 37;

    let err = settings
        .validate()
        .expect_err("bad oracle decimals rejected");
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("oracle_decimals"))
    );
}

#[test]
fn uniswap_v3_twap_settings_round_trip_and_validate_boundaries() {
    let anchor = crate::settings::PriceAnchorSettings::UniswapV3Twap {
        pool_address: "0x0000000000000000000000000000000000000400".to_string(),
        base_token_address: "0x0000000000000000000000000000000000000401".to_string(),
        quote_token_address: "0x0000000000000000000000000000000000000402".to_string(),
        base_token_decimals: 18,
        window_seconds: 1_800,
    };
    let wire = serde_json::to_value(&anchor).expect("serialize TWAP settings");
    assert_eq!(wire["type"], json!("uniswap-v3-twap"));
    assert!(wire.get("chain_id").is_none());
    assert_eq!(
        wire["pool_address"],
        json!("0x0000000000000000000000000000000000000400")
    );
    assert_eq!(
        serde_json::from_value::<crate::settings::PriceAnchorSettings>(wire).expect("deserialize"),
        anchor
    );

    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(crate::settings::TokenPriceAnchorOverride {
            key: crate::settings::TokenKey {
                chain_id: 1,
                token_address: "0x0000000000000000000000000000000000000002".to_string(),
            },
            price_anchor: anchor.clone(),
        });
    settings.validate().expect("supported TWAP accepted");
    let mut invalid = anchor;
    let crate::settings::PriceAnchorSettings::UniswapV3Twap {
        pool_address,
        base_token_address,
        quote_token_address,
        base_token_decimals,
        window_seconds,
    } = &mut invalid
    else {
        unreachable!();
    };
    *pool_address = "not-an-address".to_string();
    *base_token_address = "not-an-address".to_string();
    *quote_token_address = "not-an-address".to_string();
    *base_token_decimals = 37;
    *window_seconds = 0;
    settings.tokens.price_anchors[0].price_anchor = invalid;
    let error = settings.validate().expect_err("invalid TWAP rejected");
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("pool_address"))
    );
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("base_token_address"))
    );
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("quote_token_address"))
    );
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("base_token_decimals"))
    );
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("window_seconds"))
    );
}

#[test]
fn legacy_anchor_settings_remain_compatible_and_rail_twap_override_resets() {
    let legacy = vec![
        json!({"type":"fixed", "rate":"1000000000000000000"}),
        json!({"type":"oracle", "chain_id":1, "oracle_address":"0x0000000000000000000000000000000000000003", "token_decimals":18, "oracle_decimals":8, "is_inversed":false}),
        json!({"type":"product", "components":[{"type":"fixed", "rate":"2"}], "scale_decimals":18}),
        json!({"type":"uniswap-v3-twap", "chain_id":31337, "pool_address":"0x0000000000000000000000000000000000000400", "base_token_address":"0x0000000000000000000000000000000000000401", "quote_token_address":"0x0000000000000000000000000000000000000402", "base_token_decimals":18, "window_seconds":1800}),
    ];
    for value in legacy {
        serde_json::from_value::<crate::settings::PriceAnchorSettings>(value)
            .expect("legacy anchor");
    }
    let rail = crate::settings::TokenKey {
        chain_id: 1,
        token_address: "0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D".to_string(),
    };
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(crate::settings::TokenPriceAnchorOverride {
            key: rail.clone(),
            price_anchor: crate::settings::PriceAnchorSettings::Fixed {
                rate: "7".to_string(),
            },
        });
    let registry = build_effective_token_registry(&settings).expect("override registry");
    assert!(
        matches!(registry.tokens.get(&(1, rail.token_address.to_ascii_lowercase())).and_then(|token| token.price_anchor.as_ref()), Some(crate::settings::PriceAnchorSettings::Fixed { rate }) if rate == "7")
    );
    settings.tokens.price_anchors.clear();
    let registry = build_effective_token_registry(&settings).expect("reset registry");
    assert!(matches!(
        registry
            .tokens
            .get(&(1, rail.token_address.to_ascii_lowercase()))
            .and_then(|token| token.price_anchor.as_ref()),
        Some(crate::settings::PriceAnchorSettings::UniswapV3Twap {
            window_seconds: 1_800,
            ..
        })
    ));
}
