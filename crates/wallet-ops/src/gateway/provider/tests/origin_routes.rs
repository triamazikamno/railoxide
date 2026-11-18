use super::*;

/// A shared context whose wallet and direct clients are tagged `shared` and `direct`, and whose
/// operation clients are tagged `op-0`, `op-1` and so on in opening order.
fn tagged_operation_context() -> HttpContext {
    use crate::rpc_broker::tests::tagged_reqwest_client;
    const TAGS: [&str; 8] = [
        "op-0", "op-1", "op-2", "op-3", "op-4", "op-5", "op-6", "op-7",
    ];
    let opened = std::sync::atomic::AtomicUsize::new(0);
    HttpContext::with_rpc_clients_for_tests(
        tagged_reqwest_client("shared"),
        tagged_reqwest_client("direct"),
        crate::WalletNetworkMode::Direct,
    )
    .with_operation_clients_for_tests(move || {
        let tag = TAGS[opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst)];
        Ok(crate::OperationHttpClient::for_tests(
            tagged_reqwest_client(tag),
            crate::OperationNetworkIsolation::Dedicated,
        ))
    })
}

fn route_tags(recorded: &crate::rpc_broker::tests::RecordedRequests) -> Vec<String> {
    recorded
        .lock()
        .unwrap()
        .iter()
        .map(|(tag, _)| tag.clone().unwrap_or_default())
        .collect()
}

fn block_call(block: &str) -> Value {
    json!([{"to": alloy::primitives::Address::repeat_byte(1), "data": "0x01"}, block])
}

/// Sends one read per `(session, params)` from each session's `doc` before completing any, and
/// returns the responses.
async fn origin_reads(
    provider: &mut DappProvider,
    method: &str,
    reads: &[(u64, Value)],
) -> Vec<Value> {
    for (index, (session, params)) in reads.iter().enumerate() {
        provider
            .request(
                *session,
                "doc".into(),
                format!("read-{index}"),
                method,
                params.clone(),
                Instant::now(),
            )
            .unwrap();
    }
    while !provider.jobs.is_empty() {
        let completion = tokio::time::timeout(Duration::from_secs(30), provider.jobs.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        provider.complete_read(completion);
    }
    let responses: Vec<_> = messages(provider)
        .into_iter()
        .filter(|(_, message)| message["type"] == "provider_response")
        .map(|(_, message)| message)
        .collect();
    assert_eq!(responses.len(), reads.len());
    responses
}

#[tokio::test]
async fn each_origin_reads_over_its_own_client_while_it_keeps_a_document_and_its_grant() {
    use alloy::{eips::BlockNumHash, primitives::B256};
    let hash = B256::repeat_byte(7);
    let block = BlockNumHash::new(12, B256::repeat_byte(12));
    let (canonical, transaction, _) = included_lookup_payloads(hash, block);
    let (endpoint, recorded, server) = crate::rpc_broker::tests::spawn_recording_rpc_mock({
        let transaction = transaction.clone();
        Arc::new(move |request: Value| {
            let result = match request["method"].as_str().unwrap() {
                "eth_getBlockByNumber" => canonical.clone(),
                "eth_getTransactionByBlockHashAndIndex" => transaction.clone(),
                _ => json!("0x01"),
            };
            json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
        })
    })
    .await;
    let mut seen = 0;
    let mut sent = || {
        let tags = route_tags(&recorded);
        let fresh = tags[seen..].to_vec();
        seen = tags.len();
        fresh
    };
    let (path, mut provider, view) = fixture();
    let (peer, permission) = authorize(&mut provider, &view, endpoint);
    let mut wallet = provider.wallet.clone();
    wallet.http = Some(tagged_operation_context());
    provider.update_wallet(wallet.clone(), 3);
    // Session 2 holds a second document of the same origin. Session 3 holds another origin,
    // granted another account.
    let url = permission.origin.web_origin().unwrap().as_str().to_owned();
    provider.register(2, peer, "doc".into(), &url).unwrap();
    let other = provider
        .store
        .import_public_account(
            PASSWORD,
            &view,
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            None,
            false,
        )
        .unwrap();
    let other_origin = RpcOrigin::dapp(
        alloy::hex::encode(peer.to_bytes()),
        "https://other.invalid/",
    )
    .unwrap();
    provider
        .store
        .grant_gateway_permission(&view, &other_origin, &other.public_account_uuid, 1)
        .unwrap();
    provider
        .register(3, peer, "doc".into(), "https://other.invalid/")
        .unwrap();
    messages(&mut provider);

    // Identical concurrent reads of two origins are neither deduplicated nor sent by one client.
    for response in origin_reads(
        &mut provider,
        "eth_call",
        &[(1, block_call("0x5")), (3, block_call("0x5"))],
    )
    .await
    {
        assert_eq!(response["result"], "0x01");
    }
    let mut together = sent();
    together.sort();
    assert_eq!(together, ["op-0", "op-1"]);
    // The origin's other document is answered from the origin's own cached result.
    origin_reads(&mut provider, "eth_call", &[(2, block_call("0x5"))]).await;
    assert_eq!(sent(), [] as [String; 0]);
    origin_reads(&mut provider, "eth_call", &[(1, block_call("0x6"))]).await;
    let [own] = <[String; 1]>::try_from(sent()).unwrap();
    origin_reads(&mut provider, "eth_call", &[(2, block_call("0x7"))]).await;
    assert_eq!(sent(), std::slice::from_ref(&own));

    // A tracked transaction's included answer reads its block over the origin's client.
    let (tracker, context) = crate::public_wallet::test_tracking_context();
    provider.wallet.public_transaction_tracker = tracker;
    let family = context.start_family();
    family.register(hash);
    family.included(hash, block, 1);
    let lookup = origin_reads(
        &mut provider,
        "eth_getTransactionByHash",
        &[(1, json!([hash]))],
    )
    .await;
    assert_eq!(lookup[0]["result"], transaction);
    assert_eq!(sent(), [own.clone(), own.clone()]);
    drop(family);

    // The route outlives one of the origin's documents, but not its last one.
    provider.unregister(2, "doc");
    origin_reads(&mut provider, "eth_call", &[(1, block_call("0x8"))]).await;
    assert_eq!(sent(), [own]);
    provider.unregister(1, "doc");
    provider.register(1, peer, "doc".into(), &url).unwrap();
    messages(&mut provider);
    origin_reads(&mut provider, "eth_call", &[(1, block_call("0x9"))]).await;
    assert_eq!(sent(), ["op-2"]);
    // Locking closes the route.
    provider.update_wallet(GatewayWalletState::default(), 4);
    provider.update_wallet(wallet, 5);
    messages(&mut provider);
    origin_reads(&mut provider, "eth_call", &[(1, block_call("0xa"))]).await;
    assert_eq!(sent(), ["op-3"]);
    // A grant of another account replaces it.
    provider
        .store
        .grant_gateway_permission(&view, &permission.origin, &other.public_account_uuid, 1)
        .unwrap();
    origin_reads(&mut provider, "eth_call", &[(1, block_call("0xb"))]).await;
    assert_eq!(sent(), ["op-4"]);
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn origin_route_follows_an_endpoint_moved_from_direct_to_wallet_transport() {
    let (endpoint, recorded, server) = crate::rpc_broker::tests::spawn_recording_rpc_mock(
        Arc::new(|request: Value| json!({"jsonrpc":"2.0", "id":request["id"], "result":"0x01"})),
    )
    .await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint.clone());
    let route = |transport| {
        RpcChainRoute::new_with_transports(
            1,
            vec![(
                endpoint.clone().into(),
                transport,
                crate::RpcEndpointRoles::default(),
            )],
        )
    };
    let mut wallet = provider.wallet.clone();
    wallet.http = Some(tagged_operation_context());
    wallet.routes.insert(1, route(crate::RpcTransport::Direct));
    provider.update_wallet(wallet.clone(), 3);
    origin_reads(&mut provider, "eth_call", &[(1, block_call("0x5"))]).await;
    wallet.routes.insert(1, route(crate::RpcTransport::Wallet));
    provider.update_wallet(wallet, 4);
    origin_reads(&mut provider, "eth_call", &[(1, block_call("0x6"))]).await;
    // The edited route is rebuilt over the origin's existing client.
    assert_eq!(route_tags(&recorded), ["direct", "op-0"]);
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn origin_read_fails_without_shared_fallback_when_its_client_cannot_open() {
    let (endpoint, recorded, server) = crate::rpc_broker::tests::spawn_recording_rpc_mock(
        Arc::new(|request: Value| json!({"jsonrpc":"2.0", "id":request["id"], "result":"0x01"})),
    )
    .await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let mut wallet = provider.wallet.clone();
    wallet.http = Some(
        HttpContext::with_rpc_client_for_tests(
            crate::rpc_broker::tests::tagged_reqwest_client("shared"),
            crate::WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(|| Err(eyre::eyre!("synthetic open failure"))),
    );
    provider.update_wallet(wallet, 3);
    let failed = origin_reads(&mut provider, "eth_call", &[(1, block_call("0x5"))]).await;
    assert!(failed[0]["error"].is_object());
    assert!(recorded.lock().unwrap().is_empty());
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}
