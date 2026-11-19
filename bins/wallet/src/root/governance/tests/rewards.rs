use super::*;

fn positive_reward(token: Address, amount: U256) -> RewardView {
    RewardView::Positive {
        amount,
        evidence: Box::new(RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: Vec::new(),
            amount,
        }),
    }
}

#[test]
fn priced_positive_reward_has_formatted_usd_value() {
    let token = Address::from([1; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_rate(1, token, U256::from(10).pow(U256::from(18)));
    cache.store_native_usd_rate(1, U256::from(2_000_000), 18);

    assert_eq!(
        reward_usd_label(
            1,
            token,
            Some(&positive_reward(
                token,
                U256::from(3) * U256::from(10).pow(U256::from(18)),
            )),
            &cache,
        ),
        Some("$6.00".to_owned())
    );
}

#[test]
fn resolved_reward_total_adds_rows_and_zero_needs_no_price() {
    let priced_token = Address::from([1; 20]);
    let zero_token = Address::from([2; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_rate(1, priced_token, U256::from(10).pow(U256::from(18)));
    cache.store_native_usd_rate(1, U256::from(2_000_000), 18);
    let rewards = BTreeMap::from([
        (
            (String::from("account"), priced_token),
            positive_reward(
                priced_token,
                U256::from(3) * U256::from(10).pow(U256::from(18)),
            ),
        ),
        ((String::from("account"), zero_token), RewardView::Zero),
    ]);
    let resolved_reward_keys = rewards.keys().cloned().collect();

    assert_eq!(
        reward_usd_total(
            1,
            "account",
            &[priced_token, zero_token],
            &rewards,
            &resolved_reward_keys,
            &cache,
        ),
        RewardUsdState::Value(U256::from(6_000_000))
    );
    assert_eq!(
        reward_usd_label(
            1,
            zero_token,
            rewards.get(&(String::from("account"), zero_token)),
            &cache,
        ),
        Some("$0.00".to_owned())
    );
}

#[test]
fn reward_total_fails_closed_for_unresolved_or_overflowed_rows() {
    let first = Address::from([1; 20]);
    let second = Address::from([2; 20]);
    let unpriced = Address::from([3; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_rate(1, first, U256::ONE);
    cache.store_rate(1, second, U256::ONE);
    cache.store_native_usd_rate(1, U256::ONE, 18);
    let unpriced_reward = positive_reward(unpriced, U256::ONE);
    assert_eq!(
        reward_usd_label(1, unpriced, Some(&unpriced_reward), &cache),
        Some("USD unavailable".to_owned())
    );

    let cases = [
        (BTreeMap::new(), &[first][..], RewardUsdState::Loading),
        (
            BTreeMap::from([(
                (String::from("account"), first),
                RewardView::Unavailable("failed".into()),
            )]),
            &[first][..],
            RewardUsdState::Unavailable,
        ),
        (
            BTreeMap::from([(
                (String::from("account"), unpriced),
                positive_reward(unpriced, U256::ONE),
            )]),
            &[unpriced][..],
            RewardUsdState::Unavailable,
        ),
    ];
    for (rewards, tokens, expected) in cases {
        let resolved_reward_keys = rewards.keys().cloned().collect();
        assert_eq!(
            reward_usd_total(
                1,
                "account",
                tokens,
                &rewards,
                &resolved_reward_keys,
                &cache,
            ),
            expected
        );
    }

    let rewards = BTreeMap::from([
        (
            (String::from("account"), first),
            positive_reward(first, U256::MAX),
        ),
        (
            (String::from("account"), second),
            positive_reward(second, U256::MAX),
        ),
    ]);
    assert_eq!(
        reward_usd_total(
            1,
            "account",
            &[first, second],
            &rewards,
            &rewards.keys().cloned().collect(),
            &cache,
        ),
        RewardUsdState::Unavailable
    );
}

#[test]
fn staking_formatter_uses_deployment_symbols_and_effective_override() {
    let amount = U256::from(15) * U256::from(10).pow(U256::from(17));
    let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    for (chain_id, symbol) in [(1, "RAIL"), (56, "RAILBSC"), (137, "RAILPOLY")] {
        let token = governance_contracts(chain_id)
            .expect("supported governance deployment")
            .governance_token;
        assert_eq!(
            format_staking_amount(chain_id, token, amount, &empty_registry),
            format!("1.5 {symbol}")
        );
    }

    let token = governance_contracts(1)
        .expect("supported governance deployment")
        .governance_token;
    let override_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, token.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: token.to_string(),
                symbol: "CUSTOM".to_owned(),
                decimals: 18,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    assert_eq!(
        format_staking_amount(1, token, amount, &override_registry),
        "1.5 CUSTOM"
    );
}

#[test]
fn staking_account_projection_tracks_received_voting_power() {
    let owner = Address::from([1; 20]);
    let external = Address::from([2; 20]);
    let stakes = vec![
        StakePosition {
            owner,
            id: U256::ZERO,
            delegate: owner,
            amount: U256::from(10),
            staketime: U256::ZERO,
            locktime: U256::ZERO,
            claimed_time: U256::ZERO,
            state: StakeState::Active,
        },
        StakePosition {
            owner,
            id: U256::from(1),
            delegate: external,
            amount: U256::from(5),
            staketime: U256::ZERO,
            locktime: U256::ZERO,
            claimed_time: U256::ZERO,
            state: StakeState::Unlocking,
        },
        StakePosition {
            owner,
            id: U256::from(2),
            delegate: external,
            amount: U256::from(7),
            staketime: U256::ZERO,
            locktime: U256::ZERO,
            claimed_time: U256::ZERO,
            state: StakeState::Claimable,
        },
        StakePosition {
            owner,
            id: U256::from(3),
            delegate: external,
            amount: U256::from(11),
            staketime: U256::ZERO,
            locktime: U256::ZERO,
            claimed_time: U256::from(1),
            state: StakeState::Claimed,
        },
    ];

    let projection = staking_account_projection(&stakes);
    assert_eq!(projection.unclaimed_principal, U256::from(22));
    assert_eq!(projection.active_self_delegated, U256::from(10));
    assert_eq!(projection.active_external_delegated, U256::ZERO);
    assert_eq!(
        projection.received_voting_power(U256::from(30)),
        U256::from(20)
    );
}

#[test]
fn stale_generation_and_key_results_are_rejected() {
    let address = Address::from([1; 20]);
    let mut state = StakingReadState::default();
    let first = key("wallet", 1, "a", address);
    let generation = state.begin(first.clone());
    let second = key("wallet", 1, "b", address);
    let next_generation = state.begin(second.clone());
    assert!(!state.apply_account(&first, generation, "a".into(), Err(Arc::from("stale"))));
    assert!(state.apply_account(
        &second,
        next_generation,
        "b".into(),
        Err(Arc::from("account"))
    ));
    assert!(!state.accounts.contains_key("a"));
}

#[test]
fn reward_evidence_cache_requires_current_identity_tokens_and_ttl() {
    let address = Address::from([1; 20]);
    let token = Address::from([2; 20]);
    let token_two = Address::from([3; 20]);
    let key = key("wallet", 1, "a", address);
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    assert!(state.apply_global(
        &key,
        generation,
        Ok(StakingGlobalMetrics {
            total_staked: U256::ZERO,
            total_voting_power: U256::ZERO,
            deploy_time: U256::ZERO,
            snapshot_interval: U256::ONE,
            current_interval: U256::ZERO,
            stake_locktime: U256::ZERO,
            chain_time: U256::ZERO,
        })
    ));
    assert!(state.apply_reward(
        &key,
        generation,
        "a".into(),
        token,
        Ok(Some(RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: Vec::new(),
            amount: U256::ONE,
        }))
    ));
    assert!(state.apply_bulk_reward(
        &key,
        generation,
        "a".into(),
        vec![token, token_two],
        &Ok(Some(wallet_ops::RewardBatchEvidence {
            reward_tokens: vec![token, token_two],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: vec![Vec::new(), Vec::new()],
            expected_amounts: vec![U256::ONE, U256::ZERO],
        })),
    ));
    let now = Instant::now();
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[token], now)
            .is_some()
    );
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[token, token_two], now)
            .is_some()
    );
    assert!(
        state
            .cached_reward_evidence_at(&key, "missing", address, &[token], now)
            .is_none()
    );
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[token_two, token], now)
            .is_none()
    );
    assert!(
        state
            .cached_reward_evidence_at(
                &key,
                "a",
                address,
                &[token],
                now + STAKING_REWARD_EVIDENCE_TTL + Duration::from_secs(1),
            )
            .is_none()
    );
    state.begin(key.clone());
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[token], Instant::now())
            .is_none()
    );
}

fn zero_staking_metrics() -> StakingGlobalMetrics {
    StakingGlobalMetrics {
        total_staked: U256::ZERO,
        total_voting_power: U256::ZERO,
        deploy_time: U256::ZERO,
        snapshot_interval: U256::ONE,
        current_interval: U256::ZERO,
        stake_locktime: U256::ZERO,
        chain_time: U256::ZERO,
    }
}

#[test]
fn cached_bulk_reward_evidence_is_projected_onto_the_selected_subset() {
    let address = Address::from([1; 20]);
    let first = Address::from([2; 20]);
    let second = Address::from([3; 20]);
    let third = Address::from([4; 20]);
    let key = key("wallet", 1, "a", address);
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    assert!(state.apply_global(&key, generation, Ok(zero_staking_metrics())));
    assert!(state.apply_bulk_reward(
        &key,
        generation,
        "a".into(),
        vec![first, second, third],
        &Ok(Some(wallet_ops::RewardBatchEvidence {
            reward_tokens: vec![first, second, third],
            starting_interval: U256::ZERO,
            ending_interval: U256::ONE,
            staking_intervals: vec![U256::ZERO, U256::ONE],
            hints: vec![U256::ZERO, U256::ONE],
            claimed_intervals: vec![Vec::new(), vec![U256::ONE], Vec::new()],
            expected_amounts: vec![U256::from(5), U256::ZERO, U256::ZERO],
        })),
    ));
    let now = Instant::now();

    let projected = state
        .cached_reward_evidence_at(&key, "a", address, &[first, third], now)
        .expect("cached evidence projected onto the selection");
    assert_eq!(projected.reward_tokens, vec![first, third]);
    assert_eq!(projected.expected_amounts, vec![U256::from(5), U256::ZERO]);
    assert_eq!(
        projected.claimed_intervals,
        vec![Vec::<U256>::new(), Vec::<U256>::new()]
    );
    assert_eq!(projected.starting_interval, U256::ZERO);
    assert_eq!(projected.ending_interval, U256::ONE);
    assert_eq!(projected.hints, vec![U256::ZERO, U256::ONE]);
    assert!(
        state
            .cached_reward_evidence_at(&key, "a", address, &[second, third], now)
            .is_none()
    );
}

#[test]
fn reward_cost_presentation_prices_and_flags_below_fee_rows() {
    let chain = 1;
    let cache = TokenAnchorRateCache::new();
    let gas_cost = U256::from(10).pow(U256::from(15));
    let unpriced_native = reward_cost_presentation(
        chain,
        gas_cost,
        RewardUsdState::Value(U256::from(1_000)),
        &cache,
    );
    assert_eq!(unpriced_native.usd, None);
    assert_eq!(
        unpriced_native.label,
        crate::root::tokens::format_native_token_amount_for_display(chain, gas_cost)
    );
    assert!(!unpriced_native.below_fee);

    cache.store_native_usd_rate(
        chain,
        U256::from(2_000) * U256::from(10).pow(U256::from(6)),
        18,
    );
    let fee_usd = cache
        .cached_native_usd_micro_value(chain, gas_cost)
        .expect("native fee is priced");
    let below = reward_cost_presentation(
        chain,
        gas_cost,
        RewardUsdState::Value(fee_usd - U256::ONE),
        &cache,
    );
    assert_eq!(below.usd, Some(fee_usd));
    assert_eq!(below.label, format_usd_micro_value(fee_usd));
    assert!(below.below_fee);
    assert!(
        !reward_cost_presentation(chain, gas_cost, RewardUsdState::Value(fee_usd), &cache)
            .below_fee
    );
    assert!(
        !reward_cost_presentation(chain, gas_cost, RewardUsdState::Unavailable, &cache).below_fee
    );
}

#[test]
fn default_claim_selection_uses_marginal_fees_instead_of_standalone_costs() {
    let chain = 1;
    let address = Address::from([1; 20]);
    let covered = Address::from([2; 20]);
    let below = Address::from([3; 20]);
    let cache = TokenAnchorRateCache::new();
    cache.store_native_usd_rate(
        chain,
        U256::from(2_000) * U256::from(10).pow(U256::from(6)),
        18,
    );
    cache.store_rate(chain, covered, U256::from(10).pow(U256::from(18)));
    cache.store_rate(chain, below, U256::from(10).pow(U256::from(18)));
    let key = key("wallet", chain, "a", address);
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    assert!(state.apply_global(&key, generation, Ok(zero_staking_metrics())));
    // A whole native token of reward covers the fee; a ten-thousandth of one does not.
    for (token, amount) in [
        (covered, U256::from(10).pow(U256::from(18))),
        (below, U256::from(10).pow(U256::from(14))),
    ] {
        assert!(state.apply_reward(
            &key,
            generation,
            "a".into(),
            token,
            Ok(Some(RewardEvidence {
                token,
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                staking_intervals: vec![U256::ZERO],
                hints: vec![U256::ZERO],
                claimed_intervals: Vec::new(),
                amount,
            })),
        ));
    }

    let mut estimate = RewardSelectionEstimate {
        evidence: None,
        steps: Vec::new(),
        expected_fee_per_gas: 1_000_000_000,
        added_gas: BTreeMap::new(),
        shared_gas: Some(500_000),
    };
    // The small reward cannot cover its standalone fee, but covers its added batch fee.
    for (covered_gas, below_gas, expected) in [
        (Some(50_000), Some(50_000), vec![covered, below]),
        (Some(50_000), Some(200_000), vec![covered]),
        (Some(50_000), None, vec![covered, below]),
        (Some(2_000_000_000), Some(200_000), vec![]),
        (None, None, vec![covered, below]),
    ] {
        estimate.added_gas = BTreeMap::from([(covered, covered_gas), (below, below_gas)]);
        assert_eq!(
            default_reward_claim_selection_for(
                &state,
                chain,
                "a",
                &[below, covered],
                &estimate,
                None,
                &cache
            ),
            expected,
            "added gas: covered={covered_gas:?}, below={below_gas:?}",
        );
    }
}

#[test]
fn reward_claim_costs_reprice_cached_gas_with_the_current_quote() {
    let step = |estimated_gas| wallet_ops::RewardClaimStep {
        reward_tokens: Vec::new(),
        starting_interval: U256::ZERO,
        ending_interval: U256::ZERO,
        subtotal: U256::ZERO,
        expected_amounts: Vec::new(),
        estimated_gas,
    };

    let steps = [step(120_000), step(90_000)];
    let quote = wallet_ops::SelfBroadcastGasFeeQuote {
        observed_fee_model: None,
        rpc_gas_price: 8,
        current_base_fee_per_gas: Some(8),
        suggested_max_fee_per_gas: 20,
        suggested_max_priority_fee_per_gas: 2,
    };
    let custom = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 20,
        max_priority_fee_per_gas: 2,
    };
    // Auto must use the refreshed quote, just like the displayed gas controls.
    let auto_price = reward_claim_fee_per_gas(PublicActionGasFeeSelection::Auto, Some(quote), 3);
    assert_eq!(
        reward_selection_cost(&steps, auto_price),
        Some(U256::from(2_310_000))
    );
    let custom_price = reward_claim_fee_per_gas(custom, Some(quote), 3);
    assert_eq!(
        reward_selection_cost(&steps, custom_price),
        Some(
            U256::from(210_000)
                * U256::from(wallet_ops::expected_eip1559_fee_per_gas(quote, 20, 2))
        )
    );
    assert_eq!(custom_price, auto_price);
    // Without a quote the maximum fee is the conservative estimate, using the same gas.
    assert_eq!(
        reward_selection_cost(&steps, reward_claim_fee_per_gas(custom, None, 3)),
        Some(U256::from(4_200_000))
    );
    assert_eq!(reward_selection_cost(&[step(u64::MAX), step(1)], 1), None);
}

#[test]
fn reward_claim_marginals_exclude_shared_work_and_reconcile_the_selected_batch() {
    let first = Address::from([1; 20]);
    let second = Address::from([2; 20]);
    let empty = 520_000;
    let first_alone = 1_272_000;
    let second_alone = 1_274_000;
    let combined = 1_746_000;
    let added_first = reward_claim_added_gas(true, combined, second_alone);
    let added_second = reward_claim_added_gas(false, first_alone, combined);
    assert_eq!(added_first, Some(472_000));
    assert_eq!(added_second, Some(474_000));
    let mut added = BTreeMap::from([(first, added_first), (second, added_second)]);
    let shared = reward_claim_shared_gas(&[first, second], Some(combined), &added).unwrap();
    assert_eq!(shared, 800_000);
    assert_eq!(
        shared + added_first.unwrap() + added_second.unwrap(),
        combined
    );
    assert_eq!(
        reward_claim_shared_gas(&[], Some(empty), &added),
        Some(empty)
    );
    added.insert(first, reward_claim_added_gas(true, first_alone, empty));
    assert_eq!(
        reward_claim_shared_gas(&[first], Some(first_alone), &added),
        Some(empty)
    );
    // A failed or inconsistent comparison is unavailable, never a fake zero fee.
    added.insert(first, None);
    assert_eq!(
        reward_claim_shared_gas(&[first], Some(first_alone), &added),
        None
    );
    assert_eq!(reward_claim_added_gas(true, first_alone, combined), None);
    assert_eq!(reward_claim_added_gas(false, combined, first_alone), None);
}

#[gpui::test]
fn reward_gas_editor_validates_custom_inputs_and_resets_for_next_claim(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    cx.add_empty_window().update(|window, app| {
        let editor = app.new(|cx| Eip1559GasFeeEditorState::new(window, cx));
        editor.update(app, |editor, cx| {
            editor.mode = Eip1559GasFeeMode::Custom;
            editor
                .max_fee_input
                .update(cx, |input, cx| input.set_value("2", window, cx));
            editor
                .max_priority_fee_input
                .update(cx, |input, cx| input.set_value("3", window, cx));
            assert!(editor.selection(cx).is_err());
            editor
                .max_priority_fee_input
                .update(cx, |input, cx| input.set_value("1", window, cx));
            assert_eq!(
                editor.selection(cx),
                Ok(PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas: 2_000_000_000,
                    max_priority_fee_per_gas: 1_000_000_000,
                })
            );
            editor.refreshing = true;
            let stale_refresh = editor.refresh_id;
            editor.reset_for_request(window, cx);
            assert_eq!(editor.selection(cx), Ok(PublicActionGasFeeSelection::Auto));
            assert_ne!(editor.refresh_id, stale_refresh);
            assert!(!editor.refreshing);
            // Switching back to Custom cannot silently reuse the previous claim's caps.
            editor.mode = Eip1559GasFeeMode::Custom;
            assert!(editor.selection(cx).is_err());
        });
    });
}

fn reward_claim_flow(kind: StakingActionKind) -> GovernanceActionFlowState {
    let mut flow = GovernanceActionFlowState::default();
    flow.set_staking_selection(StakingActionSelection {
        actor_uuid: String::from("a"),
        actor: Address::from([1; 20]),
        kind,
    });
    flow
}

#[test]
fn reward_claim_selection_toggles_stay_unique_and_ascending() {
    let first = Address::from([2; 20]);
    let second = Address::from([3; 20]);
    let third = Address::from([4; 20]);
    let mut flow = reward_claim_flow(StakingActionKind::RewardClaimSelected {
        tokens: vec![second],
    });

    assert!(flow.set_reward_claim_selection_token(third, true));
    assert!(flow.set_reward_claim_selection_token(first, true));
    assert!(flow.set_reward_claim_selection_token(first, true));
    assert_eq!(
        flow.reward_claim_selection_tokens(),
        Some(&[first, second, third][..])
    );
    assert!(flow.set_reward_claim_selection_token(second, false));
    assert!(flow.set_reward_claim_selection_token(second, false));
    assert_eq!(
        flow.reward_claim_selection_tokens(),
        Some(&[first, third][..])
    );
    assert!(flow.set_reward_claim_selection_all(vec![third, first, third]));
    assert_eq!(
        flow.reward_claim_selection_tokens(),
        Some(&[first, third][..])
    );

    let mut stake_flow = reward_claim_flow(StakingActionKind::Stake);
    assert!(!stake_flow.set_reward_claim_selection_token(second, true));
    assert_eq!(stake_flow.reward_claim_selection_tokens(), None);
}

#[test]
fn stale_reward_selection_estimates_are_discarded() {
    let token = Address::from([2; 20]);
    let mut flow = reward_claim_flow(StakingActionKind::RewardClaimSelected {
        tokens: vec![token],
    });

    let generation = flow.begin_reward_selection_estimate();
    assert_eq!(
        flow.reward_selection_estimate,
        Some(RewardSelectionEstimateState::Pending)
    );
    assert!(
        !flow.apply_reward_selection_estimate(generation.wrapping_sub(1), Err(Arc::from("stale")),)
    );
    assert_eq!(
        flow.reward_selection_estimate,
        Some(RewardSelectionEstimateState::Pending)
    );
    assert!(flow.apply_reward_selection_estimate(generation, Err(Arc::from("unavailable"))));
    assert_eq!(
        flow.reward_selection_estimate,
        Some(RewardSelectionEstimateState::Unavailable(Arc::from(
            "unavailable"
        )))
    );

    assert!(flow.set_reward_claim_selection_token(token, false));
    let emptied = flow.begin_reward_selection_estimate();
    assert_ne!(emptied, generation);
    assert_eq!(
        flow.reward_selection_estimate,
        Some(RewardSelectionEstimateState::Pending)
    );
    assert!(!flow.apply_reward_selection_estimate(generation, Err(Arc::from("old selection"))));
}

#[test]
fn reward_evidence_mode_prefers_the_reviewed_selection_estimate() {
    let first = Address::from([2; 20]);
    let second = Address::from([3; 20]);
    let evidence = |tokens: Vec<Address>| wallet_ops::RewardBatchEvidence {
        expected_amounts: vec![U256::ONE; tokens.len()],
        claimed_intervals: vec![Vec::new(); tokens.len()],
        reward_tokens: tokens,
        starting_interval: U256::ZERO,
        ending_interval: U256::ZERO,
        staking_intervals: vec![U256::ZERO],
        hints: vec![U256::ZERO],
    };
    let estimate = RewardSelectionEstimateState::Estimated(Box::new(RewardSelectionEstimate {
        evidence: Some(evidence(vec![first])),
        steps: vec![wallet_ops::RewardClaimStep {
            reward_tokens: vec![first],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            subtotal: U256::ZERO,
            expected_amounts: vec![U256::ONE],
            estimated_gas: 120_000,
        }],
        expected_fee_per_gas: 1,
        added_gas: BTreeMap::new(),
        shared_gas: None,
    }));

    assert!(matches!(
        reward_evidence_mode_for_selection(Some(&estimate), &[first], None),
        RewardEvidenceMode::CachedInitial {
            steps: Some(steps),
            ..
        } if steps.len() == 1
    ));
    assert!(matches!(
        reward_evidence_mode_for_selection(
            Some(&estimate),
            &[first, second],
            Some(evidence(vec![first, second])),
        ),
        RewardEvidenceMode::CachedInitial { steps: None, .. }
    ));
    assert!(matches!(
        reward_evidence_mode_for_selection(
            Some(&RewardSelectionEstimateState::Pending),
            &[first],
            None,
        ),
        RewardEvidenceMode::Fresh
    ));
}
