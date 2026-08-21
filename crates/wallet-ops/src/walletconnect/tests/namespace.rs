use std::collections::BTreeMap;

use alloy::primitives::address;
use serde_json::json;

use crate::hardware::HardwareTypedDataSigningMode;
use crate::vault::{
    PublicAccountScope, PublicAccountSource, WalletConnectRelayIdentity,
    WalletConnectSessionAccountResolution,
};
use crate::walletconnect::{
    WalletConnectError, WalletConnectNamespaceAccountSupport, approve_walletconnect_session,
    approve_walletconnect_session_with_account_support, negotiate_walletconnect_namespaces,
    negotiate_walletconnect_namespaces_with_account_support, parse_walletconnect_session_request,
    validate_walletconnect_session_request,
    validate_walletconnect_session_request_with_account_support,
};

use super::helpers::{
    NOW, namespace, supported_chains, test_proposal, test_public_account, typed_data_payload,
};

#[test]
fn required_caip2_namespace_key_declares_chain() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155:10".to_owned(),
        namespace(&[], &["eth_accounts"], &["chainChanged"]),
    );

    let negotiated = negotiate_walletconnect_namespaces(
        &required,
        &BTreeMap::new(),
        &supported_chains(&[10]),
        address!("1111111111111111111111111111111111111111"),
        PublicAccountSource::Imported,
    )
    .unwrap();
    let approved = negotiated
        .approved_namespaces
        .get("eip155:10")
        .expect("approved keyed namespace");

    assert_eq!(approved.chains, vec!["eip155:10"]);
    assert_eq!(
        approved.accounts,
        vec!["eip155:10:0x1111111111111111111111111111111111111111"]
    );
}

#[test]
fn empty_proposal_approves_default_eip155_namespace() {
    let negotiated = negotiate_walletconnect_namespaces(
        &BTreeMap::new(),
        &BTreeMap::new(),
        &supported_chains(&[1, 137]),
        address!("1111111111111111111111111111111111111111"),
        PublicAccountSource::Imported,
    )
    .unwrap();
    let approved = negotiated
        .approved_namespaces
        .get("eip155")
        .expect("default eip155 namespace");

    assert_eq!(approved.chains, vec!["eip155:1", "eip155:137"]);
    assert_eq!(
        approved.accounts,
        vec![
            "eip155:1:0x1111111111111111111111111111111111111111",
            "eip155:137:0x1111111111111111111111111111111111111111",
        ]
    );
    assert!(approved.methods.is_empty());
    assert!(approved.events.is_empty());
}

#[test]
fn event_only_required_namespace_is_approved() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &[], &["chainChanged"]),
    );

    let negotiated = negotiate_walletconnect_namespaces(
        &required,
        &BTreeMap::new(),
        &supported_chains(&[1]),
        address!("1111111111111111111111111111111111111111"),
        PublicAccountSource::Imported,
    )
    .unwrap();
    let approved = negotiated
        .approved_namespaces
        .get("eip155")
        .expect("approved event-only namespace");

    assert_eq!(approved.methods, Vec::<String>::new());
    assert_eq!(approved.events, vec!["chainChanged"]);
}

#[test]
fn empty_required_namespace_is_approved() {
    let mut required = BTreeMap::new();
    required.insert("eip155".to_owned(), namespace(&["eip155:1"], &[], &[]));

    let negotiated = negotiate_walletconnect_namespaces(
        &required,
        &BTreeMap::new(),
        &supported_chains(&[1]),
        address!("1111111111111111111111111111111111111111"),
        PublicAccountSource::Imported,
    )
    .unwrap();
    let approved = negotiated
        .approved_namespaces
        .get("eip155")
        .expect("approved empty namespace");

    assert_eq!(approved.chains, vec!["eip155:1"]);
    assert_eq!(
        approved.accounts,
        vec!["eip155:1:0x1111111111111111111111111111111111111111"]
    );
    assert!(approved.methods.is_empty());
    assert!(approved.events.is_empty());
}

#[test]
fn event_only_optional_namespace_is_approved() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &[]),
    );
    let mut optional = BTreeMap::new();
    optional.insert(
        "eip155:137".to_owned(),
        namespace(&[], &[], &["chainChanged"]),
    );

    let negotiated = negotiate_walletconnect_namespaces(
        &required,
        &optional,
        &supported_chains(&[1, 137]),
        address!("1111111111111111111111111111111111111111"),
        PublicAccountSource::Imported,
    )
    .unwrap();
    let approved = negotiated
        .approved_namespaces
        .get("eip155:137")
        .expect("approved optional event-only namespace");

    assert_eq!(approved.methods, Vec::<String>::new());
    assert_eq!(approved.events, vec!["chainChanged"]);
}

#[test]
fn required_namespace_rejects_unsupported_method() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts", "eth_sign"], &[]),
    );

    assert!(matches!(
        negotiate_walletconnect_namespaces(
            &required,
            &BTreeMap::new(),
            &supported_chains(&[1]),
            address!("1111111111111111111111111111111111111111"),
            PublicAccountSource::Imported,
        ),
        Err(WalletConnectError::UnsatisfiedNamespaces(message)) if message.contains("eth_sign")
    ));
}

#[test]
fn optional_namespace_can_be_partially_approved() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &[]),
    );
    let mut optional = BTreeMap::new();
    optional.insert(
        "eip155:42161".to_owned(),
        namespace(
            &["eip155:42161", "eip155:999999"],
            &["eth_sendTransaction", "eth_sign"],
            &["accountsChanged", "badEvent"],
        ),
    );

    let negotiated = negotiate_walletconnect_namespaces(
        &required,
        &optional,
        &supported_chains(&[1, 42161]),
        address!("1111111111111111111111111111111111111111"),
        PublicAccountSource::Imported,
    )
    .unwrap();
    let optional = negotiated
        .approved_namespaces
        .get("eip155:42161")
        .expect("approved optional subset");

    assert_eq!(optional.chains, vec!["eip155:42161"]);
    assert_eq!(optional.methods, vec!["eth_sendTransaction"]);
    assert_eq!(optional.events, vec!["accountsChanged"]);
    assert!(
        negotiated
            .excluded_optional
            .iter()
            .any(|item| item.item == "eth_sign")
    );
}

#[test]
fn hardware_account_rejects_required_typed_data_namespace() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(
            &["eip155:1"],
            &["eth_signTypedData", "eth_signTypedData_v4"],
            &[],
        ),
    );

    assert!(matches!(
        negotiate_walletconnect_namespaces(
            &required,
            &BTreeMap::new(),
            &supported_chains(&[1]),
            address!("1111111111111111111111111111111111111111"),
            PublicAccountSource::HardwareDerived,
        ),
        Err(WalletConnectError::UnsatisfiedNamespaces(message))
            if message.contains("eth_signTypedData_v4") && message.contains("hardware")
    ));
}

#[test]
fn hardware_account_accepts_required_typed_data_namespace_with_capability() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(
            &["eip155:1"],
            &["eth_signTypedData", "eth_signTypedData_v4"],
            &[],
        ),
    );

    let negotiated = negotiate_walletconnect_namespaces_with_account_support(
        &required,
        &BTreeMap::new(),
        &supported_chains(&[1]),
        address!("1111111111111111111111111111111111111111"),
        WalletConnectNamespaceAccountSupport::hardware(HardwareTypedDataSigningMode::ClearSign),
    )
    .expect("hardware typed-data namespace");
    let approved = negotiated
        .approved_namespaces
        .get("eip155")
        .expect("approved namespace");

    assert_eq!(
        approved.methods,
        vec!["eth_signTypedData", "eth_signTypedData_v4"]
    );
}

#[test]
fn hardware_account_auto_includes_optional_typed_data_with_capability() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &[]),
    );
    let mut optional = BTreeMap::new();
    optional.insert(
        "eip155:1".to_owned(),
        namespace(&[], &["eth_signTypedData", "eth_signTypedData_v4"], &[]),
    );

    let negotiated = negotiate_walletconnect_namespaces_with_account_support(
        &required,
        &optional,
        &supported_chains(&[1]),
        address!("1111111111111111111111111111111111111111"),
        WalletConnectNamespaceAccountSupport::hardware(HardwareTypedDataSigningMode::ClearSign),
    )
    .expect("hardware namespace");
    let optional = negotiated
        .approved_namespaces
        .get("eip155:1")
        .expect("approved optional namespace");

    assert_eq!(
        optional.methods,
        vec!["eth_signTypedData", "eth_signTypedData_v4"]
    );
    assert!(negotiated.excluded_optional.is_empty());
}

#[test]
fn hardware_account_omits_optional_typed_data_without_capability() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &[]),
    );
    let mut optional = BTreeMap::new();
    optional.insert(
        "eip155:1".to_owned(),
        namespace(&[], &["eth_signTypedData", "eth_signTypedData_v4"], &[]),
    );

    let negotiated = negotiate_walletconnect_namespaces_with_account_support(
        &required,
        &optional,
        &supported_chains(&[1]),
        address!("1111111111111111111111111111111111111111"),
        WalletConnectNamespaceAccountSupport::hardware(HardwareTypedDataSigningMode::Unsupported),
    )
    .expect("hardware namespace");

    assert!(negotiated.approved_namespaces.values().all(|namespace| {
        namespace
            .methods
            .iter()
            .all(|method| method != "eth_signTypedData" && method != "eth_signTypedData_v4")
    }));
    assert!(
        negotiated
            .excluded_optional
            .iter()
            .any(|item| item.item == "eth_signTypedData")
    );
    assert!(
        negotiated
            .excluded_optional
            .iter()
            .any(|item| item.item == "eth_signTypedData_v4")
    );
}

#[test]
fn hardware_optional_typed_data_approval_and_request_validation_stay_in_sync() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &[]),
    );
    let mut proposal = test_proposal(required);
    proposal.optional_namespaces.insert(
        "eip155:1".to_owned(),
        namespace(&[], &["eth_signTypedData", "eth_signTypedData_v4"], &[]),
    );
    let relay_identity = WalletConnectRelayIdentity {
        signing_key: [8u8; 32],
        client_id: "relay-client".to_owned(),
    };
    let mut account = test_public_account(PublicAccountScope::Global);
    account.source = PublicAccountSource::HardwareDerived;
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let request = parse_walletconnect_session_request(
        31,
        "eth_signTypedData",
        &json!([account.address.to_string(), typed_data_payload(&json!(1))]),
    )
    .expect("typed-data request");

    let excluded = approve_walletconnect_session_with_account_support(
        &proposal,
        &[1u8; 32],
        &relay_identity,
        &account,
        WalletConnectNamespaceAccountSupport::hardware(HardwareTypedDataSigningMode::Unsupported),
        &supported_chains(&[1]),
        "hardware-optional-typed-data-excluded",
        NOW,
    )
    .expect("approve without optional typed data");
    assert!(
        excluded
            .session
            .approved_namespaces
            .values()
            .all(|namespace| {
                namespace
                    .methods
                    .iter()
                    .all(|method| method != "eth_signTypedData" && method != "eth_signTypedData_v4")
            })
    );
    assert!(matches!(
        validate_walletconnect_session_request_with_account_support(
            &excluded.session,
            &resolution,
            WalletConnectNamespaceAccountSupport::hardware(HardwareTypedDataSigningMode::Unsupported),
            &excluded.session.session_topic,
            31,
            "eip155:1",
            request.clone(),
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::UnsupportedMethod(method)) if method == "eth_signTypedData"
    ));

    let included = approve_walletconnect_session_with_account_support(
        &proposal,
        &[1u8; 32],
        &relay_identity,
        &account,
        WalletConnectNamespaceAccountSupport::hardware(
            HardwareTypedDataSigningMode::Eip712HashFallback,
        ),
        &supported_chains(&[1]),
        "hardware-optional-typed-data-included",
        NOW,
    )
    .expect("approve with optional typed data");
    assert!(
        included
            .session
            .approved_namespaces
            .values()
            .any(|namespace| {
                namespace
                    .methods
                    .iter()
                    .any(|method| method == "eth_signTypedData" || method == "eth_signTypedData_v4")
            })
    );
    let validation = validate_walletconnect_session_request_with_account_support(
        &included.session,
        &resolution,
        WalletConnectNamespaceAccountSupport::hardware(
            HardwareTypedDataSigningMode::Eip712HashFallback,
        ),
        &included.session.session_topic,
        31,
        "eip155:1",
        request.clone(),
        Some(NOW + 300),
        NOW,
    )
    .expect("validate included typed data");
    assert!(validation.approval_item.is_some());
    assert!(matches!(
        validate_walletconnect_session_request_with_account_support(
            &included.session,
            &resolution,
            WalletConnectNamespaceAccountSupport::hardware(HardwareTypedDataSigningMode::Unsupported),
            &included.session.session_topic,
            31,
            "eip155:1",
            request,
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::UnsupportedMethod(method)) if method == "eth_signTypedData"
    ));
}

#[cfg(not(feature = "hardware"))]
#[test]
fn default_build_hardware_account_rejects_required_signing_namespaces() {
    for method in [
        "personal_sign",
        "eth_sendTransaction",
        "eth_signTypedData",
        "eth_signTypedData_v4",
    ] {
        let mut required = BTreeMap::new();
        required.insert(
            "eip155".to_owned(),
            namespace(&["eip155:1"], &[method], &[]),
        );

        assert!(matches!(
            negotiate_walletconnect_namespaces(
                &required,
                &BTreeMap::new(),
                &supported_chains(&[1]),
                address!("1111111111111111111111111111111111111111"),
                PublicAccountSource::HardwareDerived,
            ),
            Err(WalletConnectError::UnsatisfiedNamespaces(message))
                if message.contains(method) && message.contains("hardware")
        ));
    }
}

#[cfg(not(feature = "hardware"))]
#[test]
fn default_build_hardware_account_excludes_optional_signing_methods() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &[]),
    );
    let mut optional = BTreeMap::new();
    optional.insert(
        "eip155:1".to_owned(),
        namespace(
            &[],
            &[
                "personal_sign",
                "eth_sendTransaction",
                "eth_signTypedData",
                "eth_signTypedData_v4",
            ],
            &[],
        ),
    );

    let negotiated = negotiate_walletconnect_namespaces(
        &required,
        &optional,
        &supported_chains(&[1]),
        address!("1111111111111111111111111111111111111111"),
        PublicAccountSource::HardwareDerived,
    )
    .unwrap();

    assert!(negotiated.approved_namespaces.values().all(|namespace| {
        namespace.methods.iter().all(|method| {
            method != "personal_sign"
                && method != "eth_sendTransaction"
                && method != "eth_signTypedData"
                && method != "eth_signTypedData_v4"
        })
    }));
}

#[test]
fn optional_caip2_namespace_method_grant_is_honored_for_same_chain() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &[]),
    );
    let mut proposal = test_proposal(required);
    proposal.optional_namespaces.insert(
        "eip155:1".to_owned(),
        namespace(&[], &["eth_sendTransaction"], &[]),
    );
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
        &supported_chains(&[1]),
        "optional-method-session",
        NOW,
    )
    .unwrap();
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let request = parse_walletconnect_session_request(
        21,
        "eth_sendTransaction",
        &json!([{ "from": account.address.to_string() }]),
    )
    .unwrap();

    let validation = validate_walletconnect_session_request(
        &approval.session,
        &resolution,
        &approval.session.session_topic,
        21,
        "eip155:1",
        request,
        Some(NOW + 300),
        NOW,
    )
    .unwrap();

    assert_eq!(validation.chain_id, "eip155:1");
    assert!(validation.approval_item.is_some());
}

#[test]
fn optional_generic_namespace_method_does_not_leak_to_required_chain() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &[]),
    );
    let mut proposal = test_proposal(required);
    proposal.optional_namespaces.insert(
        "eip155".to_owned(),
        namespace(&["eip155:137"], &["eth_sendTransaction"], &[]),
    );
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
        &supported_chains(&[1, 137]),
        "optional-generic-method-session",
        NOW,
    )
    .unwrap();
    let resolution = WalletConnectSessionAccountResolution::Usable(account.clone());
    let request = parse_walletconnect_session_request(
        26,
        "eth_sendTransaction",
        &json!([{ "from": account.address.to_string() }]),
    )
    .unwrap();

    assert!(matches!(
        validate_walletconnect_session_request(
            &approval.session,
            &resolution,
            &approval.session.session_topic,
            26,
            "eip155:1",
            request.clone(),
            Some(NOW + 300),
            NOW,
        ),
        Err(WalletConnectError::UnsupportedMethod(method)) if method == "eth_sendTransaction"
    ));

    let validation = validate_walletconnect_session_request(
        &approval.session,
        &resolution,
        &approval.session.session_topic,
        27,
        "eip155:137",
        request,
        Some(NOW + 300),
        NOW,
    )
    .unwrap();
    assert!(validation.approval_item.is_some());
}
