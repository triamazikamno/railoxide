use super::fixtures::*;
use super::*;

#[test]
fn hardware_typed_data_error_maps_to_unsupported_method() {
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.item.method = WalletConnectSupportedMethod::EthSignTypedDataV4;
    request.account_source = PublicAccountSource::HardwareDerived;
    let error = eyre::eyre!(
        "WalletConnect eth_signTypedData_v4 is unsupported for hardware Public accounts"
    );

    assert_eq!(
        walletconnect_request_approval_error_kind(&request, &error),
        WalletConnectRequestErrorKind::UnsupportedMethod
    );
    request.item.method = WalletConnectSupportedMethod::EthSignTypedData;
    assert_eq!(
        walletconnect_request_approval_error_kind(&request, &error),
        WalletConnectRequestErrorKind::UnsupportedMethod
    );
}

#[test]
fn hardware_typed_data_recovery_mismatch_maps_to_error_response() {
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.item.method = WalletConnectSupportedMethod::EthSignTypedDataV4;
    request.account_source = PublicAccountSource::HardwareDerived;
    let error = eyre::eyre!(
        "hardware public signer address mismatch: expected 0x1111111111111111111111111111111111111111, got 0x2222222222222222222222222222222222222222"
    );

    let response = build_walletconnect_jsonrpc_error(
        request.item.id,
        walletconnect_request_approval_error_kind(&request, &error),
        format_report_chain(&error),
    );

    assert!(response.result.is_none());
    let error = response.error.expect("error response");
    assert_eq!(
        error.code,
        WalletConnectRequestErrorKind::UnsupportedMethod.code()
    );
    assert!(error.message.contains("address mismatch"));
}

#[test]
fn hardware_device_cancel_maps_to_user_rejected() {
    let request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    let error = eyre::eyre!("Trezor ActionCancelled: user cancelled on device");

    assert!(is_walletconnect_user_rejected_error(&error));
    assert_eq!(
        walletconnect_request_approval_error_kind(&request, &error),
        WalletConnectRequestErrorKind::UserRejected
    );
}

#[test]
fn approval_relay_steps_store_before_first_publish() {
    let steps = vec![
        WalletConnectRelayStep::FetchMessages {
            topic: "session-topic".to_owned(),
        },
        WalletConnectRelayStep::Subscribe {
            topic: "session-topic".to_owned(),
        },
        WalletConnectRelayStep::Publish(WalletConnectRelayRpc::Publish {
            topic: "session-topic".to_owned(),
            message: "settle".to_owned(),
            ttl: 300,
            tag: 1102,
        }),
        WalletConnectRelayStep::Publish(WalletConnectRelayRpc::Publish {
            topic: "pairing-topic".to_owned(),
            message: "proposal-response".to_owned(),
            ttl: 300,
            tag: 1101,
        }),
    ];

    let (pre_persist, post_persist) = walletconnect_split_pre_persist_relay_steps(steps);

    assert_eq!(pre_persist.len(), 2);
    assert!(
        pre_persist
            .iter()
            .all(|step| !matches!(step, WalletConnectRelayStep::Publish(_)))
    );
    assert_eq!(post_persist.len(), 2);
    assert!(
        post_persist
            .iter()
            .all(|step| matches!(step, WalletConnectRelayStep::Publish(_)))
    );
}

#[tokio::test]
async fn approval_post_persist_relay_error_removes_session() {
    let (root_dir, store) = walletconnect_test_store();
    let store = Arc::new(store);
    let view_session = Arc::new(import_test_wallet(
        store.as_ref(),
        "wc-approval-timeout",
        "WC Approval Timeout",
    ));
    let mut session = test_walletconnect_session("approval-timeout-topic");
    session.session_uuid = "approval-timeout-session".to_owned();
    let steps = vec![WalletConnectRelayStep::Publish(
        WalletConnectRelayRpc::Publish {
            topic: "approval-timeout-topic".to_owned(),
            message: "settle-or-proposal-response".to_owned(),
            ttl: 300,
            tag: 1101,
        },
    )];
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let worker = WalletConnectRelayWorkerHandle {
        worker_id: 1,
        project_id: "project-a".to_owned(),
        command_tx,
    };
    let task_store = Arc::clone(&store);
    let task_view_session = Arc::clone(&view_session);
    let task_session = session.clone();
    let approval = tokio::spawn(async move {
        execute_walletconnect_approval_relay_steps(
            &worker,
            task_store.as_ref(),
            task_view_session.as_ref(),
            &task_session,
            steps,
        )
        .await
    });

    let command = command_rx.recv().await.expect("pre-persist command");
    let WalletConnectRelayWorkerCommand::Execute {
        steps, response_tx, ..
    } = command
    else {
        panic!("expected pre-persist execute command");
    };
    assert!(steps.is_empty());
    assert!(
        response_tx
            .send(Ok(WalletConnectRelayOutput::default()))
            .is_ok()
    );

    let command = command_rx.recv().await.expect("post-persist command");
    let WalletConnectRelayWorkerCommand::Execute {
        steps, response_tx, ..
    } = command
    else {
        panic!("expected post-persist execute command");
    };
    assert!(matches!(
        &steps[0],
        WalletConnectRelayStep::Publish(WalletConnectRelayRpc::Publish { topic, .. })
            if topic == "approval-timeout-topic"
    ));
    assert!(
        response_tx
            .send(Err("relay response timed out".to_owned()))
            .is_ok()
    );

    let result = approval.await.expect("approval task").unwrap();

    assert_eq!(
        result.post_persist_error.as_deref(),
        Some("relay response timed out")
    );
    assert!(
        store
            .load_walletconnect_session(view_session.as_ref(), &session.session_uuid)
            .is_err()
    );

    drop(view_session);
    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn walletconnect_hash_fallback_mode_uses_request_session_account() {
    let (root_dir, store) = walletconnect_test_store();
    let wallet_id = "wc-hardware-fallback-mode";
    let profile_fingerprint = "ledger:evm:0x1111111111111111111111111111111111111111";
    let descriptor = HardwareDerivationDescriptor::ledger_eip1024_v1(
        parse_bip32_path("m/44'/60'/0'/0/0").expect("hardware path"),
        0,
        profile_fingerprint.to_owned(),
        HardwareWalletSyncIntent::CreateNew,
    );
    let metadata = store
        .new_hardware_wallet_metadata(TEST_PASSWORD, wallet_id, "Hardware wallet", descriptor)
        .expect("hardware metadata");
    let view_key = HardwareViewAccessKey::new([9u8; 32]);
    store
        .store_hardware_derived_wallet_from_entropy_with_metadata(
            TEST_PASSWORD,
            wallet_id,
            0,
            &[7u8; 32],
            &metadata,
            &view_key,
        )
        .expect("store hardware wallet");
    let mut hardware_session = store
        .hardware_profile_session_for_fingerprint(
            TEST_PASSWORD,
            HardwareDeviceKind::Ledger,
            profile_fingerprint,
            None,
        )
        .expect("hardware profile session");
    let public_descriptor =
        HardwarePublicAccountDescriptor::for_wallet_public_index(HardwareDeviceKind::Ledger, 0, 0)
            .expect("hardware public descriptor");
    hardware_session
        .cache_typed_data_signing_mode(
            &public_descriptor,
            HardwareTypedDataSigningMode::Eip712HashFallback,
        )
        .expect("cache fallback mode");
    let view_session = store
        .load_hardware_view_session(TEST_PASSWORD, &hardware_session, wallet_id, &view_key)
        .expect("hardware view session");
    let mut request = test_walletconnect_request("session-topic:7", Some(1_700_000_300));
    request.account_source = PublicAccountSource::HardwareDerived;
    request.item.method = WalletConnectSupportedMethod::EthSignTypedDataV4;
    request.session.selected_public_account_uuid = "hardware-account-a".to_owned();
    request.session.selected_public_account_scope = PublicAccountScope::Global;
    let other_account = PublicAccountMetadata {
        public_account_uuid: "selected-account-b".to_owned(),
        address: alloy::primitives::Address::from([0x22; 20]),
        label: None,
        source: PublicAccountSource::Imported,
        scope: PublicAccountScope::Global,
        derivation_index: None,
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let request_account = PublicAccountMetadata {
        public_account_uuid: "hardware-account-a".to_owned(),
        address: request.item.account,
        label: None,
        source: PublicAccountSource::HardwareDerived,
        scope: PublicAccountScope::Global,
        derivation_index: Some(0),
        hardware_descriptor: Some(public_descriptor),
        status: PublicAccountStatus::Active,
        display_order: 1,
    };
    let public_accounts = vec![other_account, request_account];

    assert_eq!(
        walletconnect_hardware_typed_data_mode_for_request(
            &request,
            &public_accounts,
            Some(&view_session),
        ),
        HardwareTypedDataSigningMode::Eip712HashFallback
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn switch_chain_event_uses_eip1193_hex_chain_id() {
    let (root_dir, store) = walletconnect_test_store();
    let view_session = import_test_wallet(&store, "wc-switch-event", "WC Switch Event");
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("Switch WC"),
            true,
        )
        .expect("import public account");
    let mut session = test_walletconnect_session("switch-session-topic");
    session.selected_public_account_uuid = account.public_account_uuid.clone();
    session.selected_public_account_scope = account.scope.clone();
    session.approved_namespaces.insert(
        "eip155".to_owned(),
        WalletConnectApprovedNamespace {
            chains: vec!["eip155:1".to_owned(), "eip155:42161".to_owned()],
            accounts: vec![
                format!("eip155:1:{}", account.address),
                format!("eip155:42161:{}", account.address),
            ],
            methods: vec!["wallet_switchEthereumChain".to_owned()],
            events: vec!["chainChanged".to_owned()],
        },
    );
    let request = WalletConnectJsonRpcRequest::new(
        93,
        "wc_sessionRequest",
        json!({
            "chainId": "eip155:1",
            "request": {
                "method": "wallet_switchEthereumChain",
                "params": [{ "chainId": "0xa4b1" }]
            }
        }),
    );
    let relay_message = test_walletconnect_relay_message(&session, &request);

    let outcome = process_walletconnect_session_message(
        &store,
        &view_session,
        &session,
        &relay_message,
        &BTreeSet::from([1, 42161]),
        current_unix_seconds(),
    )
    .expect("process switch chain");

    let SessionMessageOutcome::Respond {
        response,
        post_response_requests,
        ..
    } = outcome
    else {
        panic!("expected switch response");
    };
    assert_eq!(response.error, None);
    assert_eq!(post_response_requests.len(), 1);
    assert_eq!(
        post_response_requests[0].params["event"]["data"],
        json!("0xa4b1")
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn malformed_send_transaction_params_return_invalid_params_error() {
    let (root_dir, store) = walletconnect_test_store();
    let view_session = import_test_wallet(&store, "wc-malformed-params", "WC Malformed Params");
    let session = test_walletconnect_session("malformed-session-topic");
    let request = WalletConnectJsonRpcRequest::new(
        94,
        "wc_sessionRequest",
        json!({
            "chainId": "eip155:1",
            "request": {
                "method": "eth_sendTransaction",
                "params": [{
                    "from": "0x1111111111111111111111111111111111111111",
                    "data": "0xzz"
                }]
            }
        }),
    );
    let relay_message = test_walletconnect_relay_message(&session, &request);

    let outcome = process_walletconnect_session_message(
        &store,
        &view_session,
        &session,
        &relay_message,
        &BTreeSet::from([1]),
        current_unix_seconds(),
    )
    .expect("process malformed request");

    let SessionMessageOutcome::Respond { response, .. } = outcome else {
        panic!("expected error response");
    };
    let error = response.error.expect("json-rpc error");
    assert_eq!(
        error.code,
        WalletConnectRequestErrorKind::MalformedParams.code()
    );
    assert!(error.message.contains("transaction data must be valid hex"));

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn walletconnect_transaction_request_preserves_explicit_execution_fields() {
    let from = alloy::primitives::Address::from([0x11; 20]);
    let to = alloy::primitives::Address::from([0x22; 20]);
    let access_list: alloy::rpc::types::transaction::AccessList = serde_json::from_value(json!([
        {
            "address": to.to_string(),
            "storageKeys": ["0x0000000000000000000000000000000000000000000000000000000000000003"]
        }
    ]))
    .unwrap();
    let tx = transaction_request_from_walletconnect(
        1,
        WalletConnectEvmTransaction {
            from,
            to: Some(to),
            value: Some(U256::from(5_u64)),
            data: None,
            access_list: Some(access_list.clone()),
            gas: Some(U256::from(21_000_u64)),
            gas_price: None,
            max_fee_per_gas: Some(U256::from(20_000_000_000_u64)),
            max_priority_fee_per_gas: Some(U256::from(2_000_000_000_u64)),
            chain_id: Some(1),
            nonce: Some(U256::from(7_u64)),
            transaction_type: Some(1),
            raw: json!({}),
        },
    )
    .unwrap();

    assert_eq!(tx.chain_id, Some(1));
    assert_eq!(tx.from, Some(from));
    assert_eq!(tx.to, Some(to.into()));
    assert_eq!(tx.value, Some(U256::from(5_u64)));
    assert_eq!(tx.gas, Some(21_000));
    assert_eq!(tx.max_fee_per_gas, Some(20_000_000_000));
    assert_eq!(tx.max_priority_fee_per_gas, Some(2_000_000_000));
    assert_eq!(tx.nonce, Some(7));
    assert_eq!(tx.access_list, Some(access_list));
    assert_eq!(tx.transaction_type, Some(1));
}

#[test]
fn personal_sign_message_bytes_decode_only_explicit_hex_prefix() {
    assert_eq!(walletconnect_personal_message_bytes("0x616263"), b"abc");
    assert_eq!(walletconnect_personal_message_bytes("616263"), b"616263");
    assert_eq!(
        walletconnect_personal_message_bytes("deadbeef"),
        b"deadbeef"
    );
}
