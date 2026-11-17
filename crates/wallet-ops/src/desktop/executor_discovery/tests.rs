use super::*;
use crate::settings::{WalletSettings, build_effective_chain_configs};
use alloy::primitives::B256;
use alloy::providers::ProviderBuilder;
use alloy::transports::mock::Asserter;
use std::time::Duration;

#[tokio::test]
async fn executor_inspection_uses_the_context_proxy_and_checks_chain_before_address_reads() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let proxy = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let rpc_client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&proxy_url).unwrap())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let client = OperationHttpClient::for_tests(
        rpc_client.clone(),
        crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Proxy),
    );
    let http = HttpContext::with_rpc_client_for_tests(rpc_client, crate::WalletNetworkMode::Proxy);
    let proxy_request = tokio::spawn(async move {
        let (socket, _) = proxy.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut first = String::new();
        socket.read_line(&mut first).await.unwrap();
        let mut length = None;
        loop {
            let mut header = String::new();
            assert_ne!(socket.read_line(&mut header).await.unwrap(), 0);
            if header == "\r\n" {
                break;
            }
            if let Some((key, value)) = header.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; length.unwrap()];
        socket.read_exact(&mut body).await.unwrap();
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let response =
            serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":"0x38"}).to_string();
        socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        (first, request)
    });
    let mut chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
    chain.rpc_route = crate::RpcChainRoute::new(
        1,
        vec![
            "http://executor-rpc.invalid/private-route"
                .parse::<url::Url>()
                .unwrap(),
        ],
    );
    let inspected = tokio::time::timeout(
        Duration::from_secs(3),
        inspect_executor(
            &chain,
            &http,
            &client,
            Address::repeat_byte(1),
            &[ExecutorAsset::Native],
        ),
    )
    .await
    .unwrap();
    assert!(inspected.is_err());
    let (first, request) = tokio::time::timeout(Duration::from_secs(3), proxy_request)
        .await
        .unwrap()
        .unwrap();
    assert!(first.starts_with("POST http://executor-rpc.invalid/private-route "));
    assert_eq!(request["method"], "eth_chainId");
    assert!(
        request
            .get("params")
            .is_none_or(|params| params.as_array().is_some_and(Vec::is_empty))
    );
}

#[tokio::test]
async fn executor_inspection_preserves_partial_results_without_treating_failed_reads_as_empty() {
    let chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
    let address = Address::repeat_byte(1);
    let token = ExecutorAsset::Erc20(Address::repeat_byte(2));
    let nft = ExecutorAsset::Erc721 {
        collection: Address::repeat_byte(3),
        token_id: U256::from(4),
    };
    let block = BlockNumHash::new(10, B256::repeat_byte(10));
    let responses = Asserter::new();
    let provider = ProviderBuilder::new()
        .connect_mocked_client(responses.clone())
        .erased();
    responses.push_success(&"0x0");
    responses.push_success(&Bytes::new());
    responses.push_success(&"0x0");
    responses.push_failure_msg("token read unavailable");
    responses.push_failure_msg("nonce read unavailable");
    let partial = inspect_at(&provider, &chain, address, &[token], block, false).await;
    assert_eq!(partial.activity(), ExecutorActivity::Unknown);
    assert!(partial.has_incomplete_reads());
    assert_eq!(partial.balances()[&ExecutorAsset::Native], Some(U256::ZERO));
    assert_eq!(partial.balances()[&token], None);
    assert_eq!(partial.execution_nonce(), None);

    responses.push_success(&"0x0");
    responses.push_success(&Bytes::new());
    responses.push_success(&"0x0");
    responses.push_failure_msg("token read unavailable");
    responses.push_success(&Bytes::from(
        ExecutorErc721::ownerOfCall::abi_encode_returns(&address),
    ));
    responses.push_success(&"0x0");
    let with_nft = inspect_at(&provider, &chain, address, &[token, nft], block, false).await;
    assert_eq!(with_nft.activity(), ExecutorActivity::PreviouslyUsed);
    assert!(with_nft.has_incomplete_reads());
    assert_eq!(with_nft.balances()[&nft], Some(U256::from(1)));
}

#[tokio::test]
async fn a_failed_inspection_read_withdraws_its_endpoint_and_keeps_the_partial_result() {
    use serde_json::{Value, json};
    use std::sync::Arc;

    let mut chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
    let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
    block.header.inner.number = 50;
    block.header.hash = B256::repeat_byte(50);
    // Passes the chain and head checks, then rate-limits the balance read.
    let (failing, failing_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            let result = match request["method"].as_str().unwrap() {
                "eth_chainId" => json!("0x1"),
                "eth_getBlockByNumber" => serde_json::to_value(&block).unwrap(),
                "eth_getTransactionCount" => json!("0x0"),
                "eth_getCode" => json!("0x"),
                "eth_getStorageAt" => json!(B256::ZERO),
                _ => {
                    return json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "error": {"code": -32005, "message": "limit exceeded"}
                    });
                }
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    // Another chain's endpoint, if tried first, fails the inspection's own chain check,
    // which is neutral and leaves it eligible.
    let (other, other_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(|request: Value| json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x2"})),
        Arc::default(),
        Arc::default(),
    )
    .await;
    chain.rpc_route = crate::RpcChainRoute::new(1, vec![failing, other]);
    let client = OperationHttpClient::for_tests(
        reqwest::Client::new(),
        crate::OperationNetworkIsolation::Unavailable(crate::WalletNetworkMode::Direct),
    );
    let http = HttpContext::direct_for_tests();
    let inspection = inspect_executor(
        &chain,
        &http,
        &client,
        Address::repeat_byte(1),
        &[ExecutorAsset::Native],
    )
    .await
    .unwrap();
    assert_eq!(inspection.account_nonce(), Some(0));
    assert_eq!(inspection.balances()[&ExecutorAsset::Native], None);

    // The strike reaches the broker on its advisory channel.
    let route = chain
        .rpc_route
        .for_class(crate::RpcTrafficClass::Sensitive)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while http
            .rpc_broker()
            .endpoint_candidates(route.route(), None)
            .await
            .unwrap()
            != route.endpoints()[1..]
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    failing_server.abort();
    other_server.abort();
}

#[tokio::test]
async fn executor_inspection_reads_execution_nonce_only_for_the_accepted_delegation() {
    let mut chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
    let profile = chain.accepted_executor_profile().unwrap();
    let responses = Asserter::new();
    let provider = ProviderBuilder::new()
        .connect_mocked_client(responses.clone())
        .erased();
    let address = Address::repeat_byte(1);
    let block = BlockNumHash::new(10, B256::repeat_byte(10));
    // An account on the earlier listed delegate keeps its nonce readable.
    let earlier = broadcaster_core::deployment::RailgunDeployment::for_chain(1)
        .unwrap()
        .relay_adapt_7702_delegates[0]
        .address;
    for delegate in [profile.delegate(), earlier, Address::repeat_byte(2)] {
        let listed = delegate != Address::repeat_byte(2);
        responses.push_success(&"0x2");
        responses.push_success(&Bytes::from(
            [
                EIP7702_DELEGATION_DESIGNATOR.as_slice(),
                delegate.as_slice(),
            ]
            .concat(),
        ));
        responses.push_success(&"0x0");
        if listed {
            responses.push_success(&Bytes::from(RelayAdapt7702::nonceCall::abi_encode_returns(
                &U256::from(8),
            )));
        }
        let inspection = inspect_at(&provider, &chain, address, &[], block, false).await;
        assert_eq!(inspection.activity(), ExecutorActivity::PreviouslyUsed);
        assert_eq!(inspection.account_nonce(), Some(2));
        assert_eq!(
            inspection.execution_nonce(),
            listed.then_some(U256::from(8))
        );
    }
    // A historical executor can retain a consumed nonce with no delegation.
    // Disabling new execution must preserve this recovery observation.
    chain.enabled = false;
    responses.push_success(&"0x5");
    responses.push_success(&Bytes::new());
    responses.push_success(&"0x0");
    responses.push_success(&"0x8");
    let revoked = inspect_at(&provider, &chain, address, &[], block, false).await;
    assert_eq!(revoked.execution_nonce(), Some(U256::from(8)));
    assert!(!revoked.has_incomplete_reads());

    // Nonce-only consumers must retain the stored nonce after revocation,
    // without fetching the unrelated account nonce or balances again.
    responses.push_success(&Bytes::new());
    responses.push_success(&"0x8");
    assert_eq!(
        execution_nonce_at(&provider, &chain, address, block).await,
        Some(U256::from(8)),
    );
}
#[tokio::test]
async fn recovery_reads_target_nonce_without_calling_an_unrecognized_delegate() {
    let chain = build_effective_chain_configs(&WalletSettings::default())
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
    let responses = Asserter::new();
    let provider = ProviderBuilder::new()
        .connect_mocked_client(responses.clone())
        .erased();
    let source = Address::repeat_byte(1);
    let other = Address::repeat_byte(2);
    let designator =
        Bytes::from([EIP7702_DELEGATION_DESIGNATOR.as_slice(), other.as_slice()].concat());
    let block = BlockNumHash::new(10, B256::repeat_byte(10));
    // Recovery must use retained storage, not an arbitrary delegate's nonce() ABI.
    responses.push_success(&"0x2");
    responses.push_success(&designator);
    responses.push_success(&"0x10");
    responses.push_success(&"0x8");
    let recovery = inspect_at(&provider, &chain, source, &[], block, true).await;
    assert_eq!(recovery.execution_nonce(), Some(U256::from(8)));
    assert_eq!(executor_delegation(recovery.code().unwrap()), Some(other));

    // Arbitrary runtime code cannot be authorized away as an EIP-7702 delegation.
    responses.push_success(&"0x2");
    responses.push_success(&Bytes::from_static(&[0x60, 0x00]));
    responses.push_success(&"0x10");
    let contract = inspect_at(&provider, &chain, source, &[], block, true).await;
    assert_eq!(contract.execution_nonce(), None);
}
