use super::*;

pub(super) const STUB_USDC: Address =
    alloy::primitives::address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
/// What the stub orderbook quotes for any sell amount: 0.0004 ETH.
pub(super) const STUB_BUY_AMOUNT: u64 = 400_000_000_000_000;
/// The fee the stub orderbook reports it charged any order, in `Address::repeat_byte(2)`.
pub(super) const STUB_EXECUTED_FEE: u64 = 464_572;

/// Each stub request's path and the `x-route` tag of the client that sent it.
pub(super) type StubRequests = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

/// Local stand-ins for the chain RPC, the `CoW` orderbook and the bridge providers, served from
/// their own thread, so a quote goes through the real planning and review without live
/// services. The RPC answers `eth_gasPrice` and fails everything else, like the unreachable RPC
/// of other tests, with a gas price of 1 wei unless set. The orderbook quotes
/// [`STUB_BUY_AMOUNT`] for any order, with no network fee unless set, and keeps each request's
/// body, and reports every order
/// fulfilled with [`STUB_EXECUTED_FEE`], keeping each report's path. Across and 1Click list the routes of [`stub_bridge_list`] and quote as
/// [`stub_bridge_reply`] describes.
pub(super) struct SwapStubs {
    pub(super) url: reqwest::Url,
    pub(super) quotes: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    pub(super) bridge_requests: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) order_requests: Arc<std::sync::Mutex<Vec<String>>>,
    /// The path of each request, which tells apart chains given their own RPC path, with the
    /// `x-route` tag of the client it came from.
    pub(super) requests: StubRequests,
    pub(super) across_fee_bps: Arc<std::sync::atomic::AtomicU64>,
    pub(super) across_relayer_gas_bps: Arc<std::sync::atomic::AtomicU64>,
    pub(super) across_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    pub(super) failing: Arc<std::sync::Mutex<Vec<&'static str>>>,
    pub(super) gas_price_wei: Arc<std::sync::atomic::AtomicU64>,
    pub(super) public_review_rpc: Arc<std::sync::atomic::AtomicBool>,
    pub(super) public_allowance_complete: Arc<std::sync::atomic::AtomicBool>,
    pub(super) math_deployed: Arc<std::sync::atomic::AtomicBool>,
    pub(super) fee_amount: Arc<std::sync::atomic::AtomicU64>,
    pub(super) stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub(super) thread: Option<std::thread::JoinHandle<()>>,
}

/// What the stub providers quote with: Across deposits into the swap chain's `spoke_pool` and
/// keeps `across_fee_bps` of the amount, after `across_delay_ms`. Requests to a path starting
/// with one of `failing` get a 503.
#[derive(Clone)]
pub(super) struct StubBridge {
    pub(super) spoke_pool: Address,
    pub(super) across_fee_bps: Arc<std::sync::atomic::AtomicU64>,
    pub(super) across_relayer_gas_bps: Arc<std::sync::atomic::AtomicU64>,
    pub(super) across_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    pub(super) failing: Arc<std::sync::Mutex<Vec<&'static str>>>,
    pub(super) public_review_rpc: Arc<std::sync::atomic::AtomicBool>,
    pub(super) public_allowance_complete: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the math contract an order's post-hook calls has its code on the stub chain.
    pub(super) math_deployed: Arc<std::sync::atomic::AtomicBool>,
}

impl SwapStubs {
    pub(super) fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let quotes = Arc::<std::sync::Mutex<Vec<serde_json::Value>>>::default();
        let bridge_requests = Arc::<std::sync::Mutex<Vec<String>>>::default();
        let order_requests = Arc::<std::sync::Mutex<Vec<String>>>::default();
        let requests = Arc::<std::sync::Mutex<Vec<(String, Option<String>)>>>::default();
        let across_fee_bps = Arc::new(std::sync::atomic::AtomicU64::new(10));
        let across_relayer_gas_bps = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let across_delay_ms = Arc::<std::sync::atomic::AtomicU64>::default();
        let failing = Arc::<std::sync::Mutex<Vec<&'static str>>>::default();
        let gas_price_wei = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let public_review_rpc = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let public_allowance_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let math_deployed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let served_gas_price = Arc::clone(&gas_price_wei);
        let fee_amount = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let served_fee_amount = Arc::clone(&fee_amount);
        let recorded = Arc::clone(&quotes);
        let recorded_bridge = Arc::clone(&bridge_requests);
        let recorded_orders = Arc::clone(&order_requests);
        let recorded_requests = Arc::clone(&requests);
        let bridge = StubBridge {
            spoke_pool: wallet_ops::settings::build_effective_chain_configs(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap()
            .get(1)
            .unwrap()
            .bridge_profile()
            .unwrap()
            .spoke_pool(),
            across_fee_bps: Arc::clone(&across_fee_bps),
            across_relayer_gas_bps: Arc::clone(&across_relayer_gas_bps),
            across_delay_ms: Arc::clone(&across_delay_ms),
            failing: Arc::clone(&failing),
            public_review_rpc: Arc::clone(&public_review_rpc),
            public_allowance_complete: Arc::clone(&public_allowance_complete),
            math_deployed: Arc::clone(&math_deployed),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let serve = async {
                    while let Ok((stream, _)) = listener.accept().await {
                        tokio::spawn(stub_response(
                            stream,
                            Arc::clone(&recorded),
                            Arc::clone(&recorded_bridge),
                            Arc::clone(&recorded_orders),
                            Arc::clone(&recorded_requests),
                            bridge.clone(),
                            Arc::clone(&served_gas_price),
                            Arc::clone(&served_fee_amount),
                        ));
                    }
                };
                tokio::select! {
                    () = serve => {}
                    _ = stopped => {}
                }
            });
        });
        Self {
            url,
            quotes,
            bridge_requests,
            order_requests,
            requests,
            across_fee_bps,
            across_relayer_gas_bps,
            across_delay_ms,
            failing,
            gas_price_wei,
            public_review_rpc,
            public_allowance_complete,
            math_deployed,
            fee_amount,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Have Across keep `bps` of the amount from now on.
    pub(super) fn set_across_fee_bps(&self, bps: u64) {
        self.across_fee_bps
            .store(bps, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have Across hold each fee quote it is asked for from now on for `delay`.
    pub(super) fn set_across_delay(&self, delay: Duration) {
        self.across_delay_ms.store(
            u64::try_from(delay.as_millis()).unwrap(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Have the providers answer 503 to paths starting with one of `prefixes` from now on.
    pub(super) fn set_failing(&self, prefixes: &[&'static str]) {
        *self.failing.lock().unwrap() = prefixes.to_vec();
    }

    /// Have the RPC report `wei` as its gas price from now on.
    pub(super) fn set_gas_price_wei(&self, wei: u64) {
        self.gas_price_wei
            .store(wei, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have the orderbook quote `fee` of the sell amount as its network fee from now on.
    pub(super) fn set_fee_amount(&self, fee: u64) {
        self.fee_amount
            .store(fee, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn rpc(&self) -> reqwest::Url {
        self.url.join("rpc").unwrap()
    }

    /// Public previews read an existing allowance, whether the `CoW` proxy is deployed, and
    /// the math contract's code.
    pub(super) fn enable_public_reviews(&self) {
        self.public_review_rpc
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Have the chain answer a read of the math contract's code with its runtime code, as it
    /// does at first, or with none, from now on.
    pub(super) fn set_math_deployed(&self, deployed: bool) {
        self.math_deployed
            .store(deployed, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn orderbook(&self) -> reqwest::Url {
        self.url.join("mainnet").unwrap()
    }

    /// The bodies of the quote requests so far.
    pub(super) fn quotes(&self) -> Vec<serde_json::Value> {
        self.quotes.lock().unwrap().clone()
    }

    /// The paths of the bridge provider requests so far.
    pub(super) fn bridge_requests(&self) -> Vec<String> {
        self.bridge_requests.lock().unwrap().clone()
    }

    /// The paths of the orderbook's order reports so far.
    pub(super) fn order_requests(&self) -> Vec<String> {
        self.order_requests.lock().unwrap().clone()
    }

    /// How many RPC requests arrived on `path` so far.
    pub(super) fn rpc_requests_on(&self, path: &str) -> usize {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .filter(|(request, _)| request == path)
            .count()
    }

    /// How many requests arrived so far, to tell later ones apart.
    pub(super) fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// The `x-route` tags of the requests after the first `from` whose path starts with
    /// `prefix`, each once.
    pub(super) fn routes_since(&self, from: usize, prefix: &str) -> BTreeSet<Option<String>> {
        self.requests.lock().unwrap()[from..]
            .iter()
            .filter(|(path, _)| path.starts_with(prefix))
            .map(|(_, route)| route.clone())
            .collect()
    }
}

impl Drop for SwapStubs {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Answer one request, then close the connection.
pub(super) async fn stub_response(
    stream: tokio::net::TcpStream,
    quotes: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    bridge_requests: Arc<std::sync::Mutex<Vec<String>>>,
    order_requests: Arc<std::sync::Mutex<Vec<String>>>,
    requests: StubRequests,
    bridge: StubBridge,
    gas_price_wei: Arc<std::sync::atomic::AtomicU64>,
    fee_amount: Arc<std::sync::atomic::AtomicU64>,
) {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::io::BufReader::new(stream);
    let mut request_line = String::new();
    if stream.read_line(&mut request_line).await.unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0;
    let mut route = None;
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        let line = line.to_ascii_lowercase();
        if let Some(value) = line.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        if let Some(value) = line.strip_prefix("x-route:") {
            route = Some(value.trim().to_owned());
        }
    }
    let mut body = vec![0; length];
    if stream.read_exact(&mut body).await.is_err() {
        return;
    }
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let path = request_line.split(' ').nth(1).unwrap_or_default();
    requests.lock().unwrap().push((path.to_owned(), route));
    let failing = bridge
        .failing
        .lock()
        .unwrap()
        .iter()
        .any(|prefix| path.starts_with(prefix));
    let status = if failing {
        "503 Service Unavailable"
    } else {
        "200 OK"
    };
    let reply = if path.starts_with("/across/") || path.starts_with("/near/") {
        bridge_requests.lock().unwrap().push(path.to_owned());
        if path.starts_with("/across/suggested-fees") {
            let delay = bridge
                .across_delay_ms
                .load(std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        stub_bridge_reply(path, &body, &bridge)
    } else if request_line.contains("/api/v1/quote") {
        // Price the sell token so that one buy-token base unit is worth one wei, which keeps
        // the hook allowance far below the quoted output for any sell amount.
        #[allow(clippy::cast_precision_loss)]
        let sell_token_price = body["sellAmountBeforeFee"]
            .as_str()
            .and_then(|amount| amount.parse::<f64>().ok())
            .map_or_else(
                || "1".to_owned(),
                |amount| (STUB_BUY_AMOUNT as f64 / amount).to_string(),
            );
        // The network fee comes out of the sell amount, as `CoW` quotes a sell order.
        let fee = U256::from(fee_amount.load(std::sync::atomic::Ordering::Relaxed));
        let sell_amount = body["sellAmountBeforeFee"]
            .as_str()
            .and_then(|amount| amount.parse::<U256>().ok())
            .map_or_else(
                || body["sellAmountBeforeFee"].clone(),
                |amount| amount.saturating_sub(fee).to_string().into(),
            );
        let reply = serde_json::json!({
            "quote": {
                "sellToken": body["sellToken"], "buyToken": body["buyToken"],
                "sellAmount": sell_amount,
                "buyAmount": STUB_BUY_AMOUNT.to_string(), "validTo": 1,
                "feeAmount": fee.to_string(),
                "gasAmount": "0", "gasPrice": "0", "sellTokenPrice": sell_token_price, "kind": "sell",
                "partiallyFillable": false
            },
            "expiration": "", "id": 7, "verified": true
        });
        quotes.lock().unwrap().push(body);
        reply
    } else if path.contains("/api/v1/orders/") {
        order_requests.lock().unwrap().push(path.to_owned());
        serde_json::json!({
            "status": "fulfilled",
            "executedFee": STUB_EXECUTED_FEE.to_string(),
            "executedFeeToken": Address::repeat_byte(2),
        })
    } else if bridge
        .public_review_rpc
        .load(std::sync::atomic::Ordering::Relaxed)
        && body["method"] == "eth_call"
    {
        let allowance = body["params"][0]["input"]
            .as_str()
            .or_else(|| body["params"][0]["data"].as_str())
            .is_some_and(|input| input.starts_with("0xdd62ed3e"));
        let amount = if allowance
            && bridge
                .public_allowance_complete
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            U256::MAX
        } else {
            U256::ZERO
        };
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
            "result": format!("0x{}", alloy::hex::encode(amount.to_be_bytes::<32>()))})
    } else if bridge
        .public_review_rpc
        .load(std::sync::atomic::Ordering::Relaxed)
        && body["method"] == "eth_getCode"
    {
        // Only the math contract has code: the account's `CoW` proxy isn't deployed.
        let math = body["params"][0]
            .as_str()
            .and_then(|address| address.parse::<Address>().ok())
            == Some(SWAP_MATH_ADDRESS)
            && bridge
                .math_deployed
                .load(std::sync::atomic::Ordering::Relaxed);
        let code = if math {
            &SWAP_MATH_CREATION_CODE[30..]
        } else {
            &[]
        };
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
            "result": alloy::hex::encode_prefixed(code)})
    } else if body["method"] == "eth_gasPrice" {
        let price = gas_price_wei.load(std::sync::atomic::Ordering::Relaxed);
        serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": format!("{price:#x}")})
    } else {
        serde_json::json!({
            "jsonrpc": "2.0", "id": body["id"],
            "error": {"code": -32601, "message": "unavailable in tests"}
        })
    };
    let reply = reply.to_string();
    let _ = stream
        .get_mut()
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .as_bytes(),
        )
        .await;
}

/// Mainnet USDT and Polygon's USDC and USDT, all in the default token list.
pub(super) const STUB_USDT: Address =
    alloy::primitives::address!("dac17f958d2ee523a2206206994597c13d831ec7");
pub(super) const STUB_POLYGON_USDC: Address =
    alloy::primitives::address!("3c499c542cef5e3811e1192ce70d8cc03d5c3359");
pub(super) const STUB_POLYGON_USDT: Address =
    alloy::primitives::address!("c2132d05d31c914a87c6611c10748aeb04b58e8f");

/// WETH on Ethereum, and on Arbitrum One and Base, where it is the wrapped native token.
pub(super) const STUB_WETH: Address =
    alloy::primitives::address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
pub(super) const STUB_ARBITRUM_WETH: Address =
    alloy::primitives::address!("82af49447d8a07e3bd95bd0d56f35241523fbab1");
pub(super) const STUB_BASE_WETH: Address =
    alloy::primitives::address!("4200000000000000000000000000000000000006");

/// The mints 1Click's stub lists on Solana: USDC's, and a look-alike sUSDC's.
pub(super) const STUB_SOLANA_USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub(super) const STUB_SOLANA_SUSDC: &str = "FiNPw48pLZgbwmFiaYdTyJDvQCsefYx3CsBy4iVZFpBz";
/// A Solana receiver: 32 bytes in base58, which no wallet holds.
pub(super) const SOLANA_RECEIVER: &str = "9jQtwHhZT1H2TYSMt74msmBmy8UPen4GUysNynPUVkkv";

/// The stub providers' lists from Ethereum. Across bridges USDC and USDT to Polygon, and WETH
/// to Arbitrum One and Base. 1Click lists both stablecoins on Ethereum and Polygon, native POL on
/// Polygon, native ETH on Arbitrum One, SOL, USDC and sUSDC on Solana, and BTC on Bitcoin.
pub(super) fn stub_bridge_list(path: &str) -> serde_json::Value {
    let route = |chain: u64, origin: Address, destination: Address, symbol: &str| {
        serde_json::json!({
            "originChainId": 1, "originToken": origin, "destinationChainId": chain,
            "destinationToken": destination, "originTokenSymbol": symbol,
            "destinationTokenSymbol": symbol, "isNative": false
        })
    };
    let token = |blockchain: &str, symbol: &str, contract: Option<Address>| {
        serde_json::json!({
            "assetId": format!("nep141:{blockchain}-{symbol}"), "decimals": 6,
            "blockchain": blockchain, "symbol": symbol, "contractAddress": contract
        })
    };
    if path.starts_with("/across/available-routes") {
        serde_json::json!([
            route(137, STUB_USDC, STUB_POLYGON_USDC, "USDC"),
            route(137, STUB_USDT, STUB_POLYGON_USDT, "USDT"),
            route(42161, STUB_WETH, STUB_ARBITRUM_WETH, "WETH"),
            route(8453, STUB_WETH, STUB_BASE_WETH, "WETH"),
        ])
    } else if path.starts_with("/near/v0/tokens") {
        // Non-EVM contracts are 1Click's strings, such as a Solana mint.
        let non_evm = |blockchain: &str, symbol: &str, contract: Option<&str>, decimals: u8| {
            serde_json::json!({
                "assetId": format!("nep141:{blockchain}-{symbol}"), "decimals": decimals,
                "blockchain": blockchain, "symbol": symbol, "contractAddress": contract
            })
        };
        serde_json::json!([
            token("eth", "USDC", Some(STUB_USDC)),
            token("eth", "USDT", Some(STUB_USDT)),
            token("pol", "POL", None),
            token("pol", "USDC", Some(STUB_POLYGON_USDC)),
            token("pol", "USDT", Some(STUB_POLYGON_USDT)),
            token("arb", "ETH", None),
            non_evm("sol", "SOL", None, 9),
            non_evm("sol", "USDC", Some(STUB_SOLANA_USDC), 6),
            non_evm("sol", "sUSDC", Some(STUB_SOLANA_SUSDC), 6),
            non_evm("btc", "BTC", None, 8),
            non_evm("zec", "ZEC", None, 8),
        ])
    } else {
        serde_json::json!({})
    }
}

/// The seed of the key that stands in for 1Click's quote signer.
pub(super) const STUB_QUOTE_SEED: u8 = 7;
/// What 1Click's stub quotes deliver, in base units of the destination token: about 12.6 and
/// at least 12.5 of an 18-decimal token.
pub(super) const STUB_NEAR_EXPECTED: &str = "12600000000000000000";
pub(super) const STUB_NEAR_MINIMUM: &str = "12500000000000000000";
/// 1Click's USD value of the expected output, $12.60, which values the minimum at $12.50.
pub(super) const STUB_NEAR_EXPECTED_USD: &str = "12.6";

/// A stub provider's answer. Across quotes a fee of the configured share of the amount; 1Click
/// signs a dry quote of [`STUB_NEAR_EXPECTED`] and [`STUB_NEAR_MINIMUM`] for the request it was
/// sent with the stand-in key, and reports every deposit delivered with [`STUB_NEAR_EXPECTED`].
/// Other requests get the lists of [`stub_bridge_list`].
pub(super) fn stub_bridge_reply(
    path: &str,
    body: &serde_json::Value,
    bridge: &StubBridge,
) -> serde_json::Value {
    if path.starts_with("/across/suggested-fees") {
        let amount = reqwest::Url::parse(&format!("http://stub{path}"))
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "amount")
                    .and_then(|(_, amount)| amount.parse::<U256>().ok())
            })
            .unwrap_or_default();
        let bps = bridge
            .across_fee_bps
            .load(std::sync::atomic::Ordering::Relaxed);
        let fee = amount * U256::from(bps) / U256::from(10_000_u32);
        serde_json::json!({
            "outputAmount": (amount - fee).to_string(),
            "totalRelayFee": {"pct": "0", "total": fee.to_string()},
            "relayerGasFee": {
                "pct": (U256::from(bridge.across_relayer_gas_bps.load(std::sync::atomic::Ordering::Relaxed))
                    * U256::from(100_000_000_000_000_u64)).to_string(),
                "total": "0"
            },
            "lpFee": {"pct": "0", "total": "0"},
            "timestamp": "1790718359", "fillDeadline": "1790725559",
            "exclusiveRelayer": Address::ZERO, "exclusivityDeadline": 0,
            "spokePoolAddress": bridge.spoke_pool,
            "destinationSpokePoolAddress": Address::repeat_byte(0x55),
            "isAmountTooLow": false,
            "limits": {"minDeposit": "1", "maxDeposit": U256::MAX.to_string()},
            "estimatedFillTimeSec": 2
        })
    } else if path.starts_with("/near/v0/quote") {
        let response = serde_json::json!({
            "quote": {
                "amountIn": body["amount"], "minAmountIn": body["amount"],
                "amountOut": STUB_NEAR_EXPECTED, "minAmountOut": STUB_NEAR_MINIMUM,
                "amountOutUsd": STUB_NEAR_EXPECTED_USD, "timeEstimate": 20
            },
            "quoteRequest": body,
            "timestamp": "2026-09-30T00:00:00.000Z"
        });
        serde_json::from_str(&wallet_ops::bridge::sign_quote_response_with_stand_in(
            response,
            STUB_QUOTE_SEED,
        ))
        .unwrap()
    } else if path.starts_with("/near/v0/status") {
        serde_json::json!({
            "status": "SUCCESS",
            "swapDetails": {
                "amountOut": STUB_NEAR_EXPECTED,
                "destinationChainTxHashes": [
                    {"hash": alloy::primitives::B256::repeat_byte(0x5c), "explorerUrl": ""}
                ]
            }
        })
    } else {
        stub_bridge_list(path)
    }
}

/// Bridge clients for the stub providers, on `orderbook`'s route.
pub(super) fn stub_bridge_clients(
    stubs: &SwapStubs,
    orderbook: &CowOrderbookClient,
) -> SwapBridgeClients {
    SwapBridgeClients {
        across: wallet_ops::bridge::AcrossClient::new(
            orderbook.http().clone(),
            stubs.url.join("across").unwrap(),
        )
        .unwrap(),
        near: wallet_ops::bridge::NearIntentsClient::new(
            orderbook.http().clone(),
            stubs.url.join("near").unwrap(),
            &wallet_ops::bridge::stand_in_quote_key(STUB_QUOTE_SEED),
        )
        .unwrap(),
    }
}
