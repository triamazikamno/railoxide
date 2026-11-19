use super::*;

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
