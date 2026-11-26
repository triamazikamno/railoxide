use super::*;

#[tokio::test]
async fn accepted_reads_drain_after_lock_revoke_and_document_retirement_without_old_payloads() {
    for remote_error in [false, true] {
        for invalidation in ["lock", "authority", "revoke", "disconnect", "network"] {
            let (path, mut provider, view) = fixture();
            let (endpoint, started, release, server) = held_rpc(remote_error).await;
            let (peer, permission) = authorize(&mut provider, &view, endpoint);
            let now = Instant::now();
            assert_eq!(
                request(&mut provider, 1, "doc", "delayed", "eth_blockNumber", now),
                [] as [(u64, Value); 0]
            );
            tokio::time::timeout(Duration::from_secs(5), started.notified())
                .await
                .unwrap();
            let id = *provider.reads.keys().next().unwrap();
            match invalidation {
                "lock" => provider.update_wallet(GatewayWalletState::default(), 3),
                "authority" => {
                    invalidate_authority(&provider, true);
                    provider.tick(Instant::now());
                }
                "revoke" => provider.revoke(&permission.permission_id).unwrap(),
                "network" => {
                    let mut wallet = provider.wallet.clone();
                    wallet.http = Some(HttpContext::direct_for_tests());
                    provider.update_wallet(wallet, 3);
                }
                _ => provider.retire_sessions(|_| false),
            }
            let retired = messages(&mut provider);
            if invalidation != "disconnect" {
                assert_eq!(
                    result(&retired)["error"]["code"],
                    if invalidation == "network" {
                        -32002
                    } else {
                        4100
                    }
                );
            }
            assert!(provider.reads[&id].retired);
            assert!(provider.reads[&id].phase == ReadPhase::Running);
            // Thirty-one other tickets fit. The invalidated broker submission still owns one.
            let mut charged = Vec::new();
            for _ in 0..31 {
                let ReadAdmissionDecision::Ready(ticket) = provider
                    .admission
                    .admit(permission.origin.clone(), Instant::now())
                    .unwrap()
                else {
                    panic!("remaining active slot");
                };
                charged.push(ticket);
            }
            let ReadAdmissionDecision::Queued(queued) = provider
                .admission
                .admit(permission.origin.clone(), Instant::now())
                .unwrap()
            else {
                panic!("retired submission must remain charged");
            };
            provider.admission.cancel_queued(queued.id);
            provider
                .register(
                    2,
                    peer,
                    "new".to_owned(),
                    permission.origin.web_origin().unwrap().as_str(),
                )
                .unwrap();
            messages(&mut provider);
            release.notify_one();
            let completion = provider.jobs.join_next().await.unwrap().unwrap();
            provider.complete_read(completion);
            assert_eq!(messages(&mut provider), [] as [(u64, Value); 0]);
            assert!(!provider.reads.contains_key(&id));
            assert!(matches!(
                provider
                    .admission
                    .admit(permission.origin.clone(), Instant::now()),
                Ok(ReadAdmissionDecision::Ready(_))
            ));
            for ticket in charged {
                provider.admission.complete(ticket.id, Instant::now());
            }
            server.await.unwrap();
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

#[tokio::test]
async fn queued_reads_keep_the_entry_deadline_and_expiry_never_dispatches() {
    for invalidation in ["none", "deadline", "authority"] {
        let (path, mut provider, view) = fixture();
        let (endpoint, started, release, server) = held_rpc(false).await;
        authorize(&mut provider, &view, endpoint);
        let now = Instant::now() - Duration::from_secs(5);
        // Hold actual local response delivery, filling the same admission as remote work.
        for index in 0..32 {
            provider
                .request(
                    1,
                    "doc".to_owned(),
                    format!("local-{index}"),
                    "eth_accounts",
                    json!([]),
                    now,
                )
                .unwrap();
        }
        provider
            .request(
                1,
                "doc".to_owned(),
                "queued".to_owned(),
                "eth_blockNumber",
                json!([]),
                now,
            )
            .unwrap();
        let id = *provider
            .reads
            .iter()
            .find(|(_, read)| read.owner.request_id == "queued")
            .unwrap()
            .0;
        let deadline = provider.reads[&id].ticket.deadline;
        assert!(provider.reads[&id].phase == ReadPhase::Queued);
        for duplicate in ["local-0", "queued"] {
            assert!(
                provider
                    .request(
                        1,
                        "doc".to_owned(),
                        duplicate.to_owned(),
                        "eth_accounts",
                        json!([]),
                        now
                    )
                    .is_err()
            );
        }
        // Another web origin retains its independent admission while this origin waits.
        provider
            .register(
                2,
                PeerId::from_bytes([8; 16]),
                "other".to_owned(),
                "https://other.invalid/",
            )
            .unwrap();
        provider
            .request(
                2,
                "other".to_owned(),
                "independent".to_owned(),
                "eth_accounts",
                json!([]),
                now,
            )
            .unwrap();
        assert!(provider.reads.values().any(
            |read| read.owner.request_id == "independent" && read.phase == ReadPhase::Delivery
        ));
        if invalidation == "none" {
            messages(&mut provider);
            assert!(provider.reads[&id].phase == ReadPhase::Running);
            assert_eq!(provider.reads[&id].ticket.deadline, deadline);
            started.notified().await;
            release.notify_one();
            let completion = provider.jobs.join_next().await.unwrap().unwrap();
            provider.complete_read(completion);
            let output = messages(&mut provider);
            assert!(
                output
                    .iter()
                    .any(|(_, message)| message["request_id"] == "queued"
                        && message["result"] == "0xfeed")
            );
            server.await.unwrap();
        } else {
            if invalidation == "authority" {
                invalidate_authority(&provider, true);
                provider.tick(Instant::now());
            } else {
                provider.tick(now + Duration::from_secs(30));
            }
            assert!(provider.reads[&id].retired);
            assert!(provider.reads[&id].phase == ReadPhase::Delivery);
            assert!(provider.jobs.is_empty());
            assert!(result(&messages(&mut provider))["error"].is_object());
            server.abort();
        }
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn authorized_completion_preserves_remote_payload_across_chain_addition_and_selection() {
    for remote_error in [false, true] {
        let (path, mut provider, view) = fixture();
        let (endpoint, started, release, server) = held_rpc(remote_error).await;
        authorize(&mut provider, &view, endpoint);
        request(
            &mut provider,
            1,
            "doc",
            "read",
            "eth_blockNumber",
            Instant::now(),
        );
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        let mut wallet = provider.wallet.clone();
        wallet.default_chain_id = Some(10);
        wallet.chain_ids.push(9_007_199_254_740_993);
        wallet.routes.insert(
            9_007_199_254_740_993,
            RpcChainRoute::new(
                9_007_199_254_740_993,
                vec![reqwest::Url::parse("https://custom.example").unwrap()],
            ),
        );
        assert!(provider.wallet.same_authority(&wallet));
        assert!(!provider.wallet.same_state(&wallet));
        provider.update_wallet(wallet, provider.generation);
        messages(&mut provider);
        release.notify_one();
        let completion = provider.jobs.join_next().await.unwrap().unwrap();
        provider.complete_read(completion);
        let output = messages(&mut provider);
        if remote_error {
            assert_eq!(
                result(&output)["error"],
                json!({"code":-32000,"message":"synthetic-owner-payload","data":{"private":"synthetic-owner-payload"},"extension":[null,1]})
            );
        } else {
            assert_eq!(result(&output)["result"], "0xfeed");
        }
        assert!(provider.reads.is_empty());
        server.await.unwrap();
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn terminal_delivery_retains_document_ownership_when_broker_finishes_first() {
    for drain_first in [false, true] {
        for unregister in [false, true] {
            let (path, mut provider, view) = fixture();
            let (endpoint, started, release, server) = held_rpc(false).await;
            let (_, permission) = authorize(&mut provider, &view, endpoint);
            request(
                &mut provider,
                1,
                "doc",
                "retired",
                "eth_blockNumber",
                Instant::now(),
            );
            started.notified().await;
            let id = *provider.reads.keys().next().unwrap();
            let deadline = provider.reads[&id].ticket.deadline;
            provider.tick(deadline);
            let mut deliveries = if drain_first {
                provider.drain()
            } else {
                Vec::new()
            };
            release.notify_one();
            let completion = provider.jobs.join_next().await.unwrap().unwrap();
            provider.complete_read(completion);
            if !drain_first {
                deliveries = provider.drain();
            }
            // The old broker job has finished, so all 32 slots are available before delivery.
            let charged: Vec<_> = (0..32)
                .map(|_| {
                    let ReadAdmissionDecision::Ready(ticket) = provider
                        .admission
                        .admit(permission.origin.clone(), Instant::now())
                        .unwrap()
                    else {
                        panic!("completed job must release its active slot");
                    };
                    ticket
                })
                .collect();
            if unregister {
                provider.unregister(1, "doc");
            }
            let (_, mut delivery) = deliveries
                .into_iter()
                .find(|(_, delivery)| delivery.ticket_id() == Some(id))
                .unwrap();
            assert_eq!(delivery.read.as_ref().unwrap().0.deadline, deadline);
            assert!(delivery.deadline().is_none());
            let status = provider.delivery(1, &mut delivery);
            if unregister {
                assert!(status == DeliveryStatus::Discard);
            } else {
                assert!(status == DeliveryStatus::Current);
                assert_eq!(
                    serde_json::to_value(&delivery.message).unwrap()["error"]["code"],
                    -32002
                );
            }
            provider.delivered(id);
            assert!(!provider.reads.contains_key(&id));
            for ticket in charged {
                provider.admission.complete(ticket.id, Instant::now());
            }
            server.await.unwrap();
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

#[tokio::test]
async fn tracked_hashes_stay_local_and_external_hashes_forward_without_waiting() {
    use alloy::primitives::B256;
    use std::sync::Mutex;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let requests = requests.clone();
            Arc::new(move |request| {
                requests
                    .lock()
                    .unwrap()
                    .push((request["method"].clone(), request["params"][0].clone()));
                json!({"jsonrpc":"2.0", "id":request["id"], "result":null})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let (path, mut provider, view) = fixture();
    let (_, permission) = authorize(&mut provider, &view, endpoint);
    let tracker = crate::PublicTransactionTracker::default();
    let context =
        crate::public_wallet::test_tracking_context_for(&tracker, &permission.public_account_uuid);
    // Another account's pending transaction on the same tracker must not be revealed.
    let other_context = crate::public_wallet::test_tracking_context_for(&tracker, "other-account");
    provider.wallet.public_transaction_tracker = tracker;
    let family = context.start_family();
    let hash = B256::repeat_byte(7);
    family.register(hash);
    let other_family = other_context.start_family();
    let other_hash = B256::repeat_byte(9);
    other_family.register(other_hash);
    for method in ["eth_getTransactionByHash", "eth_getTransactionReceipt"] {
        let malformed = lookup_response(&mut provider, method, json!([hash, "extra"])).await;
        assert_eq!(malformed["error"]["code"], -32602);
        let pending = lookup_response(&mut provider, method, json!([hash])).await;
        assert!(pending.get("result").is_some_and(Value::is_null));
    }
    drop(family);
    for method in ["eth_getTransactionByHash", "eth_getTransactionReceipt"] {
        let unavailable = lookup_response(&mut provider, method, json!([hash])).await;
        assert_eq!(unavailable["error"]["code"], -32002);
    }
    assert!(requests.lock().unwrap().is_empty());
    let external_hash = B256::repeat_byte(8);
    for forwarded in [external_hash, other_hash] {
        for method in ["eth_getTransactionByHash", "eth_getTransactionReceipt"] {
            let response = lookup_response(&mut provider, method, json!([forwarded])).await;
            assert!(response.get("result").is_some_and(Value::is_null));
        }
    }
    // Local and forwarded pending answers are both null; only the recorded request differs.
    assert_eq!(
        *requests.lock().unwrap(),
        vec![
            (json!("eth_getTransactionByHash"), json!(external_hash)),
            (json!("eth_getTransactionReceipt"), json!(external_hash)),
            (json!("eth_getTransactionByHash"), json!(other_hash)),
            (json!("eth_getTransactionReceipt"), json!(other_hash)),
        ]
    );
    drop(other_family);
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn exhausted_block_discovery_unlocks_authorized_hash_lookups() {
    use alloy::{eips::BlockNumHash, primitives::B256};
    use broadcaster_core::query_rpc_pool::QueryRpcPool;
    use std::sync::Mutex;

    let hash = B256::repeat_byte(7);
    let block = BlockNumHash::new(2, B256::repeat_byte(2));
    let (mut canonical, transaction, receipt) = included_lookup_payloads(hash, block);
    canonical["transactions"] = json!([B256::repeat_byte(8), hash]);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let requests = requests.clone();
            let transaction = transaction.clone();
            let receipt = receipt.clone();
            Arc::new(move |request| {
                let method = request["method"].as_str().unwrap();
                let mut requests = requests.lock().unwrap();
                requests.push((
                    request["method"].clone(),
                    request.get("params").cloned().unwrap_or_else(|| json!([])),
                ));
                let result = match method {
                    "eth_blockNumber" => {
                        if requests.len() == 1 {
                            json!("0x1")
                        } else {
                            json!("0x2")
                        }
                    }
                    "eth_getBlockByNumber" => {
                        assert_eq!(request["params"], json!(["0x2", false]));
                        canonical.clone()
                    }
                    "eth_getBlockReceipts" => {
                        assert_eq!(request["params"], json!([block.hash]));
                        return json!({"jsonrpc":"2.0", "id":request["id"],
                            "error":{"code":-32601,"message":"synthetic unsupported block receipts"}});
                    }
                    "eth_getTransactionByHash" => transaction.clone(),
                    "eth_getTransactionReceipt" => receipt.clone(),
                    _ => panic!("unexpected synthetic RPC method"),
                };
                json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let (path, mut provider, view) = fixture();
    let (_, permission) = authorize(&mut provider, &view, endpoint.clone());
    let http = provider.wallet.http.as_ref().unwrap();
    let route =
        crate::rpc_broker::tests::general(&crate::RpcChainRoute::new(1, vec![endpoint.clone()]));
    let pool = Arc::new(QueryRpcPool::with_http_client(
        vec![endpoint],
        Duration::ZERO,
        http.rpc_client.clone(),
    ));
    let observer =
        crate::block_observer::BlockObserver::establish(pool, 2, http.rpc_broker(), &route)
            .await
            .unwrap();
    let tracker = crate::PublicTransactionTracker::default();
    let context =
        crate::public_wallet::test_tracking_context_for(&tracker, &permission.public_account_uuid);
    provider.wallet.public_transaction_tracker = tracker.clone();
    let mut guard = context.admit_observer(observer).unwrap();
    guard.register(hash, 0);
    let pending = lookup_response(&mut provider, "eth_getTransactionReceipt", json!([hash])).await;
    assert!(pending.get("result").is_some_and(Value::is_null));
    let mut changes = tracker.subscribe();
    guard.handoff().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while tracker.lookup(1, hash) == crate::PublicTransactionLookup::Pending {
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(
        *requests.lock().unwrap(),
        vec![
            (json!("eth_blockNumber"), json!([])),
            (json!("eth_blockNumber"), json!([])),
            (json!("eth_getBlockByNumber"), json!(["0x2", false])),
            (json!("eth_getBlockReceipts"), json!([block.hash])),
        ]
    );
    for (method, expected) in [
        ("eth_getTransactionByHash", transaction),
        ("eth_getTransactionReceipt", receipt),
    ] {
        let response = lookup_response(&mut provider, method, json!([hash])).await;
        assert_eq!(response["result"], expected);
    }
    assert_eq!(
        &requests.lock().unwrap()[4..],
        &[
            (json!("eth_getTransactionByHash"), json!([hash])),
            (json!("eth_getTransactionReceipt"), json!([hash])),
        ]
    );
    tracker.shutdown().await;
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn included_lookups_preserve_raw_json_and_never_fallback_when_block_data_disagrees() {
    use alloy::{eips::BlockNumHash, primitives::B256};
    use std::sync::Mutex;
    let hash = B256::repeat_byte(7);
    let block = BlockNumHash::new(12, B256::repeat_byte(12));
    let payloads = included_lookup_payloads(hash, block);
    let scenario = Arc::new(Mutex::new("valid"));
    let methods = Arc::new(Mutex::new(Vec::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let scenario = scenario.clone();
            let methods = methods.clone();
            let (canonical, transaction, receipt) = payloads.clone();
            Arc::new(move |request| {
                let method = request["method"].as_str().unwrap();
                methods.lock().unwrap().push(method.to_owned());
                let scenario = *scenario.lock().unwrap();
                let mut result = match method {
                    "eth_getBlockByNumber" => {
                        assert_eq!(request["params"], json!(["0xc", false]));
                        canonical.clone()
                    }
                    "eth_getTransactionByBlockHashAndIndex" => {
                        assert_eq!(request["params"], json!([block.hash, "0x1"]));
                        transaction.clone()
                    }
                    "eth_getBlockReceipts" => {
                        assert_eq!(request["params"], json!([block.hash]));
                        let mut other = receipt.clone();
                        other["transactionHash"] = json!(B256::repeat_byte(9));
                        other["transactionIndex"] = json!("0x0");
                        json!([other, receipt])
                    }
                    _ => panic!("tracked hash must never be forwarded"),
                };
                if method == "eth_getBlockByNumber" {
                    match scenario {
                        "reorg" => result["hash"] = json!(B256::ZERO),
                        "missing_block" => result = Value::Null,
                        _ => {}
                    }
                } else {
                    match scenario {
                        "missing_data" => result = Value::Null,
                        "wrong_location" if method == "eth_getBlockReceipts" => {
                            result[1]["blockHash"] = json!(B256::ZERO);
                        }
                        "wrong_location" => result["transactionIndex"] = json!("0x0"),
                        "duplicate" if method == "eth_getBlockReceipts" => {
                            result = json!([receipt, receipt]);
                        }
                        _ => {}
                    }
                }
                json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let (path, mut provider, view) = fixture();
    let (_, permission) = authorize(&mut provider, &view, endpoint);
    let tracker = crate::PublicTransactionTracker::default();
    let context =
        crate::public_wallet::test_tracking_context_for(&tracker, &permission.public_account_uuid);
    provider.wallet.public_transaction_tracker = tracker;
    let family = context.start_family();
    family.register(hash);
    family.included(hash, block, 1);
    for method in ["eth_getTransactionByHash", "eth_getTransactionReceipt"] {
        for current in [
            "valid",
            "reorg",
            "missing_block",
            "missing_data",
            "wrong_location",
            "duplicate",
        ] {
            if current == "duplicate" && method == "eth_getTransactionByHash" {
                continue;
            }
            *scenario.lock().unwrap() = current;
            methods.lock().unwrap().clear();
            let response = lookup_response(&mut provider, method, json!([hash])).await;
            if current == "valid" {
                assert_eq!(
                    response["result"],
                    if method == "eth_getTransactionByHash" {
                        payloads.1.clone()
                    } else {
                        payloads.2.clone()
                    }
                );
            } else {
                assert_eq!(response["error"]["code"], -32002);
            }
            let methods = methods.lock().unwrap();
            assert_eq!(methods.len(), 2);
            assert!(
                methods
                    .iter()
                    .any(|method| method == "eth_getBlockByNumber")
            );
        }
    }
    drop(family);
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn included_lookup_drains_after_owner_invalidation_without_delivering_old_payloads() {
    use alloy::{eips::BlockNumHash, primitives::B256};
    let hash = B256::repeat_byte(7);
    let block = BlockNumHash::new(12, B256::repeat_byte(12));
    let (canonical, transaction, _) = included_lookup_payloads(hash, block);
    let (endpoint, started, release, server) = held_responses(
        false,
        BTreeMap::from([
            ("eth_getBlockByNumber", canonical),
            ("eth_getTransactionByBlockHashAndIndex", transaction),
        ]),
    )
    .await;
    let (path, mut provider, view) = fixture();
    let (_, permission) = authorize(&mut provider, &view, endpoint);
    let tracker = crate::PublicTransactionTracker::default();
    let context =
        crate::public_wallet::test_tracking_context_for(&tracker, &permission.public_account_uuid);
    provider.wallet.public_transaction_tracker = tracker;
    let family = context.start_family();
    family.register(hash);
    family.included(hash, block, 1);
    provider
        .request(
            1,
            "doc".into(),
            "held".into(),
            "eth_getTransactionByHash",
            json!([hash]),
            Instant::now(),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let id = *provider.reads.keys().next().unwrap();
    provider.update_wallet(GatewayWalletState::default(), 3);
    assert_eq!(result(&messages(&mut provider))["error"]["code"], 4100);
    assert!(provider.reads[&id].retired);
    assert!(provider.reads[&id].phase == ReadPhase::Running);
    release.notify_one();
    let completion = tokio::time::timeout(Duration::from_secs(30), provider.jobs.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // Both block-scoped responses completed successfully after the original owner was retired.
    assert_eq!(
        completion.1.as_ref().unwrap()["extension"]["owner"],
        "synthetic-lookup-payload"
    );
    provider.complete_read(completion);
    assert_eq!(messages(&mut provider), [] as [(u64, Value); 0]);
    assert!(!provider.reads.contains_key(&id));
    server.await.unwrap();
    drop(family);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}
