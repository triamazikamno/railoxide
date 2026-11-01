use super::*;
use crate::rpc_broker::tests::{
    RecordedRequests, RpcResponder, aggregate_response, rpc_result, spawn_recording_rpc_mock,
    test_origin,
};
use crate::rpc_broker::{RpcRead, RpcRoute, RpcSubmission, RpcTrafficClass};
use crate::settings::{TronEndpointSetting, TronSettings};
use crate::tron::http_api::TronHttpApi;
use crate::tron::tron_rpc_route;
use alloy::primitives::{Address, Bytes, U256};

const ACCOUNT: Address = Address::repeat_byte(7);

/// Responses recorded from mainnet and Nile.
fn fixture(name: &str) -> Value {
    let fixtures: Value =
        serde_json::from_str(include_str!("../../tron/fixtures/http_api.json")).unwrap();
    fixtures[name].clone()
}

// TronGrid's keyless rate-limit answer, recorded with HTTP status 200.
fn rate_limit_body() -> Value {
    fixture("trongrid_rate_limit")
}

fn word(value: u64) -> Bytes {
    Bytes::from(U256::from(value).to_be_bytes::<32>().to_vec())
}

/// A JSON-RPC endpoint reporting `chain_id` that answers every balance and Multicall3 read with
/// `5`.
fn json_rpc(chain_id: &'static str) -> RpcResponder {
    Arc::new(move |request| match request["method"].as_str() {
        Some("eth_chainId") => rpc_result(&request, &json!(chain_id)),
        Some("eth_getBalance") => rpc_result(&request, &json!(U256::from(5))),
        _ => aggregate_response(&request, vec![(true, word(5))]),
    })
}

/// A Tron JSON-RPC endpoint whose reads are rate limited.
fn rate_limited_json_rpc() -> RpcResponder {
    let limited = rate_limit_body();
    Arc::new(move |request| match request["method"].as_str() {
        Some("eth_chainId") => rpc_result(&request, &json!("0x2b6653dc")),
        _ => limited.clone(),
    })
}

/// An HTTP API answering block 0 with the recorded `genesis` and account reads with `account`.
fn http_api(genesis: &str, account: Value) -> RpcResponder {
    let genesis = fixture(genesis);
    Arc::new(move |request| {
        if request.get("num").is_some() {
            genesis.clone()
        } else {
            account.clone()
        }
    })
}

fn mainnet_api(account: Value) -> RpcResponder {
    http_api("getblockbynum_0", account)
}

fn existing_account() -> Value {
    fixture("getaccount")
}

async fn spawn(responder: RpcResponder) -> (Url, RecordedRequests) {
    let (url, recorded, _task) = spawn_recording_rpc_mock(responder).await;
    (url, recorded)
}

fn endpoint(
    rpc: &Url,
    api: Option<&Url>,
) -> (
    SensitiveUrl,
    Option<SensitiveUrl>,
    RpcTransport,
    RpcEndpointRoles,
) {
    (
        rpc.clone().into(),
        api.cloned().map(SensitiveUrl::from),
        RpcTransport::Wallet,
        RpcEndpointRoles::default(),
    )
}

/// Requests other than identity checks.
fn reads(recorded: &RecordedRequests) -> usize {
    recorded
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, body)| body["method"] != "eth_chainId" && body.get("num").is_none())
        .count()
}

async fn read_balance(http: &HttpContext, route: ClassRoute) -> Value {
    let results = http
        .rpc_broker()
        .submit(RpcSubmission::new(
            RpcRoute::from(route),
            vec![RpcRead::get_balance(ACCOUNT)],
            test_origin(),
        ))
        .await
        .unwrap();
    results[0].as_ref().unwrap().expose_value().clone()
}

#[tokio::test]
async fn endpoints_failing_either_identity_check_are_not_used() {
    let (testnet_rpc, _) = spawn(json_rpc("0xcd8690dc")).await;
    let (paired_with_nile_rpc, _) = spawn(json_rpc("0x2b6653dc")).await;
    let (mainnet_rpc, _) = spawn(json_rpc("0x2b6653dc")).await;
    let (nile_api, _) = spawn(http_api("getblockbynum_0_nile", existing_account())).await;
    let (mainnet_http_api, _) = spawn(mainnet_api(existing_account())).await;
    let route = RpcChainRoute::new_tron(vec![
        endpoint(&testnet_rpc, Some(&mainnet_http_api)),
        endpoint(&paired_with_nile_rpc, Some(&nile_api)),
        endpoint(&mainnet_rpc, Some(&mainnet_http_api)),
    ]);
    let clients = HttpContext::direct_for_tests().rpc_clients();

    for (endpoint, expected) in route.endpoints().iter().zip([
        Err(RpcBrokerError::InvalidResponse),
        Err(RpcBrokerError::InvalidResponse),
        Ok(()),
    ]) {
        assert_eq!(
            route
                .verify_endpoint_identity(&clients, endpoint, Duration::from_secs(10))
                .await,
            expected
        );
    }
    let verified = route.verify_identity(&clients).await.unwrap();
    assert_eq!(verified.configured_endpoints(), &route.endpoints()[2..]);
    assert_eq!(
        verified.tron_http_api(&verified.endpoints()[0]),
        Some(&SensitiveUrl::from(mainnet_http_api))
    );
}

#[tokio::test]
async fn reads_reach_only_endpoints_of_their_class() {
    let (general_rpc, general_rpc_log) = spawn(json_rpc("0x2b6653dc")).await;
    let (general_api, general_api_log) = spawn(mainnet_api(existing_account())).await;
    let (sensitive_rpc, sensitive_rpc_log) = spawn(json_rpc("0x2b6653dc")).await;
    let (sensitive_api, sensitive_api_log) = spawn(mainnet_api(existing_account())).await;
    let mut settings = TronSettings {
        endpoints: Some(vec![
            TronEndpointSetting {
                json_rpc_url: general_rpc.to_string(),
                http_api_url: Some(general_api.to_string()),
                sensitive: false,
                submit: false,
                ..TronEndpointSetting::default()
            },
            TronEndpointSetting {
                json_rpc_url: sensitive_rpc.to_string(),
                http_api_url: Some(sensitive_api.to_string()),
                general: false,
                ..TronEndpointSetting::default()
            },
        ]),
        ..TronSettings::default()
    };
    let route = tron_rpc_route(&settings).unwrap();
    let http = HttpContext::direct_for_tests();

    // A balance and a Send read.
    let sensitive = route.for_class(RpcTrafficClass::Sensitive).unwrap();
    assert_eq!(read_balance(&http, sensitive).await, json!(U256::from(5)));
    assert!(
        TronHttpApi::new(http.clone(), route.clone())
            .account_exists(ACCOUNT)
            .await
            .unwrap()
    );
    assert_eq!(reads(&sensitive_rpc_log), 1);
    assert_eq!(reads(&sensitive_api_log), 1);
    assert!(general_rpc_log.lock().unwrap().is_empty());
    assert!(general_api_log.lock().unwrap().is_empty());

    // A price read.
    let price = RpcRead::eth_call(Address::repeat_byte(9), Bytes::new());
    let results = http
        .rpc_broker()
        .submit(RpcSubmission::new(
            RpcRoute::from(route.for_class(RpcTrafficClass::General).unwrap()),
            vec![price],
            test_origin(),
        ))
        .await
        .unwrap();
    assert!(results[0].is_ok());
    assert_eq!(reads(&general_rpc_log), 1);
    assert_eq!(reads(&sensitive_rpc_log), 1);

    settings.enabled = false;
    assert!(tron_rpc_route(&settings).is_none());
}

#[tokio::test]
async fn rate_limited_json_rpc_fails_over() {
    let (limited_rpc, limited_log) = spawn(rate_limited_json_rpc()).await;
    let (serving_rpc, serving_log) = spawn(json_rpc("0x2b6653dc")).await;
    let route = RpcChainRoute::new_tron(vec![
        endpoint(&limited_rpc, None),
        endpoint(&serving_rpc, None),
    ]);
    let http = HttpContext::direct_for_tests();
    // Verified up front, so the read tries the endpoints in configured order.
    for endpoint in route.endpoints() {
        route
            .verify_endpoint_identity(&http.rpc_clients(), endpoint, Duration::from_secs(10))
            .await
            .unwrap();
    }

    let sensitive = route.for_class(RpcTrafficClass::Sensitive).unwrap();
    assert_eq!(read_balance(&http, sensitive).await, json!(U256::from(5)));
    assert_eq!(reads(&limited_log), 1);
    assert_eq!(reads(&serving_log), 1);
}

#[tokio::test]
async fn rate_limited_http_api_fails_over() {
    let (limited_rpc, _) = spawn(json_rpc("0x2b6653dc")).await;
    let (limited_api, limited_api_log) = spawn(mainnet_api(rate_limit_body())).await;
    let (serving_rpc, _) = spawn(json_rpc("0x2b6653dc")).await;
    let (serving_api, serving_api_log) = spawn(mainnet_api(existing_account())).await;
    let route = RpcChainRoute::new_tron(vec![
        endpoint(&limited_rpc, Some(&limited_api)),
        endpoint(&serving_rpc, Some(&serving_api)),
    ]);

    let api = TronHttpApi::new(HttpContext::direct_for_tests(), route);
    assert!(api.account_exists(ACCOUNT).await.unwrap());
    assert_eq!(reads(&limited_api_log), 1);
    assert_eq!(reads(&serving_api_log), 1);
}

#[tokio::test(start_paused = true)]
async fn requests_to_one_tron_endpoint_are_spaced() {
    let first = Url::parse("https://first.invalid").unwrap();
    let second = Url::parse("https://second.invalid").unwrap();
    let route = RpcChainRoute::new_tron(vec![endpoint(&first, None), endpoint(&second, None)]);
    // Class routes share the pacing of the route they come from.
    let sensitive = route.for_class(RpcTrafficClass::Sensitive).unwrap();

    let start = Instant::now();
    route.pace(&first).await;
    sensitive.route().pace(&first).await;
    route.pace(&first).await;
    let paced = Instant::now();
    assert!(paced - start >= 2 * TRON_REQUEST_INTERVAL);
    // Another endpoint, and any EVM route, don't wait.
    route.pace(&second).await;
    RpcChainRoute::new(1, vec![first.clone()])
        .pace(&first)
        .await;
    assert_eq!(Instant::now(), paced);
}
