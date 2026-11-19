use super::*;

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
fn broadcaster_fee_amount_saturates_instead_of_wrapping_on_huge_rate() {
    // Each row wraps to zero with plain multiplication: once at gas_limit, once at gas_price.
    for (rate, gas_limit, gas_price) in [
        (U256::from(1) << 255, 2, 1),
        (U256::from(1) << 192, 1, 1_u128 << 64),
    ] {
        assert_eq!(
            broadcaster_fee_amount(rate, gas_limit, gas_price),
            U256::MAX / uint!(1_000_000_000_000_000_000_U256)
        );
    }
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
