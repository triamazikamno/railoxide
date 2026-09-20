use super::*;

#[test]
fn fee_token_options_use_poi_spendable_balances_and_broadcaster_counts() {
    let token_a = Address::from([0x11; 20]);
    let token_b = Address::from([0x22; 20]);
    let token_c = Address::from([0x33; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            unshield_utxo_output(token_a, 5, 0, 1),
            unshield_utxo_output(token_b, 7, 0, 2),
        ],
        totals: vec![
            wallet_ops::TokenTotal {
                token: token_a.to_checksum(None),
                total: "5".to_string(),
                poi_verified_total: "5".to_string(),
            },
            wallet_ops::TokenTotal {
                token: token_b.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
            wallet_ops::TokenTotal {
                token: token_c.to_checksum(None),
                total: "9".to_string(),
                poi_verified_total: "0".to_string(),
            },
        ],
    };
    let fee_rows = vec![fee_row(1, token_a, "token-a")];

    let options = public_broadcaster_fee_token_options_from_snapshot(
        &snapshot,
        &fee_rows,
        None,
        None,
        BroadcasterFeePolicy::default(),
        &wallet_ops::PublicBroadcasterTrustFilter::default(),
        None,
        |_| None,
    );

    assert_eq!(options.len(), 2);
    let option_a = options
        .iter()
        .find(|option| option.token == token_a)
        .expect("token a option");
    assert_eq!(option_a.max_spendable, uint!(5_U256));
    assert_eq!(option_a.eligible_broadcaster_count, 1);
    let option_b = options
        .iter()
        .find(|option| option.token == token_b)
        .expect("token b option");
    assert_eq!(option_b.max_spendable, uint!(7_U256));
    assert_eq!(option_b.eligible_broadcaster_count, 0);
    assert!(!options.iter().any(|option| option.token == token_c));
}

#[test]
fn fee_token_options_use_fee_only_transaction_spend_limit() {
    let token = Address::from([0x34; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 20,
        unspent_count: 20,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: (0..20)
            .map(|position| unshield_utxo_output(token, 1, 0, position))
            .collect(),
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "20".to_string(),
            poi_verified_total: "20".to_string(),
        }],
    };
    let fee_rows = vec![fee_row(1, token, "token")];

    let options = public_broadcaster_fee_token_options_from_snapshot(
        &snapshot,
        &fee_rows,
        None,
        None,
        BroadcasterFeePolicy::default(),
        &wallet_ops::PublicBroadcasterTrustFilter::default(),
        None,
        |_| None,
    );

    assert_eq!(options.len(), 1);
    assert_eq!(options[0].max_spendable, uint!(13_U256));
}

#[test]
fn fee_token_options_include_known_token_icons() {
    let token = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        .parse::<Address>()
        .expect("usdc address");
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![unshield_utxo_output(token, 1, 0, 1)],
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "1".to_string(),
            poi_verified_total: "1".to_string(),
        }],
    };
    let fee_rows = vec![fee_row(1, token, "usdc")];

    let options = public_broadcaster_fee_token_options_from_snapshot(
        &snapshot,
        &fee_rows,
        None,
        None,
        BroadcasterFeePolicy::default(),
        &wallet_ops::PublicBroadcasterTrustFilter::default(),
        None,
        |_| None,
    );

    assert_eq!(options.len(), 1);
    assert!(options[0].icon_path.is_some());
}

#[test]
fn ethereum_weth_public_broadcaster_count_filters_available_broadcasters() {
    let weth = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse::<Address>()
        .expect("weth address");
    let other_token = Address::from([0x77; 20]);
    let mut unavailable = fee_row(1, weth, "unavailable");
    unavailable.available_wallets = 0;
    let mut expired = fee_row(1, weth, "expired");
    expired.fee_expiration = SystemTime::now() - Duration::from_secs(1);
    let mut invalid_signature = fee_row(1, weth, "invalid-signature");
    invalid_signature.signature_valid = false;
    let rows = vec![
        fee_row(1, weth, "available-weth"),
        fee_row(42161, weth, "wrong-chain"),
        fee_row(1, other_token, "wrong-token"),
        unavailable,
        expired,
        invalid_signature,
    ];

    assert_eq!(ethereum_weth_public_broadcaster_count(&rows), 1);
}

#[test]
fn ethereum_weth_public_broadcaster_count_is_zero_without_available_broadcasters() {
    let weth = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse::<Address>()
        .expect("weth address");
    let mut unavailable = fee_row(1, weth, "unavailable");
    unavailable.available_wallets = 0;

    assert_eq!(ethereum_weth_public_broadcaster_count(&[]), 0);
    assert_eq!(ethereum_weth_public_broadcaster_count(&[unavailable]), 0);
}

#[test]
fn fee_token_options_and_selection_follow_execution_route() {
    let token = Address::from([0x39; 20]);
    let required_relay = Address::from([0x40; 20]);
    let other_relay = Address::from([0x41; 20]);
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 20,
        unspent_count: 20,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: (0..20)
            .map(|position| unshield_utxo_output(token, 1, 0, position))
            .collect(),
        totals: vec![wallet_ops::TokenTotal {
            token: token.to_checksum(None),
            total: "20".to_string(),
            poi_verified_total: "20".to_string(),
        }],
    };
    let mut row = fee_row(1, token, "custom-relay");
    row.relay_adapt = required_relay;

    let matching = public_broadcaster_fee_token_options_from_snapshot(
        &snapshot,
        &[row.clone()],
        Some(required_relay),
        None,
        BroadcasterFeePolicy::default(),
        &wallet_ops::PublicBroadcasterTrustFilter::default(),
        None,
        |_| None,
    );
    let mismatched = public_broadcaster_fee_token_options_from_snapshot(
        &snapshot,
        &[row.clone()],
        Some(other_relay),
        None,
        BroadcasterFeePolicy::default(),
        &wallet_ops::PublicBroadcasterTrustFilter::default(),
        None,
        |_| None,
    );

    assert_eq!(matching[0].eligible_broadcaster_count, 1);
    assert_eq!(mismatched[0].eligible_broadcaster_count, 0);

    let profile = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .unwrap()
        .accepted_executor_profile()
        .unwrap();
    let trust = wallet_ops::PublicBroadcasterTrustFilter::default();
    for (version, delegate, expected_count) in [
        ("8.2.3", Some(profile.delegate()), 1),
        ("8.4.0", Some(profile.delegate()), 1),
        ("9.0.0", Some(profile.delegate()), 0),
        ("8.2.3", None, 0),
        ("8.2.3", Some(other_relay), 0),
    ] {
        row.version = Arc::from(version);
        row.relay_adapt_7702 = delegate;
        let rows = [row.clone()];
        let policy = BroadcasterFeePolicy::default();
        let options = public_broadcaster_fee_token_options_from_snapshot(
            &snapshot,
            &rows,
            Some(other_relay),
            Some(profile),
            policy,
            &trust,
            None,
            |_| None,
        );
        let candidates = super::super::public_broadcaster::public_broadcaster_candidates_for_route(
            &rows,
            1,
            token,
            Some(other_relay),
            Some(profile),
            policy,
            None,
            &trust,
        );
        assert_eq!(options[0].eligible_broadcaster_count, expected_count);
        assert_eq!(candidates.len(), expected_count);
        assert_eq!(
            public_broadcaster_submit_disabled_for_fee_token_options(&options, token),
            expected_count == 0,
        );
    }
}

#[test]
fn effective_chain_overrides_drive_unwrap_ui_filters() {
    let relay = Address::from([0x42; 20]);
    let wrapped = Address::from([0x43; 20]);
    let other = Address::from([0x44; 20]);
    let mut settings = WalletSettings::default();
    let chain = settings.chains.per_chain.entry(1).or_default();
    chain.railgun.contracts.relay_adapt_contract = Some(relay.to_string());
    chain.contracts.wrapped_native_token = Some(wrapped.to_string());
    let configs = build_effective_chain_configs(&settings).expect("effective chains");

    assert_eq!(
        required_relay_adapt_for_unshield(&configs, 1, true, false),
        Some(relay)
    );
    assert_eq!(
        required_relay_adapt_for_unshield(&configs, 1, false, true),
        Some(relay)
    );
    assert_eq!(
        required_relay_adapt_for_unshield(&configs, 1, false, false),
        None
    );
    assert!(is_effective_wrapped_native_token(&configs, 1, wrapped));
    assert!(!is_effective_wrapped_native_token(&configs, 1, other));
}

#[test]
fn fee_token_resolution_prefers_current_then_action_then_first_eligible() {
    let action = Address::from([0x44; 20]);
    let current = Address::from([0x45; 20]);
    let fallback = Address::from([0x46; 20]);
    let option = |token, count| PublicBroadcasterFeeTokenOption {
        token,
        label: format!("token-{count}"),
        decimals: None,
        max_spendable: U256::from(1),
        eligible_broadcaster_count: count,
        icon_path: None,
    };

    assert_eq!(
        resolve_selected_public_broadcaster_fee_token(
            current,
            action,
            &[option(current, 1), option(action, 1)],
        ),
        current
    );
    assert_eq!(
        resolve_selected_public_broadcaster_fee_token(
            current,
            action,
            &[option(current, 0), option(action, 1), option(fallback, 1)],
        ),
        action
    );
    assert_eq!(
        resolve_selected_public_broadcaster_fee_token(
            current,
            action,
            &[option(current, 0), option(action, 0), option(fallback, 1)],
        ),
        fallback
    );
}

#[test]
fn fee_token_submit_state_requires_selected_token_broadcaster_count() {
    let selected = Address::from([0x51; 20]);
    let other = Address::from([0x52; 20]);
    let options = vec![
        PublicBroadcasterFeeTokenOption {
            token: selected,
            label: "selected".to_string(),
            decimals: None,
            max_spendable: U256::from(1),
            eligible_broadcaster_count: 0,
            icon_path: None,
        },
        PublicBroadcasterFeeTokenOption {
            token: other,
            label: "other".to_string(),
            decimals: None,
            max_spendable: U256::from(1),
            eligible_broadcaster_count: 1,
            icon_path: None,
        },
    ];

    assert!(!fee_token_option_has_eligible_broadcaster(
        &options, selected
    ));
    assert!(fee_token_option_has_eligible_broadcaster(&options, other));
    assert!(public_broadcaster_submit_disabled_for_fee_token_options(
        &options, selected
    ));
    assert!(!public_broadcaster_submit_disabled_for_fee_token_options(
        &options, other
    ));
}

#[test]
fn fee_token_warning_distinguishes_empty_broadcaster_monitor() {
    let selected = Address::from([0x51; 20]);
    let options = vec![PublicBroadcasterFeeTokenOption {
        token: selected,
        label: "selected".to_string(),
        decimals: None,
        max_spendable: U256::from(1),
        eligible_broadcaster_count: 0,
        icon_path: None,
    }];
    let trust_filter = wallet_ops::PublicBroadcasterTrustFilter::default();

    assert_eq!(
        public_broadcaster_fee_token_warning(&[], 1, &options, selected, &trust_filter),
        Some("Searching for public broadcasters")
    );
}

#[test]
fn fee_token_warning_reports_no_supporting_broadcaster() {
    let selected = Address::from([0x51; 20]);
    let unsupported = Address::from([0x52; 20]);
    let row = fee_row(1, unsupported, "unsupported");
    let options = vec![PublicBroadcasterFeeTokenOption {
        token: selected,
        label: "selected".to_string(),
        decimals: None,
        max_spendable: U256::from(1),
        eligible_broadcaster_count: 0,
        icon_path: None,
    }];
    let trust_filter = wallet_ops::PublicBroadcasterTrustFilter::default();

    assert_eq!(
        public_broadcaster_fee_token_warning(&[row], 1, &options, selected, &trust_filter),
        Some("No detected public broadcaster supports your spendable fee tokens")
    );
}

#[test]
fn fee_token_warning_reports_selected_token_without_broadcaster() {
    let selected = Address::from([0x51; 20]);
    let other = Address::from([0x52; 20]);
    let row = fee_row(1, other, "supported-other");
    let options = vec![
        PublicBroadcasterFeeTokenOption {
            token: selected,
            label: "selected".to_string(),
            decimals: None,
            max_spendable: U256::from(1),
            eligible_broadcaster_count: 0,
            icon_path: None,
        },
        PublicBroadcasterFeeTokenOption {
            token: other,
            label: "other".to_string(),
            decimals: None,
            max_spendable: U256::from(1),
            eligible_broadcaster_count: 1,
            icon_path: None,
        },
    ];
    let trust_filter = wallet_ops::PublicBroadcasterTrustFilter::default();

    assert_eq!(
        public_broadcaster_fee_token_warning(&[row], 1, &options, selected, &trust_filter),
        Some("Choose a fee token with at least one eligible public broadcaster before submitting.")
    );
    assert_eq!(
        public_broadcaster_fee_token_warning(&[], 1, &options, other, &trust_filter),
        None
    );
}

#[test]
fn unsupported_specific_broadcaster_is_detected_for_fee_token_change() {
    let token = Address::from([0x61; 20]);
    let other = Address::from([0x62; 20]);
    let policy = BroadcasterFeePolicy::default();
    let row = fee_row(1, token, "supported");
    let candidates = public_broadcaster_candidates_for_asset(&[row], 1, token, None, policy, None)
        .expect("candidates");
    let choice = BroadcasterChoice::Specific {
        railgun_address: candidates[0].railgun_address.clone(),
    };
    let unsupported = public_broadcaster_candidates_for_asset(&[], 1, other, None, policy, None)
        .expect("empty candidates");

    assert!(broadcaster_choice_supported_by_candidates(
        &choice,
        &candidates,
        policy
    ));
    assert!(!broadcaster_choice_supported_by_candidates(
        &choice,
        &unsupported,
        policy
    ));
    assert!(should_preserve_estimate_after_broadcaster_policy_change(
        &choice,
        None,
        false,
        &candidates,
        policy
    ));
    assert!(should_preserve_estimate_after_broadcaster_policy_change(
        &BroadcasterChoice::Random,
        Some(&candidates[0].railgun_address),
        false,
        &candidates,
        policy
    ));
    assert!(!should_preserve_estimate_after_broadcaster_policy_change(
        &BroadcasterChoice::Random,
        Some(&candidates[0].railgun_address),
        true,
        &candidates,
        policy
    ));
    assert!(!should_preserve_estimate_after_broadcaster_policy_change(
        &BroadcasterChoice::Random,
        None,
        false,
        &candidates,
        policy
    ));
    assert!(!should_preserve_estimate_after_broadcaster_policy_change(
        &BroadcasterChoice::Random,
        Some(&candidates[0].railgun_address),
        false,
        &unsupported,
        policy
    ));
    assert!(!should_preserve_estimate_after_broadcaster_policy_change(
        &choice,
        None,
        false,
        &unsupported,
        policy
    ));
}

#[test]
fn random_submission_selection_uses_estimated_broadcaster() {
    let token = Address::from([0x63; 20]);
    let policy = BroadcasterFeePolicy::default();
    let candidates = public_broadcaster_candidates_for_asset(
        &[fee_row(1, token, "estimated")],
        1,
        token,
        None,
        policy,
        None,
    )
    .expect("candidates");
    let candidate = candidates[0].clone();
    let estimate = public_broadcaster_cost_estimate(candidate.clone());

    assert_eq!(
        WalletRoot::public_broadcaster_submission_selection(
            &BroadcasterChoice::Random,
            Some(&estimate),
        ),
        PublicBroadcasterSelection::Specific {
            railgun_address: candidate.railgun_address
        }
    );
}

#[test]
fn picker_estimated_fee_uses_exact_estimate_fee_for_matching_broadcaster() {
    let token = Address::from([0x65; 20]);
    let policy = BroadcasterFeePolicy::default();
    let candidates = public_broadcaster_candidates_for_asset(
        &[fee_row(1, token, "estimated")],
        1,
        token,
        None,
        policy,
        None,
    )
    .expect("candidates");
    let candidate = candidates[0].clone();
    let mut estimate = public_broadcaster_cost_estimate(candidate.clone());
    estimate.fee_amount = uint!(12345_U256);

    assert_eq!(
        broadcaster_candidate_estimated_fee_amount_for_estimate(&candidate, &estimate),
        Some(estimate.fee_amount)
    );
}

#[test]
fn random_submission_selection_remains_random_without_estimate() {
    assert_eq!(
        WalletRoot::public_broadcaster_submission_selection(&BroadcasterChoice::Random, None),
        PublicBroadcasterSelection::Random
    );
}

#[test]
fn specific_submission_selection_ignores_estimate() {
    let token = Address::from([0x64; 20]);
    let policy = BroadcasterFeePolicy::default();
    let candidates = public_broadcaster_candidates_for_asset(
        &[fee_row(1, token, "estimated")],
        1,
        token,
        None,
        policy,
        None,
    )
    .expect("candidates");
    let estimate = public_broadcaster_cost_estimate(candidates[0].clone());
    let choice = BroadcasterChoice::Specific {
        railgun_address: "0zk-specific".to_string(),
    };

    assert_eq!(
        WalletRoot::public_broadcaster_submission_selection(&choice, Some(&estimate)),
        PublicBroadcasterSelection::Specific {
            railgun_address: "0zk-specific".to_string()
        }
    );
}

#[test]
fn different_fee_token_fee_handling_depends_on_action_kind() {
    let action = Address::from([0x71; 20]);
    let fee = Address::from([0x72; 20]);

    assert_eq!(
        effective_fee_handling_mode(
            DeliveryFormKind::Send,
            action,
            fee,
            FeeHandlingMode::DeductFromAmount,
        ),
        FeeHandlingMode::AddToAmount
    );
    assert_eq!(
        effective_fee_handling_mode(
            DeliveryFormKind::Unshield,
            action,
            fee,
            FeeHandlingMode::DeductFromAmount,
        ),
        FeeHandlingMode::DeductFromAmount
    );
    assert_eq!(
        effective_fee_handling_mode(
            DeliveryFormKind::Send,
            action,
            action,
            FeeHandlingMode::DeductFromAmount,
        ),
        FeeHandlingMode::DeductFromAmount
    );
    assert!(!should_show_fee_mode_toggle(
        DeliveryFormKind::Send,
        action,
        fee
    ));
    assert!(should_show_fee_mode_toggle(
        DeliveryFormKind::Send,
        action,
        action
    ));
    assert!(should_show_fee_mode_toggle(
        DeliveryFormKind::Unshield,
        action,
        fee
    ));
    assert!(should_show_fee_mode_toggle(
        DeliveryFormKind::Unshield,
        action,
        action
    ));
}

#[test]
fn unshield_max_entered_amount_depends_on_fee_handling() {
    let max_receiver = uint!(2_000_000_U256);
    let add_on_top_max = super::super::public_broadcaster::unshield_max_entered_amount_for_mode(
        max_receiver,
        FeeHandlingMode::AddToAmount,
    );

    assert_eq!(
        super::super::public_broadcaster::unshield_max_entered_amount_for_mode(
            max_receiver,
            FeeHandlingMode::DeductFromAmount,
        ),
        max_receiver
    );
    assert_eq!(add_on_top_max, uint!(1_995_000_U256),);
    assert_eq!(
        format_unshield_amount_input(add_on_top_max, Some(6)),
        "1.995"
    );
}

#[test]
fn amount_adjustment_clamps_or_raises_only_at_mode_max() {
    assert_eq!(
        adjusted_amount_for_max_change(uint!(120_U256), Some(uint!(120_U256)), uint!(100_U256),),
        Some(uint!(100_U256))
    );
    assert_eq!(
        adjusted_amount_for_max_change(uint!(100_U256), Some(uint!(100_U256)), uint!(120_U256),),
        Some(uint!(120_U256))
    );
    assert_eq!(
        adjusted_amount_for_max_change(uint!(90_U256), Some(uint!(100_U256)), uint!(120_U256),),
        None
    );
}
