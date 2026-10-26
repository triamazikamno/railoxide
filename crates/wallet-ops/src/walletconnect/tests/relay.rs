use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use zeroize::Zeroize;

use crate::walletconnect::relay::{
    WalletConnectJsonRpcError, relay_response_id_matches,
    sanitize_walletconnect_relay_error_message,
};
use crate::walletconnect::{
    WALLETCONNECT_DEFAULT_PROJECT_ID, WALLETCONNECT_RELAY_URL, WalletConnectError,
    WalletConnectJsonRpcId, WalletConnectJsonRpcResponse, WalletConnectRelayClient,
    WalletConnectRelayClientAuth, WalletConnectRelayConfig, WalletConnectRelayRpc,
    WalletConnectRelaySubscriptionRequest,
};

#[test]
fn serializes_relay_publish_rpc() {
    let rpc = WalletConnectRelayRpc::Publish {
        topic: "topic-a".to_owned(),
        message: "payload".to_owned(),
        ttl: 300,
        tag: 1102,
    };
    let serialized = serde_json::to_value(rpc.request(7)).unwrap();

    assert_eq!(
        serialized,
        json!({
            "id": "7",
            "jsonrpc": "2.0",
            "method": "irn_publish",
            "params": {
                "message": "payload",
                "tag": 1102,
                "topic": "topic-a",
                "ttl": 300,
            },
        })
    );
}

#[test]
fn relay_request_id_matches_string_and_number_responses() {
    let id = 1_700_000_000_000_000_123_u64;
    let rpc = WalletConnectRelayRpc::Subscribe {
        topic: "topic-a".to_owned(),
    };
    let serialized = serde_json::to_value(rpc.request(id)).unwrap();

    assert_eq!(serialized["id"], json!(id.to_string()));
    assert!(relay_response_id_matches(
        &json!({ "id": id.to_string(), "jsonrpc": "2.0", "result": true }),
        id,
    ));
    assert!(relay_response_id_matches(
        &json!({ "id": id, "jsonrpc": "2.0", "result": true }),
        id,
    ));
}

#[test]
fn relay_subscription_error_is_not_treated_as_success() {
    let response = WalletConnectJsonRpcResponse::<Value> {
        id: 9,
        jsonrpc: "2.0".to_owned(),
        result: None,
        error: Some(WalletConnectJsonRpcError {
            code: -32_000,
            message: "subscription failed".to_owned(),
        }),
    };

    assert!(matches!(
        response.into_result(),
        Err(WalletConnectError::Relay(message)) if message.contains("subscription failed")
    ));
}

#[test]
fn default_relay_config_uses_bundled_project_id() {
    assert_eq!(
        WalletConnectRelayConfig::default().project_id,
        WALLETCONNECT_DEFAULT_PROJECT_ID
    );
}

#[test]
fn relay_client_auth_reuses_client_id_for_same_identity() {
    let identity = [7u8; 32];
    let first = WalletConnectRelayClientAuth::from_signing_key(identity);
    let second = WalletConnectRelayClientAuth::from_signing_key(identity);

    assert_eq!(first.client_id, second.client_id);
    assert!(first.client_id.starts_with("did:key:z"));
    assert_ne!(first.bearer_token(WALLETCONNECT_RELAY_URL, 60).unwrap(), "");
}

#[test]
fn relay_client_auth_secret_debug_output_is_redacted_and_zeroizable() {
    let mut auth = WalletConnectRelayClientAuth::from_signing_key([7u8; 32]);

    let debug = format!("{auth:?}");

    assert!(debug.contains("<redacted>"));
    assert!(debug.contains(&auth.client_id));
    assert!(!debug.contains("[7, 7"));

    auth.zeroize();

    assert_eq!(auth.signing_key, [0u8; 32]);
    assert_eq!(auth.client_id, "");
}

#[test]
fn relay_client_auth_jwt_uses_client_auth_claims() {
    let auth = WalletConnectRelayClientAuth::from_signing_key([7u8; 32]);
    let token = auth.client_auth_jwt(WALLETCONNECT_RELAY_URL, 60).unwrap();
    let parts = token.split('.').collect::<Vec<_>>();
    assert_eq!(parts.len(), 3);
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();

    assert_eq!(claims["iss"], auth.client_id);
    assert_ne!(claims["sub"], auth.client_id);
    assert_eq!(claims["aud"], WALLETCONNECT_RELAY_URL);
    assert_eq!(claims["act"], "client_auth");
    assert!(claims["iat"].as_u64().is_some());
    assert!(claims["exp"].as_u64().unwrap() > claims["iat"].as_u64().unwrap());
}

#[test]
fn relay_client_connection_query_includes_project_id_and_auth() {
    let auth = WalletConnectRelayClientAuth::from_signing_key([8u8; 32]);
    let client = WalletConnectRelayClient::new(
        WalletConnectRelayConfig {
            project_id: "project-override".to_owned(),
        },
        auth,
    );
    let query = client.connection_query().unwrap();

    assert!(
        query
            .iter()
            .any(|(key, value)| key == "projectId" && value == "project-override")
    );
    assert!(
        query
            .iter()
            .any(|(key, value)| key == "auth" && value.split('.').count() == 3)
    );
    assert!(
        query
            .iter()
            .any(|(key, value)| key == "ua" && value.starts_with("wc-2/rust-"))
    );
    assert!(
        query
            .iter()
            .any(|(key, value)| key == "useOnCloseEvent" && value == "true")
    );
}

#[test]
fn relay_error_sanitizer_removes_auth_jwts_and_full_queries() {
    let jwt = "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9.eyJhdWQiOiJ3c3M6Ly9yZWxheS53YWxsZXRjb25uZWN0Lm9yZyJ9.c2lnbmF0dXJlLXBheWxvYWQ";
    let raw = format!(
        "websocket upgrade failed: Request {{ url: wss://user:pass@relay.walletconnect.org/?auth={jwt}&projectId=project-override&ua=wc-2/rust-test&useOnCloseEvent=true }} auth={jwt} symKey=000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    );

    let sanitized = sanitize_walletconnect_relay_error_message(&raw);

    assert!(sanitized.contains("wss://relay.walletconnect.org/"));
    assert!(!sanitized.contains("user:pass"));
    assert!(!sanitized.contains("auth="));
    assert!(!sanitized.contains("projectId="));
    assert!(!sanitized.contains("useOnCloseEvent="));
    assert!(!sanitized.contains("symKey="));
    assert!(!sanitized.contains(jwt));
    assert!(
        !sanitized.contains("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
    );
}

#[test]
fn relay_subscription_request_is_acknowledged() {
    let request = json!({
        "id": 44,
        "jsonrpc": "2.0",
        "method": "irn_subscription",
        "params": {
            "id": "sub-1",
            "topic": "topic-a",
            "message": "payload"
        }
    });
    let parsed = WalletConnectRelaySubscriptionRequest::parse(&request)
        .unwrap()
        .expect("subscription request");
    let ack = parsed.ack();

    assert_eq!(parsed.params.topic, "topic-a");
    assert_eq!(parsed.params.message, "payload");
    assert_eq!(serde_json::to_value(&ack).unwrap()["id"], json!(44));
    assert_eq!(ack.result, Some(json!(true)));
}

#[test]
fn relay_subscription_request_accepts_string_jsonrpc_id_and_aave_metadata() {
    let request = json!({
        "id": "aave-push-id",
        "jsonrpc": "2.0",
        "method": "irn_subscription",
        "params": {
            "id": "sub-aave",
            "data": {
                "topic": "5d144cf02e8bdcfc0c2cc59600f99c336a41aa28a385c54deb4d2d6a34aea64b",
                "message": "encrypted-approve-payload",
                "tag": 1108,
                "publishedAt": 1_700_000_000_u64,
                "attestation": {
                    "origin": "https://app.aave.com"
                }
            }
        }
    });
    let parsed = WalletConnectRelaySubscriptionRequest::parse(&request)
        .unwrap()
        .expect("subscription request");
    let ack = serde_json::to_value(parsed.ack()).unwrap();

    assert_eq!(parsed.id, WalletConnectJsonRpcId::from("aave-push-id"));
    assert_eq!(parsed.params.id, "sub-aave");
    assert_eq!(
        parsed.params.topic,
        "5d144cf02e8bdcfc0c2cc59600f99c336a41aa28a385c54deb4d2d6a34aea64b"
    );
    assert_eq!(parsed.params.message, "encrypted-approve-payload");
    assert_eq!(ack["id"], json!("aave-push-id"));
    assert_eq!(ack["result"], json!(true));
}

#[test]
fn relay_subscription_request_accepts_walletconnect_data_shape() {
    let request = json!({
        "id": 45,
        "jsonrpc": "2.0",
        "method": "irn_subscription",
        "params": {
            "id": "sub-2",
            "data": {
                "topic": "topic-b",
                "message": "payload-b",
                "publishedAt": 1_700_000_000_u64
            }
        }
    });
    let parsed = WalletConnectRelaySubscriptionRequest::parse(&request)
        .unwrap()
        .expect("subscription request");

    assert_eq!(parsed.params.id, "sub-2");
    assert_eq!(parsed.params.topic, "topic-b");
    assert_eq!(parsed.params.message, "payload-b");
    assert_eq!(parsed.ack().result, Some(json!(true)));
}
