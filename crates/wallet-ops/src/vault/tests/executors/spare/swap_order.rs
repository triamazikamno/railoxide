use super::swap_setup::{
    DESTINATION_CHAIN, USDC, WETH, broadcaster, destination_chain_config, password, setup_approval,
    setup_fee_limit,
};
use super::*;
use crate::cow::{CowOrderbookClient, CowQuote, GAS_SHARE_BALANCED_BPS, GAS_SHARE_LOOSE_BPS};
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
    AppData, BUY_NATIVE_TOKEN, GPv2Settlement, ORDER_KIND_SELL, Order, OrderUid,
    TOKEN_BALANCE_ERC20, order_digest, order_uid, recover_order_signer,
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
    spawn_orderbook_on(1, db, view, operation, settlement, false).await
}

async fn spawn_orderbook_with_lost_response(
    db: Arc<local_db::DbStore>,
    view: Arc<DesktopViewSession>,
    operation: ExecutorOperationId,
    settlement: Address,
    lose_first_response: bool,
) -> (url::Url, Submissions, tokio::task::JoinHandle<()>) {
    spawn_orderbook_on(1, db, view, operation, settlement, lose_first_response).await
}

/// [`spawn_orderbook`] for orders on the chain `chain_id`, which loses its response to the
/// first order with `lose_first_response`.
pub(super) async fn spawn_orderbook_on(
    chain_id: u64,
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
            let uid = order_uid(&submitted_order(&body), chain_id, settlement, owner);
            let persisted = ExecutorStore::new(db.clone(), view.clone(), chain_id)
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
pub(super) async fn spawn_bridge_stub_with(
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
pub(super) async fn read_json_body(stream: &mut BufReader<tokio::net::TcpStream>) -> Option<Value> {
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
    store.record_account_read(operation, before).unwrap();
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
    let record = store.record_account_read(operation, observed).unwrap();
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
            crate::OperationHttpClient::direct_for_tests(),
        )
        .await
        .unwrap();
    (operation, delegate_setup(store, operation, profile))
}

/// An Across fee quote for `output` whose fill deadline is `fill_after` seconds from now.
pub(super) fn across_fee_quote(spoke_pool: Address, output: U256, fill_after: u64) -> String {
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
        delivery: SwapDelivery,
    ) -> Self {
        let operation = ExecutorOperationId::random().unwrap();
        let executor = owner
            .prepare_swap_setup(
                operation,
                broadcaster(profile.delegate()),
                setup_approval(USDC, WETH, delivery.clone()),
                None,
                &password(),
                crate::OperationHttpClient::direct_for_tests(),
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
        delivery: SwapDelivery,
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
        delivery: SwapDelivery,
    ) -> Self {
        let operation = delegated.operation();
        let provider = delivery.bridge_provider().unwrap();
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
                delivery,
                byte_budget: None,
            },
            swap_profile.app_data_byte_budget(),
            None,
        )
        .unwrap() else {
            panic!("one note fits one order");
        };
        let review = Self::review_at(plan, swap_profile, provider, 1, 1_000);
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
            expected_output_usd: None,
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
                continuing_minimum: None,
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

/// The hook-free `CoW` quote the order fixtures review: 997,500 USDC for 0.3 WETH.
fn usdc_weth_quote() -> Value {
    json!({
        "quote": {
            "sellToken": USDC, "buyToken": WETH, "sellAmount": "997500",
            "buyAmount": "300000000000000000", "validTo": 1, "feeAmount": "0",
            "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": "1000000000000",
            "kind": "sell", "partiallyFillable": false
        },
        "expiration": "", "id": 7, "verified": true
    })
}

/// The plan the wallet makes to speed up the recorded order `open`, for the account
/// `delegated` confirms and the candidate notes `notes`.
fn speed_up_plan(
    store: &ExecutorStore,
    swap_profile: &crate::settings::SwapProfile,
    delegated: crate::DelegatedSwapExecutor,
    notes: &[Utxo],
    open: OrderUid,
) -> eyre::Result<crate::SwapInputPlan> {
    let record = store
        .records()
        .unwrap()
        .into_iter()
        .find(|record| record.operation() == delegated.operation())
        .unwrap();
    // The builder the order fixtures plan with.
    let builder = railgun_wallet::TransactionBuilder {
        chain_type: 0,
        chain_id: 1,
        railgun_contract: Address::repeat_byte(4),
    };
    match crate::plan_swap_speed_up_inputs(
        &builder,
        swap_profile,
        &record,
        delegated,
        notes,
        open,
        std::time::SystemTime::now(),
        delegated.inclusion(),
    )? {
        SwapAmountPlan::Fits(plan) => Ok(plan),
        SwapAmountPlan::TooLarge { .. } => panic!("the open order's amount fits one order"),
    }
}

/// Sign `review` at `private_minimum` for the first swap use of its account, with the proved
/// `transaction` that spends `input`. A Bridge order names its route, its approved destination
/// minimum and the continuing minimum the user accepted.
async fn sign_order(
    owner: &ExecutorOwner,
    orderbook: &CowOrderbookClient,
    review: &crate::SwapReview,
    private_minimum: U256,
    transaction: &Transaction,
    input: &Utxo,
    bridge: Option<(crate::SwapBridgeRoute<'_>, U256, Option<U256>)>,
) -> eyre::Result<SwapOrderOutcome> {
    let (bridge, destination_minimum, continuing_minimum) = match bridge {
        Some((route, destination_minimum, continuing_minimum)) => {
            (Some(route), Some(destination_minimum), continuing_minimum)
        }
        None => (None, None, None),
    };
    owner
        .issue_swap_order(crate::SwapOrderSigning {
            review,
            swap_use: crate::vault::SwapUseId::first(review.plan().operation().unwrap()),
            private_minimum,
            price_acknowledged: true,
            transactions: vec![transaction.clone()],
            inputs: std::slice::from_ref(input),
            change_output_pois: Vec::new(),
            output_pois: &OutputPois::default(),
            authorization: &password(),
            orderbook,
            anchor_cache: &crate::TokenAnchorRateCache::new(),
            token_registry: &crate::settings::EffectiveTokenRegistry {
                tokens: std::collections::BTreeMap::new(),
            },
            bridge,
            destination_minimum,
            destination: None,
            continuing_minimum,
        })
        .await
}

mod across_post_hooks;
mod approval_and_external;
mod deposit_address_orders;
mod private_across;
mod quotes_and_signing;
mod signing_admission;
