use super::*;
use crate::rpc_broker::tests::general;
use crate::{PublicTransactionLookup, RpcChainRoute, RpcTrafficClass};
use serde_json::json;
use std::sync::Mutex;

#[tokio::test]
async fn custom_native_preparation_signs_observed_envelope_with_reviewed_bounds() {
    use crate::settings::{
        CustomChainSettings, NativeCurrency, WalletSettings, build_effective_chain_configs,
    };
    use alloy::consensus::{Transaction, TxEnvelope};
    use alloy::eips::Decodable2718;

    const CHAIN: u64 = 9_007_199_254_740_993;
    let mut settings = WalletSettings::default();
    settings.chains.custom.insert(
        CHAIN,
        CustomChainSettings {
            native_usd_pricing: crate::settings::NativeUsdPricing::Default,
            name: "Custom EVM".into(),
            native_currency: NativeCurrency {
                name: "Custom".into(),
                symbol: "CSTM".into(),
                decimals: 6,
            },
            rpc_endpoints: vec!["http://127.0.0.1:9".to_owned().into()],
            explorer_urls: vec![],
            enabled: true,
            contracts: crate::settings::ChainContractSettings::default(),
            finality_depth: None,
            gas: crate::settings::ChainGasSettings::default(),
        },
    );
    let chain = build_effective_chain_configs(&settings)
        .unwrap()
        .get(CHAIN)
        .cloned()
        .unwrap();
    let amount = crate::parse_send_amount("1.25", Some(chain.native_currency.decimals)).unwrap();
    assert_eq!(chain.native_currency.format_amount(amount), "1.25 CSTM");
    assert!(chain.railgun.is_none() && chain.wrapped_native_token.is_none());
    let http = HttpContext::direct_for_tests();
    let route = chain
        .rpc_route
        .for_class(RpcTrafficClass::Sensitive)
        .unwrap();
    let pool = crate::query_rpc_pool_for_route(&route, &http);
    let signer = VaultedPublicSigner::Software(
        crate::signer::SoftwareEvmSigner::from_private_key([7; 32]).unwrap(),
    );
    let from = signer.address();
    for (base_fee, failed_header, estimated_gas) in [
        (None, false, 42_000),
        (Some(5), false, 42_000),
        (None, true, 42_000),
        (Some(5), false, 50_001),
    ] {
        let reads = crate::DappRpcReadClient::new(move |_, read| {
            Box::pin(async move {
                Ok(match read.method() {
                    "eth_getBlockByNumber" if failed_header => {
                        return Err(crate::RpcBrokerError::InvalidResponse);
                    }
                    "eth_getBlockByNumber" => {
                        let mut block: alloy::rpc::types::Block =
                            alloy::rpc::types::Block::default();
                        block.header.inner.base_fee_per_gas = base_fee;
                        serde_json::to_value(block).unwrap()
                    }
                    "eth_gasPrice" => json!("0x64"),
                    "eth_maxPriorityFeePerGas" => json!("0x2"),
                    "eth_feeHistory" => return Err(crate::RpcBrokerError::InvalidResponse),
                    "eth_getTransactionCount" => json!("0x7"),
                    "eth_getBalance" => json!("0x100000000"),
                    "eth_estimateGas" => json!(format!("0x{estimated_gas:x}")),
                    method => panic!("unexpected read {method}"),
                })
            })
        });
        let prepared = public_action_preflight_from_rpc_pool_with_reads(
            &pool,
            &http,
            &route,
            CHAIN,
            from,
            TransactionRequest::default()
                .with_to(Address::repeat_byte(2))
                .with_value(amount),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 2,
            },
            &chain.gas,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            PublicActionGasLimitStrategy::ChainBuffer,
            Some(50_000),
            None,
            None,
            None,
            false,
            Some(&reads),
            None,
        )
        .await;
        if failed_header || estimated_gas > 50_000 {
            assert!(
                prepared.is_err(),
                "failed observation or exceeded review must prevent signing"
            );
            continue;
        }
        let prepared = prepared.unwrap();
        let exposure = prepared.estimated_native_gas_cost;
        let signed = signer
            .sign_transaction_request(prepared.tx_req, "custom native test")
            .await
            .unwrap();
        let envelope = TxEnvelope::decode_2718(&mut signed.as_slice()).unwrap();
        assert_eq!(envelope.chain_id(), Some(CHAIN));
        assert_eq!(envelope.value(), U256::from(1_250_000));
        assert_eq!(envelope.gas_limit(), 50_000);
        assert_eq!(envelope.max_fee_per_gas(), 100);
        assert_eq!(
            envelope.value()
                + U256::from(envelope.gas_limit()) * U256::from(envelope.max_fee_per_gas()),
            exposure
        );
        assert_eq!(
            matches!(envelope, TxEnvelope::Legacy(_)),
            base_fee.is_none()
        );
        assert_eq!(envelope.max_priority_fee_per_gas(), base_fee.map(|_| 2));
    }
}

#[tokio::test]
async fn broadcast_handoff_retains_ambiguous_attempt_before_replacement() {
    let (tracker, context) = crate::public_wallet::test_tracking_context();
    let observed_tracker = tracker.clone();
    let original = vec![1_u8];
    let replacement = vec![2_u8];
    let original_hash = keccak256(&original);
    let replacement_hash = keccak256(&replacement);
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request| {
            if request["method"] == "eth_blockNumber" {
                return json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"});
            }
            assert_eq!(request["method"], "eth_sendRawTransaction");
            let bytes = alloy::hex::decode(request["params"][0].as_str().unwrap()).unwrap();
            let hash = keccak256(bytes);
            assert_eq!(
                observed_tracker.lookup(1, hash),
                PublicTransactionLookup::Pending
            );
            if hash == original_hash {
                json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                    "code": -32000, "message": "submission outcome unavailable"
                }})
            } else {
                json!({"jsonrpc": "2.0", "id": request["id"], "result": hash})
            }
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let http = HttpContext::direct_for_tests();
    let route = general(&RpcChainRoute::new(1, vec![endpoint.clone()]));
    let pool = crate::query_rpc_pool_for_route(&route, &http);
    let mut observer = BlockObserver::establish(pool, 1, http.rpc_broker(), &route)
        .await
        .unwrap()
        .with_tracking(&context);
    let mut attempts = Vec::new();
    for (signed, fee, expiry) in [
        (original.clone(), 10, Some(0)),
        (original, 10, None),
        (replacement, 20, None),
    ] {
        let pool = crate::query_rpc_pool_for_route(
            &general(&RpcChainRoute::new(1, vec![endpoint.clone()])),
            &http,
        );
        let result = broadcast_signed_public_action_transaction(
            &pool,
            http.network_mode(),
            signed,
            "test broadcast",
            expiry,
            true,
            &mut |tx_hash, _| {
                retain_public_action_attempt(
                    &mut observer,
                    &mut attempts,
                    &SubmittedPublicActionAttempt {
                        tx_hash,
                        info: PublicActionAttemptInfo {
                            tx_hash: alloy::hex::encode_prefixed(tx_hash),
                            nonce: 7,
                            gas_limit: 21_000,
                            max_fee_per_gas: fee,
                            max_priority_fee_per_gas: fee / 2,
                        },
                        rpc_gas_price: fee,
                        estimated_native_gas_cost: U256::ZERO,
                        live_native_balance: U256::ZERO,
                    },
                );
                Ok(())
            },
        )
        .await;
        if expiry.is_some() {
            assert!(matches!(result, Err(PublicActionAttemptError::Sending(_))));
            assert!(attempts.is_empty());
            assert_eq!(
                tracker.lookup(1, original_hash),
                PublicTransactionLookup::Untracked
            );
        } else if fee == 10 {
            assert!(matches!(result, Err(PublicActionAttemptError::Sending(_))));
            assert_eq!(attempts.len(), 1);
        } else {
            assert!(result.is_ok());
        }
    }
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].tx_hash, original_hash);
    assert_eq!(attempts[1].tx_hash, replacement_hash);
    for (winner, fee) in [(0, 10), (1, 20)] {
        assert_eq!(
            public_action_winner_gas_fee(&attempts, winner),
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: fee,
                max_priority_fee_per_gas: fee / 2,
            },
        );
    }
    server.abort();
}

/// A chain-1 result quoting fixed fees and estimating `estimate` gas. It passes each raw
/// broadcast's hash to `on_send` before accepting it.
fn session_result(
    estimate: u64,
    request: &serde_json::Value,
    on_send: &dyn Fn(FixedBytes<32>),
) -> serde_json::Value {
    match request["method"].as_str().unwrap_or_default() {
        "eth_sendRawTransaction" => {
            let raw = request["params"][0].as_str().unwrap();
            let hash = keccak256(alloy::hex::decode(raw).unwrap());
            on_send(hash);
            json!(hash)
        }
        "eth_getBlockByNumber" => {
            let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
            block.header.inner.base_fee_per_gas = Some(5);
            serde_json::to_value(block).unwrap()
        }
        "eth_gasPrice" => json!("0x64"),
        "eth_maxPriorityFeePerGas" => json!("0x2"),
        "eth_feeHistory" => json!({
            "oldestBlock": "0x1",
            "baseFeePerGas": ["0x5", "0x5"],
            "gasUsedRatio": [0.5],
            "reward": [["0x1", "0x2", "0x3", "0x4"]]
        }),
        "eth_estimateGas" => json!(format!("0x{estimate:x}")),
        "eth_getTransactionCount" => json!("0x7"),
        "eth_getBalance" => json!("0x3635c9adc5dea00000"),
        "eth_getTransactionReceipt" => serde_json::Value::Null,
        _ => json!("0x1"),
    }
}

/// An endpoint answering with [`session_result`] that logs each method.
async fn session_endpoint(
    estimate: u64,
    on_send: impl Fn(FixedBytes<32>) + Send + Sync + 'static,
) -> (
    url::Url,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<Vec<String>>>,
) {
    let methods = Arc::new(Mutex::new(Vec::new()));
    let logged = methods.clone();
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: serde_json::Value| {
            let result = session_result(estimate, &request, &on_send);
            let method = request["method"].as_str().unwrap_or_default().to_owned();
            logged.lock().unwrap().push(method);
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    (endpoint, server, methods)
}

/// Runs a chain-1 native shield step session on `route`'s Public action pools with a
/// software signer.
async fn shield_session(
    route: RpcChainRoute,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    gas_fee: PublicActionGasFeeSelection,
    command_rx: &mut Option<PublicActionCommandReceiver>,
    event_tx: Option<&PublicActionSessionEventSender>,
    handoff: &mut PublicActionHandoff<'_>,
) -> Result<PublicActionStepResult> {
    let http = HttpContext::direct_for_tests();
    let client = http.operation_http_client().await.unwrap();
    shield_session_over(
        &http,
        &client,
        None,
        route,
        gas_limit_strategy,
        gas_fee,
        command_rx,
        event_tx,
        handoff,
    )
    .await
}

/// [`shield_session`] over `http` and the operation's `client`, tracked by
/// `transaction_tracking`.
async fn shield_session_over(
    http: &HttpContext,
    client: &OperationHttpClient,
    transaction_tracking: Option<&crate::PublicTransactionTrackingContext>,
    route: RpcChainRoute,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    gas_fee: PublicActionGasFeeSelection,
    command_rx: &mut Option<PublicActionCommandReceiver>,
    event_tx: Option<&PublicActionSessionEventSender>,
    handoff: &mut PublicActionHandoff<'_>,
) -> Result<PublicActionStepResult> {
    let chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let pools = public_action_pools(&route, http, client).expect("route has every class");
    let signer = VaultedPublicSigner::Software(
        crate::signer::SoftwareEvmSigner::from_private_key([7; 32]).unwrap(),
    );
    submit_public_action_step_session(
        PublicActionProgressStep::Shield,
        TransactionRequest::default()
            .with_to(Address::repeat_byte(2))
            .with_value(U256::from(1)),
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        gas_limit_strategy,
        &signer,
        "public action session test",
        &pools,
        1,
        http,
        transaction_tracking,
        None,
        1,
        signer.address(),
        &chain.gas,
        None,
        None,
        None,
        gas_fee,
        PublicActionStepFeePolicy::Custom,
        None,
        command_rx,
        event_tx,
        Some(handoff),
        PublicActionStepEnd::Receipt,
        &mut |_| {},
    )
    .await
}

#[tokio::test]
async fn handoff_session_records_each_replacement_before_its_broadcast() {
    use crate::public_wallet::tests::LogWriter;
    use tracing::instrument::WithSubscriber;

    let handed_off = Arc::new(Mutex::new(Vec::new()));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let (endpoint, server, _) = session_endpoint(21_000, {
        let handed_off = handed_off.clone();
        let sent = sent.clone();
        move |hash| {
            let recorded = handed_off
                .lock()
                .unwrap()
                .iter()
                .any(|(handed_off, _)| *handed_off == hash);
            sent.lock().unwrap().push((hash, recorded));
        }
    })
    .await;
    let fee = |max_fee_per_gas| PublicActionGasFeeSelection::Custom {
        max_fee_per_gas,
        max_priority_fee_per_gas: 2,
    };
    // The first replacement's hand-off fails; the second's succeeds.
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    for max_fee_per_gas in [200, 300] {
        command_tx
            .send(PublicActionCommand {
                kind: crate::PublicActionCommandKind::Replacement,
                gas_fee: fee(max_fee_per_gas),
            })
            .unwrap();
    }
    let mut command_rx = Some(command_rx);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut handoff = {
        let handed_off = handed_off.clone();
        move |tx_hash: FixedBytes<32>, request: &TransactionRequest, _: &[u8]| -> Result<()> {
            let mut handed_off = handed_off.lock().unwrap();
            handed_off.push((tx_hash, request.max_fee_per_gas.or(request.gas_price)));
            if handed_off.len() == 2 {
                return Err(eyre!("replacement hand-off failed"));
            }
            Ok(())
        }
    };
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(logs.clone());
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let mut events = Vec::new();
    let mut submitted = Vec::new();
    let session = Box::pin(shield_session(
        RpcChainRoute::new(1, vec![endpoint]),
        PublicActionGasLimitStrategy::ChainBuffer,
        fee(100),
        &mut command_rx,
        Some(&event_tx),
        &mut handoff,
    ));
    let observe = async {
        while submitted.len() < 2 {
            let event = event_rx.recv().await.unwrap();
            if let PublicActionSessionEvent::AttemptSubmitted { attempt, .. } = &event {
                submitted.push(attempt.clone());
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            result = session => panic!("session ended early: {:?}", result.err()),
            () = observe => {}
        }
    })
    .with_subscriber(subscriber)
    .await
    .expect("both accepted attempts are submitted");

    let handed_off = handed_off.lock().unwrap().clone();
    assert_eq!(handed_off.len(), 3);
    // Each broadcast attempt was recorded first; the failed replacement was never sent.
    assert_eq!(
        *sent.lock().unwrap(),
        vec![(handed_off[0].0, true), (handed_off[2].0, true)]
    );
    for (attempt, (tx_hash, max_fee_per_gas)) in
        submitted.iter().zip([handed_off[0], handed_off[2]])
    {
        assert_eq!(attempt.tx_hash, alloy::hex::encode_prefixed(tx_hash));
        assert_eq!(Some(attempt.max_fee_per_gas), max_fee_per_gas);
    }
    assert_ne!(handed_off[0].1, handed_off[2].1);
    // The rejection leaves the session observing and accepting the next replacement.
    let rejected = events
        .iter()
        .position(|event| {
            matches!(
                event,
                PublicActionSessionEvent::AttemptRejected { message, .. }
                    if message.contains("replacement hand-off failed")
            )
        })
        .expect("failed replacement hand-off is rejected");
    let accepted = events
        .iter()
        .rposition(|event| matches!(event, PublicActionSessionEvent::AttemptSubmitted { .. }))
        .unwrap();
    assert!(rejected < accepted);
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains("public action transaction"), "{logs}");
    assert!(!logs.contains("self-broadcast tx"), "{logs}");
    drop(command_tx);
    server.abort();
}

#[tokio::test]
async fn session_journals_each_attempt_before_its_send_whatever_the_response() {
    use crate::desktop::PublicActivity;
    use crate::public_wallet::tests::{
        TEST_IMPORTED_PRIVATE_KEY, TEST_PASSWORD, public_action_request_parts,
    };
    use crate::vault::{ActivityEntry, ActivityJournal, ActivityKind, ActivityOwner};

    let (root_dir, db, store, view) = public_action_request_parts();
    let account = store
        .import_public_account(TEST_PASSWORD, &view, TEST_IMPORTED_PRIVATE_KEY, None, false)
        .unwrap();
    let owner = ActivityOwner::Account(account.public_account_uuid.clone());
    let journal = ActivityJournal::new(Arc::clone(&db), Arc::clone(&view)).unwrap();
    let activity = ActivitySubmission::public(
        &store,
        &view,
        PublicActivity {
            account_uuid: account.public_account_uuid,
            chain_id: 1,
            kind: ActivityKind::PublicSend,
            legs: Vec::new(),
            counterparty: None,
        },
    );
    // Each raw send notes whether its hash was journaled first. Every send of the first
    // attempt fails, as when the response is lost after a node accepted it.
    let sends = Arc::new(Mutex::new(Vec::<(FixedBytes<32>, bool)>::new()));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new({
            let sends = sends.clone();
            let journal = journal.clone();
            let owner = owner.clone();
            move |request: serde_json::Value| {
                if request["method"] == "eth_sendRawTransaction" {
                    let raw = request["params"][0].as_str().unwrap();
                    let hash = keccak256(alloy::hex::decode(raw).unwrap());
                    let journaled = journal
                        .list(&owner)
                        .unwrap()
                        .iter()
                        .any(|entry| entry.attempts.iter().any(|attempt| attempt.hash == hash));
                    let mut sends = sends.lock().unwrap();
                    sends.push((hash, journaled));
                    if sends[0].0 == hash {
                        return json!({"jsonrpc": "2.0", "id": request["id"], "error": {
                            "code": -32000, "message": "submission outcome unavailable"
                        }});
                    }
                }
                let result = session_result(21_000, &request, &|_| {});
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            }
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let http = HttpContext::direct_for_tests();
    let client = http.operation_http_client().await.unwrap();
    let chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let pools = public_action_pools(&RpcChainRoute::new(1, vec![endpoint]), &http, &client)
        .expect("route has every class");
    let signer = VaultedPublicSigner::Software(
        crate::signer::SoftwareEvmSigner::from_private_key([7; 32]).unwrap(),
    );
    let fee = |max_fee_per_gas| PublicActionGasFeeSelection::Custom {
        max_fee_per_gas,
        max_priority_fee_per_gas: 2,
    };
    // A gas retry, queued for once the first attempt is handed off.
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    command_tx
        .send(PublicActionCommand {
            kind: crate::PublicActionCommandKind::Replacement,
            gas_fee: fee(200),
        })
        .unwrap();
    let mut command_rx = Some(command_rx);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut on_progress = |_| {};
    let session = Box::pin(submit_public_action_step_session(
        PublicActionProgressStep::Send,
        TransactionRequest::default()
            .with_to(Address::repeat_byte(2))
            .with_value(U256::from(1)),
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        PublicActionGasLimitStrategy::ChainBuffer,
        &signer,
        "public action journal test",
        &pools,
        1,
        &http,
        None,
        Some((&activity, ActivityStep::Main)),
        1,
        signer.address(),
        &chain.gas,
        None,
        None,
        None,
        fee(100),
        PublicActionStepFeePolicy::Custom,
        None,
        &mut command_rx,
        Some(&event_tx),
        None,
        PublicActionStepEnd::Receipt,
        &mut on_progress,
    ));
    let retried = async {
        while !matches!(
            event_rx.recv().await.unwrap(),
            PublicActionSessionEvent::AttemptSubmitted { .. }
        ) {}
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            result = session => panic!("session ended early: {:?}", result.err()),
            () = retried => {}
        }
    })
    .await
    .expect("the gas retry is submitted");

    let sends = sends.lock().unwrap().clone();
    assert!(sends.iter().all(|(_, journaled)| *journaled), "{sends:?}");
    let mut sent = Vec::new();
    for (hash, _) in sends {
        if !sent.contains(&hash) {
            sent.push(hash);
        }
    }
    assert_eq!(sent.len(), 2);
    let [entry]: [ActivityEntry; 1] = journal.list(&owner).unwrap().try_into().unwrap();
    assert_eq!(
        entry
            .attempts
            .iter()
            .map(|attempt| (attempt.hash, attempt.step))
            .collect::<Vec<_>>(),
        sent.iter()
            .map(|hash| (*hash, ActivityStep::Main))
            .collect::<Vec<_>>()
    );
    drop(command_tx);
    server.abort();
    drop(activity);
    drop(journal);
    drop(store);
    drop(view);
    drop(db);
    std::fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn native_shield_preflight_failure_ends_session_despite_queued_retry() {
    const REVIEWED_GAS_LIMIT: u64 = 50_000;
    let (endpoint, server, methods) = session_endpoint(REVIEWED_GAS_LIMIT + 1, |_| {}).await;
    let fee = PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: 100,
        max_priority_fee_per_gas: 2,
    };
    // The sender stays alive so only the native-shield rule can end the session.
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    command_tx
        .send(PublicActionCommand {
            kind: crate::PublicActionCommandKind::Retry,
            gas_fee: fee,
        })
        .unwrap();
    let mut command_rx = Some(command_rx);
    let handed_off = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handoff = {
        let handed_off = handed_off.clone();
        move |_: FixedBytes<32>, _: &TransactionRequest, _: &[u8]| -> Result<()> {
            handed_off.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    };
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        Box::pin(shield_session(
            RpcChainRoute::new(1, vec![endpoint]),
            PublicActionGasLimitStrategy::NativeShieldPrepared {
                reviewed_gas_limit: REVIEWED_GAS_LIMIT,
                profile: PublicShieldTransactionProfile::Railoxide,
            },
            fee,
            &mut command_rx,
            None,
            &mut handoff,
        )),
    )
    .await
    .expect("native shield preflight failure ends the session");

    let error = result.err().expect("oversized native estimate fails");
    assert!(
        report_chain_string(&error).contains("refreshed preparation"),
        "{error:?}"
    );
    let methods = methods.lock().unwrap().clone();
    assert!(methods.iter().any(|method| method == "eth_estimateGas"));
    assert!(
        !methods
            .iter()
            .any(|method| method == "eth_sendRawTransaction")
    );
    assert!(!handed_off.load(std::sync::atomic::Ordering::SeqCst));
    drop(command_tx);
    server.abort();
}

#[tokio::test]
async fn public_action_reads_go_to_sensitive_endpoints_and_broadcast_to_submit_ones() {
    let (general_only, general_server, general_methods) = session_endpoint(21_000, |_| {}).await;
    let (reader, reader_server, reader_methods) = session_endpoint(21_000, |_| {}).await;
    let (submitter, submitter_server, submitter_methods) = session_endpoint(21_000, |_| {}).await;
    let roles = |general, sensitive, submit| crate::RpcEndpointRoles {
        general,
        sensitive,
        submit,
    };
    let route = RpcChainRoute::new_with_transports(
        1,
        vec![
            (
                general_only.into(),
                crate::RpcTransport::Wallet,
                roles(true, false, false),
            ),
            (
                reader.into(),
                crate::RpcTransport::Wallet,
                roles(false, true, false),
            ),
            (
                submitter.into(),
                crate::RpcTransport::Wallet,
                roles(false, false, true),
            ),
        ],
    );
    let mut command_rx = None;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut handoff =
        |_: FixedBytes<32>, _: &TransactionRequest, _: &[u8]| -> Result<()> { Ok(()) };
    let session = Box::pin(shield_session(
        route,
        PublicActionGasLimitStrategy::ChainBuffer,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
        },
        &mut command_rx,
        Some(&event_tx),
        &mut handoff,
    ));
    let submitted = async {
        while !matches!(
            event_rx.recv().await.unwrap(),
            PublicActionSessionEvent::AttemptSubmitted { .. }
        ) {}
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            result = session => panic!("session ended early: {:?}", result.err()),
            () = submitted => {}
        }
    })
    .await
    .expect("the attempt is submitted");

    assert!(general_methods.lock().unwrap().is_empty());
    let reader_methods = reader_methods.lock().unwrap().clone();
    let read = |method: &str| reader_methods.iter().any(|read| read == method);
    assert!(
        ["eth_gasPrice", "eth_maxPriorityFeePerGas", "eth_feeHistory"]
            .into_iter()
            .any(read)
    );
    assert!(read("eth_getTransactionCount") && read("eth_estimateGas"));
    assert!(
        !reader_methods
            .iter()
            .any(|read| read == "eth_sendRawTransaction")
    );
    assert_eq!(
        *submitter_methods.lock().unwrap(),
        vec!["eth_sendRawTransaction".to_owned()]
    );
    general_server.abort();
    reader_server.abort();
    submitter_server.abort();
}

#[tokio::test(start_paused = true)]
async fn public_action_and_its_handed_off_observation_use_only_the_operation_client() {
    use crate::rpc_broker::tests::{RpcResponder, spawn_recording_rpc_mock, tagged_reqwest_client};
    use std::sync::atomic::{AtomicBool, Ordering};

    let block_hash = FixedBytes::<32>::repeat_byte(0x22);
    let sent = Arc::new(Mutex::new(None));
    let included = Arc::new(AtomicBool::new(false));
    // Head 1 until `included`, then head 2 holding the broadcast transaction.
    let responder: RpcResponder = {
        let sent = sent.clone();
        let included = included.clone();
        Arc::new(move |request: serde_json::Value| {
            let hash = *sent.lock().unwrap();
            let result = match (request["method"].as_str(), hash) {
                (Some("eth_blockNumber"), _) if included.load(Ordering::SeqCst) => json!("0x2"),
                (Some("eth_getBlockByNumber"), Some(hash)) if request["params"][0] == "0x2" => {
                    let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
                    block.header.hash = block_hash;
                    block.header.inner.number = 2;
                    block.transactions = alloy::rpc::types::BlockTransactions::Hashes(vec![hash]);
                    serde_json::to_value(block).unwrap()
                }
                (Some("eth_getBlockReceipts"), Some(hash)) => json!([{
                    "transactionHash": hash,
                    "blockHash": block_hash,
                    "blockNumber": "0x2",
                    "transactionIndex": "0x0",
                    "from": Address::ZERO,
                    "to": Address::ZERO,
                    "cumulativeGasUsed": "0x5208",
                    "status": "0x1",
                    "gasUsed": "0x5208",
                    "effectiveGasPrice": "0x1",
                    "logs": [],
                    "logsBloom": format!("0x{}", "00".repeat(256)),
                    "type": "0x0",
                    "contractAddress": null,
                }]),
                _ => session_result(21_000, &request, &|hash| {
                    *sent.lock().unwrap() = Some(hash);
                }),
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        })
    };
    let (reader, reader_requests, reader_server) =
        spawn_recording_rpc_mock(responder.clone()).await;
    let (submitter, submitter_requests, submitter_server) =
        spawn_recording_rpc_mock(responder).await;
    let roles = |sensitive, submit| crate::RpcEndpointRoles {
        general: false,
        sensitive,
        submit,
    };
    let route = RpcChainRoute::new_with_transports(
        1,
        vec![
            (
                reader.into(),
                crate::RpcTransport::Wallet,
                roles(true, false),
            ),
            (
                submitter.into(),
                crate::RpcTransport::Wallet,
                roles(false, true),
            ),
        ],
    );
    let http = HttpContext::with_rpc_client_for_tests(
        tagged_reqwest_client("shared"),
        crate::WalletNetworkMode::Direct,
    );
    let client = OperationHttpClient::for_tests(
        tagged_reqwest_client("operation"),
        crate::OperationNetworkIsolation::Dedicated,
    );
    let (tracker, tracking) = crate::public_wallet::test_tracking_context();
    let mut command_rx = None;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut handoff =
        |_: FixedBytes<32>, _: &TransactionRequest, _: &[u8]| -> Result<()> { Ok(()) };
    let session = Box::pin(shield_session_over(
        &http,
        &client,
        Some(&tracking),
        route,
        PublicActionGasLimitStrategy::ChainBuffer,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
        },
        &mut command_rx,
        Some(&event_tx),
        &mut handoff,
    ));
    // The progress UI closes after the broadcast, which ends the session before its
    // receipt and hands its observer to the tracker.
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::select! {
            result = session => panic!("session ended early: {:?}", result.err()),
            () = async {
                while !matches!(
                    event_rx.recv().await.unwrap(),
                    PublicActionSessionEvent::AttemptSubmitted { .. }
                ) {}
            } => {}
        }
    })
    .await
    .expect("the attempt is submitted");
    let hash = (*sent.lock().unwrap()).expect("the attempt was broadcast");

    // The tracker keeps polling before the inclusion and resolves the receipt after it.
    let handed_off_at = reader_requests.lock().unwrap().len();
    tokio::time::timeout(Duration::from_secs(10), async {
        while reader_requests.lock().unwrap().len() == handed_off_at {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the tracker polls the handed-off observer");
    included.store(true, Ordering::SeqCst);
    let mut changes = tracker.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            tracker.lookup(1, hash),
            PublicTransactionLookup::Included { .. }
        ) {
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("the handed-off observation resolves the receipt");
    // Polling stops at the receipt; the tracker would otherwise poll again within 3 s.
    let resolved_at = reader_requests.lock().unwrap().len();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(reader_requests.lock().unwrap().len(), resolved_at);

    let reader_requests = reader_requests.lock().unwrap().clone();
    let submitter_requests = submitter_requests.lock().unwrap().clone();
    assert!(
        reader_requests
            .iter()
            .chain(&submitter_requests)
            .all(|(tag, _)| tag.as_deref() == Some("operation")),
        "every request goes over the operation's client"
    );
    let methods = |requests: &[(Option<String>, serde_json::Value)]| {
        requests
            .iter()
            .map(|(_, body)| body["method"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
    };
    let reads = methods(&reader_requests);
    for method in [
        "eth_getTransactionCount",
        "eth_estimateGas",
        "eth_blockNumber",
        "eth_getBlockReceipts",
    ] {
        assert!(reads.iter().any(|read| read == method), "{method}");
    }
    assert!(!reads.iter().any(|read| read == "eth_sendRawTransaction"));
    assert_eq!(methods(&submitter_requests), ["eth_sendRawTransaction"]);
    tracker.shutdown().await;
    reader_server.abort();
    submitter_server.abort();
}

/// A chain-1 Governance `intent` in `context`, estimated over `client` and authorized for
/// submission by the vault's account as `step`.
async fn authorized_governance_request(
    context: crate::GovernanceActionContext,
    intent: &crate::GovernanceActionIntent,
    step: PublicActionProgressStep,
    chain: &crate::settings::EffectiveChainConfig,
    http: &HttpContext,
    client: &OperationHttpClient,
    vault: (
        &Arc<crate::vault::DesktopVaultStore>,
        &Arc<crate::vault::DesktopViewSession>,
    ),
) -> crate::GovernanceSubmissionRequest {
    let quote = PublicActionGasFeeQuote {
        observed_fee_model: Some(crate::EvmFeeModel::Eip1559),
        rpc_gas_price: 100,
        current_base_fee_per_gas: Some(5),
        suggested_max_fee_per_gas: 100,
        suggested_max_priority_fee_per_gas: 2,
    };
    let fee = resolve_public_action_gas_fee(
        1,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 2,
        },
        Some(quote),
    )
    .unwrap();
    let rebuilt = intent.resolve(&context).unwrap();
    let review = crate::GovernanceActionReview::from_resolved(&rebuilt, context, None).unwrap();
    let estimate = crate::estimate_governance_action(
        &review,
        rebuilt.clone(),
        chain.clone(),
        quote,
        fee,
        http,
        client,
    )
    .await
    .expect("the action is estimated")
    .estimate;
    crate::GovernanceSubmissionRequest {
        public_send: crate::PublicSendRequest {
            executor_owner: None,
            transaction_tracking: None,
            chain_id: 1,
            effective_chain: chain.clone(),
            view_session: Arc::clone(vault.1),
            vault_store: Arc::clone(vault.0),
            authorization: Some(crate::DesktopPrivateSpendAuthorization::VaultPassword(
                zeroize::Zeroizing::new(crate::public_wallet::tests::TEST_PASSWORD.into()),
            )),
            trezor_app_passphrase: None,
            trezor_pin_matrix_provider: None,
            public_account_uuid: review.context.public_account_uuid.clone(),
            intent: rebuilt.raw.clone(),
            advanced_authorization: Some(crate::PublicAdvancedTransactionAuthorization {
                payload_fingerprint: estimate.payload_fingerprint,
                gas_limit: estimate.gas_limit,
            }),
            gateway_origin: None,
            gas_fee: PublicActionGasFeeSelection::Custom {
                max_fee_per_gas: fee.max_fee_per_gas,
                max_priority_fee_per_gas: fee.max_priority_fee_per_gas,
            },
            command_rx: None,
            event_tx: None,
        },
        review,
        rebuilt,
        estimate,
        progress_step: step,
    }
}

#[tokio::test(start_paused = true)]
async fn governance_action_and_workflow_use_only_the_action_client() {
    use crate::public_wallet::tests::{
        TEST_IMPORTED_PRIVATE_KEY, TEST_PASSWORD, public_action_request_parts,
    };
    use crate::rpc_broker::tests::{RpcResponder, spawn_recording_rpc_mock, tagged_reqwest_client};
    use crate::vault::{
        ActivityAsset, ActivityDirection, ActivityJournal, ActivityLeg, ActivityOwner,
    };
    use crate::{GovernanceActionContext, GovernanceActionIntent, GovernanceContractKind};

    // Each broadcast is mined alone in the next block, so block `n + 2` holds the n-th.
    let responder: RpcResponder = {
        let mined = Arc::new(Mutex::new(Vec::<FixedBytes<32>>::new()));
        Arc::new(move |request: serde_json::Value| {
            let hashes = mined.lock().unwrap().clone();
            let mined_in = |number: u64| {
                hashes
                    .get(usize::try_from(number.checked_sub(2)?).ok()?)
                    .copied()
            };
            let block_number = request["params"][0]
                .as_str()
                .and_then(|param| u64::from_str_radix(param.strip_prefix("0x")?, 16).ok());
            let result = match (request["method"].as_str(), block_number) {
                (Some("eth_blockNumber"), _) => json!(format!("0x{:x}", hashes.len() + 1)),
                // Covers every Governance token balance and allowance the workflow reads.
                (Some("eth_call"), _) => json!(format!("0x{:064x}", 1_u128 << 80)),
                (Some("eth_getBlockByNumber"), Some(number)) => {
                    let mut block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
                    block.header.hash = FixedBytes::repeat_byte(number as u8);
                    block.header.inner.number = number;
                    block.header.inner.base_fee_per_gas = Some(5);
                    block.transactions = alloy::rpc::types::BlockTransactions::Hashes(
                        mined_in(number).into_iter().collect(),
                    );
                    serde_json::to_value(block).unwrap()
                }
                (Some("eth_getBlockReceipts"), _) => {
                    let block_hash: FixedBytes<32> =
                        serde_json::from_value(request["params"][0].clone()).unwrap();
                    let number = u64::from(block_hash[0]);
                    json!([{
                        "transactionHash": mined_in(number).unwrap(),
                        "blockHash": block_hash,
                        "blockNumber": format!("0x{number:x}"),
                        "transactionIndex": "0x0",
                        "from": Address::ZERO,
                        "to": Address::ZERO,
                        "cumulativeGasUsed": "0x5208",
                        "status": "0x1",
                        "gasUsed": "0x5208",
                        "effectiveGasPrice": "0x1",
                        "logs": [],
                        "logsBloom": format!("0x{}", "00".repeat(256)),
                        "type": "0x0",
                        "contractAddress": null,
                    }])
                }
                _ => session_result(21_000, &request, &|hash| {
                    mined.lock().unwrap().push(hash);
                }),
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        })
    };
    let (general_only, general_requests, general_server) =
        spawn_recording_rpc_mock(responder.clone()).await;
    let (action_endpoint, action_requests, action_server) =
        spawn_recording_rpc_mock(responder).await;
    let roles = |general, sensitive, submit| crate::RpcEndpointRoles {
        general,
        sensitive,
        submit,
    };
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    chain.rpc_route = RpcChainRoute::new_with_transports(
        1,
        vec![
            (
                general_only.into(),
                crate::RpcTransport::Wallet,
                roles(true, false, false),
            ),
            (
                action_endpoint.into(),
                crate::RpcTransport::Wallet,
                roles(false, true, true),
            ),
        ],
    );
    let http = HttpContext::with_rpc_client_for_tests(
        tagged_reqwest_client("shared"),
        crate::WalletNetworkMode::Direct,
    );
    let client = OperationHttpClient::for_tests(
        tagged_reqwest_client("action"),
        crate::OperationNetworkIsolation::Dedicated,
    );
    let (root_dir, db, store, view_session) = public_action_request_parts();
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            TEST_IMPORTED_PRIVATE_KEY,
            Some("Governance actor"),
            false,
        )
        .expect("import public account");
    let contracts = railgun_ui::governance_contracts(1).unwrap();
    let context = |contract, contract_kind, observed_state| GovernanceActionContext {
        private_wallet_uuid: "public-action-wallet".into(),
        chain_id: 1,
        public_account_uuid: account.public_account_uuid.clone(),
        actor: account.address,
        contract,
        contract_kind,
        observed_state,
    };

    let sponsor = authorized_governance_request(
        context(
            contracts.voting,
            GovernanceContractKind::Voting,
            FixedBytes::ZERO,
        ),
        &GovernanceActionIntent::Sponsor {
            proposal_version: crate::GovernanceContractVersion::V2,
            proposal_index: U256::from(7),
            amount: U256::from(3),
            snapshot_interval: U256::from(9),
            snapshot_hint: U256::from(2),
        },
        PublicActionProgressStep::Sponsor,
        &chain,
        &http,
        &client,
        (&store, &view_session),
    )
    .await;
    let sponsored = tokio::time::timeout(
        Duration::from_secs(30),
        Box::pin(crate::submit_governance_action_with_progress(
            sponsor,
            &http,
            &client,
            |_| {},
        )),
    )
    .await
    .expect("the sponsor action ends")
    .expect("the sponsor action is confirmed");

    // An approval-required stake: the approval, then the stake rebuilt from fresh reads.
    let plan = crate::plan_stake(
        account.address,
        contracts.staking,
        U256::from(10),
        U256::ZERO,
        U256::from(5),
    )
    .unwrap();
    let approval = authorized_governance_request(
        context(
            contracts.governance_token,
            GovernanceContractKind::GovernanceToken,
            plan.observed_state,
        ),
        plan.approval.as_ref().unwrap(),
        PublicActionProgressStep::GovernanceApprove,
        &chain,
        &http,
        &client,
        (&store, &view_session),
    )
    .await;
    let workflow = tokio::time::timeout(
        Duration::from_secs(30),
        Box::pin(crate::submit_governance_workflow_with_progress(
            crate::GovernanceWorkflowRequest {
                initial: approval,
                workflow: crate::GovernanceWorkflow::StakeApproval(plan),
            },
            &http,
            &client,
            |_| {},
        )),
    )
    .await
    .expect("the workflow ends")
    .expect("the approval and its continued stake are confirmed");

    let (token0, token1, token2) = (
        Address::repeat_byte(1),
        Address::repeat_byte(2),
        Address::repeat_byte(3),
    );
    let mut direct = Vec::new();
    for (contract, kind, intent, step) in [
        (
            contracts.staking,
            GovernanceContractKind::Staking,
            GovernanceActionIntent::Stake {
                amount: U256::from(4),
            },
            PublicActionProgressStep::Stake,
        ),
        (
            contracts.governor_rewards,
            GovernanceContractKind::GovernorRewards,
            GovernanceActionIntent::RewardClaim {
                reward_tokens: vec![token0, token1, token2],
                starting_interval: U256::ZERO,
                ending_interval: U256::from(1),
                snapshot_hints: vec![U256::ZERO; 2],
                expected_amounts: vec![U256::from(6), U256::ZERO, U256::from(8)],
            },
            PublicActionProgressStep::RewardClaim(0),
        ),
        (
            contracts.staking,
            GovernanceContractKind::Staking,
            GovernanceActionIntent::PrincipalClaim {
                stake_id: U256::from(2),
                amount: U256::from(7),
            },
            PublicActionProgressStep::PrincipalClaim,
        ),
    ] {
        let request = authorized_governance_request(
            context(contract, kind, FixedBytes::ZERO),
            &intent,
            step,
            &chain,
            &http,
            &client,
            (&store, &view_session),
        )
        .await;
        direct.push(
            tokio::time::timeout(
                Duration::from_secs(30),
                Box::pin(crate::submit_governance_action_with_progress(
                    request,
                    &http,
                    &client,
                    |_| {},
                )),
            )
            .await
            .expect("the action ends")
            .expect("the action is confirmed"),
        );
    }

    // Each submission is one journal entry carrying the tokens it moves.
    let entries = ActivityJournal::new(Arc::clone(&db), Arc::clone(&view_session))
        .unwrap()
        .list(&ActivityOwner::Account(account.public_account_uuid.clone()))
        .unwrap();
    assert_eq!(entries.len(), 5);
    let legs_of = |results: &[&crate::PublicSendResult]| {
        let hashes: Vec<FixedBytes<32>> = results
            .iter()
            .map(|result| result.tx.tx_hash.parse().unwrap())
            .collect();
        let entry = entries
            .iter()
            .find(|entry| entry.attempts.iter().any(|a| a.hash == hashes[0]))
            .expect("the submission is journaled");
        assert!(
            hashes
                .iter()
                .all(|hash| entry.attempts.iter().any(|a| a.hash == *hash)),
            "every transaction of the submission is in its entry"
        );
        entry.legs.clone()
    };
    let leg = |asset, amount: u64, direction| ActivityLeg {
        asset,
        amount: U256::from(amount),
        direction,
    };
    let rail = ActivityAsset::Token(contracts.governance_token);
    assert!(
        legs_of(&[&sponsored]).is_empty(),
        "voting weight moves no tokens"
    );
    assert_eq!(
        legs_of(&[&direct[0]]),
        [leg(rail, 4, ActivityDirection::Out)]
    );
    assert_eq!(
        legs_of(&workflow.transactions.iter().collect::<Vec<_>>()),
        [leg(rail, 5, ActivityDirection::Out)]
    );
    assert_eq!(
        legs_of(&[&direct[1]]),
        [
            leg(ActivityAsset::Token(token0), 6, ActivityDirection::In),
            leg(ActivityAsset::Token(token2), 8, ActivityDirection::In),
        ]
    );
    assert_eq!(
        legs_of(&[&direct[2]]),
        [leg(rail, 7, ActivityDirection::In)]
    );

    assert!(
        general_requests.lock().unwrap().is_empty(),
        "nothing reaches the General-only endpoint"
    );
    let action_requests = action_requests.lock().unwrap().clone();
    assert!(
        action_requests
            .iter()
            .all(|(tag, _)| tag.as_deref() == Some("action")),
        "every request goes over the action's client"
    );
    let count = |method: &str| {
        action_requests
            .iter()
            .filter(|(_, body)| body["method"] == method)
            .count()
    };
    // Estimates, the continuation's fresh reads, and each transaction's broadcast and receipt.
    assert!(count("eth_estimateGas") > 0 && count("eth_call") > 0);
    assert_eq!(count("eth_sendRawTransaction"), 6);
    assert_eq!(count("eth_getBlockReceipts"), 6);
    general_server.abort();
    action_server.abort();
    drop(store);
    drop(db);
    std::fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
