use super::*;
use railgun_wallet::tx::{CompositeExecution, CompositePlanShape};

fn test_sponsored_authorization(
    action: SponsoredActionKind,
    wrapped_native: Address,
    payer: Address,
) -> SponsoredAuthorization {
    sponsored_authorization(
        action,
        wrapped_native,
        payer,
        Address::from([0x30; 20]),
        sponsorship_payment(100, 2, U256::ZERO, SponsoredIncentive::Standard).expect("payment"),
        2,
        1,
        Address::from([0x31; 20]),
    )
}

fn test_zero_payment_authorization(
    action: SponsoredActionKind,
    wrapped_native: Address,
    payer: Address,
) -> SponsoredAuthorization {
    sponsored_authorization(
        action,
        wrapped_native,
        payer,
        Address::from([0x30; 20]),
        sponsorship_payment(100, 2, U256::from(200_u16), SponsoredIncentive::Standard)
            .expect("zero-delta payment"),
        2,
        1,
        Address::from([0x31; 20]),
    )
}

#[test]
fn sponsored_action_fingerprint_binds_send_and_unshield_fields() {
    let token = Address::from([0x20; 20]);
    let send_recipient = AddressData {
        master_public_key: U256::from(1_u8),
        viewing_public_key: [2; 32],
    };
    let send = sponsored_send_action_fingerprint(1, token, U256::from(3_u8), &send_recipient);
    assert_ne!(
        send,
        sponsored_send_action_fingerprint(2, token, U256::from(3_u8), &send_recipient)
    );
    assert_ne!(
        send,
        sponsored_send_action_fingerprint(1, token, U256::from(4_u8), &send_recipient)
    );
    assert_ne!(
        send,
        sponsored_send_action_fingerprint(
            1,
            token,
            U256::from(3_u8),
            &AddressData {
                master_public_key: U256::from(5_u8),
                viewing_public_key: [2; 32],
            },
        )
    );

    let recipient = Address::from([0x21; 20]);
    let unshield =
        sponsored_unshield_action_fingerprint(1, token, U256::from(6_u8), recipient, false, None);
    assert_ne!(
        unshield,
        sponsored_unshield_action_fingerprint(1, token, U256::from(6_u8), recipient, true, None,)
    );
    assert_ne!(
        unshield,
        sponsored_unshield_action_fingerprint(
            1,
            token,
            U256::from(6_u8),
            recipient,
            false,
            Some(&DesktopNativeTopUpPlan {
                recipient,
                wrapped_native_token: token,
                native_amount: U256::ONE,
                wrapped_native_amount: U256::from(2_u8),
            }),
        )
    );
}

#[test]
fn sponsored_send_intent_reserves_one_exact_payer_leg() {
    let wrapped_native = Address::from([0x21; 20]);
    let payer = Address::from([0x22; 20]);
    let authorization =
        test_sponsored_authorization(SponsoredActionKind::Send, wrapped_native, payer);
    let request = SponsoredPrivateIntent::Send {
        token: Address::from([0x23; 20]),
        amount: U256::from(7_u8),
        recipient: AddressData {
            master_public_key: U256::from(8_u8),
            viewing_public_key: [9; 32],
        },
    }
    .mixed_request(&authorization, None, false, None)
    .expect("sponsored send request");

    assert_eq!(
        request.adapter,
        Some(CompositeAdapter::RelayAdapt(
            authorization.relay_adapt_contract
        ))
    );
    assert_eq!(request.private_sends.len(), 1);
    assert_eq!(request.public_unshields.len(), 1);
    assert_eq!(
        request.public_unshields[0],
        CompositeUnshieldLeg {
            token_address: wrapped_native,
            amount: authorization.gross_wrapped_native_spend,
            recipient: CompositeUnshieldRecipient::RelayAdapt,
            role: CompositeUnshieldLegRole::SponsoredWrappedNative,
        }
    );
    assert_eq!(
        request.relay_actions.expect("relay actions").calls,
        vec![
            CompositeRelayAction::UnwrapBase {
                amount: authorization.builder_payment,
            },
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::BaseNative,
                recipient: payer,
                amount: authorization.builder_payment,
            },
        ]
    );
}

#[test]
fn zero_delta_send_omits_sponsorship_output_and_payer_calls() {
    let wrapped_native = Address::from([0x21; 20]);
    let authorization = test_zero_payment_authorization(
        SponsoredActionKind::Send,
        wrapped_native,
        Address::from([0x22; 20]),
    );
    let request = SponsoredPrivateIntent::Send {
        token: Address::from([0x23; 20]),
        amount: U256::from(7_u8),
        recipient: AddressData {
            master_public_key: U256::from(8_u8),
            viewing_public_key: [9; 32],
        },
    }
    .mixed_request(&authorization, None, false, None)
    .expect("zero-delta send request");

    assert_eq!(request.private_sends.len(), 1);
    assert_eq!(request.public_unshields, [] as [CompositeUnshieldLeg; 0]);
    assert!(request.relay_actions.is_none());
    assert_eq!(request.adapter, None);
}

#[test]
fn sponsored_wrapped_native_send_total_includes_private_recipient_amount() {
    let wrapped_native = Address::from([0x21; 20]);
    let recipient = AddressData {
        master_public_key: U256::from(8_u8),
        viewing_public_key: [9; 32],
    };
    let public_total = U256::from(11_u8);

    let total = sponsored_total_wrapped_native_spend(
        SponsoredPrivateIntent::Send {
            token: wrapped_native,
            amount: U256::from(7_u8),
            recipient,
        },
        public_total,
        wrapped_native,
    )
    .expect("wrapped-native total");

    assert_eq!(total, U256::from(18_u8));
}

#[test]
fn sponsored_wrapped_native_send_total_rejects_overflow() {
    let wrapped_native = Address::from([0x21; 20]);
    let error = sponsored_total_wrapped_native_spend(
        SponsoredPrivateIntent::Send {
            token: wrapped_native,
            amount: U256::ONE,
            recipient: AddressData {
                master_public_key: U256::from(8_u8),
                viewing_public_key: [9; 32],
            },
        },
        U256::MAX,
        wrapped_native,
    )
    .expect_err("wrapped-native total must not saturate");

    assert_eq!(error, SponsorshipError::ArithmeticOverflow);
}

#[test]
fn sponsored_wrapped_native_unshield_coalesces_primary_and_payer_output() {
    let wrapped_native = Address::from([0x24; 20]);
    let payer = Address::from([0x25; 20]);
    let recipient = Address::from([0x26; 20]);
    let primary_amount = U256::from(10_000_u64);
    let authorization =
        test_sponsored_authorization(SponsoredActionKind::Unshield, wrapped_native, payer);
    let request = SponsoredPrivateIntent::Unshield {
        token: wrapped_native,
        amount: primary_amount,
        recipient,
        unwrap: true,
    }
    .mixed_request(&authorization, None, false, None)
    .expect("sponsored unshield request");

    assert!(request.private_sends.is_empty());
    assert_eq!(request.public_unshields.len(), 1);
    assert_eq!(
        request.public_unshields[0].role,
        CompositeUnshieldLegRole::SponsoredWrappedNative
    );
    let primary_native_amount = native_top_up_net_after_protocol_fee(primary_amount);
    let expected_gross =
        gross_up_sponsorship_payment(authorization.builder_payment + primary_native_amount)
            .expect("combined gross");
    assert_eq!(request.public_unshields[0].amount, expected_gross);
    let calls = request.relay_actions.expect("relay actions").calls;
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls[2],
        CompositeRelayAction::UnwrapBase {
            amount: primary_native_amount,
        }
    );
}

#[test]
fn zero_delta_direct_unshield_omits_payer_calls() {
    let wrapped_native = Address::from([0x24; 20]);
    let token = Address::from([0x25; 20]);
    let recipient = Address::from([0x26; 20]);
    let authorization = test_zero_payment_authorization(
        SponsoredActionKind::Unshield,
        wrapped_native,
        Address::from([0x27; 20]),
    );
    let request = SponsoredPrivateIntent::Unshield {
        token,
        amount: U256::from(10_000_u64),
        recipient,
        unwrap: false,
    }
    .mixed_request(&authorization, None, false, None)
    .expect("zero-delta direct unshield request");

    assert!(request.private_sends.is_empty());
    assert_eq!(request.public_unshields.len(), 1);
    assert_eq!(
        request.public_unshields[0].recipient,
        CompositeUnshieldRecipient::Public(recipient)
    );
    assert!(request.relay_actions.is_none());
}

#[test]
fn zero_delta_unwrap_retains_recipient_calls_without_payer_calls() {
    let wrapped_native = Address::from([0x24; 20]);
    let recipient = Address::from([0x26; 20]);
    let authorization = test_zero_payment_authorization(
        SponsoredActionKind::Unshield,
        wrapped_native,
        Address::from([0x27; 20]),
    );
    let amount = U256::from(10_000_u64);
    let request = SponsoredPrivateIntent::Unshield {
        token: wrapped_native,
        amount,
        recipient,
        unwrap: true,
    }
    .mixed_request(&authorization, None, false, None)
    .expect("zero-delta unwrap request");

    let recipient_amount = native_top_up_net_after_protocol_fee(amount);
    assert_eq!(request.public_unshields.len(), 1);
    assert_eq!(
        request
            .relay_actions
            .expect("recipient relay actions")
            .calls,
        vec![
            CompositeRelayAction::UnwrapBase {
                amount: recipient_amount,
            },
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::BaseNative,
                recipient,
                amount: recipient_amount,
            },
        ]
    );
}

#[test]
fn zero_delta_wrapped_native_unshield_stays_direct() {
    let wrapped_native = Address::from([0x24; 20]);
    let recipient = Address::from([0x26; 20]);
    let authorization = test_zero_payment_authorization(
        SponsoredActionKind::Unshield,
        wrapped_native,
        Address::from([0x27; 20]),
    );
    let request = SponsoredPrivateIntent::Unshield {
        token: wrapped_native,
        amount: U256::from(10_000_u64),
        recipient,
        unwrap: false,
    }
    .mixed_request(&authorization, None, false, None)
    .expect("zero-delta wrapped-native unshield request");

    assert_eq!(request.public_unshields.len(), 1);
    assert_eq!(
        request.public_unshields[0].role,
        CompositeUnshieldLegRole::Primary
    );
    assert_eq!(
        request.public_unshields[0].recipient,
        CompositeUnshieldRecipient::Public(recipient)
    );
    assert!(request.relay_actions.is_none());
}

#[test]
fn sponsored_wrapped_native_output_coalesces_recipient_and_payer_output() {
    let wrapped_native = Address::from([0x27; 20]);
    let payer = Address::from([0x28; 20]);
    let recipient = Address::from([0x29; 20]);
    let primary_amount = U256::from(10_000_u64);
    let authorization =
        test_sponsored_authorization(SponsoredActionKind::Unshield, wrapped_native, payer);
    let request = SponsoredPrivateIntent::Unshield {
        token: wrapped_native,
        amount: primary_amount,
        recipient,
        unwrap: false,
    }
    .mixed_request(&authorization, None, false, None)
    .expect("sponsored wrapped-native output request");

    assert_eq!(request.public_unshields.len(), 1);
    assert_eq!(
        request.public_unshields[0].role,
        CompositeUnshieldLegRole::SponsoredWrappedNative
    );
    let recipient_amount = native_top_up_net_after_protocol_fee(primary_amount);
    assert_eq!(
        request.public_unshields[0].amount,
        gross_up_sponsorship_payment(authorization.builder_payment + recipient_amount)
            .expect("combined gross")
    );
    let calls = request.relay_actions.expect("relay actions").calls;
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2],
        CompositeRelayAction::Transfer {
            token: CompositeRelayActionToken::Erc20(wrapped_native),
            recipient,
            amount: recipient_amount,
        }
    );
}

#[test]
fn sponsored_dai_unshield_coalesces_native_top_up_with_builder_payment() {
    let wrapped_native = Address::from([0x41; 20]);
    let token = Address::from([0x42; 20]);
    let payer = Address::from([0x43; 20]);
    let recipient = Address::from([0x44; 20]);
    let authorization =
        test_sponsored_authorization(SponsoredActionKind::Unshield, wrapped_native, payer);
    let top_up = DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token: wrapped_native,
        native_amount: U256::from(100_u64),
        wrapped_native_amount: native_top_up_wrapped_native_amount(U256::from(100_u64)),
    };
    let request = SponsoredPrivateIntent::Unshield {
        token,
        amount: U256::from(1_000_u64),
        recipient,
        unwrap: false,
    }
    .mixed_request(&authorization, Some(&top_up), false, None)
    .expect("sponsored top-up request");

    assert_eq!(request.public_unshields.len(), 2);
    assert_eq!(
        request.public_unshields[0].role,
        CompositeUnshieldLegRole::SponsoredWrappedNative
    );
    let expected_gross =
        gross_up_sponsorship_payment(authorization.builder_payment + top_up.native_amount)
            .expect("combined gross");
    assert_eq!(request.public_unshields[0].amount, expected_gross);
    assert_eq!(
        request.public_unshields[1].role,
        CompositeUnshieldLegRole::Primary
    );
    let calls = request.relay_actions.expect("relay actions").calls;
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls[1],
        CompositeRelayAction::Transfer {
            token: CompositeRelayActionToken::BaseNative,
            recipient: payer,
            amount: authorization.builder_payment,
        }
    );
    assert_eq!(
        calls[3],
        CompositeRelayAction::Transfer {
            token: CompositeRelayActionToken::BaseNative,
            recipient,
            amount: top_up.native_amount,
        }
    );
}

#[test]
fn sponsored_wrapped_native_top_up_coalesces_all_recipient_and_builder_routing() {
    let wrapped_native = Address::from([0x51; 20]);
    let payer = Address::from([0x52; 20]);
    let recipient = Address::from([0x53; 20]);
    let primary_amount = U256::from(1_000_u64);
    let authorization =
        test_sponsored_authorization(SponsoredActionKind::Unshield, wrapped_native, payer);
    let top_up = DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token: wrapped_native,
        native_amount: U256::from(100_u64),
        wrapped_native_amount: native_top_up_wrapped_native_amount(U256::from(100_u64)),
    };
    let request = SponsoredPrivateIntent::Unshield {
        token: wrapped_native,
        amount: primary_amount,
        recipient,
        unwrap: false,
    }
    .mixed_request(&authorization, Some(&top_up), false, None)
    .expect("sponsored wrapped-native top-up request");

    assert_eq!(request.public_unshields.len(), 1);
    assert_eq!(
        request.public_unshields[0].role,
        CompositeUnshieldLegRole::SponsoredWrappedNative
    );
    let calls = request.relay_actions.expect("relay actions").calls;
    let combined_recipient_amount = native_top_up_required_wrapped_native_amount(
        wrapped_native,
        wrapped_native,
        primary_amount,
        top_up.native_amount,
    );
    let recipient_amount =
        native_top_up_net_after_protocol_fee(combined_recipient_amount) - top_up.native_amount;
    assert_eq!(
        calls,
        vec![
            CompositeRelayAction::UnwrapBase {
                amount: authorization.builder_payment,
            },
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::BaseNative,
                recipient: payer,
                amount: authorization.builder_payment,
            },
            CompositeRelayAction::UnwrapBase {
                amount: top_up.native_amount,
            },
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::BaseNative,
                recipient,
                amount: top_up.native_amount,
            },
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::Erc20(wrapped_native),
                recipient,
                amount: recipient_amount,
            },
        ]
    );
    let expected_net = authorization.builder_payment + top_up.native_amount + recipient_amount;
    assert_eq!(
        request.public_unshields[0].amount,
        gross_up_sponsorship_payment(expected_net).expect("combined gross")
    );
}

fn sponsored_public_output(
    unshield_index: usize,
    transaction_index: usize,
    token: Address,
    amount: U256,
) -> MixedPublicPlannedOutput {
    MixedPublicPlannedOutput {
        unshield_index,
        transaction_index,
        output_index: 0,
        token_address: token,
        amount,
        recipient: CompositeUnshieldRecipient::RelayAdapt,
        role: CompositeUnshieldLegRole::SponsoredWrappedNative,
        note: Note::new_unshield(Address::ZERO, token, amount),
    }
}

#[test]
fn sponsored_output_validation_aggregates_chunks_for_one_logical_leg() {
    let wrapped_native = Address::from([0x61; 20]);
    let outputs = vec![
        sponsored_public_output(0, 0, wrapped_native, U256::from(4_u8)),
        sponsored_public_output(0, 1, wrapped_native, U256::from(6_u8)),
    ];

    assert_eq!(
        sponsored_wrapped_native_output_total(&outputs, 0, wrapped_native),
        Ok(U256::from(10_u8))
    );
    assert_eq!(
        sponsored_wrapped_native_output_total(&outputs, 1, wrapped_native),
        Err(SponsorshipError::SponsoredPlanShapeChangeRequired)
    );

    let overflowing = vec![
        sponsored_public_output(0, 0, wrapped_native, U256::MAX),
        sponsored_public_output(0, 1, wrapped_native, U256::ONE),
    ];
    assert_eq!(
        sponsored_wrapped_native_output_total(&overflowing, 0, wrapped_native),
        Err(SponsorshipError::ArithmeticOverflow)
    );
}

#[test]
fn pinned_input_and_shape_failures_have_named_outcome() {
    let error = sponsored_rebuild_error(BuildError::PinnedInputsInsufficient(U256::ONE));
    assert_eq!(
        error.downcast_ref::<SponsorshipError>(),
        Some(&SponsorshipError::SponsoredPlanShapeChangeRequired)
    );
    let error = sponsored_rebuild_error(BuildError::CompositePlanShapeChanged {
        expected: Box::new(CompositePlanShape {
            execution: CompositeExecution::RelayAdapt(Address::ZERO),
            transaction_count: 1,
            input_count: 1,
            private_output_count: 1,
            public_output_count: 1,
            relay_call_count: 2,
            uses_relay_adapt: true,
        }),
        actual: Box::new(CompositePlanShape {
            execution: CompositeExecution::RelayAdapt(Address::ZERO),
            transaction_count: 2,
            input_count: 1,
            private_output_count: 1,
            public_output_count: 1,
            relay_call_count: 2,
            uses_relay_adapt: true,
        }),
    });
    assert_eq!(
        error.downcast_ref::<SponsorshipError>(),
        Some(&SponsorshipError::SponsoredPlanShapeChangeRequired)
    );
}

fn test_utxo(token: Address, value: U256) -> Utxo {
    test_utxo_at(token, value, 0)
}

fn test_utxo_at(token: Address, value: U256, position: u64) -> Utxo {
    Utxo::new(
        Note::new_unshield(Address::ZERO, token, value),
        0,
        position,
        UtxoSource {
            tx_hash: FixedBytes::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Transact,
    )
}

fn test_chain_config(wrapped_native: Address) -> EffectiveDesktopChainConfig {
    EffectiveDesktopChainConfig {
        rpc_route: RpcChainRoute::new(1, Vec::<Url>::new()),
        railgun_contract: Address::ZERO,
        relay_adapt_contract: Some(Address::ZERO),
        wrapped_native_token: Some(wrapped_native),
        finality_depth: 1,
        gas: settings::EffectiveChainGasSettings {
            gas_limit_buffer: GAS_LIMIT_BUFFER,
            gas_price_buffer_numerator: GAS_PRICE_BUFFER_NUMERATOR as u64,
            gas_price_buffer_denominator: GAS_PRICE_BUFFER_DENOMINATOR as u64,
        },
    }
}

#[test]
fn sponsored_quote_uses_buffered_shape_without_extra_percentage_headroom() {
    let wrapped_native =
        crate::amounts::wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
    let effective_chain =
        settings::build_effective_chain_configs(&settings::WalletSettings::default())
            .expect("effective chains")
            .get(1)
            .cloned()
            .expect("ethereum config");
    let chain = test_chain_config(wrapped_native);
    let payer = effective_chain
        .require_railgun()
        .unwrap()
        .coinbase_payer
        .expect("coinbase payer");
    let signer = Address::from([0x33; 20]);
    let amount = U256::from(20_000_000_000_000_000_u128);
    let recipient = Address::from([0x34; 20]);
    let utxos = vec![test_utxo_at(
        wrapped_native,
        U256::from(10_000_000_000_000_000_000_u128),
        0,
    )];
    let intent = SponsoredPrivateIntent::Unshield {
        token: wrapped_native,
        amount,
        recipient,
        unwrap: false,
    };
    let provisional = |mode| {
        sponsored_provisional_payment_for_intent(
            1,
            &chain,
            &utxos,
            wrapped_native,
            payer,
            Address::ZERO,
            signer,
            intent,
            None,
            150_000_000,
            10_000_000,
            U256::ZERO,
            SponsoredIncentive::Economy,
            mode,
        )
        .expect("provisional payment")
    };
    let expected = provisional(GasEstimateMode::Expected);
    let ceiling = provisional(GasEstimateMode::UpperBound);

    let quote = quote_sponsored_unshield_authorization_limit(
        1,
        &effective_chain,
        &utxos,
        wrapped_native,
        amount,
        FeeHandlingMode::DeductFromAmount,
        recipient,
        false,
        None,
        150_000_000,
        10_000_000,
        U256::ZERO,
        SponsoredIncentive::Economy,
        signer,
    )
    .expect("sponsored quote");

    assert_eq!(quote.max_transaction_gas_limit, ceiling.outer_gas_limit);
    assert!(quote.max_transaction_gas_limit > expected.outer_gas_limit);
    assert_eq!(
        quote.expected_transaction_gas_limit,
        expected.outer_gas_limit
    );

    // An exact gas limit between the expected quote and the ceiling stays authorized.
    let exact_gas_limit =
        expected.outer_gas_limit + (ceiling.outer_gas_limit - expected.outer_gas_limit) / 2;
    let exact = sponsored_authorization(
        SponsoredActionKind::Unshield,
        wrapped_native,
        payer,
        effective_chain
            .require_railgun()
            .unwrap()
            .deployment
            .relay_adapt_contract
            .expect("ethereum relay adapt"),
        sponsorship_payment(
            exact_gas_limit,
            quote.max_fee_per_gas,
            quote.signer_native_balance_snapshot,
            quote.incentive,
        )
        .expect("exact payment"),
        quote.max_fee_per_gas,
        quote.max_priority_fee_per_gas,
        signer,
    );
    let exact_request = intent
        .mixed_request(&exact, None, false, None)
        .expect("exact request");
    let exact_total = sponsored_total_wrapped_native_spend(
        intent,
        request_public_token_spend(&exact_request, wrapped_native).expect("public spend"),
        wrapped_native,
    )
    .expect("exact total");
    assert_eq!(
        validate_sponsored_authorization_limit(
            quote,
            sponsored_unshield_action_fingerprint(
                1,
                wrapped_native,
                amount,
                recipient,
                false,
                None,
            ),
            exact,
            exact_total,
        ),
        Ok(())
    );
}

#[test]
fn sponsored_unchanged_request_signs_the_exact_gas_limit() {
    let wrapped_native = Address::from([0x91; 20]);
    let payer = Address::from([0x92; 20]);
    let relay_adapt = Address::from([0x93; 20]);
    let signer = Address::from([0x94; 20]);
    let token = Address::from([0x95; 20]);
    let recipient = Address::from([0x96; 20]);
    let amount = U256::from(1_000_000_u64);
    let max_fee_per_gas = 2_000_000_000;
    let max_priority_fee_per_gas = 1_000_000_000;
    // The signer's balance covers both gas caps, so the builder payment stays zero.
    let snapshot = U256::from(1_000_000_000_000_000_000_u128);
    let incentive = SponsoredIncentive::Standard;
    let payment = |gas_limit| {
        sponsorship_payment(gas_limit, max_fee_per_gas, snapshot, incentive).expect("payment")
    };
    let authorization = |payment| {
        sponsored_authorization(
            SponsoredActionKind::Unshield,
            wrapped_native,
            payer,
            relay_adapt,
            payment,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            signer,
        )
    };
    let provisional = payment(400_000);
    let exact_gas = 450_000;
    let exact_gas_limit =
        sponsored_gas_limit_with_buffer(exact_gas, GAS_LIMIT_BUFFER).expect("buffered gas");
    let required = payment(exact_gas_limit);
    assert!(provisional.builder_payment.is_zero());
    assert_eq!(required.builder_payment, provisional.builder_payment);
    assert!(!sponsored_payment_requires_rebuild(provisional, required));

    let signed = authorization(sponsored_payment_without_rebuild(&provisional, &required));
    assert!(signed.transaction_gas_limit >= exact_gas_limit);

    let intent = SponsoredPrivateIntent::Unshield {
        token,
        amount,
        recipient,
        unwrap: false,
    };
    let request = |authorization: &SponsoredAuthorization| {
        format!(
            "{:?}",
            intent
                .mixed_request(authorization, None, false, None)
                .expect("request")
        )
    };
    assert_eq!(request(&signed), request(&authorization(provisional)));

    let fingerprint =
        sponsored_unshield_action_fingerprint(1, token, amount, recipient, false, None);
    let limit = |max_transaction_gas_limit| {
        sponsored_authorization_limit(
            fingerprint,
            max_transaction_gas_limit,
            SponsoredActionKind::Unshield,
            wrapped_native,
            payer,
            relay_adapt,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            snapshot,
            incentive,
            signer,
            U256::ZERO,
        )
        .expect("limit")
    };
    // Validation recomputes the economics from the signed gas limit and still enforces the
    // approved ceiling.
    assert_eq!(
        validate_sponsored_authorization_limit(
            limit(signed.transaction_gas_limit),
            fingerprint,
            signed,
            U256::ZERO,
        ),
        Ok(())
    );
    assert_eq!(
        validate_sponsored_authorization_limit(
            limit(signed.transaction_gas_limit - 1),
            fingerprint,
            signed,
            U256::ZERO,
        ),
        Err(SponsorshipError::AuthorizationLimitExceeded)
    );
}

#[test]
fn sponsored_quote_credits_snapshot_balance_against_builder_funding() {
    let wrapped_native =
        crate::amounts::wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
    let effective_chain =
        settings::build_effective_chain_configs(&settings::WalletSettings::default())
            .expect("effective chains")
            .get(1)
            .cloned()
            .expect("ethereum config");
    let amount = U256::from(20_000_000_000_000_000_u128);
    let recipient = Address::from([0x34; 20]);
    let signer = Address::from([0x33; 20]);
    let utxos = vec![test_utxo_at(
        wrapped_native,
        U256::from(10_000_000_000_000_000_000_u128),
        0,
    )];
    let signer_balance = U256::from(100_000_000_000_000_u128);

    let zero_balance = quote_sponsored_unshield_authorization_limit(
        1,
        &effective_chain,
        &utxos,
        wrapped_native,
        amount,
        FeeHandlingMode::DeductFromAmount,
        recipient,
        false,
        None,
        150_000_000,
        10_000_000,
        U256::ZERO,
        SponsoredIncentive::Economy,
        signer,
    )
    .expect("zero-balance quote");
    let credited = quote_sponsored_unshield_authorization_limit(
        1,
        &effective_chain,
        &utxos,
        wrapped_native,
        amount,
        FeeHandlingMode::DeductFromAmount,
        recipient,
        false,
        None,
        150_000_000,
        10_000_000,
        signer_balance,
        SponsoredIncentive::Economy,
        signer,
    )
    .expect("balance-aware quote");

    assert_eq!(credited.signer_native_balance_snapshot, signer_balance);
    assert!(
        credited
            .maximum_payment()
            .expect("credited payment")
            .gross_wrapped_native_spend
            < zero_balance
                .maximum_payment()
                .expect("zero-balance payment")
                .gross_wrapped_native_spend
    );
}

#[test]
fn sponsored_quote_does_not_report_an_intermediate_required_balance() {
    let wrapped_native =
        crate::amounts::wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
    let effective_chain =
        settings::build_effective_chain_configs(&settings::WalletSettings::default())
            .expect("effective chains")
            .get(1)
            .cloned()
            .expect("ethereum config");
    let token = Address::from([0x81; 20]);
    let signer = Address::from([0x82; 20]);
    let recipient = Address::from([0x83; 20]);
    for available in [U256::ZERO, U256::from(1_000_000_u64)] {
        let mut utxos = vec![test_utxo_at(
            token,
            U256::from(10_000_000_000_000_000_000_u128),
            0,
        )];
        if !available.is_zero() {
            utxos.push(test_utxo_at(wrapped_native, available, 1));
        }

        let error = quote_sponsored_unshield_authorization_limit(
            1,
            &effective_chain,
            &utxos,
            token,
            U256::from(1_000_000_000_000_000_000_u128),
            FeeHandlingMode::DeductFromAmount,
            recipient,
            false,
            None,
            1_000_000_000_000,
            100_000_000,
            U256::ZERO,
            SponsoredIncentive::Standard,
            signer,
        )
        .expect_err("high gas price exceeds wrapped-native balance");
        assert!(matches!(
            error.downcast_ref::<SponsorshipError>(),
            Some(SponsorshipError::InsufficientWrappedNativeForQuote {
                available: actual,
            }) if *actual == available
        ));
    }
}

#[test]
fn sponsored_quote_requires_a_legacy_relay_adapt() {
    let wrapped_native =
        crate::amounts::wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
    let mut effective_chain =
        settings::build_effective_chain_configs(&settings::WalletSettings::default())
            .expect("effective chains")
            .get(1)
            .cloned()
            .expect("ethereum config");
    effective_chain
        .railgun
        .as_mut()
        .expect("ethereum railgun")
        .deployment
        .relay_adapt_contract = None;
    let utxos = vec![test_utxo(
        wrapped_native,
        U256::from(10_000_000_000_000_000_000_u128),
    )];

    let error = quote_sponsored_unshield_authorization_limit(
        1,
        &effective_chain,
        &utxos,
        wrapped_native,
        U256::from(20_000_000_000_000_000_u128),
        FeeHandlingMode::DeductFromAmount,
        Address::from([0x84; 20]),
        false,
        None,
        150_000_000,
        10_000_000,
        U256::ZERO,
        SponsoredIncentive::Economy,
        Address::from([0x85; 20]),
    )
    .expect_err("a chain without the legacy contract cannot be sponsored");
    assert_eq!(
        error.downcast_ref::<SponsorshipError>(),
        Some(&SponsorshipError::MissingRelayAdapt)
    );
}

#[test]
fn sponsored_wrapped_native_quote_binds_canonical_coalesced_total() {
    let wrapped_native =
        crate::amounts::wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
    let effective_chain =
        settings::build_effective_chain_configs(&settings::WalletSettings::default())
            .expect("effective chains")
            .get(1)
            .cloned()
            .expect("ethereum config");
    let payer = effective_chain
        .require_railgun()
        .unwrap()
        .coinbase_payer
        .expect("coinbase payer");
    let relay_adapt: Address = effective_chain
        .require_railgun()
        .unwrap()
        .deployment
        .relay_adapt_contract
        .expect("ethereum relay adapt");
    let signer = Address::from([0x71; 20]);
    let recipient = Address::from([0x72; 20]);
    let entered_amount = U256::from(1_596_000_000_000_211_u128);
    let utxos = vec![test_utxo(
        wrapped_native,
        U256::from(10_000_000_000_000_000_000_u128),
    )];

    for fee_mode in [
        FeeHandlingMode::DeductFromAmount,
        FeeHandlingMode::AddToAmount,
    ] {
        let quote = quote_sponsored_unshield_authorization_limit(
            1,
            &effective_chain,
            &utxos,
            wrapped_native,
            entered_amount,
            fee_mode,
            recipient,
            false,
            None,
            150_000_000,
            10_000_000,
            U256::ZERO,
            SponsoredIncentive::Economy,
            signer,
        )
        .expect("sponsored quote");
        let amount = unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode)
            .expect("receiver amount");
        let intent = SponsoredPrivateIntent::Unshield {
            token: wrapped_native,
            amount,
            recipient,
            unwrap: false,
        };
        let payment = quote.maximum_payment().expect("maximum payment");
        let authorization = sponsored_authorization(
            SponsoredActionKind::Unshield,
            wrapped_native,
            payer,
            relay_adapt,
            payment,
            quote.max_fee_per_gas,
            quote.max_priority_fee_per_gas,
            signer,
        );
        let request = intent
            .mixed_request(&authorization, None, false, None)
            .expect("canonical request");
        let (_, canonical_total) = optional_sponsored_wrapped_native_leg_amount(&request)
            .expect("valid sponsored leg")
            .expect("sponsored leg");
        let fingerprint = sponsored_unshield_action_fingerprint(
            1,
            wrapped_native,
            amount,
            recipient,
            false,
            None,
        );

        assert_eq!(quote.max_total_wrapped_native_spend, canonical_total);
        assert_eq!(
            validate_sponsored_authorization_limit(
                quote,
                fingerprint,
                authorization,
                canonical_total,
            ),
            Ok(())
        );
        assert_eq!(
            validate_sponsored_authorization_limit(
                SponsoredAuthorizationLimit {
                    max_total_wrapped_native_spend: canonical_total - U256::ONE,
                    ..quote
                },
                fingerprint,
                authorization,
                canonical_total,
            ),
            Err(SponsorshipError::AuthorizationLimitExceeded)
        );
    }
}

#[test]
fn native_top_up_estimate_rejects_unwrap_as_unsupported() {
    let wrapped_native =
        crate::amounts::wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
    let chain = test_chain_config(wrapped_native);
    let error = desktop_native_top_up_plan_for_estimate(
        1,
        &chain,
        wrapped_native,
        Address::from([0x52; 20]),
        true,
        U256::ONE,
    )
    .expect_err("unwrap-to-native cannot be combined with native top-up");
    assert_eq!(
        error.to_string(),
        "native top-up cannot be combined with unwrap-to-native output"
    );
}

#[test]
fn native_top_up_plan_validation_counts_wrapped_native_broadcaster_fee() {
    let wrapped_native =
        crate::amounts::wrapped_native_token_for_chain(1).expect("ethereum wrapped native");
    let token = Address::from([0x51; 20]);
    let recipient = Address::from([0x52; 20]);
    let receiver_amount = U256::from(25_u64);
    let chain = test_chain_config(wrapped_native);
    let policy = native_top_up_policy_for_chain(1).expect("ethereum native top-up policy");
    let required_without_fee = native_top_up_required_wrapped_native_amount(
        token,
        wrapped_native,
        receiver_amount,
        policy.top_up_amount,
    );
    let utxos = vec![test_utxo(wrapped_native, required_without_fee)];

    desktop_native_top_up_plan_from_unshield_fields(
        1,
        &chain,
        token,
        recipient,
        false,
        receiver_amount,
        Some(wrapped_native),
        U256::ZERO,
        &utxos,
    )
    .expect("zero wrapped-native broadcaster fee fits available balance");

    let fee_amount = U256::from(1_u64);
    let error = desktop_native_top_up_plan_from_unshield_fields(
        1,
        &chain,
        token,
        recipient,
        false,
        receiver_amount,
        Some(wrapped_native),
        fee_amount,
        &utxos,
    )
    .expect_err("wrapped-native broadcaster fee should require additional balance");
    let expected_required = required_without_fee.saturating_add(fee_amount);
    let message = error.to_string();
    assert!(message.contains("native top-up wrapped-native max spendable"));
    assert!(message.contains(&format!("required: {expected_required}")));
}

#[test]
fn pinned_broadcaster_payload_is_one_transaction_sharing_a_pinned_note() {
    let token = Address::from([0x73; 20]);
    let fee_token = Address::from([0x74; 20]);
    let a = test_utxo_at(token, U256::from(60_u64), 0);
    let b = test_utxo_at(token, U256::from(60_u64), 1);
    let c = test_utxo_at(token, U256::from(60_u64), 2);
    let fee = test_utxo_at(fee_token, U256::from(100_u64), 3);
    let identity = SelectedInputIdentity::from_utxo;
    let pin = [identity(&a)];
    let rejected = |payload: Option<&[SelectedInputIdentity]>| {
        require_pinned_payload(Some(&pin[..]), payload)
            .unwrap_err()
            .downcast_ref::<PublicBroadcasterInputPinUnavailable>()
            .is_some()
    };
    // The separate fee transaction is not the payload; an extra unpinned note is allowed.
    let shared =
        single_payload_inputs([vec![&fee], vec![&a, &c]], token).expect("one payload transaction");
    assert_eq!(shared, vec![identity(&a), identity(&c)]);
    assert!(require_pinned_payload(Some(&pin[..]), Some(shared.as_slice())).is_ok());
    // A payload sharing no pinned note could execute alongside the earlier proof.
    let disjoint = single_payload_inputs([vec![&b, &c]], token);
    assert!(rejected(disjoint.as_deref()));
    // A payload of several transactions is rejected while pinned, and installs no pin.
    let multi_chunk = single_payload_inputs([vec![&a], vec![&b]], token);
    assert_eq!(multi_chunk, None);
    assert!(rejected(multi_chunk.as_deref()));
    assert!(require_pinned_payload(None, None).is_ok());
}
