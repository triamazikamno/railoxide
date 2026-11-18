use super::*;

#[tokio::test]
async fn chain_switch_skips_approval_only_when_website_and_live_wallet_match() {
    for (website_chain, published_chain, live_chain, no_op) in [
        (1, 1, 1, true),
        (10, 1, 1, false),
        (1, 1, 10, false),
        (1, 10, 1, true),
    ] {
        let (path, mut provider, view) = fixture();
        let (_, permission) = authorize(
            &mut provider,
            &view,
            url::Url::parse("http://127.0.0.1:1").unwrap(),
        );
        provider
            .store
            .grant_gateway_permission(
                &view,
                &permission.origin,
                &permission.public_account_uuid,
                website_chain,
            )
            .unwrap();
        let mut wallet = provider.wallet.clone();
        wallet.routes.insert(
            10,
            RpcChainRoute::new(10, vec![url::Url::parse("http://127.0.0.1:2").unwrap()]),
        );
        wallet.default_chain_id = Some(published_chain);
        provider.update_wallet(wallet.clone(), 3);
        messages(&mut provider);
        // The desktop publishes its selection before the actor receives its snapshot.
        wallet.default_chain_id = Some(live_chain);
        provider
            .authority_fallback
            .as_ref()
            .unwrap()
            .send_replace(wallet);
        let permissions = provider.store.list_gateway_permissions(&view).unwrap();

        provider
            .request(
                1,
                "doc".into(),
                "switch".into(),
                "wallet_switchEthereumChain",
                json!([{ "chainId": "0x1" }]),
                Instant::now(),
            )
            .unwrap();

        assert_eq!(provider.approval_updates.borrow().is_empty(), no_op);
        let output = messages(&mut provider);
        let response = output
            .iter()
            .find(|(_, message)| message["request_id"] == "switch");
        if no_op {
            assert_eq!(response.unwrap().1.get("result"), Some(&Value::Null));
            assert!(response.unwrap().1.get("error").is_none());
            assert!(
                output
                    .iter()
                    .filter(|(_, message)| message["type"] == "ui_snapshot")
                    .all(|(_, message)| message["pending_requests"].as_array().unwrap().is_empty())
            );

            provider
                .request(
                    1,
                    "doc".into(),
                    "retired".into(),
                    "wallet_switchEthereumChain",
                    json!([{ "chainId": "0x1" }]),
                    Instant::now(),
                )
                .unwrap();
            invalidate_authority(&provider, true);
            let output = messages(&mut provider);
            let response = output
                .iter()
                .find(|(_, message)| message["request_id"] == "retired")
                .unwrap();
            assert_eq!(response.1["error"]["code"], 4100);
        } else {
            assert!(response.is_none());
        }
        assert!(provider.store.list_gateway_permissions(&view).unwrap() == permissions);
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn native_chain_policy_commits_only_current_confirmation_and_retires_old_scope() {
    for decision in ["confirm", "same-chain", "reject", "stale", "unsupported"] {
        let (path, mut provider, view) = fixture();
        let (_, original) = authorize(
            &mut provider,
            &view,
            url::Url::parse("http://127.0.0.1:1").unwrap(),
        );
        let mut wallet = provider.wallet.clone();
        wallet.routes.insert(
            10,
            RpcChainRoute::new(10, vec![url::Url::parse("http://127.0.0.1:2").unwrap()]),
        );
        if decision == "same-chain" {
            wallet.default_chain_id = Some(10);
        }
        provider.update_wallet(wallet, 3);
        messages(&mut provider);
        personal_approval(&mut provider, "old-sign", Instant::now());
        let old = provider.approval_updates.borrow()[0].clone();
        for index in 0..32 {
            provider
                .request(
                    1,
                    "doc".into(),
                    format!("held-{index}"),
                    "eth_accounts",
                    json!([]),
                    Instant::now(),
                )
                .unwrap();
        }
        provider
            .request(
                1,
                "doc".into(),
                "queued-read".into(),
                "eth_blockNumber",
                json!([]),
                Instant::now(),
            )
            .unwrap();
        assert!(
            provider
                .reads
                .values()
                .any(|read| read.owner.request_id == "queued-read"
                    && read.phase == ReadPhase::Queued)
        );
        let chain = match decision {
            "unsupported" => "0x89",
            "same-chain" => "0x1",
            _ => "0xa",
        };
        provider
            .request(
                1,
                "doc".into(),
                "switch".into(),
                "wallet_switchEthereumChain",
                json!([{ "chainId": chain }]),
                Instant::now(),
            )
            .unwrap();
        if decision == "unsupported" {
            let output = messages(&mut provider);
            assert_eq!(
                output
                    .iter()
                    .find(|(_, message)| message["request_id"] == "switch")
                    .unwrap()
                    .1["error"]["code"],
                4901
            );
        } else {
            let ready = provider
                .approval_updates
                .borrow()
                .iter()
                .find(|request| request.id != old.id)
                .unwrap()
                .clone();
            provider.begin_approval(&ready.id).unwrap();
            if decision == "stale" {
                provider
                    .authority_fallback
                    .as_ref()
                    .unwrap()
                    .send_replace(GatewayWalletState::default());
                assert!(
                    provider
                        .complete_approval(&ready.id, Ok(Value::Null))
                        .is_err()
                );
            } else {
                let outcome = if decision == "reject" {
                    Err(crate::gateway::GatewayApprovalFailure::Local(
                        LocalProviderFailure::UserRejected,
                    ))
                } else {
                    Ok(Value::Null)
                };
                provider.complete_approval(&ready.id, outcome).unwrap();
                if matches!(decision, "confirm" | "same-chain") {
                    // The desktop selects the approved chain before the browser
                    // necessarily receives the queued response.
                    let mut wallet = provider.wallet.clone();
                    wallet.default_chain_id = Some(if decision == "same-chain" { 1 } else { 10 });
                    provider.update_wallet(wallet, 3);
                }
                let output = messages(&mut provider);
                let response = output
                    .iter()
                    .position(|(_, message)| message["request_id"] == "switch")
                    .unwrap();
                if decision == "confirm" {
                    let changed = output
                        .iter()
                        .position(|(_, message)| {
                            message["type"] == "provider_state" && message["chain_id"] == "0xa"
                        })
                        .unwrap();
                    assert!(changed < response);
                    assert!(output[response].1["result"].is_null());
                    assert!(output[response].1.get("error").is_none());
                    assert!(old.control.ensure_current().is_err());
                    assert!(
                        !provider
                            .reads
                            .values()
                            .any(|read| read.owner.request_id == "queued-read")
                    );
                    assert!(provider.jobs.is_empty());
                } else if decision == "same-chain" {
                    assert!(output[response].1["result"].is_null());
                    assert!(output[response].1.get("error").is_none());
                    assert!(old.control.ensure_current().is_ok());
                } else {
                    assert_eq!(output[response].1["error"]["code"], 4001);
                }
            }
        }
        let stored = provider
            .store
            .list_gateway_permissions(&view)
            .unwrap()
            .remove(0);
        assert_eq!(stored.chain_id, if decision == "confirm" { 10 } else { 1 });
        assert_eq!(stored.public_account_uuid, original.public_account_uuid);
        assert_eq!(stored.public_account_scope, original.public_account_scope);
        assert_eq!(
            stored.owning_private_wallet_uuid,
            original.owning_private_wallet_uuid
        );
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn add_chain_acknowledges_saved_configuration_without_mutating_authority() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let before = provider.wallet.clone();
    let permissions = provider.permissions.clone();
    provider.request(1, "doc".into(), "add".into(), "wallet_addEthereumChain", json!([{ "chainId": "0x1", "rpcUrls": ["https://ignored.invalid"], "custom": { "retained": true } }]), Instant::now()).unwrap();
    assert!(
        messages(&mut provider)
            .iter()
            .all(|(_, message)| message["type"] != "provider_response")
    );
    let ready = provider.approval_updates.borrow()[0].clone();
    provider.begin_approval(&ready.id).unwrap();
    provider
        .complete_approval(&ready.id, Ok(Value::Null))
        .unwrap();
    let output = messages(&mut provider);
    assert!(result(&output)["result"].is_null());
    assert!(result(&output).get("error").is_none());
    assert!(before.same_state(&provider.wallet));
    assert!(provider.permissions == permissions);
    provider
        .request(
            1,
            "doc".into(),
            "disabled".into(),
            "wallet_addEthereumChain",
            json!([{ "chainId": "0xa" }]),
            Instant::now(),
        )
        .unwrap();
    assert_eq!(result(&messages(&mut provider))["error"]["code"], 4901);
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn watch_asset_recognition_keeps_shared_slot_and_never_responds_twice() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let params = json!({ "type": "ERC20", "options": { "address": "0x0000000000000000000000000000000000000001", "symbol": "UNTRUSTED", "image": "https://ignored.invalid" } });
    for (id, params, code) in [
        ("type", json!({ "type": "ERC721", "options": {} }), 4200),
        (
            "chain",
            json!({ "type": "ERC20", "chainId": "0xa", "options": { "address": "0x0000000000000000000000000000000000000001" } }),
            4901,
        ),
        (
            "params",
            json!({ "type": "ERC20", "options": { "address": "invalid" } }),
            -32602,
        ),
    ] {
        provider
            .request(
                1,
                "doc".into(),
                id.into(),
                "wallet_watchAsset",
                params,
                Instant::now(),
            )
            .unwrap();
        assert_eq!(result(&messages(&mut provider))["error"]["code"], code);
    }
    for index in 0..16 {
        provider
            .request(
                1,
                "doc".into(),
                format!("watch-{index}"),
                "wallet_watchAsset",
                if index % 2 == 0 {
                    params.clone()
                } else {
                    json!([params.clone()])
                },
                Instant::now(),
            )
            .unwrap();
        assert_eq!(result(&messages(&mut provider))["result"], true);
    }
    assert_eq!(provider.approvals.len(), 16);
    personal_approval(&mut provider, "overflow", Instant::now());
    assert_eq!(result(&messages(&mut provider))["error"]["code"], -32005);
    let ready = provider.approval_updates.borrow()[0].clone();
    provider
        .complete_approval(
            &ready.id,
            Err(crate::gateway::GatewayApprovalFailure::Local(
                LocalProviderFailure::UserRejected,
            )),
        )
        .unwrap();
    assert_eq!(provider.approvals.len(), 15);
    assert!(
        messages(&mut provider)
            .iter()
            .all(|(_, message)| message["type"] != "provider_response")
    );
    provider.tick(Instant::now() + APPROVAL_WINDOW);
    assert!(provider.approvals.is_empty());
    assert!(
        messages(&mut provider)
            .iter()
            .all(|(_, message)| message["type"] != "provider_response")
    );
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn unsealed_remote_outcomes_revalidate_live_authority_before_actor_update() {
    for remote_error in [false, true] {
        for locked in [false, true] {
            let (path, mut provider, view) = fixture();
            let (endpoint, started, release, server) = held_rpc(remote_error).await;
            authorize(&mut provider, &view, endpoint);
            assert_eq!(
                request(
                    &mut provider,
                    1,
                    "doc",
                    "held",
                    "eth_blockNumber",
                    Instant::now()
                ),
                [] as [(u64, Value); 0]
            );
            started.notified().await;
            release.notify_one();
            let completion = provider.jobs.join_next().await.unwrap().unwrap();
            provider.complete_read(completion);
            let (_, mut delivery) = provider.drain().pop().unwrap();
            invalidate_authority(&provider, locked);
            assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Changed);
            let value = serde_json::to_value(&delivery.message).unwrap();
            assert_eq!(value["error"]["code"], if locked { 4100 } else { -32002 });
            assert!(!value.to_string().contains("synthetic-owner-payload"));
            // The replacement is a fixed terminal error and remains deliverable.
            assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
            provider.delivered(delivery.ticket_id().unwrap());
            assert!(provider.reads.is_empty());
            let rejected = request(
                &mut provider,
                1,
                "doc",
                "after-invalidation",
                "eth_blockNumber",
                Instant::now(),
            );
            assert_eq!(
                result(&rejected)["error"]["code"],
                if locked { 4100 } else { -32002 }
            );
            assert!(
                provider.jobs.is_empty(),
                "live invalidation must reject before broker dispatch"
            );
            server.await.unwrap();
            drop(provider);
            drop(view);
            std::fs::remove_dir_all(path).unwrap();
        }
    }
}

#[tokio::test]
async fn unconnected_chain_query_revalidates_selection_and_lock_before_delivery() {
    for change in ["selection", "disabled", "locked"] {
        let (path, mut provider, view) = fixture();
        provider
            .register(
                1,
                PeerId::from_bytes([8; 16]),
                "doc".into(),
                "https://basescan.org",
            )
            .unwrap();
        messages(&mut provider);
        provider
            .request(
                1,
                "doc".into(),
                "chain".into(),
                "eth_chainId",
                json!([]),
                Instant::now(),
            )
            .unwrap();
        assert!(provider.jobs.is_empty());
        let (_, mut delivery) = provider
            .drain()
            .into_iter()
            .find(|(_, delivery)| delivery.ticket_id().is_some())
            .unwrap();
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
        assert_eq!(
            serde_json::to_value(&delivery.message).unwrap()["result"],
            "0x1"
        );
        let mut wallet = provider.wallet.clone();
        match change {
            "selection" => wallet.default_chain_id = Some(10),
            "disabled" => wallet.chain_ids.retain(|id| *id != 1),
            "locked" => wallet.view = None,
            _ => unreachable!(),
        }
        provider
            .authority_fallback
            .as_ref()
            .unwrap()
            .send_replace(wallet.clone());
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Changed);
        assert_eq!(
            serde_json::to_value(&delivery.message).unwrap()["error"]["code"],
            if change == "locked" { 4100 } else { -32002 }
        );
        provider.delivered(delivery.ticket_id().unwrap());
        provider.update_wallet(wallet, provider.generation + 1);
        messages(&mut provider);
        let latest = request(
            &mut provider,
            1,
            "doc",
            "latest-chain",
            "eth_chainId",
            Instant::now(),
        );
        if change == "selection" {
            assert_eq!(result(&latest)["result"], "0xa");
        } else {
            assert_eq!(
                result(&latest)["error"]["code"],
                if change == "locked" { 4100 } else { 4901 }
            );
        }
        assert!(provider.permissions.is_empty());
        assert!(provider.jobs.is_empty());
        drop(delivery);
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn chain_addition_without_account_access_requires_current_confirmation() {
    for outcome in [
        "added",
        "added_before_publication",
        "reject",
        "lock",
        "disconnect",
        "expire",
    ] {
        let (path, mut provider, view) = fixture();
        let mut wallet = provider.wallet.clone();
        wallet.http = Some(HttpContext::direct_for_tests());
        provider.update_wallet(wallet, 2);
        provider
            .register(
                1,
                PeerId::from_bytes([8; 16]),
                "doc".into(),
                "https://basescan.org",
            )
            .unwrap();
        messages(&mut provider);
        let chain = request(
            &mut provider,
            1,
            "doc",
            "initial-chain",
            "eth_chainId",
            Instant::now(),
        );
        assert_eq!(result(&chain)["result"], "0x1", "{chain:?}");
        provider.request(1, "doc".into(), "add".into(), "wallet_addEthereumChain", json!([{
            "chainId": "0x7a69", "chainName": "Anvil",
            "nativeCurrency": {"name": "ETH", "symbol": "ETH", "decimals": 18},
            "rpcUrls": ["https://mainnet.base.org"], "blockExplorerUrls": ["https://basescan.org"]
        }]), Instant::now()).unwrap();
        let output = messages(&mut provider);
        assert_eq!(
            provider.approval_updates.borrow().len(),
            1,
            "{outcome}: {output:?}"
        );
        let ready = provider.approval_updates.borrow()[0].clone();
        assert!(ready.authorization.is_none());
        let (reply, read_result) = oneshot::channel();
        provider.approval_read(
            &ready.id,
            url::Url::parse("https://mainnet.base.org").unwrap().into(),
            RpcRead::from_method_params("eth_blockNumber", json!([]), 31337).unwrap(),
            Instant::now(),
            operation_client(),
            reply,
        );
        assert!(matches!(
            read_result.await.unwrap(),
            Err(RpcBrokerError::OriginRejected)
        ));
        assert!(provider.pending.is_empty());
        assert!(provider.jobs.is_empty());
        assert!(provider.permissions.is_empty());
        assert!(
            provider
                .store
                .list_gateway_permissions(&view)
                .unwrap()
                .is_empty()
        );
        match outcome {
            "added" | "added_before_publication" => {
                provider.begin_approval(&ready.id).unwrap();
                let mut wallet = provider.wallet.clone();
                wallet.chain_ids.push(31337);
                wallet.configured_chain_ids.push(31337);
                wallet.routes.insert(
                    31337,
                    RpcChainRoute::new(
                        31337,
                        vec![url::Url::parse("https://mainnet.base.org").unwrap()],
                    ),
                );
                if outcome == "added_before_publication" {
                    provider
                        .authority_fallback
                        .as_ref()
                        .unwrap()
                        .send_replace(wallet);
                } else {
                    provider.update_wallet(wallet, provider.generation);
                }
                provider
                    .complete_approval(&ready.id, Ok(Value::Null))
                    .unwrap();
                let output = messages(&mut provider);
                assert_eq!(result(&output)["result"], Value::Null);
                assert!(result(&output).get("error").is_none());
                assert_eq!(provider.wallet.default_chain_id, Some(1));
            }
            "reject" => {
                provider
                    .complete_approval(
                        &ready.id,
                        Err(crate::gateway::GatewayApprovalFailure::Local(
                            LocalProviderFailure::UserRejected,
                        )),
                    )
                    .unwrap();
                assert_eq!(result(&messages(&mut provider))["error"]["code"], 4001);
            }
            "lock" => invalidate_authority(&provider, true),
            "disconnect" => provider.unregister(1, "doc"),
            "expire" => provider.tick(ready.deadline + Duration::from_secs(1)),
            _ => unreachable!(),
        }
        assert!(provider.begin_approval(&ready.id).is_err());
        assert!(provider.permissions.is_empty());
        assert!(
            provider
                .store
                .list_gateway_permissions(&view)
                .unwrap()
                .is_empty()
        );
        if matches!(outcome, "added" | "reject") {
            assert_eq!(
                result(&request(
                    &mut provider,
                    1,
                    "doc",
                    "accounts",
                    "eth_accounts",
                    Instant::now()
                ))["result"],
                json!([])
            );
            for (method, params) in [
                ("wallet_switchEthereumChain", json!([{"chainId":"0x7a69"}])),
                (
                    "personal_sign",
                    json!(["0x6869", "0x0000000000000000000000000000000000000001"]),
                ),
                ("eth_blockNumber", json!([])),
            ] {
                provider
                    .request(
                        1,
                        "doc".into(),
                        method.into(),
                        method,
                        params,
                        Instant::now(),
                    )
                    .unwrap();
                assert_eq!(result(&messages(&mut provider))["error"]["code"], 4100);
            }
        }
        drop(ready);
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn proposed_chain_requires_current_approval_and_runtime_before_add_then_switch() {
    const ID: u64 = 9_007_199_254_740_993;
    for outcome in ["reject", "expire", "retire", "missing-runtime", "added"] {
        let (path, mut provider, view) = fixture();
        authorize(
            &mut provider,
            &view,
            url::Url::parse("http://127.0.0.1:1").unwrap(),
        );
        let before = provider.wallet.clone();
        let permissions = provider.permissions.clone();
        let raw = json!([{
            "chainId": "0x20000000000001", "chainName": "Custom",
            "nativeCurrency": {"name":"Custom coin", "symbol":"CSTM", "decimals":6, "futureCurrencyField": true},
            "rpcUrls": ["https://synthetic:credential@custom.invalid/rpc"],
            "iconUrls": ["https://never-fetched.invalid/icon.svg"], "futureExtension": {"keep":true}
        }]);
        let mut malformed = raw.clone();
        malformed[0]["rpcUrls"] = json!(["http://plain.invalid"]);
        provider
            .request(
                1,
                "doc".into(),
                "malformed".into(),
                "wallet_addEthereumChain",
                malformed,
                Instant::now(),
            )
            .unwrap();
        assert_eq!(result(&messages(&mut provider))["error"]["code"], -32602);
        assert!(provider.approvals.is_empty());
        provider
            .request(
                1,
                "doc".into(),
                "add".into(),
                "wallet_addEthereumChain",
                raw.clone(),
                Instant::now(),
            )
            .unwrap();
        let output = messages(&mut provider);
        assert!(
            !serde_json::to_string(&output)
                .unwrap()
                .contains("credential")
        );
        let ready = provider.approval_updates.borrow()[0].clone();
        let crate::WalletConnectParsedRequest::WalletAddEthereumChain {
            chain_id,
            raw: retained,
            definition: Some(definition),
        } = &ready.parsed
        else {
            panic!("complete new definition expected")
        };
        assert_eq!((*chain_id, retained), (ID, &raw));
        assert_eq!(definition.native_currency.decimals, 6);
        assert!(provider.jobs.is_empty());
        assert!(before.same_state(&provider.wallet));
        if outcome == "reject" {
            provider
                .complete_approval(
                    &ready.id,
                    Err(crate::gateway::GatewayApprovalFailure::Local(
                        LocalProviderFailure::UserRejected,
                    )),
                )
                .unwrap();
            assert_eq!(result(&messages(&mut provider))["error"]["code"], 4001);
        } else if outcome == "expire" {
            provider.tick(ready.deadline + Duration::from_secs(1));
            assert!(provider.begin_approval(&ready.id).is_err());
        } else if outcome == "retire" {
            invalidate_authority(&provider, true);
            assert!(provider.begin_approval(&ready.id).is_err());
        } else {
            provider.begin_approval(&ready.id).unwrap();
            if outcome == "added" {
                let mut wallet = provider.wallet.clone();
                wallet.chain_ids.push(ID);
                wallet.configured_chain_ids.push(ID);
                wallet.routes.insert(
                    ID,
                    RpcChainRoute::new(
                        ID,
                        vec![url::Url::parse(&definition.rpc_endpoints[0].url).unwrap()],
                    )
                    .with_identity_verification(),
                );
                wallet
                    .native_currencies
                    .insert(ID, definition.native_currency.clone());
                provider.update_wallet(wallet, provider.generation);
            }
            let completed = provider.complete_approval(&ready.id, Ok(Value::Null));
            assert_eq!(completed.is_ok(), outcome == "added");
            let output = messages(&mut provider);
            if outcome == "missing-runtime" {
                assert_eq!(result(&output)["error"]["code"], 4901);
            } else {
                assert_eq!(result(&output)["result"], Value::Null);
                assert!(result(&output).get("error").is_none());
                assert!(provider.permissions == permissions);
                assert_eq!(provider.wallet.default_chain_id, before.default_chain_id);
                provider
                    .request(
                        1,
                        "doc".into(),
                        "switch".into(),
                        "wallet_switchEthereumChain",
                        json!([{"chainId":"0x20000000000001"}]),
                        Instant::now(),
                    )
                    .unwrap();
                let switch = provider.approval_updates.borrow()[0].clone();
                provider.begin_approval(&switch.id).unwrap();
                provider
                    .complete_approval(&switch.id, Ok(Value::Null))
                    .unwrap();
                let output = messages(&mut provider);
                assert!(result(&output).get("error").is_none());
                assert_eq!(provider.permissions[0].chain_id, ID);
                assert_eq!(
                    result(&request(
                        &mut provider,
                        1,
                        "doc",
                        "chain",
                        "eth_chainId",
                        Instant::now()
                    ))["result"],
                    "0x20000000000001"
                );
            }
        }
        if outcome != "added" {
            assert!(before.same_state(&provider.wallet));
            assert!(provider.permissions == permissions);
        }
        drop(ready);
        drop(provider);
        drop(view);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
async fn changed_chain_route_retires_held_read_without_rebinding_its_payload() {
    let (path, mut provider, view) = fixture();
    let (endpoint, started, release, server) = held_rpc(false).await;
    authorize(&mut provider, &view, endpoint);
    request(
        &mut provider,
        1,
        "doc",
        "old-route",
        "eth_blockNumber",
        Instant::now(),
    );
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    let permissions = provider.permissions.clone();
    let mut next = provider.wallet.clone();
    next.routes.insert(
        1,
        RpcChainRoute::new(
            1,
            vec![url::Url::parse("https://replacement.invalid").unwrap()],
        ),
    );
    provider.update_wallet(next, provider.generation);
    let output = messages(&mut provider);
    assert!(result(&output)["error"].is_object());
    assert!(provider.permissions == permissions);
    release.notify_one();
    if let Some(Ok(completion)) = provider.jobs.join_next().await {
        provider.complete_read(completion);
    }
    assert!(
        messages(&mut provider)
            .iter()
            .all(|(_, message)| message["request_id"] != "old-route")
    );
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}
