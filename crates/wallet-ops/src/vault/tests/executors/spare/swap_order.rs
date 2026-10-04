use super::swap_setup::{
    DESTINATION_CHAIN, USDC, WETH, broadcaster, destination_chain_config, password, setup_approval,
};
use super::*;
use crate::cow::{CowOrderbookClient, CowQuote, GAS_SHARE_BALANCED_BPS};
use crate::settings::{BridgeReceiverRejection, SwapReceiverRejection};
use crate::vault::{SwapAccountRefusal, SwapAccountRole, SwapAccountUse};
use crate::{
    OperationHttpClient, OperationNetworkIsolation, SwapAmountPlan, SwapAmountRequest,
    SwapOrderOutcome, SwapPairPreparation, SwapPrice, SwapReviewChange, SwapReviewRequest,
    SwapSetupStatus, WalletNetworkMode, prepare_swap_pair, swap_setup_status,
};
use alloy::eips::eip7702::constants::EIP7702_DELEGATION_DESIGNATOR;
use broadcaster_core::contracts::across::{MulticallHandler, SpokePool, private_delivery_message};
use broadcaster_core::contracts::cow::{
    AppData, BUY_NATIVE_TOKEN, ORDER_KIND_SELL, Order, TOKEN_BALANCE_ERC20, order_digest,
    order_uid, recover_order_signer,
};
use broadcaster_core::contracts::railgun::{approveCall, shieldCall, transferCall};

pub(super) type Submissions = Arc<Mutex<Vec<(bool, Value)>>>;

/// Orderbook stub that records each order request and whether the order's UID was already
/// persisted when the request arrived.
pub(super) async fn spawn_orderbook(
    db: Arc<local_db::DbStore>,
    view: Arc<DesktopViewSession>,
    operation: ExecutorOperationId,
    settlement: Address,
) -> (url::Url, Submissions, tokio::task::JoinHandle<()>) {
    spawn_orderbook_with_lost_response(db, view, operation, settlement, false).await
}

async fn spawn_orderbook_with_lost_response(
    db: Arc<local_db::DbStore>,
    view: Arc<DesktopViewSession>,
    operation: ExecutorOperationId,
    settlement: Address,
    lose_first_response: bool,
) -> (url::Url, Submissions, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mainnet", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let submissions = Submissions::default();
    let recorded = submissions.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut stream = BufReader::new(stream);
            let Some(body) = read_json_body(&mut stream).await else {
                continue;
            };
            let owner = body["from"].as_str().unwrap().parse().unwrap();
            let uid = order_uid(&submitted_order(&body), 1, settlement, owner);
            let persisted = ExecutorStore::new(db.clone(), view.clone(), 1)
                .unwrap()
                .records()
                .unwrap()
                .iter()
                .any(|record| {
                    record.operation() == operation
                        && record.swap().is_some_and(|swap| {
                            swap.orders().iter().any(|order| order.uid() == uid)
                        })
                });
            recorded.lock().unwrap().push((persisted, body));
            let requests = recorded.lock().unwrap().len();
            if lose_first_response && requests == 1 {
                continue; // The server accepted the order, but the response was lost.
            }
            let (status, reply) = if lose_first_response && requests == 2 {
                (
                    "400 Bad Request",
                    json!({"errorType":"DuplicatedOrder", "description":"order already exists"})
                        .to_string(),
                )
            } else {
                ("201 Created", json!(uid.0).to_string())
            };
            stream
                .get_mut()
                .write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                .await
                .unwrap();
        }
    });
    (url, submissions, task)
}

/// Orderbook stub that records each quote request and answers it with `quote`.
async fn spawn_quote_stub(
    quote: Value,
    response_ready: Option<Arc<tokio::sync::Notify>>,
) -> (
    url::Url,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mainnet", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut stream = BufReader::new(stream);
            let Some(body) = read_json_body(&mut stream).await else {
                continue;
            };
            recorded.lock().unwrap().push(body);
            if let Some(ready) = &response_ready {
                ready.notified().await;
            }
            let reply = quote.to_string();
            stream
                .get_mut()
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                .await
                .unwrap();
        }
    });
    (url, requests, task)
}

/// Bridge provider stub that records each request's path and JSON body, `Null` for a GET, and
/// answers it with `respond(body)`.
pub(super) async fn spawn_bridge_stub(
    respond: impl Fn(&Value) -> String + Send + 'static,
) -> (
    url::Url,
    Arc<Mutex<Vec<(String, Value)>>>,
    tokio::task::JoinHandle<()>,
) {
    spawn_bridge_stub_with(move |_, body| (200, respond(body))).await
}

/// [`spawn_bridge_stub`] whose `respond(path, body)` also sees the request's path and chooses
/// the HTTP status.
async fn spawn_bridge_stub_with(
    respond: impl Fn(&str, &Value) -> (u16, String) + Send + 'static,
) -> (
    url::Url,
    Arc<Mutex<Vec<(String, Value)>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut stream = BufReader::new(stream);
            let mut request_line = String::new();
            if stream.read_line(&mut request_line).await.unwrap() == 0 {
                continue;
            }
            let Some(body) = read_json_body(&mut stream).await else {
                continue;
            };
            let path = request_line.split_whitespace().nth(1).unwrap().to_owned();
            let (status, reply) = respond(&path, &body);
            recorded.lock().unwrap().push((path, body));
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            stream
                .get_mut()
                .write_all(format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes())
                .await
                .unwrap();
        }
    });
    (url, requests, task)
}

/// One HTTP request's JSON body, `Null` when it has none, or `None` when the client closed the
/// connection first.
async fn read_json_body(stream: &mut BufReader<tokio::net::TcpStream>) -> Option<Value> {
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.unwrap() == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = length.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).await.unwrap();
    if body.is_empty() {
        return Some(Value::Null);
    }
    Some(serde_json::from_slice(&body).unwrap())
}

/// Records each commit of change-output PPOI contexts with the number of order requests the
/// orderbook stub had received by then.
#[derive(Default)]
pub(super) struct OutputPois {
    pub(super) submissions: Submissions,
    pub(super) commits: Mutex<Vec<(usize, Vec<local_db::PendingOutputPoiContextRecord>)>>,
}

#[async_trait::async_trait]
impl crate::SwapOutputPoiSink for OutputPois {
    async fn commit(
        &self,
        contexts: &[local_db::PendingOutputPoiContextRecord],
    ) -> eyre::Result<()> {
        let requests = self.submissions.lock().unwrap().len();
        self.commits
            .lock()
            .unwrap()
            .push((requests, contexts.to_vec()));
        Ok(())
    }
}

/// Holds each output PPOI commit until the test releases it.
#[derive(Default)]
struct StalledOutputPois {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl crate::SwapOutputPoiSink for StalledOutputPois {
    async fn commit(&self, _: &[local_db::PendingOutputPoiContextRecord]) -> eyre::Result<()> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

/// A freshly proved pre-hook's change output, as the wallet actor receives it: identified by
/// its commitment, with no transaction or on-chain observation.
/// The wallet's local notes on a destination chain, as a test sets them.
#[derive(Default)]
struct ShieldNotes(Mutex<Vec<crate::WalletUtxo>>);

impl crate::SwapShieldNotes for ShieldNotes {
    fn shield_notes(
        &self,
        shield: &crate::vault::SwapEarlierShield,
    ) -> Option<Vec<crate::WalletUtxo>> {
        Some(crate::notes_of_shield(&self.0.lock().unwrap(), shield))
    }
}

fn change_output_poi(commitment: B256) -> local_db::PendingOutputPoiContextRecord {
    local_db::PendingOutputPoiContextRecord {
        chain_id: 1,
        wallet_id: TEST_WALLET_ID.to_owned(),
        txid_version: "V2_PoseidonMerkle".to_owned(),
        output_commitment: commitment,
        output_npk: B256::repeat_byte(0x71),
        utxo_tree_in: 0,
        railgun_txid: U256::from(0x72),
        txid_merkleroot_index: None,
        pre_transaction_pois_per_txid_leaf_per_list: std::collections::BTreeMap::new(),
        required_poi_list_keys: Vec::new(),
        output_role: local_db::PendingOutputPoiRole::Change,
        created_at: 1,
        source_operation_id: None,
        observation: None,
        submitted_poi_list_keys: Vec::new(),
        terminal_error: None,
    }
}

/// Seconds since the Unix epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub(super) fn submitted_order(body: &Value) -> Order {
    let field = |name: &str| body[name].as_str().unwrap().to_owned();
    Order {
        sellToken: field("sellToken").parse().unwrap(),
        buyToken: field("buyToken").parse().unwrap(),
        receiver: field("receiver").parse().unwrap(),
        sellAmount: field("sellAmount").parse().unwrap(),
        buyAmount: field("buyAmount").parse().unwrap(),
        validTo: u32::try_from(body["validTo"].as_u64().unwrap()).unwrap(),
        appData: field("appDataHash").parse().unwrap(),
        feeAmount: field("feeAmount").parse().unwrap(),
        kind: field("kind"),
        partiallyFillable: body["partiallyFillable"].as_bool().unwrap(),
        sellTokenBalance: field("sellTokenBalance"),
        buyTokenBalance: field("buyTokenBalance"),
    }
}

#[tokio::test]
async fn swap_quote_uses_background_prices_without_fee_or_anchor_reads() {
    #[derive(serde::Serialize)]
    struct LegacyApproval<'a> {
        bounds: &'a SwapApprovedBounds,
        price_acknowledged: bool,
    }

    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let gas_sampled = Arc::new(tokio::sync::Notify::new());
    let response_ready = gas_sampled.clone();
    let (endpoint, rpc_task) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            assert_eq!(
                request["method"], "eth_gasPrice",
                "quotes need no fee or anchor RPC"
            );
            gas_sampled.notify_one();
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x3b9aca00"})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let settings = crate::settings::WalletSettings::default();
    let mut chain = crate::settings::build_effective_chain_configs(&settings)
        .unwrap()
        .get(1)
        .unwrap()
        .clone();
    chain.rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
    let profile = chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let amount = U256::from(1_000_000_000_000_000_000_u64);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            WETH,
            amount,
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
        relay_adapt_contract: Address::repeat_byte(5),
    };
    let SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
        &builder,
        &profile,
        owner.swap_preview_executor().unwrap(),
        &[input],
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: USDC,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("one note fits");
    };
    // A 0.1 WETH CoW fee makes the net output more than 3% below the anchor, but the
    // trading rate itself is fair. Explicit fees must not fail the exchange-rate check.
    // Withhold the quote until the gas request arrives: awaiting the quote before starting
    // the RPC would stall, even though both requests are independent.
    let (quote_url, quotes, quote_task) = spawn_quote_stub(
        json!({
            "quote": {
                "sellToken": WETH, "buyToken": USDC, "sellAmount": "897500000000000000",
                "buyAmount": "2692500000", "validTo": 1, "feeAmount": "100000000000000000", "gasAmount": "0",
                "gasPrice": "0", "sellTokenPrice": "1", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }),
        Some(response_ready),
    )
    .await;
    let cache = crate::TokenAnchorRateCache::new();
    cache.store_rate(1, WETH, amount);
    cache.store_rate(1, USDC, U256::from(3_000_000_000_u64));
    let tokens = crate::settings::build_effective_token_registry(&settings).unwrap();
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        ),
        quote_url,
        1,
    )
    .unwrap();
    let review = tokio::time::timeout(
        Duration::from_secs(5),
        owner.review_swap(SwapReviewRequest {
            plan: plan.clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(10),
            orderbook: &orderbook,
            anchor_cache: Some(&cache),
            token_registry: &tokens,
            bridge: None,
        }),
    )
    .await
    .expect("gas price must be queried without waiting for the quote response")
    .unwrap();
    assert!(
        matches!(review.price(), SwapPrice::Verified { rate, observations }
        if rate.buy_rate == U256::from(3_000_000_000_u64) && observations.is_empty())
    );
    // 1 gwei from RPC with the swap's 25% cushion, at 3,000 USDC/ETH.
    // Applying the broadcaster buffer as well would inflate this allowance.
    assert_eq!(
        review.gas_estimate(),
        (U256::from(plan.hook_gas_estimate()) * U256::from(15)).div_ceil(U256::from(4))
    );
    // Configured oracles with no cached rates still produce a quote. The user must
    // explicitly accept the missing independent check before it can be approved.
    let unverified = owner
        .review_swap(SwapReviewRequest {
            plan: plan.clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(10),
            orderbook: &orderbook,
            anchor_cache: Some(&crate::TokenAnchorRateCache::new()),
            token_registry: &tokens,
            bridge: None,
        })
        .await
        .unwrap();
    assert_eq!(unverified.price(), &SwapPrice::Unverified);
    let unverified_minimum = unverified.suggested_private_minimum();
    assert!(unverified.approval(unverified_minimum, false).is_err());
    assert!(unverified.approval(unverified_minimum, true).is_ok());
    // A failed fresh check must not be upgraded back to verified by the old cache
    // when returning to review. The quote and its economic terms remain available.
    let retry = owner
        .review_swap(SwapReviewRequest {
            plan: plan.clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(10),
            orderbook: &orderbook,
            anchor_cache: None,
            token_registry: &tokens,
            bridge: None,
        })
        .await
        .unwrap();
    assert_eq!(retry.price(), &SwapPrice::Unverified);
    assert_eq!(
        retry.suggested_private_minimum(),
        review.suggested_private_minimum()
    );
    assert!(
        retry
            .approval(retry.suggested_private_minimum(), false)
            .is_err()
    );
    assert_eq!(
        quotes.lock().unwrap()[0]["sellAmountBeforeFee"],
        "997500000000000000"
    );
    // Raising the anchor makes the trading rate itself poor. A large explicit fee must
    // not exempt that quote from the independent rate protection.
    cache.store_rate(1, USDC, U256::from(3_200_000_000_u64));
    let error = owner
        .review_swap(SwapReviewRequest {
            plan,
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(10),
            orderbook: &orderbook,
            anchor_cache: Some(&cache),
            token_registry: &tokens,
            bridge: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<crate::QuoteDeviationError>(),
        Some(&crate::QuoteDeviationError::ExceedsThreshold)
    );
    // Cached checks have no block observations. They must survive restart as checked, so the
    // same review doesn't ask for approval again after setup.
    let approval = review
        .approval(review.suggested_private_minimum(), false)
        .unwrap();
    let restored = rmp_serde::from_slice(&rmp_serde::to_vec_named(&approval).unwrap()).unwrap();
    assert_eq!(review.approval_change(&restored), None);
    let mut unchecked = restored;
    unchecked.price_verified = Some(false);
    assert_eq!(
        review.approval_change(&unchecked),
        Some(SwapReviewChange::PriceVerification)
    );
    // Old approvals have no explicit verification marker; their recorded observations
    // still distinguish a checked price from an acknowledged, unverified one.
    let mut legacy_bounds = approval.bounds;
    legacy_bounds.anchors = vec![SwapAnchorObservation {
        source: Address::repeat_byte(1),
        block: alloy::eips::BlockNumHash::new(10, B256::repeat_byte(2)),
        block_timestamp: 100,
        updated_at: Some(100),
    }];
    let restore_legacy = |bounds: &SwapApprovedBounds| {
        rmp_serde::from_slice(
            &rmp_serde::to_vec_named(&LegacyApproval {
                bounds,
                price_acknowledged: bounds.anchors.is_empty(),
            })
            .unwrap(),
        )
        .unwrap()
    };
    let old = restore_legacy(&legacy_bounds);
    assert_eq!(review.approval_change(&old), None);
    legacy_bounds.anchors.clear();
    let old = restore_legacy(&legacy_bounds);
    assert_eq!(
        review.approval_change(&old),
        Some(SwapReviewChange::PriceVerification)
    );
    quote_task.abort();
    rpc_task.abort();
    drop(owner);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// Proving is replaced by a hand-built pre-hook transaction that spends the planned note; the
// signing, persistence, and submission path is the one `submit_swap_order` runs after proving.
#[tokio::test]
async fn swap_order_is_signed_for_current_terms_and_persisted_before_submission() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let setup_chain = chain(&rpc);
    let profile = setup_chain.accepted_executor_profile().unwrap();
    let delegate = profile.delegate();
    let setup_owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        setup_chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let executor = setup_owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            None,
            &password(),
        )
        .await
        .unwrap()
        .context()
        .executor;
    setup_owner.shutdown().await;
    drop(setup_owner);

    // The setup wins nonce 0, so the pre-hook signs at k = 1 and the post-hook at k + 1.
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let before =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.reconcile(operation, before, &[]).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), before, Vec::new()),
            ),
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    let setup_won = (
        setup,
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(4),
            ExecutorExecutionResult::Executed,
        ),
    );
    let record = store.reconcile(operation, observed, &[setup_won]).unwrap();
    let code = [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        delegate.as_slice(),
    ]
    .concat();
    let SwapSetupStatus::Delegated(delegated) =
        swap_setup_status(&record, observed.block(), &code, profile)
    else {
        panic!("the setup delegated the executor");
    };

    // Signing must use cached anchors even with configured oracles and the shared Railgun fee
    // constant. Only the quote's gas price may reach this endpoint; no fee, head or oracle
    // request is needed.
    let unexpected_reads = Arc::new(AtomicU64::new(0));
    let unexpected = unexpected_reads.clone();
    let (fee_endpoint, fee_server) = crate::rpc_broker::tests::spawn_rpc_mock(
        Arc::new(move |request: Value| {
            if request["method"] != "eth_gasPrice" {
                unexpected.fetch_add(1, Ordering::Relaxed);
                return json!({"jsonrpc": "2.0", "id": request["id"],
                    "error": {"code": -32601, "message": "chain reads unavailable"}});
            }
            json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x1"})
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let mut swap_chain = chains.get(1).cloned().unwrap();
    swap_chain.rpc_route = crate::RpcChainRoute::new(1, vec![fee_endpoint]);
    let swap_profile = swap_chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        swap_chain.clone(),
        HttpContext::direct_for_tests(),
    )
    .unwrap();

    let amount = U256::from(1_000_000);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            WETH,
            amount,
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
        relay_adapt_contract: Address::repeat_byte(5),
    };
    let SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
        &builder,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: USDC,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        swap_profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("one note fits one order");
    };
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let (quote_url, quotes, quote_task) = spawn_quote_stub(
        json!({
            "quote": {
                "sellToken": WETH, "buyToken": USDC, "sellAmount": "996500", "buyAmount": "3000000000",
                "validTo": 1, "feeAmount": "1000", "gasAmount": "0", "gasPrice": "0",
                "sellTokenPrice": "1000000000000", "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }),
        None,
    )
    .await;
    let tokens = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let anchors = crate::TokenAnchorRateCache::new();
    anchors.store_rate(1, WETH, U256::from(997_500));
    anchors.store_rate(1, USDC, U256::from(3_000_000_000_u64));
    // The user chose a 30-minute validity for this Private delivery swap.
    let quoted_from = unix_now();
    let review = owner
        .review_swap(SwapReviewRequest {
            plan,
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(30),
            orderbook: &CowOrderbookClient::new(
                OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
                quote_url,
                1,
            )
            .unwrap(),
            anchor_cache: Some(&anchors),
            token_registry: &tokens,
            bridge: None,
        })
        .await
        .unwrap();
    let quoted_until = unix_now();
    quote_task.abort();
    assert!(matches!(review.price(), SwapPrice::Verified { .. }));
    assert_eq!(review.valid_for(), Duration::from_mins(30));
    let quote_valid_to = quotes.lock().unwrap()[0]["validTo"].as_u64().unwrap();
    assert!((quoted_from + 1_800..=quoted_until + 1_800).contains(&quote_valid_to));
    // Railgun keeps 0.25% of the 1,000,000 the pre-hook unshields. The quote prices what the
    // executor then holds, and the order sells it.
    let sell_amount = U256::from(997_500);
    assert_eq!(review.sell_amount(), sell_amount);
    assert_eq!(
        quotes.lock().unwrap()[0]["sellAmountBeforeFee"],
        json!(sell_amount.to_string())
    );
    let private_minimum = review.suggested_private_minimum();
    let buy_amount = review.buy_amount_for(private_minimum).unwrap();

    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook_with_lost_response(
        db.clone(),
        view.clone(),
        operation,
        swap_profile.settlement(),
        true,
    )
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        orderbook_url,
        1,
    )
    .unwrap();
    let pre_hook_transaction = Transaction {
        proof: SnarkProof::default(),
        merkleRoot: B256::ZERO,
        nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
        commitments: vec![B256::ZERO],
        boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
        unshieldPreimage: CommitmentPreimage::empty(),
    };
    let authorization = password();
    let output_pois = OutputPois {
        submissions: submissions.clone(),
        ..OutputPois::default()
    };
    let change = change_output_poi(pre_hook_transaction.commitments[0]);
    let issue = |transactions: Vec<Transaction>,
                 change_output_pois: Vec<local_db::PendingOutputPoiContextRecord>| {
        owner.issue_swap_order(crate::SwapOrderSigning {
            review: &review,
            swap_use: crate::vault::SwapUseId::first(review.plan().operation().unwrap()),
            private_minimum,
            price_acknowledged: true,
            transactions,
            inputs: std::slice::from_ref(&input),
            change_output_pois,
            output_pois: &output_pois,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &anchors,
            token_registry: &tokens,
            bridge: None,
            destination_minimum: None,
            destination: None,
        })
    };
    let first = || issue(vec![pre_hook_transaction.clone()], vec![change.clone()]);

    // A stalled output PPOI commit must not lock out another operation. If the selected
    // account changes during that wait, the late result must never persist or submit signed
    // data.
    {
        let gate = StalledOutputPois::default();
        let preparing = ExecutorOwner::new(
            0,
            db.clone(),
            view.clone(),
            swap_chain,
            HttpContext::direct_for_tests(),
        )
        .unwrap();
        let pending = preparing.issue_swap_order(crate::SwapOrderSigning {
            review: &review,
            swap_use: crate::vault::SwapUseId::first(review.plan().operation().unwrap()),
            private_minimum,
            price_acknowledged: true,
            transactions: vec![pre_hook_transaction.clone()],
            inputs: std::slice::from_ref(&input),
            change_output_pois: vec![change.clone()],
            output_pois: &gate,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &anchors,
            token_registry: &tokens,
            bridge: None,
            destination_minimum: None,
            destination: None,
        });
        tokio::pin!(pending);
        tokio::select! {
            () = gate.started.notified() => {},
            result = &mut pending => panic!("the PPOI commit did not wait: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(5)) => panic!("the commit never started"),
        }
        let unrelated = tokio::time::timeout(
            Duration::from_secs(2),
            preparing.reconcile_history(ExecutorOperationId::random().unwrap(), 1..2),
        )
        .await
        .expect("network preparation must release activity");
        assert!(unrelated.is_err(), "the unrelated operation does not exist");
        store.invalidate_observation(operation).unwrap();
        gate.release.notify_one();
        let error = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("changed during preparation"),
            "{error:#}"
        );
        assert!(submissions.lock().unwrap().is_empty());
        assert!(
            store
                .records()
                .unwrap()
                .iter()
                .find(|record| record.operation() == operation)
                .unwrap()
                .swap()
                .is_none()
        );
        preparing.shutdown().await;
        store.reconcile(operation, observed, &[setup_won]).unwrap();
    }
    let signed_from = unix_now();
    assert!(
        first()
            .await
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let signed_until = unix_now();
    // Reload through a new store handle. The signature survives interruption, and no
    // signing authorization or proof is needed to resend the exact original request.
    let restored_store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let restored = restored_store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let saved = restored.swap().unwrap().orders().last().unwrap();
    assert_eq!(saved.submission_status(), SwapSubmissionStatus::Pending);
    assert_eq!(
        (saved.bounds().sell_amount, saved.bounds().spend_amount()),
        (sell_amount, amount)
    );
    // A release frees the unsent order's notes, but resending the order reserves them again,
    // and only while no other operation took them after the release.
    let pre_hook_inputs = vec![ExecutorInputIdentity::from_utxo(&input)];
    assert_eq!(restored.reserved_inputs(), pre_hook_inputs);
    owner.release_input_lock(operation).unwrap();
    let released = restored_store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert!(released.reserved_inputs().is_empty());
    let other = ExecutorOperationId::random().unwrap();
    // The reservation takes the prefetched spare, which already has its address.
    let other_address = store
        .reserve(other, delegate, None, &[])
        .unwrap()
        .address()
        .unwrap_or(Address::repeat_byte(9));
    store.bind_address(other, other_address).unwrap();
    store.reconcile(other, before, &[]).unwrap();
    store
        .record_issued(
            other,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                B256::repeat_byte(0x55),
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(
                    Bytes::from_static(b"other"),
                    before,
                    pre_hook_inputs.clone(),
                ),
            ),
        )
        .unwrap();
    assert_eq!(
        owner
            .resubmit_swap_order(operation, &orderbook)
            .await
            .unwrap_err()
            .to_string(),
        "another operation now uses this order's notes; wait for the order to expire"
    );
    owner.release_input_lock(other).unwrap();
    let SwapOrderOutcome::Submitted { uid } = owner
        .resubmit_swap_order(operation, &orderbook)
        .await
        .unwrap()
    else {
        panic!("the duplicate order is accepted");
    };
    assert_eq!(uid, saved.uid());
    let submitted = submissions.lock().unwrap().clone();
    let [(persisted, body), (resent_persisted, resent)] = submitted.as_slice() else {
        panic!("one initial request and one resubmission");
    };
    assert!(*resent_persisted);
    assert_eq!(
        body, resent,
        "the order, signature, quote and appData are unchanged"
    );
    let accepted = restored_store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(
        accepted.swap().unwrap().orders()[0].submission_status(),
        SwapSubmissionStatus::Accepted
    );
    assert_eq!(accepted.reserved_inputs(), pre_hook_inputs);
    // Accepted requests are idempotent locally, too.
    assert_eq!(
        owner
            .resubmit_swap_order(operation, &orderbook)
            .await
            .unwrap(),
        SwapOrderOutcome::Submitted { uid }
    );
    assert!(
        *persisted,
        "the attempt is durable before the order request arrives"
    );
    // The change output's PPOI context reached the wallet before the order request, keyed by
    // its commitment alone, so chain observation of any settlement submits it.
    {
        let commits = output_pois.commits.lock().unwrap();
        let [(requests, contexts)] = commits.as_slice() else {
            panic!("the change-output contexts are committed once");
        };
        assert_eq!(*requests, 0);
        let [context] = contexts.as_slice() else {
            panic!("one change output");
        };
        assert_eq!(context.output_commitment, change.output_commitment);
        assert!(context.observation.is_none() && context.txid_merkleroot_index.is_none());
    }
    // A fill-or-kill sell order that the executor owns, signs, and receives.
    let order = submitted_order(body);
    assert_eq!(
        (
            order.kind.as_str(),
            order.partiallyFillable,
            order.feeAmount
        ),
        (ORDER_KIND_SELL, false, U256::ZERO)
    );
    assert_eq!((order.receiver, uid.owner()), (executor, executor));
    assert_eq!(order.buyAmount, buy_amount);
    // The 30-minute approval expires 30 minutes after signing, and the record keeps it.
    assert!((signed_from + 1_800..=signed_until + 1_800).contains(&u64::from(order.validTo)));
    assert_eq!(saved.bounds().valid_for_secs, Some(1_800));
    let signature = body["signature"]
        .as_str()
        .unwrap()
        .parse::<Bytes>()
        .unwrap();
    let signature: [u8; 65] = signature[..].try_into().unwrap();
    assert_eq!(
        recover_order_signer(
            &signature,
            &order_digest(&order, 1, swap_profile.settlement())
        )
        .unwrap(),
        executor
    );
    // Both hooks call the executor, and the post-hook's balance guard is the buy amount.
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert!(
        hooks
            .pre
            .iter()
            .chain(&hooks.post)
            .all(|hook| hook.target == executor)
    );
    let post_hook = RelayAdapt7702::multicallCall::abi_decode(&hooks.post[0].call_data).unwrap();
    let guard = transferCall::abi_decode(&post_hook._calls[0].data).unwrap();
    assert_eq!(guard._transfers[0].value, buy_amount);
    // The order and the pre-hook's exact approval both cover what the unshield leaves, so
    // the executor's balance and allowance after the pre-hook match `sellAmount`.
    assert_eq!(order.sellAmount, sell_amount);
    let pre_hook = RelayAdapt7702::executeCall::abi_decode(&hooks.pre[0].call_data).unwrap();
    let approval =
        approveCall::abi_decode(&pre_hook._actionData.calls.last().unwrap().data).unwrap();
    assert_eq!(
        (approval.spender, approval.amount),
        (swap_profile.vault_relayer(), sell_amount)
    );

    // Nothing is signed or sent while the first attempt's pre-hook can still execute, nor
    // after it executed: that order can still fill until `validTo`.
    assert_eq!(
        first().await.unwrap_err().to_string(),
        "the previous order of this swap can still execute; retry once it has ended"
    );
    let executed = SwapOrderObservations {
        pre_hook_executed: Some(SwapObservation {
            block: observed.block(),
            transaction_hash: Some(B256::repeat_byte(0x44)),
        }),
        ..SwapOrderObservations::default()
    };
    store
        .record_swap_observations(operation, uid, executed)
        .unwrap();
    assert_eq!(
        first().await.unwrap_err().to_string(),
        "the previous order of this swap can still execute; retry once it has ended"
    );
    assert_eq!(submissions.lock().unwrap().len(), 2);
    assert_eq!(output_pois.commits.lock().unwrap().len(), 1);

    // Once the pre-hook is dead at its unused nonce, a retry for the same notes and amount
    // keeps the proof: no prover runs, and its change outputs already have PPOI contexts.
    let expired = SwapOrderObservations {
        pre_hook_dead: Some(SwapPreHookDeath {
            cause: SwapPreHookDeathCause::Expired,
            observation: SwapObservation {
                block: observed.block(),
                transaction_hash: None,
            },
        }),
        ..SwapOrderObservations::default()
    };
    store
        .record_swap_observations(operation, uid, expired)
        .unwrap();
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    let digest = record.swap().unwrap().proof().digest();
    let (reused, _) =
        crate::reusable_swap_proof(&record, review.plan(), std::slice::from_ref(&input))
            .expect("same notes and amount reuse the proof");
    assert_eq!(
        alloy::sol_types::SolValue::abi_encode(&reused),
        alloy::sol_types::SolValue::abi_encode(&vec![pre_hook_transaction.clone()])
    );
    // A different amount is proved again.
    let SwapAmountPlan::Fits(smaller) = crate::plan_swap_inputs(
        &builder,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: USDC,
            amount: amount / U256::from(2),
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        swap_profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("half the note fits one order");
    };
    assert!(crate::reusable_swap_proof(&record, &smaller, std::slice::from_ref(&input)).is_none());
    // An expired order can't fill, so the retry's pre-hook invalidates nothing.
    assert_eq!(
        crate::swap_invalidation(&record, &swap_profile, std::time::SystemTime::now()).unwrap(),
        None
    );
    // A real retry starts after `validTo`; here the next second gives it a new deadline.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let SwapOrderOutcome::Submitted { uid: retry } = issue(reused, Vec::new()).await.unwrap()
    else {
        panic!("the retry is submitted");
    };
    assert_ne!(retry, uid);
    let submitted = submissions.lock().unwrap().clone();
    assert_eq!(submitted.len(), 3);
    let hooks = serde_json::from_str::<AppData>(submitted[2].1["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    let pre_hook = RelayAdapt7702::executeCall::abi_decode(&hooks.pre[0].call_data).unwrap();
    assert_eq!(
        pre_hook._actionData.calls.len(),
        2,
        "deadline and approval only"
    );
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(record.swap().unwrap().proof().digest(), digest);
    assert_eq!(record.swap().unwrap().orders().len(), 2);
    assert_eq!(output_pois.commits.lock().unwrap().len(), 1);

    // After expiry, the same account can sign a reviewed order for another pair.
    store
        .record_swap_observations(operation, retry, expired)
        .unwrap();
    let dai = alloy::primitives::address!("6b175474e89094c44da98b954eedeac495271d0f");
    let SwapAmountPlan::Fits(new_plan) = crate::plan_swap_inputs(
        &builder,
        &swap_profile,
        delegated,
        std::slice::from_ref(&input),
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: dai,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        swap_profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("the new pair fits");
    };
    assert!(crate::reusable_swap_proof(&record, &new_plan, std::slice::from_ref(&input)).is_none());
    let mut new_quote = review.quote().clone();
    new_quote.buy_token = dai;
    let new_review = crate::price_swap_review(
        new_plan,
        CowQuote {
            quote: new_quote,
            expiration: String::new(),
            id: None,
            verified: true,
            protocol_fee_bps: None,
        },
        SwapPrice::Unverified,
        U256::from(25),
        U256::from(25),
        50,
        GAS_SHARE_BALANCED_BPS,
        Duration::from_mins(10),
        1,
        U256::ZERO,
        isolation,
    )
    .unwrap();
    // The fixture expires the retry immediately; a real expiry also advances the deadline.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let SwapOrderOutcome::Submitted { uid: new_uid } = owner
        .issue_swap_order(crate::SwapOrderSigning {
            review: &new_review,
            swap_use: crate::vault::SwapUseId::first(new_review.plan().operation().unwrap()),
            private_minimum: new_review.suggested_private_minimum(),
            price_acknowledged: true,
            transactions: vec![pre_hook_transaction],
            inputs: std::slice::from_ref(&input),
            change_output_pois: Vec::new(),
            output_pois: &output_pois,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &anchors,
            token_registry: &tokens,
            bridge: None,
            destination_minimum: None,
            destination: None,
        })
        .await
        .unwrap()
    else {
        panic!("new pair submitted");
    };
    assert_eq!(new_uid.owner(), uid.owner());
    {
        let submitted = submissions.lock().unwrap();
        assert_eq!(submitted_order(&submitted[3].1).buyToken, dai);
        assert_eq!(submitted_order(&submitted[0].1).buyToken, USDC);
    }

    assert_eq!(unexpected_reads.load(Ordering::Relaxed), 0);
    orderbook_task.abort();
    fee_server.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// A new swap is planned, priced, and approved for its reserved executor before any setup is
// paid. The approval outlives the session, and hooks are signed only for a plan made from the
// confirmed delegation.
#[tokio::test]
async fn swap_is_approved_before_setup_but_signed_only_once_delegated() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let delegate = chain.accepted_executor_profile().unwrap().delegate();
    let swap_profile = chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let operation = ExecutorOperationId::random().unwrap();
    let prepared = owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(WETH, USDC, SwapDelivery::Reshield),
            None,
            &password(),
        )
        .await
        .unwrap();
    let reserved = crate::SwapExecutor::reserved(&prepared).unwrap();
    // The setup takes the fresh executor's nonce 0, so the pre-hook is planned at 1.
    assert_eq!(
        (
            reserved.operation(),
            reserved.executor(),
            reserved.delegate()
        ),
        (Some(operation), prepared.context().executor, delegate)
    );
    assert_eq!(reserved.expected_pre_hook_nonce(), U256::ONE);
    assert!(reserved.delegated().is_none());

    let amount = U256::from(1_000_000);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            WETH,
            amount,
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
        relay_adapt_contract: Address::repeat_byte(5),
    };
    let SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
        &builder,
        &swap_profile,
        reserved,
        std::slice::from_ref(&input),
        &SwapAmountRequest {
            sell_token: WETH,
            buy_token: USDC,
            amount,
            delivery: SwapDelivery::Reshield,
            byte_budget: None,
        },
        swap_profile.app_data_byte_budget(),
        None,
    )
    .unwrap() else {
        panic!("one note fits one order before setup");
    };
    assert_eq!(plan.operation(), Some(operation));
    let quote: CowQuote = serde_json::from_value(json!({
        "quote": {
            "sellToken": WETH, "buyToken": USDC, "sellAmount": "999000", "buyAmount": "3000000000",
            "validTo": 1, "feeAmount": "1000", "gasAmount": "0", "gasPrice": "0",
            "sellTokenPrice": "1000000000000", "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let price_at = |shield_fee: u64, unshield_fee: u64, gas_price_wei: u128| {
        crate::price_swap_review(
            plan.clone(),
            quote.clone(),
            SwapPrice::Unverified,
            U256::from(shield_fee),
            U256::from(unshield_fee),
            50,
            GAS_SHARE_BALANCED_BPS,
            Duration::from_mins(10),
            gas_price_wei,
            U256::ZERO,
            isolation,
        )
        .unwrap()
    };
    let price = |shield_fee, unshield_fee| price_at(shield_fee, unshield_fee, 1_000_000_000);
    let review = price(25, 25);
    // At 1 gwei plus 25% and this quote's rate of at least 3,000 USDC/ETH, price the
    // estimated hook gas. The declared limits' margin only caps execution and isn't priced.
    let estimated_gas = plan.hook_gas_estimate();
    let declared_gas = plan.pre_hook_gas_limit() + plan.post_hook_gas_limit().unwrap();
    assert!(estimated_gas < declared_gas);
    assert!(review.gas_estimate() * U256::from(4) >= U256::from(estimated_gas) * U256::from(15));
    assert!(review.gas_estimate() * U256::from(4) < U256::from(declared_gas) * U256::from(15));
    let private_minimum = review.suggested_private_minimum();
    // An unverified price is approved only with the user's acknowledgement.
    assert!(review.approval(private_minimum, false).is_err());
    let approval = review.approval(private_minimum, true).unwrap();
    // The approval binds the gas share it was priced at, and the uncushioned gas price.
    assert_eq!(
        (
            approval.bounds.gas_share_bps,
            approval.bounds.gas_estimate,
            approval.bounds.gas_allowance,
            approval.bounds.gas_price_wei,
            approval.bounds.valid_for_secs,
        ),
        (
            Some(GAS_SHARE_BALANCED_BPS),
            Some(review.gas_estimate()),
            Some(review.gas_allowance()),
            Some(1_000_000_000),
            Some(600),
        )
    );
    // After setup the entered amount is planned again; the order sells it less the fee.
    assert_eq!(
        (approval.bounds.spend_amount(), approval.bounds.sell_amount),
        (amount, U256::from(997_500))
    );
    owner
        .record_swap_approval(
            operation,
            crate::vault::SwapUseId::first(operation),
            approval.clone(),
        )
        .unwrap();
    let restarted = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let record = restarted
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert_eq!(record.swap_approval(), Some(&approval));

    // After setup the fresh review keeps the approval unless a fee or the validity moved, or
    // the minimum or the allowed gas moved beyond a fifth of the approved allowed gas.
    assert_eq!(review.approval_change(&approval), None);
    // A setup approval saved before gas shares needs a full review.
    let mut older = approval.clone();
    older.bounds.gas_share_bps = None;
    older.bounds.gas_estimate = None;
    older.bounds.gas_allowance = None;
    older.bounds.gas_price_wei = None;
    older.bounds.valid_for_secs = None;
    assert_eq!(
        review.approval_change(&older),
        Some(SwapReviewChange::GasShare)
    );
    // Gas 10% higher is signed at the approved minimum; 30% higher needs a new review.
    let pricier = price_at(25, 25, 1_100_000_000);
    assert!(pricier.suggested_private_minimum() < private_minimum);
    assert_eq!(
        pricier.approved_order_minimum(&approval),
        Ok(private_minimum)
    );
    let pricier = price_at(25, 25, 1_300_000_000);
    assert_eq!(
        pricier.approval_change(&approval),
        Some(SwapReviewChange::GasAllowance {
            approved: review.gas_allowance(),
            current: pricier.gas_allowance(),
        })
    );
    let mut longer = approval.clone();
    longer.bounds.valid_for_secs = Some(1_800);
    assert_eq!(
        review.approval_change(&longer),
        Some(SwapReviewChange::Validity {
            approved: 1_800,
            current: 600,
        })
    );
    // A lower gas estimate only raises the minimum, so the approval still holds.
    let cheaper = price_at(25, 25, 500_000_000);
    assert!(cheaper.gas_estimate() < review.gas_estimate());
    assert!(cheaper.suggested_private_minimum() > private_minimum);
    assert_eq!(cheaper.approval_change(&approval), None);

    assert_eq!(
        price(30, 25).approval_change(&approval),
        Some(SwapReviewChange::ShieldFee {
            approved: U256::from(25),
            current: U256::from(30),
        })
    );
    assert_eq!(
        price(25, 30).approval_change(&approval),
        Some(SwapReviewChange::UnshieldFee {
            approved: U256::from(25),
            current: U256::from(30),
        })
    );
    // An approved minimum that the quote misses by the whole allowed gas.
    let mut higher = approval.clone();
    higher.bounds.private_minimum = private_minimum + review.gas_allowance();
    assert_eq!(
        review.approval_change(&higher),
        Some(SwapReviewChange::Minimum {
            approved: private_minimum + review.gas_allowance(),
            current: private_minimum,
        })
    );

    // Nothing is signed, persisted, or sent for a plan made before the delegation.
    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook(
        db.clone(),
        view.clone(),
        operation,
        swap_profile.settlement(),
    )
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        orderbook_url,
        1,
    )
    .unwrap();
    let tokens = crate::settings::EffectiveTokenRegistry {
        tokens: std::collections::BTreeMap::new(),
    };
    let executor = prepared.context().executor;
    let authorization = password();
    let output_pois = OutputPois::default();
    let signed = owner
        .issue_swap_order(crate::SwapOrderSigning {
            review: &review,
            swap_use: crate::vault::SwapUseId::first(review.plan().operation().unwrap()),
            private_minimum,
            price_acknowledged: true,
            transactions: vec![Transaction {
                proof: SnarkProof::default(),
                merkleRoot: B256::ZERO,
                nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
                commitments: vec![B256::ZERO],
                boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
                unshieldPreimage: CommitmentPreimage::empty(),
            }],
            inputs: std::slice::from_ref(&input),
            change_output_pois: Vec::new(),
            output_pois: &output_pois,
            authorization: &authorization,
            orderbook: &orderbook,
            anchor_cache: &crate::TokenAnchorRateCache::new(),
            token_registry: &tokens,
            bridge: None,
            destination_minimum: None,
            destination: None,
        })
        .await;
    assert!(signed.is_err());
    let record = restarted
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == operation)
        .unwrap();
    assert!(record.swap().is_none() && record.issued().is_empty());
    assert!(submissions.lock().unwrap().is_empty());

    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(restarted);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// An External order pays its approved receiver and carries only the pre-hook. The setup's
// approval, with its pair and delivery, binds the first order; later orders on the account may
// choose either delivery kind.
#[tokio::test]
async fn external_order_pays_the_approved_receiver_without_a_post_hook() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let profile = chain.accepted_executor_profile().unwrap();
    let delegate = profile.delegate();
    let swap_profile = chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let approved = SwapDelivery::External {
        receiver: Address::repeat_byte(0x91),
    };
    let receiver = Address::repeat_byte(0x92);
    let moved = SwapDelivery::External { receiver };

    // The approved terms are in the write that creates the record, before its address is bound.
    let reserved = ExecutorOperationId::random().unwrap();
    store
        .reserve_with_swap_approval(
            reserved,
            delegate,
            Some("Private swap"),
            &[crate::ExecutorAsset::Erc20(USDC)],
            Some(setup_approval(USDC, Address::ZERO, approved)),
            None,
        )
        .unwrap();
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == reserved)
        .unwrap();
    assert_eq!(record.swap_approval_tokens(), Some((USDC, Address::ZERO)));
    assert_eq!(
        record.swap_approval().map(|approval| approval.delivery),
        Some(approved)
    );

    let operation = ExecutorOperationId::random().unwrap();
    let executor = owner
        .prepare_swap_setup(
            operation,
            broadcaster(delegate),
            setup_approval(USDC, Address::ZERO, approved),
            None,
            &password(),
        )
        .await
        .unwrap()
        .context()
        .executor;
    let before =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.reconcile(operation, before, &[]).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), before, Vec::new()),
            ),
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    let setup_won = (
        setup,
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(4),
            ExecutorExecutionResult::Executed,
        ),
    );
    let record = store.reconcile(operation, observed, &[setup_won]).unwrap();
    let code = [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        delegate.as_slice(),
    ]
    .concat();
    let SwapSetupStatus::Delegated(delegated) =
        swap_setup_status(&record, observed.block(), &code, profile)
    else {
        panic!("the setup delegated the executor");
    };

    let amount = U256::from(1_000_000);
    let input = Utxo::new(
        broadcaster_core::notes::Note::new_change(
            view.scan_keys().master_public_key,
            USDC,
            amount,
            [7; 16],
        ),
        0,
        0,
        UtxoSource {
            tx_hash: B256::ZERO,
            block_number: 0,
            block_timestamp: 0,
        },
        UtxoCommitmentKind::Shield,
    );
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
        relay_adapt_contract: Address::repeat_byte(5),
    };
    let quote: CowQuote = serde_json::from_value(json!({
        "quote": {
            "sellToken": USDC, "buyToken": BUY_NATIVE_TOKEN, "sellAmount": "997500",
            "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0", "gasAmount": "0",
            "gasPrice": "0", "sellTokenPrice": "1000000000000", "kind": "sell",
            "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    }))
    .unwrap();
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let review_at = |buy_token, delivery, gas_price_wei: u128| {
        let SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
            &builder,
            &swap_profile,
            delegated,
            std::slice::from_ref(&input),
            &SwapAmountRequest {
                sell_token: USDC,
                buy_token,
                amount,
                delivery,
                byte_budget: None,
            },
            swap_profile.app_data_byte_budget(),
            None,
        )
        .unwrap() else {
            panic!("one note fits one order");
        };
        let mut review = crate::price_swap_review(
            plan,
            quote.clone(),
            SwapPrice::Unverified,
            U256::from(25),
            U256::from(25),
            50,
            GAS_SHARE_BALANCED_BPS,
            Duration::from_mins(10),
            gas_price_wei,
            U256::ZERO,
            isolation,
        )
        .unwrap();
        // A Bridge review carries its bridge quote, whose minimum the approval binds.
        if let SwapDelivery::Bridge(bridge) = delivery {
            review.set_bridge_for_tests(&crate::SwapBridgeQuote {
                provider: bridge.provider,
                destination_minimum: U256::from(1_000),
                expected_output: U256::from(1_010),
                fee: Some(U256::ZERO),
                leg: crate::BridgeLegPrice::SameAsset,
                fill_time_sec: None,
                private: None,
            });
        }
        review
    };
    let review = |buy_token, delivery| review_at(buy_token, delivery, 1);
    let first = review(Address::ZERO, approved);
    let changed = review(Address::ZERO, moved);

    // The receiver's address is part of the approved terms; its label is not, so the same
    // address keeps the approval.
    let approval = first
        .approval(first.suggested_private_minimum(), true)
        .unwrap();
    assert_eq!(first.approval_change(&approval), None);
    assert_eq!(
        changed.approval_change(&approval),
        Some(SwapReviewChange::Delivery)
    );
    let mut private = approval.clone();
    private.delivery = SwapDelivery::Reshield;
    assert_eq!(
        first.approval_change(&private),
        Some(SwapReviewChange::Delivery)
    );
    // No post-hook, no shield fee: the receiver's minimum is the buy amount.
    assert_eq!(approval.bounds.post_hook_gas_limit, None);
    assert_eq!(approval.bounds.shield_fee_bps, U256::ZERO);
    assert_eq!(approval.bounds.buy_amount, approval.bounds.private_minimum);

    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook_with_lost_response(
        db.clone(),
        view.clone(),
        operation,
        swap_profile.settlement(),
        true,
    )
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        orderbook_url,
        1,
    )
    .unwrap();
    let transaction = Transaction {
        proof: SnarkProof::default(),
        merkleRoot: B256::ZERO,
        nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
        commitments: vec![B256::ZERO],
        boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
        unshieldPreimage: CommitmentPreimage::empty(),
    };
    let authorization = password();
    let output_pois = OutputPois::default();
    let anchors = crate::TokenAnchorRateCache::new();
    let tokens = crate::settings::EffectiveTokenRegistry {
        tokens: std::collections::BTreeMap::new(),
    };
    macro_rules! issue {
        ($review:expr) => {
            issue!($review, None)
        };
        ($review:expr, $bridge:expr) => {
            issue!($review, $bridge, $review.suggested_private_minimum())
        };
        ($review:expr, $bridge:expr, $private_minimum:expr) => {
            owner
                .issue_swap_order(crate::SwapOrderSigning {
                    review: $review,
                    swap_use: crate::vault::SwapUseId::first($review.plan().operation().unwrap()),
                    private_minimum: $private_minimum,
                    price_acknowledged: true,
                    transactions: vec![transaction.clone()],
                    inputs: std::slice::from_ref(&input),
                    change_output_pois: Vec::new(),
                    output_pois: &output_pois,
                    authorization: &authorization,
                    orderbook: &orderbook,
                    anchor_cache: &anchors,
                    token_registry: &tokens,
                    bridge: $bridge,
                    destination_minimum: $review.bridge().map(|bridge| bridge.destination_minimum),
                    destination: None,
                })
                .await
        };
    }
    let record = || {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == operation)
            .unwrap()
    };

    // The first order must be the one approved with the setup. Another receiver returns to
    // review and another pair is refused, both before anything is signed.
    assert_eq!(
        issue!(&changed).unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::Delivery)
    );
    let dai = alloy::primitives::address!("6b175474e89094c44da98b954eedeac495271d0f");
    let other_pair = review(dai, approved);
    assert!(
        issue!(&other_pair)
            .unwrap_err()
            .to_string()
            .contains("differ from the ones approved")
    );
    let unsigned = record();
    assert!(unsigned.swap().is_none());
    assert_eq!(unsigned.issued().len(), 1);
    assert_eq!(
        unsigned.swap_approval().map(|approval| approval.delivery),
        Some(approved)
    );
    assert!(submissions.lock().unwrap().is_empty());

    // A Bridge approval binds each destination term: a review that differs in any of them
    // needs a full review.
    let bridged = BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 137,
        receiver: Address::repeat_byte(0x93),
        destination_token: Address::repeat_byte(0x94),
        surplus: BridgeSurplus::BridgedByProvider,
        private: None,
    };
    let bridge = review(WETH, SwapDelivery::Bridge(bridged));
    let bridge_approval = bridge
        .approval(bridge.suggested_private_minimum(), true)
        .unwrap();
    assert_eq!(bridge.approval_change(&bridge_approval), None);
    for other_terms in [
        BridgeDelivery {
            provider: BridgeProvider::Across,
            ..bridged
        },
        BridgeDelivery {
            destination_chain: 56,
            ..bridged
        },
        BridgeDelivery {
            destination_token: Address::ZERO,
            ..bridged
        },
        BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..bridged
        },
    ] {
        let mut other = bridge_approval.clone();
        other.delivery = SwapDelivery::Bridge(other_terms);
        assert_eq!(
            bridge.approval_change(&other),
            Some(SwapReviewChange::Delivery)
        );
    }
    // NEAR Intents converts the whole deposit, so it can't reshield surplus.
    let reshielding = review(
        WETH,
        SwapDelivery::Bridge(BridgeDelivery {
            surplus: BridgeSurplus::Reshield,
            ..bridged
        }),
    );
    assert!(
        reshielding
            .approval(reshielding.suggested_private_minimum(), true)
            .is_err()
    );

    // Each protocol contract on the destination chain is refused before anything is signed,
    // even once approved, and so is an order without its bridge route.
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let polygon = chains.get(137).unwrap();
    let clients = owner.swap_bridge_clients(&orderbook).unwrap();
    let near_destination = crate::bridge::BridgeDestination {
        destination_token: bridged.destination_token,
        intermediate: WETH,
        symbol: "DST".to_owned(),
        same_asset: false,
        near: Some(crate::bridge::NearAssets {
            origin_asset: "nep141:weth".to_owned(),
            destination_asset: "nep141:dst".to_owned(),
            origin_decimals: 18,
            destination_decimals: 18,
        }),
    };
    let route = crate::SwapBridgeRoute {
        clients: &clients,
        destination: &near_destination,
        destination_chain: polygon,
    };
    let polygon_railgun = polygon.require_railgun().unwrap().deployment.contract;
    let spoke_pool = polygon.bridge_profile().unwrap().spoke_pool();
    for (receiver, rejection) in [
        (Address::ZERO, BridgeReceiverRejection::ZeroAddress),
        (polygon_railgun, BridgeReceiverRejection::Railgun),
        (spoke_pool, BridgeReceiverRejection::SpokePool),
    ] {
        let refused = review(
            WETH,
            SwapDelivery::Bridge(BridgeDelivery {
                receiver,
                ..bridged
            }),
        );
        owner
            .record_swap_approval(
                operation,
                crate::vault::SwapUseId::first(operation),
                refused
                    .approval(refused.suggested_private_minimum(), true)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            issue!(&refused, Some(route))
                .unwrap_err()
                .downcast_ref::<BridgeReceiverRejection>(),
            Some(&rejection)
        );
    }
    // A quote carries the zero address until a receiver is entered. An External order to it is
    // refused as well.
    let unaddressed = review(
        Address::ZERO,
        SwapDelivery::External {
            receiver: Address::ZERO,
        },
    );
    owner
        .record_swap_approval(
            operation,
            crate::vault::SwapUseId::first(operation),
            unaddressed
                .approval(unaddressed.suggested_private_minimum(), true)
                .unwrap(),
        )
        .unwrap();
    assert_eq!(
        issue!(&unaddressed)
            .unwrap_err()
            .downcast_ref::<SwapReceiverRejection>(),
        Some(&SwapReceiverRejection::ZeroAddress)
    );
    owner
        .record_swap_approval(
            operation,
            crate::vault::SwapUseId::first(operation),
            bridge_approval,
        )
        .unwrap();
    assert!(issue!(&bridge).is_err());
    assert!(record().swap().is_none());
    assert_eq!(record().issued().len(), 1);
    assert!(submissions.lock().unwrap().is_empty());

    // The user approves the new receiver at a gas estimate of an eighth of the quote. The
    // requote after setup finds gas 10% higher, within a fifth of the approved allowed gas, so
    // the order is signed at the approved minimum and persisted. Its submission response is
    // lost, and the resubmission rebuilds the identical order.
    let gas_price_wei = 100_000_000_000_000_000 / u128::from(changed.plan().hook_gas_estimate());
    let reviewed = review_at(Address::ZERO, moved, gas_price_wei);
    let approval = reviewed
        .approval(reviewed.suggested_private_minimum(), true)
        .unwrap();
    let private_minimum = approval.bounds.private_minimum;
    let requote = review_at(Address::ZERO, moved, gas_price_wei + gas_price_wei / 10);
    assert!(requote.suggested_private_minimum() < private_minimum);
    assert_eq!(
        requote.approved_order_minimum(&approval),
        Ok(private_minimum)
    );
    owner
        .record_swap_approval(
            operation,
            crate::vault::SwapUseId::first(operation),
            approval,
        )
        .unwrap();
    assert!(
        issue!(&requote, None, private_minimum)
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let signed = record();
    let saved = signed.swap().unwrap().orders()[0].clone();
    assert_eq!(saved.delivery(), moved);
    assert!(saved.post_hook().is_none());
    // The attempt records the gas its minimum leaves room for, less than the requote's own
    // allowed gas. The setup approval keeps the gas it was approved with.
    let room = requote.gas_allowance_for(private_minimum).unwrap();
    assert!(room < requote.gas_allowance());
    assert_eq!(saved.bounds().gas_allowance, Some(room));
    assert_eq!(
        signed.swap_approval().unwrap().bounds.gas_allowance,
        Some(reviewed.gas_allowance())
    );
    let [_, pre_hook] = signed.issued() else {
        panic!("an External order issues only its pre-hook");
    };
    assert_eq!(pre_hook.purpose(), ExecutorPayloadPurpose::SwapPreHook);
    // The executor never holds the bought asset, so recovery doesn't track it.
    assert_eq!(signed.assets(), &[crate::ExecutorAsset::Erc20(USDC)]);
    assert_eq!(
        owner
            .resubmit_swap_order(operation, &orderbook)
            .await
            .unwrap(),
        SwapOrderOutcome::Submitted { uid: saved.uid() }
    );
    let submitted = submissions.lock().unwrap().clone();
    let [(persisted, body), (_, resent)] = submitted.as_slice() else {
        panic!("one initial request and one resubmission");
    };
    assert!(*persisted);
    assert_eq!(body, resent);
    let order = submitted_order(body);
    assert_eq!(
        (order.receiver, order.buyToken, order.buyAmount),
        (receiver, BUY_NATIVE_TOKEN, private_minimum)
    );
    assert_eq!(order.buyTokenBalance, TOKEN_BALANCE_ERC20);
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert_eq!((hooks.pre.len(), hooks.post.len()), (1, 0));

    // After that order ends, the account can place a Reshield order for another pair.
    let expired = SwapOrderObservations {
        pre_hook_dead: Some(SwapPreHookDeath {
            cause: SwapPreHookDeathCause::Expired,
            observation: SwapObservation {
                block: observed.block(),
                transaction_hash: None,
            },
        }),
        ..SwapOrderObservations::default()
    };
    store
        .record_swap_observations(operation, saved.uid(), expired)
        .unwrap();
    // The fixture expires the order immediately; a real expiry also advances the deadline.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let reshield = review(WETH, SwapDelivery::Reshield);
    assert!(matches!(
        issue!(&reshield).unwrap(),
        SwapOrderOutcome::Submitted { .. }
    ));
    let body = submissions.lock().unwrap()[2].1.clone();
    assert_eq!(submitted_order(&body).receiver, executor);
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert_eq!(hooks.post.len(), 1);
    assert_eq!(
        record().assets(),
        &[
            crate::ExecutorAsset::Erc20(USDC),
            crate::ExecutorAsset::Erc20(WETH)
        ]
    );

    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

const POLYGON_WETH: Address =
    alloy::primitives::address!("7ceB23fD6bC0adD59E62ac25578270cFf1b9f619");

/// Record `operation`'s setup as the winner of nonce 0 and its account as delegated, observed
/// at nonce 1.
pub(super) fn delegate_setup(
    store: &ExecutorStore,
    operation: ExecutorOperationId,
    profile: crate::settings::ExecutorProfile,
) -> crate::DelegatedSwapExecutor {
    let delegate = profile.delegate();
    let before =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    store.reconcile(operation, before, &[]).unwrap();
    let setup = B256::repeat_byte(3);
    store
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                delegate,
                setup,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), before, Vec::new()),
            ),
        )
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(12, B256::repeat_byte(12)), U256::ONE);
    let setup_won = (
        setup,
        ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(4),
            ExecutorExecutionResult::Executed,
        ),
    );
    let record = store.reconcile(operation, observed, &[setup_won]).unwrap();
    let code = [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        delegate.as_slice(),
    ]
    .concat();
    let SwapSetupStatus::Delegated(delegated) =
        swap_setup_status(&record, observed.block(), &code, profile)
    else {
        panic!("the setup delegated the executor");
    };
    delegated
}

/// An account of `owner`'s wallet that an earlier action set up: derived for its index, with
/// its setup the winner of nonce 0 and nothing else signed.
async fn set_up_account(
    owner: &ExecutorOwner,
    store: &ExecutorStore,
    profile: crate::settings::ExecutorProfile,
    candidate: crate::PublicBroadcasterCandidate,
) -> (ExecutorOperationId, crate::DelegatedSwapExecutor) {
    let operation = ExecutorOperationId::random().unwrap();
    owner
        .prepare_operation(
            operation,
            ExecutorDelivery::PublicBroadcaster(Box::new(candidate)),
            &password(),
            &[],
            Some("Private swap"),
        )
        .await
        .unwrap();
    (operation, delegate_setup(store, operation, profile))
}

/// An Across fee quote for `output` whose fill deadline is `fill_after` seconds from now.
fn across_fee_quote(spoke_pool: Address, output: U256, fill_after: u64) -> String {
    let now = unix_now();
    json!({
        "outputAmount": output.to_string(),
        "totalRelayFee": {"pct": "0", "total": "10"},
        "relayerGasFee": {"pct": "0", "total": "0"},
        "lpFee": {"pct": "0", "total": "0"},
        "timestamp": (now - 5).to_string(),
        "fillDeadline": (now + fill_after).to_string(),
        "exclusiveRelayer": Address::repeat_byte(0x77),
        "exclusivityDeadline": 3,
        "spokePoolAddress": spoke_pool,
        "destinationSpokePoolAddress": Address::repeat_byte(0x78),
        "isAmountTooLow": false,
        "limits": {"minDeposit": "1", "maxDeposit": "1000000000000000000000"},
        "estimatedFillTimeSec": 2
    })
    .to_string()
}

/// A delegated swap account from USDC to WETH approved for `delivery`, with one spendable
/// note, and the reviewed first order with a bridge quote whose destination minimum is 1,000.
struct BridgeOrderFixture {
    operation: ExecutorOperationId,
    executor: Address,
    input: Utxo,
    transaction: Transaction,
    review: crate::SwapReview,
}

impl BridgeOrderFixture {
    /// The setup wins nonce 0, so the pre-hook signs at 1 and any post-hook at 2.
    async fn new(
        owner: &ExecutorOwner,
        store: &ExecutorStore,
        view: &DesktopViewSession,
        profile: crate::settings::ExecutorProfile,
        swap_profile: &crate::settings::SwapProfile,
        delivery: BridgeDelivery,
    ) -> Self {
        let operation = ExecutorOperationId::random().unwrap();
        let executor = owner
            .prepare_swap_setup(
                operation,
                broadcaster(profile.delegate()),
                setup_approval(USDC, WETH, SwapDelivery::Bridge(delivery)),
                None,
                &password(),
            )
            .await
            .unwrap()
            .context()
            .executor;
        Self::after_setup(
            store,
            view,
            profile,
            swap_profile,
            operation,
            executor,
            delivery,
        )
    }

    /// The fixture for `operation`, whose setup for `delivery` is prepared for `executor`.
    fn after_setup(
        store: &ExecutorStore,
        view: &DesktopViewSession,
        profile: crate::settings::ExecutorProfile,
        swap_profile: &crate::settings::SwapProfile,
        operation: ExecutorOperationId,
        executor: Address,
        delivery: BridgeDelivery,
    ) -> Self {
        let delegated = delegate_setup(store, operation, profile);
        Self::planned(view, swap_profile, delegated, executor, delivery)
    }

    /// The fixture for the account `delegated` confirms at `executor`, approved for `delivery`.
    fn planned(
        view: &DesktopViewSession,
        swap_profile: &crate::settings::SwapProfile,
        delegated: crate::DelegatedSwapExecutor,
        executor: Address,
        delivery: BridgeDelivery,
    ) -> Self {
        let operation = delegated.operation();
        let amount = U256::from(1_000_000);
        let input = Utxo::new(
            broadcaster_core::notes::Note::new_change(
                view.scan_keys().master_public_key,
                USDC,
                amount,
                [7; 16],
            ),
            0,
            0,
            UtxoSource {
                tx_hash: B256::ZERO,
                block_number: 0,
                block_timestamp: 0,
            },
            UtxoCommitmentKind::Shield,
        );
        let builder = railgun_wallet::TransactionBuilder {
            chain_type: 0,
            chain_id: 1,
            railgun_contract: Address::repeat_byte(4),
            relay_adapt_contract: Address::repeat_byte(5),
        };
        let SwapAmountPlan::Fits(plan) = crate::plan_swap_inputs(
            &builder,
            swap_profile,
            delegated,
            std::slice::from_ref(&input),
            &SwapAmountRequest {
                sell_token: USDC,
                buy_token: WETH,
                amount,
                delivery: SwapDelivery::Bridge(delivery),
                byte_budget: None,
            },
            swap_profile.app_data_byte_budget(),
            None,
        )
        .unwrap() else {
            panic!("one note fits one order");
        };
        let review = Self::review_at(plan, swap_profile, delivery.provider, 1, 1_000);
        let transaction = Transaction {
            proof: SnarkProof::default(),
            merkleRoot: B256::ZERO,
            nullifiers: vec![B256::from(input.nullifier(view.scan_keys().nullifying_key))],
            commitments: vec![B256::ZERO],
            boundParams: BoundParams::new_transact(0, 0, 1, Vec::new(), executor, B256::ZERO),
            unshieldPreimage: CommitmentPreimage::empty(),
        };
        Self {
            operation,
            executor,
            input,
            transaction,
            review,
        }
    }

    /// `plan`'s review at `gas_price_wei`, with a bridge quote whose destination minimum is
    /// `destination_minimum`.
    fn review_at(
        plan: crate::SwapInputPlan,
        swap_profile: &crate::settings::SwapProfile,
        provider: BridgeProvider,
        gas_price_wei: u128,
        destination_minimum: u64,
    ) -> crate::SwapReview {
        let quote: CowQuote = serde_json::from_value(json!({
            "quote": {
                "sellToken": USDC, "buyToken": WETH, "sellAmount": "997500",
                "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0",
                "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": "1000000000000",
                "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }))
        .unwrap();
        let mut review = crate::price_swap_review(
            plan,
            quote,
            SwapPrice::Unverified,
            U256::from(25),
            U256::from(25),
            50,
            GAS_SHARE_BALANCED_BPS,
            // As a reviewed Bridge order, whatever validity was requested.
            swap_profile.valid_to_window(),
            gas_price_wei,
            U256::ZERO,
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        )
        .unwrap();
        review.set_bridge_for_tests(&crate::SwapBridgeQuote {
            provider,
            destination_minimum: U256::from(destination_minimum),
            expected_output: U256::from(destination_minimum + 10),
            fee: Some(U256::ZERO),
            leg: crate::BridgeLegPrice::SameAsset,
            fill_time_sec: None,
            private: None,
        });
        review
    }

    /// Sign the reviewed order over `route` for the approved minimums.
    async fn issue(
        &self,
        owner: &ExecutorOwner,
        orderbook: &CowOrderbookClient,
        route: crate::SwapBridgeRoute<'_>,
    ) -> eyre::Result<SwapOrderOutcome> {
        self.issue_with(
            owner,
            orderbook,
            route,
            &self.review,
            self.review.suggested_private_minimum(),
            None,
        )
        .await
    }

    /// Sign `review` over `route` for `private_minimum` and the approved destination minimum,
    /// with a private delivery's confirmed `destination` account.
    async fn issue_with(
        &self,
        owner: &ExecutorOwner,
        orderbook: &CowOrderbookClient,
        route: crate::SwapBridgeRoute<'_>,
        review: &crate::SwapReview,
        private_minimum: U256,
        destination: Option<crate::SwapDestinationSigning<'_>>,
    ) -> eyre::Result<SwapOrderOutcome> {
        owner
            .issue_swap_order(crate::SwapOrderSigning {
                review,
                swap_use: crate::vault::SwapUseId::first(review.plan().operation().unwrap()),
                private_minimum,
                price_acknowledged: true,
                transactions: vec![self.transaction.clone()],
                inputs: std::slice::from_ref(&self.input),
                change_output_pois: Vec::new(),
                output_pois: &OutputPois::default(),
                authorization: &password(),
                orderbook,
                anchor_cache: &crate::TokenAnchorRateCache::new(),
                token_registry: &crate::settings::EffectiveTokenRegistry {
                    tokens: std::collections::BTreeMap::new(),
                },
                bridge: Some(route),
                destination_minimum: Some(U256::from(1_000)),
                destination,
            })
            .await
    }

    fn record(&self, store: &ExecutorStore) -> ExecutorRecord {
        store
            .records()
            .unwrap()
            .into_iter()
            .find(|record| record.operation() == self.operation)
            .unwrap()
    }

    /// Nothing was signed, persisted or sent.
    fn assert_unsigned(&self, store: &ExecutorStore, submissions: &Submissions) {
        let record = self.record(store);
        assert!(record.swap().is_none());
        assert_eq!(record.issued().len(), 1, "only the setup is issued");
        assert!(submissions.lock().unwrap().is_empty());
    }
}

// An Across order pays the executor. Its post-hook deposits exactly the buy amount for the
// approved destination minimum on the terms of a quote fetched while signing, and shields any
// surplus. A quote below that minimum, or with a fill deadline the SpokePool could refuse,
// stops signing.
#[tokio::test]
async fn across_order_deposits_the_approved_terms_in_its_post_hook() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let profile = chain.accepted_executor_profile().unwrap();
    let swap_profile = chain.swap_profile().unwrap();
    let spoke_pool = chain.bridge_profile().unwrap().spoke_pool();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let receiver = Address::repeat_byte(0x93);
    let delivery = BridgeDelivery {
        provider: BridgeProvider::Across,
        destination_chain: 137,
        receiver,
        destination_token: POLYGON_WETH,
        surplus: BridgeSurplus::Reshield,
        private: None,
    };
    let fixture =
        BridgeOrderFixture::new(&owner, &store, &view, profile, &swap_profile, delivery).await;
    let executor = fixture.executor;

    // The quote's output and fill deadline, in seconds after the request.
    let quoted = Arc::new(Mutex::new((U256::from(999), 3 * 60 * 60)));
    let answer = quoted.clone();
    let (across_url, across_requests, across_task) = spawn_bridge_stub(move |_| {
        let (output, fill_after) = *answer.lock().unwrap();
        across_fee_quote(spoke_pool, output, fill_after)
    })
    .await;
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook_with_lost_response(
        db.clone(),
        view.clone(),
        fixture.operation,
        swap_profile.settlement(),
        true,
    )
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        orderbook_url,
        1,
    )
    .unwrap();
    let mut clients = owner.swap_bridge_clients(&orderbook).unwrap();
    clients.across = crate::bridge::AcrossClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        across_url,
    )
    .unwrap();
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let destination = crate::bridge::BridgeDestination {
        destination_token: POLYGON_WETH,
        intermediate: WETH,
        symbol: "WETH".to_owned(),
        same_asset: true,
        near: None,
    };
    let route = crate::SwapBridgeRoute {
        clients: &clients,
        destination: &destination,
        destination_chain: chains.get(137).unwrap(),
    };

    // A Bridge review keeps the profile's validity even when another is requested, since its
    // bridge leg is quoted for that window.
    let (quote_url, quotes, quote_task) = spawn_quote_stub(
        json!({
            "quote": {
                "sellToken": USDC, "buyToken": WETH, "sellAmount": "997500",
                "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0",
                "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": "1000000000000",
                "kind": "sell", "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        }),
        None,
    )
    .await;
    let window = swap_profile.valid_to_window().as_secs();
    let quoted_from = unix_now();
    let reviewed = owner
        .review_swap(SwapReviewRequest {
            plan: fixture.review.plan().clone(),
            slippage_bps: 50,
            gas_share_bps: GAS_SHARE_BALANCED_BPS,
            valid_for: Duration::from_mins(30),
            orderbook: &CowOrderbookClient::new(
                OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
                quote_url,
                1,
            )
            .unwrap(),
            anchor_cache: None,
            token_registry: &crate::settings::EffectiveTokenRegistry {
                tokens: std::collections::BTreeMap::new(),
            },
            bridge: Some(route),
        })
        .await
        .unwrap();
    let quoted_until = unix_now();
    quote_task.abort();
    assert_eq!(reviewed.valid_for(), swap_profile.valid_to_window());
    let quote_valid_to = quotes.lock().unwrap()[0]["validTo"].as_u64().unwrap();
    assert!((quoted_from + window..=quoted_until + window).contains(&quote_valid_to));

    // Another gas share prices the held quote again and asks only Across, for the new buy
    // amount. The orderbook stub is gone, so a `CoW` request would fail.
    *quoted.lock().unwrap() = (U256::from(1_234), 3 * 60 * 60);
    let requoted = owner
        .requote_swap_bridge(
            &reviewed,
            crate::cow::GAS_SHARE_TIGHT_BPS,
            route,
            None,
            &crate::settings::EffectiveTokenRegistry {
                tokens: std::collections::BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    *quoted.lock().unwrap() = (U256::from(999), 3 * 60 * 60);
    let requoted_amount = requoted.suggested_private_minimum();
    assert_ne!(requoted_amount, reviewed.suggested_private_minimum());
    let (path, _) = across_requests.lock().unwrap().last().cloned().unwrap();
    assert!(
        path.contains(&format!("amount={requoted_amount}")),
        "{path}"
    );
    assert_eq!(requoted.gas_share_bps(), crate::cow::GAS_SHARE_TIGHT_BPS);
    assert_eq!(
        requoted.bridge().unwrap().destination_minimum,
        U256::from(1_234)
    );
    assert_eq!(
        (requoted.quote(), requoted.valid_for()),
        (reviewed.quote(), reviewed.valid_for())
    );
    assert_eq!(quotes.lock().unwrap().len(), 1);

    assert_eq!(
        fixture.issue(&owner, &orderbook, route).await.unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(999),
        })
    );
    fixture.assert_unsigned(&store, &submissions);
    // A deadline that leaves relayers too little time after the order expires.
    *quoted.lock().unwrap() = (U256::from(1_005), 60);
    let error = fixture.issue(&owner, &orderbook, route).await.unwrap_err();
    assert!(
        error.to_string().contains("Across's quote can't be used"),
        "{error:#}"
    );
    fixture.assert_unsigned(&store, &submissions);

    // The approval allows gas of about 3% of the quote. The requote after setup finds gas 1%
    // higher and an Across output of 999 for its own deposit. Raising the deposit to deliver
    // 1,000 again costs less than a fifth of the approved allowed gas, so no review is needed.
    let plan = fixture.review.plan().clone();
    let gas_price_wei = 100_000_000_000_000_000 / u128::from(plan.hook_gas_estimate());
    let reviewed = BridgeOrderFixture::review_at(
        plan.clone(),
        &swap_profile,
        delivery.provider,
        gas_price_wei,
        1_000,
    );
    let approval = reviewed
        .approval(reviewed.suggested_private_minimum(), true)
        .unwrap();
    let requote = BridgeOrderFixture::review_at(
        plan,
        &swap_profile,
        delivery.provider,
        gas_price_wei + gas_price_wei / 100,
        999,
    );
    let buy_amount = requote.approved_order_minimum(&approval).unwrap();
    assert!(buy_amount > approval.bounds.private_minimum);

    // The order is persisted with the quote's terms and sent. Its response is lost, and the
    // resubmission rebuilds the identical order.
    *quoted.lock().unwrap() = (U256::from(1_005), 3 * 60 * 60);
    let signed_from = unix_now();
    assert!(
        fixture
            .issue_with(&owner, &orderbook, route, &requote, buy_amount, None)
            .await
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let signed_until = unix_now();
    let (path, _) = across_requests.lock().unwrap().last().cloned().unwrap();
    assert!(path.contains(&format!("amount={buy_amount}")), "{path}");
    let saved = fixture.record(&store).swap().unwrap().orders()[0].clone();
    let Some(BridgeOrderTerms::Across(terms)) = saved.bridge().cloned() else {
        panic!("an Across order keeps its deposit terms");
    };
    assert_eq!(
        terms,
        AcrossOrderTerms {
            spoke_pool,
            input_token: WETH,
            output_token: POLYGON_WETH,
            input_amount: buy_amount,
            // The approved minimum, not the quote's higher output.
            output_amount: U256::from(1_000),
            exclusive_relayer: Address::repeat_byte(0x77),
            exclusivity_parameter: 3,
            ..terms
        }
    );
    assert_eq!(saved.bounds().destination_minimum, Some(U256::from(1_000)));
    assert_eq!(saved.bounds().buy_amount, buy_amount);
    assert_eq!(
        owner
            .resubmit_swap_order(fixture.operation, &orderbook)
            .await
            .unwrap(),
        SwapOrderOutcome::Submitted { uid: saved.uid() }
    );
    let submitted = submissions.lock().unwrap().clone();
    let [(persisted, body), (_, resent)] = submitted.as_slice() else {
        panic!("one initial request and one resubmission");
    };
    assert!(*persisted, "the terms are durable before the order request");
    assert_eq!(body, resent);
    assert_eq!(submitted_order(body).receiver, executor);
    // The Bridge order is valid for the profile's window after signing.
    assert!(
        (signed_from + window..=signed_until + window)
            .contains(&u64::from(submitted_order(body).validTo))
    );

    // The post-hook guards, approves and deposits the buy amount, then shields the surplus.
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    let post_hook = RelayAdapt7702::multicallCall::abi_decode(&hooks.post[0].call_data).unwrap();
    let calls = post_hook._calls;
    assert_eq!(
        calls.iter().map(|call| call.to).collect::<Vec<_>>(),
        [executor, WETH, spoke_pool, executor]
    );
    let guard = transferCall::abi_decode(&calls[0].data).unwrap();
    assert_eq!(guard._transfers[0].value, buy_amount);
    let approval = approveCall::abi_decode(&calls[1].data).unwrap();
    assert_eq!(
        (approval.spender, approval.amount),
        (spoke_pool, buy_amount)
    );
    let deposit =
        broadcaster_core::contracts::across::SpokePool::depositV3Call::abi_decode(&calls[2].data)
            .unwrap();
    assert_eq!((deposit.depositor, deposit.recipient), (executor, receiver));
    assert_eq!(
        (
            deposit.inputToken,
            deposit.outputToken,
            deposit.destinationChainId
        ),
        (WETH, POLYGON_WETH, U256::from(137))
    );
    // The raised deposit still pays exactly the approved destination minimum.
    assert_eq!(
        (deposit.inputAmount, deposit.outputAmount),
        (buy_amount, U256::from(1_000))
    );
    assert_eq!(
        (
            deposit.quoteTimestamp,
            deposit.fillDeadline,
            deposit.exclusiveRelayer,
            deposit.exclusivityParameter
        ),
        (
            terms.quote_timestamp,
            terms.fill_deadline,
            terms.exclusive_relayer,
            terms.exclusivity_parameter
        )
    );
    assert!(deposit.message.is_empty());
    let shield =
        broadcaster_core::contracts::railgun::shieldCall::abi_decode(&calls[3].data).unwrap();
    assert_eq!(shield._shieldRequests[0].preimage.token.tokenAddress, WETH);

    across_task.abort();
    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

// A private Across order's destination stealth account signs a guarded shield on the
// destination chain first. That payload is in its record before the signing-time quote carries
// the handler message out of the wallet and before the swap's own account signs anything. The
// post-hook's deposit then pays the handler with that message, whose fallback follows the
// failure choice. The preview quote names no recipient, message or account.
//
// Each account is new, and set up by this swap, or existing. The first order is signed for the
// accounts the approval binds, each at its current nonce, whichever of them needed setup.
// Once the swap is delivered, both accounts can serve another swap. The destination account is
// then judged again before it signs for that swap: its old shields' nonce must be consumed, its
// earlier shield's note accepted or spent, and its balance of the receiving token zero.
#[tokio::test]
async fn private_across_order_records_the_destination_shield_before_its_deposit_is_signed() {
    use alloy::sol_types::SolValue as _;

    for (source_new, destination_new, on_shield_failure) in [
        (true, true, BridgeShieldFailure::RefundOnOrigin),
        (true, true, BridgeShieldFailure::KeepOnDestination),
        (false, true, BridgeShieldFailure::RefundOnOrigin),
        (true, false, BridgeShieldFailure::KeepOnDestination),
        (false, false, BridgeShieldFailure::RefundOnOrigin),
    ] {
        let rpc = Rpc::start().await;
        let (root, db, vault) = desktop_store_with_vault();
        let view = Arc::new(import_wallet_with_metadata(
            &vault,
            TEST_WALLET_ID,
            "Wallet",
        ));
        let (origin_chain, destination_chain) = (chain(&rpc), destination_chain_config(&rpc).await);
        let profile = origin_chain.accepted_executor_profile().unwrap();
        let destination_profile = destination_chain.accepted_executor_profile().unwrap();
        let swap_profile = origin_chain.swap_profile().unwrap();
        let spoke_pool = origin_chain.bridge_profile().unwrap().spoke_pool();
        let handler = destination_chain
            .bridge_profile()
            .unwrap()
            .multicall_handler();
        let [owner, destination_owner] = [origin_chain, destination_chain.clone()].map(|chain| {
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                chain,
                HttpContext::direct_for_tests(),
            )
            .unwrap()
        });
        let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
        let destination_store =
            ExecutorStore::new(db.clone(), view.clone(), DESTINATION_CHAIN).unwrap();

        // Both accounts are claimed for the swap, each as a new account or as one an earlier
        // action set up, and the approval saved with the swap names the destination account as
        // receiver.
        let authorization = password();
        let destination_authorization = authorization.for_destination().unwrap();
        let mut destination_candidate = broadcaster(destination_profile.delegate());
        destination_candidate.chain_id = DESTINATION_CHAIN;
        let (operation, existing_source) = if source_new {
            (ExecutorOperationId::random().unwrap(), None)
        } else {
            let candidate = broadcaster(profile.delegate());
            let (operation, delegated) = set_up_account(&owner, &store, profile, candidate).await;
            (operation, Some(delegated))
        };
        let (destination_operation, existing_destination) = if destination_new {
            (ExecutorOperationId::random().unwrap(), None)
        } else {
            let (operation, delegated) = set_up_account(
                &destination_owner,
                &destination_store,
                destination_profile,
                destination_candidate.clone(),
            )
            .await;
            (operation, Some(delegated))
        };
        let choice = |new, operation| {
            if new {
                crate::vault::SwapAccountChoice::New(operation)
            } else {
                crate::vault::SwapAccountChoice::Existing(operation)
            }
        };
        let mut approval = setup_approval(
            USDC,
            WETH,
            SwapDelivery::Bridge(BridgeDelivery {
                provider: BridgeProvider::Across,
                destination_chain: DESTINATION_CHAIN,
                receiver: Address::ZERO,
                destination_token: POLYGON_WETH,
                surplus: BridgeSurplus::KeepInAccount,
                private: Some(BridgePrivateDelivery { on_shield_failure }),
            }),
        );
        // Only a side that needs setup has a setup fee limit.
        let setup_fee = |new: bool| new.then_some(U256::from(1_000));
        approval.bounds.source_setup_fee = setup_fee(source_new);
        approval.bounds.destination_setup_fee = setup_fee(destination_new);
        approval.bounds.destination_shield_fee_bps = Some(crate::RAILGUN_PROTOCOL_FEE_BPS);
        let prepared = prepare_swap_pair(
            &owner,
            Some(&destination_owner),
            SwapPairPreparation {
                use_id: crate::vault::SwapUseId::first(operation),
                source: choice(source_new, operation),
                destination: Some(choice(destination_new, destination_operation)),
                approval,
                candidate: source_new.then(|| broadcaster(profile.delegate())),
                destination_candidate: destination_new.then_some(destination_candidate),
                authorization: &authorization,
                destination_authorization: Some(&destination_authorization),
            },
        )
        .await
        .unwrap();
        let SwapDelivery::Bridge(delivery) = prepared.approval.delivery else {
            panic!("the approval keeps its Bridge delivery");
        };
        let destination_executor = prepared.destination.as_ref().unwrap().executor();
        assert_eq!(delivery.receiver, destination_executor);
        assert_eq!(
            (
                prepared.origin.requires_setup(),
                prepared.destination.as_ref().unwrap().requires_setup()
            ),
            (source_new, destination_new)
        );
        let destination_record = || {
            destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == destination_operation)
                .unwrap()
        };

        // Before its setup is confirmed, a new destination account can't back an order.
        if destination_new {
            let error = destination_owner
                .delegated_swap_destination(
                    destination_operation,
                    9,
                    1,
                    operation,
                    crate::vault::SwapUseId::first(operation),
                    destination_executor,
                    None,
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("destination network"),
                "{error:#}"
            );
            assert!(destination_record().issued().is_empty());
        }

        // A new account's setup is confirmed now. An existing account keeps the one it had.
        let source_delegated =
            existing_source.unwrap_or_else(|| delegate_setup(&store, operation, profile));
        let fixture = BridgeOrderFixture::planned(
            &view,
            &swap_profile,
            source_delegated,
            prepared.origin.executor(),
            delivery,
        );
        let executor = fixture.executor;
        let delegated = existing_destination.unwrap_or_else(|| {
            delegate_setup(
                &destination_store,
                destination_operation,
                destination_profile,
            )
        });
        // A reused destination's balance of the receiving token is read from the chain before
        // each of its shields, so the chain holds its delegation and nonce.
        if !destination_new {
            rpc.set_delegated_account(
                destination_executor,
                destination_profile.delegate(),
                delegated.observed().nonce(),
            );
        }
        // A completed destination setup still belongs to its origin swap, even before any
        // shield has been issued. It must not be offered or previewed for an unrelated swap.
        assert!(
            destination_owner
                .swap_account_candidates()
                .unwrap()
                .is_empty()
        );
        assert!(
            destination_owner
                .swap_order_preview(destination_operation, true)
                .is_err()
        );
        let destination = crate::SwapDestinationSigning {
            owner: &destination_owner,
            delegated,
            authorization: &destination_authorization,
            notes: None,
        };
        let mut review = fixture.review.clone();
        review.set_bridge_for_tests(&crate::SwapBridgeQuote {
            provider: BridgeProvider::Across,
            destination_minimum: U256::from(1_000),
            expected_output: U256::from(1_000),
            fee: Some(U256::ZERO),
            leg: crate::BridgeLegPrice::SameAsset,
            fill_time_sec: None,
            private: Some(crate::SwapPrivateBridgeQuote {
                quoted_output: U256::from(1_050),
                delivery_allowance: U256::from(50),
                destination_shield_fee_bps: crate::RAILGUN_PROTOCOL_FEE_BPS,
            }),
        });

        // The quote's output, and whether Across's simulation of the message fails. For each
        // request with a message, the stub records whether the destination account's payload
        // in that message was already in its record, and whether the swap's own account had
        // still signed nothing.
        let quoted = Arc::new(Mutex::new((U256::from(1_005), false)));
        let answer = quoted.clone();
        let durable = Arc::new(Mutex::new(Vec::new()));
        let observed = durable.clone();
        // A destination owner that the stub closes while it answers the quote.
        let ending = Arc::new(Mutex::new(None::<Arc<ExecutorOwner>>));
        let ends = ending.clone();
        let (stub_db, stub_view) = (db.clone(), view.clone());
        let (across_url, across_requests, across_task) = spawn_bridge_stub_with(move |path, _| {
            let url = url::Url::parse(&format!("http://across{path}")).unwrap();
            if let Some((_, message)) = url.query_pairs().find(|(key, _)| key == "message") {
                let message = message.parse::<Bytes>().unwrap();
                let shield = MulticallHandler::Instructions::abi_decode(&message)
                    .unwrap()
                    .calls
                    .swap_remove(1)
                    .callData;
                let issued = |chain_id, operation| {
                    ExecutorStore::new(stub_db.clone(), stub_view.clone(), chain_id)
                        .unwrap()
                        .records()
                        .unwrap()
                        .into_iter()
                        .find(|record| record.operation() == operation)
                        .unwrap()
                        .issued()
                        .to_vec()
                };
                observed.lock().unwrap().push((
                    issued(DESTINATION_CHAIN, destination_operation)
                        .iter()
                        .any(|payload| {
                            payload.purpose() == ExecutorPayloadPurpose::SwapDestinationShield
                                && *payload.context().calldata() == shield
                        }),
                    issued(1, operation).len() == 1,
                ));
            }
            if let Some(owner) = ends.lock().unwrap().take() {
                owner.close();
            }
            let (output, simulation_fails) = *answer.lock().unwrap();
            if simulation_fails {
                let error = json!({
                    "type": "AcrossApiError", "code": "SIMULATION_ERROR", "status": 400,
                    "message": "execution reverted"
                });
                (400, error.to_string())
            } else {
                (200, across_fee_quote(spoke_pool, output, 3 * 60 * 60))
            }
        })
        .await;
        let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
        let (orderbook_url, submissions, orderbook_task) = spawn_orderbook(
            db.clone(),
            view.clone(),
            operation,
            swap_profile.settlement(),
        )
        .await;
        let orderbook = CowOrderbookClient::new(
            OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
            orderbook_url,
            1,
        )
        .unwrap();
        let mut clients = owner.swap_bridge_clients(&orderbook).unwrap();
        clients.across = crate::bridge::AcrossClient::new(
            OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
            across_url,
        )
        .unwrap();
        let bridge_destination = crate::bridge::BridgeDestination {
            destination_token: POLYGON_WETH,
            intermediate: WETH,
            symbol: "WETH".to_owned(),
            same_asset: true,
            near: None,
        };
        let route = crate::SwapBridgeRoute {
            clients: &clients,
            destination: &bridge_destination,
            destination_chain: &destination_chain,
        };
        let tokens = crate::settings::EffectiveTokenRegistry {
            tokens: std::collections::BTreeMap::new(),
        };

        // The preview asks Across with the tokens, chains and amount only. Without a cached
        // anchor the delivery allowance is scaled from that quote, so the destination chain's
        // RPC isn't read.
        let (quote_url, _, quote_task) = spawn_quote_stub(
            json!({
                "quote": {
                    "sellToken": USDC, "buyToken": WETH, "sellAmount": "997500",
                    "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0",
                    "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": "1000000000000",
                    "kind": "sell", "partiallyFillable": false
                },
                "expiration": "", "id": 7, "verified": true
            }),
            None,
        )
        .await;
        let previewed = owner
            .review_swap(SwapReviewRequest {
                plan: review.plan().clone(),
                slippage_bps: 50,
                gas_share_bps: GAS_SHARE_BALANCED_BPS,
                valid_for: Duration::from_mins(30),
                orderbook: &CowOrderbookClient::new(
                    OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
                    quote_url,
                    1,
                )
                .unwrap(),
                anchor_cache: None,
                token_registry: &tokens,
                bridge: Some(route),
            })
            .await
            .unwrap();
        quote_task.abort();
        assert!(previewed.bridge().unwrap().private.is_some());
        let (path, _) = across_requests.lock().unwrap().last().cloned().unwrap();
        let path = path.to_lowercase();
        assert!(
            !path.contains("recipient") && !path.contains("message"),
            "{path}"
        );
        for address in [executor, destination_executor, handler] {
            assert!(!path.contains(&format!("{address:x}")), "{path}");
        }
        assert!(durable.lock().unwrap().is_empty());

        // A private order without its destination account is refused.
        assert!(
            fixture
                .issue_with(
                    &owner,
                    &orderbook,
                    route,
                    &review,
                    review.suggested_private_minimum(),
                    None,
                )
                .await
                .is_err()
        );
        fixture.assert_unsigned(&store, &submissions);
        assert_eq!(destination_record().issued().len(), 1);

        // A signing-time quote below the approved destination minimum returns to review, and a
        // message that fails Across's simulation is an error. Neither signs for the swap's own
        // account.
        let issue = || {
            fixture.issue_with(
                &owner,
                &orderbook,
                route,
                &review,
                review.suggested_private_minimum(),
                Some(destination),
            )
        };
        // The first order is signed for the accounts its approval binds. Another setup need or
        // another account returns to review, and neither account signs. An approved new account
        // that has no address yet takes its derived one, which is no change.
        let bound = prepared.approval.accounts.unwrap();
        let mut resetup = bound;
        resetup.source.setup = !source_new;
        let mut replaced = bound;
        replaced.destination = Some(crate::vault::SwapApprovedAccount {
            address: Some(Address::repeat_byte(0xee)),
            setup: destination_new,
        });
        let mut unresolved = bound;
        if source_new {
            unresolved.source.address = None;
        }
        if destination_new {
            unresolved.destination = Some(crate::vault::SwapApprovedAccount {
                address: None,
                setup: true,
            });
        }
        for (accounts, changed) in [(resetup, true), (replaced, true), (unresolved, false)] {
            let approval = SwapApproval {
                accounts: Some(accounts),
                ..prepared.approval.clone()
            };
            owner
                .record_swap_approval(
                    operation,
                    crate::vault::SwapUseId::first(operation),
                    approval,
                )
                .unwrap();
            if changed {
                assert_eq!(
                    issue().await.unwrap(),
                    SwapOrderOutcome::ReviewRequired(SwapReviewChange::Accounts)
                );
                fixture.assert_unsigned(&store, &submissions);
                assert_eq!(destination_record().issued().len(), 1);
            }
        }

        *quoted.lock().unwrap() = (U256::from(999), false);
        assert_eq!(
            issue().await.unwrap(),
            SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
                approved: U256::from(1_000),
                current: U256::from(999),
            })
        );
        fixture.assert_unsigned(&store, &submissions);
        *quoted.lock().unwrap() = (U256::from(1_005), true);
        let error = issue().await.unwrap_err();
        assert!(
            error.to_string().contains("fails in its simulation"),
            "{error:#}"
        );
        fixture.assert_unsigned(&store, &submissions);

        // The destination session is needed until the order is signed. One that ends while the
        // quote is pending stops the order.
        *quoted.lock().unwrap() = (U256::from(1_005), false);
        let ended = Arc::new(
            ExecutorOwner::new(
                0,
                db.clone(),
                view.clone(),
                destination_chain.clone(),
                HttpContext::direct_for_tests(),
            )
            .unwrap(),
        );
        *ending.lock().unwrap() = Some(ended.clone());
        assert!(
            fixture
                .issue_with(
                    &owner,
                    &orderbook,
                    route,
                    &review,
                    review.suggested_private_minimum(),
                    Some(crate::SwapDestinationSigning {
                        owner: &ended,
                        ..destination
                    }),
                )
                .await
                .is_err(),
            "the ended destination session stops the order"
        );
        fixture.assert_unsigned(&store, &submissions);

        let SwapOrderOutcome::Submitted { uid } = issue().await.unwrap() else {
            panic!("the order is submitted");
        };
        assert_eq!(
            *durable.lock().unwrap(),
            [(true, true); 4],
            "each message's shield payload is durable before the request, and nothing of the swap's own account is signed yet"
        );

        // The post-hook guards, approves and deposits for the handler, with the message.
        let submitted = submissions.lock().unwrap().clone();
        let [(persisted, body)] = submitted.as_slice() else {
            panic!("one order request");
        };
        assert!(*persisted, "the terms are durable before the order request");
        let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
            .unwrap()
            .metadata
            .hooks;
        let calls = RelayAdapt7702::multicallCall::abi_decode(&hooks.post[0].call_data)
            .unwrap()
            ._calls;
        assert_eq!(
            calls.iter().map(|call| call.to).collect::<Vec<_>>(),
            [executor, WETH, spoke_pool]
        );
        let deposit = SpokePool::depositV3Call::abi_decode(&calls[2].data).unwrap();
        assert_eq!((deposit.depositor, deposit.recipient), (executor, handler));
        assert_eq!(deposit.outputAmount, U256::from(1_000));
        let instructions = MulticallHandler::Instructions::abi_decode(&deposit.message).unwrap();
        let shield_calldata = instructions.calls[1].callData.clone();
        let fallback = match on_shield_failure {
            BridgeShieldFailure::RefundOnOrigin => None,
            BridgeShieldFailure::KeepOnDestination => Some(destination_executor),
        };
        let message = private_delivery_message(
            handler,
            POLYGON_WETH,
            destination_executor,
            shield_calldata.clone(),
            fallback,
        );
        assert_eq!(deposit.message, message);

        // The signing-time quote named the handler and carried that message.
        let (path, _) = across_requests.lock().unwrap().last().cloned().unwrap();
        let url = url::Url::parse(&format!("http://across{path}")).unwrap();
        let sent = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap()
        };
        assert_eq!(sent("recipient").parse::<Address>().unwrap(), handler);
        assert_eq!(sent("message").parse::<Bytes>().unwrap(), message);

        // The destination account's record holds that payload at its current nonce: the guard
        // for the approved destination minimum, then a full-balance shield of the token.
        let payload = destination_record()
            .issued()
            .iter()
            .find(|payload| *payload.context().calldata() == shield_calldata)
            .cloned()
            .unwrap();
        assert_eq!(
            (payload.purpose(), payload.nonce()),
            (
                ExecutorPayloadPurpose::SwapDestinationShield,
                delegated.observed().nonce()
            )
        );
        let shield = RelayAdapt7702::multicallCall::abi_decode(&shield_calldata).unwrap();
        assert!(shield._requireSuccess);
        assert_eq!(shield._nonce, delegated.observed().nonce());
        assert_eq!(
            shield._calls.iter().map(|call| call.to).collect::<Vec<_>>(),
            [destination_executor; 2]
        );
        let guard = transferCall::abi_decode(&shield._calls[0].data).unwrap();
        assert_eq!(
            (guard._transfers[0].to, guard._transfers[0].value),
            (destination_executor, U256::from(1_000))
        );
        let shielded = shieldCall::abi_decode(&shield._calls[1].data).unwrap();
        let preimage = &shielded._shieldRequests[0].preimage;
        assert_eq!(preimage.token.tokenAddress, POLYGON_WETH);
        assert!(preimage.value.is_zero());

        // The swap's own account signed its hooks at its current nonce and the one after.
        let source_nonce = source_delegated.observed().nonce();
        let hooks = fixture.record(&store);
        for (purpose, nonce) in [
            (ExecutorPayloadPurpose::SwapPreHook, source_nonce),
            (
                ExecutorPayloadPurpose::SwapPostHook,
                source_nonce + U256::ONE,
            ),
        ] {
            assert!(
                hooks
                    .issued()
                    .iter()
                    .any(|payload| payload.purpose() == purpose && payload.nonce() == nonce)
            );
        }

        // A retry of the same use signs its own shield at the same nonce, and the signature
        // the order's deposit carries stays recorded.
        let retried = destination_owner
            .issue_swap_destination_shield(
                delegated,
                crate::vault::SwapUseId::first(operation),
                POLYGON_WETH,
                U256::from(1_001),
                &destination_authorization,
                None,
            )
            .await
            .unwrap();
        assert_ne!(retried, shield_calldata);
        let reissued = destination_record();
        for calldata in [&shield_calldata, &retried] {
            assert!(reissued.issued().iter().any(|payload| {
                payload.context().calldata() == calldata
                    && payload.nonce() == delegated.observed().nonce()
            }));
        }

        // The order keeps the signed recipient and message hash, and the private terms.
        let saved = fixture.record(&store).swap().unwrap().orders()[0].clone();
        assert_eq!(saved.uid(), uid);
        let Some(BridgeOrderTerms::Across(terms)) = saved.bridge().cloned() else {
            panic!("an Across order keeps its deposit terms");
        };
        assert_eq!(
            (terms.recipient, terms.message_hash),
            (Some(handler), Some(alloy::primitives::keccak256(&message)))
        );
        assert_eq!(
            (
                saved.bounds().destination_shield_fee_bps,
                saved.bounds().delivery_allowance,
                saved.bounds().destination_setup_fee,
            ),
            (
                Some(crate::RAILGUN_PROTOCOL_FEE_BPS),
                Some(U256::from(50)),
                setup_fee(destination_new),
            )
        );

        // The order trades and hands off, and its fill on the destination chain runs the
        // shield. The destination account's record takes that outcome from its origin swap.
        let (fill_block, fill) = (
            BlockNumHash::new(15, B256::repeat_byte(15)),
            B256::repeat_byte(0x61),
        );
        let traded = crate::vault::SwapObservation {
            block: BlockNumHash::new(13, B256::repeat_byte(13)),
            transaction_hash: Some(B256::repeat_byte(0x50)),
        };
        store
            .record_swap_settlement(
                operation,
                uid,
                traded,
                crate::vault::SwapTradeAmounts {
                    sell_amount: U256::from(997_500),
                    buy_amount: U256::from(300_000_000_000_000_000_u64),
                    fee_amount: U256::ZERO,
                    settlement_gas_used: None,
                    settlement_effective_gas_price: None,
                    executed_fee: None,
                    executed_fee_token: None,
                },
                None,
                Some(crate::vault::SwapBridgeHandoff {
                    observation: traded,
                    deposit_id: Some(U256::from(77)),
                }),
            )
            .unwrap();
        store
            .record_swap_bridge_outcome(
                operation,
                uid,
                crate::vault::SwapBridgeOutcome::DeliveredVerified {
                    block: fill_block,
                    transaction_hash: fill,
                    output_amount: U256::from(1_000),
                    shielded: true,
                },
            )
            .unwrap();
        assert!(destination_store.reconcile_swap_destinations().unwrap());

        // Another swap names both accounts. While the delivered swap's shields are still at
        // the account's recorded nonce, the destination takes no other swap and signs nothing.
        let second = crate::vault::SwapUseId::random().unwrap();
        let mut second_approval = prepared.approval.clone();
        let accounts = second_approval.accounts.as_mut().unwrap();
        accounts.source.setup = false;
        accounts.destination.as_mut().unwrap().setup = false;
        let claim = || {
            owner.claim_swap_use(
                Some(&destination_owner),
                crate::SwapUseClaim {
                    id: second,
                    source: crate::vault::SwapAccountChoice::Existing(operation),
                    approval: second_approval.clone(),
                    destination: Some(crate::vault::SwapAccountChoice::Existing(
                        destination_operation,
                    )),
                },
            )
        };
        let issued_count = || destination_record().issued().len();
        let issued_before = issued_count();
        assert!(claim().is_err());
        assert_eq!(
            destination_owner
                .swap_account_refusal(
                    destination_operation,
                    SwapAccountRole::Destination {
                        token: POLYGON_WETH
                    },
                )
                .unwrap(),
            Some(SwapAccountRefusal::UnfinishedWork)
        );
        let setup = destination_record().issued()[0].clone();
        destination_store
            .reconcile(
                destination_operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(20, B256::repeat_byte(20)),
                    U256::from(2),
                ),
                &[(setup.hash(), setup.inclusion().unwrap())],
            )
            .unwrap();

        // The account now has its delegation's code and execution nonce 2 on the chain.
        rpc.state.head.store(40, Ordering::Relaxed);
        rpc.set_delegated_account(
            destination_executor,
            destination_profile.delegate(),
            U256::from(2),
        );
        let answer_call = |contract, word: Option<u64>| {
            rpc.state
                .calls
                .lock()
                .unwrap()
                .insert(contract, word.map(U256::from));
        };
        // The settled former destination is offered in either role, and is admitted to place
        // orders from its current state alone.
        let offered = |candidates: Vec<crate::SwapAccountCandidate>| {
            candidates
                .iter()
                .any(|candidate| candidate.operation() == destination_operation)
        };
        assert!(offered(
            destination_owner.swap_account_candidates().unwrap()
        ));
        assert!(offered(
            destination_owner
                .swap_destination_candidates(POLYGON_WETH)
                .unwrap()
        ));
        let sourced = destination_owner
            .reuse_swap_account(
                destination_operation,
                39,
                SwapAccountRole::Source,
                SwapAccountUse::New,
            )
            .await
            .unwrap();
        assert_eq!(sourced.expected_pre_hook_nonce(), U256::from(2));

        // The second swap claims both accounts. Admission of its destination reads the
        // earlier shield's note from the wallet's local notes: none, a blocked one and one
        // without a verdict each refuse, despite the consumed nonce and an empty balance.
        claim().unwrap();
        let notes = ShieldNotes::default();
        let shield_note = |status: Option<crate::PoiStatus>, spent: bool| {
            let mut utxo = Utxo::new(
                broadcaster_core::notes::Note::new_change(
                    view.scan_keys().master_public_key,
                    POLYGON_WETH,
                    U256::from(1_000),
                    [9; 16],
                ),
                0,
                7,
                UtxoSource {
                    tx_hash: fill,
                    block_number: fill_block.number,
                    block_timestamp: 0,
                },
                UtxoCommitmentKind::Shield,
            );
            if let Some(status) = status {
                for list in poi::poi::default_active_poi_list_keys() {
                    utxo.poi.statuses.insert(list, status);
                }
            }
            let mut note = crate::WalletUtxo::new(utxo);
            if spent {
                note.spent = Some(UtxoSource {
                    tx_hash: B256::repeat_byte(0x62),
                    block_number: fill_block.number + 1,
                    block_timestamp: 0,
                });
            }
            note
        };
        let hold = |note| *notes.0.lock().unwrap() = vec![note];
        let admit = || {
            destination_owner.delegated_swap_destination(
                destination_operation,
                39,
                1,
                operation,
                second,
                destination_executor,
                Some(&notes),
            )
        };
        let refusal = |error: eyre::Report| {
            *error
                .downcast_ref::<SwapAccountRefusal>()
                .unwrap_or_else(|| panic!("{error:#}"))
        };
        let transaction_hash = fill;
        assert_eq!(
            refusal(admit().await.unwrap_err()),
            SwapAccountRefusal::EarlierShieldUnknown { transaction_hash }
        );
        hold(shield_note(Some(crate::PoiStatus::ShieldBlocked), false));
        assert_eq!(
            refusal(admit().await.unwrap_err()),
            SwapAccountRefusal::EarlierShieldBlocked { transaction_hash }
        );
        hold(shield_note(None, false));
        assert_eq!(
            refusal(admit().await.unwrap_err()),
            SwapAccountRefusal::EarlierShieldPending { transaction_hash }
        );
        // A spent note needs no refund, whatever its verdict was, and an accepted one admits.
        hold(shield_note(Some(crate::PoiStatus::ShieldBlocked), true));
        admit().await.unwrap();
        hold(shield_note(Some(crate::PoiStatus::Valid), false));
        let reused = admit().await.unwrap();
        assert_eq!(reused.observed().nonce(), U256::from(2));

        // The shield is judged again when it is issued. A verdict that changed since
        // admission, a balance of the receiving token and a balance that can't be read each
        // issue nothing.
        let issue_shield = || {
            destination_owner.issue_swap_destination_shield(
                reused,
                second,
                POLYGON_WETH,
                U256::from(1_000),
                &destination_authorization,
                Some(&notes),
            )
        };
        hold(shield_note(Some(crate::PoiStatus::ShieldBlocked), false));
        assert_eq!(
            refusal(issue_shield().await.unwrap_err()),
            SwapAccountRefusal::EarlierShieldBlocked { transaction_hash }
        );
        hold(shield_note(Some(crate::PoiStatus::Valid), false));
        answer_call(POLYGON_WETH, Some(5));
        assert_eq!(
            refusal(issue_shield().await.unwrap_err()),
            SwapAccountRefusal::ReceivingBalance
        );
        answer_call(POLYGON_WETH, None);
        assert_eq!(
            refusal(issue_shield().await.unwrap_err()),
            SwapAccountRefusal::ReceivingBalanceUnknown
        );
        assert_eq!(issued_count(), issued_before);

        // Native currency in the account doesn't fail the balance rule. The shield is signed
        // at the account's current nonce, and the first swap's shields stay recorded.
        answer_call(POLYGON_WETH, Some(0));
        rpc.state.native_balance.store(7, Ordering::Relaxed);
        let reissued = issue_shield().await.unwrap();
        let record = destination_record();
        assert_eq!(record.issued().len(), issued_before + 1);
        let payload = record.issued().last().unwrap();
        assert_eq!(
            (
                payload.purpose(),
                payload.nonce(),
                payload.context().calldata()
            ),
            (
                ExecutorPayloadPurpose::SwapDestinationShield,
                U256::from(2),
                &reissued
            )
        );

        across_task.abort();
        orderbook_task.abort();
        owner.shutdown().await;
        destination_owner.shutdown().await;
        drop((owner, destination_owner));
        drop((store, destination_store));
        drop(view);
        drop(vault);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

// A NEAR Intents order pays the deposit address of a 1Click quote requested while signing,
// which names the receiver and refunds to the executor, and has no post-hook. A quote that
// doesn't verify, or that can't deliver the approved minimum, stops signing.
#[tokio::test]
async fn near_intents_order_pays_a_verified_deposit_address() {
    let rpc = Rpc::start().await;
    let (root, db, vault) = desktop_store_with_vault();
    let view = Arc::new(import_wallet_with_metadata(
        &vault,
        TEST_WALLET_ID,
        "Wallet",
    ));
    let chain = chain(&rpc);
    let profile = chain.accepted_executor_profile().unwrap();
    let swap_profile = chain.swap_profile().unwrap();
    let owner = ExecutorOwner::new(
        0,
        db.clone(),
        view.clone(),
        chain,
        HttpContext::direct_for_tests(),
    )
    .unwrap();
    let store = ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let receiver = Address::repeat_byte(0x93);
    let delivery = BridgeDelivery {
        provider: BridgeProvider::NearIntents,
        destination_chain: 137,
        receiver,
        destination_token: Address::repeat_byte(0x94),
        surplus: BridgeSurplus::BridgedByProvider,
        private: None,
    };
    let fixture =
        BridgeOrderFixture::new(&owner, &store, &view, profile, &swap_profile, delivery).await;
    let executor = fixture.executor;
    let buy_amount = fixture.review.suggested_private_minimum();

    // 1Click signs the request it was sent. The test key stands in for 1Click's.
    let quote_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    let pinned = format!(
        "ed25519:{}",
        bs58::encode(quote_key.verifying_key().to_bytes()).into_string()
    );
    let deposit_address = Address::repeat_byte(0xd0);
    // The quote's minimum output and the key that signs it.
    let quoted = Arc::new(Mutex::new((
        U256::from(1_000),
        ed25519_dalek::SigningKey::from_bytes(&[8; 32]),
    )));
    let answer = quoted.clone();
    let (near_url, near_requests, near_task) = spawn_bridge_stub(move |request| {
        let (minimum, key) = answer.lock().unwrap().clone();
        let response = json!({
            "quote": {
                "amountIn": request["amount"],
                "amountInFormatted": "1",
                "amountInUsd": "1",
                "minAmountIn": request["amount"],
                "amountOut": (minimum + U256::from(10)).to_string(),
                "amountOutFormatted": "1",
                "amountOutUsd": "1",
                "minAmountOut": minimum.to_string(),
                "timeEstimate": 20,
                "deadline": request["deadline"],
                "timeWhenInactive": request["deadline"],
                "depositAddress": deposit_address
            },
            "quoteRequest": request,
            "timestamp": "2026-09-30T00:00:00.000Z"
        });
        crate::bridge::sign_quote_response_for_tests(response, &key)
    })
    .await;
    let isolation = OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct);
    let (orderbook_url, submissions, orderbook_task) = spawn_orderbook_with_lost_response(
        db.clone(),
        view.clone(),
        fixture.operation,
        swap_profile.settlement(),
        true,
    )
    .await;
    let orderbook = CowOrderbookClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        orderbook_url,
        1,
    )
    .unwrap();
    let mut clients = owner.swap_bridge_clients(&orderbook).unwrap();
    clients.near = crate::bridge::NearIntentsClient::new(
        OperationHttpClient::for_tests(reqwest::Client::new(), isolation),
        near_url,
        &pinned,
    )
    .unwrap();
    let chains =
        crate::settings::build_effective_chain_configs(&crate::settings::WalletSettings::default())
            .unwrap();
    let destination = crate::bridge::BridgeDestination {
        destination_token: delivery.destination_token,
        intermediate: WETH,
        symbol: "DST".to_owned(),
        same_asset: false,
        near: Some(crate::bridge::NearAssets {
            origin_asset: "nep141:eth-weth.omft.near".to_owned(),
            destination_asset: "nep141:pol-dst.omft.near".to_owned(),
            origin_decimals: 18,
            destination_decimals: 18,
        }),
    };
    let route = crate::SwapBridgeRoute {
        clients: &clients,
        destination: &destination,
        destination_chain: chains.get(137).unwrap(),
    };

    // A quote signed by another key isn't 1Click's.
    let error = fixture.issue(&owner, &orderbook, route).await.unwrap_err();
    assert!(
        error.to_string().contains("quote couldn't be verified"),
        "{error:#}"
    );
    fixture.assert_unsigned(&store, &submissions);
    *quoted.lock().unwrap() = (U256::from(999), quote_key.clone());
    assert_eq!(
        fixture.issue(&owner, &orderbook, route).await.unwrap(),
        SwapOrderOutcome::ReviewRequired(SwapReviewChange::DestinationMinimum {
            approved: U256::from(1_000),
            current: U256::from(999),
        })
    );
    fixture.assert_unsigned(&store, &submissions);

    // The order is persisted with the verified quote and sent. Its response is lost, and the
    // resubmission rebuilds the identical order from the saved deposit address.
    *quoted.lock().unwrap() = (U256::from(1_000), quote_key);
    assert!(
        fixture
            .issue(&owner, &orderbook, route)
            .await
            .unwrap_err()
            .downcast_ref::<crate::cow::CowApiError>()
            .is_some()
    );
    let (_, request) = near_requests.lock().unwrap().last().cloned().unwrap();
    let sent = |field: &str| request[field].as_str().unwrap().parse::<Address>().unwrap();
    assert_eq!((sent("recipient"), sent("refundTo")), (receiver, executor));
    assert_eq!(request["dry"], false);
    assert_eq!(request["amount"], buy_amount.to_string());
    let record = fixture.record(&store);
    let saved = record.swap().unwrap().orders()[0].clone();
    let Some(BridgeOrderTerms::NearIntents(terms)) = saved.bridge().cloned() else {
        panic!("a NEAR Intents order keeps its verified quote");
    };
    assert_eq!(
        (
            terms.deposit_address,
            terms.min_amount_out,
            terms.amount_out
        ),
        (deposit_address, U256::from(1_000), U256::from(1_010))
    );
    assert_eq!(terms.deadline, request["deadline"]);
    let signed: Value = serde_json::from_str(&terms.signed_quote).unwrap();
    assert_eq!(signed["quoteRequest"], request);
    assert_eq!(saved.bounds().destination_minimum, Some(U256::from(1_000)));
    assert!(saved.post_hook().is_none());
    let [_, pre_hook] = record.issued() else {
        panic!("a NEAR Intents order issues only its pre-hook");
    };
    assert_eq!(pre_hook.purpose(), ExecutorPayloadPurpose::SwapPreHook);
    assert_eq!(
        owner
            .resubmit_swap_order(fixture.operation, &orderbook)
            .await
            .unwrap(),
        SwapOrderOutcome::Submitted { uid: saved.uid() }
    );
    let submitted = submissions.lock().unwrap().clone();
    let [(persisted, body), (_, resent)] = submitted.as_slice() else {
        panic!("one initial request and one resubmission");
    };
    assert!(*persisted, "the quote is durable before the order request");
    assert_eq!(body, resent);
    let order = submitted_order(body);
    assert_eq!(
        (order.receiver, order.buyToken, order.buyAmount),
        (deposit_address, WETH, buy_amount)
    );
    let hooks = serde_json::from_str::<AppData>(body["appData"].as_str().unwrap())
        .unwrap()
        .metadata
        .hooks;
    assert_eq!((hooks.pre.len(), hooks.post.len()), (1, 0));

    near_task.abort();
    orderbook_task.abort();
    owner.shutdown().await;
    drop(owner);
    drop(store);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}
