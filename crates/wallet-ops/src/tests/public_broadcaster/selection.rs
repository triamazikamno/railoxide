use super::*;

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
fn submission_keeps_the_reviewed_quote_when_advertisements_refresh() {
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
    let select_reviewed =
        |delivery,
         selection: &PublicBroadcasterSelection,
         anchor,
         trust: &PublicBroadcasterTrustFilter,
         reviewed: Option<&PublicBroadcasterCandidate>| {
            crate::desktop::public_broadcaster_for_request(
                &rows,
                1,
                reserved.token,
                selection,
                policy,
                trust,
                anchor,
                delivery,
                reviewed,
            )
        };
    let select = |delivery,
                  selection: &PublicBroadcasterSelection,
                  anchor,
                  trust: &PublicBroadcasterTrustFilter| {
        select_reviewed(delivery, selection, anchor, trust, None)
    };
    let random = PublicBroadcasterSelection::Random;
    let specific = PublicBroadcasterSelection::Specific {
        railgun_address: reserved.railgun_address.clone(),
    };
    assert_eq!(
        select(None, &random, None, &trust).unwrap().fees_id,
        "refreshed-fees"
    );
    for selection in [random.clone(), specific.clone()] {
        let selected = select(Some(&delivery), &selection, None, &trust).unwrap();
        assert_eq!(selected.fees_id, reserved.fees_id);
        assert_eq!(selected.fee, reserved.fee);
        assert_eq!(selected.fee_expiration, reserved.fee_expiration);
    }
    // A reviewed quote outranks the same broadcaster's refreshed higher-fee row.
    let reviewed = select_reviewed(None, &specific, None, &trust, Some(&reserved)).unwrap();
    assert_eq!(
        (reviewed.fees_id, reviewed.fee),
        (reserved.fees_id.clone(), reserved.fee)
    );
    assert!(select(Some(&delivery), &random, Some(U256::from(100)), &trust).is_err());
    let favorites_only = PublicBroadcasterTrustFilter {
        favorites_only: true,
        ..trust
    };
    assert!(select(Some(&delivery), &random, None, &favorites_only).is_err());
    let mut expired = reserved.clone();
    expired.fee_expiration = SystemTime::now() - Duration::from_secs(1);
    // Fee expiry is informational for reviewed and executor quotes alike.
    assert!(
        select_reviewed(
            None,
            &specific,
            None,
            &PublicBroadcasterTrustFilter::default(),
            Some(&expired)
        )
        .is_ok()
    );
    assert!(
        select(
            Some(&crate::ExecutorDelivery::PublicBroadcaster(Box::new(
                expired
            ))),
            &random,
            None,
            &PublicBroadcasterTrustFilter::default(),
        )
        .is_ok()
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
