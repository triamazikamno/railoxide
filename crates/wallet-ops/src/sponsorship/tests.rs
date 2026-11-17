use super::*;

#[test]
fn custom_incentive_rejects_out_of_range_percentages() {
    assert_eq!(SponsoredIncentive::Custom(1).percent(), Ok(1));
    assert_eq!(SponsoredIncentive::Custom(100).percent(), Ok(100));
    assert!(SponsoredIncentive::Custom(0).percent().is_err());
    assert!(SponsoredIncentive::Custom(101).percent().is_err());
}

#[test]
fn payment_rounds_incentive_and_protocol_gross_up_upward() {
    let payment =
        sponsorship_payment(1, 1, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
    assert_eq!(payment.reimbursement_base, U256::from(21_001_u64));
    assert_eq!(payment.builder_payment, U256::from(22_052_u64));
    assert_eq!(
        native_top_up_net_after_protocol_fee(payment.gross_wrapped_native_spend),
        payment.builder_payment
    );
}

#[test]
fn payment_applies_buffer_outer_funding_and_incentive_rules_in_order() {
    let payment =
        sponsorship_payment_from_estimate(100, 20, 2, U256::ZERO, SponsoredIncentive::Economy)
            .expect("payment");
    assert_eq!(payment.outer_gas_cap, U256::from(240_u64));
    assert_eq!(payment.funding_gas_cap, U256::from(42_000_u64));
    assert_eq!(payment.reimbursement_base, U256::from(42_240_u64));
    assert_eq!(payment.builder_payment, U256::from(42_663_u64));
}

#[test]
fn payment_credits_partial_balance_and_conditionally_adds_funding_gas() {
    let payment = sponsorship_payment(100, 2, U256::from(80_u8), SponsoredIncentive::Economy)
        .expect("payment");

    assert_eq!(payment.outer_gas_cap, U256::from(200_u16));
    assert_eq!(payment.balance_credit, U256::from(80_u8));
    assert_eq!(payment.funding_principal, U256::from(120_u8));
    assert_eq!(payment.funding_gas_limit, SPONSORED_FUNDING_GAS_LIMIT);
    assert_eq!(payment.funding_gas_cap, U256::from(42_000_u64));
    assert_eq!(payment.reimbursement_base, U256::from(42_120_u64));
    assert_eq!(payment.builder_payment, U256::from(42_542_u64));
}

#[test]
fn payment_caps_overfunded_balance_credit_and_has_zero_delta() {
    let snapshot = U256::from(250_u16);
    let payment =
        sponsorship_payment(100, 2, snapshot, SponsoredIncentive::Standard).expect("payment");

    assert_eq!(payment.signer_native_balance_snapshot, snapshot);
    assert_eq!(payment.outer_gas_cap, U256::from(200_u16));
    assert_eq!(payment.balance_credit, payment.outer_gas_cap);
    assert_eq!(payment.funding_principal, U256::ZERO);
    assert_eq!(payment.funding_gas_limit, 0);
    assert_eq!(payment.funding_gas_cap, U256::ZERO);
    assert_eq!(payment.reimbursement_base, U256::ZERO);
    assert_eq!(payment.builder_payment, U256::ZERO);
    assert_eq!(payment.gross_wrapped_native_spend, U256::ZERO);
    assert_eq!(payment.protocol_fee, U256::ZERO);
}

#[test]
fn provisional_payment_is_monotone_and_bounded_to_four_steps() {
    let mut estimates = vec![101_u64, 102, 103, 103].into_iter();
    let payment =
        provisional_sponsorship_payment(100, 0, 1, U256::ZERO, SponsoredIncentive::Economy, |_| {
            Ok(estimates.next().expect("bounded estimate"))
        })
        .expect("converged payment");
    assert_eq!(payment.outer_gas_cap, U256::from(103_u64));

    let mut next = 101_u64;
    assert_eq!(
        provisional_sponsorship_payment(100, 0, 1, U256::ZERO, SponsoredIncentive::Economy, |_| {
            let estimate = next;
            next += 1;
            Ok(estimate)
        },),
        Err(SponsorshipError::ProvisionalPaymentDidNotConverge)
    );
}

#[test]
fn exact_payment_decision_converges_or_reports_second_estimate_amounts() {
    let embedded =
        sponsorship_payment(100, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
    let covered =
        sponsorship_payment(99, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
    let required =
        sponsorship_payment(101, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment");
    assert_eq!(
        validate_final_sponsorship_payment(embedded, covered),
        Ok(())
    );
    assert_eq!(
        validate_final_sponsorship_payment(embedded, required),
        Err(SponsorshipError::FinalPaymentUnderfunded {
            embedded: embedded.builder_payment,
            required: required.builder_payment,
        })
    );
}

#[test]
fn authorization_limit_accepts_lower_exact_economics_and_rejects_changes() {
    let wrapped_native = Address::from([4; 20]);
    let payer = Address::from([5; 20]);
    let signer = Address::from([6; 20]);
    let relay_adapt = Address::from([7; 20]);
    let additional_spend = U256::from(7_u8);
    let signer_native_balance_snapshot = U256::from(1_000_000_u64);
    let action_fingerprint = FixedBytes::from([7; 32]);
    let max_total = sponsorship_payment(
        2_000_000,
        2,
        signer_native_balance_snapshot,
        SponsoredIncentive::Standard,
    )
    .expect("maximum payment")
    .gross_wrapped_native_spend
        + additional_spend;
    let limit = sponsored_authorization_limit(
        action_fingerprint,
        2_000_000,
        SponsoredActionKind::Send,
        wrapped_native,
        payer,
        relay_adapt,
        2,
        1,
        signer_native_balance_snapshot,
        SponsoredIncentive::Standard,
        signer,
        max_total,
    )
    .expect("authorization limit");
    let exact_payment = sponsorship_payment(
        1_000_000,
        2,
        signer_native_balance_snapshot,
        SponsoredIncentive::Standard,
    )
    .expect("exact payment");
    let exact = sponsored_authorization(
        SponsoredActionKind::Send,
        wrapped_native,
        payer,
        relay_adapt,
        exact_payment,
        2,
        1,
        signer,
    );
    let total = additional_spend + exact.gross_wrapped_native_spend;
    assert_eq!(
        validate_sponsored_authorization_limit(limit, action_fingerprint, exact, total),
        Ok(())
    );
    assert_eq!(
        validate_sponsored_authorization_limit(limit, FixedBytes::from([9; 32]), exact, total,),
        Err(SponsorshipError::AuthorizationMismatch)
    );
    for changed in [
        SponsoredAuthorization {
            action: SponsoredActionKind::Unshield,
            ..exact
        },
        SponsoredAuthorization {
            wrapped_native_token: Address::from([8; 20]),
            ..exact
        },
        SponsoredAuthorization {
            coinbase_payer: Address::from([8; 20]),
            ..exact
        },
        SponsoredAuthorization {
            relay_adapt_contract: Address::from([8; 20]),
            ..exact
        },
        SponsoredAuthorization {
            max_fee_per_gas: 3,
            ..exact
        },
        SponsoredAuthorization {
            max_priority_fee_per_gas: 2,
            ..exact
        },
        SponsoredAuthorization {
            signer_native_balance_snapshot: signer_native_balance_snapshot + U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            outer_gas_cap: exact.outer_gas_cap - U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            balance_credit: U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            funding_principal: exact.funding_principal - U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            funding_gas_limit: exact.funding_gas_limit - 1,
            ..exact
        },
        SponsoredAuthorization {
            funding_gas_cap: exact.funding_gas_cap - U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            reimbursement_base: exact.reimbursement_base - U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            builder_payment: exact.builder_payment - U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            gross_wrapped_native_spend: exact.gross_wrapped_native_spend - U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            protocol_fee: exact.protocol_fee - U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            incentive: SponsoredIncentive::Economy,
            ..exact
        },
        SponsoredAuthorization {
            signer: Address::from([8; 20]),
            ..exact
        },
        SponsoredAuthorization {
            delivery: PrivateDeliveryMode::PublicBroadcaster,
            ..exact
        },
    ] {
        assert_eq!(
            validate_sponsored_authorization_limit(limit, action_fingerprint, changed, total,),
            Err(SponsorshipError::AuthorizationMismatch)
        );
    }
}

#[test]
fn authorization_limit_rejects_exact_economics_above_maximum() {
    let wrapped_native = Address::from([4; 20]);
    let payer = Address::from([5; 20]);
    let signer = Address::from([6; 20]);
    let relay_adapt = Address::from([7; 20]);
    let action_fingerprint = FixedBytes::from([8; 32]);
    let max_total = sponsorship_payment(1_000_000, 2, U256::ZERO, SponsoredIncentive::Standard)
        .expect("maximum payment")
        .gross_wrapped_native_spend;
    let limit = sponsored_authorization_limit(
        action_fingerprint,
        1_000_000,
        SponsoredActionKind::Unshield,
        wrapped_native,
        payer,
        relay_adapt,
        2,
        1,
        U256::ZERO,
        SponsoredIncentive::Standard,
        signer,
        max_total,
    )
    .expect("authorization limit");
    let maximum_payment = limit.maximum_payment().expect("maximum payment");
    let exact = sponsored_authorization(
        SponsoredActionKind::Unshield,
        wrapped_native,
        payer,
        relay_adapt,
        maximum_payment,
        2,
        1,
        signer,
    );

    assert_eq!(
        validate_sponsored_authorization_limit(limit, action_fingerprint, exact, max_total),
        Ok(())
    );
    for exceeded in [
        SponsoredAuthorization {
            transaction_gas_limit: limit.max_transaction_gas_limit + 1,
            ..exact
        },
        SponsoredAuthorization {
            builder_payment: maximum_payment.builder_payment + U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            gross_wrapped_native_spend: maximum_payment.gross_wrapped_native_spend + U256::ONE,
            ..exact
        },
        SponsoredAuthorization {
            protocol_fee: maximum_payment.protocol_fee + U256::ONE,
            ..exact
        },
    ] {
        assert_eq!(
            validate_sponsored_authorization_limit(limit, action_fingerprint, exceeded, max_total,),
            Err(SponsorshipError::AuthorizationLimitExceeded)
        );
    }
    assert_eq!(
        validate_sponsored_authorization_limit(
            limit,
            action_fingerprint,
            exact,
            max_total + U256::ONE,
        ),
        Err(SponsorshipError::AuthorizationLimitExceeded)
    );
}

#[test]
fn poi_spendable_balance_uses_only_the_requested_token() {
    let token = Address::from([4; 20]);
    let other = Address::from([5; 20]);
    let utxo = |token, value| {
        Utxo::new(
            Note::new_unshield(Address::ZERO, token, U256::from(value)),
            0,
            value,
            UtxoSource {
                tx_hash: FixedBytes::ZERO,
                block_number: 0,
                block_timestamp: 0,
            },
            UtxoCommitmentKind::Transact,
        )
    };
    assert_eq!(
        poi_spendable_token_balance(&[utxo(token, 7), utxo(other, 11)], token),
        U256::from(7_u8)
    );
}

#[test]
fn payment_overflow_is_rejected() {
    assert_eq!(
        gross_up_sponsorship_payment(U256::MAX),
        Err(SponsorshipError::ArithmeticOverflow)
    );
}

#[test]
fn payer_and_signer_code_checks_fail_closed() {
    assert_eq!(
        verify_coinbase_payer_runtime(&[]),
        Err(SponsorshipError::PayerRuntimeMismatch)
    );
    assert_eq!(verify_sponsored_signer_code(&[]), Ok(()));
    assert_eq!(
        verify_sponsored_signer_code(&[0xef, 0x01]),
        Err(SponsorshipError::SignerHasCode)
    );
}

#[test]
fn blocked_shield_and_missing_prerequisites_are_rejected() {
    let valid = SponsoredAdmission {
        action: SponsoredActionKind::Send,
        delivery: PrivateDeliveryMode::SelfBroadcast,
        has_relays: true,
        wrapped_native_token: Some(Address::from([1; 20])),
        coinbase_payer: Some(Address::from([2; 20])),
        relay_adapt_contract: Some(Address::from([3; 20])),
        payer_verified: true,
        signer_eligible: true,
        poi_spendable_wrapped_native: U256::from(10_u8),
        required_wrapped_native: U256::from(10_u8),
    };
    assert_eq!(validate_sponsored_admission(valid), Ok(()));
    assert_eq!(
        validate_sponsored_admission(SponsoredAdmission {
            action: SponsoredActionKind::PublicAction,
            ..valid
        }),
        Err(SponsorshipError::ActionUnsupported)
    );
    assert_eq!(
        validate_sponsored_admission(SponsoredAdmission {
            action: SponsoredActionKind::BlockedShield,
            ..valid
        }),
        Err(SponsorshipError::BlockedShieldUnsupported)
    );
    assert_eq!(
        validate_sponsored_admission(SponsoredAdmission {
            poi_spendable_wrapped_native: U256::from(9_u8),
            ..valid
        }),
        Err(SponsorshipError::InsufficientWrappedNative {
            available: U256::from(9_u8),
            required: U256::from(10_u8),
        })
    );
    for (admission, expected) in [
        (
            SponsoredAdmission {
                delivery: PrivateDeliveryMode::PublicBroadcaster,
                ..valid
            },
            SponsorshipError::DeliveryUnsupported,
        ),
        (
            SponsoredAdmission {
                has_relays: false,
                ..valid
            },
            SponsorshipError::MissingRelay,
        ),
        (
            SponsoredAdmission {
                wrapped_native_token: None,
                ..valid
            },
            SponsorshipError::MissingWrappedNativeToken,
        ),
        (
            SponsoredAdmission {
                coinbase_payer: None,
                ..valid
            },
            SponsorshipError::MissingCoinbasePayer,
        ),
        (
            SponsoredAdmission {
                relay_adapt_contract: None,
                ..valid
            },
            SponsorshipError::MissingRelayAdapt,
        ),
        (
            SponsoredAdmission {
                payer_verified: false,
                ..valid
            },
            SponsorshipError::PayerNotVerified,
        ),
        (
            SponsoredAdmission {
                signer_eligible: false,
                ..valid
            },
            SponsorshipError::SignerIneligible,
        ),
    ] {
        assert_eq!(validate_sponsored_admission(admission), Err(expected));
    }
}
