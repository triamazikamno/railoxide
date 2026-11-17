use super::*;
use crate::tron::confirmation::TronBlock;
use crate::tron::http_api::TronTransactionInfo;
use crate::tron::transaction::EXPIRATION_WINDOW_MS;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const TRX: u64 = 1_000_000;
const START_MS: u64 = 1_793_459_319_000;
const BLOCK_MS: u64 = 3_000;
const USDT: Address = Address::repeat_byte(0xa6);
const PARAMETERS: ChainFeeParameters = ChainFeeParameters {
    bandwidth_price: 1_000,
    energy_price: 100,
    create_account_fee: TRX / 10,
    create_account_system_fee: TRX,
    create_account_bandwidth_rate: 1,
};

/// A chain whose latest block is at the clock, less `stale_ms`. Every broadcast transaction
/// is included, successfully and without logs, in the first solidified block after it.
struct Chain {
    now_ms: AtomicU64,
    stale_ms: u64,
    sender_exists: bool,
    receiver_exists: bool,
    energy_estimate: AtomicU64,
    broadcast_answer: Result<TronBroadcast, RpcBrokerError>,
    broadcasts: Mutex<Vec<SignedTransaction>>,
}

impl Chain {
    fn new() -> Self {
        Self {
            now_ms: AtomicU64::new(START_MS),
            stale_ms: 0,
            sender_exists: true,
            receiver_exists: true,
            energy_estimate: AtomicU64::new(65_000),
            broadcast_answer: Ok(TronBroadcast {
                accepted: true,
                code: None,
            }),
            broadcasts: Mutex::new(Vec::new()),
        }
    }

    fn latest(&self) -> TronBlock {
        let timestamp_ms = self.now_ms() - self.stale_ms;
        TronBlock {
            number: timestamp_ms / BLOCK_MS,
            hash: B256::repeat_byte(1),
            timestamp_ms,
            transactions: Vec::new(),
        }
    }

    fn broadcast_txids(&self) -> Vec<B256> {
        self.broadcasts
            .lock()
            .unwrap()
            .iter()
            .map(SignedTransaction::txid)
            .collect()
    }
}

#[async_trait::async_trait]
impl TronSendReads for Chain {
    async fn account_exists(&self, account: Address) -> Result<bool, RpcBrokerError> {
        Ok(if account == signer().address() {
            self.sender_exists
        } else {
            self.receiver_exists
        })
    }

    async fn account_resources(&self, _: Address) -> Result<AccountResources, RpcBrokerError> {
        Ok(AccountResources::default())
    }

    async fn chain_fee_parameters(&self) -> Result<ChainFeeParameters, RpcBrokerError> {
        Ok(PARAMETERS)
    }

    async fn energy_estimate(
        &self,
        _: Address,
        _: Address,
        _: Bytes,
    ) -> Result<u64, RpcBrokerError> {
        Ok(self.energy_estimate.load(Ordering::SeqCst))
    }

    async fn latest_block(&self) -> Result<ReferenceBlock, RpcBrokerError> {
        let block = self.latest();
        Ok(ReferenceBlock {
            number: block.number,
            hash: block.hash,
            timestamp_ms: block.timestamp_ms,
        })
    }

    async fn broadcast(
        &self,
        transaction: &SignedTransaction,
    ) -> Result<TronBroadcast, RpcBrokerError> {
        assert!(
            self.now_ms() < transaction.expiration_ms(),
            "expired broadcast"
        );
        self.broadcasts.lock().unwrap().push(transaction.clone());
        self.broadcast_answer.clone()
    }

    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl TronBlockReads for Chain {
    async fn solidified_head(&self) -> Result<TronBlock, RpcBrokerError> {
        let latest = self.latest();
        Ok(TronBlock {
            number: latest.number + 1,
            transactions: self.broadcast_txids(),
            ..latest
        })
    }

    async fn block(&self, number: u64) -> Result<TronBlock, RpcBrokerError> {
        Ok(TronBlock {
            number,
            ..self.latest()
        })
    }

    async fn transaction_infos(&self, _: u64) -> Result<Vec<TronTransactionInfo>, RpcBrokerError> {
        Ok(self
            .broadcast_txids()
            .into_iter()
            .map(|id| TronTransactionInfo {
                id,
                succeeded: true,
                logs: Vec::new(),
                fee_sun: 0,
            })
            .collect())
    }
}

fn signer() -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::repeat_byte(0x11)).unwrap()
}

fn draft(asset: TronSendAsset, amount: TronSendAmount, trx_balance: u64) -> TronSendDraft {
    TronSendDraft {
        sender: signer().address(),
        receiver: Address::repeat_byte(0x22),
        asset,
        amount,
        trx_balance,
    }
}

async fn submit(chain: &Chain, review: &TronSendReview) -> TronSendSubmission {
    submit_tron_send(
        chain,
        review,
        async { Ok(signer()) },
        None,
        None,
        || true,
        |_| {},
    )
    .await
    .unwrap()
}

#[test]
fn receivers_other_than_base58_tron_are_refused() {
    assert_eq!(
        parse_tron_receiver(" TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t ").unwrap(),
        alloy::primitives::address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c")
    );
    for text in [
        "0xa614f803b6fd780986a42c78ec9c7f77e6ded13c",
        "41a614f803b6fd780986a42c78ec9c7f77e6ded13c",
        "3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy",
    ] {
        assert!(
            matches!(
                parse_tron_receiver(text),
                Err(TronSendError::NotATronAddress)
            ),
            "{text}"
        );
    }
}

#[test]
fn a_receiver_lacks_the_token_only_above_125_percent_of_a_holders_estimate() {
    assert!(receiver_lacks_token(126, 100));
    assert!(!receiver_lacks_token(125, 100));
    assert!(!receiver_lacks_token(100, 100));
}

#[tokio::test]
async fn a_sender_not_on_chain_cannot_send() {
    let chain = Chain {
        sender_exists: false,
        ..Chain::new()
    };
    let draft = draft(
        TronSendAsset::Trx,
        TronSendAmount::Exact(U256::from(TRX)),
        TRX,
    );
    assert!(matches!(
        prepare_tron_send(&chain, &draft).await,
        Err(TronSendError::SenderMissing)
    ));
}

#[tokio::test]
async fn trx_to_the_sender_is_refused_before_any_read_but_a_token_is_not() {
    // A missing sender would fail the first read, so `SelfTransfer` shows none ran.
    let chain = Chain {
        sender_exists: false,
        ..Chain::new()
    };
    for amount in [TronSendAmount::Exact(U256::from(TRX)), TronSendAmount::Max] {
        let draft = TronSendDraft {
            receiver: signer().address(),
            ..draft(TronSendAsset::Trx, amount, 10 * TRX)
        };
        assert!(matches!(
            prepare_tron_send(&chain, &draft).await,
            Err(TronSendError::SelfTransfer)
        ));
    }

    let token = TronSendDraft {
        receiver: signer().address(),
        ..draft(
            TronSendAsset::Trc20(USDT),
            TronSendAmount::Exact(U256::from(1)),
            100 * TRX,
        )
    };
    let review = prepare_tron_send(&Chain::new(), &token).await.unwrap();
    assert_eq!(review.transfer.receiver, review.transfer.sender);
}

#[tokio::test]
async fn trx_to_a_new_account_states_creation_max_and_missing_trx() {
    let chain = Chain {
        receiver_exists: false,
        ..Chain::new()
    };
    let creation = TRX + TRX / 10;
    let max_draft = draft(TronSendAsset::Trx, TronSendAmount::Max, 5 * TRX);
    let max = prepare_tron_send(&chain, &max_draft).await.unwrap();
    assert!(max.creates_account);
    assert_eq!(max.fees.maximum, creation);
    assert_eq!(
        max.transfer.asset,
        TronAsset::Trx {
            amount_sun: 5 * TRX - creation
        }
    );
    assert_eq!(max.missing_trx, None);

    let exact = draft(
        TronSendAsset::Trx,
        TronSendAmount::Exact(U256::from(5 * TRX)),
        5 * TRX,
    );
    let review = prepare_tron_send(&chain, &exact).await.unwrap();
    assert_eq!(review.missing_trx, Some(creation));
}

#[tokio::test(start_paused = true)]
async fn a_higher_maximum_at_signing_returns_a_new_review_unsigned() {
    let chain = Chain::new();
    let draft = draft(
        TronSendAsset::Trc20(USDT),
        TronSendAmount::Exact(U256::from(1)),
        100 * TRX,
    );
    let review = prepare_tron_send(&chain, &draft).await.unwrap();
    chain.energy_estimate.store(70_000, Ordering::SeqCst);
    let TronSendSubmission::Rereview(rereview) = submit(&chain, &review).await else {
        panic!("signed despite a higher maximum");
    };
    assert!(rereview.fees.maximum > review.fees.maximum);
    assert!(chain.broadcasts.lock().unwrap().is_empty());

    // The new review signs within its own bounds.
    let TronSendSubmission::Decided { handoff, .. } = submit(&chain, &rereview).await else {
        panic!("unchanged fees asked for another review");
    };
    assert_eq!(handoff.transfer, rereview.transfer);
}

#[tokio::test(start_paused = true)]
async fn a_slow_approval_signs_on_a_fresh_block_and_any_broadcast_answer_waits_for_blocks() {
    let chain = Chain {
        broadcast_answer: Err(RpcBrokerError::Timeout),
        ..Chain::new()
    };
    let draft = draft(
        TronSendAsset::Trx,
        TronSendAmount::Exact(U256::from(TRX)),
        10 * TRX,
    );
    let review = prepare_tron_send(&chain, &draft).await.unwrap();
    let approval = async {
        chain
            .now_ms
            .fetch_add(2 * EXPIRATION_WINDOW_MS, Ordering::SeqCst);
        Ok(signer())
    };
    let mut updates = Vec::new();
    let submission = submit_tron_send(
        &chain,
        &review,
        approval,
        None,
        None,
        || true,
        |update| {
            updates.push(update);
        },
    )
    .await
    .unwrap();
    let TronSendSubmission::Decided { handoff, outcome } = submission else {
        panic!("unchanged fees asked for another review");
    };
    assert_eq!(handoff.reference_block, chain.latest().number);
    assert_eq!(chain.broadcast_txids(), [handoff.txid]);
    assert!(matches!(
        outcome,
        TronSendOutcome::Complete { amount, .. } if amount == U256::from(TRX)
    ));
    assert!(
        updates.iter().any(|update| update.message.is_some()
            && update.status == PublicActionProgressStatus::Pending)
    );
    assert_eq!(
        updates.last().unwrap().status,
        PublicActionProgressStatus::Done
    );
}

#[tokio::test]
async fn an_expired_transaction_is_never_handed_off() {
    // An endpoint whose latest block is older than the expiration window.
    let chain = Chain {
        stale_ms: EXPIRATION_WINDOW_MS + BLOCK_MS,
        ..Chain::new()
    };
    let draft = draft(
        TronSendAsset::Trx,
        TronSendAmount::Exact(U256::from(TRX)),
        10 * TRX,
    );
    let review = prepare_tron_send(&chain, &draft).await.unwrap();
    let (event_tx, mut events) = tokio::sync::mpsc::unbounded_channel();
    let result = submit_tron_send(
        &chain,
        &review,
        async { Ok(signer()) },
        Some(&event_tx),
        None,
        || true,
        |_| {},
    )
    .await;
    assert!(matches!(result, Err(TronSendError::Expired)));
    assert!(chain.broadcasts.lock().unwrap().is_empty());
    assert!(events.try_recv().is_err(), "handoff announced");
}

/// A Tron node whose latest block is the clock's, and whose solidified head at the same
/// height is past any expiration, so a send is dropped without a transaction lookup.
fn json_rpc_node() -> crate::rpc_broker::tests::RpcResponder {
    // A recorded mainnet block, so the broker's block validation accepts it.
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/confirmation.json")).unwrap();
    let recorded = fixture["blocks"]["87595297"].clone();
    Arc::new(move |request| {
        let result = if request["method"] == "eth_chainId" {
            json!("0x2b6653dc")
        } else {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let timestamp = if request["params"][0] == "finalized" {
                now + 86_400
            } else {
                now
            };
            let mut block = recorded.clone();
            block["number"] = json!(U64::from(100));
            block["hash"] = json!(B256::repeat_byte(1));
            block["timestamp"] = json!(U64::from(timestamp));
            block["transactions"] = json!([]);
            block
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    })
}

/// An HTTP API on which every account exists without resources and every broadcast is
/// accepted.
fn http_api_node() -> crate::rpc_broker::tests::RpcResponder {
    let fixtures: Value = serde_json::from_str(include_str!("../fixtures/http_api.json")).unwrap();
    Arc::new(move |request| {
        if request.get("num").is_some() {
            fixtures["getblockbynum_0"].clone()
        } else if request.get("address").is_some() {
            json!({ "address": request["address"] })
        } else if request.get("transaction").is_some() {
            json!({ "result": true })
        } else {
            fixtures["getchainparameters"].clone()
        }
    })
}

#[tokio::test(start_paused = true)]
async fn each_send_reads_and_broadcasts_only_over_its_own_operation_client() {
    use crate::rpc_broker::tests::{spawn_recording_rpc_mock, tagged_reqwest_client};
    use crate::settings::{TronEndpointSetting, TronSettings};
    use crate::tron::address::encode_tron_address;
    use crate::{HttpContext, OperationHttpClient, OperationNetworkIsolation};

    let (rpc, rpc_log, _rpc) = spawn_recording_rpc_mock(json_rpc_node()).await;
    let (api, api_log, _api) = spawn_recording_rpc_mock(http_api_node()).await;
    let route = crate::tron::tron_rpc_route(&TronSettings {
        endpoints: Some(vec![TronEndpointSetting {
            json_rpc_url: rpc.to_string(),
            http_api_url: Some(api.to_string()),
            ..TronEndpointSetting::default()
        }]),
        ..TronSettings::default()
    })
    .unwrap();
    let http = HttpContext::direct_for_tests();
    // Each send has its own receiver, so its receiver reads can be told apart.
    let receiver = |tag: &str| Address::repeat_byte(if tag == "a" { 0x22 } else { 0x33 });
    let send = |tag: &'static str| {
        let reads = TronReads::new(
            http.clone(),
            &route,
            OperationHttpClient::for_tests(
                tagged_reqwest_client(tag),
                OperationNetworkIsolation::Dedicated,
            ),
        );
        let draft = TronSendDraft {
            receiver: receiver(tag),
            ..draft(
                TronSendAsset::Trx,
                TronSendAmount::Exact(U256::from(TRX)),
                100 * TRX,
            )
        };
        async move {
            let review = prepare_tron_send(&reads, &draft).await.unwrap();
            let submission = submit_tron_send(
                &reads,
                &review,
                async { Ok(signer()) },
                None,
                None,
                || true,
                |_| {},
            )
            .await
            .unwrap();
            let TronSendSubmission::Decided { outcome, .. } = submission else {
                panic!("unchanged fees asked for another review");
            };
            assert_eq!(outcome, TronSendOutcome::Dropped);
        }
    };
    tokio::join!(send("a"), send("b"));

    // Identity checks go over the shared clients and name no account.
    for (api_name, log) in [("JSON-RPC", &rpc_log), ("HTTP API", &api_log)] {
        let requests: Vec<_> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["method"] != "eth_chainId" && body["num"] != 0)
            .cloned()
            .collect();
        for tag in ["a", "b"] {
            assert!(
                requests
                    .iter()
                    .any(|(sent, _)| sent.as_deref() == Some(tag)),
                "{api_name}: no request of {tag}"
            );
        }
        for (sent, body) in &requests {
            assert!(
                matches!(sent.as_deref(), Some("a" | "b")),
                "{api_name}: {body} over {sent:?}"
            );
        }
    }
    for tag in ["a", "b"] {
        let address = encode_tron_address(receiver(tag));
        let reads: Vec<_> = api_log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body["address"] == address.as_str())
            .map(|(sent, _)| sent.clone())
            .collect();
        assert!(!reads.is_empty(), "{tag}'s receiver unread");
        assert!(
            reads.iter().all(|sent| sent.as_deref() == Some(tag)),
            "{tag}'s receiver read over {reads:?}"
        );
    }
}
