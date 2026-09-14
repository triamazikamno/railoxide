use super::fixtures::*;
use super::*;

#[test]
fn nested_walletconnect_request_expiry_is_preferred() {
    let request_params = json!({
        "chainId": "eip155:1",
        "expiryTimestamp": 1_700_000_300u64,
        "request": {
            "method": "eth_accounts",
            "expiryTimestamp": 1_700_000_010u64
        }
    });
    let request_payload = request_params.get("request").unwrap();

    let expiry =
        match walletconnect_session_request_expiry_timestamp(&request_params, request_payload) {
            Ok(expiry) => expiry,
            Err(error) => panic!("request expiry failed: {}", error.message),
        };

    assert_eq!(expiry, Some(1_700_000_010));
}

#[test]
fn submitted_transaction_expiry_boundary_keeps_result_response() {
    assert!(!walletconnect_approval_should_publish_expired_response(
        Some(1_700_000_000),
        1_700_000_000,
        Some("0xabc")
    ));
    assert!(walletconnect_approval_should_publish_expired_response(
        Some(1_700_000_000),
        1_700_000_000,
        None
    ));
    assert!(!walletconnect_approval_should_publish_expired_response(
        Some(1_700_000_001),
        1_700_000_000,
        None
    ));
}

#[test]
fn send_transaction_approval_does_not_use_expiry_timeout_after_authorization() {
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.parsed = WalletConnectParsedRequest::EthSendTransaction {
        transaction: WalletConnectEvmTransaction {
            from: request.item.account,
            to: None,
            value: None,
            data: None,
            access_list: None,
            gas: None,
            gas_price: None,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            chain_id: None,
            nonce: None,
            transaction_type: None,
            raw: json!({}),
        },
    };
    let personal_sign = WalletConnectParsedRequest::PersonalSign {
        message: "0x68656c6c6f".to_owned(),
        account: request.item.account,
    };

    assert!(!walletconnect_request_approval_uses_expiry_timeout(
        &request.parsed
    ));
    assert!(walletconnect_request_approval_uses_expiry_timeout(
        &personal_sign
    ));
}

#[test]
fn expired_request_keys_skip_unexpired_and_in_flight_requests() {
    let mut pending_requests = BTreeMap::new();
    pending_requests.insert(
        "session-topic:1".to_owned(),
        test_walletconnect_request("session-topic:1", Some(1_700_000_010)),
    );
    pending_requests.insert(
        "session-topic:2".to_owned(),
        test_walletconnect_request("session-topic:2", Some(1_700_000_020)),
    );
    pending_requests.insert(
        "session-topic:3".to_owned(),
        test_walletconnect_request("session-topic:3", None),
    );
    let mut request_actions = BTreeSet::new();
    request_actions.insert("session-topic:1".to_owned());

    let expired =
        expired_walletconnect_request_keys(&pending_requests, &request_actions, 1_700_000_011);

    assert!(expired.is_empty());

    let expired =
        expired_walletconnect_request_keys(&pending_requests, &request_actions, 1_700_000_021);

    assert_eq!(expired, vec!["session-topic:2".to_owned()]);
}

#[test]
fn pending_request_expiry_allows_less_than_protocol_minimum_remaining() {
    assert!(
        walletconnect_validate_pending_request_expiry(Some(1_700_000_240), 1_700_000_000).is_ok()
    );

    let error = walletconnect_validate_pending_request_expiry(Some(1_700_000_000), 1_700_000_000)
        .expect_err("expired request");
    assert_eq!(error.kind, WalletConnectRequestErrorKind::ExpiredRequest);
}

#[tokio::test]
async fn request_expiry_deadline_cancels_slow_approval_future() {
    let result =
        walletconnect_await_before_request_expiry(Some(current_unix_seconds() + 1), async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            "completed"
        })
        .await;

    assert_eq!(result, Err(WalletConnectRequestExpired));
}

#[tokio::test]
async fn expired_approval_task_publishes_expired_response() {
    let (root_dir, store) = walletconnect_test_store();
    let store = Arc::new(store);
    let view_session = Arc::new(import_test_wallet(
        store.as_ref(),
        "wc-expired-approval",
        "WC Expired Approval",
    ));
    let mut request =
        test_walletconnect_request("session-topic:expired", Some(current_unix_seconds()));
    request.binding.public_account_uuid = "public-account".to_owned();
    let request_id = request.item.id;
    let session = test_walletconnect_session("session-topic");
    let session_topic = session.session_topic.clone();
    let sym_key = session.keys.sym_key;
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let context = WalletConnectClientContext {
        worker: WalletConnectRelayWorkerHandle {
            worker_id: 1,
            project_id: "project-a".to_owned(),
            command_tx,
        },
    };
    let response_sender = WalletConnectRequestRoute::from_session(&session)
        .response_sender(context.worker, request_id);
    let http = wallet_ops::build_http_client(None).expect("direct http context");

    let task_store = Arc::clone(&store);
    let task_view_session = Arc::clone(&view_session);
    let approval = tokio::spawn(async move {
        Box::pin(approve_walletconnect_request_task(
            request,
            task_store,
            task_view_session,
            Zeroizing::new(TEST_PASSWORD.to_owned()),
            None,
            None,
            None,
            None,
            response_sender,
            http,
            false,
            None,
            None,
            None,
        ))
        .await
    });

    let command = command_rx.recv().await.expect("expired response command");
    let WalletConnectRelayWorkerCommand::Execute {
        steps,
        wait_for_push,
        emit_pushes,
        response_tx,
    } = command
    else {
        panic!("expected execute command");
    };
    assert!(!wait_for_push);
    assert!(emit_pushes);
    let WalletConnectRelayStep::Publish(WalletConnectRelayRpc::Publish {
        topic,
        message,
        ttl,
        tag,
    }) = &steps[0]
    else {
        panic!("expected expired response publish");
    };
    assert_eq!(topic, &session_topic);
    assert_eq!(*ttl, WALLETCONNECT_RELAY_TTL_SECS);
    assert_eq!(*tag, WC_SESSION_REQUEST_RESPONSE_TAG);
    let envelope = wallet_ops::WalletConnectEnvelope::from_base64(message).expect("envelope");
    let plaintext = decode_walletconnect_message(&sym_key, &envelope).expect("expired response");
    let response: WalletConnectJsonRpcResponse<Value> =
        serde_json::from_slice(&plaintext).expect("response json");
    assert_eq!(response.id, request_id);
    assert_eq!(
        response.error.expect("expired error").code,
        WalletConnectRequestErrorKind::ExpiredRequest.code()
    );
    assert!(
        response_tx
            .send(Ok(WalletConnectRelayOutput::default()))
            .is_ok()
    );

    let outcome = approval
        .await
        .expect("approval task")
        .expect("approval task");

    assert!(outcome.expired);
    assert!(outcome.response_published);
    assert!(outcome.submitted_tx_hash.is_none());
    assert!(command_rx.try_recv().is_err());

    drop(view_session);
    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
