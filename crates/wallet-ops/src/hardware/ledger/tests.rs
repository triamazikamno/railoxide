use super::*;
use serde_json::json;

#[tokio::test]
async fn signing_dispatch_rechecks_after_queueing_and_device_preparation() {
    use crate::{RpcBrokerError, dapp_request::DappRequestControl};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;
    use tokio::time::{Instant, timeout};

    let writes = Arc::new(AtomicUsize::new(0));
    let control = DappRequestControl::new(Instant::now() + Duration::from_secs(30), || Ok(()));
    let lock = LEDGER_IO_LOCK.lock().await;
    let queued = ledger_signing_dispatch(Some(control.clone()), || Ok(()), {
        let writes = writes.clone();
        move |()| {
            writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    tokio::pin!(queued);
    assert!(futures_util::poll!(queued.as_mut()).is_pending());
    control.invalidate(&RpcBrokerError::OriginRejected);
    drop(lock);
    assert!(matches!(
        timeout(Duration::from_secs(2), queued).await.unwrap(),
        Err(HardwareDerivationError::RequestAuthority(
            RpcBrokerError::OriginRejected
        ))
    ));

    let control = DappRequestControl::new(Instant::now() + Duration::from_secs(30), || Ok(()));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let preparing = tokio::spawn(ledger_signing_dispatch(
        Some(control.clone()),
        move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        },
        {
            let writes = writes.clone();
            move |()| {
                writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        },
    ));
    timeout(Duration::from_secs(2), entered_rx)
        .await
        .unwrap()
        .unwrap();
    control.invalidate(&RpcBrokerError::Shutdown);
    release_tx.send(()).unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(2), preparing)
            .await
            .unwrap()
            .unwrap(),
        Err(HardwareDerivationError::RequestAuthority(
            RpcBrokerError::Shutdown
        ))
    ));
    assert_eq!(writes.load(Ordering::SeqCst), 0);

    // A later signing APDU or fallback attempt must re-enter admission after preparation.
    let control = DappRequestControl::new(Instant::now() + Duration::from_secs(30), || Ok(()));
    ledger_signing_dispatch(Some(control.clone()), || Ok(()), |()| Ok(()))
        .await
        .unwrap();
    control.invalidate(&RpcBrokerError::OriginRejected);
    let result = ledger_signing_dispatch(Some(control), || Ok(()), {
        let writes = writes.clone();
        move |()| {
            writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    })
    .await;
    let error = result.unwrap_err();
    assert_eq!(
        classify_ledger_eip712_clear_failure(&error, &ledger_sign_apdu(), false),
        LedgerEip712ClearFailureKind::Other
    );
    assert_eq!(writes.load(Ordering::SeqCst), 0);
}

fn answer_with_status(status: u16) -> APDUAnswer {
    APDUAnswer::from_answer(status.to_be_bytes().to_vec()).expect("status answer")
}

#[test]
fn ledger_hid_preflight_filter_matches_coins_ledger_filter() {
    #[cfg(not(target_os = "linux"))]
    {
        assert!(ledger_hid_device_matches(0x2c97, 0xffa0));
        assert!(!ledger_hid_device_matches(0x2c97, 0x0001));
    }
    #[cfg(target_os = "linux")]
    {
        assert!(ledger_hid_device_matches(0x2c97, 0x0001));
    }
    assert!(!ledger_hid_device_matches(0x1234, 0xffa0));
}

#[test]
fn ledger_connect_device_not_found_preserves_retry_guidance() {
    let error = HardwareDerivationError::LedgerUnavailable(LEDGER_READY_MESSAGE);

    assert!(matches!(
        error,
        HardwareDerivationError::LedgerUnavailable(LEDGER_READY_MESSAGE)
    ));
    assert!(error.to_string().contains("enter your PIN"));
    assert!(error.to_string().contains("open the Ethereum app"));
}

#[test]
fn ledger_app_closed_status_points_to_ethereum_app() {
    let error = ledger_response_data(&answer_with_status(0x6511), "get Ethereum address")
        .expect_err("app closed status should fail");

    assert!(matches!(
        error,
        HardwareDerivationError::LedgerStatus {
            operation: "get Ethereum address",
            status: 0x6511,
            ..
        }
    ));
    let message = error.to_string();
    assert!(message.contains("0x6511"));
    assert!(message.contains("Open the Ethereum app on your Ledger"));
}

#[test]
fn ledger_known_bad_retcode_points_to_ethereum_app() {
    let error = ledger_exchange_error(
        LedgerError::BadRetcode(coins_ledger::common::APDUResponseCodes::InsNotSupported),
        "get Ethereum app version",
    );

    assert!(matches!(
        error,
        HardwareDerivationError::LedgerStatus {
            operation: "get Ethereum app version",
            status: 0x6d00,
            ..
        }
    ));
    assert!(
        error
            .to_string()
            .contains("Open the Ethereum app on your Ledger")
    );
}

#[test]
fn ledger_locked_status_points_to_pin_entry() {
    let error = ledger_response_data(&answer_with_status(0x6b0c), "get Ethereum address")
        .expect_err("locked status should fail");

    assert!(matches!(
        error,
        HardwareDerivationError::LedgerStatus { status: 0x6b0c, .. }
    ));
    assert!(error.to_string().contains("Enter your PIN"));
}

#[test]
fn ledger_address_confirmation_sets_display_flag() {
    assert_eq!(ledger_address_display_p1(false), 0x00);
    assert_eq!(ledger_address_display_p1(true), 0x01);
}

#[test]
fn ledger_account_public_key_command_requests_chain_code_without_display() {
    let path = hardware_public_account_key_path(0).expect("account key path");
    assert_eq!(path, [0x8000_002c, 0x8000_003c, 0x8000_0000, 0]);
    let command = ledger_account_public_key_command(&path).expect("account key command");
    assert_eq!(
        (command.cla, command.ins, command.p1, command.p2),
        (0xe0, 0x02, 0x00, 0x01)
    );
    assert_eq!(
        *command.data,
        ledger_path_payload(&path).expect("path payload")
    );
}

#[test]
fn ledger_account_public_key_response_parses_key_and_chain_code() {
    use crate::hardware::{TEST_ACCOUNT_CHAIN_CODE, TEST_ACCOUNT_PUBLIC_KEY};

    let mut response = vec![65];
    response.extend_from_slice(&TEST_ACCOUNT_PUBLIC_KEY);
    response.push(40);
    response.extend_from_slice(b"1111111111111111111111111111111111111111");
    response.extend_from_slice(&TEST_ACCOUNT_CHAIN_CODE);

    let key = ledger_account_public_key_from_response(&response).expect("account key");
    assert_eq!(key.public_key, TEST_ACCOUNT_PUBLIC_KEY);
    assert_eq!(key.chain_code, TEST_ACCOUNT_CHAIN_CODE);
    assert_eq!(
        ledger_address_from_response(&response).expect("address"),
        "1111111111111111111111111111111111111111"
    );

    // Empty, inside the public key, inside the address, inside the chain code.
    for len in [0, 30, 87, response.len() - 1] {
        assert!(
            matches!(
                ledger_account_public_key_from_response(&response[..len]),
                Err(HardwareDerivationError::UnexpectedHardwareResponse(_))
            ),
            "response cut at {len}"
        );
    }
}

#[test]
fn ledger_typed_data_classification_is_conservative() {
    let clear = LedgerDeviceInfo {
        model: LedgerDeviceModel::NanoX,
        ethereum_app_version: HardwareAppVersion::new(1, 10, 0),
    };
    let fallback = LedgerDeviceInfo {
        model: LedgerDeviceModel::NanoS,
        ethereum_app_version: HardwareAppVersion::new(1, 5, 0),
    };
    let unknown_model = LedgerDeviceInfo {
        model: LedgerDeviceModel::Unknown,
        ethereum_app_version: HardwareAppVersion::new(1, 10, 0),
    };
    let unsupported = LedgerDeviceInfo {
        model: LedgerDeviceModel::NanoX,
        ethereum_app_version: HardwareAppVersion::new(1, 4, 9),
    };

    assert_eq!(
        classify_ledger_typed_data_signing_mode(&clear),
        HardwareTypedDataSigningMode::ClearSign
    );
    assert_eq!(
        classify_ledger_typed_data_signing_mode(&fallback),
        HardwareTypedDataSigningMode::Eip712HashFallback
    );
    assert_eq!(
        classify_ledger_typed_data_signing_mode(&unknown_model),
        HardwareTypedDataSigningMode::Eip712HashFallback
    );
    assert_eq!(
        classify_ledger_typed_data_signing_mode(&unsupported),
        HardwareTypedDataSigningMode::Unsupported
    );
}

fn ledger_clear_info() -> LedgerDeviceInfo {
    LedgerDeviceInfo {
        model: LedgerDeviceModel::NanoX,
        ethereum_app_version: HardwareAppVersion::new(1, 10, 0),
    }
}

fn ledger_no_hash_fallback_info() -> LedgerDeviceInfo {
    LedgerDeviceInfo {
        model: LedgerDeviceModel::NanoX,
        ethereum_app_version: HardwareAppVersion::new(1, 4, 9),
    }
}

fn ledger_definition_apdu() -> LedgerEip712Apdu {
    LedgerEip712Apdu {
        ins: 0x1a,
        p1: 0x00,
        p2: 0x00,
        data: b"Message".to_vec(),
    }
}

fn ledger_sign_apdu() -> LedgerEip712Apdu {
    LedgerEip712Apdu {
        ins: 0x0c,
        p1: 0x00,
        p2: 0x01,
        data: vec![0],
    }
}

#[test]
fn ledger_clear_failure_downgrades_only_for_pre_confirmation_capability_rejection() {
    let error = ledger_status_error("sign EIP-712 typed data", 0x6d00);
    let failed_apdu = ledger_definition_apdu();

    assert_eq!(
        classify_ledger_eip712_clear_failure(&error, &failed_apdu, false),
        LedgerEip712ClearFailureKind::CapabilityUnsupportedBeforeConfirmation
    );
    assert_eq!(
        ledger_eip712_clear_failure_downgrade_mode(
            &error,
            &failed_apdu,
            false,
            ledger_clear_info()
        ),
        Some(HardwareTypedDataSigningMode::Eip712HashFallback)
    );
    assert_eq!(
        ledger_eip712_clear_failure_downgrade_mode(
            &error,
            &failed_apdu,
            false,
            ledger_no_hash_fallback_info()
        ),
        None
    );
}

#[test]
fn ledger_clear_failure_does_not_downgrade_payload_specific_rejection() {
    let error = ledger_status_error("sign EIP-712 typed data", 0x6a80);
    let failed_apdu = ledger_definition_apdu();

    assert_eq!(
        classify_ledger_eip712_clear_failure(&error, &failed_apdu, false),
        LedgerEip712ClearFailureKind::PayloadUnsupported
    );
    assert_eq!(
        ledger_eip712_clear_failure_downgrade_mode(
            &error,
            &failed_apdu,
            false,
            ledger_clear_info()
        ),
        None
    );
}

#[test]
fn ledger_clear_failure_does_not_downgrade_signing_or_user_rejection() {
    let unsupported_error = ledger_status_error("sign EIP-712 typed data", 0x6d00);
    let rejected_error = ledger_status_error("sign EIP-712 typed data", 0x6985);
    let sign_apdu = ledger_sign_apdu();
    let definition_apdu = ledger_definition_apdu();

    assert_eq!(
        classify_ledger_eip712_clear_failure(&unsupported_error, &sign_apdu, false),
        LedgerEip712ClearFailureKind::Other
    );
    assert_eq!(
        classify_ledger_eip712_clear_failure(&unsupported_error, &definition_apdu, true),
        LedgerEip712ClearFailureKind::Other
    );
    assert_eq!(
        classify_ledger_eip712_clear_failure(&rejected_error, &definition_apdu, false),
        LedgerEip712ClearFailureKind::UserRejected
    );
    assert_eq!(
        ledger_eip712_clear_failure_downgrade_mode(
            &rejected_error,
            &definition_apdu,
            false,
            ledger_clear_info()
        ),
        None
    );
}

#[test]
fn ledger_device_model_uses_product_string_before_product_id() {
    assert_eq!(
        ledger_device_model(0xffff, Some("Ledger Nano X")),
        LedgerDeviceModel::NanoX
    );
    assert_eq!(
        ledger_device_model(0x0005, None),
        LedgerDeviceModel::NanoSPlus
    );
    assert_eq!(
        ledger_device_model(0xffff, None),
        LedgerDeviceModel::Unknown
    );
}

fn typed_data_model() -> HardwareEip712Model {
    HardwareEip712Model::from_walletconnect_typed_data_json(json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "chainId", "type": "uint256" }
            ],
            "Person": [
                { "name": "wallet", "type": "address" },
                { "name": "name", "type": "string" }
            ],
            "Message": [
                { "name": "count", "type": "uint256" },
                { "name": "people", "type": "Person[]" },
                { "name": "tags", "type": "string[2]" },
                { "name": "payload", "type": "bytes" },
                { "name": "digest", "type": "bytes32" }
            ]
        },
        "primaryType": "Message",
        "domain": {
            "name": "RailOxide",
            "chainId": 1
        },
        "message": {
            "count": 128,
            "people": [
                {
                    "wallet": "0x1111111111111111111111111111111111111111",
                    "name": "Alice"
                }
            ],
            "tags": ["permit", "test"],
            "payload": "0x010203",
            "digest": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }
    }))
    .expect("typed-data model")
}

fn reordered_domain_typed_data_model() -> HardwareEip712Model {
    HardwareEip712Model::from_walletconnect_typed_data_json(json!({
        "types": {
            "EIP712Domain": [
                { "name": "chainId", "type": "uint256" },
                { "name": "name", "type": "string" }
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

fn long_string_typed_data_model() -> HardwareEip712Model {
    HardwareEip712Model::from_walletconnect_typed_data_json(json!({
        "types": {
            "EIP712Domain": [],
            "Message": [
                { "name": "contents", "type": "string" }
            ]
        },
        "primaryType": "Message",
        "domain": {},
        "message": {
            "contents": "x".repeat(300)
        }
    }))
    .expect("long string typed-data model")
}

fn ledger_descriptor() -> HardwarePublicAccountDescriptor {
    HardwarePublicAccountDescriptor::for_wallet_public_index(HardwareDeviceKind::Ledger, 0, 0)
        .expect("ledger descriptor")
}

#[test]
fn ledger_eip712_definition_apdus_encode_doc_type_descriptors() {
    let model = typed_data_model();
    let definitions = ledger_eip712_definition_apdus(&model).expect("definitions");

    assert_eq!(definitions[0].ins, 0x1a);
    assert_eq!(definitions[0].p2, 0x00);
    assert_eq!(definitions[0].data, b"EIP712Domain");

    let count = definitions
        .iter()
        .find(|apdu| apdu.p2 == 0xff && apdu.data.ends_with(b"\x05count"))
        .expect("count field definition");
    assert_eq!(count.data, [0x42, 32, 5, b'c', b'o', b'u', b'n', b't']);

    let people = definitions
        .iter()
        .find(|apdu| apdu.p2 == 0xff && apdu.data.ends_with(b"\x06people"))
        .expect("people field definition");
    assert_eq!(
        people.data,
        [
            0x80, 6, b'P', b'e', b'r', b's', b'o', b'n', 1, 0, 6, b'p', b'e', b'o', b'p', b'l',
            b'e'
        ]
    );

    let tags = definitions
        .iter()
        .find(|apdu| apdu.p2 == 0xff && apdu.data.ends_with(b"\x04tags"))
        .expect("tags field definition");
    assert_eq!(tags.data, [0x85, 1, 1, 2, 4, b't', b'a', b'g', b's']);
}

#[test]
fn ledger_eip712_implementation_apdus_traverse_values_in_order() {
    let model = typed_data_model();
    let implementations = ledger_eip712_implementation_apdus(&model).expect("implementations");

    assert_eq!(implementations[0].ins, 0x1c);
    assert_eq!(implementations[0].p1, 0x00);
    assert_eq!(implementations[0].p2, 0x00);
    assert_eq!(implementations[0].data, b"EIP712Domain");
    assert_eq!(implementations[1].p1, 0x00);
    assert_eq!(implementations[1].p2, 0xff);
    assert_eq!(implementations[1].data, b"\x00\x09RailOxide");
    assert_eq!(implementations[2].p1, 0x00);
    assert_eq!(implementations[2].p2, 0xff);
    assert_eq!(implementations[2].data, b"\x00\x01\x01");

    let message_root = implementations
        .iter()
        .position(|apdu| apdu.p2 == 0x00 && apdu.data == b"Message")
        .expect("message root");
    assert_eq!(implementations[message_root + 1].data, b"\x00\x01\x80");
    assert_eq!(implementations[message_root + 2].p1, 0x00);
    assert_eq!(implementations[message_root + 2].p2, 0x0f);
    assert_eq!(implementations[message_root + 2].data, [1]);
    assert!(implementations.iter().all(|apdu| apdu.p1 == 0x00));
}

#[test]
fn ledger_eip712_domain_values_follow_canonical_hash_order() {
    let model = reordered_domain_typed_data_model();
    let definitions = ledger_eip712_definition_apdus(&model).expect("definitions");
    let implementations = ledger_eip712_implementation_apdus(&model).expect("implementations");

    assert_eq!(definitions[0].data, b"EIP712Domain");
    assert!(definitions[1].data.ends_with(b"\x04name"));
    assert!(definitions[2].data.ends_with(b"\x07chainId"));
    assert_eq!(implementations[0].data, b"EIP712Domain");
    assert_eq!(implementations[1].data, b"\x00\x09RailOxide");
    assert_eq!(implementations[2].data, b"\x00\x01\x01");
}

#[test]
fn ledger_eip712_field_values_are_chunked_without_truncation() {
    let model = long_string_typed_data_model();
    let implementations = ledger_eip712_implementation_apdus(&model).expect("implementations");
    let chunks: Vec<_> = implementations
        .iter()
        .filter(|apdu| apdu.p2 == 0xff)
        .collect();

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].p1, 0x01);
    assert_eq!(chunks[0].data.len(), 255);
    assert_eq!(chunks[0].data[..2], [0x01, 0x2c]);
    assert_eq!(chunks[1].p1, 0x00);
    assert_eq!(chunks[1].data.len(), 47);
}

#[test]
fn ledger_eip712_hash_fallback_payload_uses_local_hashes() {
    let model = typed_data_model();
    let descriptor = ledger_descriptor();

    let apdu = ledger_eip712_hash_signing_apdu(&descriptor, &model).expect("hash apdu");

    let path = ledger_path_payload(&descriptor.path).expect("path payload");
    assert_eq!(apdu.ins, 0x0c);
    assert_eq!(apdu.p1, 0x00);
    assert_eq!(apdu.p2, 0x00);
    assert_eq!(apdu.data.len(), path.len() + 64);
    assert_eq!(&apdu.data[..path.len()], path.as_slice());
    assert_eq!(
        &apdu.data[path.len()..path.len() + 32],
        model.domain_separator_hash().as_slice()
    );
    assert_eq!(
        &apdu.data[path.len() + 32..],
        model.message_hash().expect("message hash").as_slice()
    );
}

#[test]
fn ledger_eip712_clear_sequence_sends_definitions_implementations_then_sign() {
    let model = typed_data_model();
    let descriptor = ledger_descriptor();

    let apdus = ledger_eip712_clear_signing_apdus(&descriptor, &model).expect("clear apdus");

    assert!(apdus.iter().take_while(|apdu| apdu.ins == 0x1a).count() > 0);
    let first_implementation = apdus
        .iter()
        .position(|apdu| apdu.ins == 0x1c)
        .expect("implementation APDU");
    let final_apdu = apdus.last().expect("final sign APDU");
    assert!(
        apdus[..first_implementation]
            .iter()
            .all(|apdu| apdu.ins == 0x1a)
    );
    assert!(
        apdus[first_implementation..apdus.len() - 1]
            .iter()
            .all(|apdu| apdu.ins == 0x1c)
    );
    assert_eq!(final_apdu.ins, 0x0c);
    assert_eq!(final_apdu.p2, 0x01);
    assert_eq!(
        final_apdu.data,
        ledger_path_payload(&descriptor.path).expect("path")
    );
}
