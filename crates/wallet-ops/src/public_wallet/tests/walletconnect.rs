use super::*;

#[test]
fn walletconnect_personal_sign_uses_spend_authorized_public_signer() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("WalletConnect signer"),
            false,
        )
        .expect("import public account");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let denied = runtime.block_on(walletconnect_sign_personal_message(
        WalletConnectPersonalSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: None,
            view_session: Arc::clone(&view_session),
            vault_store: Arc::clone(&store),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new("wrong password".to_owned()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: account.public_account_uuid.clone(),
            message: b"hello".to_vec(),
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    ));
    assert!(denied.is_err());

    let signature = runtime
        .block_on(walletconnect_sign_personal_message(
            WalletConnectPersonalSignRequest {
                executor_owner: None,
                chain_id: 1,
                request_control: None,
                view_session: Arc::clone(&view_session),
                vault_store: Arc::clone(&store),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid,
                message: b"hello".to_vec(),
                event_tx: None,
            },
            crate::OperationHttpClient::direct_for_tests(),
        ))
        .expect("personal sign");

    assert!(signature.starts_with("0x"));
    assert_eq!(signature.len(), 132);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

/// Signs `typed_data` through `eth_signTypedData_v4` with an imported software Public account
/// and checks that a 65-byte signature comes back.
fn assert_software_public_account_signs_typed_data(typed_data: Value) {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("WalletConnect typed data"),
            false,
        )
        .expect("import public account");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let signature = runtime
        .block_on(walletconnect_sign_typed_data_v4(
            WalletConnectTypedDataSignRequest {
                executor_owner: None,
                chain_id: 1,
                request_control: None,
                view_session: Arc::clone(&view_session),
                vault_store: Arc::clone(&store),
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        None,
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: account.public_account_uuid,
                typed_data,
                hash_fallback_confirmed: false,
                event_tx: None,
            },
            crate::OperationHttpClient::direct_for_tests(),
        ))
        .expect("typed-data sign");

    assert!(signature.starts_with("0x"));
    assert_eq!(signature.len(), 132);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn walletconnect_typed_data_signs_for_software_public_account() {
    let typed_data = serde_json::json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "version", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "Message": [
                { "name": "contents", "type": "string" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "version": "1",
            "chainId": 1
        },
        "message": {
            "contents": "hello"
        }
    });
    assert_software_public_account_signs_typed_data(typed_data);
}

#[test]
fn walletconnect_typed_data_signs_primitive_prefixed_custom_types_for_software_public_account() {
    let typed_data = serde_json::json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "bytesPayload": [
                { "name": "digest", "type": "bytes32" }
            ],
            "Message": [
                { "name": "payload", "type": "bytesPayload" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "chainId": 1
        },
        "message": {
            "payload": {
                "digest": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        }
    });
    assert_software_public_account_signs_typed_data(typed_data);
}

#[test]
fn walletconnect_typed_data_signing_error_preserves_method_label() {
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("WalletConnect typed data diagnostics"),
            false,
        )
        .expect("import public account");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    for method in [
        WalletConnectSupportedMethod::EthSignTypedData,
        WalletConnectSupportedMethod::EthSignTypedDataV4,
    ] {
        let error = runtime
            .block_on(walletconnect_sign_typed_data(
                WalletConnectTypedDataSignRequest {
                    executor_owner: None,
                    chain_id: 1,
                    request_control: None,
                    view_session: Arc::clone(&view_session),
                    vault_store: Arc::clone(&store),
                    authorization: Some(
                        crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                            Zeroizing::new("wrong password".to_owned()),
                            None,
                        ),
                    ),
                    trezor_app_passphrase: None,
                    trezor_pin_matrix_provider: None,
                    public_account_uuid: account.public_account_uuid.clone(),
                    typed_data: serde_json::json!({}),
                    hash_fallback_confirmed: false,
                    event_tx: None,
                },
                method,
                crate::OperationHttpClient::direct_for_tests(),
            ))
            .expect_err("typed-data signing should fail with the wrong password");
        let message = error
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(": ");
        assert!(message.contains(method.as_str()));
        if method == WalletConnectSupportedMethod::EthSignTypedData {
            assert!(!message.contains(WalletConnectSupportedMethod::EthSignTypedDataV4.as_str()));
        } else {
            assert!(message.contains(WalletConnectSupportedMethod::EthSignTypedDataV4.as_str()));
        }
    }

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn walletconnect_decoded_intents_use_only_the_fixed_operation_table() {
    let address = address!("0x3333333333333333333333333333333333333333");
    let amount = U256::from(7_u64);
    let cases = [
        (
            WalletConnectDecodedCallKind::NativeTransfer,
            PUBLIC_NATIVE_SEND_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::Erc20Transfer {
                recipient: address,
                amount,
            },
            PUBLIC_ERC20_SEND_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::Erc20TransferFrom {
                from: address,
                to: address,
                amount,
            },
            PUBLIC_ERC20_SEND_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::Erc20Approve {
                spender: address,
                amount,
            },
            PUBLIC_NATIVE_APPROVE_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::WrappedDeposit,
            PUBLIC_NATIVE_WRAP_GAS_UNITS,
        ),
        (
            WalletConnectDecodedCallKind::WrappedWithdraw { amount },
            PUBLIC_NATIVE_UNWRAP_GAS_UNITS,
        ),
    ];
    for (kind, expected) in cases {
        assert_eq!(
            public_native_action_gas_units_from_walletconnect_intent(&kind),
            Some(expected)
        );
    }
    assert_eq!(
        public_native_action_gas_units_from_walletconnect_intent(
            &WalletConnectDecodedCallKind::ContractCall { selector: None },
        ),
        None
    );
    assert_eq!(
        public_native_action_gas_units_from_walletconnect_intent(
            &WalletConnectDecodedCallKind::ContractCreation,
        ),
        None
    );
    assert_eq!(
        public_walletconnect_operation_gas_limit(
            &WalletConnectDecodedCallKind::NativeTransfer,
            123,
        ),
        Some(PUBLIC_NATIVE_SEND_GAS_UNITS + 123)
    );
    assert_eq!(
        public_walletconnect_operation_gas_limit(
            &WalletConnectDecodedCallKind::ContractCreation,
            123,
        ),
        None
    );
}

#[test]
fn walletconnect_fee_projection_keeps_raw_buffered_source_and_optional_usd() {
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(200),
        suggested_max_fee_per_gas: 120,
        suggested_max_priority_fee_per_gas: 3,
    };
    let resolved = PublicActionResolvedGasFee {
        rpc_gas_price: 100,
        max_fee_per_gas: 120,
        max_priority_fee_per_gas: 3,
    };
    let projection = project_public_action_fee(
        100,
        110,
        quote,
        resolved,
        PublicActionFeeSource::OperationTable,
        Some(U256::from(2_000_000_u64)),
        18,
    );
    assert_eq!(projection.expected_fee_per_gas, 120);
    assert_eq!(projection.expected_gas_cost, U256::from(12_000));
    assert_eq!(projection.maximum_gas_cost, U256::from(13_200));
    assert_eq!(projection.source, PublicActionFeeSource::OperationTable);
    assert_eq!(projection.expected_native_usd_micro_value, Some(U256::ZERO));
    assert_eq!(
        crate::expected_eip1559_fee_per_gas(quote, 120, 3),
        120,
        "fee history caps the projected effective fee at the selected maximum"
    );
    assert_eq!(
        crate::expected_eip1559_fee_per_gas(
            PublicActionGasFeeQuote {
                current_base_fee_per_gas: None,
                ..quote
            },
            120,
            3,
        ),
        120,
        "gas-price fallback uses the selected maximum as the expected fee"
    );
    let estimate = PublicAdvancedTransactionEstimate {
        payload_fingerprint: B256::ZERO,
        raw_gas_limit: 100,
        gas_limit: 110,
        max_fee_per_gas: 120,
        max_priority_fee_per_gas: 3,
        expected_fee_per_gas: 120,
        expected_gas_cost: U256::from(12_000_u64),
        max_gas_cost: U256::from(13_200_u64),
    };
    let custom_precision = estimate.fee_projection(Some(U256::from(2_000_000)), 6);
    assert_eq!(
        custom_precision.expected_native_usd_micro_value,
        Some(U256::from(24_000))
    );
    assert_eq!(
        custom_precision.maximum_native_usd_micro_value,
        Some(U256::from(26_400))
    );
    let estimate_projection =
        estimate.fee_projection(Some(U256::from(1_000_000_000_000_000_000_u128)), 18);
    assert_eq!(
        estimate_projection.source,
        PublicActionFeeSource::NetworkSimulation
    );
    assert_eq!(estimate_projection.raw_gas_limit, estimate.raw_gas_limit);
    assert_eq!(estimate_projection.gas_limit, estimate.gas_limit);
    assert_eq!(
        estimate_projection.expected_fee_per_gas,
        estimate.expected_fee_per_gas
    );
    assert_eq!(
        estimate_projection.expected_gas_cost,
        estimate.expected_gas_cost
    );
    assert_eq!(estimate_projection.maximum_gas_cost, estimate.max_gas_cost);
    assert_eq!(
        estimate_projection.expected_native_usd_micro_value,
        Some(estimate.expected_gas_cost)
    );
    assert_eq!(
        estimate_projection.maximum_native_usd_micro_value,
        Some(estimate.max_gas_cost)
    );
    assert!(public_action_maximum_gas_cost_is_significant(
        projection.expected_gas_cost,
        projection.maximum_gas_cost,
    ));

    assert!(!public_action_maximum_gas_cost_is_significant(
        U256::from(100_u64),
        U256::from(100_u64),
    ));
    assert!(!public_action_maximum_gas_cost_is_significant(
        U256::from(100_u64),
        U256::from(109_u64),
    ));
    assert!(public_action_maximum_gas_cost_is_significant(
        U256::ZERO,
        U256::from(1_u64),
    ));
}

#[test]
fn walletconnect_reviewed_fingerprint_rejects_changed_payload() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let to = address!("0x2222222222222222222222222222222222222222");
    let request = TransactionRequest::default()
        .with_from(from)
        .with_to(to)
        .with_value(U256::from(1_u64));
    let fingerprint = walletconnect_transaction_payload_fingerprint(1, from, &request);
    assert!(
        validate_walletconnect_reviewed_transaction(1, from, &request, fingerprint, 55).is_ok()
    );
    let changed = request.clone().with_value(U256::from(2_u64));
    assert!(
        validate_walletconnect_reviewed_transaction(1, from, &changed, fingerprint, 55).is_err()
    );
    assert!(
        validate_walletconnect_reviewed_transaction(1, from, &request, fingerprint, 0).is_err()
    );
}

#[test]
fn walletconnect_custom_fee_resolves_without_an_automatic_quote() {
    let custom = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 17,
        max_priority_fee_per_gas: 3,
    };
    let resolved = super::gas::resolve_public_action_gas_fee(
        1,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        custom,
        None,
    )
    .expect("custom WalletConnect fee does not need an automatic quote");
    assert_eq!(resolved.max_fee_per_gas, 17);
    assert_eq!(resolved.max_priority_fee_per_gas, 3);
    assert!(
        super::gas::resolve_public_action_gas_fee(
            1,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            PublicActionGasFeeSelection::Auto,
            None,
        )
        .is_err()
    );
}

#[test]
fn walletconnect_transaction_sanitizer_discards_dapp_envelope_and_preserves_access_list() {
    let from = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let access_list = AccessList::default();
    let request = TransactionRequest {
        from: Some(from),
        to: Some(recipient.into()),
        gas_price: Some(9),
        gas: Some(21_000),
        nonce: Some(4),
        max_fee_per_gas: Some(99),
        max_priority_fee_per_gas: Some(3),
        transaction_type: Some(1),
        access_list: Some(access_list.clone()),
        ..Default::default()
    };
    let sanitized = sanitize_walletconnect_transaction_request(request, 1, from);
    assert_eq!(sanitized.to, Some(recipient.into()));
    assert_eq!(sanitized.gas, None);
    assert_eq!(sanitized.gas_price, None);
    assert_eq!(sanitized.max_fee_per_gas, None);
    assert_eq!(sanitized.max_priority_fee_per_gas, None);
    assert_eq!(sanitized.nonce, None);
    assert_eq!(sanitized.transaction_type, None);
    assert_eq!(sanitized.access_list, Some(access_list));
}

#[test]
fn passphrase_walletconnect_signer_requires_the_active_protected_session() {
    let (root_dir, db, store, base_view_session) = public_action_request_parts();
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("context creation grant");
    let CreateSoftwareContextResult::Created {
        public_account,
        protected_seed_session,
        ..
    } = store
        .create_software_context(
            &base_view_session.clone_vault_view_unlock(),
            &mut grant,
            base_view_session.wallet_id(),
            "walletconnect-passphrase",
            0,
            "WalletConnect passphrase",
            Zeroizing::new("TREZOR".to_owned()),
            Zeroizing::new("TREZOR".to_owned()),
            SoftwareContextSyncIntent::RecoverExisting,
            &[SoftwareContextChainInput {
                chain_type: 0,
                chain_id: 1,
                contract: "0xcontract".to_owned(),
                deployment_block: 0,
                current_safe_head: None,
            }],
            VaultSessionId::from_bytes([31; 16]),
        )
        .expect("create passphrase context")
    else {
        panic!("expected created context");
    };
    let view_session = Arc::new(
        store
            .load_view_session(TEST_PASSWORD, "walletconnect-passphrase")
            .expect("passphrase view session"),
    );
    let protected_seed_session = Arc::new(protected_seed_session);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let missing = runtime.block_on(walletconnect_sign_personal_message(
        WalletConnectPersonalSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: None,
            view_session: Arc::clone(&view_session),
            vault_store: Arc::clone(&store),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.to_owned()),
                    None,
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: public_account.public_account_uuid.clone(),
            message: b"passphrase session".to_vec(),
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    ));
    assert!(missing.is_err());

    let wrong_session = {
        let seed = bip39_seed_from_mnemonic(TEST_MNEMONIC, "TREZOR").expect("context seed");
        let wrong_grant = store
            .create_spend_grant(TEST_PASSWORD)
            .expect("wrong-session grant");
        Arc::new(
            wrong_grant
                .spend_unlock()
                .expect("wrong-session spend unlock")
                .seal_software_seed_session(
                    SoftwareSeedSessionBinding::new(
                        base_view_session.wallet_id(),
                        "wrong-context",
                        VaultSessionId::from_bytes([32; 16]),
                    ),
                    seed.as_ref(),
                )
                .expect("wrong protected session"),
        )
    };
    let wrong = runtime.block_on(walletconnect_sign_personal_message(
        WalletConnectPersonalSignRequest {
            executor_owner: None,
            chain_id: 1,
            request_control: None,
            view_session: Arc::clone(&view_session),
            vault_store: Arc::clone(&store),
            authorization: Some(
                crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                    Zeroizing::new(TEST_PASSWORD.to_owned()),
                    Some(wrong_session),
                ),
            ),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: public_account.public_account_uuid.clone(),
            message: b"passphrase session".to_vec(),
            event_tx: None,
        },
        crate::OperationHttpClient::direct_for_tests(),
    ));
    assert!(wrong.is_err());

    let signature = runtime
        .block_on(walletconnect_sign_personal_message(
            WalletConnectPersonalSignRequest {
                executor_owner: None,
                chain_id: 1,
                request_control: None,
                view_session,
                vault_store: store,
                authorization: Some(
                    crate::DesktopPrivateSpendAuthorization::from_software_credentials(
                        Zeroizing::new(TEST_PASSWORD.to_owned()),
                        Some(protected_seed_session),
                    ),
                ),
                trezor_app_passphrase: None,
                trezor_pin_matrix_provider: None,
                public_account_uuid: public_account.public_account_uuid,
                message: b"passphrase session".to_vec(),
                event_tx: None,
            },
            crate::OperationHttpClient::direct_for_tests(),
        ))
        .expect("passphrase WalletConnect signature");
    assert!(signature.starts_with("0x"));

    drop(base_view_session);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
