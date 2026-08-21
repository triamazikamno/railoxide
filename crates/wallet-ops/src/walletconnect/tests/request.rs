use std::collections::BTreeMap;

use alloy::primitives::{U256, address};
use serde_json::json;

use crate::hardware::HardwareTypedDataSigningMode;
use crate::vault::{
    PublicAccountScope, PublicAccountSource, WalletConnectRelayIdentity,
    WalletConnectSessionAccountResolution,
};
use crate::walletconnect::{
    WalletConnectDecodedCallKind, WalletConnectDecodedTransaction, WalletConnectError,
    WalletConnectNamespaceAccountSupport, WalletConnectParsedRequest,
    WalletConnectPendingRequestQueue, approve_walletconnect_session,
    approve_walletconnect_session_with_account_support, parse_walletconnect_session_request,
    validate_walletconnect_session_request,
    validate_walletconnect_session_request_with_account_support,
};

use super::helpers::{
    NOW, approved_request_session, namespace, supported_chains, test_proposal, test_public_account,
    typed_data_payload,
};

#[test]
fn parses_supported_requests_and_rejects_unsafe_methods() {
    assert!(matches!(
        parse_walletconnect_session_request(1, "eth_accounts", &json!([])).unwrap(),
        WalletConnectParsedRequest::EthAccounts
    ));
    assert!(matches!(
        parse_walletconnect_session_request(
            2,
            "personal_sign",
            &json!(["0x68656c6c6f", "0x1111111111111111111111111111111111111111"]),
        )
        .unwrap(),
        WalletConnectParsedRequest::PersonalSign { .. }
    ));
    assert!(matches!(
        parse_walletconnect_session_request(3, "eth_sign", &json!([])),
        Err(WalletConnectError::UnsupportedMethod(method)) if method == "eth_sign"
    ));
}

#[test]
fn parses_unsuffixed_typed_data_with_exact_identity_and_rejects_v3() {
    let account = address!("2222222222222222222222222222222222222222");
    let payload = typed_data_payload(&json!(1));

    let object_request = parse_walletconnect_session_request(
        4,
        "eth_signTypedData",
        &json!([account.to_string(), payload]),
    )
    .expect("unsuffixed object typed-data request");
    assert!(matches!(
        &object_request,
        WalletConnectParsedRequest::EthSignTypedData {
            account: request_account,
            domain_chain_id: Some(chain_id),
            ..
        } if *request_account == account && *chain_id == U256::from(1)
    ));
    assert_eq!(object_request.method().as_str(), "eth_signTypedData");

    let json_request = parse_walletconnect_session_request(
        5,
        "eth_signTypedData",
        &json!([account.to_string(), payload.to_string()]),
    )
    .expect("unsuffixed JSON-text typed-data request");
    assert!(matches!(
        json_request,
        WalletConnectParsedRequest::EthSignTypedData { .. }
    ));

    assert!(matches!(
        parse_walletconnect_session_request(6, "eth_signTypedData_v3", &json!([])),
        Err(WalletConnectError::UnsupportedMethod(method)) if method == "eth_signTypedData_v3"
    ));
}

#[test]
fn unsuffixed_typed_data_requires_its_own_active_session_permission() {
    let (session, account) = approved_request_session(&["eth_signTypedData_v4"]);
    let request = parse_walletconnect_session_request(
        7,
        "eth_signTypedData",
        &json!([account.address.to_string(), typed_data_payload(&json!(1))]),
    )
    .expect("unsuffixed typed-data request");
    let resolution = WalletConnectSessionAccountResolution::Usable(account);

    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            7,
            "eip155:1",
            request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::UnsupportedMethod(method)) if method == "eth_signTypedData"
    ));
}

#[test]
fn unsuffixed_typed_data_rejects_malformed_account_and_chain_inputs() {
    let account = address!("2222222222222222222222222222222222222222");
    assert!(matches!(
        parse_walletconnect_session_request(8, "eth_signTypedData", &json!({})),
        Err(WalletConnectError::MalformedParams(message))
            if message == "eth_signTypedData params must be an array"
    ));

    let (session, selected_account) = approved_request_session(&["eth_signTypedData"]);
    let resolution = WalletConnectSessionAccountResolution::Usable(selected_account.clone());
    let foreign_request = parse_walletconnect_session_request(
        9,
        "eth_signTypedData",
        &json!([account.to_string(), typed_data_payload(&json!(1))]),
    )
    .expect("foreign-account typed-data request");
    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            9,
            "eip155:1",
            foreign_request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::Relay(message))
            if message.contains("does not match selected Public account")
    ));

    let mismatched_chain_request = parse_walletconnect_session_request(
        10,
        "eth_signTypedData",
        &json!([
            selected_account.address.to_string(),
            typed_data_payload(&json!(2))
        ]),
    )
    .expect("chain-mismatched typed-data request");
    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            10,
            "eip155:1",
            mismatched_chain_request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::Relay(message))
            if message.contains("typed-data domain.chainId")
    ));
}

#[test]
fn rejects_malformed_personal_sign_hex_before_approval() {
    let account = address!("1111111111111111111111111111111111111111");

    assert!(matches!(
        parse_walletconnect_session_request(
            32,
            "personal_sign",
            &json!(["0xzz", account.to_string()]),
        ),
        Err(WalletConnectError::MalformedParams(message))
            if message.contains("valid hex")
    ));
    assert!(matches!(
        parse_walletconnect_session_request(
            33,
            "personal_sign",
            &json!(["0x123", account.to_string()]),
        ),
        Err(WalletConnectError::MalformedParams(message))
            if message.contains("valid hex")
    ));
    assert!(matches!(
        parse_walletconnect_session_request(
            34,
            "personal_sign",
            &json!(["plain text", account.to_string()]),
        )
        .unwrap(),
        WalletConnectParsedRequest::PersonalSign { .. }
    ));
}

#[cfg(not(feature = "hardware"))]
#[test]
fn default_build_hardware_session_request_rejects_signing_method() {
    let (session, mut account) = approved_request_session(&["personal_sign"]);
    account.source = PublicAccountSource::HardwareDerived;
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let request = parse_walletconnect_session_request(
        28,
        "personal_sign",
        &json!(["0x6869", account.address.to_string()]),
    )
    .unwrap();

    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            28,
            "eip155:1",
            request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::UnsupportedMethod(method)) if method == "personal_sign"
    ));
}

#[test]
fn hardware_typed_data_request_validation_allows_unknown_capability_probe() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_signTypedData_v4"], &[]),
    );
    let proposal = test_proposal(required);
    let relay_identity = WalletConnectRelayIdentity {
        signing_key: [8u8; 32],
        client_id: "relay-client".to_owned(),
    };
    let mut account = test_public_account(PublicAccountScope::Global);
    account.source = PublicAccountSource::HardwareDerived;
    let supported =
        WalletConnectNamespaceAccountSupport::hardware(HardwareTypedDataSigningMode::ClearSign);
    let approval = approve_walletconnect_session_with_account_support(
        &proposal,
        &[1u8; 32],
        &relay_identity,
        &account,
        supported,
        &supported_chains(&[1]),
        "hardware-typed-data-session",
        NOW,
    )
    .expect("approve typed-data session");
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let request = parse_walletconnect_session_request(
        33,
        "eth_signTypedData_v4",
        &json!([account.address.to_string(), typed_data_payload(&json!(1))]),
    )
    .expect("typed-data request");

    let validation = validate_walletconnect_session_request_with_account_support(
        &approval.session,
        &resolution,
        supported,
        &approval.session.session_topic,
        33,
        "eip155:1",
        request.clone(),
        Some(NOW + 300),
        NOW,
    )
    .expect("supported typed-data validation");
    assert_eq!(validation.request.method().as_str(), "eth_signTypedData_v4");

    let unknown_validation = validate_walletconnect_session_request_with_account_support(
        &approval.session,
        &resolution,
        WalletConnectNamespaceAccountSupport::hardware_typed_data_capability_unknown(),
        &approval.session.session_topic,
        33,
        "eip155:1",
        request.clone(),
        Some(NOW + 300),
        NOW,
    )
    .expect("unknown hardware typed-data capability can be probed at approval time");
    assert_eq!(
        unknown_validation.request.method().as_str(),
        "eth_signTypedData_v4"
    );
    assert!(unknown_validation.approval_item.is_some());

    assert!(matches!(
        validate_walletconnect_session_request_with_account_support(
            &approval.session,
            &resolution,
            WalletConnectNamespaceAccountSupport::hardware(HardwareTypedDataSigningMode::Unsupported),
            &approval.session.session_topic,
            33,
            "eip155:1",
            request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::UnsupportedMethod(method)) if method == "eth_signTypedData_v4"
    ));
}

#[test]
fn decodes_complete_transaction_review_states_and_strict_call_shapes() {
    let (session, account) = approved_request_session(&["eth_sendTransaction"]);
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let decode = |id: u64, transaction: serde_json::Value| {
        let request =
            parse_walletconnect_session_request(id, "eth_sendTransaction", &json!([transaction]))
                .expect("transaction request");
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            id,
            "eip155:1",
            request,
            Some(NOW + 300),
            NOW,
        )
        .expect("transaction validation")
        .approval_item
        .expect("transaction approval")
        .decoded_transaction
        .expect("transaction decode")
    };

    let recipient = address!("2222222222222222222222222222222222222222");
    let spender = address!("3333333333333333333333333333333333333333");
    let token = address!("4444444444444444444444444444444444444444");

    assert_eq!(
        decode(
            100,
            json!({
                "from": account.address.to_string(),
                "to": recipient.to_string(),
                "value": "0x5"
            }),
        ),
        WalletConnectDecodedTransaction {
            target: Some(recipient),
            native_value: U256::from(5),
            kind: WalletConnectDecodedCallKind::NativeTransfer,
        }
    );
    assert_eq!(
        decode(
            101,
            json!({
                "from": account.address.to_string(),
                "to": token.to_string()
            }),
        ),
        WalletConnectDecodedTransaction {
            target: Some(token),
            native_value: U256::ZERO,
            kind: WalletConnectDecodedCallKind::ContractCall { selector: None },
        }
    );
    assert_eq!(
        decode(
            102,
            json!({
                "from": account.address.to_string(),
                "value": "0x7",
                "data": "0x6000"
            }),
        ),
        WalletConnectDecodedTransaction {
            target: None,
            native_value: U256::from(7),
            kind: WalletConnectDecodedCallKind::ContractCreation,
        }
    );
    assert_eq!(
        decode(
            103,
            json!({
                "from": account.address.to_string(),
                "to": token.to_string(),
                "data": concat!(
                    "0xa9059cbb",
                    "0000000000000000000000002222222222222222222222222222222222222222",
                    "0000000000000000000000000000000000000000000000000000000000000008"
                ),
                "value": "0x4"
            }),
        ),
        WalletConnectDecodedTransaction {
            target: Some(token),
            native_value: U256::from(4),
            kind: WalletConnectDecodedCallKind::Erc20Transfer {
                recipient,
                amount: U256::from(8),
            },
        }
    );
    assert_eq!(
        decode(
            104,
            json!({
                "from": account.address.to_string(),
                "to": token.to_string(),
                "data": concat!(
                    "0x23b872dd",
                    "0000000000000000000000002222222222222222222222222222222222222222",
                    "0000000000000000000000003333333333333333333333333333333333333333",
                    "0000000000000000000000000000000000000000000000000000000000000009"
                )
            }),
        ),
        WalletConnectDecodedTransaction {
            target: Some(token),
            native_value: U256::ZERO,
            kind: WalletConnectDecodedCallKind::Erc20TransferFrom {
                from: recipient,
                to: spender,
                amount: U256::from(9),
            },
        }
    );
    assert_eq!(
        decode(
            105,
            json!({
                "from": account.address.to_string(),
                "to": token.to_string(),
                "value": "0xa",
                "data": "0xd0e30db0"
            }),
        ),
        WalletConnectDecodedTransaction {
            target: Some(token),
            native_value: U256::from(10),
            kind: WalletConnectDecodedCallKind::WrappedDeposit,
        }
    );
    assert_eq!(
        decode(
            106,
            json!({
                "from": account.address.to_string(),
                "to": token.to_string(),
                "value": "0x2",
                "data": concat!(
                    "0x2e1a7d4d",
                    "000000000000000000000000000000000000000000000000000000000000000b"
                )
            }),
        ),
        WalletConnectDecodedTransaction {
            target: Some(token),
            native_value: U256::from(2),
            kind: WalletConnectDecodedCallKind::WrappedWithdraw {
                amount: U256::from(11),
            },
        }
    );

    for (id, data) in [
        (
            107,
            concat!(
                "0x095ea7b3",
                "0000000000000000000000002222222222222222222222222222222222222222",
                "0000000000000000000000000000000000000000000000000000000000000001",
                "00"
            ),
        ),
        (
            108,
            concat!(
                "0x095ea7b3",
                "0100000000000000000000002222222222222222222222222222222222222222",
                "0000000000000000000000000000000000000000000000000000000000000001"
            ),
        ),
    ] {
        assert_eq!(
            decode(
                id,
                json!({
                    "from": account.address.to_string(),
                    "to": token.to_string(),
                    "data": data
                }),
            )
            .kind,
            WalletConnectDecodedCallKind::ContractCall {
                selector: Some([0x09, 0x5e, 0xa7, 0xb3]),
            }
        );
    }

    assert_eq!(
        decode(
            109,
            json!({
                "from": account.address.to_string(),
                "to": token.to_string(),
                "data": "0xdeadbeef0102"
            }),
        )
        .kind,
        WalletConnectDecodedCallKind::ContractCall {
            selector: Some([0xde, 0xad, 0xbe, 0xef]),
        }
    );
    assert_eq!(
        decode(
            110,
            json!({
                "from": account.address.to_string(),
                "to": token.to_string(),
                "data": "0x1234"
            }),
        )
        .kind,
        WalletConnectDecodedCallKind::ContractCall { selector: None }
    );

    assert_eq!(
        WalletConnectDecodedCallKind::Erc20Approve {
            spender,
            amount: U256::ZERO,
        }
        .selector(),
        Some([0x09, 0x5e, 0xa7, 0xb3])
    );
    assert_eq!(
        WalletConnectDecodedCallKind::Erc20Transfer {
            recipient,
            amount: U256::ZERO,
        }
        .selector(),
        Some([0xa9, 0x05, 0x9c, 0xbb])
    );
    assert_eq!(
        WalletConnectDecodedCallKind::Erc20TransferFrom {
            from: recipient,
            to: spender,
            amount: U256::ZERO,
        }
        .selector(),
        Some([0x23, 0xb8, 0x72, 0xdd])
    );
    assert_eq!(
        WalletConnectDecodedCallKind::WrappedDeposit.selector(),
        Some([0xd0, 0xe3, 0x0d, 0xb0])
    );
    assert_eq!(
        WalletConnectDecodedCallKind::WrappedWithdraw { amount: U256::ZERO }.selector(),
        Some([0x2e, 0x1a, 0x7d, 0x4d])
    );
    assert_eq!(
        WalletConnectDecodedCallKind::ContractCall {
            selector: Some([0xde, 0xad, 0xbe, 0xef]),
        }
        .selector(),
        Some([0xde, 0xad, 0xbe, 0xef])
    );
    assert_eq!(
        WalletConnectDecodedCallKind::NativeTransfer.selector(),
        None
    );
    assert_eq!(
        WalletConnectDecodedCallKind::ContractCreation.selector(),
        None
    );
}

#[test]
fn accepts_session_request_expiry_with_less_than_minimum_remaining() {
    let (session, account) = approved_request_session(&["personal_sign"]);
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let request = parse_walletconnect_session_request(
        31,
        "personal_sign",
        &json!(["0x6869", account.address.to_string()]),
    )
    .unwrap();

    let validation = validate_walletconnect_session_request(
        &session,
        &resolution,
        &session.session_topic,
        31,
        "eip155:1",
        request,
        Some(NOW + 299),
        NOW,
    )
    .unwrap();
    assert!(validation.approval_item.is_some());
}

#[test]
fn rejects_session_request_expiry_when_expired_or_too_far_future() {
    let (session, account) = approved_request_session(&["personal_sign"]);
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let request = parse_walletconnect_session_request(
        31,
        "personal_sign",
        &json!(["0x6869", account.address.to_string()]),
    )
    .unwrap();

    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            31,
            "eip155:1",
            request.clone(),
            Some(NOW),
            NOW,
        ),
        Err(WalletConnectError::ExpiredUri)
    ));
    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            31,
            "eip155:1",
            request.clone(),
            Some(NOW + 604_801),
            NOW,
        ),
        Err(WalletConnectError::ExpiredUri)
    ));

    let validation = validate_walletconnect_session_request(
        &session,
        &resolution,
        &session.session_topic,
        31,
        "eip155:1",
        request,
        Some(NOW + 604_800),
        NOW,
    )
    .unwrap();
    assert!(validation.approval_item.is_some());
}

#[test]
fn parses_send_transaction_execution_overrides() {
    let account = address!("1111111111111111111111111111111111111111");
    let request = parse_walletconnect_session_request(
        22,
        "eth_sendTransaction",
        &json!([{
            "from": account.to_string(),
            "gas": "0x5208",
            "gasPrice": "0x3b9aca00",
            "maxFeePerGas": "0x4a817c800",
            "maxPriorityFeePerGas": "0x77359400",
            "nonce": "0x2a",
            "type": "0x1",
            "accessList": [{
                "address": "0x2222222222222222222222222222222222222222",
                "storageKeys": ["0x0000000000000000000000000000000000000000000000000000000000000003"]
            }],
        }]),
    )
    .unwrap();
    let WalletConnectParsedRequest::EthSendTransaction { transaction } = request else {
        panic!("expected eth_sendTransaction");
    };

    assert_eq!(transaction.gas, Some(U256::from(0x5208_u64)));
    assert_eq!(transaction.gas_price, Some(U256::from(1_000_000_000_u64)));
    assert_eq!(
        transaction.max_fee_per_gas,
        Some(U256::from(20_000_000_000_u64))
    );
    assert_eq!(
        transaction.max_priority_fee_per_gas,
        Some(U256::from(2_000_000_000_u64))
    );
    assert_eq!(transaction.nonce, Some(U256::from(42_u64)));
    assert_eq!(transaction.transaction_type, Some(1));
    let access_list = transaction.access_list.expect("access list");
    assert_eq!(access_list.len(), 1);
    assert_eq!(
        access_list[0].address,
        address!("2222222222222222222222222222222222222222")
    );
}

#[test]
fn wallet_switch_ethereum_chain_accepts_different_approved_target_chain() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(
            &["eip155:1", "eip155:42161"],
            &["wallet_switchEthereumChain"],
            &["chainChanged"],
        ),
    );
    let proposal = test_proposal(required);
    let relay_identity = WalletConnectRelayIdentity {
        signing_key: [8u8; 32],
        client_id: "relay-client".to_owned(),
    };
    let account = test_public_account(PublicAccountScope::Global);
    let approval = approve_walletconnect_session(
        &proposal,
        &[1u8; 32],
        &relay_identity,
        &account,
        &supported_chains(&[1, 42161]),
        "switch-session",
        NOW,
    )
    .unwrap();
    let resolution = WalletConnectSessionAccountResolution::Usable(account);
    let request = parse_walletconnect_session_request(
        23,
        "wallet_switchEthereumChain",
        &json!([{ "chainId": "0xa4b1" }]),
    )
    .unwrap();

    let validation = validate_walletconnect_session_request(
        &approval.session,
        &resolution,
        &approval.session.session_topic,
        23,
        "eip155:1",
        request,
        Some(NOW + 300),
        NOW,
    )
    .unwrap();

    assert!(matches!(
        validation.request,
        WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id: 42161 }
    ));
}

#[test]
fn validates_aave_style_approve_send_transaction_as_pending_request() {
    let (session, account) = approved_request_session(&["eth_sendTransaction"]);
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let approve_data = concat!(
        "0x095ea7b3",
        "0000000000000000000000002222222222222222222222222222222222222222",
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
    );
    let request = parse_walletconnect_session_request(
        2_526,
        "eth_sendTransaction",
        &json!([{
            "from": account.address.to_string(),
            "to": "0xdAC17F958D2ee523a2206206994597C13D831ec7",
            "data": approve_data,
            "value": "0x0"
        }]),
    )
    .unwrap();

    let validation = validate_walletconnect_session_request(
        &session,
        &resolution,
        &session.session_topic,
        2_526,
        "eip155:1",
        request,
        Some(NOW + 300),
        NOW,
    )
    .unwrap();
    let approval = validation.approval_item.expect("approval item");

    assert_eq!(approval.id, 2_526);
    assert_eq!(approval.method.as_str(), "eth_sendTransaction");
    assert_eq!(approval.chain_id, "eip155:1");
    assert_eq!(approval.account, account.address);
    assert_eq!(
        approval.raw_details["to"],
        json!("0xdAC17F958D2ee523a2206206994597C13D831ec7")
    );
    assert!(matches!(
        approval.decoded_transaction,
        Some(WalletConnectDecodedTransaction {
            target: Some(target),
            native_value,
            kind: WalletConnectDecodedCallKind::Erc20Approve { spender, amount },
        }) if target == address!("dac17f958d2ee523a2206206994597c13d831ec7")
            && native_value.is_zero()
            && spender == address!("2222222222222222222222222222222222222222")
            && amount == U256::MAX
    ));
}

#[test]
fn rejects_invalid_transaction_data_hex_before_approval() {
    let (_, account) = approved_request_session(&["eth_sendTransaction"]);

    assert!(matches!(
        parse_walletconnect_session_request(
            19,
            "eth_sendTransaction",
            &json!([{ "from": account.address.to_string(), "data": "0xzz" }]),
        ),
        Err(WalletConnectError::MalformedParams(message)) if message.contains("valid hex")
    ));
    assert!(matches!(
        parse_walletconnect_session_request(
            20,
            "eth_sendTransaction",
            &json!([{ "from": account.address.to_string(), "input": "0x123" }]),
        ),
        Err(WalletConnectError::MalformedParams(message)) if message.contains("valid hex")
    ));
}

#[test]
fn rejects_transaction_and_typed_data_chain_mismatches_before_approval() {
    let (session, account) =
        approved_request_session(&["eth_sendTransaction", "eth_signTypedData_v4"]);
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());

    let tx_request = parse_walletconnect_session_request(
        11,
        "eth_sendTransaction",
        &json!([{ "from": account.address.to_string(), "chainId": "0xa" }]),
    )
    .unwrap();
    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            11,
            "eip155:1",
            tx_request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::Relay(message)) if message.contains("transaction chainId")
    ));

    let typed_request = parse_walletconnect_session_request(
        12,
        "eth_signTypedData_v4",
        &json!([
            account.address.to_string(),
            typed_data_payload(&json!("0xa"))
        ]),
    )
    .unwrap();
    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            12,
            "eip155:1",
            typed_request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::Relay(message)) if message.contains("typed-data")
    ));

    let oversized_request = parse_walletconnect_session_request(
        24,
        "eth_signTypedData_v4",
        &json!([
            account.address.to_string(),
            typed_data_payload(&json!(
                "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
            ))
        ]),
    )
    .unwrap();
    assert!(matches!(
        validate_walletconnect_session_request(
            &session,
            &resolution,
            &session.session_topic,
            24,
            "eip155:1",
            oversized_request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::Relay(message)) if message.contains("typed-data")
    ));
}

#[test]
fn rejects_malformed_typed_data_domain_chain_id_before_approval() {
    let (_, account) = approved_request_session(&["eth_signTypedData_v4"]);

    assert!(matches!(
        parse_walletconnect_session_request(
            25,
            "eth_signTypedData_v4",
            &json!([
                account.address.to_string(),
                typed_data_payload(&json!("0x10000000000000000000000000000000000000000000000000000000000000000"))
            ]),
        ),
        Err(WalletConnectError::MalformedParams(message)) if message.contains("domain.chainId")
    ));
}

#[test]
fn rejects_malformed_typed_data_payload_before_approval() {
    let (_, account) = approved_request_session(&["eth_signTypedData_v4"]);

    assert!(matches!(
        parse_walletconnect_session_request(
            29,
            "eth_signTypedData_v4",
            &json!([account.address.to_string(), {}]),
        ),
        Err(WalletConnectError::MalformedParams(message))
            if message.contains("invalid EIP-712")
    ));

    assert!(matches!(
        parse_walletconnect_session_request(
            30,
            "eth_signTypedData_v4",
            &json!([
                account.address.to_string(),
                {
                    "types": {
                        "EIP712Domain": [],
                        "Message": [{ "name": "contents", "type": "string" }]
                    },
                    "domain": {},
                    "message": { "contents": "hello" }
                }
            ]),
        ),
        Err(WalletConnectError::MalformedParams(message))
            if message.contains("invalid EIP-712")
    ));
}

#[test]
fn pending_queue_removes_expired_requests() {
    let mut queue = WalletConnectPendingRequestQueue::default();
    let (session, account) = approved_request_session(&["personal_sign"]);
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let request = parse_walletconnect_session_request(
        13,
        "personal_sign",
        &json!(["0x6869", account.address.to_string()]),
    )
    .unwrap();
    let validation = validate_walletconnect_session_request(
        &session,
        &resolution,
        &session.session_topic,
        13,
        "eip155:1",
        request,
        Some(NOW + 300),
        NOW,
    )
    .unwrap();

    queue.insert(validation.approval_item.expect("approval item"));
    assert!(queue.get(13).is_some());
    let expired = queue.remove_expired(NOW + 301);

    assert_eq!(expired.len(), 1);
    assert!(queue.get(13).is_none());
}
