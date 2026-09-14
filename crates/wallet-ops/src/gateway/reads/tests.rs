use super::*;
use crate::gateway::errors::LocalProviderFailure;
use crate::gateway::provider::tests::{authorize, fixture};
use crate::gateway::provider::{DappProvider, DeliveryStatus};
use crate::rpc_broker::tests::spawn_rpc_mock;
use crate::rpc_broker::{RpcRemoteError, RpcRevert, WalletRpcOrigin};
use alloy::primitives::{Address, B256, Bytes};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use url::Url;

fn origin() -> RpcOrigin {
    RpcOrigin::dapp("authenticated-test-peer", "https://dapp.example").unwrap()
}

fn read(method: &str, params: Value) -> RpcRead {
    parse_read(method, params, 1).expect("accepted broker parameters")
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

async fn provider_response(provider: &mut DappProvider, method: &str, params: Value) -> Value {
    provider
        .request(
            1,
            "doc".to_owned(),
            method.to_owned(),
            method,
            params,
            Instant::now(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(completion) = provider.jobs.join_next().await {
            provider.complete_read(completion.unwrap());
        }
    })
    .await
    .expect("provider reads completed");
    let mut response = None;
    loop {
        let messages = provider.drain();
        if messages.is_empty() {
            break;
        }
        for (session, mut delivery) in messages {
            let status = provider.delivery(session, &mut delivery);
            let ticket = delivery.ticket_id();
            if status != DeliveryStatus::Discard {
                let message = serde_json::to_value(delivery.message).unwrap();
                if message["type"] == "provider_response" {
                    assert_eq!(session, 1);
                    assert_eq!(message["document"], "doc");
                    assert_eq!(message["request_id"], method);
                    assert!(response.replace(message).is_none(), "duplicate response");
                }
            }
            if let Some(id) = ticket {
                provider.delivered(id);
            }
        }
    }
    response.expect("delivered provider response")
}

#[tokio::test]
async fn full_method_matrix_preserves_wire_context_and_ordered_results() {
    let address = Address::ZERO;
    let hash = B256::ZERO;
    let block_id = json!({"blockHash": hash, "requireCanonical": true});
    let transaction = json!({
        "from": address, "data": "0x0001", "input": "0x0001", "chainId": "0x1",
        "gas": "0x5208", "value": "0x1", "nonce": "0x0", "type": "0x2",
        "maxFeePerGas": "0xa", "maxPriorityFeePerGas": "0x1",
        "accessList": [{"address": address, "storageKeys": [hash]}],
        "context": {"nested": [null, "private-fixture"]}
    });
    let pending_transaction = json!({
        "type": "0x2", "hash": hash, "from": address, "to": null,
        "nonce": "0x0", "gas": "0x5208", "value": "0x0", "input": "0x0001",
        "chainId": "0x1", "maxFeePerGas": "0xa", "maxPriorityFeePerGas": "0x1",
        "accessList": [], "v": "0x0", "r": "0x1", "s": "0x1", "yParity": "0x0",
        "blockHash": null, "blockNumber": null, "transactionIndex": null,
        "extension": {"nested": [null, "private-fixture"]}
    });
    let block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
    let mut block = serde_json::to_value(block).unwrap();
    block["transactions"] = json!([pending_transaction]);
    block["extension"] = json!({"private-fixture": [null, 1]});
    let cases = vec![
        (
            "eth_call",
            json!([transaction, block_id, {(address.to_string()): {"balance": "0x2", "code": "0x6000"}}]),
            json!("0x0001"),
        ),
        ("eth_getBalance", json!([address, block_id]), json!("0x2")),
        ("eth_blockNumber", json!([]), json!("0x3")),
        ("eth_getCode", json!([address, block_id]), json!("0x6000")),
        (
            "eth_getStorageAt",
            json!([address, "0x1", block_id]),
            json!(hash),
        ),
        (
            "eth_getTransactionCount",
            json!([address, block_id]),
            json!("0x4"),
        ),
        ("eth_getBlockByHash", json!([hash, true]), block),
        (
            "eth_getBlockByNumber",
            json!(["pending", false]),
            Value::Null,
        ),
        (
            "eth_getBlockTransactionCountByHash",
            json!([hash]),
            Value::Null,
        ),
        (
            "eth_getBlockTransactionCountByNumber",
            json!(["safe"]),
            json!("0x5"),
        ),
        (
            "eth_getTransactionByHash",
            json!([hash]),
            pending_transaction.clone(),
        ),
        ("eth_getTransactionReceipt", json!([hash]), Value::Null),
        (
            "eth_getTransactionByBlockHashAndIndex",
            json!([hash, "0x0"]),
            pending_transaction,
        ),
        (
            "eth_getTransactionByBlockNumberAndIndex",
            json!(["latest", "0x1"]),
            Value::Null,
        ),
        ("eth_getBlockReceipts", json!(["finalized"]), json!([])),
        (
            "eth_getLogs",
            json!([{"address": address, "topics": [null, hash], "fromBlock": "0x1", "toBlock": "latest"}]),
            json!([]),
        ),
        ("eth_gasPrice", json!([]), json!("0x6")),
        ("eth_maxPriorityFeePerGas", json!([]), json!("0x7")),
        (
            "eth_feeHistory",
            json!(["0x1", "latest", [50.5]]),
            json!({"oldestBlock": "0x1", "baseFeePerGas": ["0x1", "0x2"], "gasUsedRatio": [0.5], "reward": [["0x1"]], "extension": [null, "private-fixture"]}),
        ),
        (
            "eth_estimateGas",
            json!([transaction, "pending"]),
            json!("0x5208"),
        ),
    ];
    let calls = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = spawn_rpc_mock(
        {
            let cases = cases.clone();
            let calls = calls.clone();
            Arc::new(move |request| {
                calls.fetch_add(1, Ordering::SeqCst);
                let (_, params, result) = cases
                    .iter()
                    .find(|(method, _, _)| request["method"] == *method)
                    .expect("matrix method");
                assert!(request["params"] == *params, "broker wire context changed");
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let reads = cases
        .iter()
        .map(|(method, params, _)| read(method, params.clone()))
        .collect();
    let results = submit_reads(
        HttpContext::direct_for_tests(),
        RpcChainRoute::new(1, vec![endpoint.clone()]),
        origin(),
        reads,
        deadline(),
    )
    .await
    .unwrap();
    assert_eq!(results.len(), cases.len());
    for (result, (_, _, expected)) in results.iter().zip(&cases) {
        assert!(
            result.as_ref().expect("successful member").expose_value() == expected,
            "result JSON changed"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), cases.len());
    assert!(!format!("{results:?}").contains("private-fixture"));
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    for (method, params, expected) in &cases {
        let response = provider_response(&mut provider, method, params.clone()).await;
        assert!(response.get("error").is_none(), "provider read failed");
        assert!(
            response.get("result") == Some(expected),
            "provider result JSON changed"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), cases.len() * 2);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
    server.abort();
}

#[tokio::test]
async fn member_errors_preserve_remote_payloads_and_do_not_replace_successes() {
    let remote = json!({"code": -32602, "message": "private-fixture revert-like fault", "data": {"nested": [null, "0x0001"]}, "extension": ["private-fixture"]});
    let revert = json!({"code": 3, "message": "private-fixture revert", "data": "0x000001ff", "extension": {"context": null}});
    let (endpoint, server) = spawn_rpc_mock(
        {
            let remote = remote.clone();
            let revert = revert.clone();
            Arc::new(move |request| match request["method"].as_str().unwrap() {
                "eth_gasPrice" => json!({"id": request["id"], "error": remote}),
                "eth_call" => json!({"id": request["id"], "error": revert}),
                _ => json!({"id": request["id"], "result": "0x7"}),
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let results = submit_reads(
        HttpContext::direct_for_tests(),
        RpcChainRoute::new(1, vec![endpoint.clone()]),
        origin(),
        vec![
            read("eth_gasPrice", json!([])),
            read("eth_blockNumber", json!([])),
            read("eth_call", json!([{}])),
        ],
        deadline(),
    )
    .await
    .unwrap();
    assert!(matches!(results[0], Err(RpcBrokerError::Remote(_))));
    assert_eq!(results[1].as_ref().unwrap().expose_value(), &json!("0x7"));
    assert!(matches!(results[2], Err(RpcBrokerError::InnerRevert(_))));
    for (index, expected) in [(0, &remote), (2, &revert)] {
        let projected = ProviderRpcError::from_broker(
            results[index].clone().unwrap_err(),
            ProviderAvailability::Available,
        );
        assert!(
            projected.expose_value() == expected,
            "remote object changed"
        );
        assert!(
            serde_json::to_value(&projected).unwrap() == *expected,
            "serialized error changed"
        );
        assert!(!format!("{projected:?} {projected}").contains("private-fixture"));
    }
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    for (method, params, expected) in [
        ("eth_gasPrice", json!([]), remote),
        ("eth_call", json!([{}]), revert),
    ] {
        let response = provider_response(&mut provider, method, params).await;
        assert!(response.get("result").is_none(), "error became a success");
        assert!(
            response.get("error") == Some(&expected),
            "provider error object changed"
        );
    }
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
    server.abort();
}

#[tokio::test]
async fn expired_deadline_and_disallowed_origin_dispatch_nothing() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = spawn_rpc_mock(
        {
            let calls = calls.clone();
            Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Value::Null
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let http = HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(1, vec![endpoint]);
    assert!(matches!(
        submit_reads(
            http.clone(),
            route.clone(),
            origin(),
            vec![read("eth_blockNumber", json!([]))],
            Instant::now()
        )
        .await,
        Err(RpcBrokerError::TimeoutBeforeDispatch)
    ));
    assert!(matches!(
        submit_reads(
            http,
            route,
            WalletRpcOrigin::PublicWallet.into(),
            vec![read("eth_getTransactionByHash", json!([B256::ZERO]))],
            deadline()
        )
        .await,
        Err(RpcBrokerError::OriginRejected)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn queue_wait_uses_the_original_deadline() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let http = HttpContext::direct_for_tests();
    let accepted = Instant::now();
    let deadline = accepted + Duration::from_secs(30);
    tokio::time::advance(Duration::from_secs(29)).await;
    let result = submit_reads(
        http,
        RpcChainRoute::new(1, vec![endpoint]),
        origin(),
        vec![read("eth_blockNumber", json!([]))],
        deadline,
    )
    .await;
    assert!(match result {
        Err(RpcBrokerError::Timeout) => true,
        Ok(members) => members.iter().all(|member| matches!(
            member,
            Err(RpcBrokerError::Timeout | RpcBrokerError::TimeoutBeforeDispatch)
        )),
        _ => false,
    });
    assert!(
        Instant::now() <= deadline + Duration::from_millis(1),
        "queue wait reset the request budget"
    );
}

#[test]
fn error_projection_uses_typed_failures_and_owner_availability() {
    for (failure, code) in [
        (LocalProviderFailure::UserRejected, 4001),
        (LocalProviderFailure::Unauthorized, 4100),
        (LocalProviderFailure::Unsupported, 4200),
        (LocalProviderFailure::Disconnected, 4900),
        (LocalProviderFailure::ChainUnavailable, 4901),
        (LocalProviderFailure::InvalidParams, -32602),
        (LocalProviderFailure::InvalidInput, -32000),
        (LocalProviderFailure::NotFound, -32001),
        (LocalProviderFailure::Unavailable, -32002),
        (LocalProviderFailure::TransactionRejected, -32003),
        (LocalProviderFailure::LimitExceeded, -32005),
        (LocalProviderFailure::Internal, -32603),
    ] {
        assert_eq!(ProviderRpcError::local(failure).into_value()["code"], code);
    }
    assert_eq!(
        ProviderRpcError::from_broker(
            RpcBrokerError::ResponseTooLarge,
            ProviderAvailability::Available,
        )
        .into_value()["code"],
        -32005
    );
    for (availability, expected) in [
        (ProviderAvailability::Available, -32002),
        (ProviderAvailability::Disconnected, 4900),
        (ProviderAvailability::ChainUnavailable, 4901),
    ] {
        for error in [
            RpcBrokerError::Transport,
            RpcBrokerError::HttpStatus(503),
            RpcBrokerError::NoEndpoint { chain_id: 1 },
            RpcBrokerError::Shutdown,
        ] {
            assert_eq!(
                ProviderRpcError::from_broker(error, availability).into_value()["code"],
                expected
            );
        }
        assert_eq!(
            ProviderRpcError::from_broker(RpcBrokerError::Timeout, availability).into_value()["code"],
            -32002
        );
    }
    for (params, expected) in [
        (json!([{"input": "0x01", "data": "0x02"}]), -32602),
        (json!([{"chainId": "0x2"}]), -32000),
    ] {
        assert_eq!(
            parse_read("eth_call", params, 1).unwrap_err().into_value()["code"],
            expected
        );
    }
    let original = json!({"code": -32002, "message": "private-fixture", "data": "0x000001ff", "extra": {"nested": [null]}});
    let remote = RpcRemoteError::from(
        serde_json::from_value::<
            alloy::serde::WithOtherFields<alloy::rpc::json_rpc::ErrorPayload<Value>>,
        >(original.clone())
        .unwrap(),
    );
    let projected = ProviderRpcError::from_broker(
        RpcBrokerError::Remote(remote),
        ProviderAvailability::Disconnected,
    );
    assert!(
        projected.into_value() == original,
        "hexadecimal provider fault was reclassified"
    );
    let projected = ProviderRpcError::from_broker(
        RpcBrokerError::InnerRevert(RpcRevert::from_multicall(Bytes::from_static(&[
            0, 0, 1, 255,
        ]))),
        ProviderAvailability::Available,
    );
    assert_eq!(
        projected.into_value(),
        json!({"code": 3, "message": "execution reverted", "data": "0x000001ff"})
    );
}
