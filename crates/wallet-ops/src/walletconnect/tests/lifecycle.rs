use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::vault::{PublicAccountScope, WalletConnectRelayIdentity};
use crate::walletconnect::{
    WalletConnectError, WalletConnectLifecycleRequestOutcome, WalletConnectRelayLifecycle,
    WalletConnectRelayRpc, WalletConnectRelayStep, WalletConnectRequestErrorKind,
    WalletConnectTerminalLifecycleEnd, approve_walletconnect_session,
    build_walletconnect_disconnect_plan, build_walletconnect_jsonrpc_error,
    build_walletconnect_session_event, handle_walletconnect_lifecycle_request,
};

use super::helpers::{
    NOW, approved_request_session, decode_encrypted_json, namespace, supported_chains,
    test_proposal, test_public_account,
};

#[test]
fn relay_lifecycle_pauses_without_unsubscribing_and_terminal_unsubscribes() {
    let mut lifecycle = WalletConnectRelayLifecycle::default();
    lifecycle.add_subscribed_topic("session-topic", "sub-1", true);

    assert!(lifecycle.local_processing_active());
    assert_eq!(
        lifecycle.pause_for_lock_or_shutdown(),
        [] as [WalletConnectRelayRpc; 0]
    );
    assert!(!lifecycle.local_processing_active());

    lifecycle.resume_after_unlock();
    assert!(lifecycle.local_processing_active());

    let unsubscribe = lifecycle
        .terminal_end(
            "session-topic",
            WalletConnectTerminalLifecycleEnd::Disconnect,
        )
        .expect("unsubscribe on terminal end");
    assert!(matches!(
        unsubscribe,
        WalletConnectRelayRpc::Unsubscribe { topic, id }
            if topic == "session-topic" && id == "sub-1"
    ));
    assert!(!lifecycle.local_processing_active());
}

#[test]
fn restored_session_steps_fetch_before_subscribe() {
    let steps = WalletConnectRelayLifecycle::restored_session_steps("session-topic");

    assert!(matches!(
        &steps[0],
        WalletConnectRelayStep::FetchMessages { topic } if topic == "session-topic"
    ));
    assert!(matches!(
        &steps[1],
        WalletConnectRelayStep::Subscribe { topic } if topic == "session-topic"
    ));
}

#[test]
fn reconnect_steps_fetch_and_resubscribe_active_topics() {
    let mut lifecycle = WalletConnectRelayLifecycle::default();
    lifecycle.add_subscribed_topic("session-topic", "sub-1", true);
    let steps = lifecycle.reconnect_steps();

    assert!(matches!(
        &steps[0],
        WalletConnectRelayStep::FetchMessages { topic } if topic == "session-topic"
    ));
    assert!(matches!(
        &steps[1],
        WalletConnectRelayStep::Subscribe { topic } if topic == "session-topic"
    ));
}

#[test]
fn disconnect_plan_sends_session_delete_and_unsubscribes_when_possible() {
    let mut required = BTreeMap::new();
    required.insert(
        "eip155".to_owned(),
        namespace(&["eip155:1"], &["eth_accounts"], &["accountsChanged"]),
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
        &supported_chains(&[1]),
        "disconnect-session",
        NOW,
    )
    .unwrap();

    let plan = build_walletconnect_disconnect_plan(&approval.session, 99, Some("sub-1")).unwrap();

    assert_eq!(plan.delete_request.method, "wc_sessionDelete");
    assert_eq!(plan.relay_steps.len(), 2);
    assert!(matches!(
        &plan.relay_steps[0],
        WalletConnectRelayStep::Publish(_)
    ));
    let delete = plan.relay_steps[0].rpc().request(100);
    assert_eq!(delete.params["ttl"], json!(86_400));
    assert_eq!(delete.params["tag"], json!(1112));
    assert!(serde_json::from_str::<Value>(delete.params["message"].as_str().unwrap()).is_err());
    let delete_message = decode_encrypted_json(
        &approval.session.keys.sym_key,
        delete.params["message"].as_str().unwrap(),
    );
    assert_eq!(delete_message["method"], "wc_sessionDelete");
    assert!(matches!(
        &plan.relay_steps[1],
        WalletConnectRelayStep::Unsubscribe { topic, id }
            if topic == &approval.session.session_topic && id == "sub-1"
    ));
}

#[test]
fn session_event_builder_checks_approved_event_and_chain_payload() {
    let (session, _) = approved_request_session(&["eth_accounts"]);

    let event =
        build_walletconnect_session_event(&session, 14, "eip155:1", "chainChanged", json!(1))
            .unwrap();

    assert_eq!(event.method, "wc_sessionEvent");
    assert_eq!(event.params["chainId"], "eip155:1");
    assert_eq!(event.params["event"]["data"], 1);
    assert!(matches!(
        build_walletconnect_session_event(&session, 15, "eip155:1", "badEvent", json!(null)),
        Err(WalletConnectError::UnsupportedEvent(event)) if event == "badEvent"
    ));
}

#[test]
fn lifecycle_delete_ping_and_error_mapping_are_protocol_compatible() {
    assert!(matches!(
        handle_walletconnect_lifecycle_request(16, "wc_sessionDelete"),
        WalletConnectLifecycleRequestOutcome::Delete { response }
            if response.result == Some(json!(true))
    ));
    assert!(matches!(
        handle_walletconnect_lifecycle_request(17, "wc_sessionPing"),
        WalletConnectLifecycleRequestOutcome::Ping { response }
            if response.result == Some(json!(true))
    ));

    let expired = build_walletconnect_jsonrpc_error(
        18,
        WalletConnectRequestErrorKind::ExpiredRequest,
        "expired",
    );
    assert_eq!(expired.error.unwrap().code, 8_000);
}
