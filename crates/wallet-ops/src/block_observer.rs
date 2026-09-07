use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use alloy::consensus::BlockHeader as _;
use alloy::network::primitives::{BlockTransactions, HeaderResponse as _};
use alloy::network::{BlockResponse as _, ReceiptResponse as _, TransactionResponse as _};
use alloy::primitives::{Address, B256, FixedBytes};
use alloy::providers::Provider as _;
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool};
use eyre::{Result, eyre};

use crate::{RpcBroker, TxReceiptOutput};

const MAX_BLOCKS_PER_POLL: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockIdentity {
    number: u64,
    hash: B256,
    parent_hash: B256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockObservation {
    pub(crate) receipt: Option<(usize, TxReceiptOutput)>,
    pub(crate) head: Option<(usize, u64)>,
}

#[derive(Debug)]
enum ProviderPollFailure {
    Lagging,
    UnsupportedReceipts,
    Failed,
}

#[derive(Debug)]
enum BlockFetch {
    Missing,
    Invalid,
    Valid(BlockIdentity, Vec<B256>),
}

#[derive(Debug)]
enum ReceiptFetch {
    Unsupported,
    Failed,
    Invalid,
    Valid(TxReceiptOutput),
}

pub(crate) struct BlockObserver {
    query_rpc_pool: Arc<QueryRpcPool>,
    finality_depth: u64,
    next_block: u64,
    registered: HashMap<B256, usize>,
    history: VecDeque<BlockIdentity>,
    unsupported_receipts: HashSet<usize>,
    active_provider: Option<usize>,
    rpc_broker: Arc<RpcBroker>,
    chain_id: u64,
}

impl BlockObserver {
    pub(crate) async fn establish(
        query_rpc_pool: Arc<QueryRpcPool>,
        finality_depth: u64,
        rpc_broker: Arc<RpcBroker>,
        chain_id: u64,
    ) -> Result<Self> {
        let mut observer = Self {
            query_rpc_pool,
            finality_depth,
            next_block: 0,
            registered: HashMap::new(),
            history: VecDeque::new(),
            unsupported_receipts: HashSet::new(),
            active_provider: None,
            rpc_broker,
            chain_id,
        };
        observer.establish_baseline().await?;
        Ok(observer)
    }

    pub(crate) fn register(&mut self, tx_hash: FixedBytes<32>, attempt_id: usize) {
        self.registered.insert(tx_hash, attempt_id);
    }

    pub(crate) async fn poll(&mut self) -> Result<BlockObservation> {
        if self.registered.is_empty() {
            return Ok(BlockObservation {
                receipt: None,
                head: None,
            });
        }

        let mut providers = self.query_rpc_pool.available_providers();
        if let Some(active_provider) = self.active_provider {
            providers.sort_by_key(|provider| usize::from(provider.index != active_provider));
        }
        if providers.is_empty() {
            return Err(eyre!("privacy-preserving block observation is unavailable"));
        }

        let mut lagging = false;
        let mut saw_unsupported_receipts = false;
        for provider in providers {
            match self.poll_provider(&provider).await {
                Ok(observation) => {
                    self.active_provider = Some(provider.index);
                    // Inclusion invalidates rather than notifies: an equal head does not evict,
                    // and this endpoint may lead the one that served a cached latest read.
                    if observation.receipt.is_some() {
                        self.rpc_broker.invalidate_block(self.chain_id);
                    } else if let Some((_, head)) = observation.head {
                        self.rpc_broker.notify_block(self.chain_id, head);
                    }
                    return Ok(observation);
                }
                Err(ProviderPollFailure::Lagging) => lagging = true,
                Err(ProviderPollFailure::UnsupportedReceipts) => {
                    saw_unsupported_receipts = true;
                    self.unsupported_receipts.insert(provider.index);
                }
                Err(ProviderPollFailure::Failed) => {
                    self.query_rpc_pool.mark_bad_provider(&provider);
                    let rpc = crate::http::redact_url_for_display(&provider.url);
                    tracing::warn!(%rpc, "block-scoped query RPC observation failed");
                }
            }
        }

        if lagging {
            return Ok(BlockObservation {
                receipt: None,
                head: None,
            });
        }
        if saw_unsupported_receipts {
            return Err(eyre!(
                "privacy-preserving block receipt observation is unavailable"
            ));
        }
        Err(eyre!("privacy-preserving block observation is unavailable"))
    }

    async fn establish_baseline(&mut self) -> Result<()> {
        let providers = self.query_rpc_pool.available_providers();
        if providers.is_empty() {
            return Err(eyre!("privacy-preserving block observation is unavailable"));
        }
        for provider in providers {
            let Ok(head) = provider.provider.get_block_number().await else {
                self.query_rpc_pool.mark_bad_provider(&provider);
                let rpc = crate::http::redact_url_for_display(&provider.url);
                tracing::warn!(%rpc, "block observation baseline failed");
                continue;
            };
            self.next_block = head.saturating_add(1);
            self.active_provider = Some(provider.index);
            return Ok(());
        }
        Err(eyre!(
            "privacy-preserving block observation baseline is unavailable"
        ))
    }

    async fn poll_provider(
        &mut self,
        provider: &ProviderHandle,
    ) -> std::result::Result<BlockObservation, ProviderPollFailure> {
        let head = provider
            .provider
            .get_block_number()
            .await
            .map_err(|_| ProviderPollFailure::Failed)?;

        if let Some(tip) = self.history.back().copied() {
            match fetch_block(provider, tip.number).await {
                BlockFetch::Missing if head < tip.number => {
                    return Err(ProviderPollFailure::Lagging);
                }
                BlockFetch::Valid(identity, _) if identity == tip => {}
                BlockFetch::Valid(_, _) => {
                    self.rollback_to_common_ancestor(provider).await?;
                }
                BlockFetch::Missing | BlockFetch::Invalid => {
                    return Err(ProviderPollFailure::Failed);
                }
            }
        }

        if head < self.next_block {
            return Err(ProviderPollFailure::Lagging);
        }

        let end = head.min(
            self.next_block
                .saturating_add(MAX_BLOCKS_PER_POLL.saturating_sub(1)),
        );
        for number in self.next_block..=end {
            let BlockFetch::Valid(identity, transactions) = fetch_block(provider, number).await
            else {
                return Err(ProviderPollFailure::Failed);
            };
            if identity.number != number {
                return Err(ProviderPollFailure::Failed);
            }
            if self
                .history
                .back()
                .is_some_and(|previous| identity.parent_hash != previous.hash)
            {
                self.rollback_to_common_ancestor(provider).await?;
                return Ok(BlockObservation {
                    receipt: None,
                    head: self.processed_head(provider.index),
                });
            }

            let matching_hashes = transactions
                .iter()
                .copied()
                .filter(|hash| self.registered.contains_key(hash))
                .collect::<Vec<_>>();
            if matching_hashes.len() > 1 {
                return Err(ProviderPollFailure::Failed);
            }
            if let Some(tx_hash) = matching_hashes.first().copied() {
                if self.unsupported_receipts.contains(&provider.index) {
                    return Err(ProviderPollFailure::UnsupportedReceipts);
                }
                return match fetch_receipt(provider, identity, &transactions, tx_hash).await {
                    ReceiptFetch::Valid(receipt) => {
                        self.record(identity);
                        self.next_block = number.saturating_add(1);
                        Ok(BlockObservation {
                            receipt: Some((self.registered[&tx_hash], receipt)),
                            head: self.processed_head(provider.index),
                        })
                    }
                    ReceiptFetch::Unsupported => Err(ProviderPollFailure::UnsupportedReceipts),
                    ReceiptFetch::Failed | ReceiptFetch::Invalid => {
                        Err(ProviderPollFailure::Failed)
                    }
                };
            }

            self.record(identity);
            self.next_block = number.saturating_add(1);
        }

        Ok(BlockObservation {
            receipt: None,
            head: self.processed_head(provider.index),
        })
    }

    async fn rollback_to_common_ancestor(
        &mut self,
        provider: &ProviderHandle,
    ) -> std::result::Result<(), ProviderPollFailure> {
        if self.history.len() < 2 {
            return Err(ProviderPollFailure::Failed);
        }
        for index in (0..self.history.len() - 1).rev() {
            let expected = self.history[index];
            match fetch_block(provider, expected.number).await {
                BlockFetch::Valid(identity, _) if identity == expected => {
                    self.history.truncate(index + 1);
                    self.next_block = expected.number.saturating_add(1);
                    return Ok(());
                }
                BlockFetch::Missing | BlockFetch::Invalid => {
                    return Err(ProviderPollFailure::Failed);
                }
                BlockFetch::Valid(_, _) => {}
            }
        }
        Err(ProviderPollFailure::Failed)
    }

    fn record(&mut self, identity: BlockIdentity) {
        self.history.push_back(identity);
        let capacity = usize::try_from(self.finality_depth)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
            .max(1);
        while self.history.len() > capacity {
            self.history.pop_front();
        }
    }

    fn processed_head(&self, provider_index: usize) -> Option<(usize, u64)> {
        self.history
            .back()
            .map(|identity| (provider_index, identity.number))
    }
}

pub(crate) async fn resolve_transaction_sender_by_block(
    query_rpc_pool: &QueryRpcPool,
    block_number: u64,
    tx_hash: FixedBytes<32>,
) -> Result<Address> {
    let providers = query_rpc_pool.available_providers();
    if providers.is_empty() {
        return Err(eyre!(
            "privacy-preserving source transaction resolution is unavailable"
        ));
    }
    for provider in providers {
        let Ok(Some(sender)) = fetch_full_block_sender(&provider, block_number, tx_hash).await
        else {
            query_rpc_pool.mark_bad_provider(&provider);
            let rpc = crate::http::redact_url_for_display(&provider.url);
            tracing::warn!(%rpc, "source block transaction resolution failed");
            continue;
        };
        return Ok(sender);
    }
    Err(eyre!(
        "privacy-preserving source transaction resolution is unavailable"
    ))
}

async fn fetch_full_block_sender(
    provider: &ProviderHandle,
    block_number: u64,
    tx_hash: FixedBytes<32>,
) -> std::result::Result<Option<Address>, ()> {
    let Some(block) = provider
        .provider
        .get_block_by_number(block_number.into())
        .full()
        .await
        .map_err(|_| ())?
    else {
        return Ok(None);
    };
    if block.header().number() != block_number {
        return Ok(None);
    }

    let mut sender = None;
    let mut matches = 0;
    let BlockTransactions::Full(transactions) = block.transactions() else {
        return Ok(None);
    };
    for transaction in transactions {
        if transaction.tx_hash() == tx_hash {
            matches += 1;
            sender = Some(transaction.from());
        }
    }
    if matches == 1 { Ok(sender) } else { Ok(None) }
}

async fn fetch_block(provider: &ProviderHandle, number: u64) -> BlockFetch {
    let block = match provider.provider.get_block_by_number(number.into()).await {
        Ok(Some(block)) => block,
        Ok(None) => return BlockFetch::Missing,
        Err(_) => return BlockFetch::Invalid,
    };
    if block.header().number() != number {
        return BlockFetch::Invalid;
    }
    let BlockTransactions::Hashes(transactions) = block.transactions() else {
        return BlockFetch::Invalid;
    };
    BlockFetch::Valid(
        BlockIdentity {
            number,
            hash: block.header().hash(),
            parent_hash: block.header().parent_hash(),
        },
        transactions.clone(),
    )
}

async fn fetch_receipt(
    provider: &ProviderHandle,
    block: BlockIdentity,
    transactions: &[B256],
    tx_hash: B256,
) -> ReceiptFetch {
    let receipts = match provider
        .provider
        .get_block_receipts(block.hash.into())
        .await
    {
        Ok(Some(receipts)) => receipts,
        Ok(None) => return ReceiptFetch::Invalid,
        Err(error)
            if error
                .as_error_resp()
                .is_some_and(|response| response.code == -32601) =>
        {
            return ReceiptFetch::Unsupported;
        }
        Err(_) => return ReceiptFetch::Failed,
    };
    let expected = transactions.iter().copied().collect::<HashSet<_>>();
    if expected.len() != transactions.len() || receipts.len() != transactions.len() {
        return ReceiptFetch::Invalid;
    }
    let mut seen = HashSet::with_capacity(receipts.len());
    let mut target_receipt = None;
    for receipt in receipts {
        let receipt_tx_hash = receipt.transaction_hash();
        if !seen.insert(receipt_tx_hash)
            || !expected.contains(&receipt_tx_hash)
            || receipt.block_hash() != Some(block.hash)
            || receipt.block_number() != Some(block.number)
        {
            return ReceiptFetch::Invalid;
        }
        if receipt_tx_hash == tx_hash {
            target_receipt = Some(receipt);
        }
    }
    if seen != expected {
        return ReceiptFetch::Invalid;
    }
    let Some(receipt) = target_receipt else {
        return ReceiptFetch::Invalid;
    };
    let status = receipt.status();
    let gas_used = receipt.gas_used();
    let contract_address = receipt
        .contract_address()
        .map(|address| address.to_checksum(None));
    ReceiptFetch::Valid(TxReceiptOutput {
        tx_hash: tx_hash.to_string(),
        status,
        block_number: block.number,
        gas_used,
        contract_address,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::rpc_broker::tests::data_result;
    use crate::rpc_broker::tests::spawn_counting_test_broker;
    use crate::{RpcChainRoute, RpcRead, RpcRoute, RpcSubmission, WalletRpcOrigin};
    use alloy::primitives::Bytes;
    use reqwest::Url;
    use serde_json::{Value, json};

    fn test_pool(url: Url) -> Arc<QueryRpcPool> {
        Arc::new(QueryRpcPool::with_http_client(
            vec![url],
            Duration::ZERO,
            reqwest::Client::new(),
        ))
    }

    fn test_broker() -> Arc<RpcBroker> {
        RpcBroker::spawn(reqwest::Client::new())
            .expect("test broker requires an active Tokio runtime")
    }

    fn spawn_rpc_script<F>(
        request_count: usize,
        handler: F,
    ) -> (Url, Receiver<Value>, thread::JoinHandle<()>)
    where
        F: Fn(&Value) -> std::result::Result<Value, Value> + Send + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind RPC fixture");
        let url = Url::parse(&format!(
            "http://{}",
            listener.local_addr().expect("RPC fixture address")
        ))
        .expect("RPC fixture URL");
        let (request_tx, request_rx) = mpsc::channel();
        let task = thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().expect("accept RPC fixture request");
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 1024];
                let (header_end, content_length) = loop {
                    let read = stream.read(&mut buffer).expect("read RPC fixture headers");
                    assert!(read != 0, "RPC fixture request ended before headers");
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let header_end = header_end + 4;
                        let headers = String::from_utf8_lossy(&bytes[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().expect("content length"))
                            })
                            .expect("RPC fixture content length");
                        break (header_end, content_length);
                    }
                };
                while bytes.len() < header_end + content_length {
                    let read = stream.read(&mut buffer).expect("read RPC fixture body");
                    assert!(read != 0, "RPC fixture request ended before body");
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let request: Value =
                    serde_json::from_slice(&bytes[header_end..header_end + content_length])
                        .expect("RPC fixture JSON");
                request_tx
                    .send(request.clone())
                    .expect("record RPC fixture request");
                let id = request.get("id").cloned().unwrap_or(Value::Null);
                let response = match handler(&request) {
                    Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
                    Err(error) => json!({"jsonrpc":"2.0", "id":id, "error":error}),
                };
                let response = serde_json::to_string(&response).expect("RPC fixture response");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .expect("write RPC fixture response");
            }
        });
        (url, request_rx, task)
    }

    fn quantity(value: u64) -> Value {
        Value::String(format!("0x{value:x}"))
    }

    fn zero_bloom() -> String {
        format!("0x{}", "00".repeat(256))
    }

    fn block(number: u64, hash: B256, parent_hash: B256, transactions: &[Value]) -> Value {
        json!({
            "number": quantity(number),
            "hash": hash,
            "parentHash": parent_hash,
            "sha3Uncles": B256::ZERO,
            "miner": Address::ZERO,
            "stateRoot": B256::ZERO,
            "transactionsRoot": B256::ZERO,
            "receiptsRoot": B256::ZERO,
            "logsBloom": zero_bloom(),
            "difficulty": quantity(0),
            "gasLimit": quantity(30_000_000),
            "gasUsed": quantity(0),
            "timestamp": quantity(1),
            "extraData": "0x",
            "mixHash": B256::ZERO,
            "nonce": "0x0000000000000000",
            "baseFeePerGas": quantity(1),
            "uncles": [],
            "transactions": transactions,
        })
    }

    fn receipt(tx_hash: B256, block_number: u64, block_hash: B256, status: u64) -> Value {
        json!({
            "transactionHash": tx_hash,
            "blockHash": block_hash,
            "blockNumber": quantity(block_number),
            "transactionIndex": quantity(0),
            "from": Address::ZERO,
            "to": Address::ZERO,
            "cumulativeGasUsed": quantity(21_000),
            "status": quantity(status),
            "gasUsed": quantity(21_000),
            "effectiveGasPrice": quantity(1),
            "logs": [],
            "logsBloom": zero_bloom(),
            "type": "0x0",
            "contractAddress": Value::Null,
        })
    }

    fn known_full_transaction_hash() -> B256 {
        "0xe9e91f1ee4b56c0df2e9f06c2b8c27c6076195a88a7b8537ba8313d80e6f124e"
            .parse()
            .expect("known typed transaction hash")
    }

    fn full_legacy_transaction(nonce: &str, hash: B256, from: Address) -> Value {
        json!({
            "blockHash": B256::from([0x99; 32]),
            "blockNumber": quantity(9),
            "hash": hash,
            "transactionIndex": quantity(0),
            "type": "0x0",
            "nonce": nonce,
            "input": "0x",
            "r": "0x3b08715b4403c792b8c7567edea634088bedcd7f60d9352b1f16c69830f3afd5",
            "s": "0x10b9afb67d2ec8b956f0e1dbc07eb79152904f3a7bf789fc869db56320adfe09",
            "chainId": "0x0",
            "v": "0x1c",
            "from": from,
            "to": "0xdf190dc7190dfba737d7777a163445b7fff16133",
            "value": "0x6113a84987be800",
            "gas": "0xc350",
            "gasPrice": "0xdf8475800",
        })
    }

    fn assert_no_exact_hash_methods(requests: &[Value]) {
        assert!(requests.iter().all(|request| {
            !matches!(
                request.get("method").and_then(Value::as_str),
                Some("eth_getTransactionByHash" | "eth_getTransactionReceipt")
            )
        }));
    }

    #[tokio::test]
    async fn observer_baselines_before_handoff_and_matches_block_receipt_locally() {
        let tx_hash = B256::from([0x11; 32]);
        let block_hash = B256::from([0x22; 32]);
        let parent_hash = B256::from([0x01; 32]);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) =
            spawn_rpc_script(4, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        2
                    },
                )),
                Some("eth_getBlockByNumber") => {
                    assert_eq!(request["params"], json!(["0x2", false]));
                    Ok(block(2, block_hash, parent_hash, &[json!(tx_hash)]))
                }
                Some("eth_getBlockReceipts") => {
                    assert_eq!(request["params"], json!([block_hash.to_string()]));
                    Ok(json!([receipt(tx_hash, 2, block_hash, 1)]))
                }
                method => panic!("unexpected RPC method {method:?}"),
            });
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 2, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(tx_hash, 3);
        let observation = observer.poll().await.expect("observe containing block");
        let (_, output) = observation.receipt.expect("matching receipt");
        assert_eq!(output.tx_hash, tx_hash.to_string());
        assert!(output.status);
        assert_eq!(output.block_number, 2);
        assert_eq!(output.gas_used, 21_000);
        task.join().expect("RPC fixture task");
        let requests = request_rx.try_iter().collect::<Vec<_>>();
        assert_eq!(requests[0]["method"], "eth_blockNumber");
        assert_no_exact_hash_methods(&requests);
    }

    #[tokio::test]
    async fn observer_receipt_forces_next_latest_read_past_the_cache() {
        let tx_hash = B256::from([0x11; 32]);
        let block_hash = B256::from([0x22; 32]);
        let parent_hash = B256::from([0x01; 32]);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let (url, _request_rx, task) =
            spawn_rpc_script(4, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        2
                    },
                )),
                Some("eth_getBlockByNumber") => {
                    Ok(block(2, block_hash, parent_hash, &[json!(tx_hash)]))
                }
                Some("eth_getBlockReceipts") => Ok(json!([receipt(tx_hash, 2, block_hash, 1)])),
                method => panic!("unexpected RPC method {method:?}"),
            });

        let (broker, executions) = spawn_counting_test_broker();
        broker.notify_block(1, 10);
        tokio::time::sleep(Duration::from_millis(5)).await;
        let route = RpcRoute::from(RpcChainRoute::new(
            1,
            vec![Url::parse("https://cache.invalid").unwrap()],
        ));
        let read = RpcRead::eth_call(Address::from([4_u8; 20]), Bytes::from_static(b"read"));
        let make_submission = || {
            RpcSubmission::new(
                route.clone(),
                vec![read.clone()],
                WalletRpcOrigin::PublicWallet.into(),
            )
        };
        assert_eq!(
            broker.submit(make_submission()).await.unwrap()[0],
            Ok(data_result(Bytes::from_static(b"cached")))
        );
        assert_eq!(
            broker.submit(make_submission()).await.unwrap()[0],
            Ok(data_result(Bytes::from_static(b"cached")))
        );
        assert_eq!(executions.load(Ordering::SeqCst), 1);

        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(pool, 2, Arc::clone(&broker), 1)
            .await
            .expect("establish observer baseline");
        observer.register(tx_hash, 0);
        let observation = observer.poll().await.expect("observe containing block");
        assert!(observation.receipt.is_some());

        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(broker.submit(make_submission()).await.unwrap()[0].is_ok());
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        task.join().expect("RPC fixture task");
    }

    #[tokio::test]
    async fn observer_selects_a_replacement_attempt_after_original_stays_pending() {
        let original = B256::from([0x12; 32]);
        let replacement = B256::from([0x13; 32]);
        let block_2 = B256::from([0x02; 32]);
        let block_3 = B256::from([0x03; 32]);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) =
            spawn_rpc_script(7, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else if head_calls_for_server.load(Ordering::SeqCst) == 2 {
                        2
                    } else {
                        3
                    },
                )),
                Some("eth_getBlockByNumber") => {
                    let number = u64::from_str_radix(
                        request["params"][0]
                            .as_str()
                            .expect("block number")
                            .trim_start_matches("0x"),
                        16,
                    )
                    .expect("block number quantity");
                    if number == 2 {
                        Ok(block(2, block_2, B256::from([0x01; 32]), &[]))
                    } else {
                        Ok(block(3, block_3, block_2, &[json!(replacement)]))
                    }
                }
                Some("eth_getBlockReceipts") => Ok(json!([receipt(replacement, 3, block_3, 0)])),
                method => panic!("unexpected RPC method {method:?}"),
            });
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 2, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(original, 0);
        assert!(
            observer
                .poll()
                .await
                .expect("original remains pending")
                .receipt
                .is_none()
        );
        observer.register(replacement, 1);
        let observation = observer.poll().await.expect("observe replacement");
        let (winner, output) = observation.receipt.expect("replacement receipt");
        assert_eq!(winner, 1);
        assert_eq!(output.tx_hash, replacement.to_string());
        assert!(!output.status);
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn observer_catch_up_is_bounded_and_preserves_cursor() {
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) =
            spawn_rpc_script(13, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        10
                    },
                )),
                Some("eth_getBlockByNumber") => {
                    let number = u64::from_str_radix(
                        request["params"][0]
                            .as_str()
                            .expect("block number")
                            .trim_start_matches("0x"),
                        16,
                    )
                    .expect("block number quantity");
                    let hash = B256::from([number as u8; 32]);
                    let parent = B256::from([number.saturating_sub(1) as u8; 32]);
                    Ok(block(number, hash, parent, &[]))
                }
                method => panic!("unexpected RPC method {method:?}"),
            });
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 2, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(B256::from([0x33; 32]), 0);
        let first = observer.poll().await.expect("first bounded poll");
        assert_eq!(first.head.map(|(_, head)| head), Some(9));
        let second = observer.poll().await.expect("second bounded poll");
        assert_eq!(second.head.map(|(_, head)| head), Some(10));
        task.join().expect("RPC fixture task");
        let requests = request_rx.try_iter().collect::<Vec<_>>();
        let block_requests = requests
            .iter()
            .filter(|request| request["method"] == "eth_getBlockByNumber")
            .collect::<Vec<_>>();
        assert_eq!(block_requests.len(), 10);
        assert_no_exact_hash_methods(&requests);
    }

    #[tokio::test]
    async fn observer_fails_closed_on_receipt_identity_mismatch() {
        let tx_hash = B256::from([0x44; 32]);
        let block_hash = B256::from([0x55; 32]);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) =
            spawn_rpc_script(4, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        2
                    },
                )),
                Some("eth_getBlockByNumber") => Ok(block(
                    2,
                    block_hash,
                    B256::from([0x01; 32]),
                    &[json!(tx_hash)],
                )),
                Some("eth_getBlockReceipts") => Ok(json!([receipt(tx_hash, 3, block_hash, 1,)])),
                method => panic!("unexpected RPC method {method:?}"),
            });
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 1, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(tx_hash, 0);
        let error = observer
            .poll()
            .await
            .expect_err("reject bad receipt identity");
        assert!(error.to_string().contains("block observation"));
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn observer_rejects_an_incomplete_block_receipt_set() {
        let tx_hash = B256::from([0x45; 32]);
        let other_hash = B256::from([0x46; 32]);
        let block_hash = B256::from([0x56; 32]);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) =
            spawn_rpc_script(4, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        2
                    },
                )),
                Some("eth_getBlockByNumber") => Ok(block(
                    2,
                    block_hash,
                    B256::from([0x01; 32]),
                    &[json!(tx_hash), json!(other_hash)],
                )),
                Some("eth_getBlockReceipts") => Ok(json!([receipt(tx_hash, 2, block_hash, 1)])),
                method => panic!("unexpected RPC method {method:?}"),
            });
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 1, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(tx_hash, 0);
        let error = observer
            .poll()
            .await
            .expect_err("incomplete receipt set must fail closed");
        assert!(error.to_string().contains("block observation"));
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn observer_marks_unsupported_receipts_session_local_and_fails_closed() {
        let tx_hash = B256::from([0xaa; 32]);
        let block_hash = B256::from([0xbb; 32]);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) =
            spawn_rpc_script(4, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        2
                    },
                )),
                Some("eth_getBlockByNumber") => Ok(block(
                    2,
                    block_hash,
                    B256::from([0x01; 32]),
                    &[json!(tx_hash)],
                )),
                Some("eth_getBlockReceipts") => Err(json!({
                    "code": -32601,
                    "message": "method not found",
                })),
                method => panic!("unexpected RPC method {method:?}"),
            });
        let mut url = url;
        url.set_username("rpc-user-sentinel")
            .expect("set RPC username");
        url.set_password(Some("rpc-password-sentinel"))
            .expect("set RPC password");
        url.set_path("/rpc-path-sentinel");
        url.set_query(Some("rpc-query-sentinel"));
        url.set_fragment(Some("rpc-fragment-sentinel"));
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 1, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(tx_hash, 0);
        let error = observer
            .poll()
            .await
            .expect_err("unsupported receipts fail closed");
        let message = error.to_string();
        assert!(message.contains("block receipt observation"));
        assert!(!message.contains(&tx_hash.to_string()));
        assert!(!message.contains("rpc-user-sentinel"));
        assert!(!message.contains("rpc-password-sentinel"));
        assert!(!message.contains("rpc-path-sentinel"));
        assert!(!message.contains("rpc-query-sentinel"));
        assert!(!message.contains("rpc-fragment-sentinel"));
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn observer_waits_for_a_lagging_provider_without_advancing_cursor() {
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) = spawn_rpc_script(2, move |request| {
            assert_eq!(request["method"], "eth_blockNumber");
            Ok(quantity(
                if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                    5
                } else {
                    4
                },
            ))
        });
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 1, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(B256::from([0xcc; 32]), 0);
        let observation = observer.poll().await.expect("lagging provider is pending");
        assert!(observation.receipt.is_none());
        assert!(observation.head.is_none());
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn observer_fails_over_when_baseline_provider_is_unusable() {
        let (bad_url, bad_requests, bad_task) = spawn_rpc_script(1, |_| {
            Err(json!({"code": -32000, "message": "temporary failure"}))
        });
        let (good_url, good_requests, good_task) = spawn_rpc_script(1, |_| Ok(quantity(4)));
        let pool = Arc::new(QueryRpcPool::with_http_client(
            vec![bad_url, good_url],
            Duration::ZERO,
            reqwest::Client::new(),
        ));
        let observer = BlockObserver::establish(Arc::clone(&pool), 1, test_broker(), 1)
            .await
            .expect("fail over to healthy baseline provider");
        drop(observer);
        bad_task.join().expect("bad RPC fixture task");
        good_task.join().expect("good RPC fixture task");
        assert_no_exact_hash_methods(&bad_requests.try_iter().collect::<Vec<_>>());
        assert_no_exact_hash_methods(&good_requests.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn observer_fails_over_after_a_transient_receipt_provider_error() {
        let tx_hash = B256::from([0xab; 32]);
        let block_hash = B256::from([0xbc; 32]);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_primary = Arc::clone(&head_calls);
        let (primary_url, primary_requests, primary_task) =
            spawn_rpc_script(4, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_primary.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        2
                    },
                )),
                Some("eth_getBlockByNumber") => Ok(block(
                    2,
                    block_hash,
                    B256::from([0x01; 32]),
                    &[json!(tx_hash)],
                )),
                Some("eth_getBlockReceipts") => Err(json!({
                    "code": -32000,
                    "message": "temporary receipt failure",
                })),
                method => panic!("unexpected RPC method {method:?}"),
            });
        let (backup_url, backup_requests, backup_task) =
            spawn_rpc_script(3, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(2)),
                Some("eth_getBlockByNumber") => Ok(block(
                    2,
                    block_hash,
                    B256::from([0x01; 32]),
                    &[json!(tx_hash)],
                )),
                Some("eth_getBlockReceipts") => Ok(json!([receipt(tx_hash, 2, block_hash, 1)])),
                method => panic!("unexpected RPC method {method:?}"),
            });
        let pool = Arc::new(QueryRpcPool::with_http_client(
            vec![primary_url, backup_url],
            Duration::from_mins(1),
            reqwest::Client::new(),
        ));
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 1, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(tx_hash, 0);
        let observation = observer
            .poll()
            .await
            .expect("fail over to backup receipt provider");
        assert!(observation.receipt.is_some());
        assert_eq!(pool.available_providers().len(), 1);
        primary_task.join().expect("primary RPC fixture task");
        backup_task.join().expect("backup RPC fixture task");
        assert_no_exact_hash_methods(&primary_requests.try_iter().collect::<Vec<_>>());
        assert_no_exact_hash_methods(&backup_requests.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn observer_rolls_back_to_a_retained_common_ancestor() {
        let reorged = Arc::new(AtomicBool::new(false));
        let reorged_for_server = Arc::clone(&reorged);
        let old_block_2 = B256::from([0x02; 32]);
        let old_block_3 = B256::from([0x03; 32]);
        let new_block_3 = B256::from([0x33; 32]);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) =
            spawn_rpc_script(8, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        3
                    },
                )),
                Some("eth_getBlockByNumber") => {
                    let number = u64::from_str_radix(
                        request["params"][0]
                            .as_str()
                            .expect("block number")
                            .trim_start_matches("0x"),
                        16,
                    )
                    .expect("block number quantity");
                    let reorged = reorged_for_server.load(Ordering::SeqCst);
                    let (hash, parent) = match number {
                        2 => (old_block_2, B256::from([0x01; 32])),
                        3 if reorged => (new_block_3, old_block_2),
                        3 => (old_block_3, old_block_2),
                        _ => panic!("unexpected block number {number}"),
                    };
                    Ok(block(number, hash, parent, &[]))
                }
                method => panic!("unexpected RPC method {method:?}"),
            });
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 3, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(B256::from([0xdd; 32]), 0);
        observer.poll().await.expect("scan original chain");
        reorged.store(true, Ordering::SeqCst);
        observer
            .poll()
            .await
            .expect("rollback and rescan reorged tip");
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn observer_fails_when_reorg_diverges_beyond_retained_history() {
        let reorged = Arc::new(AtomicBool::new(false));
        let reorged_for_server = Arc::clone(&reorged);
        let head_calls = Arc::new(AtomicUsize::new(0));
        let head_calls_for_server = Arc::clone(&head_calls);
        let (url, request_rx, task) =
            spawn_rpc_script(6, move |request| match request["method"].as_str() {
                Some("eth_blockNumber") => Ok(quantity(
                    if head_calls_for_server.fetch_add(1, Ordering::SeqCst) == 0 {
                        1
                    } else {
                        3
                    },
                )),
                Some("eth_getBlockByNumber") => {
                    let number = u64::from_str_radix(
                        request["params"][0]
                            .as_str()
                            .expect("block number")
                            .trim_start_matches("0x"),
                        16,
                    )
                    .expect("block number quantity");
                    let reorged = reorged_for_server.load(Ordering::SeqCst);
                    let (hash, parent) = if reorged {
                        (
                            B256::from([(number as u8).saturating_add(0x30); 32]),
                            B256::from([number.saturating_sub(1) as u8 + 0x30; 32]),
                        )
                    } else {
                        (
                            B256::from([number as u8; 32]),
                            B256::from([number.saturating_sub(1) as u8; 32]),
                        )
                    };
                    Ok(block(number, hash, parent, &[]))
                }
                method => panic!("unexpected RPC method {method:?}"),
            });
        let pool = test_pool(url);
        let mut observer = BlockObserver::establish(Arc::clone(&pool), 1, test_broker(), 1)
            .await
            .expect("establish observer baseline");
        observer.register(B256::from([0xee; 32]), 0);
        observer.poll().await.expect("scan original chain");
        reorged.store(true, Ordering::SeqCst);
        let error = observer
            .poll()
            .await
            .expect_err("divergence beyond history fails closed");
        assert!(error.to_string().contains("block observation"));
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn source_resolution_uses_full_block_with_decoy_transaction() {
        let target_hash = known_full_transaction_hash();
        let decoy_hash = B256::from([0x77; 32]);
        let sender = Address::from([0x88; 20]);
        let (url, request_rx, task) = spawn_rpc_script(1, move |request| {
            assert_eq!(request["method"], "eth_getBlockByNumber");
            assert_eq!(request["params"], json!(["0x9", true]));
            Ok(block(
                9,
                B256::from([0x99; 32]),
                B256::from([0x01; 32]),
                &[
                    full_legacy_transaction("0x43ec", decoy_hash, Address::from([0x70; 20])),
                    full_legacy_transaction("0x43eb", target_hash, sender),
                ],
            ))
        });
        let pool = test_pool(url);
        let origin = resolve_transaction_sender_by_block(&pool, 9, target_hash)
            .await
            .expect("resolve source sender");
        assert_eq!(origin, sender);
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn source_resolution_fails_over_after_a_null_block_response() {
        let target_hash = known_full_transaction_hash();
        let sender = Address::from([0x89; 20]);
        let (null_url, null_requests, null_task) = spawn_rpc_script(1, |_| Ok(Value::Null));
        let (valid_url, valid_requests, valid_task) = spawn_rpc_script(1, move |request| {
            assert_eq!(request["method"], "eth_getBlockByNumber");
            Ok(block(
                9,
                B256::from([0x9a; 32]),
                B256::from([0x01; 32]),
                &[full_legacy_transaction("0x43eb", target_hash, sender)],
            ))
        });
        let pool = Arc::new(QueryRpcPool::with_http_client(
            vec![null_url, valid_url],
            Duration::ZERO,
            reqwest::Client::new(),
        ));
        let origin = resolve_transaction_sender_by_block(&pool, 9, target_hash)
            .await
            .expect("fail over to valid source block provider");
        assert_eq!(origin, sender);
        null_task.join().expect("null RPC fixture task");
        valid_task.join().expect("valid RPC fixture task");
        assert_no_exact_hash_methods(&null_requests.try_iter().collect::<Vec<_>>());
        assert_no_exact_hash_methods(&valid_requests.try_iter().collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn source_resolution_rejects_hash_only_transaction_bodies() {
        let target_hash = B256::from([0xfa; 32]);
        let (url, request_rx, task) = spawn_rpc_script(1, move |request| {
            assert_eq!(request["method"], "eth_getBlockByNumber");
            Ok(block(
                9,
                B256::from([0x99; 32]),
                B256::from([0x01; 32]),
                &[json!(target_hash)],
            ))
        });
        let pool = test_pool(url);
        let error = resolve_transaction_sender_by_block(&pool, 9, target_hash)
            .await
            .expect_err("hash-only source body must fail");
        assert!(error.to_string().contains("source transaction resolution"));
        task.join().expect("RPC fixture task");
        assert_no_exact_hash_methods(&request_rx.try_iter().collect::<Vec<_>>());
    }
}
