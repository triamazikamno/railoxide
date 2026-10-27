use super::helpers::*;
use alloy::rpc::types::TransactionRequest;
use railgun_wallet::tx::{ETHEREUM_GAS_MODEL, GasEstimateMode, POLYGON_GAS_MODEL};

#[test]
fn public_broadcaster_candidates_filter_unsupported_rows_and_allow_valid_poi_required() {
    let token = address(0x21);
    let mut rows = vec![fee_row(1, token, 10, 0.9, "ok")];

    let mut expired = fee_row(1, token, 10, 0.9, "expired");
    expired.fee_expiration = SystemTime::now() - Duration::from_secs(1);
    rows.push(expired);

    let mut unavailable = fee_row(1, token, 10, 0.9, "unavailable");
    unavailable.available_wallets = 0;
    rows.push(unavailable);

    let mut unsupported_version = fee_row(1, token, 10, 0.9, "version");
    unsupported_version.version = Arc::from("9.0.0");
    rows.push(unsupported_version);

    let mut poi_required = fee_row_with_broadcaster_seed(1, token, 10, 0.9, "poi", 8);
    poi_required.required_poi_list_keys = vec![Arc::from("00".repeat(32))];
    rows.push(poi_required);

    rows.push(fee_row(2, token, 10, 0.9, "chain"));
    rows.push(fee_row(1, address(0x22), 10, 0.9, "token"));

    let candidates = eligible_public_broadcasters(&rows, 1, token, SystemTime::now());

    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].fees_id, "ok");
    assert_eq!(candidates[1].fees_id, "poi");
}

#[test]
fn executor_submission_keeps_the_reserved_quote_when_advertisements_refresh() {
    let (mut reserved, _) = sample_public_broadcaster_candidate(11);
    let chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    reserved.relay_adapt_7702 = Some(chain.accepted_executor_profile().unwrap().delegate());
    let rows = [fee_row_with_broadcaster_seed(
        1,
        reserved.token,
        50,
        0.9,
        "refreshed-fees",
        11,
    )];
    let delivery = crate::ExecutorDelivery::PublicBroadcaster(Box::new(reserved.clone()));
    let policy = BroadcasterFeePolicy::default();
    let trust = PublicBroadcasterTrustFilter::default();
    let select = |delivery,
                  selection: &PublicBroadcasterSelection,
                  anchor,
                  trust: &PublicBroadcasterTrustFilter| {
        crate::desktop::public_broadcaster_for_request(
            &rows,
            1,
            reserved.token,
            selection,
            policy,
            trust,
            anchor,
            delivery,
        )
    };
    let random = PublicBroadcasterSelection::Random;
    assert_eq!(
        select(None, &random, None, &trust).unwrap().fees_id,
        "refreshed-fees"
    );
    for selection in [
        random.clone(),
        PublicBroadcasterSelection::Specific {
            railgun_address: reserved.railgun_address.clone(),
        },
    ] {
        let selected = select(Some(&delivery), &selection, None, &trust).unwrap();
        assert_eq!(selected.fees_id, reserved.fees_id);
        assert_eq!(selected.fee, reserved.fee);
        assert_eq!(selected.fee_expiration, reserved.fee_expiration);
    }
    assert!(select(Some(&delivery), &random, Some(U256::from(100)), &trust).is_err());
    let favorites_only = PublicBroadcasterTrustFilter {
        favorites_only: true,
        ..trust
    };
    assert!(select(Some(&delivery), &random, None, &favorites_only).is_err());
    let mut expired = reserved.clone();
    expired.fee_expiration = SystemTime::now() - Duration::from_secs(1);
    assert!(
        select(
            Some(&crate::ExecutorDelivery::PublicBroadcaster(Box::new(
                expired
            ))),
            &random,
            None,
            &PublicBroadcasterTrustFilter::default(),
        )
        .is_err()
    );
}

#[test]
fn executor_delivery_targets_the_broadcasters_listed_delegate() {
    let primary =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .unwrap()
            .accepted_executor_profile()
            .unwrap();
    let earlier = broadcaster_core::deployment::RailgunDeployment::for_chain(1)
        .unwrap()
        .relay_adapt_7702_delegates[0]
        .address;
    assert_ne!(earlier, primary.delegate());
    let (mut candidate, _) = sample_public_broadcaster_candidate(12);
    for (advertised, target) in [
        (primary.delegate(), Some(primary.delegate())),
        (earlier, Some(earlier)),
        (address(0x23), None),
    ] {
        candidate.relay_adapt_7702 = Some(advertised);
        let admitted = crate::ExecutorDelivery::PublicBroadcaster(Box::new(candidate.clone()))
            .admit(primary)
            .ok()
            .map(crate::settings::ExecutorProfile::delegate);
        assert_eq!(admitted, target);
    }
    let self_broadcast = crate::ExecutorDelivery::SelfBroadcast {
        sender: address(0x24),
        sponsored: false,
    };
    assert_eq!(self_broadcast.admit(primary).unwrap(), primary);
}

#[test]
fn public_broadcaster_candidates_are_keyed_by_selected_fee_token() {
    let action_token = address(0x43);
    let fee_token = address(0x44);
    let candidates = public_broadcaster_candidates(
        &[
            fee_row(1, action_token, 10, 0.9, "action-token"),
            fee_row(1, fee_token, 11, 0.9, "fee-token"),
        ],
        1,
        fee_token,
        SystemTime::now(),
        BroadcasterFeePolicy::default(),
        None,
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].fees_id, "fee-token");
    assert_eq!(candidates[0].token, fee_token);
}

#[test]
fn specific_public_broadcaster_fails_when_not_available_for_fee_token() {
    let action_token = address(0x45);
    let fee_token = address(0x46);
    let railgun_address = sample_railgun_address(51);
    let candidates = public_broadcaster_candidates(
        &[fee_row_with_broadcaster_seed(
            1,
            action_token,
            10,
            0.9,
            "action-only",
            51,
        )],
        1,
        fee_token,
        SystemTime::now(),
        BroadcasterFeePolicy::default(),
        None,
    );

    let error = select_public_broadcaster_with_policy(
        &candidates,
        &PublicBroadcasterSelection::Specific { railgun_address },
        BroadcasterFeePolicy::default(),
    )
    .expect_err("specific broadcaster should not match action-token row");

    assert!(error.to_string().contains("no longer eligible"));
}

#[test]
fn public_broadcaster_selection_sorts_by_fee_then_seeded_tie_breaker() {
    let token = address(0x23);
    let candidates = eligible_public_broadcasters(
        &[
            fee_row_with_broadcaster_seed(1, token, 20, 0.99, "expensive", 11),
            fee_row_with_broadcaster_seed(1, token, 10, 0.50, "cheap-low-rel", 12),
            fee_row_with_broadcaster_seed(1, token, 10, 0.90, "cheap-high-rel", 13),
        ],
        1,
        token,
        SystemTime::now(),
    );
    let sort_seed = (0_u8..=u8::MAX)
        .map(|seed| [seed; 32])
        .find(|seed| {
            sort_specific_public_broadcasters(candidates.clone(), seed)[0].fees_id
                == "cheap-low-rel"
        })
        .expect("test fixture should have a seed that places lower reliability first");

    let sorted = sort_specific_public_broadcasters(candidates.clone(), &sort_seed);
    let ids: Vec<_> = sorted
        .iter()
        .map(|candidate| candidate.fees_id.as_str())
        .collect();
    assert_eq!(ids, vec!["cheap-low-rel", "cheap-high-rel", "expensive"]);
    assert_eq!(
        ids,
        sort_specific_public_broadcasters(candidates.clone(), &sort_seed)
            .iter()
            .map(|candidate| candidate.fees_id.as_str())
            .collect::<Vec<_>>()
    );
    let cheap_low_rel_address = candidates
        .iter()
        .find(|candidate| candidate.fees_id == "cheap-low-rel")
        .expect("cheap-low-rel candidate")
        .railgun_address
        .clone();

    let selected = select_public_broadcaster(
        &candidates,
        &PublicBroadcasterSelection::Specific {
            railgun_address: cheap_low_rel_address,
        },
    )
    .expect("specific candidate");
    assert_eq!(selected.fees_id, "cheap-low-rel");
    assert!(select_public_broadcaster(&candidates, &PublicBroadcasterSelection::Random).is_ok());
}

#[test]
fn public_broadcaster_seeded_tie_breaker_replaces_lexical_address_order() {
    let token = address(0x24);
    let candidates = eligible_public_broadcasters(
        &[
            fee_row_with_broadcaster_seed(1, token, 10, 0.9, "candidate-a", 71),
            fee_row_with_broadcaster_seed(1, token, 10, 0.9, "candidate-b", 72),
            fee_row_with_broadcaster_seed(1, token, 10, 0.9, "candidate-c", 73),
            fee_row_with_broadcaster_seed(1, token, 10, 0.9, "candidate-d", 74),
        ],
        1,
        token,
        SystemTime::now(),
    );
    let mut lexical = candidates.clone();
    lexical.sort_by(|left, right| left.railgun_address.cmp(&right.railgun_address));
    let lexical_ids = lexical
        .iter()
        .map(|candidate| candidate.fees_id.as_str())
        .collect::<Vec<_>>();
    let sort_seed = (0_u8..=u8::MAX)
        .map(|seed| [seed; 32])
        .find(|seed| {
            sort_specific_public_broadcasters(candidates.clone(), seed)
                .iter()
                .map(|candidate| candidate.fees_id.as_str())
                .collect::<Vec<_>>()
                != lexical_ids
        })
        .expect("test fixture should have a seed that differs from lexical address order");

    let sorted = sort_specific_public_broadcasters(candidates, &sort_seed);
    let sorted_ids = sorted
        .iter()
        .map(|candidate| candidate.fees_id.as_str())
        .collect::<Vec<_>>();
    assert_ne!(sorted_ids, lexical_ids);
}

#[test]
fn random_public_broadcaster_pool_includes_valid_poi_candidates_only_after_filters() {
    let token = address(0x27);
    let mut poi_required = fee_row_with_broadcaster_seed(1, token, 10, 0.9, "poi", 31);
    poi_required.required_poi_list_keys = vec![Arc::from("11".repeat(32))];
    let mut malformed = fee_row_with_broadcaster_seed(1, token, 10, 0.9, "malformed", 33);
    malformed.required_poi_list_keys = vec![Arc::from("not-a-list-key")];
    let out_of_policy = fee_row_with_broadcaster_seed(1, token, 20, 0.9, "out-of-policy", 34);
    let banned = fee_row_with_broadcaster_seed(1, token, 10, 0.9, "banned", 35);
    let mut candidates = public_broadcaster_candidates(
        &[
            poi_required,
            fee_row_with_broadcaster_seed(1, token, 10, 0.9, "no-poi", 32),
            malformed,
            out_of_policy,
            banned,
        ],
        1,
        token,
        SystemTime::now(),
        BroadcasterFeePolicy::default(),
        Some(U256::from(10)),
    );
    let malformed_candidate = candidates
        .iter()
        .find(|candidate| candidate.fees_id == "no-poi")
        .cloned()
        .expect("no-POI candidate");
    let mut malformed_candidate = malformed_candidate;
    malformed_candidate.required_poi_list_keys = vec!["malformed".to_string()];
    candidates.push(malformed_candidate);

    let trust_filter = PublicBroadcasterTrustFilter {
        preferences: vault::BroadcasterPreferences {
            favorites: Vec::new(),
            banned: vec![broadcaster_preference_entry(35)],
        },
        favorites_only: false,
    };
    let pool = random_eligible_public_broadcasters(
        &candidates,
        BroadcasterFeePolicy::default(),
        &trust_filter,
    );

    assert_eq!(
        pool.iter()
            .map(|candidate| candidate.fees_id.as_str())
            .collect::<Vec<_>>(),
        vec!["poi", "no-poi"]
    );
}

#[test]
fn public_broadcaster_deduplication_keeps_freshest_parsed_identity() {
    let token = address(0x28);
    let now = SystemTime::now();
    let mut older = fee_row_with_broadcaster_seed(1, token, 10, 0.9, "older-fees", 36);
    older.identifier = Some(Arc::from("older-identifier"));
    older.fee_expiration = now + Duration::from_secs(10);
    let mut fresher = older.clone();
    fresher.fees_id = Arc::from("fresh-fees");
    fresher.identifier = Some(Arc::from("fresh-identifier"));
    fresher.fee_expiration = now + Duration::from_secs(20);

    let candidates = eligible_public_broadcasters(&[older, fresher], 1, token, now);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].fees_id, "fresh-fees");
    assert_eq!(
        candidates[0].identifier.as_deref(),
        Some("fresh-identifier")
    );

    let mut older_candidate = candidates[0].clone();
    older_candidate.fees_id = "older-direct-fees".to_string();
    older_candidate.identifier = Some("older-direct-identifier".to_string());
    older_candidate.fee_expiration = now + Duration::from_secs(30);
    let mut fresher_candidate = older_candidate.clone();
    fresher_candidate.fees_id = "fresh-direct-fees".to_string();
    fresher_candidate.identifier = Some("fresh-direct-identifier".to_string());
    fresher_candidate.fee_expiration = now + Duration::from_secs(40);

    let pool = random_eligible_public_broadcasters(
        &[older_candidate, fresher_candidate],
        BroadcasterFeePolicy::default(),
        &PublicBroadcasterTrustFilter::default(),
    );
    assert_eq!(pool.len(), 1);
    assert_eq!(pool[0].fees_id, "fresh-direct-fees");
    assert_eq!(
        pool[0].identifier.as_deref(),
        Some("fresh-direct-identifier")
    );
}

#[test]
fn public_broadcaster_specific_selection_survives_fees_id_refresh() {
    let token = address(0x24);
    let railgun_address = sample_railgun_address(21);
    let candidates = eligible_public_broadcasters(
        &[fee_row_with_broadcaster_seed(
            1,
            token,
            10,
            0.9,
            "fresh-fees-id",
            21,
        )],
        1,
        token,
        SystemTime::now(),
    );

    let selected = select_public_broadcaster(
        &candidates,
        &PublicBroadcasterSelection::Specific { railgun_address },
    )
    .expect("specific candidate by stable address");

    assert_eq!(selected.fees_id, "fresh-fees-id");
}

#[test]
fn public_broadcaster_trust_filter_gives_banned_precedence() {
    let token = address(0x47);
    let candidates = public_broadcaster_candidates(
        &[
            fee_row_with_broadcaster_seed(1, token, 10, 0.9, "favorite-and-banned", 61),
            fee_row_with_broadcaster_seed(1, token, 11, 0.9, "neutral", 62),
        ],
        1,
        token,
        SystemTime::now(),
        BroadcasterFeePolicy::default(),
        None,
    );
    let trust_filter = PublicBroadcasterTrustFilter {
        preferences: vault::BroadcasterPreferences {
            favorites: vec![broadcaster_preference_entry(61)],
            banned: vec![broadcaster_preference_entry(61)],
        },
        favorites_only: false,
    };

    let trusted = filter_public_broadcasters_by_trust(&candidates, &trust_filter);

    assert_eq!(trusted.len(), 1);
    assert_eq!(trusted[0].fees_id, "neutral");
}

#[test]
fn public_broadcaster_trust_filter_supports_favorites_only_selection() {
    let token = address(0x48);
    let candidates = public_broadcaster_candidates(
        &[
            fee_row_with_broadcaster_seed(1, token, 10, 0.9, "favorite", 63),
            fee_row_with_broadcaster_seed(1, token, 11, 0.9, "neutral", 64),
        ],
        1,
        token,
        SystemTime::now(),
        BroadcasterFeePolicy::default(),
        None,
    );
    let favorite_address = sample_railgun_address(63);
    let neutral_address = sample_railgun_address(64);
    let trust_filter = PublicBroadcasterTrustFilter {
        preferences: vault::BroadcasterPreferences {
            favorites: vec![broadcaster_preference_entry(63)],
            banned: Vec::new(),
        },
        favorites_only: true,
    };

    let favorite = select_public_broadcaster_with_policy_and_trust(
        &candidates,
        &PublicBroadcasterSelection::Specific {
            railgun_address: favorite_address,
        },
        BroadcasterFeePolicy::default(),
        &trust_filter,
    )
    .expect("favorite broadcaster remains selectable");
    let error = select_public_broadcaster_with_policy_and_trust(
        &candidates,
        &PublicBroadcasterSelection::Specific {
            railgun_address: neutral_address,
        },
        BroadcasterFeePolicy::default(),
        &trust_filter,
    )
    .expect_err("non-favorite should be rejected");

    assert_eq!(favorite.fees_id, "favorite");
    assert!(error.to_string().contains("preferences"));
}

#[test]
fn public_broadcaster_trust_filter_rejects_stale_estimated_broadcaster() {
    let token = address(0x49);
    let railgun_address = sample_railgun_address(65);
    let candidates = public_broadcaster_candidates(
        &[fee_row_with_broadcaster_seed(
            1, token, 10, 0.9, "stale", 65,
        )],
        1,
        token,
        SystemTime::now(),
        BroadcasterFeePolicy::default(),
        None,
    );
    let trust_filter = PublicBroadcasterTrustFilter {
        preferences: vault::BroadcasterPreferences {
            favorites: Vec::new(),
            banned: vec![broadcaster_preference_entry(65)],
        },
        favorites_only: false,
    };

    let error = select_public_broadcaster_with_policy_and_trust(
        &candidates,
        &PublicBroadcasterSelection::Specific { railgun_address },
        BroadcasterFeePolicy::default(),
        &trust_filter,
    )
    .expect_err("banned estimated broadcaster should be rejected");

    assert!(error.to_string().contains("preferences"));
}

#[test]
fn public_broadcaster_fee_policy_classifies_anchor_bounds() {
    let token = address(0x28);
    let policy = BroadcasterFeePolicy::default();
    let candidates = public_broadcaster_candidates(
        &[
            fee_row_with_broadcaster_seed(1, token, 89, 0.9, "below", 1),
            fee_row_with_broadcaster_seed(1, token, 90, 0.9, "lower-bound", 2),
            fee_row_with_broadcaster_seed(1, token, 150, 0.9, "upper-bound", 3),
            fee_row_with_broadcaster_seed(1, token, 151, 0.9, "above", 4),
        ],
        1,
        token,
        SystemTime::now(),
        policy,
        Some(uint!(100_U256)),
    );

    let eligible_ids = fee_policy_eligible_public_broadcasters(&candidates, policy)
        .into_iter()
        .map(|candidate| candidate.fees_id)
        .collect::<Vec<_>>();
    assert_eq!(eligible_ids, vec!["lower-bound", "upper-bound"]);
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.fees_id == "below"
                && matches!(
                    candidate.fee_policy_status,
                    BroadcasterFeePolicyStatus::Suspicious {
                        premium_bps: Some(-1100),
                        ..
                    }
                ))
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.fees_id == "above"
                && matches!(
                    candidate.fee_policy_status,
                    BroadcasterFeePolicyStatus::Suspicious {
                        premium_bps: Some(5100),
                        ..
                    }
                ))
    );
}

#[test]
fn public_broadcaster_fee_policy_allows_unknown_anchor_rows() {
    let token = address(0x29);
    let policy = BroadcasterFeePolicy::default();
    let candidates = public_broadcaster_candidates(
        &[fee_row(1, token, 1_000_000, 0.9, "raw")],
        1,
        token,
        SystemTime::now(),
        policy,
        None,
    );

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].fee_policy_status,
        BroadcasterFeePolicyStatus::UnknownAnchor
    );
    assert_eq!(
        fee_policy_eligible_public_broadcasters(&candidates, policy).len(),
        1
    );
}

#[test]
fn public_broadcaster_policy_uses_fixed_anchor_without_cache() {
    let weth = wrapped_native_token_for_chain(1).expect("ethereum wrapped native");

    assert_eq!(
        fixed_token_anchor_rate(1, weth),
        Some(uint!(1_000_000_000_000_000_000_U256))
    );
    assert_eq!(
        public_broadcaster_anchor_rate_for_policy(None, 1, weth),
        Some(uint!(1_000_000_000_000_000_000_U256))
    );
}

#[test]
fn public_broadcaster_fee_breakdown_splits_gas_and_margin() {
    let breakdown = public_broadcaster_fee_breakdown(
        uint!(3_000_U256),
        10,
        100,
        Some(uint!(2_000_000_000_000_000_000_U256)),
    );

    assert_eq!(breakdown.native_gas_cost, uint!(1_250_U256));
    assert_eq!(breakdown.fee_token_gas_cost, Some(uint!(2_500_U256)));
    assert_eq!(
        breakdown.broadcaster_fee,
        Some(PublicBroadcasterFeeMargin::Positive(uint!(500_U256)))
    );
}

#[test]
fn public_broadcaster_fee_breakdown_handles_negative_and_missing_anchor() {
    let negative = public_broadcaster_fee_breakdown(
        uint!(1_000_U256),
        10,
        100,
        Some(uint!(2_000_000_000_000_000_000_U256)),
    );
    let missing = public_broadcaster_fee_breakdown(uint!(1_000_U256), 10, 100, None);

    assert_eq!(
        negative.broadcaster_fee,
        Some(PublicBroadcasterFeeMargin::Negative(uint!(1_500_U256)))
    );
    assert_eq!(missing.native_gas_cost, uint!(1_250_U256));
    assert_eq!(missing.fee_token_gas_cost, None);
    assert_eq!(missing.broadcaster_fee, None);
}

#[test]
fn public_broadcaster_fee_policy_override_includes_suspicious_rows() {
    let token = address(0x2a);
    let policy = BroadcasterFeePolicy::default();
    let allow_policy = policy.with_allow_suspicious_broadcasters(true);
    let candidates = public_broadcaster_candidates(
        &[fee_row(1, token, 151, 0.9, "above")],
        1,
        token,
        SystemTime::now(),
        policy,
        Some(uint!(100_U256)),
    );

    assert!(
        select_public_broadcaster_with_policy(
            &candidates,
            &PublicBroadcasterSelection::Random,
            policy
        )
        .is_err()
    );
    assert!(
        select_public_broadcaster_with_policy(
            &candidates,
            &PublicBroadcasterSelection::Random,
            allow_policy
        )
        .is_ok()
    );
}

#[test]
fn specific_public_broadcaster_drift_rechecks_latest_fee_policy() {
    let token = address(0x2b);
    let railgun_address = sample_railgun_address(41);
    let policy = BroadcasterFeePolicy::default();
    let initial = public_broadcaster_candidates(
        &[fee_row_with_broadcaster_seed(
            1, token, 100, 0.9, "initial", 41,
        )],
        1,
        token,
        SystemTime::now(),
        policy,
        Some(uint!(100_U256)),
    );
    let selection = PublicBroadcasterSelection::Specific { railgun_address };
    assert!(select_public_broadcaster_with_policy(&initial, &selection, policy).is_ok());

    let drifted = public_broadcaster_candidates(
        &[fee_row_with_broadcaster_seed(
            1, token, 151, 0.9, "drifted", 41,
        )],
        1,
        token,
        SystemTime::now(),
        policy,
        Some(uint!(100_U256)),
    );
    let error = select_public_broadcaster_with_policy(&drifted, &selection, policy)
        .expect_err("drifted broadcaster should be blocked");
    assert!(error.to_string().contains("outside the allowed range"));
    assert!(
        select_public_broadcaster_with_policy(
            &drifted,
            &selection,
            policy.with_allow_suspicious_broadcasters(true)
        )
        .is_ok()
    );
}

#[test]
fn broadcaster_fee_amount_uses_same_token_fee_rate() {
    let fee = broadcaster_fee_amount(
        uint!(2_000_000_000_000_000_000_U256),
        150_000,
        20_000_000_000,
    );

    assert_eq!(fee, uint!(6_000_000_000_000_000_U256));
}

#[test]
fn railgun_protocol_fee_uses_hardcoded_unshield_bps() {
    let amount = uint!(1_000_000_U256);

    assert_eq!(
        crate::railgun_protocol_fee_amount(amount, RAILGUN_PROTOCOL_FEE_BPS),
        uint!(2_500_U256)
    );
    assert_eq!(
        crate::railgun_protocol_fee_amount(amount, U256::ZERO),
        U256::ZERO
    );
}

#[test]
fn unshield_fee_handling_handles_protocol_fee_for_same_and_different_fee_tokens() {
    let entered = uint!(1_000_000_U256);
    let broadcaster_fee = uint!(400_U256);
    let gross =
        crate::railgun_protocol_gross_amount_for_recipient(entered, RAILGUN_PROTOCOL_FEE_BPS)
            .expect("gross protocol amount");
    let protocol_fee = crate::railgun_protocol_fee_amount(gross, RAILGUN_PROTOCOL_FEE_BPS);

    assert_eq!(gross, uint!(1_002_506_U256));
    assert_eq!(gross - protocol_fee, entered);
    assert_eq!(
        crate::unshield_receiver_amount_for_fee_mode(entered, FeeHandlingMode::DeductFromAmount)
            .expect("deduct unshield receiver amount"),
        entered
    );
    assert_eq!(
        crate::unshield_receiver_amount_for_fee_mode(entered, FeeHandlingMode::AddToAmount)
            .expect("add unshield receiver amount"),
        gross
    );
    assert_eq!(
        crate::unshield_protocol_fee_amount_for_fee_mode(
            entered,
            FeeHandlingMode::DeductFromAmount,
        )
        .expect("deduct protocol fee"),
        uint!(2_500_U256)
    );
    assert_eq!(
        crate::unshield_protocol_fee_amount_for_fee_mode(entered, FeeHandlingMode::AddToAmount,)
            .expect("add protocol fee"),
        gross - entered
    );

    let same_token_add = public_broadcaster_amount_split_for_tokens_and_protocol(
        entered,
        broadcaster_fee,
        FeeHandlingMode::AddToAmount,
        true,
        RAILGUN_PROTOCOL_FEE_BPS,
    )
    .expect("same-token add split");
    assert_eq!(same_token_add.receiver_amount, gross);
    assert_eq!(same_token_add.total_private_spend, gross + broadcaster_fee);
    assert_eq!(same_token_add.fee_mode, FeeHandlingMode::AddToAmount);

    let same_token_deduct = public_broadcaster_amount_split_for_tokens_and_protocol(
        entered,
        broadcaster_fee,
        FeeHandlingMode::DeductFromAmount,
        true,
        RAILGUN_PROTOCOL_FEE_BPS,
    )
    .expect("same-token deduct split");
    assert_eq!(same_token_deduct.receiver_amount, entered - broadcaster_fee);
    assert_eq!(same_token_deduct.total_private_spend, entered);
    assert_eq!(
        same_token_deduct.fee_mode,
        FeeHandlingMode::DeductFromAmount
    );

    let different_token_add = public_broadcaster_amount_split_for_tokens_and_protocol(
        entered,
        broadcaster_fee,
        FeeHandlingMode::AddToAmount,
        false,
        RAILGUN_PROTOCOL_FEE_BPS,
    )
    .expect("different-token add split");
    assert_eq!(different_token_add.receiver_amount, gross);
    assert_eq!(different_token_add.total_private_spend, gross);
    assert_eq!(different_token_add.fee_mode, FeeHandlingMode::AddToAmount);

    let different_token_deduct = public_broadcaster_amount_split_for_tokens_and_protocol(
        entered,
        broadcaster_fee,
        FeeHandlingMode::DeductFromAmount,
        false,
        RAILGUN_PROTOCOL_FEE_BPS,
    )
    .expect("different-token deduct split");
    assert_eq!(different_token_deduct.receiver_amount, entered);
    assert_eq!(different_token_deduct.total_private_spend, entered);
    assert_eq!(
        different_token_deduct.fee_mode,
        FeeHandlingMode::DeductFromAmount
    );

    let max_receiver = uint!(2_000_000_U256);
    assert_eq!(
        public_broadcaster_max_entered_amount_for_tokens_and_protocol(
            max_receiver,
            broadcaster_fee,
            FeeHandlingMode::AddToAmount,
            true,
            RAILGUN_PROTOCOL_FEE_BPS,
        ),
        uint!(1_995_000_U256)
    );
    assert_eq!(
        public_broadcaster_max_entered_amount_for_tokens_and_protocol(
            max_receiver,
            broadcaster_fee,
            FeeHandlingMode::DeductFromAmount,
            true,
            RAILGUN_PROTOCOL_FEE_BPS,
        ),
        max_receiver + broadcaster_fee
    );
    assert_eq!(
        public_broadcaster_max_entered_amount_for_tokens_and_protocol(
            max_receiver,
            broadcaster_fee,
            FeeHandlingMode::DeductFromAmount,
            false,
            RAILGUN_PROTOCOL_FEE_BPS,
        ),
        max_receiver
    );
}

#[test]
fn public_broadcaster_fee_stabilization_accepts_covering_fee() {
    let required = uint!(1_000_U256);

    assert!(broadcaster_fee_covers(required, required));
    assert!(broadcaster_fee_covers(required + uint!(1_U256), required));
    assert!(!broadcaster_fee_covers(required - uint!(1_U256), required));
}

#[test]
fn public_broadcaster_fee_stabilization_buffers_retries() {
    use crate::desktop::bounded_public_broadcaster_fee;

    assert_eq!(
        buffered_public_broadcaster_fee(uint!(10_000_U256)),
        uint!(10_100_U256)
    );
    assert_eq!(
        buffered_public_broadcaster_fee(uint!(1_U256)),
        uint!(2_U256)
    );
    let approved = buffered_public_broadcaster_fee(uint!(10_000_U256));
    // A small required-fee increase can consume the approved buffer, but must
    // neither increase the approved spend nor reject a fee that still fits.
    assert_eq!(
        bounded_public_broadcaster_fee(uint!(10_050_U256), Some(approved)).unwrap(),
        approved
    );
    assert_eq!(
        bounded_public_broadcaster_fee(approved, Some(approved)).unwrap(),
        approved
    );
    assert!(bounded_public_broadcaster_fee(approved + U256::ONE, Some(approved)).is_err());
    assert_eq!(
        bounded_public_broadcaster_fee(uint!(10_050_U256), None).unwrap(),
        buffered_public_broadcaster_fee(uint!(10_050_U256))
    );
}

#[test]
fn executor_quote_refresh_keeps_payment_separate_from_approval_allowance() {
    let token = address(0x25);
    let entered = uint!(10_000_000_000_000_U256);
    for (fee_token, mode) in [
        (token, FeeHandlingMode::DeductFromAmount),
        (token, FeeHandlingMode::AddToAmount),
        (address(0x26), FeeHandlingMode::DeductFromAmount),
    ] {
        let broadcaster = eligible_public_broadcasters(
            &[fee_row(
                1,
                fee_token,
                1_000_000_000_000_000_000,
                0.9,
                "approval",
            )],
            1,
            fee_token,
            SystemTime::now(),
        )
        .remove(0);
        let estimate = |gas_price, starting_fee| {
            approximate_public_broadcaster_cost(
                broadcaster.clone(),
                crate::GAS_LIMIT_BUFFER,
                token,
                fee_token,
                entered,
                mode,
                RAILGUN_PROTOCOL_FEE_BPS,
                gas_price,
                starting_fee,
                None,
                |_split| {
                    let capacity = entered * U256::from(2);
                    let selection = selection_info(capacity, 1, 1, 2, 1, capacity);
                    Ok(unshield_approximate_shape(&selection, capacity, true).with_executor(true))
                },
            )
            .unwrap()
        };
        let approved = estimate(100_000, U256::ZERO);
        let maximum = crate::default_public_broadcaster_fee_limit(approved.fee_amount);
        let bounds = approved.approval_bounds(maximum).unwrap();
        for price in [90_000, 100_000, 112_500, 125_000] {
            let refreshed = estimate(price, U256::ZERO);
            assert!(bounds.covers(&refreshed));
            if price == 100_000 {
                // Merely approving more must not increase either the fee or the debit.
                assert_eq!(refreshed.fee_amount, approved.fee_amount);
                assert_eq!(refreshed.recipient_amount, approved.recipient_amount);
                assert_eq!(refreshed.total_private_spend, approved.total_private_spend);
                assert!(refreshed.fee_amount < bounds.maximum_fee());
            }
            let required = broadcaster_fee_amount(
                refreshed.broadcaster.fee,
                refreshed.gas_limit,
                crate::public_broadcaster_service_gas_price(refreshed.min_gas_price),
            );
            assert_eq!(
                crate::desktop::bounded_public_broadcaster_fee(
                    required,
                    Some(bounds.maximum_fee())
                )
                .unwrap(),
                refreshed.fee_amount,
            );
        }
        let exceeded = estimate(125_001, U256::ZERO);
        assert!(!bounds.covers(&exceeded));
        // A refresh near the limit must not grant a second 25% increase.
        assert!(!bounds.covers(&estimate(140_000, U256::ZERO)));
    }
}

#[test]
fn custom_broadcaster_fee_stays_exact_within_the_reviewed_limit() {
    use crate::desktop::validate_custom_public_broadcaster_fee;
    let custom = uint!(20_000_U256);
    for required in [uint!(10_000_U256), custom] {
        assert_eq!(
            validate_custom_public_broadcaster_fee(custom, required, Some(custom)).unwrap(),
            custom
        );
    }
    assert!(validate_custom_public_broadcaster_fee(custom, custom + U256::ONE, None).is_err());
    assert!(
        validate_custom_public_broadcaster_fee(custom, U256::ONE, Some(custom - U256::ONE))
            .is_err()
    );
    assert!(validate_custom_public_broadcaster_fee(U256::ZERO, U256::ZERO, None).is_err());
}

#[test]
fn custom_broadcaster_fee_drives_note_selection_and_recipient_amounts() {
    let token = address(0x25);
    let entered = uint!(10_000_000_000_U256);
    let custom = uint!(2_000_000_000_U256);
    let capacity = entered * U256::from(2);
    for (fee_token, mode) in [
        (token, FeeHandlingMode::DeductFromAmount),
        (token, FeeHandlingMode::AddToAmount),
        (address(0x26), FeeHandlingMode::DeductFromAmount),
    ] {
        let broadcaster = eligible_public_broadcasters(
            &[fee_row(
                1,
                fee_token,
                1_000_000_000_000_000_000,
                0.9,
                "custom-fee",
            )],
            1,
            fee_token,
            SystemTime::now(),
        )
        .remove(0);
        let estimate = approximate_public_broadcaster_cost(
            broadcaster.clone(),
            crate::GAS_LIMIT_BUFFER,
            token,
            fee_token,
            entered,
            mode,
            uint!(25_U256),
            100,
            custom + U256::ONE,
            Some(custom),
            |split| {
                assert_eq!(split.fee_amount, custom);
                let selection = selection_info(capacity, 1, 1, 2, 1, capacity);
                Ok(unshield_approximate_shape(&selection, capacity, true).with_executor(true))
            },
        )
        .unwrap();
        assert_eq!(estimate.fee_amount, custom);
        if token == fee_token && mode == FeeHandlingMode::DeductFromAmount {
            assert_eq!(estimate.receiver_amount, entered - custom);
            assert_eq!(estimate.total_private_spend, entered);
        } else if mode == FeeHandlingMode::AddToAmount {
            let gross = crate::desktop::railgun_protocol_gross_amount_for_recipient(
                entered,
                uint!(25_U256),
            )
            .unwrap();
            assert_eq!(estimate.recipient_amount, entered);
            assert_eq!(estimate.receiver_amount, gross);
            assert_eq!(estimate.total_private_spend, gross + custom);
        } else {
            assert_eq!(estimate.receiver_amount, entered);
            assert_eq!(estimate.total_private_spend, entered);
        }
        assert_eq!(
            estimate.recipient_amount + estimate.protocol_fee_amount,
            estimate.receiver_amount
        );
        assert!(
            approximate_public_broadcaster_cost(
                broadcaster,
                crate::GAS_LIMIT_BUFFER,
                token,
                fee_token,
                entered,
                mode,
                uint!(25_U256),
                100,
                U256::ZERO,
                Some(U256::ONE),
                |_| {
                    let selection = selection_info(capacity, 1, 1, 2, 1, capacity);
                    Ok(unshield_approximate_shape(&selection, capacity, true).with_executor(true))
                },
            )
            .is_err()
        );
    }
}

#[test]
fn fee_handling_mode_deducts_or_adds_fee() {
    let entered = uint!(100_U256);
    let fee = uint!(7_U256);

    let deducted = public_broadcaster_amount_split(entered, fee, FeeHandlingMode::DeductFromAmount)
        .expect("deduct split");
    assert_eq!(deducted.receiver_amount, uint!(93_U256));
    assert_eq!(deducted.total_private_spend, entered);

    let added = public_broadcaster_amount_split(entered, fee, FeeHandlingMode::AddToAmount)
        .expect("add split");
    assert_eq!(added.receiver_amount, entered);
    assert_eq!(added.total_private_spend, uint!(107_U256));

    for protocol_fee in [U256::ZERO, RAILGUN_PROTOCOL_FEE_BPS] {
        assert!(
            public_broadcaster_amount_split_for_tokens_and_protocol(
                entered,
                U256::MAX,
                FeeHandlingMode::AddToAmount,
                true,
                protocol_fee,
            )
            .is_err()
        );
    }
}

#[test]
fn different_token_fee_handling_preserves_selected_mode() {
    let entered = uint!(100_U256);
    let fee = uint!(7_U256);

    let deducted = public_broadcaster_amount_split_for_tokens(
        entered,
        fee,
        FeeHandlingMode::DeductFromAmount,
        false,
    )
    .expect("different-token split");

    assert_eq!(deducted.entered_amount, entered);
    assert_eq!(deducted.receiver_amount, entered);
    assert_eq!(deducted.total_private_spend, entered);
    assert_eq!(deducted.fee_amount, fee);
    assert_eq!(deducted.fee_mode, FeeHandlingMode::DeductFromAmount);

    let added = public_broadcaster_amount_split_for_tokens(
        entered,
        fee,
        FeeHandlingMode::AddToAmount,
        false,
    )
    .expect("different-token add split");

    assert_eq!(added.entered_amount, entered);
    assert_eq!(added.receiver_amount, entered);
    assert_eq!(added.total_private_spend, entered);
    assert_eq!(added.fee_amount, fee);
    assert_eq!(added.fee_mode, FeeHandlingMode::AddToAmount);
    assert_eq!(
        public_broadcaster_max_entered_amount_for_tokens(
            uint!(123_U256),
            fee,
            FeeHandlingMode::DeductFromAmount,
            false,
        ),
        uint!(123_U256)
    );
}

#[test]
fn public_broadcaster_build_error_distinguishes_fee_token_balance() {
    let report = public_broadcaster_build_error(
        BuildError::InsufficientFeeTokenBalance(uint!(123_U256)),
        uint!(7_U256),
        FeeHandlingMode::AddToAmount,
        false,
        U256::ZERO,
    );

    assert_eq!(
        report.to_string(),
        "public broadcaster fee-token max spendable: 123; required fee: 7"
    );
}

#[test]
fn fee_handling_mode_rejects_deducting_full_amount() {
    assert!(
        public_broadcaster_amount_split(
            uint!(7_U256),
            uint!(7_U256),
            FeeHandlingMode::DeductFromAmount,
        )
        .is_err()
    );
}

#[test]
fn public_broadcaster_max_entered_amount_depends_on_fee_handling() {
    let max_receiver_amount = uint!(100_U256);
    let fee = uint!(7_U256);

    assert_eq!(
        public_broadcaster_max_entered_amount(
            max_receiver_amount,
            fee,
            FeeHandlingMode::DeductFromAmount,
        ),
        uint!(107_U256)
    );
    assert_eq!(
        public_broadcaster_max_entered_amount(
            max_receiver_amount,
            fee,
            FeeHandlingMode::AddToAmount,
        ),
        max_receiver_amount
    );
}

#[test]
fn public_broadcaster_estimate_preserves_fee_handling_amount_split() {
    let token = address(0x25);
    let broadcaster = eligible_public_broadcasters(
        &[fee_row(
            1,
            token,
            1_000_000_000_000_000_000,
            0.9,
            "fee-mode",
        )],
        1,
        token,
        SystemTime::now(),
    )
    .into_iter()
    .next()
    .expect("candidate");
    let entered = uint!(1_000_000_000_U256);
    let selected_total = uint!(2_000_000_000_U256);

    let deducted = approximate_public_broadcaster_cost(
        broadcaster.clone(),
        crate::GAS_LIMIT_BUFFER,
        token,
        token,
        entered,
        FeeHandlingMode::DeductFromAmount,
        U256::ZERO,
        100,
        U256::ZERO,
        None,
        |_split| {
            let selection = selection_info(selected_total, 1, 1, 2, 0, selected_total);
            Ok(send_approximate_shape(&selection, selected_total))
        },
    )
    .expect("deduct estimate");
    assert_eq!(deducted.entered_amount, entered);
    assert_eq!(deducted.total_private_spend, entered);
    assert_eq!(deducted.receiver_amount + deducted.fee_amount, entered);
    assert_eq!(deducted.protocol_fee_amount, U256::ZERO);
    assert_eq!(deducted.recipient_amount, deducted.receiver_amount);
    assert_eq!(deducted.fee_mode, FeeHandlingMode::DeductFromAmount);
    let cushioned_gas_cost = U256::from(deducted.gas_limit) * U256::from(125);
    assert_eq!(deducted.native_gas_cost, cushioned_gas_cost);
    assert!(deducted.fee_amount >= cushioned_gas_cost);

    let added = approximate_public_broadcaster_cost(
        broadcaster,
        crate::GAS_LIMIT_BUFFER,
        token,
        token,
        entered,
        FeeHandlingMode::AddToAmount,
        U256::ZERO,
        100,
        U256::ZERO,
        None,
        |_split| {
            let selection = selection_info(selected_total, 1, 1, 2, 0, selected_total);
            Ok(send_approximate_shape(&selection, selected_total))
        },
    )
    .expect("add estimate");
    assert_eq!(added.entered_amount, entered);
    assert_eq!(added.receiver_amount, entered);
    assert_eq!(added.total_private_spend, entered + added.fee_amount);
    assert_eq!(added.protocol_fee_amount, U256::ZERO);
    assert_eq!(added.recipient_amount, added.receiver_amount);
    assert_eq!(added.fee_mode, FeeHandlingMode::AddToAmount);
}

#[test]
fn public_broadcaster_estimate_reports_separate_fee_token_amounts() {
    let action_token = address(0x41);
    let fee_token = address(0x42);
    let broadcaster = eligible_public_broadcasters(
        &[fee_row(
            1,
            fee_token,
            1_000_000_000_000_000_000,
            0.9,
            "separate-fee",
        )],
        1,
        fee_token,
        SystemTime::now(),
    )
    .into_iter()
    .next()
    .expect("candidate");
    let entered = uint!(1_000_000_000_U256);
    let max_receiver = uint!(2_000_000_000_U256);
    let seed_shape = ApproximateTransactionShape {
        transaction_count: 2,
        input_count: 2,
        private_output_count: 3,
        public_output_count: 0,
        max_receiver_amount: max_receiver,
        relay_call_count: 0,
        uses_relay_adapt: false,
        unwrap_count: 0,
        executor: false,
    };
    let initial_fee_amount = initial_separate_token_public_broadcaster_fee(
        &broadcaster,
        crate::GAS_LIMIT_BUFFER,
        100,
        seed_shape,
    );
    let mut observed_fee_amounts = Vec::new();

    let estimate = approximate_public_broadcaster_cost(
        broadcaster,
        crate::GAS_LIMIT_BUFFER,
        action_token,
        fee_token,
        entered,
        FeeHandlingMode::DeductFromAmount,
        U256::ZERO,
        100,
        initial_fee_amount,
        None,
        |split| {
            observed_fee_amounts.push(split.fee_amount);
            let selection = selection_info(max_receiver, 2, 2, 3, 0, max_receiver);
            Ok(send_approximate_shape(&selection, max_receiver))
        },
    )
    .expect("separate-token estimate");

    assert_eq!(estimate.action_token, action_token);
    assert_eq!(estimate.fee_token, fee_token);
    assert_eq!(estimate.entered_amount, entered);
    assert_eq!(estimate.receiver_amount, entered);
    assert_eq!(estimate.total_private_spend, entered);
    assert_eq!(estimate.recipient_amount, entered);
    assert_eq!(estimate.fee_mode, FeeHandlingMode::DeductFromAmount);
    assert_eq!(estimate.max_receiver_amount, max_receiver);
    assert_eq!(estimate.max_entered_amount, max_receiver);
    assert_eq!(estimate.transaction_count, 2);
    assert!(!initial_fee_amount.is_zero());
    assert_eq!(observed_fee_amounts.first(), Some(&initial_fee_amount));
    assert!(observed_fee_amounts.iter().all(|fee| !fee.is_zero()));
}

#[test]
fn public_broadcaster_unshield_estimate_includes_protocol_fee() {
    let token = address(0x26);
    let broadcaster = eligible_public_broadcasters(
        &[fee_row(
            1,
            token,
            1_000_000_000_000_000_000,
            0.9,
            "unshield-fee",
        )],
        1,
        token,
        SystemTime::now(),
    )
    .into_iter()
    .next()
    .expect("candidate");
    let entered = uint!(1_000_000_U256);
    let selected_total = uint!(2_000_000_U256);

    let estimate = approximate_public_broadcaster_cost(
        broadcaster,
        crate::GAS_LIMIT_BUFFER,
        token,
        token,
        entered,
        FeeHandlingMode::AddToAmount,
        RAILGUN_PROTOCOL_FEE_BPS,
        100,
        U256::ZERO,
        None,
        |_split| {
            let selection = selection_info(selected_total, 1, 1, 1, 1, selected_total);
            Ok(unshield_approximate_shape(
                &selection,
                selected_total,
                false,
            ))
        },
    )
    .expect("unshield estimate");

    let expected_fee = estimate.receiver_amount * uint!(25_U256) / uint!(10_000_U256);
    assert_eq!(estimate.protocol_fee_bps, RAILGUN_PROTOCOL_FEE_BPS);
    assert_eq!(estimate.receiver_amount, uint!(1_002_506_U256));
    assert_eq!(estimate.protocol_fee_amount, expected_fee);
    assert_eq!(estimate.recipient_amount, entered);
    assert_eq!(
        estimate.total_private_spend,
        estimate.receiver_amount + estimate.fee_amount
    );
}

#[test]
fn approximate_public_broadcaster_gas_tracks_transaction_shape() {
    let base = ApproximateTransactionShape {
        transaction_count: 1,
        input_count: 1,
        private_output_count: 2,
        public_output_count: 1,
        max_receiver_amount: U256::ZERO,
        relay_call_count: 1,
        uses_relay_adapt: true,
        unwrap_count: 1,
        executor: false,
    };
    let gas = |shape| {
        approximate_public_broadcaster_gas(&ETHEREUM_GAS_MODEL, GasEstimateMode::Expected, shape)
    };
    for larger in [
        ApproximateTransactionShape {
            transaction_count: 2,
            ..base
        },
        ApproximateTransactionShape {
            input_count: 2,
            ..base
        },
        ApproximateTransactionShape {
            private_output_count: 3,
            ..base
        },
        ApproximateTransactionShape {
            unwrap_count: 2,
            ..base
        },
        base.with_executor(true),
    ] {
        assert!(gas(larger) > gas(base), "{larger:?}");
    }
    // Polygon's repriced precompiles make the same transaction cost more.
    assert!(
        approximate_public_broadcaster_gas(&POLYGON_GAS_MODEL, GasEstimateMode::Expected, base)
            > gas(base)
    );
}

#[test]
fn public_broadcaster_fee_quote_includes_the_gas_limit_buffer_once() {
    let token = address(0x27);
    let broadcaster = eligible_public_broadcasters(
        &[fee_row(1, token, 1_000_000_000_000_000_000, 0.9, "buffer")],
        1,
        token,
        SystemTime::now(),
    )
    .remove(0);
    let selected_total = uint!(2_000_000_000_U256);
    let selection = selection_info(selected_total, 1, 1, 2, 0, selected_total);
    let shape = send_approximate_shape(&selection, selected_total);
    let buffer = 123_456;
    // The live check adds the buffer to the RPC estimate, so the quote must add it exactly once.
    let gas_limit =
        approximate_public_broadcaster_gas(&ETHEREUM_GAS_MODEL, GasEstimateMode::Expected, shape)
            + buffer;
    let estimate = approximate_public_broadcaster_cost(
        broadcaster.clone(),
        buffer,
        token,
        token,
        uint!(1_000_000_000_U256),
        FeeHandlingMode::DeductFromAmount,
        U256::ZERO,
        100,
        U256::ZERO,
        None,
        |_split| Ok(shape),
    )
    .expect("estimate");
    assert_eq!(estimate.gas_limit, gas_limit);
    assert_eq!(
        initial_separate_token_public_broadcaster_fee(&broadcaster, buffer, 100, shape),
        buffered_public_broadcaster_fee(broadcaster_fee_amount(
            broadcaster.fee,
            gas_limit,
            crate::public_broadcaster_service_gas_price(100),
        ))
    );
}

#[test]
fn public_broadcaster_gas_limit_uses_configured_buffer() {
    assert_eq!(
        public_broadcaster_gas_limit_with_buffer(210_000, 250_000),
        460_000
    );
}

#[test]
fn public_broadcaster_bound_min_gas_price_is_zero_on_arbitrum() {
    assert_eq!(public_broadcaster_bound_min_gas_price(42161, 21_000_000), 0);
    assert_eq!(public_broadcaster_bound_min_gas_price(42170, 21_000_000), 0);
    assert_eq!(
        public_broadcaster_bound_min_gas_price(421_614, 21_000_000),
        0
    );
    assert_eq!(
        public_broadcaster_bound_min_gas_price(1, 21_000_000),
        21_000_000
    );
}

#[test]
fn approximate_shapes_include_broadcaster_fee_output_and_change() {
    let send_selection = selection_info(uint!(15_U256), 2, 1, 3, 0, uint!(13_U256));
    let send = send_approximate_shape(&send_selection, uint!(13_U256));
    assert_eq!(send.input_count, 2);
    assert_eq!(send.transaction_count, 1);
    assert_eq!(send.private_output_count, 3);
    assert_eq!(send.public_output_count, 0);
    assert_eq!(send.relay_call_count, 0);
    assert!(!send.uses_relay_adapt);

    let unshield_selection = selection_info(uint!(12_U256), 1, 1, 1, 1, uint!(10_U256));
    let unshield = unshield_approximate_shape(&unshield_selection, uint!(10_U256), true);
    assert_eq!(unshield.input_count, 1);
    assert_eq!(unshield.transaction_count, 1);
    assert_eq!(unshield.private_output_count, 1);
    assert_eq!(unshield.public_output_count, 1);
    assert_eq!(unshield.relay_call_count, 1);
    assert!(unshield.uses_relay_adapt);
    assert_eq!(unshield.unwrap_count, 1);
}

#[test]
fn public_broadcaster_transact_envelope_roundtrips() {
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::transactCall;

    let (candidate, broadcaster) = sample_public_broadcaster_candidate(9);
    let call = transactCall {
        _transactions: Vec::new(),
    };
    let params = public_broadcaster_transact_params(
        &candidate,
        TransactionRequest::default()
            .to(address(0x33))
            .gas_limit(210_000)
            .input(call.abi_encode().into()),
        20_000_000_000,
        BTreeMap::new(),
    )
    .unwrap();

    let encrypted = EncryptedTransactRequest::encrypt_with_seed(
        candidate.viewing_public_key,
        &params,
        [8u8; 32],
    )
    .expect("encrypt request");
    let payload = encrypted.to_transact_payload().expect("serialize envelope");
    let value: serde_json::Value = serde_json::from_slice(&payload).expect("json envelope");
    assert_eq!(value["method"], "transact");
    // Check the wire spelling before typed decoding normalizes the public key.
    assert_eq!(value["params"]["pubkey"], hex::encode(encrypted.pubkey));
    assert_eq!(
        value["params"]["encryptedData"],
        serde_json::to_value(&encrypted.encrypted_data).unwrap()
    );
    assert_eq!(transact_topic(1), "/railgun/v2/0-1-transact/json");

    let pubkey: FixedBytes<32> =
        serde_json::from_value(value["params"]["pubkey"].clone()).expect("envelope public key");
    let encrypted_data = serde_json::from_value(value["params"]["encryptedData"].clone())
        .expect("envelope encrypted data");
    let decrypted =
        try_decrypt_transact_request(&broadcaster.viewing_private_key, pubkey.0, &encrypted_data)
            .expect("decrypt request")
            .expect("request for broadcaster");
    assert_eq!(decrypted.params.fees_id.as_deref(), Some("fees-id"));
    assert_eq!(decrypted.params.other["minVersion"], "8.0.0");
    assert_eq!(decrypted.params.other["maxVersion"], "8.999.0");
    assert!(!decrypted.params.other.contains_key("gasLimit"));
    assert_eq!(decrypted.params.other["useRelayAdapt"], false);
    // SDK broadcasters suppress unrecognized errors unless this wire flag is set.
    assert_eq!(decrypted.params.other["devLog"], true);
    assert_eq!(
        decrypted.params.min_gas_price,
        Some(uint!(20_000_000_000_U256))
    );
    assert!(
        decrypted
            .params
            .pre_transaction_pois_per_txid_leaf_per_list
            .is_empty()
    );

    let error = "execution reverted: recovery authorization nonce is already used";
    let response =
        DecryptedTransactResponse::encrypted_error_message(None, &decrypted.shared_key, error)
            .expect("encrypt detailed error");
    assert_eq!(
        decode_public_broadcaster_response(&encrypted.shared_key, &response).unwrap(),
        Some(PublicBroadcasterResultKind::Failed {
            error: error.into()
        })
    );
}

#[test]
fn public_broadcaster_transact_payload_includes_single_chunk_poi() {
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::{ActionData, relayCall};

    let (mut candidate, broadcaster) = sample_public_broadcaster_candidate(10);
    let call = relayCall {
        _transactions: Vec::new(),
        _actionData: ActionData {
            random: FixedBytes::ZERO,
            requireSuccess: true,
            minGasLimit: U256::ZERO,
            calls: Vec::new(),
        },
    };
    let list_key = FixedBytes::from([0x88; 32]);
    let txid_leaf = FixedBytes::from([0x99; 32]);
    candidate.required_poi_list_keys = vec![hex::encode(list_key)];
    let required_keys = candidate
        .parsed_required_poi_list_keys()
        .expect("required list keys");
    let params = public_broadcaster_transact_params(
        &candidate,
        TransactionRequest::default()
            .to(candidate.relay_adapt)
            .input(call.abi_encode().into()),
        20_000_000_000,
        sample_poi_map(&required_keys, &[txid_leaf]),
    )
    .unwrap();

    let encrypted = EncryptedTransactRequest::encrypt_with_seed(
        candidate.viewing_public_key,
        &params,
        [8u8; 32],
    )
    .expect("encrypt request");
    let decrypted = try_decrypt_transact_request(
        &broadcaster.viewing_private_key,
        encrypted.pubkey,
        &encrypted.encrypted_data,
    )
    .expect("decrypt request")
    .expect("request for broadcaster");

    assert_eq!(decrypted.params.other["useRelayAdapt"], true);
    let per_leaf = decrypted
        .params
        .pre_transaction_pois_per_txid_leaf_per_list
        .get(&list_key)
        .expect("list key");
    assert_eq!(per_leaf.len(), 1);
    assert!(per_leaf.contains_key(&txid_leaf));
}

#[test]
fn public_broadcaster_transact_payload_preserves_executor_authorization_and_all_pois() {
    use alloy::eips::eip7702::Authorization;
    use alloy::signers::{SignerSync, local::PrivateKeySigner};
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::{
        BoundParams, CommitmentPreimage, RelayAdapt7702, RelayAdapt7702ActionData, SnarkProof,
        Transaction,
    };
    use broadcaster_core::crypto::aes_gcm::{decrypt_in_place_16b_iv, split_iv_tag};
    use broadcaster_core::transact::{BroadcasterTransactRequestType, compute_railgun_txid};
    let (mut candidate, broadcaster) = sample_public_broadcaster_candidate(11);
    let list_keys = [FixedBytes::from([0x81; 32]), FixedBytes::from([0x82; 32])];
    let owner = PrivateKeySigner::from_bytes(&FixedBytes::repeat_byte(7)).unwrap();
    let chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    candidate.version = "alternative-implementation".to_owned();
    candidate.relay_adapt_7702 = Some(delegate);
    let transactions = [1, 2].map(|tree| Transaction {
        proof: SnarkProof::default(),
        merkleRoot: FixedBytes::repeat_byte(tree),
        nullifiers: vec![FixedBytes::repeat_byte(tree + 1)],
        commitments: vec![FixedBytes::repeat_byte(tree + 2)],
        boundParams: BoundParams::new_transact(
            u32::from(tree),
            0,
            1,
            Vec::new(),
            owner.address(),
            FixedBytes::ZERO,
        ),
        unshieldPreimage: CommitmentPreimage::empty(),
    });
    let leaves = transactions
        .iter()
        .map(|transaction| {
            FixedBytes::from(railgun_txid_leaf_hash(
                compute_railgun_txid(
                    transaction,
                    Some(broadcaster_core::transact::DEFAULT_TXID_VERSION),
                )
                .unwrap(),
                u64::from(transaction.boundParams.treeNumber),
            ))
        })
        .collect::<Vec<_>>();
    let action = RelayAdapt7702ActionData {
        requireSuccess: true,
        minGasLimit: U256::ZERO,
        calls: Vec::new(),
    };
    let hash = broadcaster_core::contracts::executor::execute_signing_hash(
        &transactions,
        &action,
        U256::from(3),
        1,
        owner.address(),
    );
    let call = RelayAdapt7702::executeCall {
        _transactions: transactions.into(),
        _actionData: action,
        _nonce: U256::from(3),
        _signature: owner.sign_hash_sync(&hash).unwrap().as_bytes().into(),
    };
    let authorization = Authorization {
        chain_id: U256::ONE,
        address: delegate,
        nonce: 7,
    };
    let signature = owner
        .sign_hash_sync(&authorization.signature_hash())
        .unwrap();
    let signed_authorization = authorization.into_signed(signature);
    let mut transaction = TransactionRequest::default()
        .to(owner.address())
        .input(call.abi_encode().into());
    transaction.transaction_type = Some(4);
    transaction.authorization_list = Some(vec![signed_authorization.clone()]);
    transaction.max_fee_per_gas = Some(25_000_000_000);
    transaction.max_priority_fee_per_gas = Some(1_000_000_000);
    transaction.gas = Some(1_495_776);
    candidate.required_poi_list_keys = list_keys.iter().map(hex::encode).collect();
    let required_keys = candidate
        .parsed_required_poi_list_keys()
        .expect("required list keys");
    let params = public_broadcaster_transact_params(
        &candidate,
        transaction.clone(),
        20_000_000_000,
        sample_poi_map(&required_keys, &leaves),
    )
    .unwrap();
    let mut missing_gas = transaction.clone();
    missing_gas.gas = None;
    assert!(
        public_broadcaster_transact_params(
            &candidate,
            missing_gas,
            20_000_000_000,
            sample_poi_map(&required_keys, &leaves),
        )
        .is_err()
    );
    let mut delegated = transaction.clone();
    delegated.transaction_type = Some(2);
    delegated.authorization_list = None;
    // An installed delegation still needs fresh authorization to select the
    // SDK broadcaster's TX7702 route instead of its ordinary contract route.
    assert!(
        public_broadcaster_transact_params(
            &candidate,
            delegated,
            20_000_000_000,
            sample_poi_map(&required_keys, &leaves),
        )
        .is_err()
    );
    for list in list_keys {
        let mut incomplete = sample_poi_map(&required_keys, &leaves);
        incomplete.get_mut(&list).unwrap().remove(&leaves[1]);
        assert!(
            public_broadcaster_transact_params(
                &candidate,
                transaction.clone(),
                20_000_000_000,
                incomplete
            )
            .is_err()
        );
    }
    let mut incompatible = candidate.clone();
    incompatible.relay_adapt_7702 = Some(Address::repeat_byte(0x23));
    assert!(
        public_broadcaster_transact_params(
            &incompatible,
            transaction,
            20_000_000_000,
            sample_poi_map(&required_keys, &leaves)
        )
        .is_err()
    );

    let encrypted = EncryptedTransactRequest::encrypt_with_seed(
        candidate.viewing_public_key,
        &params,
        [8u8; 32],
    )
    .expect("encrypt request");
    let decrypted = try_decrypt_transact_request(
        &broadcaster.viewing_private_key,
        encrypted.pubkey,
        &encrypted.encrypted_data,
    )
    .expect("decrypt request")
    .expect("request for broadcaster");

    assert_eq!(
        decrypted.params.transact_type,
        Some(BroadcasterTransactRequestType::Tx7702)
    );
    assert_eq!(
        decrypted.params.other.get("gasLimit"),
        Some(&serde_json::json!("1495776"))
    );
    assert_eq!(
        decrypted.params.other.get("useRelayAdapt"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        decrypted.params.max_fee_per_gas,
        Some(U256::from(25_000_000_000_u64))
    );
    assert_eq!(
        decrypted.params.max_priority_fee_per_gas,
        Some(U256::from(1_000_000_000_u64))
    );
    assert_eq!(
        decrypted
            .params
            .authorization
            .unwrap()
            .signed_authorization()
            .unwrap(),
        signed_authorization
    );
    assert_eq!(decrypted.params.data.as_ref(), call.abi_encode());
    // Inspect the encrypted JSON itself: typed decoding accepts both prefixes
    // and would hide an incompatible spelling from the SDK's string lookups.
    let (iv, tag) = split_iv_tag(encrypted.encrypted_data[0].as_ref().try_into().unwrap());
    let mut plaintext = encrypted.encrypted_data[1].to_vec();
    decrypt_in_place_16b_iv(&decrypted.shared_key, &iv, &tag, &mut plaintext).unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&plaintext).unwrap();
    let poi_map = decrypted.params.pre_transaction_pois_per_txid_leaf_per_list;
    assert_eq!(poi_map.len(), 2);
    for list_key in list_keys {
        let per_leaf = poi_map.get(&list_key).expect("list key");
        assert_eq!(per_leaf.len(), 2);
        for leaf in &leaves {
            assert!(per_leaf.contains_key(leaf));
            let wire_poi = &wire["preTransactionPOIsPerTxidLeafPerList"][hex::encode(list_key)]
                [hex::encode(leaf)];
            assert_eq!(
                wire_poi["txidMerkleroot"],
                hex::encode(per_leaf[leaf].txid_merkleroot)
            );
            assert_eq!(
                wire_poi["poiMerkleroots"],
                serde_json::json!(
                    per_leaf[leaf]
                        .poi_merkleroots
                        .iter()
                        .map(hex::encode)
                        .collect::<Vec<_>>()
                )
            );
        }
    }
}

#[test]
fn public_broadcaster_invalid_poi_list_key_fails_preparation() {
    let (mut candidate, _) = sample_public_broadcaster_candidate(12);
    candidate.required_poi_list_keys = vec!["poi-list".to_string()];

    let error = candidate
        .parsed_required_poi_list_keys()
        .expect_err("invalid POI list key should fail");

    assert!(error.to_string().contains("invalid required POI list key"));
}

#[test]
fn public_broadcaster_response_decodes_tx_hash() {
    let shared_key = [7u8; 32];
    let tx_hash = TxHash::from([3u8; 32]);
    let response = DecryptedTransactResponse::encrypted_tx_hash_message(None, &shared_key, tx_hash)
        .expect("response payload");

    let decoded = decode_public_broadcaster_response(&shared_key, &response)
        .expect("decode response")
        .expect("decryptable response");

    assert_eq!(
        decoded,
        PublicBroadcasterResultKind::Submitted {
            tx_hash: tx_hash.to_string()
        }
    );
}

/// Submissions on fake submission nodes, which report what they were built from and did.
mod submission_nodes {
    use super::*;
    use crate::desktop::{
        SubmissionNode, SubmissionNodeSpec, SubmissionRouteHealth,
        submit_public_broadcaster_transaction,
    };
    use crate::http::{OperationHttpClient, OperationNetworkIsolation};
    use crate::rpc_broker::tests::{spawn_recording_rpc_mock, tagged_reqwest_client};
    use crate::{
        HttpContext, PublicBroadcasterWaku, SubmissionNodeScope, SubmissionRouteProgress,
        SubmissionRouteSender, SubmissionRouteState, WalletNetworkMode, WalletTorClientProvider,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::{mpsc, watch};
    use tokio::time::Instant;
    use waku::proto::WakuMessage;
    use waku_relay::client::{
        AdditionalPeer, Client as WakuClient, ClientConfig, RelayNetworkConfig, RelayNetworkMode,
    };

    const RESPONSE_TOPIC: &str = "/railgun/v2/0-1-transact-response/json";
    const REPUBLISH_INTERVAL: Duration = Duration::from_secs(5);

    enum NodeEvent {
        Subscribed(usize, mpsc::Sender<WakuMessage>),
        Published(usize, Vec<u8>),
        ShutDown(usize),
        Dropped(usize),
    }

    /// The route of a harness's nodes, which a test drops and restores.
    struct FakeRoute {
        /// Connected peers. The route is ready while there are any.
        peers: watch::Sender<usize>,
        refreshes: AtomicUsize,
        /// Publications fail, after they are reported, while set.
        failing: AtomicBool,
    }

    impl FakeRoute {
        fn set_peers(&self, peers: usize) {
            self.peers.send_replace(peers);
        }

        fn refreshes(&self) -> usize {
            self.refreshes.load(Ordering::SeqCst)
        }
    }

    struct FakeNode {
        index: usize,
        route: Arc<FakeRoute>,
        /// Every publication after the first never completes.
        hang_republish: bool,
        publications: AtomicUsize,
        events: mpsc::UnboundedSender<NodeEvent>,
    }

    impl Drop for FakeNode {
        fn drop(&mut self) {
            let _ = self.events.send(NodeEvent::Dropped(self.index));
        }
    }

    #[async_trait::async_trait]
    impl SubmissionNode for FakeNode {
        fn local_peer_id(&self) -> Option<waku::PeerId> {
            None
        }

        fn pubsub_path(&self) -> &'static str {
            "/waku/2/rs/5/1"
        }

        fn route_health(&self) -> SubmissionRouteHealth {
            let peers = *self.route.peers.borrow();
            SubmissionRouteHealth {
                peers,
                lightpush: peers > 0,
                filter: peers > 0,
            }
        }

        fn refresh_route(&self) -> bool {
            self.route.refreshes.fetch_add(1, Ordering::SeqCst);
            true
        }

        async fn wait_ready(&self, bound: Duration) -> eyre::Result<()> {
            let mut peers = self.route.peers.subscribe();
            let connected = matches!(
                tokio::time::timeout(bound, peers.wait_for(|count| *count > 0)).await,
                Ok(Ok(_))
            );
            if connected {
                Ok(())
            } else {
                Err(eyre::eyre!("no peer connected"))
            }
        }

        async fn subscribe(
            &self,
            content_topics: Vec<String>,
        ) -> eyre::Result<mpsc::Receiver<WakuMessage>> {
            assert_eq!(content_topics, [RESPONSE_TOPIC]);
            let (responses_tx, responses) = mpsc::channel(4);
            let _ = self
                .events
                .send(NodeEvent::Subscribed(self.index, responses_tx));
            Ok(responses)
        }

        async fn publish(&self, content_topic: &str, payload: &[u8]) -> eyre::Result<()> {
            assert_eq!(content_topic, transact_topic(1));
            let _ = self
                .events
                .send(NodeEvent::Published(self.index, payload.to_vec()));
            if self.hang_republish && self.publications.fetch_add(1, Ordering::SeqCst) > 0 {
                std::future::pending::<()>().await;
            }
            if self.route.failing.load(Ordering::SeqCst) {
                return Err(eyre::eyre!("publish failed"));
            }
            Ok(())
        }

        async fn shutdown(&self, _bound: Duration) -> eyre::Result<()> {
            let _ = self.events.send(NodeEvent::ShutDown(self.index));
            Ok(())
        }
    }

    struct Harness {
        waku: PublicBroadcasterWaku,
        scope: SubmissionNodeScope,
        specs: Arc<Mutex<Vec<SubmissionNodeSpec>>>,
        events: mpsc::UnboundedReceiver<NodeEvent>,
        route: Arc<FakeRoute>,
        /// The route watch of every spawned submission.
        route_tx: SubmissionRouteSender,
    }

    /// Submissions seeded from `root`, with fake nodes numbered in build order.
    fn build_harness(
        root: Arc<WakuClient>,
        config: ClientConfig,
        http: HttpContext,
        ready: bool,
        hang_republish: bool,
    ) -> Harness {
        let scope = SubmissionNodeScope::new();
        let specs = Arc::new(Mutex::new(Vec::new()));
        let (events_tx, events) = mpsc::unbounded_channel();
        let route = Arc::new(FakeRoute {
            peers: watch::Sender::new(if ready { 3 } else { 0 }),
            refreshes: AtomicUsize::new(0),
            failing: AtomicBool::new(false),
        });
        let built = Arc::clone(&specs);
        let node_route = Arc::clone(&route);
        let waku = PublicBroadcasterWaku::new(root, config, http, scope.clone())
            .with_node_factory_for_tests(move |spec| {
                let mut built = built.lock().unwrap();
                built.push(spec);
                let node: Box<dyn SubmissionNode> = Box::new(FakeNode {
                    index: built.len() - 1,
                    route: Arc::clone(&node_route),
                    hang_republish,
                    publications: AtomicUsize::new(0),
                    events: events_tx.clone(),
                });
                Ok(node)
            });
        Harness {
            waku,
            scope,
            specs,
            events,
            route,
            route_tx: watch::Sender::new(SubmissionRouteProgress::default()),
        }
    }

    /// A root whose Waku is off: its snapshot is empty, and it can't subscribe or publish.
    fn disabled_root() -> Arc<WakuClient> {
        Arc::new(
            WakuClient::new_with_network(
                &ClientConfig::default(),
                RelayNetworkConfig::proxy(reqwest::Client::new()),
            )
            .unwrap(),
        )
    }

    /// A root that knows `count` configured peers, at a local port nothing listens on.
    fn seeded_root(count: usize) -> Arc<WakuClient> {
        let config = ClientConfig {
            direct_peers: (0..count)
                .map(|_| AdditionalPeer {
                    peer_id: libp2p::PeerId::random().to_string(),
                    addrs: vec!["/ip4/127.0.0.1/tcp/1".to_owned()],
                })
                .collect(),
            dns_enr_trees: Some(Vec::new()),
            ..ClientConfig::default()
        };
        Arc::new(WakuClient::new_with_network(&config, RelayNetworkConfig::direct()).unwrap())
    }

    /// Tor-mode networking whose every operation client is new, carries `x-route: submission`,
    /// and has its own Tor provider. Each is recorded in `opened`.
    fn tor_http(opened: Arc<Mutex<Vec<OperationHttpClient>>>) -> HttpContext {
        HttpContext::with_rpc_client_for_tests(reqwest::Client::new(), WalletNetworkMode::Tor)
            .with_operation_clients_for_tests(move || {
                let provider: WalletTorClientProvider = Arc::new(|| None);
                let operation = OperationHttpClient::for_tests(
                    tagged_reqwest_client("submission"),
                    OperationNetworkIsolation::Dedicated,
                )
                .with_tor_client_provider_for_tests(provider);
                opened.lock().unwrap().push(operation.clone());
                Ok(operation)
            })
    }

    async fn submit(
        waku: &PublicBroadcasterWaku,
        broadcaster: &PublicBroadcasterCandidate,
        timeout: Duration,
    ) -> eyre::Result<PublicBroadcasterResultKind> {
        submit_watched(waku, broadcaster, timeout, None).await
    }

    async fn submit_watched(
        waku: &PublicBroadcasterWaku,
        broadcaster: &PublicBroadcasterCandidate,
        timeout: Duration,
        route_tx: Option<SubmissionRouteSender>,
    ) -> eyre::Result<PublicBroadcasterResultKind> {
        submit_public_broadcaster_transaction(
            waku,
            TransactionRequest::default()
                .to(broadcaster.relay_adapt)
                .input(Bytes::from_static(&[1, 2, 3]).into()),
            BTreeMap::default(),
            broadcaster,
            1_000_000_000,
            None,
            route_tx,
            timeout,
            REPUBLISH_INTERVAL,
        )
        .await
    }

    /// A submission that reports its route on the harness's watch.
    fn spawn_submission(
        harness: &Harness,
        broadcaster: &PublicBroadcasterCandidate,
        timeout: Duration,
    ) -> tokio::task::JoinHandle<eyre::Result<PublicBroadcasterResultKind>> {
        let (waku, broadcaster) = (harness.waku.clone(), broadcaster.clone());
        let route_tx = harness.route_tx.clone();
        tokio::spawn(
            async move { submit_watched(&waku, &broadcaster, timeout, Some(route_tx)).await },
        )
    }

    async fn next_event(events: &mut mpsc::UnboundedReceiver<NodeEvent>) -> NodeEvent {
        events.recv().await.expect("node event")
    }

    /// The broadcaster's encrypted `txHash` answer to the transact request `payload`.
    fn tx_hash_response(viewing: &ViewingKeyData, payload: &[u8]) -> WakuMessage {
        let envelope: serde_json::Value = serde_json::from_slice(payload).unwrap();
        let pubkey: FixedBytes<32> =
            serde_json::from_value(envelope["params"]["pubkey"].clone()).unwrap();
        let encrypted_data =
            serde_json::from_value(envelope["params"]["encryptedData"].clone()).unwrap();
        let request =
            try_decrypt_transact_request(&viewing.viewing_private_key, pubkey.0, &encrypted_data)
                .unwrap()
                .unwrap();
        WakuMessage {
            content_topic: RESPONSE_TOPIC.to_owned(),
            payload: DecryptedTransactResponse::encrypted_tx_hash_message(
                None,
                &request.shared_key,
                TxHash::repeat_byte(3),
            )
            .unwrap(),
            ..WakuMessage::default()
        }
    }

    /// The root's Waku is off here, so a submission that subscribed or published on it would
    /// fail.
    #[tokio::test(start_paused = true)]
    async fn submissions_publish_on_their_own_nodes_and_tor_isolations() {
        let opened = Arc::default();
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::clone(&opened)),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);

        for _ in 0..2 {
            assert_eq!(
                submit(&harness.waku, &broadcaster, Duration::from_secs(1))
                    .await
                    .unwrap(),
                PublicBroadcasterResultKind::TimedOut
            );
        }

        {
            let opened = opened.lock().unwrap();
            let specs = harness.specs.lock().unwrap();
            assert_eq!((opened.len(), specs.len()), (2, 2));
            assert!(!Arc::ptr_eq(
                &opened[0].tor_client_provider().unwrap(),
                &opened[1].tor_client_provider().unwrap()
            ));
            for (spec, operation) in specs.iter().zip(opened.iter()) {
                assert_eq!(spec.network.mode, RelayNetworkMode::Tor);
                assert!(Arc::ptr_eq(
                    spec.network.tor_client.as_ref().unwrap(),
                    &operation.tor_client_provider().unwrap()
                ));
            }
        }
        let mut publishers = Vec::new();
        while let Ok(event) = harness.events.try_recv() {
            if let NodeEvent::Published(node, _) = event {
                publishers.push(node);
            }
        }
        assert_eq!(publishers, [0, 1]);
    }

    #[tokio::test]
    async fn submission_nodes_seed_from_the_root_snapshot_or_run_their_own_discovery() {
        let configured = ClientConfig {
            dns_enr_trees: Some(vec!["enrtree://configured@nodes.example.org".to_owned()]),
            max_peers: Some(40),
            ..ClientConfig::default()
        };

        // Three of the root's peers, and no discovery of the node's own.
        let root = seeded_root(5);
        let snapshot = root.seed_peers();
        assert_eq!(snapshot.len(), 5);
        let seeded = ClientConfig {
            backup_peers: snapshot[..1].to_vec(),
            ..configured.clone()
        };
        let harness = build_harness(root, seeded, tor_http(Arc::default()), true, false);
        drop(harness.waku.start_node().await.unwrap());
        let spec = harness.specs.lock().unwrap().pop().unwrap();
        let seeds = &spec.config.direct_peers;
        assert!(seeds.iter().all(|seed| snapshot.contains(seed)));
        let distinct: std::collections::HashSet<_> =
            seeds.iter().map(|seed| &seed.peer_id).collect();
        assert_eq!((seeds.len(), distinct.len()), (3, 3));
        assert_eq!(spec.config.dns_enr_trees, Some(Vec::new()));
        assert_eq!(spec.config.backup_peers, []);
        assert_eq!(spec.config.max_peers, Some(3));

        // No seed: the configured trees, resolved over the submission's operation client.
        let (url, recorded, server) =
            spawn_recording_rpc_mock(Arc::new(|_| serde_json::Value::Null)).await;
        let harness = build_harness(
            disabled_root(),
            configured.clone(),
            tor_http(Arc::default()),
            true,
            false,
        );
        drop(harness.waku.start_node().await.unwrap());
        let spec = harness.specs.lock().unwrap().pop().unwrap();
        assert_eq!(spec.config.direct_peers, []);
        assert_eq!(spec.config.dns_enr_trees, configured.dns_enr_trees);
        assert_eq!(spec.network.mode, RelayNetworkMode::Tor);
        spec.network
            .http_client
            .unwrap()
            .post(url)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(recorded.lock().unwrap()[0].0.as_deref(), Some("submission"));
        server.abort();

        // No seed and discovery disabled: no node, nothing published.
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig {
                dns_enr_trees: Some(Vec::new()),
                ..configured
            },
            tor_http(Arc::default()),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        let error = submit(&harness.waku, &broadcaster, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("DNS discovery is disabled"));
        assert!(harness.specs.lock().unwrap().is_empty());
        assert!(harness.events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_submission_node_that_never_connects_fails_with_nothing_published() {
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::default()),
            false,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);

        let error = submit(&harness.waku, &broadcaster, Duration::from_mins(2))
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Waku delivery network is unavailable")
        );
        // Dropped without subscribing, publishing or a teardown delay.
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Dropped(0)
        ));
        assert!(harness.events.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_nwaku_publish_of_a_submission_node_uses_its_operation_client() {
        let (url, recorded, server) =
            spawn_recording_rpc_mock(Arc::new(|_| serde_json::Value::Null)).await;
        let nwaku_url = url.as_str().trim_end_matches('/').to_owned();
        let http = HttpContext::with_rpc_client_for_tests(
            reqwest::Client::new(),
            WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(|| {
            Ok(OperationHttpClient::for_tests(
                tagged_reqwest_client("submission"),
                OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
            ))
        });
        let harness = build_harness(
            seeded_root(1),
            ClientConfig {
                nwaku_url: Some(nwaku_url.clone()),
                ..ClientConfig::default()
            },
            http,
            true,
            false,
        );
        drop(harness.waku.start_node().await.unwrap());
        let spec = harness.specs.lock().unwrap().pop().unwrap();
        assert_eq!(spec.config.nwaku_url, Some(nwaku_url));

        // A node built from the spec. Its seed refuses connections, so only nwaku receives this.
        let node = WakuClient::new_with_network(&spec.config, spec.network).unwrap();
        node.publish(&transact_topic(1), b"{}").await.unwrap();

        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0.as_deref(), Some("submission"));
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_republish_delays_no_result_and_the_node_ends_after_its_tail() {
        const TIMEOUT: Duration = Duration::from_secs(7);
        let (broadcaster, viewing) = sample_public_broadcaster_candidate(11);
        for respond in [true, false] {
            let mut harness = build_harness(
                disabled_root(),
                ClientConfig::default(),
                tor_http(Arc::default()),
                true,
                true,
            );
            let started = Instant::now();
            let submission = spawn_submission(&harness, &broadcaster, TIMEOUT);
            let NodeEvent::Subscribed(0, responses) = next_event(&mut harness.events).await else {
                panic!("the node subscribes before publishing");
            };
            let NodeEvent::Published(0, request) = next_event(&mut harness.events).await else {
                panic!("the node publishes the request");
            };
            // The first republish never completes.
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::Published(0, _)
            ));

            let expected = if respond {
                responses
                    .send(tx_hash_response(&viewing, &request))
                    .await
                    .unwrap();
                PublicBroadcasterResultKind::Submitted {
                    tx_hash: TxHash::repeat_byte(3).to_string(),
                }
            } else {
                PublicBroadcasterResultKind::TimedOut
            };
            assert_eq!(submission.await.unwrap().unwrap(), expected);
            let ended = Instant::now();
            let deadline = if respond { REPUBLISH_INTERVAL } else { TIMEOUT };
            assert!(ended - started < deadline + Duration::from_secs(1));

            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::ShutDown(0)
            ));
            let tail = ended.elapsed();
            assert!(
                (Duration::from_secs(10)..=Duration::from_secs(60)).contains(&tail),
                "{tail:?}"
            );
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::Dropped(0)
            ));
        }
    }

    /// Stop and executor close drop the submission's future, and lock then closes the scope.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_submission_retires_its_node_through_the_tail_without_publishing() {
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        for close_scope in [false, true] {
            let mut harness = build_harness(
                disabled_root(),
                ClientConfig::default(),
                tor_http(Arc::default()),
                true,
                false,
            );
            let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
            // Held open so that the submission is still waiting when it is aborted.
            let NodeEvent::Subscribed(0, _responses) = next_event(&mut harness.events).await else {
                panic!("the node subscribes before publishing");
            };
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::Published(0, _)
            ));

            submission.abort();
            assert!(submission.await.unwrap_err().is_cancelled());
            // Closed at once, while the node is still retiring.
            assert_eq!(
                harness.route_tx.borrow().state,
                SubmissionRouteState::Closed
            );
            let aborted = Instant::now();
            if close_scope {
                harness.scope.close();
            }

            // Any republish would arrive before the shutdown.
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::ShutDown(0)
            ));
            let tail = aborted.elapsed();
            let expected = if close_scope {
                Duration::ZERO..=Duration::from_secs(1)
            } else {
                Duration::from_secs(10)..=Duration::from_secs(60)
            };
            assert!(expected.contains(&tail), "{tail:?}");
            assert!(matches!(
                next_event(&mut harness.events).await,
                NodeEvent::Dropped(0)
            ));
            assert!(harness.events.try_recv().is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_scope_ends_a_tail_at_once_and_refuses_new_nodes() {
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::default()),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);

        // Publications at 0, 5 and 10 seconds, then the timeout.
        let result = spawn_submission(&harness, &broadcaster, Duration::from_secs(12)).await;
        assert_eq!(
            result.unwrap().unwrap(),
            PublicBroadcasterResultKind::TimedOut
        );
        let mut publications = 0;
        while let Ok(event) = harness.events.try_recv() {
            publications += usize::from(matches!(event, NodeEvent::Published(0, _)));
        }
        assert_eq!(publications, 3);

        // Shorter than any tail, and long enough for two more republishes.
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert!(harness.events.try_recv().is_err());

        let closed = Instant::now();
        harness.scope.close();
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::ShutDown(0)
        ));
        assert!(closed.elapsed() < Duration::from_secs(1));
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Dropped(0)
        ));

        // A callback that still holds the closed scope gets no node.
        let error = submit(&harness.waku, &broadcaster, Duration::from_secs(12))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("locked"));
        assert_eq!(harness.specs.lock().unwrap().len(), 1);
        assert!(harness.events.try_recv().is_err());
    }

    fn route_progress(
        state: SubmissionRouteState,
        peers: usize,
        recoveries_used: u32,
        publishes: u32,
    ) -> SubmissionRouteProgress {
        SubmissionRouteProgress {
            state,
            peers,
            recoveries_used,
            publishes,
        }
    }

    /// The node keeps its identity: no second node is built, and node 0 publishes throughout.
    #[tokio::test(start_paused = true)]
    async fn a_lost_route_is_recovered_on_new_circuits_and_its_response_accepted() {
        let opened = Arc::default();
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::clone(&opened)),
            true,
            false,
        );
        let (broadcaster, viewing) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let at = |millis| started + Duration::from_millis(millis);
        let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
        let NodeEvent::Subscribed(0, responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        let NodeEvent::Published(0, request) = next_event(&mut harness.events).await else {
            panic!("the node publishes the request");
        };
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Ready, 3, 0, 1)
        );

        // Lost from the first poll, at 1 s, so the grace ends at 11 s.
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(10_500)).await;
        assert_eq!(harness.route.refreshes(), 0);
        tokio::time::sleep_until(at(11_500)).await;
        let reisolations = opened.lock().unwrap()[0].reisolations_for_tests();
        assert_eq!((reisolations, harness.route.refreshes()), (1, 1));
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Reconnecting, 0, 1, 1)
        );
        // Nothing went out while the route was down, though a republish was due at 5 s.
        assert!(harness.events.try_recv().is_err());

        harness.route.set_peers(2);
        let NodeEvent::Published(0, republished) = next_event(&mut harness.events).await else {
            panic!("the recovered node republishes");
        };
        assert_eq!(republished, request);
        assert!((at(11_500)..at(12_000)).contains(&Instant::now()));
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Ready, 2, 1, 2)
        );

        responses
            .send(tx_hash_response(&viewing, &request))
            .await
            .unwrap();
        assert_eq!(
            submission.await.unwrap().unwrap(),
            PublicBroadcasterResultKind::Submitted {
                tx_hash: TxHash::repeat_byte(3).to_string(),
            }
        );
        // Closed with its counts kept.
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Closed, 2, 1, 2)
        );
        assert_eq!(harness.specs.lock().unwrap().len(), 1);
    }

    /// A republish left pending by the route's loss doesn't hold off the recovered route's
    /// resend.
    #[tokio::test(start_paused = true)]
    async fn a_recovered_route_resends_at_once_past_a_pending_publish() {
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::default()),
            true,
            true,
        );
        let (broadcaster, viewing) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let at = |millis| started + Duration::from_millis(millis);
        let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
        let NodeEvent::Subscribed(0, responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        let NodeEvent::Published(0, request) = next_event(&mut harness.events).await else {
            panic!("the node publishes the request");
        };
        // The republish at 5 s never completes. The route is lost from the poll at 6 s, so
        // recovery starts at 16 s.
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(16_500)).await;
        assert_eq!(harness.route.refreshes(), 1);

        harness.route.set_peers(2);
        let NodeEvent::Published(0, resent) = next_event(&mut harness.events).await else {
            panic!("the recovered node resends");
        };
        assert_eq!(resent, request);
        assert!((at(16_500)..at(17_500)).contains(&Instant::now()));
        assert_eq!(harness.route_tx.borrow().publishes, 3);

        responses
            .send(tx_hash_response(&viewing, &request))
            .await
            .unwrap();
        assert_eq!(
            submission.await.unwrap().unwrap(),
            PublicBroadcasterResultKind::Submitted {
                tx_hash: TxHash::repeat_byte(3).to_string(),
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_short_drop_recovers_nothing_and_publishes_count_only_on_a_ready_route() {
        let opened = Arc::default();
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::clone(&opened)),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let at = |millis| started + Duration::from_millis(millis);
        let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
        let NodeEvent::Subscribed(0, _responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));

        // Down for the polls at 1 to 10 s, 9 s apart: short of the grace.
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(10_500)).await;
        assert_eq!(harness.route_tx.borrow().publishes, 1);
        harness.route.set_peers(3);
        // The republish due at 5 s goes out at the first ready poll, and the next one fails.
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));
        assert_eq!(Instant::now(), at(11_000));
        harness.route.failing.store(true, Ordering::SeqCst);
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));
        assert_eq!(Instant::now(), at(16_000));
        tokio::time::sleep_until(at(16_500)).await;

        assert_eq!(harness.route_tx.borrow().publishes, 3);
        let reisolations = opened.lock().unwrap()[0].reisolations_for_tests();
        assert_eq!((reisolations, harness.route.refreshes()), (0, 0));
        submission.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn route_recovery_stops_at_its_limit_and_the_submission_times_out_on_time() {
        const TIMEOUT: Duration = Duration::from_secs(100);
        let opened = Arc::default();
        let mut harness = build_harness(
            disabled_root(),
            ClientConfig::default(),
            tor_http(Arc::clone(&opened)),
            true,
            false,
        );
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let at = |millis| started + Duration::from_millis(millis);
        let submission = spawn_submission(&harness, &broadcaster, TIMEOUT);
        let NodeEvent::Subscribed(0, _responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));

        // The first recovery starts at 11 s and waits out its 30 s; the second follows at once.
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(40_500)).await;
        assert_eq!(harness.route.refreshes(), 1);
        tokio::time::sleep_until(at(41_500)).await;
        assert_eq!(harness.route.refreshes(), 2);
        harness.route.set_peers(3);
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));

        // Lost again, with no recovery left.
        harness.route.set_peers(0);
        tokio::time::sleep_until(at(99_500)).await;
        let reisolations = opened.lock().unwrap()[0].reisolations_for_tests();
        assert_eq!((reisolations, harness.route.refreshes()), (2, 2));
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Reconnecting, 0, 2, 2)
        );
        assert_eq!(
            submission.await.unwrap().unwrap(),
            PublicBroadcasterResultKind::TimedOut
        );
        assert!((at(100_000)..at(100_500)).contains(&Instant::now()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_direct_mode_route_reconnects_on_its_own_without_recovery() {
        let opened: Arc<Mutex<Vec<OperationHttpClient>>> = Arc::default();
        let recorded = Arc::clone(&opened);
        let http = HttpContext::with_rpc_client_for_tests(
            reqwest::Client::new(),
            WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(move || {
            let operation = OperationHttpClient::for_tests(
                reqwest::Client::new(),
                OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
            );
            recorded.lock().unwrap().push(operation.clone());
            Ok(operation)
        });
        let mut harness =
            build_harness(disabled_root(), ClientConfig::default(), http, true, false);
        let (broadcaster, _) = sample_public_broadcaster_candidate(11);
        let started = Instant::now();
        let submission = spawn_submission(&harness, &broadcaster, Duration::from_mins(2));
        let NodeEvent::Subscribed(0, _responses) = next_event(&mut harness.events).await else {
            panic!("the node subscribes before publishing");
        };
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));

        // Well past the grace.
        harness.route.set_peers(0);
        tokio::time::sleep_until(started + Duration::from_millis(20_500)).await;
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Reconnecting, 0, 0, 1)
        );
        let reisolations = opened.lock().unwrap()[0].reisolations_for_tests();
        assert_eq!((reisolations, harness.route.refreshes()), (0, 0));

        // The next poll finds the node ready and sends the overdue republish.
        harness.route.set_peers(2);
        assert!(matches!(
            next_event(&mut harness.events).await,
            NodeEvent::Published(0, _)
        ));
        assert_eq!(
            *harness.route_tx.borrow(),
            route_progress(SubmissionRouteState::Ready, 2, 0, 2)
        );
        submission.abort();
    }
}
