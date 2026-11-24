use super::*;

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

/// Answers every `UniversalResolver` call with `address`.
fn ens_responder(address: Address) -> crate::rpc_broker::tests::RpcResponder {
    use alloy::ens::{EnsResolver, UniversalResolver};
    Arc::new(move |request: Value| {
        let result =
            UniversalResolver::resolveCall::abi_encode_returns(&UniversalResolver::resolveReturn {
                _0: EnsResolver::addrCall::abi_encode_returns(&address).into(),
                _1: Address::repeat_byte(8),
            });
        json!({"jsonrpc":"2.0", "id":request["id"], "result":Bytes::from(result)})
    })
}

/// A mock RPC that answers every `UniversalResolver` call with `address`.
async fn spawn_ens_mock(address: Address) -> (Url, tokio::task::JoinHandle<()>) {
    crate::rpc_broker::tests::spawn_rpc_mock(ens_responder(address), Arc::default(), Arc::default())
        .await
}

#[tokio::test]
async fn ens_recipient_needs_two_distinct_endpoints_to_agree() {
    let target = Address::repeat_byte(9);
    let (first, first_server) = spawn_ens_mock(target).await;
    let (second, second_server) = spawn_ens_mock(target).await;
    let (other, other_server) = spawn_ens_mock(Address::repeat_byte(7)).await;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let refused = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    drop(listener);
    let http = HttpContext::direct_for_tests();
    let client = http.operation_http_client().await.unwrap();
    for (endpoints, expected) in [
        (vec![first.clone(), other], None),
        // A refused endpoint gives no answer, so another one is asked.
        (vec![refused.clone(), first.clone(), second], Some(target)),
        // A URL listed twice is one endpoint, so its answer alone is not enough.
        (vec![first.clone(), first.clone(), refused], None),
    ] {
        let mut chain = effective_chain_for_rpc(first.as_str(), 0);
        chain.rpc_route = RpcChainRoute::new(1, endpoints);
        let resolved = resolve_public_ens_recipient("raffy.eth", &chain, &http, &client).await;
        assert_eq!(resolved.ok(), expected);
    }
    for server in [first_server, second_server, other_server] {
        server.abort();
    }
}

#[tokio::test]
async fn ens_lookup_asks_no_further_endpoint_while_one_is_pending() {
    use crate::rpc_broker::tests::{RpcMockGate, spawn_gated_rpc_mock, spawn_recording_rpc_mock};
    let target = Address::repeat_byte(9);
    let (first, first_server) = spawn_ens_mock(target).await;
    let gate = RpcMockGate::default();
    let (held, held_server) = spawn_gated_rpc_mock(
        ens_responder(target),
        Arc::default(),
        Arc::default(),
        gate.clone(),
    )
    .await;
    let (third, third_requests, third_server) =
        spawn_recording_rpc_mock(ens_responder(target)).await;
    let http = HttpContext::direct_for_tests();
    let client = http.operation_http_client().await.unwrap();
    let mut chain = effective_chain_for_rpc(first.as_str(), 0);
    chain.rpc_route = RpcChainRoute::new(1, vec![first, held, third]);
    let mut lookup = std::pin::pin!(resolve_public_ens_recipient(
        "raffy.eth",
        &chain,
        &http,
        &client
    ));
    tokio::select! {
        () = gate.request_started.notified() => {}
        resolved = &mut lookup => {
            panic!("the lookup finished before the held endpoint was asked: {resolved:?}")
        }
    }
    // The lookup waits for the held endpoint instead of asking the third one.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), &mut lookup)
            .await
            .is_err()
    );
    gate.release_response.notify_one();
    assert_eq!(lookup.await.unwrap(), target);
    assert!(third_requests.lock().unwrap().is_empty());
    for server in [first_server, held_server, third_server] {
        server.abort();
    }
}
