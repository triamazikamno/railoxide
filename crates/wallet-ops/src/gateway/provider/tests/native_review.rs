use super::*;

fn operation_slot() -> crate::OperationHttpClientSlot {
    crate::OperationHttpClientSlot::for_tests(HttpContext::direct_for_tests(), operation_client())
}

fn approval_handle(
    provider: &DappProvider,
) -> (
    crate::gateway::GatewayHandle,
    tokio::sync::mpsc::Receiver<crate::gateway::Command>,
) {
    let (commands, receiver) = tokio::sync::mpsc::channel(32);
    let (_, snapshots) = tokio::sync::watch::channel(crate::gateway::GatewaySnapshot {
        config: crate::gateway::GatewayConfig::default(),
        listener_addr: None,
        peers: Vec::new(),
        pairing_active: false,
        locked: false,
        generation: 0,
        error: None,
        permissions: Vec::new(),
    });
    (
        crate::gateway::GatewayHandle {
            commands,
            ui_events: tokio::sync::broadcast::channel(32).0,
            snapshots,
            approvals: provider.approval_updates.subscribe(),
            wallet_switches: provider.switch_updates.subscribe(),
            authority: provider.authority_fallback.as_ref().unwrap().clone(),
        },
        receiver,
    )
}

#[tokio::test]
async fn native_review_fee_fanout_succeeds_with_full_dapp_queue() {
    use crate::gateway::Command;
    let (path, mut provider, view) = fixture();
    let responses = BTreeMap::from([
        (
            "eth_feeHistory",
            json!({"oldestBlock": "0x1", "baseFeePerGas": ["0x1", "0x2"], "gasUsedRatio": [0.5], "reward": [["0x1"]]}),
        ),
        ("eth_gasPrice", json!("0x6")),
        ("eth_maxPriorityFeePerGas", json!("0x7")),
    ]);
    let mut endpoints = Vec::new();
    let mut servers = Vec::new();
    for _ in 0..23 {
        let (endpoint, _, release, server) = held_responses(false, responses.clone()).await;
        release.notify_one();
        endpoints.push(endpoint);
        servers.push(server);
    }
    authorize(&mut provider, &view, endpoints[0].clone());
    let mut wallet = provider.wallet.clone();
    wallet
        .routes
        .insert(1, RpcChainRoute::new(1, endpoints.clone()));
    provider.update_wallet(wallet, 3);
    personal_approval(&mut provider, "sign", Instant::now());
    let ready = provider.approval_updates.borrow()[0].clone();
    let now = Instant::now();
    // Fill active slots and the entire queue, allowing the rate bucket to refill.
    for _ in 0..32 {
        assert!(matches!(
            provider
                .admission
                .admit(ready.origin.clone(), now - Duration::from_secs(2)),
            Ok(ReadAdmissionDecision::Ready(_))
        ));
    }
    for entered in [now - Duration::from_secs(2), now - Duration::from_secs(1)] {
        for _ in 0..32 {
            assert!(matches!(
                provider.admission.admit(ready.origin.clone(), entered),
                Ok(ReadAdmissionDecision::Queued(_))
            ));
        }
    }
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "doc",
            "excess",
            "eth_blockNumber",
            now
        ))["error"]["code"],
        -32005
    );
    let (handle, mut commands) = approval_handle(&provider);
    let operation = operation_slot();
    let mut callers = tokio::task::JoinSet::new();
    for endpoint in endpoints {
        for (&method, expected) in &responses {
            let reads = handle.approval_reads(ready.id.clone(), operation.clone());
            let endpoint = endpoint.clone();
            let expected = expected.clone();
            let params = if method == "eth_feeHistory" {
                json!(["0x1", "latest", [50.0]])
            } else {
                json!([])
            };
            callers.spawn(async move {
                let result = reads
                    .read(
                        endpoint.into(),
                        RpcRead::from_method_params(method, params, 1).unwrap(),
                    )
                    .await;
                (result, expected)
            });
        }
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while !callers.is_empty() {
            tokio::select! {
                command = commands.recv() => {
                    let Command::ApprovalRead(id, endpoint, rpc, entered, client, reply) = command.unwrap() else {
                        panic!("expected approval read");
                    };
                    provider.approval_read(&id, endpoint, rpc, entered, client, reply);
                }
                completion = provider.jobs.join_next(), if !provider.jobs.is_empty() => {
                    provider.complete_read(completion.unwrap().unwrap());
                }
                result = callers.join_next() => {
                    let (actual, expected) = result.unwrap().unwrap();
                    assert_eq!(actual, Ok(expected), "wallet fee reads must bypass the full dapp queue");
                }
            }
        }
        for server in servers {
            server.await.unwrap();
        }
    }).await.unwrap();
    assert!(provider.reads.is_empty());
    assert_eq!(
        result(&request(
            &mut provider,
            1,
            "doc",
            "still-excess",
            "eth_blockNumber",
            Instant::now()
        ))["error"]["code"],
        -32005
    );
    drop(handle);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

/// A browser dapp's transaction request: its simulation and preflight read through the real
/// approval callback, then it is broadcast and observed, all over the request's own client.
#[tokio::test]
async fn native_transaction_request_reaches_the_chain_only_over_its_client() {
    use crate::gateway::Command;
    use crate::rpc_broker::tests::{RpcResponder, spawn_recording_rpc_mock, tagged_reqwest_client};
    use alloy::network::TransactionBuilder as _;
    use alloy::primitives::{Address, Bytes, U256};

    let responder: RpcResponder = Arc::new(|request: Value| {
        let result = match request["method"].as_str().unwrap_or_default() {
            "eth_sendRawTransaction" => {
                let raw = alloy::hex::decode(request["params"][0].as_str().unwrap()).unwrap();
                json!(alloy::primitives::keccak256(raw))
            }
            "eth_getBlockByNumber" => {
                let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
                block.header.inner.base_fee_per_gas = Some(5);
                serde_json::to_value(block).unwrap()
            }
            "eth_feeHistory" => json!({
                "oldestBlock": "0x1",
                "baseFeePerGas": ["0x5", "0x5"],
                "gasUsedRatio": [0.5],
                "reward": [["0x1", "0x2", "0x3", "0x4"]]
            }),
            "eth_estimateGas" => json!("0x5208"),
            "eth_getBalance" => json!("0x3635c9adc5dea00000"),
            _ => json!("0x1"),
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    });
    let (reader, reader_requests, reader_server) =
        spawn_recording_rpc_mock(responder.clone()).await;
    let (submitter, submitter_requests, submitter_server) =
        spawn_recording_rpc_mock(responder).await;
    let roles = |sensitive, submit| crate::RpcEndpointRoles {
        general: false,
        sensitive,
        submit,
    };
    let route = RpcChainRoute::new_with_transports(
        1,
        vec![
            (
                reader.into(),
                crate::RpcTransport::Wallet,
                roles(true, false),
            ),
            (
                submitter.into(),
                crate::RpcTransport::Wallet,
                roles(false, true),
            ),
        ],
    );
    let http = HttpContext::with_rpc_client_for_tests(
        tagged_reqwest_client("shared"),
        crate::WalletNetworkMode::Direct,
    );
    let operation = crate::OperationHttpClientSlot::for_tests(
        http.clone(),
        crate::OperationHttpClient::for_tests(
            tagged_reqwest_client("request"),
            crate::OperationNetworkIsolation::Dedicated,
        ),
    );
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    chain.rpc_route = route.clone();

    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let mut wallet = provider.wallet.clone();
    wallet.http = Some(http.clone());
    wallet.routes.insert(1, route);
    provider.update_wallet(wallet, 3);
    let account = provider
        .resolve(&provider.documents[&(1, "doc".to_owned())].origin)
        .unwrap()
        .1;
    let to = Address::repeat_byte(2);
    provider
        .request(
            1,
            "doc".to_owned(),
            "send".to_owned(),
            "eth_sendTransaction",
            json!([{"from": account.address, "to": to, "value": "0x7"}]),
            Instant::now(),
        )
        .unwrap();
    let ready = provider.approval_updates.borrow()[0].clone();
    let (handle, mut commands) = approval_handle(&provider);
    let reads = handle.approval_reads(ready.id.clone(), operation.clone());
    let client = operation.client().await.unwrap();
    let (tracker, tracking) = crate::public_wallet::test_tracking_context();
    let gas_fee = crate::PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    let quote = crate::PublicActionGasFeeQuote {
        observed_fee_model: Some(crate::EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(5),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 2,
    };
    let resolved = crate::resolve_public_action_gas_fee(
        1,
        crate::PublicShieldTransactionProfile::Railoxide,
        crate::PublicAssetId::Native,
        gas_fee,
        Some(quote),
    )
    .unwrap();
    let vault_store = Arc::new(DesktopVaultStore::from_db(provider.store.db()));
    let mut review_and_submit = Box::pin(async {
        crate::simulate_public_advanced_transaction_with_fee_and_reads(
            crate::PublicAdvancedTransactionEstimateRequest {
                chain_id: 1,
                effective_chain: chain.clone(),
                from: account.address,
                intent: crate::PublicTransactionIntent::Raw {
                    to: Some(to),
                    value: U256::from(7),
                    data: Bytes::new(),
                },
                gas_fee,
                access_list: None,
            },
            quote,
            resolved,
            &http,
            &client,
            Some(&reads),
        )
        .await
        .expect("the request is simulated");
        crate::submit_walletconnect_send_transaction(
            crate::WalletConnectSendTransactionRequest {
                executor_owner: None,
                request_control: Some(ready.control.clone()),
                rpc_reads: Some(reads),
                transaction_tracking: Some(tracking),
                chain_id: 1,
                effective_chain: chain,
                view_session: Arc::clone(&view),
                vault_store,
                authorization: Some(crate::DesktopPrivateSpendAuthorization::VaultPassword(
                    zeroize::Zeroizing::new(PASSWORD.to_owned()),
                )),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid.clone(),
                dapp_name: "Dapp".to_owned(),
                tx_req: alloy::rpc::types::TransactionRequest::default()
                    .with_to(to)
                    .with_value(U256::from(7)),
                decoded_transaction: None,
                reviewed_transaction: None,
                reviewed_fee: None,
                gas_fee,
                expiry_timestamp: None,
                event_tx: None,
            },
            &http,
            &client,
        )
        .await
    });
    // The gateway actor's side: each approval read the request sends runs on the provider.
    let mut approval_reads = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                result = &mut review_and_submit => break result,
                command = commands.recv() => {
                    let Command::ApprovalRead(id, endpoint, rpc, entered, client, reply) = command.unwrap() else {
                        panic!("expected approval read");
                    };
                    approval_reads += 1;
                    provider.approval_read(&id, endpoint, rpc, entered, client, reply);
                }
                completion = provider.jobs.join_next(), if !provider.jobs.is_empty() => {
                    provider.complete_read(completion.unwrap().unwrap());
                }
            }
        }
    })
    .await
    .expect("the request ends")
    .expect("the request is submitted");
    drop(review_and_submit);
    assert!(approval_reads > 0);

    // The tracker keeps observing the handed-off transaction.
    let handed_off_at = reader_requests.lock().unwrap().len();
    tokio::time::timeout(Duration::from_secs(10), async {
        while reader_requests.lock().unwrap().len() == handed_off_at {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the tracker polls the handed-off observer");

    let reader_requests = reader_requests.lock().unwrap().clone();
    let submitter_requests = submitter_requests.lock().unwrap().clone();
    assert!(
        reader_requests
            .iter()
            .chain(&submitter_requests)
            .all(|(tag, _)| tag.as_deref() == Some("request")),
        "every request goes over the request's client"
    );
    let methods = |requests: &[(Option<String>, Value)]| {
        requests
            .iter()
            .map(|(_, body)| body["method"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
    };
    let read_methods = methods(&reader_requests);
    for method in [
        "eth_estimateGas",
        "eth_getTransactionCount",
        "eth_getBalance",
        "eth_blockNumber",
    ] {
        assert!(read_methods.iter().any(|read| read == method), "{method}");
    }
    assert!(
        !read_methods
            .iter()
            .any(|read| read == "eth_sendRawTransaction")
    );
    assert_eq!(methods(&submitter_requests), ["eth_sendRawTransaction"]);
    tracker.shutdown().await;
    reader_server.abort();
    submitter_server.abort();
    drop(handle);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn native_review_read_waits_for_command_capacity_and_delivers_once() {
    use crate::gateway::Command;
    let (path, mut provider, view) = fixture();
    let endpoint = url::Url::parse("http://127.0.0.1:1").unwrap();
    authorize(&mut provider, &view, endpoint.clone());
    personal_approval(&mut provider, "sign", Instant::now());
    let ready = provider.approval_updates.borrow()[0].clone();
    let (handle, mut commands) = approval_handle(&provider);
    for _ in 0..32 {
        handle
            .commands
            .try_send(Command::Pair(false, oneshot::channel().0))
            .ok()
            .unwrap();
    }
    let reads = handle.approval_reads(ready.id.clone(), operation_slot());
    let mut read = Box::pin(reads.read(
        endpoint.into(),
        RpcRead::from_method_params("eth_blockNumber", json!([]), 1).unwrap(),
    ));
    assert!(futures_util::poll!(&mut read).is_pending());
    assert!(matches!(commands.try_recv().unwrap(), Command::Pair(..)));
    assert!(futures_util::poll!(&mut read).is_pending());
    for _ in 0..31 {
        assert!(matches!(commands.try_recv().unwrap(), Command::Pair(..)));
    }
    let Command::ApprovalRead(_, _, _, _, _, reply) = commands.try_recv().unwrap() else {
        panic!("expected the waiting approval read");
    };
    reply.send(Ok(json!("0xfeed"))).unwrap();
    assert_eq!(read.await, Ok(json!("0xfeed")));
    assert!(commands.try_recv().is_err(), "read must only enqueue once");
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test(start_paused = true)]
async fn native_review_read_cancellation_expiry_and_drop_prevent_late_enqueue() {
    use crate::gateway::Command;
    for stop in ["cancelled", "expired", "dropped"] {
        let (path, mut provider, view) = fixture();
        let endpoint = url::Url::parse("http://127.0.0.1:1").unwrap();
        authorize(&mut provider, &view, endpoint.clone());
        personal_approval(&mut provider, "sign", Instant::now());
        let ready = provider.approval_updates.borrow()[0].clone();
        if stop == "expired" {
            tokio::time::advance(
                (ready.deadline - Instant::now())
                    .checked_sub(Duration::from_secs(1))
                    .unwrap(),
            )
            .await;
        }
        let (handle, mut commands) = approval_handle(&provider);
        for _ in 0..32 {
            handle
                .commands
                .try_send(Command::Pair(false, oneshot::channel().0))
                .ok()
                .unwrap();
        }
        let reads = handle.approval_reads(ready.id.clone(), operation_slot());
        let mut read = Box::pin(reads.read(
            endpoint.into(),
            RpcRead::from_method_params("eth_blockNumber", json!([]), 1).unwrap(),
        ));
        assert!(futures_util::poll!(&mut read).is_pending());
        let read = match stop {
            "cancelled" => {
                ready.control.invalidate(&RpcBrokerError::OriginRejected);
                Some((read, RpcBrokerError::OriginRejected))
            }
            "expired" => {
                tokio::time::advance(Duration::from_secs(1)).await;
                Some((read, RpcBrokerError::Timeout))
            }
            "dropped" => {
                drop(read);
                None
            }
            _ => unreachable!(),
        };
        // Capacity becomes available before the cancelled waiter is polled again.
        for _ in 0..32 {
            assert!(matches!(commands.try_recv().unwrap(), Command::Pair(..)));
        }
        if let Some((read, expected)) = read {
            assert_eq!(read.await, Err(expected));
        }
        tokio::task::yield_now().await;
        assert!(
            commands.try_recv().is_err(),
            "stopped reads must not enqueue"
        );
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn native_review_read_queue_and_response_wait_share_original_deadline() {
    use crate::gateway::{Command, admission::READ_TIMEOUT};
    for waiting_for_response in [false, true] {
        let (path, mut provider, view) = fixture();
        let endpoint = url::Url::parse("http://127.0.0.1:1").unwrap();
        authorize(&mut provider, &view, endpoint.clone());
        personal_approval(&mut provider, "sign", Instant::now());
        let ready = provider.approval_updates.borrow()[0].clone();
        let (handle, mut commands) = approval_handle(&provider);
        for _ in 0..32 {
            handle
                .commands
                .try_send(Command::Pair(false, oneshot::channel().0))
                .ok()
                .unwrap();
        }
        let reads = handle.approval_reads(ready.id.clone(), operation_slot());
        let mut read = Box::pin(reads.read(
            endpoint.into(),
            RpcRead::from_method_params("eth_blockNumber", json!([]), 1).unwrap(),
        ));
        let entered = Instant::now();
        assert!(futures_util::poll!(&mut read).is_pending());
        tokio::time::advance(READ_TIMEOUT.checked_sub(Duration::from_secs(1)).unwrap()).await;
        let reply = if waiting_for_response {
            for _ in 0..32 {
                assert!(matches!(commands.try_recv().unwrap(), Command::Pair(..)));
            }
            assert!(futures_util::poll!(&mut read).is_pending());
            let Command::ApprovalRead(_, _, _, command_entered, _, reply) =
                commands.try_recv().unwrap()
            else {
                panic!("expected the waiting approval read");
            };
            assert_eq!(command_entered, entered, "actor must retain the queue wait");
            Some(reply)
        } else {
            None
        };
        tokio::time::advance(Duration::from_secs(1)).await;
        if !waiting_for_response {
            for _ in 0..32 {
                assert!(matches!(commands.try_recv().unwrap(), Command::Pair(..)));
            }
        }
        assert_eq!(
            read.await,
            Err(if waiting_for_response {
                RpcBrokerError::Timeout
            } else {
                RpcBrokerError::TimeoutBeforeDispatch
            })
        );
        assert_eq!(Instant::now(), entered + READ_TIMEOUT);
        if let Some(reply) = reply {
            assert!(reply.is_closed());
        }
        assert!(
            commands.try_recv().is_err(),
            "expired reads must not enqueue"
        );
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn native_completed_decisions_wait_for_command_capacity_without_reexecution() {
    use crate::gateway::{Command, GatewayApprovalFailure};
    for decision in ["reject", "success", "fallback"] {
        let (path, mut provider, view) = fixture();
        authorize(
            &mut provider,
            &view,
            url::Url::parse("http://127.0.0.1:1").unwrap(),
        );
        personal_approval(&mut provider, "sign", Instant::now());
        let ready = provider.approval_updates.borrow()[0].clone();
        if decision != "reject" {
            provider.begin_approval(&ready.id).unwrap();
        }
        let (handle, mut commands) = approval_handle(&provider);
        for _ in 0..32 {
            handle
                .commands
                .try_send(Command::Pair(false, tokio::sync::oneshot::channel().0))
                .ok()
                .unwrap();
        }
        let mut completion: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), LocalProviderFailure>>>,
        > = if decision == "fallback" {
            Box::pin(handle.return_approval_to_review(ready.id.clone()))
        } else {
            Box::pin(handle.complete_approval(
                ready.id.clone(),
                if decision == "reject" {
                    Err(GatewayApprovalFailure::Local(
                        LocalProviderFailure::UserRejected,
                    ))
                } else {
                    Ok(json!("already-signed"))
                },
            ))
        };
        assert!(futures_util::poll!(&mut completion).is_pending());
        for _ in 0..32 {
            assert!(matches!(commands.try_recv().unwrap(), Command::Pair(..)));
        }
        assert!(futures_util::poll!(&mut completion).is_pending());
        match commands.try_recv().unwrap() {
            Command::CompleteApproval(id, outcome, reply) => {
                reply
                    .send(provider.complete_approval(&id, outcome))
                    .unwrap();
            }
            Command::ReturnApprovalToReview(id, reply) => {
                reply.send(provider.return_approval_to_review(&id)).unwrap();
            }
            _ => panic!("unexpected command"),
        }
        assert_eq!(completion.await, Ok(()));
        assert!(
            commands.try_recv().is_err(),
            "decision must only enqueue once"
        );
        if decision == "fallback" {
            assert_eq!(provider.approvals.len(), 1);
            assert!(provider.begin_approval(&ready.id).is_ok());
            assert_eq!(
                provider.approval_updates.borrow()[0].deadline,
                ready.deadline
            );
            assert!(
                messages(&mut provider)
                    .iter()
                    .all(|(_, value)| value["type"] != "provider_response")
            );
        } else {
            let delivered = messages(&mut provider);
            let outcome = result(&delivered);
            if decision == "reject" {
                assert_eq!(outcome["error"]["code"], 4001);
            } else {
                assert_eq!(outcome["result"], "already-signed");
            }
            assert!(provider.approvals.is_empty());
        }
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn native_decision_waits_are_bounded_by_original_control() {
    use crate::gateway::Command;
    for waiting_for_ack in [false, true] {
        for expire in [false, true] {
            let (path, mut provider, view) = fixture();
            authorize(
                &mut provider,
                &view,
                url::Url::parse("http://127.0.0.1:1").unwrap(),
            );
            personal_approval(&mut provider, "sign", Instant::now());
            let ready = provider.approval_updates.borrow()[0].clone();
            provider.begin_approval(&ready.id).unwrap();
            let (handle, mut commands) = approval_handle(&provider);
            if !waiting_for_ack {
                for _ in 0..32 {
                    handle
                        .commands
                        .try_send(Command::Pair(false, tokio::sync::oneshot::channel().0))
                        .ok()
                        .unwrap();
                }
            }
            let mut completion =
                Box::pin(handle.complete_approval(ready.id.clone(), Ok(json!("signed-once"))));
            assert!(futures_util::poll!(&mut completion).is_pending());
            let held = if waiting_for_ack {
                Some(commands.try_recv().unwrap())
            } else {
                None
            };
            if expire {
                tokio::time::advance(ready.deadline - Instant::now()).await;
            } else {
                ready.control.invalidate(&RpcBrokerError::OriginRejected);
            }
            assert_eq!(completion.await, Err(LocalProviderFailure::Unavailable));
            if let Some(Command::CompleteApproval(id, outcome, reply)) = held {
                assert!(reply.is_closed());
                assert!(provider.complete_approval(&id, outcome).is_err());
            }
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

#[tokio::test]
async fn native_broker_completion_preserves_payload_and_revalidates_delivery_owner() {
    use crate::gateway::GatewayApprovalFailure;
    let payload = json!({"code": 4001, "message": "remote nonce rejection",
        "data": {"nested": [null, false, {"value": 3}]}, "extension": ["retained"]});
    let remote = crate::RpcRemoteError::from(
        serde_json::from_value::<
            alloy::serde::WithOtherFields<alloy::rpc::json_rpc::ErrorPayload<serde_json::Value>>,
        >(payload.clone())
        .unwrap(),
    );
    for broker in [
        RpcBrokerError::Remote(remote.clone()),
        RpcBrokerError::InnerRevert(crate::RpcRevert::from_individual(
            alloy::primitives::Bytes::from_static(&[1, 2]),
            remote,
        )),
    ] {
        for invalidate in [false, true] {
            let (path, mut provider, view) = fixture();
            authorize(
                &mut provider,
                &view,
                url::Url::parse("http://127.0.0.1:1").unwrap(),
            );
            personal_approval(&mut provider, "sign", Instant::now());
            let ready = provider.approval_updates.borrow()[0].clone();
            let failure = GatewayApprovalFailure::Broker(broker.clone());
            assert_eq!(
                provider.complete_approval(&ready.id, Err(failure.clone())),
                Err(LocalProviderFailure::Unavailable)
            );
            provider.begin_approval(&ready.id).unwrap();
            let (handle, mut commands) = approval_handle(&provider);
            let mut completion = Box::pin(handle.complete_approval(ready.id.clone(), Err(failure)));
            assert!(futures_util::poll!(&mut completion).is_pending());
            let crate::gateway::Command::CompleteApproval(id, failure, reply) =
                commands.try_recv().unwrap()
            else {
                panic!("expected typed native completion");
            };
            reply
                .send(provider.complete_approval(&id, failure))
                .unwrap();
            assert_eq!(completion.await, Ok(()));
            let mut delivery = provider
                .drain()
                .into_iter()
                .find(|(_, delivery)| {
                    matches!(
                        delivery.message,
                        GatewayServerMessage::ProviderResponse { .. }
                    )
                })
                .unwrap()
                .1;
            if invalidate {
                provider.unregister(1, "doc");
            }
            let status = provider.delivery(1, &mut delivery);
            if invalidate {
                assert!(status != DeliveryStatus::Current);
            } else {
                assert!(status == DeliveryStatus::Current);
                assert_eq!(
                    serde_json::to_value(delivery.message).unwrap()["error"],
                    payload
                );
            }
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}
