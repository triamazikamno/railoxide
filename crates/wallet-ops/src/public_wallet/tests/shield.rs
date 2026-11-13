use super::*;

#[tokio::test]
async fn public_shield_allowance_uses_shared_rpc_client_and_decodes_result() {
    let server = MockRpcServer::spawn(false, 50_000).await;
    let chain_route = RpcChainRoute::new(1, vec![Url::parse(&server.url).expect("mock RPC URL")]);
    let http = http_context_for_route(WalletNetworkMode::Tor, "tor");
    let token = address!("1111111111111111111111111111111111111111");
    let owner = address!("2222222222222222222222222222222222222222");
    let spender = address!("3333333333333333333333333333333333333333");

    let allowance = super::actions::query_erc20_allowance(
        &sensitive(&chain_route),
        &http,
        PublicAssetId::Erc20(token),
        owner,
        spender,
    )
    .await
    .expect("query allowance through broker");
    assert_eq!(allowance, U256::from(0x1234));

    let calls = server.calls();
    let allowance_calls = rpc_calls_for(&calls, "eth_call");
    assert_eq!(allowance_calls.len(), 1);
    let call = allowance_calls[0];
    assert_eq!(call.route.as_deref(), Some("tor"));
    let transaction: TransactionRequest =
        serde_json::from_value(call.params[0].clone()).expect("allowance transaction");
    assert_eq!(transaction.to, Some(TxKind::Call(token)));
    let calldata = transaction.input.input().expect("allowance calldata");
    let decoded = PublicErc20::allowanceCall::abi_decode(calldata).expect("decode allowance call");
    assert_eq!(decoded.owner, owner);
    assert_eq!(decoded.spender, spender);
}

#[tokio::test]
async fn public_shield_allowance_propagates_rpc_member_error() {
    let server = MockRpcServer::spawn(true, 50_000).await;
    let chain_route = RpcChainRoute::new(1, vec![Url::parse(&server.url).expect("mock RPC URL")]);
    let http = http_context_for_route(WalletNetworkMode::Tor, "tor");
    let error = super::actions::query_erc20_allowance(
        &sensitive(&chain_route),
        &http,
        PublicAssetId::Erc20(address!("1111111111111111111111111111111111111111")),
        address!("2222222222222222222222222222222222222222"),
        address!("3333333333333333333333333333333333333333"),
    )
    .await
    .expect_err("failed allowance read must not produce an approval decision");

    assert_eq!(error.to_string(), "query public shield ERC-20 allowance");
    assert!(matches!(
        error.downcast_ref::<crate::RpcBrokerError>(),
        Some(crate::RpcBrokerError::Remote(_))
    ));
    assert!(!rpc_calls_for(&server.calls(), "eth_call").is_empty());
}

#[test]
fn public_shield_approval_follows_mode_and_allowance() {
    use PublicShieldApprovalMode::{Exact, UnlimitedWhenShort};

    let amount = U256::from(10_u64);
    for (mode, allowance, required, approval_amount) in [
        // An exact approval is signed for every shield, whatever the allowance.
        (Exact, U256::MAX, true, amount),
        (Exact, U256::from(9_u64), true, amount),
        (UnlimitedWhenShort, amount, false, U256::MAX),
        (UnlimitedWhenShort, U256::from(9_u64), true, U256::MAX),
    ] {
        assert_eq!(
            public_shield_approval_required(mode, allowance, amount),
            required,
            "{mode:?} with allowance {allowance}"
        );
        assert_eq!(public_shield_approval_amount(mode, amount), approval_amount);
    }
    // Mimic Railway locks Railway's policy; Railoxide keeps the chosen mode.
    for chosen in [Exact, UnlimitedWhenShort] {
        assert_eq!(
            PublicShieldTransactionProfile::Railway.approval_mode(chosen),
            UnlimitedWhenShort
        );
        assert_eq!(
            PublicShieldTransactionProfile::Railoxide.approval_mode(chosen),
            chosen
        );
    }
}

#[test]
fn public_shield_protocol_fee_amount_uses_floor_rounding() {
    assert_eq!(
        public_shield_protocol_fee_amount(U256::from(12_345_u64)),
        U256::from(30_u64)
    );
}

#[tokio::test]
async fn shield_submission_checks_remaining_funding_before_estimation_and_signing() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .list_active_public_accounts_for_session(&view_session)
        .unwrap()
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .unwrap();
    let native_balance = 1_000_000_000_000_000_000_000_u128;
    let token = PublicAssetId::Erc20(Address::repeat_byte(7));
    for (case, asset, profile) in [
        ("approval", token, PublicShieldTransactionProfile::Railoxide),
        ("approval", token, PublicShieldTransactionProfile::Railway),
        ("allowance", token, PublicShieldTransactionProfile::Railway),
        (
            "auto_ceiling",
            token,
            PublicShieldTransactionProfile::Railway,
        ),
        (
            "native",
            PublicAssetId::Native,
            PublicShieldTransactionProfile::Railoxide,
        ),
        ("retry", token, PublicShieldTransactionProfile::Railoxide),
        (
            "estimate_reserve",
            token,
            PublicShieldTransactionProfile::Railoxide,
        ),
    ] {
        let mut chain = default_effective_chain(42161);
        let (_, approval_gas, shield_gas) = super::gas::public_shield_estimated_gas_units(
            railgun_wallet::tx::RailgunGasModel::for_chain(42161),
            asset,
            profile,
            chain.gas.gas_limit_buffer,
        )
        .unwrap();
        let estimate_gas = approval_gas.saturating_add(shield_gas.saturating_mul(2));
        let estimate = if case == "estimate_reserve" {
            MockRpcEstimateResult::Success(estimate_gas)
        } else {
            MockRpcEstimateResult::Error {
                code: -32000,
                message: "estimation reached".to_owned(),
                data: None,
            }
        };
        let server = MockRpcServer::spawn_with_estimate_result(false, estimate).await;
        chain.rpc_route = RpcChainRoute::new(42161, vec![Url::parse(&server.url).unwrap()]);
        let max_fee = match case {
            "approval" => native_balance / u128::from(approval_gas),
            "allowance" => native_balance / u128::from(shield_gas + approval_gas / 2),
            "estimate_reserve" => {
                native_balance
                    / u128::from(estimate_gas + chain.gas.gas_limit_buffer + shield_gas / 2)
            }
            _ => 1,
        };
        let fee = |max_fee_per_gas| PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas: 1,
        };
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
        if case == "retry" {
            command_tx
                .send(PublicActionCommand {
                    kind: PublicActionCommandKind::Retry,
                    gas_fee: fee(native_balance / u128::from(approval_gas)),
                })
                .unwrap();
        }
        drop(command_tx);
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let http = HttpContext::direct_for_tests();
        let error = submit_public_shield(
            PublicShieldRequest {
                executor_owner: None,
                transaction_tracking: None,
                chain_id: 42161,
                effective_chain: chain,
                view_session: view_session.clone(),
                vault_store: store.clone(),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid.clone(),
                asset,
                amount: match case {
                    "native" => U256::from(native_balance),
                    "allowance" => U256::from(1),
                    _ => U256::from(5_000),
                },
                profile,
                approval_mode: PublicShieldApprovalMode::Exact,
                native_preparation: None,
                native_operation: None,
                gateway_origin: None,
                gas_fee: fee(max_fee),
                gas_fee_mode: if case == "auto_ceiling" {
                    PublicActionGasFeeMode::Auto
                } else {
                    PublicActionGasFeeMode::Custom
                },
                authorized_fee_ceiling: fee(if case == "auto_ceiling" {
                    native_balance / u128::from(shield_gas) + 1
                } else {
                    max_fee
                }),
                command_rx: Some(command_rx),
                event_tx: Some(event_tx),
            },
            &http,
            &http.operation_http_client().await.unwrap(),
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        if case == "allowance" {
            assert!(message.contains("estimation reached"), "{message}");
            assert!(!message.contains("insufficient native balance"));
        } else {
            assert!(
                message.contains("insufficient native balance"),
                "{case}: {message}"
            );
            if case != "estimate_reserve" {
                assert!(message.contains("estimated Shield fees"), "{message}");
            }
        }
        let calls = server.calls();
        assert_eq!(
            rpc_calls_for(&calls, "eth_estimateGas").len(),
            usize::from(matches!(case, "allowance" | "retry" | "estimate_reserve")),
            "{case}",
        );
        assert_eq!(
            rpc_calls_for(&calls, "eth_getBalance").len(),
            if case == "retry" { 2 } else { 1 },
            "{case}",
        );
        assert!(rpc_calls_for(&calls, "eth_sendRawTransaction").is_empty());
        let expected_step = match (asset, case) {
            // The wrap fallback's value is the whole balance, which leaves no gas to wrap it.
            (PublicAssetId::Native, _) => PublicActionProgressStep::Wrap,
            (_, "allowance") => PublicActionProgressStep::Shield,
            _ => PublicActionProgressStep::Approve,
        };
        let mut failures = 0;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                PublicActionSessionEvent::StepFailed { step, .. } => {
                    assert_eq!(step, expected_step);
                    failures += 1;
                }
                PublicActionSessionEvent::AttemptHandoff { .. }
                | PublicActionSessionEvent::AttemptSubmitted { .. } => {
                    panic!("{case}: an unaffordable Shield must not reach transaction signing");
                }
                _ => {}
            }
        }
        assert_eq!(failures, if case == "retry" { 2 } else { 1 });
    }
    drop(view_session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn shield_allowance_read_failure_waits_for_retry_and_adopts_its_fee() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .list_active_public_accounts_for_session(&view_session)
        .unwrap()
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .unwrap();
    let server = MockRpcServer::spawn_with_estimate_result(
        false,
        MockRpcEstimateResult::Error {
            code: -32000,
            message: "estimation reached".to_owned(),
            data: None,
        },
    )
    .await;
    server.fail_eth_call.store(true, Ordering::SeqCst);
    let mut chain = default_effective_chain(42161);
    chain.rpc_route = RpcChainRoute::new(42161, vec![Url::parse(&server.url).unwrap()]);
    let fee = |max_fee_per_gas| PublicActionGasFeeSelection::Custom {
        max_fee_per_gas,
        max_priority_fee_per_gas: 1,
    };
    // The request's fee reaches Shield estimation; the retry's fee cannot be funded.
    let retry_fee = fee(1_000_000_000_000_000_000_000);
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let http = HttpContext::direct_for_tests();
    let client = http.operation_http_client().await.unwrap();
    let submit = submit_public_shield(
        PublicShieldRequest {
            executor_owner: None,
            transaction_tracking: None,
            chain_id: 42161,
            effective_chain: chain,
            view_session: view_session.clone(),
            vault_store: store.clone(),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.to_owned()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: account.public_account_uuid.clone(),
            asset: PublicAssetId::Erc20(Address::repeat_byte(7)),
            // Below the mock allowance, so no approval is needed after the retried read.
            amount: U256::from(1),
            profile: PublicShieldTransactionProfile::Railoxide,
            approval_mode: PublicShieldApprovalMode::UnlimitedWhenShort,
            native_preparation: None,
            native_operation: None,
            gateway_origin: None,
            gas_fee: fee(1),
            gas_fee_mode: PublicActionGasFeeMode::Custom,
            authorized_fee_ceiling: fee(1),
            command_rx: Some(command_rx),
            event_tx: Some(event_tx),
        },
        &http,
        &client,
    );
    let retry = async {
        loop {
            match event_rx.recv().await.expect("allowance failure event") {
                PublicActionSessionEvent::StepFailed { step, .. } => {
                    assert_eq!(step, PublicActionProgressStep::Approve);
                    break;
                }
                PublicActionSessionEvent::AttemptHandoff { .. }
                | PublicActionSessionEvent::AttemptSubmitted { .. } => {
                    panic!("a failed allowance read must not reach transaction signing");
                }
                _ => {}
            }
        }
        server.fail_eth_call.store(false, Ordering::SeqCst);
        let failed_reads = rpc_calls_for(&server.calls(), "eth_call").len();
        command_tx
            .send(PublicActionCommand {
                kind: PublicActionCommandKind::Retry,
                gas_fee: retry_fee,
            })
            .unwrap();
        drop(command_tx);
        failed_reads
    };
    let (result, failed_reads) = tokio::join!(submit, retry);

    let message = format!("{:#}", result.unwrap_err());
    assert!(message.contains("insufficient native balance"), "{message}");
    let calls = server.calls();
    assert!(failed_reads > 0);
    assert!(rpc_calls_for(&calls, "eth_call").len() > failed_reads);
    assert!(rpc_calls_for(&calls, "eth_estimateGas").is_empty());
    assert!(rpc_calls_for(&calls, "eth_sendRawTransaction").is_empty());
    let mut failed_steps = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let PublicActionSessionEvent::StepFailed { step, .. } = event {
            failed_steps.push(step);
        }
    }
    assert_eq!(failed_steps, [PublicActionProgressStep::Shield]);
    drop(view_session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn shield_speedup_funding_uses_the_previously_signed_gas_limit() {
    let server = MockRpcServer::spawn(false, 50_000).await;
    let mut chain = default_effective_chain(42161);
    chain.rpc_route = RpcChainRoute::new(42161, vec![Url::parse(&server.url).unwrap()]);
    let (_, _, model_gas) = super::gas::public_shield_estimated_gas_units(
        railgun_wallet::tx::RailgunGasModel::for_chain(42161),
        PublicAssetId::Native,
        PublicShieldTransactionProfile::Railoxide,
        chain.gas.gas_limit_buffer,
    )
    .unwrap();
    let signed_gas_limit = model_gas / 2;
    let native_balance = U256::from(1_000_000_000_000_000_000_000_u128);
    let value = native_balance - U256::from(model_gas) * U256::from(100);
    let http = HttpContext::direct_for_tests();
    let pool = crate::query_rpc_pool_for_route(&sensitive(&chain.rpc_route), &http);
    for (max_fee_per_gas, affordable) in [(125_u128, true), (300, false)] {
        let actual_cost = value + U256::from(signed_gas_limit) * U256::from(max_fee_per_gas);
        assert_eq!(actual_cost <= native_balance, affordable);
        assert!(value + U256::from(model_gas) * U256::from(max_fee_per_gas) > native_balance);
        let result = super::submission::public_action_preflight_from_rpc_pool(
            &pool,
            &http,
            &sensitive(&chain.rpc_route),
            42161,
            Address::repeat_byte(1),
            TransactionRequest::default()
                .with_to(Address::repeat_byte(2))
                .with_value(value),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas: 1,
            },
            &chain.gas,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None,
            Some(7),
            Some(signed_gas_limit),
            None,
            false,
            Some(super::submission::PublicShieldFundingBudget {
                gas_units: model_gas,
                remaining_shield_gas_units: 0,
                remaining_max_fee_per_gas: None,
            }),
        )
        .await;
        if affordable {
            assert!(
                result.is_ok(),
                "affordable replacement failed: {:?}",
                result.err()
            );
        } else {
            assert!(matches!(
                result,
                Err(super::submission::PublicActionPreflightError::InsufficientShieldFunding(_))
            ));
        }
    }
    let calls = server.calls();
    assert!(rpc_calls_for(&calls, "eth_estimateGas").is_empty());
    assert!(rpc_calls_for(&calls, "eth_getTransactionCount").is_empty());
}
