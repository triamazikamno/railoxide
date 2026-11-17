use super::*;
use alloy::eips::BlockNumHash;
use alloy::network::primitives::BlockTransactions;
use alloy::primitives::{B256, U256};

#[tokio::test]
async fn account_read_returns_code_and_nonce_at_a_confirmed_block_only() {
    use alloy::providers::{ProviderBuilder, mock::Asserter};
    let chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let address = Address::repeat_byte(1);
    // An account on the earlier listed delegate is read like one on the primary.
    let delegate = broadcaster_core::deployment::RailgunDeployment::for_chain(1)
        .unwrap()
        .relay_adapt_7702_delegates[0]
        .address;
    assert_ne!(
        delegate,
        chain.accepted_executor_profile().unwrap().delegate()
    );
    let designator = alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
    let code = Bytes::from([&designator[..], delegate.as_slice()].concat());
    let confirmed_tip = 50;
    // `None` reads the confirmed tip, an earlier block is read as requested, and
    // a block past the confirmed tip is refused.
    for requested in [None, Some(40), Some(confirmed_tip + 1)] {
        let responses = Asserter::new();
        let provider = ProviderBuilder::new()
            .connect_mocked_client(responses.clone())
            .erased();
        let number = requested.unwrap_or(confirmed_tip);
        let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
        block.header.inner.number = number;
        block.header.hash = B256::repeat_byte(9);
        block.transactions = BlockTransactions::Full(Vec::new());
        responses.push_success(&format!("0x{:x}", confirmed_tip + chain.finality_depth));
        responses.push_success(&block);
        responses.push_success(&code);
        responses.push_success(&Bytes::from(U256::from(7).to_be_bytes::<32>()));
        let result = read_account_at_provider(&provider, &chain, address, requested).await;
        if number > confirmed_tip {
            assert!(result.is_err());
            continue;
        }
        let read = result.unwrap();
        let at = BlockNumHash::new(number, B256::repeat_byte(9));
        assert_eq!(read.block, at);
        assert_eq!(read.code, code);
        assert_eq!(
            read.nonce,
            Some(ExecutorNonceObservation::new(at, U256::from(7)))
        );
    }
}

#[tokio::test]
async fn a_failed_endpoint_sits_out_and_a_verified_one_is_not_rechecked() {
    use serde_json::{Value, json};
    /// New endpoints, as each executor owner read observes through, once both are admitted.
    async fn admitted_endpoints(
        chain: &EffectiveChainConfig,
        http: &HttpContext,
        client: &OperationHttpClient,
    ) -> ObservationEndpoints {
        let endpoints =
            ObservationEndpoints::new(chain, http, crate::RpcTrafficClass::Sensitive, client)
                .unwrap();
        endpoints.providers().await;
        // Admission returns at the first verified endpoint and admits the other after.
        tokio::time::timeout(Duration::from_secs(5), async {
            while endpoints
                .current_pool()
                .unwrap()
                .available_providers()
                .len()
                < 2
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        endpoints
    }
    async fn read(endpoints: &ObservationEndpoints, chain: &EffectiveChainConfig) {
        let read = read_executor_account(endpoints, chain, Address::repeat_byte(1), None)
            .await
            .unwrap();
        assert_eq!(read.block, BlockNumHash::new(50, B256::repeat_byte(50)));
    }
    let mut chain =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap()
            .get(1)
            .cloned()
            .unwrap();
    let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
    block.header.inner.number = 50;
    block.header.hash = B256::repeat_byte(50);
    let head = format!("0x{:x}", 50 + chain.finality_depth);
    let logged = |methods: &Arc<Mutex<Vec<String>>>, request: &Value| {
        let method = request["method"].as_str().unwrap().to_owned();
        methods.lock().unwrap().push(method.clone());
        method
    };
    let failing_methods = Arc::new(Mutex::new(Vec::new()));
    let recorded = failing_methods.clone();
    let (failing, failing_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            if logged(&recorded, &request) == "eth_chainId" {
                json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"})
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32005, "message": "limit exceeded"}
                })
            }
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let serving_methods = Arc::new(Mutex::new(Vec::new()));
    let recorded = serving_methods.clone();
    let (serving, serving_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            let result = match logged(&recorded, &request).as_str() {
                "eth_chainId" => json!("0x1"),
                "eth_blockNumber" => json!(head),
                "eth_getBlockByNumber" => serde_json::to_value(&block).unwrap(),
                "eth_getCode" => json!("0x"),
                "eth_getStorageAt" => json!(B256::ZERO),
                method => panic!("unexpected RPC method {method}"),
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    chain.rpc_route =
        crate::RpcChainRoute::new(1, vec![failing, serving]).with_identity_verification();
    let client = OperationHttpClient::for_tests(
        reqwest::Client::new(),
        crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
    );
    // The broker breaks candidate ties in configured order, so the failing endpoint is
    // tried first while both are healthy.
    let http = HttpContext::direct_for_tests();
    read(&admitted_endpoints(&chain, &http, &client).await, &chain).await;

    let endpoints = admitted_endpoints(&chain, &http, &client).await;
    let serving = &endpoints.route.endpoints()[1..];
    // The strike reaches the broker on its advisory channel.
    tokio::time::timeout(Duration::from_secs(5), async {
        while http
            .rpc_broker()
            .endpoint_candidates(endpoints.route.route(), None)
            .await
            .unwrap()
            != serving
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Both endpoints are admitted, yet only the healthy one is offered, now or later.
    let indices = |providers: Vec<ProviderHandle>| {
        providers
            .into_iter()
            .map(|provider| provider.index)
            .collect::<Vec<_>>()
    };
    assert_eq!(indices(endpoints.providers().await), vec![1]);
    assert_eq!(
        indices(
            endpoints
                .later_providers(&BTreeSet::new(), tokio::time::Instant::now())
                .await
        ),
        vec![1]
    );
    read(&endpoints, &chain).await;
    // The first read tried the failing endpoint once. The second, on new endpoints,
    // skipped it during its withdrawal.
    assert_eq!(
        *failing_methods.lock().unwrap(),
        vec!["eth_chainId", "eth_blockNumber"]
    );
    let served = serving_methods.lock().unwrap().clone();
    let count = |method: &str| served.iter().filter(|call| *call == method).count();
    assert_eq!((count("eth_chainId"), count("eth_blockNumber")), (1, 2));

    failing_server.abort();
    serving_server.abort();
}
