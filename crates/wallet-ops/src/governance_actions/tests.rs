use super::*;
use alloy::primitives::address;

fn context() -> GovernanceActionContext {
    GovernanceActionContext {
        private_wallet_uuid: "wallet".into(),
        chain_id: 1,
        public_account_uuid: "account".into(),
        actor: address!("1111111111111111111111111111111111111111"),
        contract: address!("2222222222222222222222222222222222222222"),
        contract_kind: GovernanceContractKind::Voting,
        observed_state: B256::ZERO,
    }
}
fn sponsor() -> GovernanceActionIntent {
    GovernanceActionIntent::Sponsor {
        proposal_version: GovernanceContractVersion::V2,
        proposal_index: U256::from(7),
        amount: U256::from(3),
        snapshot_interval: U256::from(9),
        snapshot_hint: U256::from(2),
    }
}

#[test]
fn each_context_identity_changes_fingerprint() {
    let base = context();
    let raw = sponsor().resolve(&base).unwrap().raw;
    let fp = base.fingerprint(&sponsor(), &raw);
    let mut changed = base.clone();
    changed.private_wallet_uuid = "other".into();
    assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
    changed = base.clone();
    changed.chain_id = 56;
    assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
    changed = base.clone();
    changed.public_account_uuid = "other".into();
    assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
    changed = base.clone();
    changed.actor = address!("3333333333333333333333333333333333333333");
    assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
    changed = base.clone();
    changed.contract = address!("3333333333333333333333333333333333333333");
    assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
    changed = base;
    changed.observed_state = B256::from([1_u8; 32]);
    assert_ne!(fp, changed.fingerprint(&sponsor(), &raw));
}

#[test]
fn variants_and_raw_calls_are_distinct_and_zero_value() {
    let ctx = context();
    let sponsor = sponsor().resolve(&ctx).unwrap();
    let nay = GovernanceActionIntent::Nay {
        proposal_version: GovernanceContractVersion::V2,
        proposal_index: U256::from(7),
        amount: U256::from(3),
        snapshot_interval: U256::from(9),
        snapshot_hint: U256::from(2),
    }
    .resolve(&ctx)
    .unwrap();
    assert_ne!(sponsor.fingerprint, nay.fingerprint);
    assert!(
        matches!(sponsor.raw, PublicTransactionIntent::Raw { to: Some(_), value, .. } if value.is_zero())
    );
    let PublicTransactionIntent::Raw { data, .. } = sponsor.raw else {
        unreachable!()
    };
    let decoded = GovernanceVotingActions::sponsorProposalCall::abi_decode(&data).unwrap();
    assert_eq!(decoded.account, ctx.actor);
    assert_eq!(decoded.proposalID, U256::from(7));
    assert_eq!(decoded.hint, U256::from(2));
}

#[test]
fn principal_claim_binds_its_amount_but_calls_with_the_stake_alone() {
    let ctx = GovernanceActionContext {
        contract_kind: GovernanceContractKind::Staking,
        ..context()
    };
    let claim = |amount: u64| {
        GovernanceActionIntent::PrincipalClaim {
            stake_id: U256::from(2),
            amount: U256::from(amount),
        }
        .resolve(&ctx)
        .unwrap()
    };
    let (claim, other) = (claim(7), claim(8));
    assert_eq!(claim.raw, other.raw);
    assert_ne!(claim.fingerprint, other.fingerprint);
}

#[test]
fn reward_call_binds_ordered_tokens_amounts_actor_range_and_hints() {
    let mut ctx = context();
    ctx.contract_kind = GovernanceContractKind::GovernorRewards;
    let action = GovernanceActionIntent::RewardClaim {
        reward_tokens: vec![
            address!("1111111111111111111111111111111111111111"),
            address!("3333333333333333333333333333333333333333"),
        ],
        starting_interval: U256::from(4),
        ending_interval: U256::from(5),
        snapshot_hints: vec![U256::from(8), U256::from(9)],
        expected_amounts: vec![U256::from(10), U256::ZERO],
    };
    let resolved = action.resolve(&ctx).unwrap();
    let PublicTransactionIntent::Raw { data, .. } = resolved.raw else {
        unreachable!()
    };
    let decoded = GovernanceRewardsActions::claimCall::abi_decode(&data).unwrap();
    assert_eq!(
        decoded.tokens,
        vec![
            address!("1111111111111111111111111111111111111111"),
            address!("3333333333333333333333333333333333333333")
        ]
    );
    assert_eq!(decoded.account, ctx.actor);
    assert_eq!(decoded.startingInterval, U256::from(4));
    assert_eq!(decoded.endingInterval, U256::from(5));
    assert_eq!(decoded.hints, vec![U256::from(8), U256::from(9)]);
    let mut changed = action;
    if let GovernanceActionIntent::RewardClaim {
        expected_amounts, ..
    } = &mut changed
    {
        expected_amounts[0] = U256::from(11);
    }
    assert_ne!(
        resolved.fingerprint,
        changed.resolve(&ctx).unwrap().fingerprint
    );
}

#[test]
fn reward_call_rejects_invalid_token_vectors() {
    let mut ctx = context();
    ctx.contract_kind = GovernanceContractKind::GovernorRewards;
    let first_token = address!("1111111111111111111111111111111111111111");
    for action in [
        GovernanceActionIntent::RewardClaim {
            reward_tokens: Vec::new(),
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            snapshot_hints: vec![U256::ZERO],
            expected_amounts: Vec::new(),
        },
        GovernanceActionIntent::RewardClaim {
            reward_tokens: vec![first_token],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            snapshot_hints: vec![U256::ZERO],
            expected_amounts: vec![],
        },
    ] {
        assert!(matches!(
            action.resolve(&ctx),
            Err(GovernanceActionError::InvalidRewardAmounts)
        ));
    }
    assert!(matches!(
        (GovernanceActionIntent::RewardClaim {
            reward_tokens: vec![
                address!("3333333333333333333333333333333333333333"),
                address!("1111111111111111111111111111111111111111")
            ],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            snapshot_hints: vec![U256::ZERO],
            expected_amounts: vec![U256::ONE, U256::ONE],
        })
        .resolve(&ctx),
        Err(GovernanceActionError::InvalidRewardTokenOrder)
    ));
    assert!(matches!(
        (GovernanceActionIntent::RewardClaim {
            reward_tokens: vec![first_token; 2],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            snapshot_hints: vec![U256::ZERO],
            expected_amounts: vec![U256::ONE, U256::ONE],
        })
        .resolve(&ctx),
        Err(GovernanceActionError::InvalidRewardTokenOrder)
    ));
    assert!(matches!(
        (GovernanceActionIntent::RewardClaim {
            reward_tokens: vec![first_token],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            snapshot_hints: vec![U256::ZERO],
            expected_amounts: vec![U256::ZERO],
        })
        .resolve(&ctx),
        Err(GovernanceActionError::ZeroReward)
    ));
}

#[test]
fn stale_rebuild_and_missing_authorization_are_rejected_before_submission() {
    let ctx = context();
    let resolved = sponsor().resolve(&ctx).unwrap();
    let review = GovernanceActionReview::from_resolved(&resolved, ctx.clone(), None).unwrap();
    let mut inconsistent = resolved.clone();
    inconsistent.fingerprint = B256::from([6_u8; 32]);
    assert!(!review.rebuild_matches(&inconsistent));
    let mut changed = ctx.clone();
    changed.observed_state = B256::from([4_u8; 32]);
    let rebuilt = sponsor().resolve(&changed).unwrap();
    assert_eq!(
        validate_governance_submission(&review, &rebuilt, 1, "account", &rebuilt.raw, true),
        Err(GovernanceActionError::StaleDraft)
    );
    assert_eq!(
        validate_governance_submission(&review, &resolved, 1, "account", &resolved.raw, false),
        Err(GovernanceActionError::MissingAuthorization)
    );
    assert_eq!(
        validate_governance_submission(&review, &resolved, 56, "account", &resolved.raw, true),
        Err(GovernanceActionError::WrongPublicContext)
    );
    assert_eq!(
        validate_governance_submission(&review, &resolved, 1, "other", &resolved.raw, true),
        Err(GovernanceActionError::WrongPublicContext)
    );
    let other_raw = PublicTransactionIntent::Raw {
        to: Some(ctx.contract),
        value: U256::ZERO,
        data: Bytes::from_static(&[1]),
    };
    assert_eq!(
        validate_governance_submission(&review, &resolved, 1, "account", &other_raw, true),
        Err(GovernanceActionError::WrongPublicContext)
    );
}

#[test]
fn authorization_must_match_the_current_public_estimate() {
    let estimate = PublicAdvancedTransactionEstimate {
        payload_fingerprint: B256::from([1_u8; 32]),
        raw_gas_limit: 1,
        gas_limit: 2,
        max_fee_per_gas: 3,
        max_priority_fee_per_gas: 4,
        expected_fee_per_gas: 3,
        expected_gas_cost: U256::from(3),
        max_gas_cost: U256::from(6),
    };
    let matching = PublicAdvancedTransactionAuthorization {
        payload_fingerprint: estimate.payload_fingerprint,
        gas_limit: estimate.gas_limit,
    };
    assert!(validate_governance_authorization(Some(&matching), Some(&estimate)).is_ok());
    let mismatched = PublicAdvancedTransactionAuthorization {
        payload_fingerprint: B256::from([9_u8; 32]),
        gas_limit: estimate.gas_limit,
    };
    assert_eq!(
        validate_governance_authorization(Some(&mismatched), Some(&estimate)),
        Err(GovernanceActionError::MismatchedAuthorization)
    );
    assert_eq!(
        validate_governance_authorization(None, Some(&estimate)),
        Err(GovernanceActionError::MissingAuthorization)
    );
}

#[test]
fn action_cannot_target_a_different_contract_class() {
    let mut ctx = context();
    ctx.contract_kind = GovernanceContractKind::Staking;
    assert_eq!(
        sponsor().resolve(&ctx),
        Err(GovernanceActionError::WrongContractKind)
    );
}

#[test]
fn close_after_submission_preserves_submitted_state() {
    let mut progress = GovernanceProgress::new([B256::from([7_u8; 32])]);
    assert!(progress.record_submission(0, B256::from([8_u8; 32])));
    progress.close();
    assert!(progress.dismissed);
    assert!(progress.handed_off());
    assert_eq!(progress.status, GovernanceProgressStatus::Submitted);
}

#[test]
fn confirmed_progress_tracks_continuations_and_failures() {
    let first = B256::from([7_u8; 32]);
    let first_hash = B256::from([8_u8; 32]);
    let second = B256::from([9_u8; 32]);
    let second_hash = B256::from([10_u8; 32]);
    let mut progress = GovernanceProgress::new([first]);
    assert!(progress.record_confirmation(0, first_hash, true));
    assert_eq!(progress.status, GovernanceProgressStatus::Pending);
    assert!(progress.add_step(second));
    assert!(progress.record_confirmation(1, second_hash, false));
    assert_eq!(progress.status, GovernanceProgressStatus::Confirmed);
    assert_eq!(progress.steps[0].transaction_hash, Some(first_hash));

    let mut failed = GovernanceProgress::new([first]);
    assert!(failed.record_confirmation(0, first_hash, true));
    assert!(failed.add_step(second));
    assert!(failed.record_failure(1, None));
    assert_eq!(failed.status, GovernanceProgressStatus::Failed);
    assert_eq!(failed.steps[0].transaction_hash, Some(first_hash));
    assert_eq!(failed.steps[1].transaction_hash, None);
}
