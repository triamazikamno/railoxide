use super::helpers::*;

#[test]
fn self_broadcast_top_up_preflight_message_explains_current_gas_requirement() {
    let error = self_broadcast_insufficient_native_gas_error(U256::from(7_u64), U256::from(9_u64));

    let message = self_broadcast_preflight_error_message(&error, true);

    assert!(message.contains("insufficient native gas for self-broadcast"));
    assert!(message.contains("cannot pay for the current outer transaction"));
}

#[test]
fn self_broadcast_transaction_request_preserves_delegation_and_bound_identity() {
    use alloy::eips::eip7702::Authorization;
    use alloy::rpc::types::TransactionRequest;
    use alloy::signers::{SignerSync, local::PrivateKeySigner};
    let from = address(0x11);
    let to = address(0x22);
    let calldata = Bytes::from_static(&[0xaa, 0xbb, 0xcc]);
    let owner = PrivateKeySigner::from_bytes(&FixedBytes::repeat_byte(7)).unwrap();
    let authorization = Authorization {
        chain_id: U256::from(5),
        address: address(0x33),
        nonce: 9,
    };
    let signature = owner
        .sign_hash_sync(&authorization.signature_hash())
        .unwrap();
    let authorization = authorization.into_signed(signature);
    let mut prepared = TransactionRequest::default()
        .to(to)
        .input(calldata.clone().into())
        .value(U256::from(3));
    prepared.chain_id = Some(5);
    prepared.from = Some(from);
    prepared.nonce = Some(7);
    prepared.transaction_type = Some(4);
    prepared.authorization_list = Some(vec![authorization.clone()]);
    let tx_req = self_broadcast_transaction_request(5, from, prepared.clone(), 42, 0, 7).unwrap();

    assert_eq!(tx_req.chain_id, Some(5));
    assert_eq!(tx_req.from, Some(from));
    assert_eq!(tx_req.to, Some(to.into()));
    assert_eq!(tx_req.max_fee_per_gas, Some(42));
    assert_eq!(tx_req.max_priority_fee_per_gas, Some(0));
    assert_eq!(tx_req.nonce, Some(7));
    assert_eq!(tx_req.value, Some(U256::from(3)));
    assert_eq!(tx_req.transaction_type, Some(4));
    assert_eq!(
        tx_req.authorization_list.as_deref(),
        Some(std::slice::from_ref(&authorization))
    );
    assert_eq!(
        tx_req.input.input().expect("self-broadcast input"),
        calldata.as_ref()
    );
    for (chain, payer, nonce) in [(6, from, 7), (5, address(0x44), 7), (5, from, 8)] {
        assert!(
            self_broadcast_transaction_request(chain, payer, prepared.clone(), 42, 0, nonce)
                .is_err()
        );
    }
    let legacy = TransactionRequest::default().to(to).input(calldata.into());
    assert!(
        self_broadcast_transaction_request(5, from, legacy, 42, 0, 7)
            .unwrap()
            .authorization_list
            .is_none()
    );
}

#[test]
fn self_broadcast_auto_gas_fee_uses_rpc_gas_price_with_min_tip() {
    let quote = SelfBroadcastGasFeeQuote::from_rpc_gas_price(100);
    let resolved = resolve_self_broadcast_gas_fee(SelfBroadcastGasFeeSelection::Auto, quote)
        .expect("resolve auto gas fee");

    assert_eq!(quote.suggested_max_fee_per_gas, 120);
    assert_eq!(quote.suggested_max_priority_fee_per_gas, 1);
    assert_eq!(resolved.rpc_gas_price, 100);
    assert_eq!(resolved.max_fee_per_gas, 120);
    assert_eq!(resolved.max_priority_fee_per_gas, 1);
}

#[test]
fn self_broadcast_fee_samples_ignore_zero_tips_when_non_zero_exists() {
    let samples = [
        SelfBroadcastFeeSample {
            rpc_gas_price: Some(100),
            max_priority_fee_per_gas: Some(0),
            current_base_fee_per_gas: Some(70),
            next_base_fee_per_gas: Some(80),
            priority_fee_rewards: vec![0, 0, 0],
        },
        SelfBroadcastFeeSample {
            rpc_gas_price: Some(110),
            max_priority_fee_per_gas: Some(0),
            current_base_fee_per_gas: Some(75),
            next_base_fee_per_gas: Some(90),
            priority_fee_rewards: vec![0, 5, 7],
        },
    ];

    let quote = self_broadcast_quote_from_fee_samples(&samples).expect("fee quote");

    assert_eq!(quote.suggested_max_priority_fee_per_gas, 5);
    assert_eq!(quote.rpc_gas_price, 110);
    assert_eq!(quote.current_base_fee_per_gas, Some(75));
    assert_eq!(quote.suggested_max_fee_per_gas, 132);
}

#[test]
fn self_broadcast_fee_samples_use_lower_quartile_priority_suggestion() {
    let samples = [10, 20, 30, 40, 50].map(|tip| SelfBroadcastFeeSample {
        rpc_gas_price: Some(100),
        max_priority_fee_per_gas: Some(tip),
        current_base_fee_per_gas: Some(70),
        next_base_fee_per_gas: Some(80),
        priority_fee_rewards: Vec::new(),
    });

    let quote = self_broadcast_quote_from_fee_samples(&samples).expect("fee quote");

    assert_eq!(quote.suggested_max_priority_fee_per_gas, 20);
    assert_eq!(quote.suggested_max_fee_per_gas, 120);
}

#[test]
fn self_broadcast_fee_samples_can_use_rpc_gas_price_as_tip_fallback() {
    let samples = [SelfBroadcastFeeSample {
        rpc_gas_price: Some(100),
        max_priority_fee_per_gas: Some(0),
        current_base_fee_per_gas: None,
        next_base_fee_per_gas: None,
        priority_fee_rewards: vec![0],
    }];

    let default_quote = self_broadcast_quote_from_fee_samples(&samples).expect("fee quote");
    let rpc_fallback_quote = self_broadcast_quote_from_fee_samples_with_tip_fallback(
        &samples,
        SelfBroadcastTipFallback::RpcGasPrice,
    )
    .expect("fee quote with rpc gas price fallback");

    assert_eq!(default_quote.suggested_max_priority_fee_per_gas, 1);
    assert_eq!(rpc_fallback_quote.suggested_max_fee_per_gas, 120);
    assert_eq!(rpc_fallback_quote.suggested_max_priority_fee_per_gas, 100);
}

#[test]
fn self_broadcast_fee_samples_prefer_non_zero_tip_over_rpc_gas_price_fallback() {
    let samples = [SelfBroadcastFeeSample {
        rpc_gas_price: Some(100),
        max_priority_fee_per_gas: Some(5),
        current_base_fee_per_gas: None,
        next_base_fee_per_gas: None,
        priority_fee_rewards: vec![0],
    }];

    let quote = self_broadcast_quote_from_fee_samples_with_tip_fallback(
        &samples,
        SelfBroadcastTipFallback::RpcGasPrice,
    )
    .expect("fee quote");

    assert_eq!(quote.suggested_max_fee_per_gas, 120);
    assert_eq!(quote.suggested_max_priority_fee_per_gas, 5);
}

#[test]
fn self_broadcast_fee_samples_include_fee_history_base_fee_cap() {
    let samples = [SelfBroadcastFeeSample {
        rpc_gas_price: Some(100),
        max_priority_fee_per_gas: Some(1),
        current_base_fee_per_gas: Some(190),
        next_base_fee_per_gas: Some(200),
        priority_fee_rewards: vec![10],
    }];

    let quote = self_broadcast_quote_from_fee_samples(&samples).expect("fee quote");

    assert_eq!(quote.suggested_max_priority_fee_per_gas, 10);
    assert_eq!(quote.suggested_max_fee_per_gas, 250);
}

#[test]
fn eip1559_projection_uses_cushioned_current_base_and_caps_at_max_fee() {
    let quote = SelfBroadcastGasFeeQuote {
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(100),
        suggested_max_fee_per_gas: 120,
        suggested_max_priority_fee_per_gas: 2,
    };
    let projection = crate::eip1559_gas_cost_projection(10, quote, 1_000, 2);

    assert_eq!(projection.expected_fee_per_gas, 115);
    assert_eq!(projection.expected_cost, U256::from(1_150_u64));
    assert_eq!(projection.maximum_cost, U256::from(10_000_u64));
    assert_eq!(crate::expected_eip1559_fee_per_gas(quote, 110, 2), 110);
}

#[test]
fn eip1559_projection_falls_back_to_max_fee_without_fee_history() {
    let quote = SelfBroadcastGasFeeQuote::from_rpc_gas_price(100);

    assert_eq!(
        crate::expected_eip1559_fee_per_gas(quote, 1_000, 500),
        1_000
    );
    assert_eq!(crate::expected_eip1559_fee_per_gas(quote, 80, 1), 80);
}

#[test]
fn executor_unshield_estimate_includes_delegation_and_chain_buffer_before_allocation() {
    let token = address(0x42);
    let utxos = vec![utxo(token, 10_000, 0, 0).utxo];
    let quote = SelfBroadcastGasFeeQuote::from_rpc_gas_price(100);
    let gas = crate::settings::EffectiveChainGasSettings {
        gas_limit_buffer: 30_000,
        gas_price_buffer_numerator: 1,
        gas_price_buffer_denominator: 1,
    };
    let estimate = |executor_gas| {
        crate::estimate_desktop_unshield_self_broadcast_cost(
            executor_gas,
            &utxos,
            token,
            U256::from(1_000),
            FeeHandlingMode::DeductFromAmount,
            true,
            None,
            quote,
            120,
            2,
        )
        .unwrap()
    };
    let legacy = estimate(None);
    let executor = estimate(Some(&gas));
    let without_buffer = estimate(Some(&crate::settings::EffectiveChainGasSettings {
        gas_limit_buffer: 0,
        ..gas
    }));
    assert!(without_buffer.gas_limit > legacy.gas_limit);
    assert_eq!(
        executor.gas_limit,
        without_buffer.gas_limit + gas.gas_limit_buffer
    );
    assert_eq!(
        executor.gas_cost.maximum_cost,
        U256::from(executor.gas_limit) * U256::from(120)
    );
    assert_eq!(executor.protocol_fees, legacy.protocol_fees);
}

#[test]
fn direct_self_broadcast_estimates_private_send_and_unshield_costs() {
    let token = address(0x42);
    let utxos = vec![utxo(token, 10_000, 0, 0).utxo];
    let quote = SelfBroadcastGasFeeQuote {
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(100),
        suggested_max_fee_per_gas: 120,
        suggested_max_priority_fee_per_gas: 2,
    };

    let send = crate::estimate_desktop_send_self_broadcast_cost(
        &utxos,
        token,
        U256::from(1_000_u64),
        quote,
        120,
        2,
    )
    .expect("send estimate");
    let unshield = crate::estimate_desktop_unshield_self_broadcast_cost(
        None,
        &utxos,
        token,
        U256::from(1_000_u64),
        FeeHandlingMode::DeductFromAmount,
        false,
        None,
        quote,
        120,
        2,
    )
    .expect("unshield estimate");

    assert!(!send.gas_cost.expected_cost.is_zero());
    assert!(send.gas_cost.maximum_cost > send.gas_cost.expected_cost);
    assert!(send.protocol_fees.is_empty());
    assert_eq!(
        unshield.protocol_fees.as_slice(),
        &[crate::DesktopSelfBroadcastProtocolFee {
            token,
            amount: crate::unshield_protocol_fee_amount_for_fee_mode(
                U256::from(1_000_u64),
                FeeHandlingMode::DeductFromAmount,
            )
            .expect("protocol fee"),
        }],
    );
    assert_ne!(unshield.gas_limit, 0);
}

#[test]
fn direct_self_broadcast_estimate_includes_each_native_top_up_protocol_fee() {
    let token = address(0x42);
    let wrapped_native = address(0x43);
    let native_amount = U256::from(100_u64);
    let native_top_up = DesktopNativeTopUpPlan {
        recipient: address(0x44),
        wrapped_native_token: wrapped_native,
        native_amount,
        wrapped_native_amount: crate::native_top_up_wrapped_native_amount(native_amount),
    };
    let quote = SelfBroadcastGasFeeQuote::from_rpc_gas_price(100);
    let utxos = vec![
        utxo(token, 10_000, 0, 0).utxo,
        utxo(wrapped_native, 10_000, 0, 1).utxo,
    ];

    let separate_tokens = crate::estimate_desktop_unshield_self_broadcast_cost(
        None,
        &utxos,
        token,
        U256::from(1_000_u64),
        FeeHandlingMode::DeductFromAmount,
        false,
        Some(&native_top_up),
        quote,
        120,
        1,
    )
    .expect("separate-token native top-up estimate");
    assert_eq!(separate_tokens.protocol_fees.len(), 2);
    assert_eq!(separate_tokens.protocol_fees[0].token, token);
    assert_eq!(separate_tokens.protocol_fees[1].token, wrapped_native);
    assert_eq!(
        separate_tokens.protocol_fees[1].amount,
        native_top_up.wrapped_native_amount - native_amount,
    );

    let wrapped_utxos = vec![utxo(wrapped_native, 10_000, 0, 0).utxo];
    let entered_amount = U256::from(1_000_u64);
    let combined_amount = crate::native_top_up_required_wrapped_native_amount_for_fee_mode(
        wrapped_native,
        wrapped_native,
        entered_amount,
        FeeHandlingMode::DeductFromAmount,
        native_amount,
    );
    let primary_recipient = crate::native_top_up_primary_recipient_amount_for_fee_mode(
        wrapped_native,
        wrapped_native,
        entered_amount,
        FeeHandlingMode::DeductFromAmount,
        native_amount,
    );
    let combined = crate::estimate_desktop_unshield_self_broadcast_cost(
        None,
        &wrapped_utxos,
        wrapped_native,
        entered_amount,
        FeeHandlingMode::DeductFromAmount,
        false,
        Some(&native_top_up),
        quote,
        120,
        1,
    )
    .expect("combined wrapped-native top-up estimate");
    assert_eq!(combined.protocol_fees.len(), 1);
    assert_eq!(combined.protocol_fees[0].token, wrapped_native);
    assert_eq!(
        combined.protocol_fees[0].amount,
        combined_amount - primary_recipient - native_amount,
    );
}

#[test]
fn self_broadcast_already_known_classifier_excludes_nonce_errors() {
    for message in [
        "already known",
        "already in mempool",
        "known transaction: 0xabc",
        "transaction already imported",
        "Transaction already exists",
    ] {
        assert!(
            is_self_broadcast_tx_already_known_message(message),
            "expected {message:?} to be classified as already known"
        );
    }

    for message in [
        "nonce too low",
        "replacement transaction underpriced",
        "transaction gas price below minimum",
    ] {
        assert!(
            !is_self_broadcast_tx_already_known_message(message),
            "expected {message:?} to remain retryable"
        );
    }
}

#[test]
fn self_broadcast_custom_gas_fee_validates_caps() {
    assert!(validate_self_broadcast_gas_fee(1, 0).is_ok());
    assert!(validate_self_broadcast_gas_fee(1, 1).is_ok());
    assert!(validate_self_broadcast_gas_fee(0, 0).is_err());
    assert!(validate_self_broadcast_gas_fee(1, 2).is_err());
}

#[test]
fn self_broadcast_replacement_bump_uses_ceil_twelve_point_five_percent() {
    assert_eq!(crate::self_broadcast_replacement_bumped_fee(0), 0);
    assert_eq!(crate::self_broadcast_replacement_bumped_fee(1), 2);
    assert_eq!(crate::self_broadcast_replacement_bumped_fee(8), 9);
    assert_eq!(crate::self_broadcast_replacement_bumped_fee(100), 113);
}

#[test]
fn self_broadcast_gas_cost_uses_max_fee_cap() {
    assert_eq!(
        self_broadcast_gas_limit_with_buffer(21_000, 5_000, None).unwrap(),
        26_000
    );
    assert_eq!(
        self_broadcast_gas_limit_with_buffer(u64::MAX, 1, None).unwrap(),
        u64::MAX
    );
    assert_eq!(
        self_broadcast_gas_limit_with_buffer(21_000, 5_000, Some(26_000)).unwrap(),
        26_000
    );
    assert!(self_broadcast_gas_limit_with_buffer(21_000, 5_000, Some(25_999)).is_err());
    assert_eq!(
        self_broadcast_native_gas_cost(26_000, 2_000_000_000),
        U256::from(52_000_000_000_000_u128)
    );
}

#[test]
fn self_broadcast_insufficient_gas_error_is_terminal_and_formatted() {
    let error = self_broadcast_insufficient_native_gas_error(U256::from(7_u64), U256::from(9_u64));

    assert!(is_self_broadcast_insufficient_native_gas_error(&error));
    assert_eq!(
        error.to_string(),
        "insufficient native gas for self-broadcast: live balance 7, estimated cost 9"
    );
}

#[test]
fn self_broadcast_pending_spent_hash_parsing_accepts_submitted_tx_hash() {
    let hash = "0x1111111111111111111111111111111111111111111111111111111111111111";

    assert_eq!(
        parse_submitted_tx_hash(hash),
        Some(FixedBytes::from([0x11; 32]))
    );
    assert_eq!(parse_submitted_tx_hash("not-a-hash"), None);
}
