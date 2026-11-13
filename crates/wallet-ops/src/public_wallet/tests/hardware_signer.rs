use super::*;

#[test]
fn hardware_typed_data_hash_fallback_confirmation_error_survives_context() {
    let session = HardwareProfileSession::matched(
        HardwareDeviceKind::Ledger,
        "profile-a",
        HardwareProfileBinding::evm_address_fingerprint("fingerprint-a"),
        None,
    );
    let error = eyre::Report::from(
        WalletConnectHardwareTypedDataHashFallbackConfirmationRequired::new(Some(session.clone())),
    )
    .wrap_err("WalletConnect eth_signTypedData_v4");

    assert!(is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required(&error));
    assert_eq!(
        walletconnect_hardware_typed_data_hash_fallback_confirmation_session(&error),
        Some(session)
    );
    assert_eq!(
        format!(
            "{:?}",
            error
                .downcast_ref::<WalletConnectHardwareTypedDataHashFallbackConfirmationRequired>()
                .expect("confirmation error")
        ),
        "WalletConnectHardwareTypedDataHashFallbackConfirmationRequired"
    );
}

fn hardware_typed_data_signer_with_mode(
    mode: HardwareTypedDataSigningMode,
) -> HardwarePublicEvmSigner {
    let descriptor =
        HardwarePublicAccountDescriptor::for_wallet_public_index(HardwareDeviceKind::Ledger, 0, 0)
            .expect("ledger descriptor");
    let mut hardware_session = HardwareProfileSession::unmatched(
        HardwareDeviceKind::Ledger,
        HardwareProfileBinding::evm_address_fingerprint(
            "ledger:evm:0x1111111111111111111111111111111111111111",
        ),
        None,
    );
    hardware_session
        .cache_typed_data_signing_mode(&descriptor, mode)
        .expect("cache typed-data mode");
    HardwarePublicEvmSigner {
        request_control: None,
        address: address!("0x1111111111111111111111111111111111111111"),
        descriptor,
        hardware_session: std::sync::Mutex::new(hardware_session),
        trezor_app_passphrase: std::sync::Mutex::new(None),
        trezor_pin_matrix_provider: None,
    }
}

fn hardware_typed_data_model_for_tests() -> HardwareEip712Model {
    HardwareEip712Model::from_walletconnect_typed_data_json(serde_json::json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "Message": [
                { "name": "contents", "type": "string" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "chainId": 1
        },
        "message": {
            "contents": "hello"
        }
    }))
    .expect("typed-data model")
}

#[test]
fn hardware_public_signer_requires_hash_fallback_confirmation_before_signing() {
    let signer =
        hardware_typed_data_signer_with_mode(HardwareTypedDataSigningMode::Eip712HashFallback);
    let model = hardware_typed_data_model_for_tests();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let error = runtime
        .block_on(signer.sign_typed_data_v4(
            &model,
            Some(HardwareTypedDataSigningMode::Eip712HashFallback),
            false,
        ))
        .expect_err("fallback confirmation required");

    assert!(is_walletconnect_hardware_typed_data_hash_fallback_confirmation_required(&error));
    assert_eq!(
        walletconnect_hardware_typed_data_hash_fallback_confirmation_session(&error)
            .and_then(|session| session.typed_data_signing_mode(&signer.descriptor)),
        Some(HardwareTypedDataSigningMode::Eip712HashFallback)
    );
}

#[cfg(not(feature = "hardware"))]
#[test]
fn hardware_public_signer_rejects_confirmed_hash_fallback_without_hardware_feature() {
    let signer =
        hardware_typed_data_signer_with_mode(HardwareTypedDataSigningMode::Eip712HashFallback);
    let model = hardware_typed_data_model_for_tests();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let error = runtime
        .block_on(signer.sign_typed_data_v4(
            &model,
            Some(HardwareTypedDataSigningMode::Eip712HashFallback),
            true,
        ))
        .expect_err("default build cannot sign fallback");

    assert!(
        error
            .to_string()
            .contains("hardware public signing is not enabled in this build")
    );
}

#[test]
fn hardware_typed_data_signature_recovery_mismatch_rejects() {
    let model = hardware_typed_data_model_for_tests();
    let signer = SoftwareEvmSigner::from_private_key([7u8; 32]).expect("software signer");
    let signature = signer
        .sign_typed_data_v4(model.typed_data())
        .expect("typed-data signature");

    let error = verify_hardware_typed_data_signature_address(
        address!("0x1111111111111111111111111111111111111111"),
        &signature,
        &model,
    )
    .expect_err("recovery mismatch");

    assert!(
        error
            .to_string()
            .contains("hardware public signer address mismatch")
    );
}

#[test]
fn hardware_public_signer_consumes_trezor_app_passphrase_once() {
    let mut hardware_session = HardwareProfileSession::unmatched(
        HardwareDeviceKind::Trezor,
        HardwareProfileBinding::evm_address_fingerprint(
            "trezor:evm:0x1111111111111111111111111111111111111111",
        ),
        Some(vec![1, 2, 3]),
    );
    hardware_session.set_trezor_passphrase_mode(TrezorPassphraseMode::EnterInApp);
    let signer = HardwarePublicEvmSigner {
        request_control: None,
        address: address!("0x1111111111111111111111111111111111111111"),
        descriptor: HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Trezor,
            0,
            0,
        )
        .expect("trezor descriptor"),
        hardware_session: std::sync::Mutex::new(hardware_session),
        trezor_app_passphrase: std::sync::Mutex::new(Some(Zeroizing::new("app secret".to_owned()))),
        trezor_pin_matrix_provider: None,
    };

    let passphrase = signer
        .take_trezor_app_passphrase()
        .expect("first passphrase take");
    assert_eq!(passphrase.as_str(), "app secret");
    assert!(signer.take_trezor_app_passphrase().is_none());
}

#[test]
fn hardware_public_signer_updates_in_memory_trezor_session_id_preserving_typed_data_mode() {
    let mut hardware_session = HardwareProfileSession::unmatched(
        HardwareDeviceKind::Trezor,
        HardwareProfileBinding::evm_address_fingerprint(
            "trezor:evm:0x1111111111111111111111111111111111111111",
        ),
        Some(vec![1, 2, 3]),
    );
    hardware_session.set_trezor_passphrase_mode(TrezorPassphraseMode::EnterInApp);
    let descriptor =
        HardwarePublicAccountDescriptor::for_wallet_public_index(HardwareDeviceKind::Trezor, 0, 0)
            .expect("trezor descriptor");
    hardware_session
        .cache_typed_data_signing_mode(&descriptor, HardwareTypedDataSigningMode::ClearSign)
        .expect("cache typed-data mode");
    let signer = HardwarePublicEvmSigner {
        request_control: None,
        address: address!("0x1111111111111111111111111111111111111111"),
        descriptor,
        hardware_session: std::sync::Mutex::new(hardware_session),
        trezor_app_passphrase: std::sync::Mutex::new(None),
        trezor_pin_matrix_provider: None,
    };

    signer
        .replace_trezor_session_id_if_trezor(Some(vec![4, 5, 6]))
        .expect("replace Trezor session id");
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .trezor_session_id,
        Some(vec![4, 5, 6])
    );
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .typed_data_signing_mode(&signer.descriptor),
        Some(HardwareTypedDataSigningMode::ClearSign)
    );
    signer
        .replace_trezor_session_id_if_trezor(None)
        .expect("clear Trezor session id");
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .trezor_session_id,
        None
    );
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .typed_data_signing_mode(&signer.descriptor),
        Some(HardwareTypedDataSigningMode::ClearSign)
    );
}

#[cfg(not(feature = "hardware"))]
#[test]
fn hardware_typed_data_probe_is_unsupported_without_hardware_feature() {
    let hardware_session = HardwareProfileSession::unmatched(
        HardwareDeviceKind::Ledger,
        HardwareProfileBinding::evm_address_fingerprint(
            "ledger:evm:0x1111111111111111111111111111111111111111",
        ),
        None,
    );
    let signer = HardwarePublicEvmSigner {
        request_control: None,
        address: address!("0x1111111111111111111111111111111111111111"),
        descriptor: HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Ledger,
            0,
            0,
        )
        .expect("ledger descriptor"),
        hardware_session: std::sync::Mutex::new(hardware_session),
        trezor_app_passphrase: std::sync::Mutex::new(None),
        trezor_pin_matrix_provider: None,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let mode = runtime
        .block_on(signer.typed_data_signing_mode())
        .expect("default-build typed-data mode");

    assert_eq!(mode, HardwareTypedDataSigningMode::Unsupported);
    assert_eq!(
        signer
            .hardware_session()
            .expect("hardware session")
            .typed_data_signing_mode(&signer.descriptor),
        Some(HardwareTypedDataSigningMode::Unsupported)
    );
}
