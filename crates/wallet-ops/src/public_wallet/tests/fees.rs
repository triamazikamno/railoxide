use super::*;

#[test]
fn public_native_action_gas_reserve_uses_buffered_units() {
    let send_steps = [PublicActionProgressStep::Send];
    assert_eq!(
        public_native_action_gas_units(1, &send_steps),
        PUBLIC_NATIVE_SEND_GAS_UNITS + GAS_LIMIT_BUFFER,
    );
    assert_eq!(
        public_native_action_gas_reserve(1, 2, &send_steps),
        U256::from((PUBLIC_NATIVE_SEND_GAS_UNITS + GAS_LIMIT_BUFFER) * 2),
    );

    let shield_steps = [
        PublicActionProgressStep::ShieldKey,
        PublicActionProgressStep::Shield,
    ];
    assert_eq!(
        public_native_action_gas_units(1, &shield_steps),
        public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::UpperBound)
            + GAS_LIMIT_BUFFER,
    );
    assert_eq!(
        public_native_action_gas_units_with_buffer(&ETHEREUM_GAS_MODEL, &send_steps, 7),
        PUBLIC_NATIVE_SEND_GAS_UNITS + 7,
    );
}

#[test]
fn public_action_gas_cost_separates_execution_and_signed_units() {
    let token = address!("0x3333333333333333333333333333333333333333");
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 2,
        current_base_fee_per_gas: Some(1),
        suggested_max_fee_per_gas: 3,
        suggested_max_priority_fee_per_gas: 1,
    };

    let native_send = estimate_public_action_gas_cost(
        1,
        &default_effective_chain(1),
        PublicActionKind::Send,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("native send estimate");
    assert_eq!(
        native_send.expected_cost,
        U256::from(PUBLIC_NATIVE_SEND_GAS_UNITS * 3)
    );
    assert_eq!(
        native_send.maximum_cost,
        U256::from((PUBLIC_NATIVE_SEND_GAS_UNITS + GAS_LIMIT_BUFFER) * 3)
    );

    let erc20_send = estimate_public_action_gas_cost(
        1,
        &default_effective_chain(1),
        PublicActionKind::Send,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("erc20 send estimate");
    assert_eq!(erc20_send.expected_cost, U256::from(65_000_u64 * 3));
    assert_eq!(
        erc20_send.maximum_cost,
        U256::from((65_000 + GAS_LIMIT_BUFFER) * 3)
    );

    let native_shield = estimate_public_action_gas_cost(
        1,
        &default_effective_chain(1),
        PublicActionKind::Shield,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
        },
        None,
    )
    .expect("native shield estimate");
    // The Railoxide wrap fallback: wrap, approval and Shield, each with its own buffer.
    assert_eq!(
        native_shield.expected_cost,
        U256::from(
            PUBLIC_NATIVE_WRAP_GAS_UNITS
                + PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::Expected)
        )
    );
    assert_eq!(
        native_shield.maximum_cost,
        U256::from(
            PUBLIC_NATIVE_WRAP_GAS_UNITS
                + PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::UpperBound)
                + (3 * GAS_LIMIT_BUFFER)
        )
    );
    let erc20_shield = estimate_public_action_gas_cost(
        1,
        &default_effective_chain(1),
        PublicActionKind::Shield,
        PublicAssetId::Erc20(token),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
        },
        None,
    )
    .expect("erc20 shield estimate");
    assert_eq!(
        erc20_shield.expected_cost,
        U256::from(
            PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::Expected)
        )
    );
    assert_eq!(
        erc20_shield.maximum_cost,
        U256::from(
            PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::UpperBound)
                + (2 * GAS_LIMIT_BUFFER),
        )
    );
}

#[tokio::test]
async fn advanced_simulation_uses_each_provider_once_and_selects_revert_plurality() {
    let revert_servers = [
        MockRpcServer::spawn_revert("Order has expired").await,
        MockRpcServer::spawn_revert("Order has expired").await,
        MockRpcServer::spawn_revert("Order has expired").await,
    ];
    let unavailable_server = MockRpcServer::spawn(true, 50_000).await;
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let mut chain = effective_chain_for_rpc(&revert_servers[0].url, 8_000);
    chain.rpc_route = RpcChainRoute::new(
        1,
        revert_servers
            .iter()
            .map(|server| Url::parse(&server.url).expect("RPC URL"))
            .chain(std::iter::once(
                Url::parse(&unavailable_server.url).expect("RPC URL"),
            ))
            .collect(),
    )
    .with_multicall(chain.rpc_route.multicall().expect("multicall"));
    let request = PublicAdvancedTransactionEstimateRequest {
        chain_id: 1,
        effective_chain: chain,
        from,
        intent: PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(7_u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        },
        gas_fee: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
        },
        access_list: None,
    };
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(80),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 2,
    };
    let resolved = PublicActionResolvedGasFee {
        rpc_gas_price: 100,
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };

    let http = HttpContext::direct_for_tests();
    let result = simulate_public_advanced_transaction_with_fee(
        request,
        quote,
        resolved,
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await;
    assert!(matches!(
        result,
        Err(PublicAdvancedTransactionSimulationError::Reverted(reason))
            if reason == "Order has expired"
    ));
    for server in &revert_servers {
        assert_eq!(
            rpc_calls_for(&server.calls(), "eth_estimateGas").len(),
            1,
            "each configured provider should receive one estimate request"
        );
    }
    assert_eq!(
        rpc_calls_for(&unavailable_server.calls(), "eth_estimateGas").len(),
        1
    );
}

/// Chain 1 on `endpoints`, each of which must pass an identity check before a fee read.
fn identity_checked_chain(endpoints: Vec<Url>) -> EffectiveChainConfig {
    let mut chain = effective_chain_for_rpc(endpoints[0].as_str(), 8_000);
    chain.rpc_route = RpcChainRoute::new(1, endpoints)
        .with_identity_verification()
        .with_multicall(chain.rpc_route.multicall().expect("multicall"));
    chain
}

fn raw_estimate_request(chain: EffectiveChainConfig) -> PublicAdvancedTransactionEstimateRequest {
    PublicAdvancedTransactionEstimateRequest {
        chain_id: 1,
        effective_chain: chain,
        from: Address::repeat_byte(1),
        intent: PublicTransactionIntent::Raw {
            to: Some(Address::repeat_byte(2)),
            value: U256::from(7_u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        },
        gas_fee: PublicActionGasFeeSelection::Auto,
        access_list: None,
    }
}

#[tokio::test]
async fn gas_only_public_reads_never_reach_a_wrong_chain_endpoint() {
    // Sampled, the wrong-chain endpoint would set the upper-quartile gas price.
    let (wrong, wrong_server, wrong_methods) =
        logged_fee_endpoint(5, 1_000, Some(99_000), None).await;
    let (valid, valid_server, _) = logged_fee_endpoint(1, 100, Some(42_000), None).await;
    let chain = identity_checked_chain(vec![wrong, valid]);
    let http = HttpContext::direct_for_tests();

    let quote = quote_public_action_gas_fee(1, &chain, &http)
        .await
        .expect("quote from the valid endpoint");
    assert_eq!(quote.rpc_gas_price, 100);
    assert_eq!(*wrong_methods.lock().unwrap(), ["eth_chainId"]);

    // The estimate's quote and estimate stages share one source, so the endpoint is checked
    // once more rather than once per stage.
    let estimate = estimate_public_advanced_transaction(
        raw_estimate_request(chain),
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await
    .expect("estimate from the valid endpoint");
    assert_eq!(estimate.raw_gas_limit, 42_000);
    assert_eq!(
        *wrong_methods.lock().unwrap(),
        ["eth_chainId", "eth_chainId"]
    );
    wrong_server.abort();
    valid_server.abort();
}

#[tokio::test]
async fn advanced_estimate_uses_an_endpoint_admitted_after_its_quote() {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);
    // The first endpoint quotes but cannot estimate; the second passes its identity check only
    // once the quote has finished and the first estimate has failed.
    let (quoting, quoting_server, quoting_methods) = logged_fee_endpoint(1, 100, None, None).await;
    let gate = crate::rpc_broker::tests::RpcMockGate::default();
    let (late, late_server, late_methods) =
        logged_fee_endpoint(1, 100, Some(42_000), Some(gate.clone())).await;
    let chain = identity_checked_chain(vec![quoting, late]);
    let http = HttpContext::direct_for_tests();
    let release_late_endpoint = async {
        tokio::time::timeout(WAIT, gate.request_started.notified())
            .await
            .expect("the late endpoint's identity is checked");
        while !quoting_methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "eth_estimateGas")
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Releases the identity check, then the estimate that follows it.
        gate.release_response.notify_one();
        gate.request_started.notified().await;
        gate.release_response.notify_one();
    };
    let client = http.operation_http_client().await.unwrap();
    let (estimate, released) = tokio::join!(
        tokio::time::timeout(
            WAIT,
            estimate_public_advanced_transaction(raw_estimate_request(chain), &http, &client),
        ),
        tokio::time::timeout(WAIT, release_late_endpoint),
    );
    let estimate = estimate
        .expect("the estimate finishes")
        .expect("the late endpoint estimates");
    released.expect("the quote finishes and the first estimate fails before the release");
    assert_eq!(estimate.raw_gas_limit, 42_000);
    assert_eq!(
        *late_methods.lock().unwrap(),
        ["eth_chainId", "eth_estimateGas"]
    );
    quoting_server.abort();
    late_server.abort();
}

#[test]
fn railway_profile_uses_floor_multiplier_and_no_native_gas_model() {
    assert_eq!(railway_gas_limit(100_001), 120_001);

    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 1,
        current_base_fee_per_gas: Some(1),
        suggested_max_fee_per_gas: 1,
        suggested_max_priority_fee_per_gas: 0,
    };
    // A native Mimic shield's fees come only from its prepared estimate, never a fixed limit.
    assert!(
        estimate_public_action_gas_cost_with_profile(
            1,
            &default_effective_chain(1),
            PublicActionKind::Shield,
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railway,
            PublicActionGasFeeSelection::Auto,
            Some(quote),
        )
        .is_err()
    );

    let token = address!("0x3333333333333333333333333333333333333333");
    let erc20 = estimate_public_action_gas_cost_with_profile(
        1,
        &default_effective_chain(1),
        PublicActionKind::Shield,
        PublicAssetId::Erc20(token),
        PublicShieldTransactionProfile::Railway,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railway ERC-20 shield estimate");
    assert_eq!(
        erc20.expected_cost,
        U256::from(
            PUBLIC_NATIVE_APPROVE_GAS_UNITS
                + public_shield_gas_units(&ETHEREUM_GAS_MODEL, GasEstimateMode::Expected)
        )
    );
    assert_eq!(
        erc20.maximum_cost,
        U256::from(
            railway_gas_limit(PUBLIC_NATIVE_APPROVE_GAS_UNITS)
                + railway_gas_limit(public_shield_gas_units(
                    &ETHEREUM_GAS_MODEL,
                    GasEstimateMode::UpperBound
                ))
        )
    );
}

#[test]
fn railway_bnb_legacy_fee_resolution_is_erc20_only() {
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 7,
        current_base_fee_per_gas: Some(5),
        suggested_max_fee_per_gas: 12,
        suggested_max_priority_fee_per_gas: 2,
    };
    let token = PublicAssetId::Erc20(address!("0x3333333333333333333333333333333333333333"));
    let auto = super::gas::resolve_public_action_gas_fee(
        56,
        PublicShieldTransactionProfile::Railway,
        token,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railway BNB auto fee");
    assert_eq!(auto.max_fee_per_gas, 7);
    assert_eq!(auto.max_priority_fee_per_gas, 0);
    let native = super::gas::resolve_public_action_gas_fee(
        56,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railway BNB native type-4 fee");
    assert_eq!(native.max_fee_per_gas, 12);
    assert_eq!(native.max_priority_fee_per_gas, 2);
    // Observing a legacy chain never downgrades the native type-4 envelope.
    let legacy_observed = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Legacy),
        ..quote
    };
    assert!(!super::gas::public_action_uses_legacy_envelope(
        56,
        PublicShieldTransactionProfile::Railway,
        PublicAssetId::Native,
        Some(legacy_observed),
    ));

    let railoxide = super::gas::resolve_public_action_gas_fee(
        56,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railoxide generic fee");
    assert_eq!(railoxide.max_fee_per_gas, 12);
    assert_eq!(railoxide.max_priority_fee_per_gas, 2);

    let custom = super::gas::resolve_public_action_gas_fee(
        56,
        PublicShieldTransactionProfile::Railway,
        token,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 11,
            max_priority_fee_per_gas: 3,
        },
        Some(quote),
    )
    .expect("Railway BNB custom fee");
    assert_eq!(custom.max_fee_per_gas, 11);
    assert_eq!(custom.max_priority_fee_per_gas, 0);

    let projection = estimate_public_action_gas_cost_with_profile(
        56,
        &default_effective_chain(56),
        PublicActionKind::Shield,
        token,
        PublicShieldTransactionProfile::Railway,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
    )
    .expect("Railway BNB gas projection");
    assert_eq!(projection.expected_fee_per_gas, 7);
    assert_eq!(projection.maximum_fee_per_gas, 7);
    let projection_with_ceiling = estimate_public_action_gas_cost_with_profile_and_ceiling(
        56,
        &default_effective_chain(56),
        PublicActionKind::Shield,
        token,
        PublicShieldTransactionProfile::Railway,
        PublicActionGasFeeSelection::Auto,
        Some(quote),
        Some(PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 13,
            max_priority_fee_per_gas: 0,
        }),
    )
    .expect("Railway BNB gas projection with ceiling");
    assert_eq!(
        projection_with_ceiling.expected_cost,
        projection.expected_cost
    );
    assert_eq!(
        projection_with_ceiling.maximum_cost,
        projection.maximum_cost / U256::from(7) * U256::from(13)
    );
}

#[test]
fn railway_standard_quote_uses_lower_median_60th_rewards_and_110_percent_base() {
    let base_fees = [80, 90, 100, 101];
    let rewards = vec![
        vec![1, 40, 80, 95],
        vec![1, 10, 80, 95],
        vec![1, 30, 80, 95],
        vec![1, 20, 80, 95],
    ];
    let quote = railway_standard_gas_fee_quote(&base_fees, Some(rewards.as_slice()))
        .expect("Railway standard quote");
    assert_eq!(quote.suggested_max_priority_fee_per_gas, 20);
    assert_eq!(quote.suggested_max_fee_per_gas, 131);
    assert_eq!(quote.current_base_fee_per_gas, Some(100));
    let bundle = railway_standard_gas_fee_quote_bundle(&base_fees, Some(rewards.as_slice()), None)
        .expect("Railway standard and aggressive quote");
    assert_eq!(bundle.standard, quote);
    // BNB native caps the next base fee before the 110% multiplier: floor(50 * 1.1) + 20.
    let capped =
        railway_standard_gas_fee_quote_bundle(&base_fees, Some(rewards.as_slice()), Some(50))
            .expect("Railway BNB native quote")
            .standard;
    assert_eq!(capped.suggested_max_fee_per_gas, 75);
    assert_eq!(capped.suggested_max_priority_fee_per_gas, 20);
    // Unusable observations fail this provider rather than being repaired.
    for (base_fees, rewards) in [
        (&[][..], Some(rewards.as_slice())),
        (&base_fees[..], None),
        (&base_fees[..], Some(&[][..])),
        (&base_fees[..], Some(&[vec![1]][..])),
    ] {
        assert!(railway_standard_gas_fee_quote_bundle(base_fees, rewards, Some(50)).is_err());
    }
    assert_eq!(
        bundle.authorization_ceiling,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 236,
            max_priority_fee_per_gas: 95,
        }
    );
}

#[test]
fn railway_bnb_quote_caps_provider_gas_price_before_110_percent_floor() {
    assert_eq!(
        railway_bnb_gas_fee_quote(60_000_000).rpc_gas_price,
        55_000_000
    );
    assert_eq!(
        railway_bnb_gas_fee_quote(40_000_000).rpc_gas_price,
        44_000_000
    );
    assert_eq!(
        railway_bnb_gas_fee_quote_bundle(60_000_000).authorization_ceiling,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 70_000_000,
            max_priority_fee_per_gas: 0,
        }
    );
    assert_eq!(
        railway_bnb_gas_fee_quote_bundle(40_000_000).authorization_ceiling,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 56_000_000,
            max_priority_fee_per_gas: 0,
        }
    );
}

#[test]
fn railway_auto_fee_ceiling_allows_decreases_but_rejects_increases() {
    let ceiling = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 130,
        max_priority_fee_per_gas: 20,
    };
    assert!(railway_auto_fee_within_authorized_ceiling(
        1,
        PublicAssetId::Erc20(Address::ZERO),
        ceiling,
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 130,
            max_priority_fee_per_gas: 20,
        },
    ));
    assert!(railway_auto_fee_within_authorized_ceiling(
        1,
        PublicAssetId::Erc20(Address::ZERO),
        ceiling,
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 129,
            max_priority_fee_per_gas: 19,
        },
    ));
    assert!(!railway_auto_fee_within_authorized_ceiling(
        1,
        PublicAssetId::Erc20(Address::ZERO),
        ceiling,
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 131,
            max_priority_fee_per_gas: 20,
        },
    ));
    assert!(!railway_auto_fee_within_authorized_ceiling(
        1,
        PublicAssetId::Erc20(Address::ZERO),
        ceiling,
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 130,
            max_priority_fee_per_gas: 21,
        },
    ));
    assert!(railway_auto_fee_within_authorized_ceiling(
        56,
        PublicAssetId::Erc20(Address::ZERO),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 55,
            max_priority_fee_per_gas: 0,
        },
        &crate::SelfBroadcastResolvedGasFee {
            rpc_gas_price: 0,
            max_fee_per_gas: 55,
            max_priority_fee_per_gas: 999,
        },
    ));
}

#[test]
fn railway_auto_approval_and_shield_steps_start_from_auto_independently() {
    let authorized_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 130,
        max_priority_fee_per_gas: 20,
    };
    assert_eq!(
        public_action_step_initial_gas_fee_selection(
            PublicShieldTransactionProfile::Railway,
            PublicActionStepFeePolicy::RefreshRailwayStandard,
            authorized_fee,
        ),
        PublicActionGasFeeSelection::Auto
    );
    assert_eq!(
        public_action_step_initial_gas_fee_selection(
            PublicShieldTransactionProfile::Railway,
            PublicActionStepFeePolicy::Captured,
            authorized_fee,
        ),
        authorized_fee
    );
    assert_eq!(
        public_action_step_initial_gas_fee_selection(
            PublicShieldTransactionProfile::Railoxide,
            PublicActionStepFeePolicy::RefreshRailwayStandard,
            authorized_fee,
        ),
        authorized_fee
    );
}

#[test]
fn public_action_eip1559_request_sets_fee_caps_and_nonce() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let base = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount: U256::from(5_u64),
            recipient,
        },
    )
    .expect("native transfer request");

    let tx = public_action_eip1559_transaction_request(base, 1, from, 42, 3, 9);

    assert_eq!(tx.chain_id, Some(1));
    assert_eq!(tx.from, Some(from));
    assert_eq!(tx.to, Some(recipient.into()));
    assert_eq!(tx.max_fee_per_gas, Some(42));
    assert_eq!(tx.max_priority_fee_per_gas, Some(3));
    assert_eq!(tx.nonce, Some(9));
}

#[test]
fn railway_profile_uses_legacy_envelope_only_on_bnb() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let base = public_send_transaction_request(
        56,
        from,
        &PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount: U256::from(5_u64),
            recipient,
        },
    )
    .expect("native transfer request");
    let legacy = public_action_legacy_transaction_request(base, 56, from, 42, 9);
    assert_eq!(legacy.gas_price, Some(42));
    assert_eq!(legacy.max_fee_per_gas, None);
    assert_eq!(legacy.max_priority_fee_per_gas, None);
    let token = PublicAssetId::Erc20(Address::ZERO);
    assert!(PublicShieldTransactionProfile::Railway.uses_legacy_envelope(56, token));
    assert!(
        !PublicShieldTransactionProfile::Railway.uses_legacy_envelope(56, PublicAssetId::Native)
    );
    for chain_id in [1, 137, 42161] {
        assert!(!PublicShieldTransactionProfile::Railway.uses_legacy_envelope(chain_id, token));
    }
}

#[test]
fn public_action_replacement_bump_reuses_self_broadcast_policy() {
    assert_eq!(public_action_replacement_bumped_fee(8), 9);
    assert_eq!(public_action_replacement_bumped_fee(9), 11);
}

#[test]
fn public_action_tip_fallback_uses_rpc_gas_price_only_for_bnb() {
    assert_eq!(
        public_action_tip_fallback(56),
        SelfBroadcastTipFallback::RpcGasPrice,
    );
    assert_eq!(
        public_action_tip_fallback(1),
        SelfBroadcastTipFallback::Minimum,
    );
}
