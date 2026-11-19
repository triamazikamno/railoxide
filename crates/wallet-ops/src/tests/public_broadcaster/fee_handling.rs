use super::*;

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
