use super::helpers::*;

#[test]
fn native_top_up_policy_constants_are_fixed() {
    let ethereum = native_top_up_policy_for_chain(1).expect("ethereum policy");
    assert_eq!(ethereum.top_up_amount, uint!(3_000_000_000_000_000_U256));
    let arbitrum = native_top_up_policy_for_chain(42161).expect("arbitrum policy");
    assert_eq!(arbitrum.top_up_amount, uint!(500_000_000_000_000_U256));
    let polygon = native_top_up_policy_for_chain(137).expect("polygon policy");
    assert_eq!(polygon.top_up_amount, uint!(1_000_000_000_000_000_000_U256));
    let bsc = native_top_up_policy_for_chain(56).expect("bsc policy");
    assert_eq!(bsc.top_up_amount, uint!(5_000_000_000_000_000_U256));
    assert_eq!(native_top_up_policy_for_chain(10), None);
}

#[test]
fn native_top_up_composite_request_adds_adapter_leg_for_non_wrapped_asset() {
    let token = address(0x41);
    let wrapped_native = wrapped_native_token_for_chain(1).expect("weth");
    let recipient = address(0x42);
    let top_up = DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token: wrapped_native,
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    };

    let request = native_top_up_composite_unshield_request(
        token,
        uint!(25_U256),
        recipient,
        false,
        false,
        &top_up,
    )
    .expect("composite request");

    assert_eq!(request.legs.len(), 2);
    assert_eq!(request.legs[0].token_address, token);
    assert_eq!(request.legs[0].amount, uint!(25_U256));
    assert_eq!(
        request.legs[0].recipient,
        CompositeUnshieldRecipient::Public(recipient)
    );
    assert_eq!(request.legs[0].role, CompositeUnshieldLegRole::Primary);
    assert_eq!(request.legs[1].token_address, wrapped_native);
    assert_eq!(request.legs[1].amount, top_up.wrapped_native_amount);
    assert_eq!(
        request.legs[1].amount
            - crate::railgun_protocol_fee_amount(request.legs[1].amount, RAILGUN_PROTOCOL_FEE_BPS,),
        top_up.native_amount
    );
    assert_eq!(
        request.legs[1].recipient,
        CompositeUnshieldRecipient::RelayAdapt
    );
    assert_eq!(request.legs[1].role, CompositeUnshieldLegRole::NativeTopUp);
    let actions = request.relay_actions.expect("relay actions");
    assert_eq!(actions.calls.len(), 2);
    assert_eq!(
        actions.calls[0],
        CompositeRelayAction::UnwrapBase {
            amount: top_up.native_amount,
        }
    );
    assert_eq!(
        actions.calls[1],
        CompositeRelayAction::Transfer {
            token: CompositeRelayActionToken::BaseNative,
            recipient,
            amount: top_up.native_amount,
        }
    );
}

#[test]
fn native_top_up_composite_request_combines_wrapped_output_and_native_top_up() {
    let wrapped_native = wrapped_native_token_for_chain(1).expect("weth");
    let recipient = address(0x43);
    let top_up = DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token: wrapped_native,
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    };
    let selected_amount = uint!(25_U256);
    let selected_net = selected_amount
        - crate::railgun_protocol_fee_amount(selected_amount, RAILGUN_PROTOCOL_FEE_BPS);
    let combined_net = selected_net + top_up.native_amount;
    let combined_wrapped_native_amount =
        crate::railgun_protocol_gross_amount_for_recipient(combined_net, RAILGUN_PROTOCOL_FEE_BPS)
            .expect("combined gross wrapped-native amount");

    let request = native_top_up_composite_unshield_request(
        wrapped_native,
        selected_amount,
        recipient,
        false,
        false,
        &top_up,
    )
    .expect("wrapped composite request");

    assert_eq!(request.legs.len(), 1);
    assert_eq!(request.legs[0].token_address, wrapped_native);
    assert_eq!(request.legs[0].amount, combined_wrapped_native_amount);
    assert_eq!(
        request.legs[0].amount
            - crate::railgun_protocol_fee_amount(request.legs[0].amount, RAILGUN_PROTOCOL_FEE_BPS,),
        combined_net
    );
    assert_eq!(
        request.legs[0].recipient,
        CompositeUnshieldRecipient::RelayAdapt
    );
    assert_eq!(
        request.legs[0].role,
        CompositeUnshieldLegRole::WrappedNativeOutput
    );
    let actions = request.relay_actions.expect("relay actions");
    assert_eq!(actions.calls.len(), 3);
    assert_eq!(
        actions.calls[0],
        CompositeRelayAction::UnwrapBase {
            amount: top_up.native_amount,
        }
    );
    assert_eq!(
        actions.calls[1],
        CompositeRelayAction::Transfer {
            token: CompositeRelayActionToken::BaseNative,
            recipient,
            amount: top_up.native_amount,
        }
    );
    assert_eq!(
        actions.calls[2],
        CompositeRelayAction::Transfer {
            token: CompositeRelayActionToken::Erc20(wrapped_native),
            recipient,
            amount: selected_net,
        }
    );
}

#[test]
fn native_top_up_public_broadcaster_shape_accounts_for_fee_token_spend() {
    let token = address(0x51);
    let wrapped_native = wrapped_native_token_for_chain(1).expect("weth");
    let recipient = address(0x52);
    let top_up = DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token: wrapped_native,
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    };
    let utxos = vec![
        utxo(token, 25, 0, 0).utxo,
        utxo(wrapped_native, 3_007_518_796_992_481, 0, 1).utxo,
    ];

    let _error = native_top_up_approximate_shape(
        &utxos,
        token,
        wrapped_native,
        uint!(25_U256),
        uint!(1_U256),
        &top_up,
    )
    .expect_err("wrapped-native fee should require additional spendable balance");

    let funded_utxos = vec![
        utxo(token, 25, 0, 0).utxo,
        utxo(wrapped_native, 3_007_518_796_992_482, 0, 1).utxo,
    ];
    let shape = native_top_up_approximate_shape(
        &funded_utxos,
        token,
        wrapped_native,
        uint!(25_U256),
        uint!(1_U256),
        &top_up,
    )
    .expect("funded wrapped-native fee shape");
    assert_eq!(shape.relay_call_count, 2);
    assert!(shape.uses_relay_adapt);
}

#[test]
fn native_top_up_public_broadcaster_shape_seeds_third_fee_token_without_fee_amount() {
    let token = address(0x54);
    let fee_token = address(0x55);
    let wrapped_native = wrapped_native_token_for_chain(1).expect("weth");
    let recipient = address(0x56);
    let top_up = DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token: wrapped_native,
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    };
    let amount = uint!(25_U256);
    let non_wrapped_utxos = vec![
        utxo(token, 25, 0, 0).utxo,
        utxo(
            wrapped_native,
            top_up.wrapped_native_amount.to::<u64>(),
            0,
            1,
        )
        .utxo,
        utxo(fee_token, 1, 0, 2).utxo,
    ];

    let non_wrapped_shape = native_top_up_approximate_shape(
        &non_wrapped_utxos,
        token,
        fee_token,
        amount,
        U256::ZERO,
        &top_up,
    )
    .expect("third-token fee seed shape for non-wrapped asset");
    assert_eq!(non_wrapped_shape.transaction_count, 3);
    assert_eq!(non_wrapped_shape.relay_call_count, 2);

    let combined_wrapped_native_amount = native_top_up_required_wrapped_native_amount(
        wrapped_native,
        wrapped_native,
        amount,
        top_up.native_amount,
    );
    let wrapped_utxos = vec![
        utxo(
            wrapped_native,
            combined_wrapped_native_amount.to::<u64>(),
            1,
            0,
        )
        .utxo,
        utxo(fee_token, 1, 1, 1).utxo,
    ];

    let wrapped_shape = native_top_up_approximate_shape(
        &wrapped_utxos,
        wrapped_native,
        fee_token,
        amount,
        U256::ZERO,
        &top_up,
    )
    .expect("third-token fee seed shape for wrapped-native asset");
    assert_eq!(wrapped_shape.transaction_count, 2);
    assert_eq!(wrapped_shape.relay_call_count, 3);
}

#[test]
fn wrapped_native_top_up_report_amounts_match_combined_private_spend() {
    let wrapped_native = wrapped_native_token_for_chain(1).expect("weth");
    let recipient = address(0x53);
    let top_up = DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token: wrapped_native,
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    };
    let split = public_broadcaster_amount_split_for_tokens_and_protocol(
        uint!(1_000_000_U256),
        uint!(400_U256),
        FeeHandlingMode::AddToAmount,
        true,
        RAILGUN_PROTOCOL_FEE_BPS,
    )
    .expect("wrapped-native split");

    let reported = public_broadcaster_reported_amounts(
        wrapped_native,
        wrapped_native,
        split,
        RAILGUN_PROTOCOL_FEE_BPS,
        Some(&top_up),
    );
    let combined_wrapped_native_amount = native_top_up_required_wrapped_native_amount(
        wrapped_native,
        wrapped_native,
        split.receiver_amount,
        top_up.native_amount,
    );
    let expected_protocol_fee = crate::railgun_protocol_fee_amount(
        combined_wrapped_native_amount,
        RAILGUN_PROTOCOL_FEE_BPS,
    );

    assert!(reported.total_private_spend > split.total_private_spend);
    assert_eq!(
        reported.total_private_spend,
        combined_wrapped_native_amount + split.fee_amount
    );
    assert_eq!(reported.protocol_fee_amount, expected_protocol_fee);
    assert_eq!(
        reported.recipient_amount + top_up.native_amount,
        combined_wrapped_native_amount - expected_protocol_fee
    );
}

#[test]
fn native_top_up_primary_recipient_amount_for_fee_mode_reports_protocol_net() {
    let token = address(0x57);
    let wrapped_native = wrapped_native_token_for_chain(1).expect("weth");
    let entered = uint!(1_000_000_U256);
    let native_amount = uint!(3_000_000_000_000_000_U256);

    assert_eq!(
        native_top_up_primary_recipient_amount_for_fee_mode(
            token,
            wrapped_native,
            entered,
            FeeHandlingMode::DeductFromAmount,
            native_amount,
        ),
        uint!(997_500_U256)
    );
    assert_eq!(
        native_top_up_primary_recipient_amount_for_fee_mode(
            token,
            wrapped_native,
            entered,
            FeeHandlingMode::AddToAmount,
            native_amount,
        ),
        entered
    );
    assert_eq!(
        native_top_up_primary_recipient_amount_for_fee_mode(
            wrapped_native,
            wrapped_native,
            entered,
            FeeHandlingMode::DeductFromAmount,
            native_amount,
        ),
        uint!(997_500_U256)
    );
}

#[test]
fn native_top_up_public_broadcaster_shape_rejects_composite_batch_overflow() {
    let token = address(0x61);
    let wrapped_native = wrapped_native_token_for_chain(1).expect("weth");
    let recipient = address(0x62);
    let top_up = DesktopNativeTopUpPlan {
        recipient,
        wrapped_native_token: wrapped_native,
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    };
    let mut utxos = (0..8)
        .map(|tree| utxo(token, 10, tree, 0).utxo)
        .collect::<Vec<_>>();
    utxos.push(
        utxo(
            wrapped_native,
            top_up.wrapped_native_amount.to::<u64>(),
            20,
            0,
        )
        .utxo,
    );

    let error = native_top_up_approximate_shape(
        &utxos,
        token,
        wrapped_native,
        uint!(80_U256),
        U256::ZERO,
        &top_up,
    )
    .expect_err("composite shape should exceed shared batch limit");

    assert!(
        error
            .to_string()
            .contains("composite unshield plan exceeds batch transaction limit")
    );
}

#[test]
fn executor_unwrap_keeps_protocol_fee_and_existing_dust_out_of_transfer() {
    let token = wrapped_native_token_for_chain(1).unwrap();
    let recipient = address(0x42);
    let context = railgun_wallet::tx::ExecutorContext {
        chain_id: 1,
        executor: address(0x43),
        delegate: address(0x44),
        execution_nonce: U256::ZERO,
    };
    let gross = uint!(10_000_U256);
    let request = crate::desktop::desktop_composite_unshield_request(
        token,
        gross,
        recipient,
        true,
        false,
        None,
        Some(context),
    )
    .unwrap()
    .unwrap();
    let net = gross - crate::railgun_protocol_fee_amount(gross, RAILGUN_PROTOCOL_FEE_BPS);
    assert_eq!(request.legs.len(), 1);
    assert_eq!(request.legs[0].amount, gross);
    assert_eq!(
        request.legs[0].recipient,
        CompositeUnshieldRecipient::RelayAdapt
    );
    assert_eq!(
        request.relay_actions.unwrap().calls,
        vec![
            CompositeRelayAction::UnwrapBase { amount: net },
            CompositeRelayAction::Transfer {
                token: CompositeRelayActionToken::BaseNative,
                recipient,
                amount: net,
            },
        ]
    );
    // A direct output must retain its direct route, rather than leaving an unused
    // executor reservation attached to a plain transact call.
    assert!(
        crate::desktop::desktop_composite_unshield_request(
            token,
            gross,
            recipient,
            false,
            false,
            None,
            Some(context),
        )
        .is_err()
    );
    assert!(
        crate::desktop::desktop_composite_unshield_request(
            token, gross, recipient, false, false, None, None,
        )
        .unwrap()
        .is_none()
    );
}
