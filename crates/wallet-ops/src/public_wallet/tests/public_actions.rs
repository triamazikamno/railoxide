use super::*;
use crate::DesktopWalletVaultDbExt as _;

#[test]
fn public_action_attempt_errors_distinguish_signing_from_retryable_sending() {
    let signing = PublicActionAttemptError::Signing(eyre!("user rejected on device"));
    let sending = PublicActionAttemptError::Sending(eyre!("rpc rejected transaction"));

    assert!(matches!(signing, PublicActionAttemptError::Signing(_)));
    assert!(matches!(sending, PublicActionAttemptError::Sending(_)));
}

#[test]
fn public_action_pre_broadcast_checkpoint_yields_for_abort() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    runtime.block_on(async {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = ready_tx.send(());
            public_action_before_raw_broadcast_checkpoint().await;
            true
        });

        ready_rx.await.expect("checkpoint task started");
        task.abort();
        let error = task.await.expect_err("checkpoint task should abort");
        assert!(error.is_cancelled());
    });
}

#[test]
fn effective_public_chain_config_uses_settings_overrides() {
    let mut effective = effective_chain_for_rpc("https://rpc.example", 42);
    effective.rpc_route = effective
        .rpc_route
        .with_multicall(address!("0x0000000000000000000000000000000000000003"));
    let private = effective.railgun.as_mut().unwrap();
    private.deployment.contract = address!("0x0000000000000000000000000000000000000001");
    private.deployment.relay_adapt_contract =
        Some(address!("0x0000000000000000000000000000000000000004"));
    effective.wrapped_native_token = Some(address!("0x0000000000000000000000000000000000000002"));

    let config = public_chain_runtime_config(1, &effective).expect("effective config");

    assert_eq!(config.rpc_route.endpoint_urls().len(), 1);
    assert_eq!(
        config.rpc_route.endpoint_urls()[0].as_str(),
        "https://rpc.example/"
    );
    assert_eq!(
        config.require_railgun().unwrap().contract,
        address!("0x0000000000000000000000000000000000000001")
    );
    assert_eq!(
        config.require_railgun().unwrap().relay_adapt_contract,
        Some(address!("0x0000000000000000000000000000000000000004"))
    );
    assert_eq!(
        config.wrapped_native_token,
        Some(address!("0x0000000000000000000000000000000000000002"))
    );
    assert_eq!(
        config.rpc_route.multicall(),
        Some(address!("0x0000000000000000000000000000000000000003"))
    );
    assert_eq!(config.gas.gas_limit_buffer, 42);
}

#[test]
fn walletconnect_effective_public_chain_config_rejects_disabled_chain() {
    let mut effective = effective_chain_for_rpc("https://rpc.example", 42);
    effective.enabled = false;

    let Err(error) = public_chain_runtime_config(1, &effective) else {
        panic!("disabled chain was accepted")
    };

    assert!(error.to_string().contains("disabled"));
}

#[test]
fn public_send_request_uses_native_value_or_erc20_transfer() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let token = address!("0x3333333333333333333333333333333333333333");
    let amount = U256::from(5_u64);

    let native = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Native,
            amount,
            recipient,
        },
    )
    .expect("native transfer request");
    assert_eq!(native.to, Some(recipient.into()));
    assert_eq!(native.value, Some(amount));

    let erc20 = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Transfer {
            asset: PublicAssetId::Erc20(token),
            amount,
            recipient,
        },
    )
    .expect("ERC20 transfer request");
    assert_eq!(erc20.to, Some(token.into()));
    let expected_transfer = PublicErc20::transferCall { recipient, amount }.abi_encode();
    assert_eq!(
        erc20.input.input().expect("transfer input").as_ref(),
        expected_transfer.as_slice()
    );
}

#[test]
fn public_send_request_wraps_with_deposit_value_and_unwraps_with_withdraw() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let wrapped = address!("0x3333333333333333333333333333333333333333");
    let amount = U256::from(5_u64);

    let wrap = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::WrapNative { wrapped, amount },
    )
    .expect("wrap request");
    assert_eq!(wrap.to, Some(wrapped.into()));
    assert_eq!(wrap.value, Some(amount));
    assert_eq!(
        wrap.input.input().expect("deposit input").as_ref(),
        PublicWrappedNative::depositCall {}.abi_encode().as_slice()
    );

    let unwrap = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::UnwrapNative { wrapped, amount },
    )
    .expect("unwrap request");
    assert_eq!(unwrap.to, Some(wrapped.into()));
    assert_eq!(unwrap.value.unwrap_or_default(), U256::ZERO);
    assert_eq!(
        unwrap.input.input().expect("withdraw input").as_ref(),
        PublicWrappedNative::withdrawCall { amount }
            .abi_encode()
            .as_slice()
    );
}

#[test]
fn public_send_request_supports_raw_calls_and_contract_creation() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let call_data = Bytes::from_static(&[0x12, 0x34, 0x56, 0x78, 0xaa]);
    let call = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(7_u64),
            data: call_data.clone(),
        },
    )
    .expect("raw call request");
    assert_eq!(call.to, Some(to.into()));
    assert_eq!(call.value, Some(U256::from(7_u64)));
    assert_eq!(call.input.input(), Some(&call_data));

    let init_code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00]);
    let creation = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: None,
            value: U256::ZERO,
            data: init_code.clone(),
        },
    )
    .expect("contract creation request");
    assert_eq!(creation.to, Some(TxKind::Create));
    assert_eq!(creation.value, Some(U256::ZERO));
    assert_eq!(creation.input.input(), Some(&init_code));

    let mut managed_creation =
        public_action_eip1559_transaction_request(creation, 1, from, 10, 2, 3);
    managed_creation.gas = Some(100_000);
    let built = managed_creation
        .build_consensus_tx()
        .expect("build managed contract creation transaction");
    let alloy::consensus::TypedTransaction::Eip1559(built) = built else {
        panic!("expected EIP-1559 contract creation")
    };
    assert_eq!(built.to, TxKind::Create);
    assert_eq!(built.value, U256::ZERO);
    assert_eq!(built.input, init_code);

    let value_call = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::from(1_u64),
            data: Bytes::new(),
        },
    )
    .expect("positive-value empty-data call");
    assert_eq!(value_call.to, Some(to.into()));
    assert_eq!(value_call.value, Some(U256::from(1_u64)));
}

#[test]
fn public_send_request_rejects_empty_raw_intents() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");

    let empty_creation = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: None,
            value: U256::from(1_u64),
            data: Bytes::new(),
        },
    )
    .expect_err("empty creation must fail");
    assert!(empty_creation.to_string().contains("non-empty init code"));

    let no_op = public_send_transaction_request(
        1,
        from,
        &PublicTransactionIntent::Raw {
            to: Some(to),
            value: U256::ZERO,
            data: Bytes::new(),
        },
    )
    .expect_err("empty zero-value call must fail");
    assert!(no_op.to_string().contains("native value or data"));
}

#[test]
fn advanced_public_transaction_authorization_is_payload_and_gas_bounded() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let intent = PublicTransactionIntent::Raw {
        to: Some(to),
        value: U256::from(7_u64),
        data: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]),
    };
    let fingerprint = public_advanced_transaction_payload_fingerprint(1, from, &intent, 10, 2);
    let authorization = PublicAdvancedTransactionAuthorization {
        payload_fingerprint: fingerprint,
        gas_limit: buffered_advanced_gas_limit(50_000, 12_345),
    };

    assert_eq!(authorization.gas_limit, 62_345);
    assert_eq!(
        public_send_authorized_gas_limit(
            1,
            from,
            &intent,
            Some(authorization),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 10,
                max_priority_fee_per_gas: 2,
            },
        )
        .expect("matching authorization"),
        Some(62_345)
    );
    assert!(ensure_advanced_gas_estimate_authorized(62_345, 62_345).is_ok());
    assert!(ensure_advanced_gas_estimate_authorized(62_346, 62_345).is_err());

    let changed_intent = PublicTransactionIntent::Raw {
        to: Some(to),
        value: U256::from(8_u64),
        data: Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]),
    };
    let stale = public_send_authorized_gas_limit(
        1,
        from,
        &changed_intent,
        Some(authorization),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 10,
            max_priority_fee_per_gas: 2,
        },
    )
    .expect_err("changed payload must invalidate authorization");
    assert!(stale.to_string().contains("changed after gas estimation"));

    let authorized_fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 10,
        max_priority_fee_per_gas: 2,
    };
    assert!(
        ensure_public_action_command_gas_fee_authorized(Some(authorized_fee), authorized_fee)
            .is_ok()
    );
    assert!(
        ensure_public_action_command_gas_fee_authorized(
            Some(authorized_fee),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 11,
                max_priority_fee_per_gas: 2,
            },
        )
        .is_err()
    );
}

#[test]
fn advanced_native_balance_exposure_includes_value_and_authorized_gas() {
    assert_eq!(
        public_action_native_exposure(U256::from(7_u64), 50_000, 10),
        U256::from(500_007_u64)
    );
}

#[test]
fn transaction_receipt_output_omits_absent_contract_address() {
    let transfer = crate::TxReceiptOutput {
        tx_hash: "0x01".to_string(),
        status: true,
        block_number: 1,
        gas_used: 21_000,
        contract_address: None,
    };
    let transfer_json = serde_json::to_value(&transfer).expect("serialize transfer receipt");
    assert!(transfer_json.get("contract_address").is_none());

    let creation = crate::TxReceiptOutput {
        contract_address: Some("0x2222222222222222222222222222222222222222".to_string()),
        ..transfer
    };
    let creation_json = serde_json::to_value(&creation).expect("serialize creation receipt");
    assert_eq!(
        creation_json["contract_address"],
        "0x2222222222222222222222222222222222222222"
    );
}

#[tokio::test]
async fn native_preflight_filters_broker_withdrawals_and_tries_each_candidate_once() {
    let failing = MockRpcServer::spawn(true, 50_000).await;
    let other = MockRpcServer::spawn_with_estimate_result(
        false,
        MockRpcEstimateResult::Error {
            code: -32000,
            message: "estimate unavailable".to_owned(),
            data: None,
        },
    )
    .await;
    let route = RpcChainRoute::new(
        1,
        vec![
            Url::parse(&failing.url).unwrap(),
            Url::parse(&other.url).unwrap(),
        ],
    );
    let http = HttpContext::direct_for_tests();
    // The original transaction pool predates the broker's withdrawal and still contains both.
    let pool = crate::query_rpc_pool_for_route(&sensitive(&route), &http);
    let warmup = http.rpc_broker().warm_identities(vec![route.clone()], None);
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            if http
                .rpc_broker()
                .endpoint_candidates(&route, None)
                .await
                .unwrap()
                .len()
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed startup responsiveness check withdraws one endpoint");
    let failed_before = failing.calls().len();
    for warmed in [true, false] {
        let fresh_http = HttpContext::direct_for_tests();
        let http = if warmed { &http } else { &fresh_http };
        let before_other = other.calls();
        let error = super::submission::public_action_preflight_from_rpc_pool(
            &pool,
            http,
            &sensitive(&route),
            1,
            Address::repeat_byte(1),
            TransactionRequest::default().with_to(Address::repeat_byte(2)),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 2,
            },
            &default_effective_chain(1).gas,
            PublicShieldTransactionProfile::Railway,
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
        .err()
        .expect("all eligible candidates fail");
        assert!(matches!(
            error,
            super::submission::PublicActionPreflightError::Other(_)
        ));
        let after_other = other.calls();
        assert_eq!(
            rpc_calls_for(&after_other, "eth_estimateGas").len()
                - rpc_calls_for(&before_other, "eth_estimateGas").len(),
            1,
        );
        if warmed {
            assert_eq!(
                failing.calls().len(),
                failed_before,
                "withdrawn endpoint was bypassed"
            );
        } else {
            assert_eq!(
                rpc_calls_for(&failing.calls(), "eth_getTransactionCount").len(),
                1
            );
        }
    }
    drop(warmup);
}

#[test]
fn public_actions_reject_zero_amount_and_public_only_shield_before_signing() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let http = runtime.block_on(async { HttpContext::direct_for_tests() });
    let client = runtime.block_on(http.operation_http_client()).unwrap();
    let recipient = address!("0x2222222222222222222222222222222222222222");

    let send_result = runtime.block_on(submit_public_send(
        PublicSendRequest {
            executor_owner: None,
            transaction_tracking: None,
            chain_id: 1,
            effective_chain: default_effective_chain(1),
            view_session: Arc::clone(&view_session),
            vault_store: Arc::clone(&store),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.to_string()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: "unused".to_string(),
            intent: PublicTransactionIntent::Transfer {
                asset: PublicAssetId::Native,
                amount: U256::ZERO,
                recipient,
            },
            advanced_authorization: None,
            gateway_origin: None,
            gas_fee: PublicActionGasFeeSelection::Auto,
            command_rx: None,
            event_tx: None,
        },
        &http,
        &client,
    ));
    match send_result {
        Ok(_) => panic!("zero-value public send unexpectedly succeeded"),
        Err(error) => assert!(error.to_string().contains("amount is required")),
    }

    let wrapped = address!("0x3333333333333333333333333333333333333333");
    for (configured, intent, expected) in [
        (
            Some(address!("0x4444444444444444444444444444444444444444")),
            PublicTransactionIntent::WrapNative {
                wrapped,
                amount: U256::from(1),
            },
            "wrapped native token changed",
        ),
        (
            None,
            PublicTransactionIntent::UnwrapNative {
                wrapped,
                amount: U256::from(1),
            },
            "no wrapped native token",
        ),
    ] {
        let mut chain = default_effective_chain(1);
        chain.wrapped_native_token = configured;
        let result = runtime.block_on(submit_public_send(
            PublicSendRequest {
                executor_owner: None,
                transaction_tracking: None,
                chain_id: 1,
                effective_chain: chain,
                view_session: Arc::clone(&view_session),
                vault_store: Arc::clone(&store),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_string()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: "unused".to_string(),
                intent,
                advanced_authorization: None,
                gateway_origin: None,
                gas_fee: PublicActionGasFeeSelection::Auto,
                command_rx: None,
                event_tx: None,
            },
            &http,
            &client,
        ));
        match result {
            Ok(_) => panic!("wrap or unwrap to an unconfigured token unexpectedly succeeded"),
            Err(error) => assert!(error.to_string().contains(expected), "{error}"),
        }
    }

    for public_only in [false, true] {
        let mut chain = default_effective_chain(1);
        if public_only {
            chain.railgun = None;
        }
        let shield_result = runtime.block_on(submit_public_shield(
            PublicShieldRequest {
                executor_owner: None,
                transaction_tracking: None,
                chain_id: 1,
                effective_chain: chain,
                view_session: view_session.clone(),
                vault_store: store.clone(),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_string()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: "unused".to_string(),
                asset: PublicAssetId::Native,
                amount: if public_only {
                    U256::from(1)
                } else {
                    U256::ZERO
                },
                profile: PublicShieldTransactionProfile::Railoxide,
                approval_mode: PublicShieldApprovalMode::Exact,
                native_preparation: None,
                native_operation: None,
                gateway_origin: None,
                gas_fee: PublicActionGasFeeSelection::Auto,
                gas_fee_mode: PublicActionGasFeeMode::Auto,
                authorized_fee_ceiling: PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas: 1,
                    max_priority_fee_per_gas: 1,
                },
                command_rx: None,
                event_tx: None,
            },
            &http,
            &client,
        ));
        match shield_result {
            Ok(_) => panic!("zero-value public shield unexpectedly succeeded"),
            Err(error) => assert!(error.to_string().contains(if public_only {
                "Railgun"
            } else {
                "amount is required"
            })),
        }
    }

    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn vaulted_public_signer_resolves_private_self_broadcast_gas_payers() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let derived = store
        .list_active_public_accounts_for_session(&view_session)
        .expect("active accounts")
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .expect("derived account");
    let derived_secret_key = format!("public-account-secret|{}", derived.public_account_uuid);
    assert!(
        db.get_desktop_wallet_vault_record(&derived_secret_key)
            .expect("load derived secret record")
            .is_none()
    );

    let derived_signer = vaulted_public_signer(
        &store,
        &view_session,
        Some(TEST_PASSWORD),
        &derived.public_account_uuid,
        None,
        None,
        None,
    )
    .expect("derived signer");
    assert_eq!(derived_signer.address(), derived.address);
    let Err(missing_password) = vaulted_public_signer(
        &store,
        &view_session,
        None,
        &derived.public_account_uuid,
        None,
        None,
        None,
    ) else {
        panic!("software public signer without password unexpectedly succeeded");
    };
    assert!(
        missing_password
            .to_string()
            .contains("vault password required for software public account signer")
    );

    let hardware_index = store
        .next_derived_public_account_index_for_session(&view_session)
        .expect("next hardware public index");
    let hardware_descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
        HardwareDeviceKind::Ledger,
        view_session.derivation_index(),
        hardware_index,
    )
    .expect("hardware descriptor");
    let hardware_address = address!("0x2222222222222222222222222222222222222222");
    let confirmed =
        ConfirmedHardwarePublicAccount::new_for_tests(hardware_descriptor, hardware_address);
    assert!(matches!(
        store.add_hardware_public_account(&view_session, &confirmed, Some("Ledger Gas")),
        Err(VaultError::HardwareWalletViewRequiresDevice)
    ));

    let hardware_wallet_id = "hardware-public-action-wallet";
    let hardware_private_descriptor = HardwareDerivationDescriptor::ledger_eip1024_v1(
        parse_bip32_path("m/44'/60'/0'/0/0").expect("hardware path"),
        0,
        "ledger:evm:0x1111111111111111111111111111111111111111".to_string(),
        HardwareWalletSyncIntent::CreateNew,
    );
    let output = HardwareOperationOutput::new([42; 32]);
    let view_access_key =
        hardware_view_access_key_from_hardware_output(&hardware_private_descriptor, &output)
            .expect("hardware view key");
    let entropy = synthetic_entropy_from_hardware_output(&hardware_private_descriptor, output)
        .expect("hardware entropy");
    let hardware_metadata = store
        .new_hardware_wallet_metadata(
            TEST_PASSWORD,
            hardware_wallet_id,
            "Hardware public action",
            hardware_private_descriptor.clone(),
        )
        .expect("hardware wallet metadata");
    store
        .store_hardware_derived_wallet_from_entropy_with_metadata(
            TEST_PASSWORD,
            hardware_wallet_id,
            hardware_private_descriptor.account_index,
            entropy.expose_secret(),
            &hardware_metadata,
            &view_access_key,
        )
        .expect("store hardware wallet");
    let hardware_session = store
        .hardware_profile_session_for_fingerprint(
            TEST_PASSWORD,
            HardwareDeviceKind::Ledger,
            &hardware_private_descriptor.profile_fingerprint,
            None,
        )
        .expect("hardware profile session");
    let hardware_view_session = store
        .load_hardware_view_session(
            TEST_PASSWORD,
            &hardware_session,
            hardware_wallet_id,
            &view_access_key,
        )
        .expect("hardware view session");
    let hardware_public_descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
        HardwareDeviceKind::Ledger,
        hardware_view_session.derivation_index(),
        0,
    )
    .expect("hardware public descriptor");
    #[cfg(not(feature = "hardware"))]
    assert!(
        hardware_session
            .typed_data_signing_mode(&hardware_public_descriptor)
            .is_none()
    );
    let hardware_public = store
        .add_hardware_public_account(
            &hardware_view_session,
            &ConfirmedHardwarePublicAccount::new_for_tests(
                hardware_public_descriptor,
                address!("0x3333333333333333333333333333333333333333"),
            ),
            Some("Hardware Ledger Gas"),
        )
        .expect("hardware public account under hardware view");
    let hardware_secret_key = format!(
        "public-account-secret|{}",
        hardware_public.public_account_uuid
    );
    assert!(
        db.get_desktop_wallet_vault_record(&hardware_secret_key)
            .expect("load hardware public secret record")
            .is_none()
    );
    let hardware_signer = vaulted_public_signer(
        &store,
        &hardware_view_session,
        None,
        &hardware_public.public_account_uuid,
        None,
        None,
        None,
    )
    .expect("hardware signer with profile session");
    assert_eq!(hardware_signer.address(), hardware_public.address);
    assert!(hardware_signer.requires_device_approval());
    #[cfg(not(feature = "hardware"))]
    {
        // The uncached capability is admitted, then the default-build probe reports unsupported.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let error = runtime.block_on(walletconnect_sign_typed_data_v4(WalletConnectTypedDataSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: None,
            vault_store: store.clone(),
            view_session: Arc::new(hardware_view_session),
            authorization: Some(crate::DesktopPrivateSpendAuthorization::from_software_credentials(Zeroizing::new(TEST_PASSWORD.into()), None)),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: hardware_public.public_account_uuid,
            typed_data: serde_json::json!({
                "types": {"EIP712Domain": [], "Message": [{"name": "text", "type": "string"}]},
                "primaryType": "Message", "domain": {}, "message": {"text": "hello"}
            }),
            hash_fallback_confirmed: false,
            event_tx: Some(events),
        }, crate::OperationHttpClient::direct_for_tests())).unwrap_err();
        assert!(
            matches!(error.downcast_ref::<crate::walletconnect::WalletConnectError>(),
            Some(crate::walletconnect::WalletConnectError::UnsupportedMethod(method)) if method == "eth_signTypedData_v4")
        );
        assert!(
            received.try_recv().is_err(),
            "unsupported capability must stop before device signing"
        );
    }

    let imported = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("Imported"),
            false,
        )
        .expect("import public account");
    let imported_signer = vaulted_public_signer(
        &store,
        &view_session,
        Some(TEST_PASSWORD),
        &imported.public_account_uuid,
        None,
        None,
        None,
    )
    .expect("imported signer");
    assert_eq!(imported_signer.address(), imported.address);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[tokio::test]
async fn native_preflight_logs_failure_stage_without_sensitive_payloads() {
    use tracing::instrument::WithSubscriber;

    let from = Address::repeat_byte(0xab);
    let to = Address::repeat_byte(0xcd);
    let data = Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe]);
    let value = U256::from(12_345_678_901_234_567_890_123_u128);
    let remote_message = "private-remote-error-message";
    let remote_data = "private-remote-error-data";
    for remote_failure in [true, false] {
        let estimate = if remote_failure {
            MockRpcEstimateResult::Error {
                code: -32000,
                message: remote_message.to_owned(),
                data: Some(remote_data.to_owned()),
            }
        } else {
            MockRpcEstimateResult::Success(50_000)
        };
        let server = MockRpcServer::spawn_with_estimate_result(false, estimate).await;
        let mut endpoint = Url::parse(&server.url).unwrap();
        endpoint.set_username("private-rpc-user").unwrap();
        endpoint.set_password(Some("private-rpc-password")).unwrap();
        endpoint.set_path("/private-rpc-path");
        endpoint.set_query(Some("key=private-rpc-key"));
        let mut chain = default_effective_chain(42161);
        let mut second_endpoint = endpoint.clone();
        second_endpoint.set_path("/private-rpc-path/second");
        chain.rpc_route = RpcChainRoute::new(42161, vec![endpoint, second_endpoint]);
        let http = HttpContext::direct_for_tests_with_candidate_order(<[usize]>::reverse);
        let pool = crate::query_rpc_pool_for_route(&sensitive(&chain.rpc_route), &http);
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let error = super::submission::public_action_preflight_from_rpc_pool(
            &pool,
            &http,
            &sensitive(&chain.rpc_route),
            42161,
            from,
            TransactionRequest::default()
                .with_to(to)
                .with_value(value)
                .with_input(data.clone()),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 2,
            },
            &chain.gas,
            PublicShieldTransactionProfile::Railway,
            PublicAssetId::Native,
            super::types::PublicActionGasLimitStrategy::ChainBuffer,
            None,
            None,
            None,
            None,
            false,
            None,
        )
        .with_subscriber(subscriber)
        .await
        .err()
        .expect("preflight fails");
        let super::submission::PublicActionPreflightError::Other(error) = error else {
            panic!("failure must retain the original error");
        };
        if remote_failure {
            let error = error
                .downcast_ref::<alloy::transports::TransportError>()
                .expect("original transport error remains available");
            let payload = error.as_error_resp().expect("original RPC error");
            assert_eq!(payload.code, -32000);
            assert_eq!(payload.message, remote_message);
        } else {
            assert!(
                error
                    .to_string()
                    .contains("all public action query RPC attempts failed")
            );
            assert!(format!("{error:#}").contains("insufficient native balance"));
        }
        let calls = server.calls();
        assert_eq!(rpc_calls_for(&calls, "eth_getTransactionCount").len(), 2);
        assert_eq!(rpc_calls_for(&calls, "eth_estimateGas").len(), 2);
        assert_eq!(
            rpc_calls_for(&calls, "eth_getBalance").len(),
            2 * usize::from(!remote_failure)
        );
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        let failures = logs
            .lines()
            .filter(|line| line.contains("public action preflight failed"))
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        assert!(failures[0].contains("rpc_index=1"));
        assert!(failures[1].contains("rpc_index=0"));
        assert!(failures.iter().all(|line| line.contains(&format!(
            "endpoint={}",
            crate::http::redact_url_for_display(&Url::parse(&server.url).unwrap())
        ))));
        for field in ["chain_id=42161", "attempt=1", "attempts=2", "elapsed_ms="] {
            assert!(logs.contains(field), "missing diagnostic field {field}");
        }
        if remote_failure {
            assert!(logs.contains("stage=\"estimate\""));
            assert!(logs.contains("failure=\"rpc_error\""));
            assert!(logs.contains("rpc_code=-32000"));
        } else {
            assert!(logs.contains("stage=\"native_balance_validation\""));
            assert!(logs.contains("failure=\"validation\""));
        }
        for secret in [
            remote_message.to_owned(),
            remote_data.to_owned(),
            "private-rpc-user".to_owned(),
            "private-rpc-password".to_owned(),
            "private-rpc-path".to_owned(),
            "private-rpc-key".to_owned(),
            alloy::hex::encode_prefixed(from),
            alloy::hex::encode_prefixed(to),
            alloy::hex::encode_prefixed(&data),
            value.to_string(),
            "1000000000000000000000".to_owned(),
        ] {
            assert!(
                !logs.contains(&secret),
                "sensitive data appeared in diagnostics"
            );
        }
    }
}

#[tokio::test]
async fn ens_recipient_uses_normalized_alloy_calls_on_the_configured_desktop_route() {
    use alloy::ens::{
        EnsResolver, UNIVERSAL_RESOLVER_ADDRESS, UniversalResolver, dns_encode, namehash,
    };
    let target = Address::repeat_byte(9);
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request| {
            assert_eq!(request["method"], "eth_call");
            let transaction: TransactionRequest =
                serde_json::from_value(request["params"][0].clone()).unwrap();
            assert_eq!(transaction.to, Some(UNIVERSAL_RESOLVER_ADDRESS.into()));
            let call =
                UniversalResolver::resolveCall::abi_decode(transaction.input.input().unwrap())
                    .unwrap();
            let assigned = call.name == dns_encode("raffy.eth");
            let name = if assigned {
                "raffy.eth"
            } else {
                "unassigned.eth"
            };
            assert_eq!(call.name, Bytes::from(dns_encode(name)));
            assert_eq!(
                EnsResolver::addrCall::abi_decode(&call.data).unwrap().node,
                namehash(name)
            );
            let address = if assigned { target } else { Address::ZERO };
            let result = UniversalResolver::resolveCall::abi_encode_returns(
                &UniversalResolver::resolveReturn {
                    _0: EnsResolver::addrCall::abi_encode_returns(&address).into(),
                    _1: Address::repeat_byte(8),
                },
            );
            json!({"jsonrpc":"2.0", "id":request["id"], "result":Bytes::from(result)})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let mut chain = effective_chain_for_rpc(endpoint.as_str(), 0);
    chain.rpc_route = RpcChainRoute::new(1, vec![endpoint]);
    let http = HttpContext::direct_for_tests();
    let client = http.operation_http_client().await.unwrap();
    assert_eq!(
        resolve_public_ens_recipient("RaFFY.eth", &chain, &http, &client)
            .await
            .unwrap(),
        target
    );
    assert!(
        resolve_public_ens_recipient("Unassigned.eth", &chain, &http, &client)
            .await
            .is_err()
    );
    assert!(
        resolve_public_ens_recipient("a\u{200d}b.eth", &chain, &http, &client)
            .await
            .is_err()
    );
    server.abort();
}
