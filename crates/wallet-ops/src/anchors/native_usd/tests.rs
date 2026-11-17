use super::*;
use crate::settings::{NativeUsdPricing, WalletSettings, build_effective_chain_configs};
use alloy::primitives::{Address, I256, uint};
use std::sync::Arc;

#[tokio::test(start_paused = true)]
async fn failed_sources_back_off_independently_and_recovery_resets_retries() {
    use futures_util::FutureExt;
    use std::cell::RefCell;

    let settings = WalletSettings::default();
    let configs = build_effective_chain_configs(&settings).unwrap();
    let configs: EffectiveChainRegistry = configs
        .iter()
        .filter(|(id, _)| matches!(**id, 1 | 56))
        .map(|(_, chain)| chain.clone())
        .collect();
    let cache = TokenAnchorRateCache::new();
    cache.reconcile_native_sources(&configs, &[]);
    cache.set_active_native_chains(&[1, 56]);
    let attempts = RefCell::new(BTreeMap::<u64, usize>::new());
    let (_configuration_tx, configuration_rx) = watch::channel(0);
    let (_wake_tx, wake_rx) = watch::channel(0);
    let worker = run_worker_with(&cache, configuration_rx, wake_rx, |chain| {
        let mut attempts = attempts.borrow_mut();
        let attempt = attempts.entry(chain.chain_id).or_default();
        *attempt += 1;
        std::future::ready(if chain.chain_id == 1 && (*attempt <= 5 || *attempt == 7) {
            Err("Oracle unavailable".to_owned())
        } else {
            Ok((U256::from(3_000_000_000_u64), 8))
        })
    });
    tokio::pin!(worker);
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(attempts.borrow()[&1], 1);
    assert_eq!(attempts.borrow()[&56], 1);
    assert_eq!(cache.cached_native_usd_rate(1), None);

    // Each deadline belongs only to the failing source, and sustained failure caps at 30s.
    for (delay, expected_attempts) in [(5, 2), (10, 3), (20, 4), (30, 5), (30, 6)] {
        tokio::time::advance(Duration::from_secs(delay - 1)).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(attempts.borrow()[&1], expected_attempts - 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(attempts.borrow()[&1], expected_attempts);
        assert_eq!(attempts.borrow()[&56], 1);
    }
    assert_eq!(
        cache.native_usd_statuses()["1"].state,
        NativeUsdState::Available
    );
    let recovered = cache.cached_native_usd_rate(1).unwrap();

    // Recovery restores the normal refresh cadence; its next failure retains the quote.
    tokio::time::advance(Duration::from_secs(204)).await;
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(attempts.borrow()[&1], 6);
    assert_eq!(attempts.borrow()[&56], 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(attempts.borrow()[&1], 7);
    assert_eq!(attempts.borrow()[&56], 2);
    assert_eq!(
        cache.native_usd_statuses()["1"].state,
        NativeUsdState::Failed
    );
    assert_eq!(cache.cached_native_usd_rate(1), Some(recovered));
    assert!(cache.native_usd_statuses()["1"].quote.is_some());

    // A new failure after success starts again at five seconds, even with a cached quote.
    tokio::time::advance(Duration::from_secs(4)).await;
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(attempts.borrow()[&1], 7);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(attempts.borrow()[&1], 8);
    assert_eq!(attempts.borrow()[&56], 2);
    assert_eq!(
        cache.native_usd_statuses()["1"].state,
        NativeUsdState::Available
    );
}

#[tokio::test(start_paused = true)]
async fn native_worker_waits_for_activation_and_fences_retired_reads() {
    use futures_util::FutureExt;
    use std::cell::RefCell;

    let mut chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .unwrap()
        .clone();
    chain.chain_id = 31337;
    chain.railgun = None;
    let mut configs: EffectiveChainRegistry = std::iter::once(chain).collect();
    let cache = TokenAnchorRateCache::new();
    cache.reconcile_native_sources(&configs, &[]);
    let requests = RefCell::new(Vec::new());
    let (configuration_tx, configuration_rx) = watch::channel(0_u64);
    let (_wake_tx, wake_rx) = watch::channel(0);
    let set_active = |ids: &[u64]| {
        if cache.set_active_native_chains(ids) {
            configuration_tx.send_modify(|revision| *revision += 1);
        }
    };
    let worker = run_worker_with(&cache, configuration_rx, wake_rx, |chain| {
        let (send, receive) = tokio::sync::oneshot::channel();
        requests.borrow_mut().push((chain.native_usd_oracle, send));
        async move { receive.await.unwrap() }
    });
    tokio::pin!(worker);

    assert!(worker.as_mut().now_or_never().is_none());
    assert!(requests.borrow().is_empty());
    assert_eq!(
        cache.native_usd_statuses()["31337"].state,
        NativeUsdState::Pending
    );
    tokio::time::advance(TOKEN_ANCHOR_REFRESH_INTERVAL).await;
    assert!(worker.as_mut().now_or_never().is_none());
    assert!(requests.borrow().is_empty());
    set_active(&[31337]);
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(requests.borrow().len(), 1);
    set_active(&[31337, 31337]);
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(requests.borrow().len(), 1);
    requests
        .borrow_mut()
        .pop()
        .unwrap()
        .1
        .send(Ok((U256::from(2_000_000), 8)))
        .unwrap();
    assert!(worker.as_mut().now_or_never().is_none());
    let published = cache.native_usd_statuses();
    assert_eq!(
        cache.cached_native_usd_rate(31337),
        Some(U256::from(2_000_000))
    );
    set_active(&[31337]);
    assert!(worker.as_mut().now_or_never().is_none());
    assert!(requests.borrow().is_empty());

    tokio::time::advance(TOKEN_ANCHOR_REFRESH_INTERVAL).await;
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(requests.borrow().len(), 1);
    set_active(&[]);
    assert!(worker.as_mut().now_or_never().is_none());
    requests
        .borrow_mut()
        .pop()
        .unwrap()
        .1
        .send(Ok((U256::MAX, 8)))
        .unwrap();
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(cache.native_usd_statuses(), published);
    assert_eq!(
        cache.cached_native_usd_rate(31337),
        Some(U256::from(2_000_000))
    );

    let replacement_source = Address::repeat_byte(7);
    configs.get_mut(31337).unwrap().native_usd_oracle = Some(replacement_source);
    assert!(cache.reconcile_native_sources(&configs, &[]));
    configuration_tx.send_modify(|revision| *revision += 1);
    assert_eq!(cache.cached_native_usd_rate(31337), None);
    assert!(worker.as_mut().now_or_never().is_none());
    tokio::time::advance(TOKEN_ANCHOR_REFRESH_INTERVAL).await;
    assert!(worker.as_mut().now_or_never().is_none());
    assert!(requests.borrow().is_empty());

    set_active(&[31337]);
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(
        requests.borrow().len(),
        1,
        "reactivation refreshes immediately"
    );
    let (source, send) = requests.borrow_mut().pop().unwrap();
    assert_eq!(source, Some(replacement_source));
    send.send(Ok((U256::from(3_000_000), 8))).unwrap();
    assert!(worker.as_mut().now_or_never().is_none());
    assert_eq!(
        cache.cached_native_usd_rate(31337),
        Some(U256::from(3_000_000))
    );
}

#[tokio::test]
async fn native_worker_batches_metadata_and_refreshes_saved_sources_without_token_work() {
    use crate::rpc_broker::tests::{RpcResponder, spawn_rpc_mock};
    use alloy::primitives::Bytes;
    use alloy::providers::bindings::IMulticall3;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let multicall = Address::repeat_byte(9);
    let batches = Arc::new(AtomicUsize::new(0));
    let responder: RpcResponder = {
        let batches = batches.clone();
        Arc::new(move |request| {
            assert_eq!(request["method"], "eth_call");
            assert_eq!(request["params"][0]["to"], multicall.to_string());
            let call = &request["params"][0];
            let bytes = call
                .get("input")
                .or_else(|| call.get("data"))
                .unwrap()
                .as_str()
                .unwrap()
                .parse::<Bytes>()
                .unwrap();
            let calls = IMulticall3::tryAggregateCall::abi_decode(&bytes)
                .unwrap()
                .calls;
            assert_eq!(calls.len(), 2, "price and precision share a multicall");
            assert_eq!(calls[0].target, calls[1].target);
            let returns = calls
                .iter()
                .map(|call| {
                    let data = if call.callData.as_ref()
                        == AggregatorInterface::latestAnswerCall::SELECTOR
                    {
                        AggregatorInterface::latestAnswerCall::abi_encode_returns(
                            &I256::try_from(200_000_000).unwrap(),
                        )
                    } else {
                        assert_eq!(
                            call.callData.as_ref(),
                            AggregatorInterface::decimalsCall::SELECTOR
                        );
                        AggregatorInterface::decimalsCall::abi_encode_returns(&8)
                    };
                    IMulticall3::Result {
                        success: true,
                        returnData: data.into(),
                    }
                })
                .collect::<Vec<_>>();
            batches.fetch_add(1, Ordering::SeqCst);
            json!({"jsonrpc": "2.0", "id": request["id"], "result": Bytes::from(IMulticall3::tryAggregateCall::abi_encode_returns(&returns))})
        })
    };
    let (endpoint, server) = spawn_rpc_mock(
        responder,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let settings = WalletSettings::default();
    let mut chain = build_effective_chain_configs(&settings)
        .unwrap()
        .get(1)
        .unwrap()
        .clone();
    chain.chain_id = 31337;
    chain.native_currency.decimals = 6;
    chain.railgun = None;
    chain.wrapped_native_token = None;
    chain.rpc_route = crate::RpcChainRoute::new(31337, vec![endpoint]).with_multicall(multicall);
    let mut configs: EffectiveChainRegistry = std::iter::once(chain).collect();
    let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
        network_mode: Some(crate::WalletNetworkMode::Direct),
        proxy: None,
        data_dir: &std::env::temp_dir(),
    })
    .await
    .unwrap();
    let cache = Arc::new(TokenAnchorRateCache::new());
    let mut updates = cache.subscribe_refreshes();
    let probe_http = http.clone();
    let worker = super::super::spawn_token_anchor_refresh_worker(
        &tokio::runtime::Handle::current(),
        cache.clone(),
        vec![],
        configs.clone(),
        crate::settings::build_effective_token_registry(&settings).unwrap(),
        http,
    );
    worker.set_active_chains(&[31337]);
    for replacement in [None, Some(Address::repeat_byte(7))] {
        if let Some(address) = replacement {
            configs.get_mut(31337).unwrap().native_usd_oracle = Some(address);
            worker.reconcile_native_sources(&configs, &[]);
            assert_eq!(cache.cached_native_usd_rate(31337), None);
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while cache.cached_native_usd_rate(31337).is_none() {
                updates.changed().await.unwrap();
            }
        })
        .await
        .expect("startup and replacement refresh promptly");
        assert_eq!(
            cache.cached_native_usd_micro_value(31337, U256::from(1_500_000)),
            Some(U256::from(3_000_000))
        );
    }
    assert_eq!(batches.load(Ordering::SeqCst), 2);
    // A Test reads the same source through the broker and publishes nothing.
    worker.set_active_chains(&[]);
    let published = cache.native_usd_statuses();
    let quote = probe_native_usd_quote(configs.get(31337).unwrap(), &probe_http)
        .await
        .unwrap();
    assert_eq!(quote.micro_usd, "2000000");
    assert_eq!(quote.feed_decimals, 8);
    assert_eq!(cache.native_usd_statuses(), published);
    configs.get_mut(31337).unwrap().enabled = false;
    worker.reconcile_native_sources(&configs, &[]);
    assert_eq!(cache.cached_native_usd_rate(31337), None);
    assert!(cache.native_reads().is_empty());
    drop(worker);
    server.abort();
}

#[test]
fn native_quote_decoding_discovers_precision_and_rejects_unusable_answers() {
    let decode = |answer: I256, decimals: u8| {
        decode_native_quote(
            1,
            &AggregatorInterface::latestAnswerCall::abi_encode_returns(&answer),
            &AggregatorInterface::decimalsCall::abi_encode_returns(&decimals),
        )
    };
    for decimals in [0, 6, 8, 18] {
        let answer = U256::from(3000) * U256::from(10).pow(U256::from(decimals));
        assert_eq!(
            decode(I256::try_from(answer).unwrap(), decimals),
            Ok((uint!(3_000_000_000_U256), decimals))
        );
    }
    for (answer, decimals, expected) in [
        (I256::ZERO, 8, "price of 0"),
        (I256::MINUS_ONE, 8, "negative price"),
        (I256::ONE, 8, "rounds to $0"),
        (I256::MAX, 0, "does not fit"),
        (I256::MAX, 255, "does not fit"),
    ] {
        let error = decode(answer, decimals).unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
    assert!(
        decode_native_quote(8453, &[], &[])
            .unwrap_err()
            .contains("No contract answered at this address on chain 8453")
    );
    assert!(
        decode_native_quote(1, &[0], &[8])
            .unwrap_err()
            .contains("do not decode as int256")
    );
    let answer =
        AggregatorInterface::latestAnswerCall::abi_encode_returns(&I256::try_from(3000).unwrap());
    assert!(
        decode_native_quote(1, &answer, &[])
            .unwrap_err()
            .contains("do not decode as uint8")
    );
}

#[test]
fn public_only_precision_changes_revalue_amounts_without_losing_quotes() {
    let cache = TokenAnchorRateCache::new();
    let mut settings = WalletSettings::default();
    let definition = crate::settings::CustomChainSettings {
        native_usd_pricing: NativeUsdPricing::Oracle {
            contract_address: Address::repeat_byte(7),
        },
        name: "Public EVM".into(),
        native_currency: crate::settings::NativeCurrency {
            name: "Coin".into(),
            symbol: "COIN".into(),
            decimals: 6,
        },
        rpc_endpoints: vec!["https://rpc.example".to_owned().into()],
        explorer_urls: vec![],
        enabled: true,
        contracts: crate::settings::ChainContractSettings::default(),
        finality_depth: None,
        gas: crate::settings::ChainGasSettings::default(),
    };
    settings.chains.custom.insert(31337, definition);
    let mut configs = build_effective_chain_configs(&settings).unwrap();
    cache.reconcile_native_sources(&configs, &[]);
    cache.set_active_native_chains(&[31337]);
    let read = cache
        .native_reads()
        .into_iter()
        .find(|read| read.chain.chain_id == 31337)
        .unwrap();
    let config = configs.get(31337).unwrap();
    assert!(config.railgun.is_none());
    assert!(config.wrapped_native_token.is_none());
    cache.publish_native(&read, Ok((U256::from(2_000_000), 8)));
    assert_eq!(
        cache.cached_native_usd_micro_value(31337, U256::from(1_500_000)),
        Some(U256::from(3_000_000))
    );
    settings
        .chains
        .custom
        .get_mut(&31337)
        .unwrap()
        .native_currency
        .decimals = 3;
    configs = build_effective_chain_configs(&settings).unwrap();
    cache.reconcile_native_sources(&configs, &[]);
    assert_eq!(
        cache.cached_native_usd_micro_value(31337, U256::from(1500)),
        Some(U256::from(3_000_000))
    );
    cache.publish_native(&read, Err("Old precision".to_owned()));
    assert_eq!(
        cache.native_usd_statuses()["31337"].state,
        NativeUsdState::Pending
    );
    let current = cache
        .native_reads()
        .into_iter()
        .find(|read| read.chain.chain_id == 31337)
        .unwrap();
    settings.chains.custom.remove(&31337);
    cache.reconcile_native_sources(&build_effective_chain_configs(&settings).unwrap(), &[31337]);
    cache.publish_native(&current, Ok((U256::MAX, 77)));
    assert_eq!(cache.cached_native_usd_rate(31337), None);
    assert!(!cache.native_usd_statuses().contains_key("31337"));
}

#[tokio::test]
async fn native_source_changes_fence_late_results_and_preserve_unrelated_quotes() {
    let cache = Arc::new(TokenAnchorRateCache::new());
    let mut settings = WalletSettings::default();
    let mut configs = build_effective_chain_configs(&settings).unwrap();
    cache.reconcile_native_sources(&configs, &[]);
    cache.set_active_native_chains(&[1, 56]);
    let read = |id| {
        cache
            .native_reads()
            .into_iter()
            .find(|read| read.chain.chain_id == id)
            .unwrap()
    };
    let original = read(1);
    cache.publish_native(&original, Ok((U256::from(3_000_000_000_u64), 8)));
    cache.publish_native(&read(56), Ok((U256::from(500_000_000), 8)));
    cache.publish_native(&original, Err("Connection unavailable".to_owned()));
    assert_eq!(
        cache.cached_native_usd_rate(1),
        Some(U256::from(3_000_000_000_u64))
    );
    assert_eq!(
        cache.native_usd_statuses()["1"].state,
        NativeUsdState::Failed
    );
    let mut notifications = cache.subscribe_refreshes();
    // An unrelated settings revision must admit an already pending read.
    settings.runtime.auto_lock_timeout_secs = Some(600);
    configs = build_effective_chain_configs(&settings).unwrap();
    assert!(!cache.reconcile_native_sources(&configs, &[]));
    cache.publish_native(&original, Ok((U256::from(3_001_000_000_u64), 18)));
    assert_eq!(
        cache.native_usd_statuses()["1"]
            .quote
            .as_ref()
            .unwrap()
            .feed_decimals,
        18
    );
    for replacement in [
        NativeUsdPricing::Oracle {
            contract_address: Address::repeat_byte(7),
        },
        NativeUsdPricing::Default,
        NativeUsdPricing::Disabled,
    ] {
        let old = read(1);
        let (finish, pending) = tokio::sync::oneshot::channel();
        let stale_cache = cache.clone();
        let stale = tokio::spawn(async move {
            pending.await.unwrap();
            stale_cache.publish_native(&old, Ok((U256::MAX, 77)));
            stale_cache.publish_native(&old, Err("Retired failure".to_owned()));
        });
        settings
            .chains
            .per_chain
            .get_mut(&1)
            .unwrap()
            .native_usd_pricing = replacement;
        configs = build_effective_chain_configs(&settings).unwrap();
        notifications.borrow_and_update();
        cache.reconcile_native_sources(&configs, &[]);
        assert!(notifications.has_changed().unwrap());
        assert_eq!(cache.cached_native_usd_rate(1), None);
        let current_status = cache.native_usd_statuses()["1"].clone();
        finish.send(()).unwrap();
        stale.await.unwrap();
        assert_eq!(cache.native_usd_statuses()["1"], current_status);
        assert_eq!(cache.cached_native_usd_rate(1), None);
        assert_eq!(
            cache.cached_native_usd_rate(56),
            Some(U256::from(500_000_000))
        );
        if replacement != NativeUsdPricing::Disabled {
            cache.publish_native(&read(1), Err("Replacement unavailable".to_owned()));
            assert_eq!(cache.cached_native_usd_rate(1), None);
        }
    }
    assert!(
        !cache
            .native_reads()
            .iter()
            .any(|read| read.chain.chain_id == 1)
    );
    // Even the original A generation cannot return after A -> B -> A -> Disabled.
    cache.publish_native(&original, Ok((U256::MAX, 77)));
    assert_eq!(cache.cached_native_usd_rate(1), None);
}
