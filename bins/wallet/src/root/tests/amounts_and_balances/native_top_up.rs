use super::*;

#[test]
fn native_top_up_plan_display_and_request_use_fixed_native_amount() {
    let token = Address::from([0x41; 20]);
    let candidate = wallet_ops::public_broadcaster_candidates_for_asset(
        &[fee_row(1, token, "native-top-up")],
        1,
        token,
        BroadcasterFeePolicy::default(),
        None,
    )
    .expect("candidate")
    .remove(0);
    let mut estimate = public_broadcaster_cost_estimate(candidate);
    estimate.native_top_up = Some(DesktopNativeTopUpPlan {
        recipient: Address::from([0x42; 20]),
        wrapped_native_token: token,
        native_amount: uint!(3_000_000_000_000_000_U256),
        wrapped_native_amount: wallet_ops::native_top_up_wrapped_native_amount(uint!(
            3_000_000_000_000_000_U256
        )),
    });

    let display = PublicBroadcasterCostDisplay::from_estimate_chain(1, &estimate, None, None);
    assert_eq!(
        display.native_top_up_recipient_suffix(),
        Some("+ 0.003 ETH (gas top-up)".to_string())
    );
    assert_eq!(
        estimate
            .native_top_up
            .as_ref()
            .map(|top_up| top_up.wrapped_native_amount),
        Some(uint!(3_007_518_796_992_481_U256))
    );

    assert_eq!(
        native_top_up_request_from_plan(estimate.native_top_up.as_ref()),
        Some(wallet_ops::DesktopNativeTopUpRequest)
    );

    let result = PublicBroadcasterSubmissionResult {
        broadcaster: estimate.broadcaster.clone(),
        action_token: estimate.action_token,
        fee_token: estimate.fee_token,
        entered_amount: estimate.entered_amount,
        receiver_amount: estimate.receiver_amount,
        recipient_amount: estimate.recipient_amount,
        total_private_spend: estimate.total_private_spend,
        fee_amount: estimate.fee_amount,
        protocol_fee_amount: estimate.protocol_fee_amount,
        protocol_fee_bps: estimate.protocol_fee_bps,
        fee_mode: estimate.fee_mode,
        gas_limit: estimate.gas_limit,
        min_gas_price: estimate.min_gas_price,
        transaction_count: estimate.transaction_count,
        input_count: estimate.input_count,
        private_output_count: estimate.private_output_count,
        public_output_count: estimate.public_output_count,
        relay_call_count: estimate.relay_call_count,
        uses_relay_adapt: estimate.uses_relay_adapt,
        result: PublicBroadcasterResultKind::Submitted {
            tx_hash: alloy::primitives::B256::repeat_byte(0xab),
        },
        native_top_up: estimate.native_top_up.clone(),
        payload_inputs: None,
    };
    let display = PublicBroadcasterCostDisplay::from_result(&result, None, None);
    assert_eq!(
        display.native_top_up_recipient_suffix(),
        Some("+ 0.003 ETH (gas top-up)".to_string())
    );
}

#[test]
fn native_top_up_eligibility_accepts_known_and_arbitrary_recipients_but_stays_explicit() {
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let recipient = Address::from([0x42; 20]);
    let private_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![unshield_utxo_output(weth, 3_007_518_796_992_481, 0, 1)],
        totals: Vec::new(),
    };
    let state = unshield_native_top_up_state_from_inputs(
        1,
        usdc,
        false,
        recipient,
        uint!(100_000_000_U256),
        wallet_ops::FeeHandlingMode::DeductFromAmount,
        Some(&private_snapshot),
        Some(weth),
    );
    let plan = state.plan.expect("eligible top-up plan");
    assert_eq!(plan.recipient, recipient);
    assert_eq!(plan.native_amount, uint!(3_000_000_000_000_000_U256));
    assert_eq!(
        plan.wrapped_native_amount,
        uint!(3_007_518_796_992_481_U256)
    );
    assert_eq!(
        enabled_native_top_up_plan(false, Some(&plan)),
        None,
        "eligibility must not auto-select the top-up"
    );
    assert_eq!(
        enabled_native_top_up_plan(true, Some(&plan)),
        Some(plan.clone())
    );

    let arbitrary = unshield_native_top_up_state_from_inputs(
        1,
        usdc,
        false,
        Address::from([0x43; 20]),
        uint!(100_000_000_U256),
        wallet_ops::FeeHandlingMode::DeductFromAmount,
        Some(&private_snapshot),
        Some(weth),
    );
    assert!(arbitrary.plan.is_some());
}

#[test]
fn native_top_up_eligibility_uses_add_on_top_gross_amount() {
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let recipient = Address::from([0x42; 20]);
    let entered_amount = uint!(1_000_000_U256);
    let top_up_amount = uint!(3_000_000_000_000_000_U256);
    let old_required =
        entered_amount + wallet_ops::native_top_up_wrapped_native_amount(top_up_amount);
    let gross_required = wallet_ops::native_top_up_required_wrapped_native_amount_for_fee_mode(
        weth,
        weth,
        entered_amount,
        wallet_ops::FeeHandlingMode::AddToAmount,
        top_up_amount,
    );
    assert!(gross_required > old_required);
    let private_snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![unshield_utxo_output(weth, old_required.to::<u64>(), 0, 1)],
        totals: Vec::new(),
    };
    let state = unshield_native_top_up_state_from_inputs(
        1,
        weth,
        false,
        recipient,
        entered_amount,
        wallet_ops::FeeHandlingMode::AddToAmount,
        Some(&private_snapshot),
        Some(weth),
    );

    assert!(state.plan.is_none());
}

#[test]
fn native_top_up_passive_eligibility_refresh_does_not_cancel_estimate() {
    assert!(!native_top_up_refresh_invalidates_estimate(
        false, false, true
    ));
    assert!(!native_top_up_refresh_invalidates_estimate(
        false, true, true
    ));
    assert!(native_top_up_refresh_invalidates_estimate(true, true, true));
    assert!(native_top_up_refresh_invalidates_estimate(
        true, false, false
    ));
}
