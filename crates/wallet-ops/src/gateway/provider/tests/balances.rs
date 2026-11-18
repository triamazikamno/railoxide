use super::*;

fn seed_balance(
    provider: &mut DappProvider,
    asset: crate::PublicBalanceAsset,
    observed_at: Instant,
) -> (crate::PublicBalanceScope, PublicAccountMetadata) {
    let permission = &provider.permissions[0];
    let (_, account) = provider.resolve(&permission.origin).unwrap();
    provider.wallet.public_accounts = vec![account.clone()];
    // Publish the fixture's tracked accounts before admitting reads against them.
    provider
        .authority_fallback
        .as_ref()
        .unwrap()
        .send_replace(provider.wallet.clone());
    let scope = crate::PublicBalanceScope::new(
        provider
            .wallet
            .view
            .as_ref()
            .unwrap()
            .wallet_id()
            .to_owned(),
        provider.wallet.active_wallet_generation,
        provider.wallet.http.as_ref().unwrap().rpc_broker(),
        provider.wallet.routes[&permission.chain_id].clone(),
    );
    let cache = &provider.wallet.public_balance_cache;
    cache.set_scope(scope.clone());
    let ticket = cache
        .begin_refresh(&scope, [account.public_account_uuid.as_str()])
        .unwrap();
    assert!(
        cache
            .finish_refresh(
                ticket,
                Some(crate::PublicBalanceSnapshot {
                    chain_id: permission.chain_id,
                    refreshed_at: std::time::SystemTime::now(),
                    accounts: vec![crate::PublicAccountBalance {
                        account: account.clone(),
                        balances: vec![crate::PublicBalanceEntry {
                            asset,
                            amount: crate::PublicBalanceAmount::Available(
                                alloy::primitives::U256::from(7)
                            ),
                        }],
                        observed_at: Some(observed_at.into_std()),
                        observed_block: Some(10),
                    }],
                })
            )
            .accepted
    );
    (scope, account)
}

async fn balance_rpc() -> (
    url::Url,
    Arc<std::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    use alloy::sol_types::SolValue;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let requests = requests.clone();
            Arc::new(move |request| {
                requests.lock().unwrap().push(request.clone());
                let result = match request["method"].as_str().unwrap() {
                    "eth_getBalance" => json!("0x63"),
                    "eth_call" => json!(alloy::primitives::Bytes::from(
                        alloy::primitives::U256::from(99).abi_encode()
                    )),
                    _ => panic!("unexpected balance RPC"),
                };
                json!({"jsonrpc":"2.0", "id":request["id"], "result":result})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    (endpoint, requests, server)
}

#[tokio::test]
async fn native_local_balance_requires_current_authorized_observation() {
    let (endpoint, requests, server) = balance_rpc().await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let native = crate::public_wallet::native_asset_for_chain(
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .unwrap(),
    );
    let (_, account) = seed_balance(&mut provider, native.clone(), Instant::now());
    let params = json!([account.address, "latest"]);
    assert!(provider.wallet.token_registry.is_none());
    assert_eq!(
        lookup_response(&mut provider, "eth_getBalance", params.clone()).await["result"],
        "0x7"
    );
    assert!(requests.lock().unwrap().is_empty());
    provider
        .register(
            2,
            PeerId::from_bytes([8; 16]),
            "unapproved".into(),
            "https://unapproved.invalid/",
        )
        .unwrap();
    messages(&mut provider);
    provider
        .request(
            2,
            "unapproved".into(),
            "denied".into(),
            "eth_getBalance",
            params.clone(),
            Instant::now(),
        )
        .unwrap();
    assert_eq!(result(&messages(&mut provider))["error"]["code"], 4100);
    assert!(requests.lock().unwrap().is_empty());
    // Native ingress still requires its selector, even when a fresh local value exists.
    assert_eq!(
        lookup_response(&mut provider, "eth_getBalance", json!([account.address])).await["error"]["code"],
        -32602
    );
    for case in [
        "stale",
        "pending",
        "scope",
        "historical",
        "other-account",
        "untracked",
    ] {
        provider.wallet.public_balance_cache.clear();
        let now = Instant::now();
        let observed = if case == "stale" {
            now - Duration::from_secs(31)
        } else {
            now
        };
        let (scope, account) = seed_balance(&mut provider, native.clone(), observed);
        let mut params = params.clone();
        match case {
            "pending" => provider
                .wallet
                .public_balance_cache
                .invalidate_account(&scope, &account, true, None),
            "scope" => {
                provider
                    .wallet
                    .public_balance_cache
                    .set_scope(crate::PublicBalanceScope::new(
                        view.wallet_id().to_owned(),
                        provider.wallet.active_wallet_generation + 1,
                        provider.wallet.http.as_ref().unwrap().rpc_broker(),
                        provider.wallet.routes[&1].clone(),
                    ));
            }
            "historical" => params[1] = json!("pending"),
            "other-account" => params[0] = json!(alloy::primitives::Address::ZERO),
            "untracked" => {
                provider.wallet.public_accounts.clear();
                provider
                    .authority_fallback
                    .as_ref()
                    .unwrap()
                    .send_replace(provider.wallet.clone());
            }
            _ => {}
        }
        let before = requests.lock().unwrap().len();
        assert_eq!(
            lookup_response(&mut provider, "eth_getBalance", params.clone()).await["result"],
            "0x63",
            "{case}"
        );
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), before + 1, "{case}");
        assert_eq!(recorded.last().unwrap()["params"], params, "{case}");
    }
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn local_balance_delivery_rechecks_generation_and_namespace() {
    let (path, mut provider, view) = fixture();
    authorize(
        &mut provider,
        &view,
        url::Url::parse("http://127.0.0.1:1").unwrap(),
    );
    let native = crate::public_wallet::native_asset_for_chain(
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .unwrap(),
    );
    for case in ["pending", "refreshed-generation", "replaced-namespace"] {
        provider.wallet.public_balance_cache.clear();
        let (scope, account) = seed_balance(&mut provider, native.clone(), Instant::now());
        provider
            .request(
                1,
                "doc".into(),
                "local".into(),
                "eth_getBalance",
                json!([account.address, "latest"]),
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
        if case == "replaced-namespace" {
            provider.wallet.public_balance_cache.clear();
        } else {
            provider.wallet.public_balance_cache.invalidate_account(
                &scope,
                &account,
                case == "pending",
                None,
            );
        }
        if case != "pending" {
            seed_balance(&mut provider, native.clone(), Instant::now());
        }
        assert!(
            provider.delivery(1, &mut delivery) == DeliveryStatus::Changed,
            "{case}"
        );
        assert!(delivery.local_error);
        assert_eq!(
            serde_json::to_value(&delivery.message).unwrap()["error"]["code"],
            -32002
        );
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
        provider.delivered(delivery.ticket_id().unwrap());
    }
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn queued_balance_loses_eligibility_before_dispatch_and_forwards() {
    let (endpoint, requests, server) = balance_rpc().await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let (scope, account) = seed_balance(
        &mut provider,
        crate::public_wallet::native_asset_for_chain(
            crate::settings::build_effective_chain_configs(
                &crate::settings::WalletSettings::default(),
            )
            .unwrap()
            .get(1)
            .unwrap(),
        ),
        Instant::now(),
    );
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
            "balance".into(),
            "eth_getBalance",
            json!([account.address, "latest"]),
            Instant::now(),
        )
        .unwrap();
    assert!(
        provider
            .reads
            .values()
            .any(|read| read.owner.request_id == "balance" && read.phase == ReadPhase::Queued)
    );
    provider
        .wallet
        .public_balance_cache
        .invalidate_account(&scope, &account, true, None);
    messages(&mut provider);
    let completion = tokio::time::timeout(Duration::from_secs(30), provider.jobs.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    provider.complete_read(completion);
    assert_eq!(result(&messages(&mut provider))["result"], "0x63");
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn token_local_balance_preserves_ingress_and_forwards_ineligible_calls() {
    use alloy::primitives::{Address, Bytes, U256};
    use alloy::sol_types::SolValue;
    let (endpoint, requests, server) = balance_rpc().await;
    let (path, mut provider, view) = fixture();
    authorize(&mut provider, &view, endpoint);
    let token = Address::repeat_byte(0x42);
    let asset = crate::PublicBalanceAsset {
        id: crate::PublicAssetId::Erc20(token),
        symbol: "TEST".into(),
        decimals: 6,
    };
    let (_, account) = seed_balance(&mut provider, asset, Instant::now());
    provider.wallet.token_registry = Some(Arc::new(crate::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, token.to_string().to_ascii_lowercase()),
            crate::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: token.to_string(),
                symbol: "TEST".into(),
                decimals: 6,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    }));
    // Standard selector with an Alloy-encoded address word.
    let calldata = Bytes::from(
        [
            alloy::primitives::hex!("70a08231").as_slice(),
            account.address.abi_encode().as_slice(),
        ]
        .concat(),
    );
    let call = json!({"to":token, "data":calldata});
    for params in [
        json!([call]),
        json!([call, "latest"]),
        json!([{"to":token, "input":calldata, "data":calldata}, "latest"]),
    ] {
        assert_eq!(
            lookup_response(&mut provider, "eth_call", params).await["result"],
            json!(Bytes::from(U256::from(7).abi_encode()))
        );
    }
    assert!(requests.lock().unwrap().is_empty());
    for (params, code) in [
        (
            json!([{"to":token, "input":calldata, "data":"0x01"}]),
            -32602,
        ),
        (
            json!([{"to":token, "data":calldata, "chainId":"0x2"}]),
            -32000,
        ),
        (
            json!([{"to":token, "data":calldata, "gas":"invalid"}]),
            -32602,
        ),
        (json!([call, "latest", null]), -32602),
    ] {
        assert_eq!(
            lookup_response(&mut provider, "eth_call", params).await["error"]["code"],
            code
        );
        assert!(provider.jobs.is_empty());
        assert!(requests.lock().unwrap().is_empty());
    }
    let mut cases = Vec::new();
    for (field, value) in [
        ("from", json!(Address::ZERO)),
        ("value", json!("0x0")),
        ("gas", Value::Null),
        ("accessList", json!([])),
        ("context", Value::Null),
    ] {
        let mut context = call.clone();
        context[field] = value;
        cases.push(json!([context, "latest"]));
    }
    let trailing = Bytes::from([calldata.as_ref(), &[0]].concat());
    let mut noncanonical = calldata.to_vec();
    noncanonical[4] = 1;
    let allowance = Bytes::from(
        [
            alloy::primitives::hex!("dd62ed3e").as_slice(),
            account.address.abi_encode().as_slice(),
            token.abi_encode().as_slice(),
        ]
        .concat(),
    );
    cases.extend([
        json!([call, "latest", {}]),
        json!([call, "pending"]),
        json!([call, {"blockNumber":"0xa"}]),
        json!([{"to":Address::repeat_byte(0x43), "data":calldata}, "latest"]),
        json!([{"to":token, "data":trailing}, "latest"]),
        json!([{"to":token, "data":Bytes::from(noncanonical)}, "latest"]),
        json!([{"to":token, "data":allowance}, "latest"]),
        json!([{"to":null, "data":calldata}, "latest"]),
    ]);
    for params in cases {
        let expected = RpcRead::from_method_params("eth_call", params.clone(), 1).unwrap();
        let before = requests.lock().unwrap().len();
        assert_eq!(
            lookup_response(&mut provider, "eth_call", params).await["result"],
            json!(Bytes::from(U256::from(99).abi_encode()))
        );
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), before + 1);
        let forwarded = recorded.last().unwrap();
        assert_eq!(forwarded["method"], "eth_call");
        assert_eq!(
            RpcRead::from_method_params("eth_call", forwarded["params"].clone(), 1).unwrap(),
            expected
        );
    }
    // Registry publication must invalidate a prepared token answer without revoking unrelated reads.
    for remove in [false, true] {
        let mut wallet = provider.wallet.clone();
        Arc::make_mut(wallet.token_registry.as_mut().unwrap())
            .tokens
            .values_mut()
            .next()
            .unwrap()
            .decimals = 6;
        provider.update_wallet(wallet, provider.generation);
        messages(&mut provider);
        provider
            .request(
                1,
                "doc".into(),
                "registry".into(),
                "eth_call",
                json!([call]),
                Instant::now(),
            )
            .unwrap();
        let (_, mut delivery) = provider
            .drain()
            .into_iter()
            .find(|(_, delivery)| delivery.ticket_id().is_some())
            .unwrap();
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Current);
        let mut changed = provider.wallet.clone();
        if remove {
            Arc::make_mut(changed.token_registry.as_mut().unwrap())
                .tokens
                .clear();
        } else {
            Arc::make_mut(changed.token_registry.as_mut().unwrap())
                .tokens
                .values_mut()
                .next()
                .unwrap()
                .decimals = 18;
        }
        assert!(provider.wallet.same_authority(&changed));
        assert!(!provider.wallet.same_state(&changed));
        provider.update_wallet(changed, provider.generation);
        assert!(provider.delivery(1, &mut delivery) == DeliveryStatus::Changed);
        assert!(delivery.local_error);
        assert_eq!(
            serde_json::to_value(&delivery.message).unwrap()["error"]["code"],
            -32002
        );
        provider.delivered(delivery.ticket_id().unwrap());
        messages(&mut provider);
        assert_eq!(
            lookup_response(&mut provider, "eth_call", json!([call])).await["result"],
            json!(Bytes::from(U256::from(99).abi_encode()))
        );
    }
    server.abort();
    drop(provider);
    drop(view);
    std::fs::remove_dir_all(path).unwrap();
}
