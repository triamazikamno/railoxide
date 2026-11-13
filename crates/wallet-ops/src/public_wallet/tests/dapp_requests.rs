use super::*;

#[test]
fn walletconnect_send_rejects_expired_request_before_raw_broadcast() {
    assert!(ensure_public_action_broadcast_not_expired(None, "walletconnect").is_ok());
    assert!(
        ensure_public_action_broadcast_not_expired(
            Some(public_action_current_unix_seconds() + 60),
            "walletconnect",
        )
        .is_ok()
    );

    let error = ensure_public_action_broadcast_not_expired(
        Some(public_action_current_unix_seconds()),
        "walletconnect",
    )
    .expect_err("expired request");

    assert!(
        error
            .to_string()
            .contains("request expired before transaction broadcast")
    );
}

#[tokio::test]
async fn walletconnect_rpc_privacy_and_submission_boundaries_use_one_http_context() {
    let server = MockRpcServer::spawn(false, 50_000).await;
    let chain = effective_chain_for_rpc(&server.url, 12_345);

    for (mode, route) in [
        (WalletNetworkMode::Tor, "tor"),
        (WalletNetworkMode::Proxy, "proxy"),
        (WalletNetworkMode::Direct, "direct"),
    ] {
        let http = http_context_for_route(mode, route);
        quote_public_action_gas_fee(1, &chain, &http)
            .await
            .expect("request-independent fee quote");
    }

    let quote_calls = server.calls();
    assert!(!quote_calls.is_empty());
    assert!(quote_calls.iter().all(|call| {
        matches!(
            call.method.as_str(),
            "eth_getBlockByNumber" | "eth_gasPrice" | "eth_maxPriorityFeePerGas" | "eth_feeHistory"
        ) && !contains_transaction_field(&call.params)
    }));
    for route in ["tor", "proxy", "direct"] {
        assert!(
            quote_calls
                .iter()
                .any(|call| call.route.as_deref() == Some(route))
        );
    }
    assert!(rpc_calls_for(&quote_calls, "eth_estimateGas").is_empty());

    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let custom_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    let http = http_context_for_route(WalletNetworkMode::Direct, "direct");
    let pool = crate::query_rpc_pool_for_route(
        &sensitive(&RpcChainRoute::new(
            1,
            vec![Url::parse(&server.url).expect("mock RPC URL")],
        )),
        &http,
    );
    let recognized_request = TransactionRequest::default()
        .with_to(to)
        .with_value(U256::from(1_u64));
    super::submission::public_action_preflight_from_rpc_pool(
        &pool,
        &http,
        &sensitive(&chain.rpc_route),
        1,
        from,
        recognized_request,
        custom_fee,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        super::types::PublicActionGasLimitStrategy::ChainBuffer,
        None,
        None,
        Some(65_000 + chain.gas.gas_limit_buffer),
        None,
        false,
        None,
    )
    .await
    .expect("recognized operation preflight");
    assert!(rpc_calls_for(&server.calls(), "eth_estimateGas").is_empty());

    let unknown_request = TransactionRequest::default()
        .with_to(to)
        .with_value(U256::from(1_u64))
        .with_input(Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]));
    let before_submission_estimate = rpc_calls_for(&server.calls(), "eth_estimateGas").len();
    super::submission::public_action_preflight_from_rpc_pool(
        &pool,
        &http,
        &sensitive(&chain.rpc_route),
        1,
        from,
        unknown_request,
        PublicActionGasFeeSelection::Auto,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        super::types::PublicActionGasLimitStrategy::ChainBuffer,
        None,
        None,
        None,
        None,
        false,
        None,
    )
    .await
    .expect("managed post-approval resolution");
    let after_submission_calls = server.calls();
    assert_eq!(
        rpc_calls_for(&after_submission_calls, "eth_estimateGas").len(),
        before_submission_estimate + 1
    );
    assert!(
        rpc_calls_for(&after_submission_calls, "eth_estimateGas")
            .last()
            .is_some_and(|call| contains_transaction_field(&call.params))
    );

    let simulation_server = MockRpcServer::spawn(false, 42_000).await;
    let simulation_chain = effective_chain_for_rpc(&simulation_server.url, 8_000);
    let simulation_http = http_context_for_route(WalletNetworkMode::Direct, "direct");
    let simulation_request = PublicAdvancedTransactionEstimateRequest {
        chain_id: 1,
        effective_chain: simulation_chain,
        from,
        intent: PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(7_u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        },
        gas_fee: custom_fee,
        access_list: None,
    };
    let simulation_quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(80),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 2,
    };
    let simulation_resolved = PublicActionResolvedGasFee {
        rpc_gas_price: 100,
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    assert!(rpc_calls_for(&simulation_server.calls(), "eth_estimateGas").is_empty());
    let simulation = simulate_public_advanced_transaction_with_fee(
        simulation_request,
        simulation_quote,
        simulation_resolved,
        &simulation_http,
        &simulation_http.operation_http_client().await.unwrap(),
    )
    .await
    .expect("explicit simulation after action");
    assert_eq!(simulation.raw_gas_limit, 42_000);
    assert_eq!(
        rpc_calls_for(&simulation_server.calls(), "eth_estimateGas").len(),
        1
    );

    for (mode, route) in [
        (WalletNetworkMode::Tor, "tor"),
        (WalletNetworkMode::Proxy, "proxy"),
    ] {
        let failed_server = MockRpcServer::spawn(true, 50_000).await;
        let failed_chain = effective_chain_for_rpc(&failed_server.url, 0);
        let failed_http = http_context_for_route(mode, route);
        let _ = quote_public_action_gas_fee(1, &failed_chain, &failed_http).await;
        let failed_calls = failed_server.calls();
        assert!(!failed_calls.is_empty());
        assert!(
            failed_calls
                .iter()
                .all(|call| call.route.as_deref() == Some(route))
        );
        assert!(
            !failed_calls
                .iter()
                .any(|call| call.route.as_deref() == Some("direct"))
        );
    }
}

#[tokio::test]
async fn admitted_dapp_reads_preserve_fee_and_simulation_policy_and_cover_preflight() {
    let server = MockRpcServer::spawn(false, 42_000).await;
    let chain = effective_chain_for_rpc(&server.url, 8_000);
    let http = http_context_for_route(WalletNetworkMode::Direct, "native");
    let admitted_http = http_context_for_route(WalletNetworkMode::Direct, "admitted");
    let callback_calls = Arc::new(AtomicU64::new(0));
    let recorded = callback_calls.clone();
    let reads = DappRpcReadClient::new(move |endpoint, read| {
        let http = admitted_http.clone();
        recorded.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let route = RpcChainRoute::new(1, vec![endpoint.into_exposed_url()]);
            let mut results = http
                .rpc_broker()
                .submit(RpcSubmission::new(
                    RpcRoute::from(sensitive(&route)),
                    vec![read],
                    crate::RpcOrigin::dapp("test-peer", "https://example.test").unwrap(),
                ))
                .await?;
            results.remove(0).map(crate::RpcResult::into_value)
        })
    });
    let native_quote = quote_public_action_gas_fee(1, &chain, &http).await.unwrap();
    let before = server.calls().len();
    let quote = quote_public_action_gas_fee_with_reads(1, &chain, &http, Some(&reads))
        .await
        .unwrap();
    assert_eq!(quote, native_quote);
    assert_eq!(callback_calls.load(Ordering::SeqCst), 4);
    assert!(
        server.calls()[before..]
            .iter()
            .all(|call| call.route.as_deref() == Some("admitted"))
    );

    let from = Address::repeat_byte(1);
    let to = Address::repeat_byte(2);
    let gas_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    let resolved = resolve_public_action_gas_fee(
        1,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        gas_fee,
        Some(quote),
    )
    .unwrap();
    let request = || PublicAdvancedTransactionEstimateRequest {
        chain_id: 1,
        effective_chain: chain.clone(),
        from,
        intent: PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(7),
            data: Bytes::from_static(&[1, 2, 3, 4]),
        },
        gas_fee,
        access_list: Some(AccessList::default()),
    };
    let native = simulate_public_advanced_transaction_with_fee(
        request(),
        quote,
        resolved,
        &http,
        &http.operation_http_client().await.unwrap(),
    )
    .await
    .unwrap();
    let before = server.calls().len();
    let admitted = simulate_public_advanced_transaction_with_fee_and_reads(
        request(),
        quote,
        resolved,
        &http,
        &http.operation_http_client().await.unwrap(),
        Some(&reads),
    )
    .await
    .unwrap();
    assert_eq!(admitted, native);
    let calls = server.calls();
    assert_eq!(calls.len() - before, 1);
    assert_eq!(calls[before].route.as_deref(), Some("admitted"));
    assert_eq!(calls[before].params[1], json!("pending"));

    let pool = crate::query_rpc_pool_for_route(&sensitive(&chain.rpc_route), &http);
    let before = server.calls().len();
    let before_callbacks = callback_calls.load(Ordering::SeqCst);
    super::submission::public_action_preflight_from_rpc_pool_with_reads(
        &pool,
        &http,
        &sensitive(&chain.rpc_route),
        1,
        from,
        TransactionRequest::default()
            .with_to(to)
            .with_value(U256::from(7)),
        PublicActionGasFeeSelection::Auto,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        super::types::PublicActionGasLimitStrategy::ChainBuffer,
        Some(admitted.gas_limit),
        None,
        None,
        None,
        false,
        Some(&reads),
        None,
    )
    .await
    .expect("admitted post-approval preflight");
    assert_eq!(callback_calls.load(Ordering::SeqCst) - before_callbacks, 7);
    let calls = server.calls();
    let preflight = &calls[before..];
    assert_eq!(preflight.len(), 7);
    assert!(
        preflight
            .iter()
            .all(|call| call.route.as_deref() == Some("admitted"))
    );
    for method in [
        "eth_getBlockByNumber",
        "eth_gasPrice",
        "eth_maxPriorityFeePerGas",
        "eth_feeHistory",
        "eth_getTransactionCount",
        "eth_estimateGas",
        "eth_getBalance",
    ] {
        assert_eq!(rpc_calls_for(preflight, method).len(), 1);
    }
    let estimate = rpc_calls_for(preflight, "eth_estimateGas")[0];
    let tx: TransactionRequest = serde_json::from_value(estimate.params[0].clone()).unwrap();
    assert_eq!(tx.nonce, Some(7));
    assert_eq!(tx.from, Some(from));
    assert_eq!(tx.max_fee_per_gas, Some(quote.suggested_max_fee_per_gas));

    // A failed live-balance read must fail preflight even when the selected provider
    // would return enough funds over its native client.
    let fail_balance = DappRpcReadClient::new(move |endpoint, read| {
        let reads = reads.clone();
        Box::pin(async move {
            if read == RpcRead::get_balance(from) {
                Err(crate::RpcBrokerError::Transport)
            } else {
                reads.read(endpoint, read).await
            }
        })
    });
    let before_balance = rpc_calls_for(&server.calls(), "eth_getBalance").len();
    let error = super::submission::public_action_preflight_from_rpc_pool_with_reads(
        &pool,
        &http,
        &sensitive(&chain.rpc_route),
        1,
        from,
        TransactionRequest::default()
            .with_to(to)
            .with_value(U256::from(7)),
        gas_fee,
        &chain.gas,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        super::types::PublicActionGasLimitStrategy::ChainBuffer,
        None,
        None,
        None,
        None,
        false,
        Some(&fail_balance),
        None,
    )
    .await
    .err()
    .expect("live balance failure must stop preflight");
    assert!(
        matches!(error, super::submission::PublicActionPreflightError::Other(error)
        if error.downcast_ref::<crate::RpcBrokerError>() == Some(&crate::RpcBrokerError::Transport))
    );
    assert_eq!(
        rpc_calls_for(&server.calls(), "eth_getBalance").len(),
        before_balance
    );
}

#[tokio::test]
async fn admitted_dapp_read_rejections_stop_retries_without_provider_fallback() {
    use crate::RpcBrokerError;

    let server = MockRpcServer::spawn(false, 42_000).await;
    let other = MockRpcServer::spawn(false, 42_000).await;
    let mut chain = effective_chain_for_rpc(&server.url, 8_000);
    // Gateway reads verify identities in their own admission; a native probe would reach a
    // server.
    chain.rpc_route = RpcChainRoute::new(
        1,
        vec![
            Url::parse(&server.url).unwrap(),
            Url::parse(&other.url).unwrap(),
        ],
    )
    .with_identity_verification();
    let http = HttpContext::direct_for_tests();
    let pool = crate::query_rpc_pool_for_route(&sensitive(&chain.rpc_route), &http);
    let from = Address::repeat_byte(1);
    let to = Address::repeat_byte(2);
    let gas_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(80),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 2,
    };
    let resolved = resolve_public_action_gas_fee(
        1,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        gas_fee,
        Some(quote),
    )
    .unwrap();
    for rejection in [
        RpcBrokerError::OriginRejected,
        RpcBrokerError::AdmissionRejected,
        RpcBrokerError::TimeoutBeforeDispatch,
        RpcBrokerError::Shutdown,
    ] {
        let count = Arc::new(AtomicU64::new(0));
        let recorded = count.clone();
        let error = rejection.clone();
        let reads = DappRpcReadClient::new(move |_, _| {
            recorded.fetch_add(1, Ordering::SeqCst);
            let error = error.clone();
            Box::pin(async move { Err(error) })
        });
        let error = quote_public_action_gas_fee_with_reads(1, &chain, &http, Some(&reads))
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<RpcBrokerError>(), Some(&rejection));
        count.store(0, Ordering::SeqCst);
        let error = simulate_public_advanced_transaction_with_fee_and_reads(
            PublicAdvancedTransactionEstimateRequest {
                chain_id: 1,
                effective_chain: chain.clone(),
                from,
                intent: PublicTransactionIntent::Raw {
                    to: Some(to),
                    value: U256::from(1_u8),
                    data: Bytes::new(),
                },
                gas_fee,
                access_list: None,
            },
            quote,
            resolved,
            &http,
            &http.operation_http_client().await.unwrap(),
            Some(&reads),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, PublicAdvancedTransactionSimulationError::RpcRead(error) if error == rejection)
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        count.store(0, Ordering::SeqCst);
        let error = super::submission::public_action_preflight_from_rpc_pool_with_reads(
            &pool,
            &http,
            &sensitive(&chain.rpc_route),
            1,
            from,
            TransactionRequest::default().with_to(to),
            gas_fee,
            &chain.gas,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None,
            None,
            None,
            None,
            false,
            Some(&reads),
            None,
        )
        .await
        .err()
        .expect("rejected preflight");
        assert!(
            matches!(error, super::submission::PublicActionPreflightError::Other(error)
            if error.downcast_ref::<RpcBrokerError>() == Some(&rejection))
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    for rejection in [
        RpcBrokerError::Remote(crate::RpcRemoteError::from(serde_json::from_value::<alloy::serde::WithOtherFields<alloy::rpc::json_rpc::ErrorPayload<serde_json::Value>>>(serde_json::json!({
            "code": -32042, "message": "nonce rejected", "data": {"nested": [null, 5]}, "extension": true
        })).unwrap())),
        RpcBrokerError::InnerRevert(crate::RpcRevert::from_multicall(Bytes::from_static(&[1, 2, 3]))),
    ] {
        let expected = rejection.clone();
        let reads = DappRpcReadClient::new(move |_, read| {
            match read.method() {
                "eth_getBlockByNumber" => {
                    let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
                    block.header.inner.base_fee_per_gas = Some(5);
                    return Box::pin(async move { Ok(serde_json::to_value(block).unwrap()) });
                }
                "eth_gasPrice" | "eth_maxPriorityFeePerGas" => return Box::pin(async { Ok(json!("0x64")) }),
                "eth_feeHistory" => return Box::pin(async { Err(RpcBrokerError::InvalidResponse) }),
                _ => {}
            }
            assert_eq!(read, RpcRead::from_method_params("eth_getTransactionCount", serde_json::json!([from, "latest"]), 1).unwrap());
            let error = rejection.clone();
            Box::pin(async move { Err(error) })
        });
        let error = super::submission::public_action_preflight_from_rpc_pool_with_reads(
            &pool, &http, &sensitive(&chain.rpc_route), 1, from, TransactionRequest::default().with_to(to),
            gas_fee, &chain.gas, PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None, None, None,
            None, false, Some(&reads),
            None,
        ).await.err().expect("remote nonce failure");
        assert!(matches!(error, super::submission::PublicActionPreflightError::Other(error)
            if error.downcast_ref::<RpcBrokerError>() == Some(&expected)));
    }
    assert!(server.calls().is_empty());
    assert!(other.calls().is_empty());
}

#[tokio::test]
async fn invalidated_dapp_signing_requests_stop_before_spend_authorization() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let control = crate::dapp_request::DappRequestControl::new(
        tokio::time::Instant::now() + std::time::Duration::from_mins(1),
        || Ok(()),
    );
    control.invalidate(&crate::RpcBrokerError::OriginRejected);
    // Invalid vault credentials distinguish request rejection from signer construction.
    let personal = walletconnect_sign_personal_message(
        WalletConnectPersonalSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: Some(control.clone()),
            view_session: view_session.clone(),
            vault_store: store.clone(),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new("wrong password".into()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: "missing".into(),
            message: b"hello".to_vec(),
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    )
    .await
    .unwrap_err();
    let typed = walletconnect_sign_typed_data_v4(
        WalletConnectTypedDataSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: Some(control),
            view_session: view_session.clone(),
            vault_store: store.clone(),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new("wrong password".into()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: "missing".into(),
            typed_data: Value::Null,
            hash_fallback_confirmed: false,
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    )
    .await
    .unwrap_err();
    for error in [personal, typed] {
        assert_eq!(
            error.downcast_ref::<crate::RpcBrokerError>(),
            Some(&crate::RpcBrokerError::OriginRejected)
        );
    }
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[tokio::test]
async fn dapp_send_invalidation_stops_before_baseline_or_raw_broadcast() {
    #[derive(Clone, Copy)]
    enum InvalidateAt {
        Entry,
        Preflight,
        Signed,
    }

    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("Dapp send signer"),
            false,
        )
        .expect("import public account");
    for phase in [
        InvalidateAt::Entry,
        InvalidateAt::Preflight,
        InvalidateAt::Signed,
    ] {
        let server = MockRpcServer::spawn(false, 42_000).await;
        let chain = effective_chain_for_rpc(&server.url, 8_000);
        let http = HttpContext::direct_for_tests();
        let control = crate::dapp_request::DappRequestControl::new(
            tokio::time::Instant::now() + std::time::Duration::from_mins(1),
            || Ok(()),
        );
        if matches!(phase, InvalidateAt::Entry) {
            control.invalidate(&crate::RpcBrokerError::OriginRejected);
        }
        let read_control = control.clone();
        let read_http = http.clone();
        let reads = DappRpcReadClient::new(move |endpoint, read| {
            let http = read_http.clone();
            let control = read_control.clone();
            Box::pin(async move {
                let route = RpcChainRoute::new(1, vec![endpoint.into_exposed_url()]);
                let mut results = http
                    .rpc_broker()
                    .submit(RpcSubmission::new(
                        RpcRoute::from(sensitive(&route)),
                        vec![read],
                        crate::RpcOrigin::dapp("test-peer", "https://example.test").unwrap(),
                    ))
                    .await?;
                let result = results.remove(0).map(crate::RpcResult::into_value);
                if matches!(phase, InvalidateAt::Preflight) {
                    control.invalidate(&crate::RpcBrokerError::OriginRejected);
                }
                result
            })
        });
        let (tracker, tracking) = super::transaction_tracker::test_tracking_context();
        let changes = tracker.subscribe();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let invalidation = if matches!(phase, InvalidateAt::Signed) {
            let control = control.clone();
            Some(tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    if matches!(event, PublicActionSessionEvent::AttemptHandoff { .. }) {
                        // The software signer runs before the broadcast checkpoint yields.
                        control.invalidate(&crate::RpcBrokerError::OriginRejected);
                        return;
                    }
                }
                panic!("send must reach signing before invalidation");
            }))
        } else {
            None
        };
        let error = submit_walletconnect_send_transaction(
            WalletConnectSendTransactionRequest {
                executor_owner: None,
                request_control: Some(control.clone()),
                rpc_reads: Some(reads),
                transaction_tracking: Some(tracking),
                chain_id: 1,
                effective_chain: chain,
                view_session: view_session.clone(),
                vault_store: store.clone(),
                authorization: Some(crate::DesktopPrivateSpendAuthorization::VaultPassword(
                    Zeroizing::new(
                        if matches!(phase, InvalidateAt::Entry) {
                            "wrong password"
                        } else {
                            TEST_PASSWORD
                        }
                        .into(),
                    ),
                )),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid.clone(),
                dapp_name: "Dapp".to_owned(),
                tx_req: TransactionRequest::default()
                    .with_to(Address::repeat_byte(2))
                    .with_value(U256::from(7)),
                decoded_transaction: None,
                reviewed_transaction: None,
                reviewed_fee: None,
                gas_fee: PublicActionGasFeeSelection::Auto,
                expiry_timestamp: None,
                event_tx: Some(event_tx),
            },
            &http,
            &http.operation_http_client().await.unwrap(),
        )
        .await
        .unwrap_err();
        if let Some(invalidation) = invalidation {
            invalidation.await.expect("invalidation task");
        }
        assert_eq!(
            error.downcast_ref::<crate::RpcBrokerError>(),
            Some(&crate::RpcBrokerError::OriginRejected)
        );
        assert!(!control.handed_off());
        assert!(
            !changes.has_changed().expect("tracker remains open"),
            "no pending transaction was registered"
        );
        let calls = server.calls();
        assert!(rpc_calls_for(&calls, "eth_sendRawTransaction").is_empty());
        match phase {
            InvalidateAt::Entry => assert!(calls.is_empty()),
            InvalidateAt::Preflight => {
                assert!(!rpc_calls_for(&calls, "eth_getBalance").is_empty());
                assert!(rpc_calls_for(&calls, "eth_blockNumber").is_empty());
            }
            InvalidateAt::Signed => assert_eq!(rpc_calls_for(&calls, "eth_blockNumber").len(), 1),
        }
        tracker.shutdown().await;
    }
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[tokio::test]
async fn dapp_raw_broadcast_does_not_log_or_return_remote_payloads() {
    use tracing::instrument::WithSubscriber;

    let server = MockRpcServer::spawn(true, 21_000).await;
    let http = HttpContext::direct_for_tests();
    let pool = crate::query_rpc_pool_for_route(
        &sensitive(&RpcChainRoute::new(
            1,
            vec![Url::parse(&server.url).unwrap()],
        )),
        &http,
    );
    let tx_hash = B256::repeat_byte(0xab);
    for log_transaction_details in [true, false] {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let error = crate::self_broadcast_send_raw_transaction_to_rpc_pool_with_logging(
            &pool,
            WalletNetworkMode::Direct,
            vec![1, 2, 3],
            tx_hash,
            log_transaction_details,
        )
        .with_subscriber(subscriber)
        .await
        .err()
        .expect("RPC rejects broadcast");
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        if log_transaction_details {
            assert!(logs.contains("mock route failure"));
            assert!(logs.contains(&alloy::hex::encode_prefixed(tx_hash)));
            assert!(error.to_string().contains("mock route failure"));
        } else {
            assert!(!logs.contains("mock route failure"));
            assert!(!logs.contains(&alloy::hex::encode_prefixed(tx_hash)));
            assert!(!error.to_string().contains("mock route failure"));
        }
    }
}
