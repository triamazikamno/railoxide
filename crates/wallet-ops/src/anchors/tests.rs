use crate::RpcChainRoute;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::{Arc as SharedArc, Mutex, mpsc};
use std::thread;

use alloy::primitives::address;
use alloy::primitives::aliases::I56;
use alloy::primitives::{Bytes, U160};
use alloy::uint;
use railgun_ui::WRAPPED_NATIVE_FEE_RATE;
use serde_json::{Value, json};
use tokio::time::timeout;
use tracing::instrument::WithSubscriber;

use super::*;

const SHARED_ORACLE_SOURCE_6: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x0000000000000000000000000000000000000100"),
    token_decimals: 6,
    oracle_decimals: 8,
    is_inversed: false,
}];
const SHARED_ORACLE_SOURCE_18: &[TokenAnchorSource] = &[TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x0000000000000000000000000000000000000100"),
    token_decimals: 18,
    oracle_decimals: 8,
    is_inversed: false,
}];
const ETH_USD_18_SOURCE: TokenAnchorSource = TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x0000000000000000000000000000000000000200"),
    token_decimals: 18,
    oracle_decimals: 8,
    is_inversed: false,
};
const ARB_USD_INVERSE_18_SOURCE: TokenAnchorSource = TokenAnchorSource::ChainlinkOracle {
    addr: address!("0x0000000000000000000000000000000000000300"),
    token_decimals: 18,
    oracle_decimals: 8,
    is_inversed: true,
};
const ARB_PER_ETH_PRODUCT_SOURCES: &[TokenAnchorSource] =
    &[ETH_USD_18_SOURCE, ARB_USD_INVERSE_18_SOURCE];
const ARB_PER_ETH_ANCHOR_SOURCE: &[TokenAnchorSource] = &[TokenAnchorSource::Product {
    sources: ARB_PER_ETH_PRODUCT_SOURCES,
    scale_decimals: 18,
}];

const TWAP_POOL: Address = address!("0x0000000000000000000000000000000000000400");
const TWAP_BASE: Address = address!("0x0000000000000000000000000000000000000401");
const TWAP_QUOTE: Address = address!("0x0000000000000000000000000000000000000402");

fn twap_source(window_seconds: u32) -> RuntimeTokenAnchorSource {
    RuntimeTokenAnchorSource::UniswapV3Twap {
        pool: TWAP_POOL,
        base_token: TWAP_BASE,
        quote_token: TWAP_QUOTE,
        base_token_decimals: 18,
        window_seconds,
    }
}

fn spawn_twap_rpc_fixture(
    multicall: Address,
    base: Address,
    quote: Address,
    expected_window_seconds: u32,
    request_count: usize,
    fail_second: bool,
) -> (String, mpsc::Receiver<Value>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind TWAP RPC fixture");
    let url = format!("http://{}", listener.local_addr().expect("fixture address"));
    let (request_tx, request_rx) = mpsc::channel();
    let task = thread::spawn(move || {
        let mut application_calls = 0;
        while application_calls < request_count {
            let (mut stream, _) = listener.accept().expect("accept fixture request");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            let (header_end, content_length) = loop {
                let read = stream.read(&mut buffer).expect("read fixture headers");
                assert!(read > 0);
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let end = index + 4;
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("content length"))
                        })
                        .expect("content length");
                    break (end, length);
                }
            };
            while bytes.len() < header_end + content_length {
                let read = stream.read(&mut buffer).expect("read fixture body");
                assert!(read > 0);
                bytes.extend_from_slice(&buffer[..read]);
            }
            let request: Value =
                serde_json::from_slice(&bytes[header_end..header_end + content_length])
                    .expect("fixture JSON");
            application_calls += 1;
            request_tx
                .send(request.clone())
                .expect("record fixture request");
            let params = request["params"][0].clone();
            assert_eq!(
                params["to"]
                    .as_str()
                    .and_then(|value| value.parse::<Address>().ok()),
                Some(multicall)
            );
            let call_data = params
                .get("input")
                .or_else(|| params.get("data"))
                .and_then(Value::as_str)
                .expect("eth_call calldata missing input/data");
            let mut metadata = false;
            let mut observation = false;
            let call_bytes = call_data
                .parse::<alloy::primitives::Bytes>()
                .expect("call bytes");
            let decoded =
                alloy::providers::bindings::IMulticall3::tryAggregateCall::abi_decode(&call_bytes)
                    .expect("tryAggregate calldata");
            let call_count = decoded.calls.len();
            for call in decoded.calls {
                if call
                    .callData
                    .starts_with(&UniswapV3PoolInterface::token0Call::SELECTOR)
                    || call
                        .callData
                        .starts_with(&UniswapV3PoolInterface::token1Call::SELECTOR)
                {
                    metadata = true;
                }
                if call
                    .callData
                    .starts_with(&UniswapV3PoolInterface::observeCall::SELECTOR)
                {
                    observation = true;
                    let decoded = UniswapV3PoolInterface::observeCall::abi_decode(&call.callData)
                        .expect("observe calldata");
                    assert_eq!(decoded.secondsAgos, vec![expected_window_seconds, 0]);
                }
            }
            assert_ne!(metadata, observation, "exactly two homogeneous batches");
            let returns = if metadata {
                (0..call_count)
                    .map(|index| {
                        let success = !(fail_second && index >= 2);
                        let return_data = if !success {
                            Bytes::new()
                        } else if index % 2 == 0 {
                            UniswapV3PoolInterface::token0Call::abi_encode_returns(&base).into()
                        } else {
                            UniswapV3PoolInterface::token1Call::abi_encode_returns(&quote).into()
                        };
                        alloy::providers::bindings::IMulticall3::Result {
                            success,
                            returnData: return_data,
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                type ObserveReturn = <UniswapV3PoolInterface::observeCall as SolCall>::Return;
                let decoded = ObserveReturn {
                    tickCumulatives: vec![I56::ZERO, I56::ZERO],
                    secondsPerLiquidityCumulativeX128s: vec![U160::ZERO, U160::ZERO],
                };
                (0..call_count)
                    .map(|index| alloy::providers::bindings::IMulticall3::Result {
                        success: !(fail_second && index >= 1),
                        returnData: if fail_second && index >= 1 {
                            Bytes::new()
                        } else {
                            UniswapV3PoolInterface::observeCall::abi_encode_returns(&decoded).into()
                        },
                    })
                    .collect::<Vec<_>>()
            };
            let response =
                alloy::providers::bindings::IMulticall3::tryAggregateCall::abi_encode_returns(
                    &returns,
                );
            let body = json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": format!("0x{}", alloy::hex::encode(response)),
            });
            let body = serde_json::to_string(&body).expect("fixture response");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).expect("write fixture response");
        }
    });
    (url, request_rx, task)
}

#[derive(Clone)]
struct SharedLogWriter(SharedArc<Mutex<Vec<u8>>>);

impl Write for SharedLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn runtime_sources(sources: &[TokenAnchorSource]) -> Vec<RuntimeTokenAnchorSource> {
    sources
        .iter()
        .map(|source| static_anchor_source_to_runtime(1, source))
        .collect()
}

#[test]
fn fixed_anchor_source_uses_wrapped_native_rate() {
    assert_eq!(
        non_zero_rate(WRAPPED_NATIVE_FEE_RATE),
        Some(uint!(1_000_000_000_000_000_000_U256))
    );
}

#[test]
fn oracle_answer_to_anchor_rate_handles_non_inverted_feed() {
    let rate = oracle_answer_to_anchor_rate(uint!(3_000_00000000_U256), 6, 8, false).expect("rate");

    assert_eq!(rate, uint!(3_000_000_000_U256));
}

#[test]
fn oracle_answer_to_anchor_rate_handles_inverted_feed() {
    let rate = oracle_answer_to_anchor_rate(uint!(15_000_000_000_000_000_000_U256), 8, 18, true)
        .expect("rate");

    assert_eq!(rate, uint!(6_666_666_U256));
}

#[test]
fn oracle_answer_discards_non_sensible_values() {
    assert_eq!(oracle_answer_to_anchor_rate(U256::ZERO, 6, 8, false), None);
    assert_eq!(oracle_answer_to_anchor_rate(U256::ONE, 0, 8, false), None);
    assert_eq!(oracle_answer_to_anchor_rate(U256::MAX, 18, 0, false), None);
}

#[test]
fn average_non_outlier_anchor_rates_averages_agreeing_sources() {
    let rates = [uint!(100_U256), uint!(110_U256), uint!(105_U256)];

    assert_eq!(
        average_non_outlier_anchor_rates(&rates),
        Some(uint!(105_U256))
    );
}

#[test]
fn average_non_outlier_anchor_rates_rejects_large_outlier() {
    let rates = [uint!(100_U256), uint!(105_U256), uint!(1_000_U256)];

    assert_eq!(
        average_non_outlier_anchor_rates(&rates),
        Some(uint!(102_U256))
    );
}

#[test]
fn average_non_outlier_anchor_rates_returns_none_without_survivors() {
    let rates = [uint!(100_U256), uint!(1_000_U256)];

    assert_eq!(average_non_outlier_anchor_rates(&rates), None);
}

#[test]
fn cached_pair_requires_configured_rates_and_uses_fixed_sources_without_a_read() {
    let cache = TokenAnchorRateCache::new();
    let tokens = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let weth = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    assert_eq!(
        cache.cached_pair_rate(1, weth, usdc, &tokens),
        Err(AnchorBlocked {
            token: usdc,
            failures: vec![AnchorReadFailure::NoRate]
        }),
    );
    cache.store_rate(1, usdc, uint!(3_000_000_000_U256));
    assert_eq!(
        cache.cached_pair_rate(1, weth, usdc, &tokens),
        Ok(Some(PairAnchorRate {
            sell_rate: WRAPPED_NATIVE_FEE_RATE,
            buy_rate: uint!(3_000_000_000_U256)
        })),
    );
    let unconfigured = EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    assert_eq!(
        cache.cached_pair_rate(1, weth, usdc, &unconfigured),
        Ok(None)
    );
}

#[test]
fn cached_bridge_leg_rate_compares_usd_values_across_chains() {
    let cache = TokenAnchorRateCache::new();
    let tokens = crate::settings::build_effective_token_registry(
        &crate::settings::WalletSettings::default(),
    )
    .unwrap();
    let arb_weth = address!("82af49447d8a07e3bd95bd0d56f35241523fbab1");
    let bsc_usdc = address!("8ac76a51cc950d9822d68b83fe1ad97b32cd580d");
    let leg_rate = || cache.cached_bridge_leg_rate(42161, arb_weth, 56, bsc_usdc, &tokens);
    // 600 USDC per BNB at 600 USD per BNB, but no native USD rate on Arbitrum One yet.
    cache.store_rate(56, bsc_usdc, uint!(600_000_000_000_000_000_000_U256));
    cache.store_native_usd_rate(56, uint!(600_000_000_U256), 18);
    assert_eq!(
        leg_rate(),
        Err(AnchorBlocked {
            token: arb_weth,
            failures: vec![AnchorReadFailure::NoRate]
        })
    );
    // At 3,000 USD per ETH, one WETH is worth exactly 3,000 USDC on BNB Chain.
    cache.store_native_usd_rate(42161, uint!(3_000_000_000_U256), 18);
    let rate = leg_rate().unwrap().unwrap();
    let usdc = uint!(3_000_000_000_000_000_000_000_U256);
    assert_eq!(
        check_quote_against_anchor(WRAPPED_NATIVE_FEE_RATE, usdc, rate, 0),
        Ok(())
    );
    assert_eq!(
        check_quote_against_anchor(WRAPPED_NATIVE_FEE_RATE, usdc - U256::ONE, rate, 0),
        Err(QuoteDeviationError::ExceedsThreshold)
    );
}

#[test]
fn cache_keeps_stale_rate_when_refresh_has_no_usable_value() {
    let cache = TokenAnchorRateCache::new();
    let token = address!("0x0000000000000000000000000000000000000001");
    let entry = RuntimeTokenAnchorInfo {
        chain_id: 1,
        token,
        anchor_sources: runtime_sources(SHARED_ORACLE_SOURCE_6),
    };
    cache.store_rate(1, token, uint!(123_U256));

    store_anchor_rates_from_entries_with_inputs(
        &cache,
        &[entry],
        &BTreeMap::new(),
        &TwapFetchedInputs::default(),
    );

    assert_eq!(cache.cached_rate(1, token), Some(uint!(123_U256)));
    assert_eq!(
        cache.cached_rate(1, address!("0x0000000000000000000000000000000000000002")),
        None
    );
    let twap_token = address!("0x0000000000000000000000000000000000000003");
    let twap_entry = RuntimeTokenAnchorInfo {
        chain_id: 1,
        token: twap_token,
        anchor_sources: vec![twap_source(1_800)],
    };

    store_anchor_rates_from_entries_with_inputs(
        &cache,
        std::slice::from_ref(&twap_entry),
        &BTreeMap::new(),
        &TwapFetchedInputs::default(),
    );
    assert_eq!(cache.cached_rate(1, twap_token), None);

    let mut usable_inputs = TwapFetchedInputs::default();
    usable_inputs.metadata.insert(
        PoolKey {
            chain_id: 1,
            pool: TWAP_POOL,
        },
        PoolMetadata {
            token0: TWAP_BASE,
            token1: TWAP_QUOTE,
        },
    );
    usable_inputs.observations.insert(
        ObservationKey {
            chain_id: 1,
            pool: TWAP_POOL,
            window_seconds: 1_800,
        },
        TwapObservation {
            tick_cumulatives: vec![0, 0],
        },
    );
    store_anchor_rates_from_entries_with_inputs(
        &cache,
        std::slice::from_ref(&twap_entry),
        &BTreeMap::new(),
        &usable_inputs,
    );
    assert_eq!(
        cache.cached_rate(1, twap_token),
        Some(uint!(1_000_000_000_000_000_000_U256))
    );

    store_anchor_rates_from_entries_with_inputs(
        &cache,
        std::slice::from_ref(&twap_entry),
        &BTreeMap::new(),
        &TwapFetchedInputs::default(),
    );
    assert_eq!(
        cache.cached_rate(1, twap_token),
        Some(uint!(1_000_000_000_000_000_000_U256))
    );
}

#[test]
fn cached_usd_values_exclude_testnets_even_with_populated_rates() {
    let cache = TokenAnchorRateCache::new();
    let token = Address::repeat_byte(7);
    let token_rate = uint!(1_000_000_000_000_000_000_U256);
    let native_rate = uint!(3_000_000_000_U256);
    let amount = uint!(1_000_000_000_000_000_000_U256);

    cache.store_native_usd_rate(1, U256::ZERO, 18);
    assert_eq!(cache.cached_native_usd_rate(1), None);

    for chain_id in [1, 11_155_111] {
        cache.store_native_usd_rate(chain_id, native_rate, 18);
        cache.store_rate(chain_id, token, token_rate);
        // Token-to-native rates remain available for broadcaster gas fees.
        assert_eq!(cache.cached_rate(chain_id, token), Some(token_rate));
    }
    assert_eq!(cache.cached_native_usd_rate(1), Some(native_rate));
    assert_eq!(cache.cached_native_usd_rate(56), None);
    assert_eq!(cache.cached_native_usd_rate(11_155_111), None);
    assert_eq!(
        cache.cached_native_usd_micro_value(1, amount),
        Some(native_rate)
    );
    assert_eq!(
        cache.cached_token_usd_micro_value(1, token, amount),
        Some(native_rate)
    );
    assert_eq!(
        cache.cached_native_usd_micro_value(11_155_111, amount),
        None
    );
    assert_eq!(
        cache.cached_token_usd_micro_value(11_155_111, token, amount),
        None
    );
}

#[test]
fn missing_token_rates_use_fast_refresh_retry() {
    let cache = TokenAnchorRateCache::new();
    let settings = crate::settings::WalletSettings::default();
    let token_registry = crate::settings::build_effective_token_registry(&settings)
        .expect("effective token registry");

    assert_eq!(
        token_anchor_refresh_delay(&cache, &[1], &token_registry),
        TOKEN_ANCHOR_MISSING_RATE_RETRY_INTERVAL
    );

    for entry in token_anchor_entries_for_chains(&[1], &token_registry) {
        cache.store_rate(entry.chain_id, entry.token, U256::ONE);
    }
    assert_eq!(
        token_anchor_refresh_delay(&cache, &[1], &token_registry),
        TOKEN_ANCHOR_REFRESH_INTERVAL
    );
    assert_eq!(
        token_anchor_refresh_delay(&cache, &[1, 56], &token_registry),
        TOKEN_ANCHOR_MISSING_RATE_RETRY_INTERVAL
    );
}

#[tokio::test]
async fn oracle_multicall_timeout_bounds_unresponsive_provider() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind unresponsive RPC listener");
    let rpc_url = format!(
        "http://{}",
        listener.local_addr().expect("RPC listener address")
    );
    let settings = crate::settings::WalletSettings::default();
    let mut effective_chains =
        crate::settings::build_effective_chain_configs(&settings).expect("effective chain configs");
    let multicall = effective_chains
        .get(1)
        .and_then(|chain| chain.rpc_route.multicall())
        .expect("multicall");
    effective_chains
        .get_mut(1)
        .expect("Ethereum config")
        .rpc_route = RpcChainRoute::new(1, vec![reqwest::Url::parse(&rpc_url).expect("RPC URL")])
        .with_multicall(multicall);
    let data_dir = std::env::temp_dir();
    let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
        network_mode: Some(crate::WalletNetworkMode::Direct),
        proxy: None,
        data_dir: &data_dir,
    })
    .await
    .expect("direct HTTP context");

    let error = fetch_oracle_answers_for_chain_with_timeout(
        1,
        &[address!("0x0000000000000000000000000000000000000100")],
        &effective_chains,
        None,
        &http,
        Duration::from_millis(25),
    )
    .await
    .expect_err("unresponsive oracle RPC must time out");

    assert!(
        matches!(
            error.downcast_ref::<RpcBrokerError>(),
            Some(RpcBrokerError::Timeout | RpcBrokerError::TimeoutBeforeDispatch)
        ),
        "expected a request timeout, got {error:#}"
    );
}

#[tokio::test]
async fn twap_multicall_timeout_bounds_unresponsive_provider() {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind unresponsive RPC listener");
    let rpc_url = format!(
        "http://{}",
        listener.local_addr().expect("RPC listener address")
    );
    let settings = crate::settings::WalletSettings::default();
    let mut effective_chains =
        crate::settings::build_effective_chain_configs(&settings).expect("effective chain configs");
    let multicall = effective_chains
        .get(1)
        .and_then(|chain| chain.rpc_route.multicall())
        .expect("multicall");
    effective_chains
        .get_mut(1)
        .expect("Ethereum config")
        .rpc_route = RpcChainRoute::new(1, vec![reqwest::Url::parse(&rpc_url).expect("RPC URL")])
        .with_multicall(multicall);
    let data_dir = std::env::temp_dir();
    let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
        network_mode: Some(crate::WalletNetworkMode::Direct),
        proxy: None,
        data_dir: &data_dir,
    })
    .await
    .expect("direct HTTP context");
    let error = fetch_twap_inputs_for_chain_with_timeout(
        1,
        &[PoolKey {
            chain_id: 1,
            pool: TWAP_POOL,
        }],
        &[ObservationKey {
            chain_id: 1,
            pool: TWAP_POOL,
            window_seconds: 1_800,
        }],
        &effective_chains,
        None,
        &http,
        Duration::from_millis(25),
    )
    .await
    .expect_err("unresponsive TWAP RPC must time out");
    assert!(
        matches!(
            error.downcast_ref::<RpcBrokerError>(),
            Some(RpcBrokerError::Timeout | RpcBrokerError::TimeoutBeforeDispatch)
        ),
        "expected a request timeout, got {error:#}"
    );
}

#[tokio::test]
async fn twap_fetch_decodes_two_typed_batches_and_routes_non_ethereum_chain() {
    let multicall = address!("0x0000000000000000000000000000000000000600");
    let (rpc_url, requests, server) =
        spawn_twap_rpc_fixture(multicall, TWAP_BASE, TWAP_QUOTE, 1_800, 2, false);
    let settings = crate::settings::WalletSettings::default();
    let mut effective_chains =
        crate::settings::build_effective_chain_configs(&settings).expect("effective chain configs");
    effective_chains
        .get_mut(42161)
        .expect("Arbitrum config")
        .rpc_route =
        RpcChainRoute::new(42161, vec![reqwest::Url::parse(&rpc_url).expect("RPC URL")])
            .with_multicall(multicall);
    let data_dir = std::env::temp_dir();
    let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
        network_mode: Some(crate::WalletNetworkMode::Direct),
        proxy: None,
        data_dir: &data_dir,
    })
    .await
    .expect("direct HTTP context");
    let pool_key = PoolKey {
        chain_id: 42161,
        pool: TWAP_POOL,
    };
    let observation_key = ObservationKey {
        chain_id: 42161,
        pool: TWAP_POOL,
        window_seconds: 1_800,
    };
    let inputs = fetch_twap_inputs_for_chain_with_timeout(
        42161,
        &[pool_key],
        &[observation_key],
        &effective_chains,
        None,
        &http,
        Duration::from_secs(2),
    )
    .await
    .expect("typed TWAP batches");
    assert_eq!(
        inputs
            .metadata
            .get(&pool_key)
            .map(|metadata| metadata.token0),
        Some(TWAP_BASE)
    );
    assert_eq!(
        inputs
            .metadata
            .get(&pool_key)
            .map(|metadata| metadata.token1),
        Some(TWAP_QUOTE)
    );
    assert_eq!(
        inputs
            .observations
            .get(&observation_key)
            .map(|observation| observation.tick_cumulatives.as_slice()),
        Some([0_i128, 0_i128].as_slice())
    );
    for label in ["metadata request", "observation request"] {
        assert_eq!(
            requests.recv().expect(label)["params"][0]["to"]
                .as_str()
                .and_then(|value| value.parse::<Address>().ok()),
            Some(multicall)
        );
    }
    server.join().expect("fixture server");
}

#[tokio::test]
async fn twap_fetch_isolates_individual_pool_and_observation_reverts() {
    let multicall = address!("0x0000000000000000000000000000000000000601");
    let (rpc_url, _requests, server) =
        spawn_twap_rpc_fixture(multicall, TWAP_BASE, TWAP_QUOTE, 1_800, 2, true);
    let settings = crate::settings::WalletSettings::default();
    let mut effective_chains =
        crate::settings::build_effective_chain_configs(&settings).expect("effective chain configs");
    effective_chains
        .get_mut(42161)
        .expect("Arbitrum config")
        .rpc_route =
        RpcChainRoute::new(42161, vec![reqwest::Url::parse(&rpc_url).expect("RPC URL")])
            .with_multicall(multicall);
    let data_dir = std::env::temp_dir();
    let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
        network_mode: Some(crate::WalletNetworkMode::Direct),
        proxy: None,
        data_dir: &data_dir,
    })
    .await
    .expect("direct HTTP context");
    let first_pool = PoolKey {
        chain_id: 42161,
        pool: TWAP_POOL,
    };
    let second_pool = PoolKey {
        chain_id: 42161,
        pool: address!("0x0000000000000000000000000000000000000403"),
    };
    let first_observation = ObservationKey {
        chain_id: 42161,
        pool: TWAP_POOL,
        window_seconds: 1_800,
    };
    let second_observation = ObservationKey {
        chain_id: 42161,
        pool: second_pool.pool,
        window_seconds: 1_800,
    };
    let inputs = fetch_twap_inputs_for_chain_with_timeout(
        42161,
        &[first_pool, second_pool],
        &[first_observation, second_observation],
        &effective_chains,
        None,
        &http,
        Duration::from_secs(2),
    )
    .await
    .expect("partial TWAP batches");
    assert!(inputs.metadata.contains_key(&first_pool));
    assert!(!inputs.metadata.contains_key(&second_pool));
    assert!(inputs.observations.contains_key(&first_observation));
    assert!(!inputs.observations.contains_key(&second_observation));
    server.join().expect("fixture server");
}

#[tokio::test]
async fn twap_fetch_marks_failed_provider_and_fails_over_deterministically() {
    let multicall = address!("0x0000000000000000000000000000000000000602");
    let dead_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("dead provider");
    let dead_url = format!(
        "http://{}",
        dead_listener.local_addr().expect("dead address")
    );
    drop(dead_listener);
    let (healthy_url, requests, server) =
        spawn_twap_rpc_fixture(multicall, TWAP_BASE, TWAP_QUOTE, 1_800, 2, false);
    let settings = crate::settings::WalletSettings::default();
    let mut effective_chains =
        crate::settings::build_effective_chain_configs(&settings).expect("effective chain configs");
    effective_chains
        .get_mut(42161)
        .expect("Arbitrum config")
        .rpc_route = RpcChainRoute::new(
        42161,
        vec![
            reqwest::Url::parse(&dead_url).expect("RPC URL"),
            reqwest::Url::parse(&healthy_url).expect("RPC URL"),
        ],
    )
    .with_multicall(multicall);
    let data_dir = std::env::temp_dir();
    let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
        network_mode: Some(crate::WalletNetworkMode::Direct),
        proxy: None,
        data_dir: &data_dir,
    })
    .await
    .expect("direct HTTP context");
    let pool_key = PoolKey {
        chain_id: 42161,
        pool: TWAP_POOL,
    };
    let observation_key = ObservationKey {
        chain_id: 42161,
        pool: TWAP_POOL,
        window_seconds: 1_800,
    };
    let inputs = fetch_twap_inputs_for_chain_with_timeout(
        42161,
        &[pool_key],
        &[observation_key],
        &effective_chains,
        None,
        &http,
        Duration::from_secs(2),
    )
    .await
    .expect("healthy failover result");
    assert!(inputs.metadata.contains_key(&pool_key));
    assert!(inputs.observations.contains_key(&observation_key));
    assert!(requests.recv().is_ok());
    assert!(requests.recv().is_ok());
    server.join().expect("fixture server");
}

#[tokio::test]
async fn successful_oracle_chain_rates_survive_later_source_timeout() {
    let cache = TokenAnchorRateCache::new();
    let token = address!("0x0000000000000000000000000000000000000001");
    let pending_token = address!("0x0000000000000000000000000000000000000002");
    let successful_oracle = address!("0x0000000000000000000000000000000000000100");
    let pending_oracle = address!("0x0000000000000000000000000000000000000200");
    let successful_source = RuntimeTokenAnchorSource::ChainlinkOracle {
        chain_id: 1,
        addr: successful_oracle,
        token_decimals: 6,
        oracle_decimals: 8,
        is_inversed: false,
    };
    let entries = [
        RuntimeTokenAnchorInfo {
            chain_id: 42,
            token,
            anchor_sources: vec![successful_source],
        },
        RuntimeTokenAnchorInfo {
            chain_id: 42,
            token: pending_token,
            anchor_sources: vec![RuntimeTokenAnchorSource::ChainlinkOracle {
                chain_id: 2,
                addr: pending_oracle,
                token_decimals: 6,
                oracle_decimals: 8,
                is_inversed: false,
            }],
        },
    ];
    let oracle_addresses_by_chain = oracle_addresses_for_entries(&entries);

    let refresh = refresh_token_anchor_rates_with_fetch(
        &cache,
        &entries,
        oracle_addresses_by_chain,
        BTreeSet::new(),
        BTreeSet::new(),
        Duration::from_millis(25),
        move |plan| {
            Box::pin(async move {
                let AnchorSourcePlan::Chainlink {
                    chain_id,
                    addresses,
                } = plan
                else {
                    unreachable!("oracle-only test plan")
                };
                if chain_id == 1 {
                    assert_eq!(addresses, vec![successful_oracle]);
                    AnchorSourceResult::Chainlink {
                        chain_id,
                        result: Ok(BTreeMap::from([(
                            successful_oracle,
                            uint!(3_000_00000000_U256),
                        )])),
                    }
                } else {
                    assert_eq!(chain_id, 2);
                    assert_eq!(addresses, vec![pending_oracle]);
                    std::future::pending::<AnchorSourceResult>().await
                }
            })
        },
    );

    timeout(Duration::from_millis(250), refresh)
        .await
        .expect("refresh respects internal deadline");
    assert_eq!(
        cache.cached_rate(42, token),
        Some(uint!(3_000_000_000_U256))
    );
    assert_eq!(cache.cached_rate(42, pending_token), None);
}

/// One production refresh cycle whose Chainlink reads go through `http`'s broker.
async fn refresh_oracle_anchors_through_broker(
    cache: &TokenAnchorRateCache,
    entries: &[RuntimeTokenAnchorInfo],
    effective_chains: &EffectiveChainRegistry,
    http: &HttpContext,
) {
    refresh_token_anchor_rates_with_fetch(
        cache,
        entries,
        oracle_addresses_for_entries(entries),
        BTreeSet::new(),
        BTreeSet::new(),
        TOKEN_ANCHOR_CHAIN_REFRESH_TIMEOUT,
        |plan| {
            Box::pin(async move {
                let AnchorSourcePlan::Chainlink {
                    chain_id,
                    addresses,
                } = plan
                else {
                    unreachable!("oracle-only test plan")
                };
                AnchorSourceResult::Chainlink {
                    chain_id,
                    result: fetch_oracle_answers_for_chain(
                        chain_id,
                        &addresses,
                        effective_chains,
                        None,
                        http,
                    )
                    .await,
                }
            })
        },
    )
    .await;
}

#[tokio::test]
async fn oracle_refresh_uses_verified_endpoint_while_other_identity_checks_stall() {
    use alloy::primitives::I256;
    use alloy::providers::bindings::IMulticall3;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const STALLED: usize = 10;
    // Far below the stalled checks' identity timeout and the oracle request deadline.
    const PROMPT: Duration = Duration::from_secs(3);
    let oracle = address!("0x0000000000000000000000000000000000000100");
    let token = address!("0x0000000000000000000000000000000000000001");
    let settings = crate::settings::WalletSettings::default();
    let mut effective_chains =
        crate::settings::build_effective_chain_configs(&settings).expect("effective chain configs");
    let multicall = effective_chains
        .get(1)
        .and_then(|chain| chain.rpc_route.multicall())
        .expect("multicall");

    // Listeners that never accept hold every identity check sent to them until they drop.
    let stalled = (0..STALLED)
        .map(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind stalled RPC"))
        .collect::<Vec<_>>();
    let mut endpoints = stalled
        .iter()
        .map(|listener| {
            reqwest::Url::parse(&format!(
                "http://{}",
                listener.local_addr().expect("stalled RPC address")
            ))
            .expect("stalled RPC URL")
        })
        .collect::<Vec<_>>();
    let answer = SharedArc::new(AtomicU64::new(300_000_000_000));
    let mut identity_checks = Vec::new();
    let mut reads = Vec::new();
    let mut servers = Vec::new();
    for _ in STALLED..2 * STALLED {
        let endpoint_identity_checks = SharedArc::new(AtomicUsize::new(0));
        let endpoint_reads = SharedArc::new(AtomicUsize::new(0));
        let (identity_count, read_count, answer) = (
            endpoint_identity_checks.clone(),
            endpoint_reads.clone(),
            answer.clone(),
        );
        let (url, server) = crate::rpc_broker::tests::spawn_rpc_mock(
            SharedArc::new(move |request: Value| {
                let result = match request["method"].as_str() {
                    Some("eth_chainId") => {
                        identity_count.fetch_add(1, Ordering::SeqCst);
                        json!("0x1")
                    }
                    Some("eth_call") => {
                        read_count.fetch_add(1, Ordering::SeqCst);
                        let encoded = AggregatorInterface::latestAnswerCall::abi_encode_returns(
                            &I256::try_from(answer.load(Ordering::SeqCst)).expect("oracle answer"),
                        );
                        let params = &request["params"][0];
                        let to = params["to"]
                            .as_str()
                            .and_then(|value| value.parse::<Address>().ok());
                        if to == Some(multicall) {
                            let call_data = params
                                .get("input")
                                .or_else(|| params.get("data"))
                                .and_then(Value::as_str)
                                .expect("eth_call calldata missing input/data")
                                .parse::<Bytes>()
                                .expect("call bytes");
                            let returns = IMulticall3::tryAggregateCall::abi_decode(&call_data)
                                .expect("tryAggregate calldata")
                                .calls
                                .iter()
                                .map(|_| IMulticall3::Result {
                                    success: true,
                                    returnData: encoded.clone().into(),
                                })
                                .collect::<Vec<_>>();
                            json!(Bytes::from(
                                IMulticall3::tryAggregateCall::abi_encode_returns(&returns)
                            ))
                        } else {
                            json!(Bytes::from(encoded))
                        }
                    }
                    _ => {
                        return json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "error": {"code": -32601, "message": "unexpected method"}
                        });
                    }
                };
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            }),
            SharedArc::default(),
            SharedArc::default(),
        )
        .await;
        endpoints.push(url);
        identity_checks.push(endpoint_identity_checks);
        reads.push(endpoint_reads);
        servers.push(server);
    }
    effective_chains
        .get_mut(1)
        .expect("Ethereum config")
        .rpc_route = RpcChainRoute::new(1, endpoints)
        .with_multicall(multicall)
        .with_identity_verification();
    // Configured order would fill the three-probe window with stalled endpoints; moving the
    // first healthy endpoint into it probes stalled endpoints 0 and 1 beside endpoint 10.
    let http = HttpContext::direct_for_tests_with_candidate_order(|order| {
        if let Some(position) = order.iter().position(|&index| index == STALLED) {
            order.swap(2, position);
        }
    });
    let entries = [RuntimeTokenAnchorInfo {
        chain_id: 1,
        token,
        anchor_sources: vec![RuntimeTokenAnchorSource::ChainlinkOracle {
            chain_id: 1,
            addr: oracle,
            token_decimals: 6,
            oracle_decimals: 8,
            is_inversed: false,
        }],
    }];
    let cache = TokenAnchorRateCache::new();

    timeout(
        PROMPT,
        refresh_oracle_anchors_through_broker(&cache, &entries, &effective_chains, &http),
    )
    .await
    .expect("startup refresh does not wait for stalled identity checks");
    assert_eq!(cache.cached_rate(1, token), Some(uint!(3_000_000_000_U256)));
    assert_eq!(identity_checks[0].load(Ordering::SeqCst), 1);
    assert_eq!(reads[0].load(Ordering::SeqCst), 1);

    // Latest reads are not result-cached without an observed head, so this read reaches an
    // endpoint again while the stalled checks remain unresolved.
    answer.store(310_000_000_000, Ordering::SeqCst);
    timeout(
        PROMPT,
        refresh_oracle_anchors_through_broker(&cache, &entries, &effective_chains, &http),
    )
    .await
    .expect("later refresh reuses the verified identity");
    assert_eq!(cache.cached_rate(1, token), Some(uint!(3_100_000_000_U256)));
    assert_eq!(identity_checks[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        reads
            .iter()
            .map(|count| count.load(Ordering::SeqCst))
            .sum::<usize>(),
        2
    );

    drop(http);
    drop(stalled);
    for server in servers {
        server.abort();
    }
}

#[tokio::test]
async fn anchor_source_fetch_warnings_redact_opaque_errors() {
    let logs = SharedArc::new(Mutex::new(Vec::new()));
    let writer_logs = SharedArc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || SharedLogWriter(SharedArc::clone(&writer_logs)))
        .finish();
    let cache = TokenAnchorRateCache::new();
    let chainlink_sentinel = "https://chainlink-user:chainlink-secret@rpc.invalid";
    let twap_sentinel = "https://twap-user:twap-secret@rpc.invalid";
    let chainlink_chain_id = 101;
    let twap_source_chain = 202;
    let chainlink_address = address!("0x0000000000000000000000000000000000000500");
    let pool = PoolKey {
        chain_id: twap_source_chain,
        pool: address!("0x0000000000000000000000000000000000000501"),
    };
    let observation = ObservationKey {
        chain_id: twap_source_chain,
        pool: pool.pool,
        window_seconds: 1_800,
    };

    async {
        refresh_token_anchor_rates_with_fetch(
            &cache,
            &[],
            BTreeMap::from([(chainlink_chain_id, vec![chainlink_address])]),
            BTreeSet::from([pool]),
            BTreeSet::from([observation]),
            Duration::from_secs(1),
            move |plan| {
                Box::pin(async move {
                    match plan {
                        AnchorSourcePlan::Chainlink { chain_id, .. } => {
                            assert_eq!(chain_id, chainlink_chain_id);
                            AnchorSourceResult::Chainlink {
                                chain_id,
                                result: Err(eyre::eyre!("provider failed at {chainlink_sentinel}")),
                            }
                        }
                        AnchorSourcePlan::Twap { chain_id, .. } => {
                            assert_eq!(chain_id, twap_source_chain);
                            AnchorSourceResult::Twap {
                                chain_id,
                                result: Err(eyre::eyre!("provider failed at {twap_sentinel}")),
                            }
                        }
                    }
                })
            },
        )
        .await;
    }
    .with_subscriber(subscriber)
    .await;

    let output = String::from_utf8(logs.lock().expect("log buffer lock").clone())
        .expect("tracing output is UTF-8");
    assert!(output.contains("anchor source fetch failed"));
    assert!(output.contains("source_kind=\"chainlink\""));
    assert!(output.contains("source_kind=\"uniswap_v3_twap\""));
    assert!(output.contains("chain_id=101"));
    assert!(output.contains("chain_id=202"));
    assert!(!output.contains(chainlink_sentinel));
    assert!(!output.contains(twap_sentinel));
}

#[test]
fn cache_refresh_notifications_increment_generation() {
    let cache = TokenAnchorRateCache::new();
    let mut refresh_rx = cache.subscribe_refreshes();

    assert_eq!(*refresh_rx.borrow_and_update(), 0);

    cache.notify_refreshed();

    assert!(refresh_rx.has_changed().expect("watch channel open"));
    assert_eq!(*refresh_rx.borrow_and_update(), 1);
}

#[test]
fn oracle_addresses_for_entries_deduplicates_shared_sources() {
    let entries = [
        RuntimeTokenAnchorInfo {
            chain_id: 1,
            token: address!("0x0000000000000000000000000000000000000001"),
            anchor_sources: runtime_sources(SHARED_ORACLE_SOURCE_6),
        },
        RuntimeTokenAnchorInfo {
            chain_id: 1,
            token: address!("0x0000000000000000000000000000000000000002"),
            anchor_sources: runtime_sources(SHARED_ORACLE_SOURCE_18),
        },
        RuntimeTokenAnchorInfo {
            chain_id: 1,
            token: address!("0x0000000000000000000000000000000000000003"),
            anchor_sources: runtime_sources(ARB_PER_ETH_ANCHOR_SOURCE),
        },
    ];

    assert_eq!(
        oracle_addresses_for_entries(&entries),
        BTreeMap::from([(
            1,
            vec![
                address!("0x0000000000000000000000000000000000000100"),
                address!("0x0000000000000000000000000000000000000200"),
                address!("0x0000000000000000000000000000000000000300"),
            ],
        )])
    );
}

#[test]
fn twap_planning_deduplicates_pool_and_observation_keys() {
    let source = twap_source(1_800);
    let different_window = twap_source(900);
    let entries = [
        RuntimeTokenAnchorInfo {
            chain_id: 1,
            token: TWAP_QUOTE,
            anchor_sources: vec![source.clone(), source.clone(), different_window],
        },
        RuntimeTokenAnchorInfo {
            chain_id: 1,
            token: TWAP_BASE,
            anchor_sources: vec![RuntimeTokenAnchorSource::Product {
                sources: vec![source.clone(), source],
                scale_decimals: 18,
            }],
        },
        RuntimeTokenAnchorInfo {
            chain_id: 2,
            token: address!("0x0000000000000000000000000000000000000004"),
            anchor_sources: vec![twap_source(1_800)],
        },
    ];
    let (pools, observations) = twap_keys_for_entries(&entries);
    assert_eq!(pools.len(), 2);
    assert_eq!(observations.len(), 3);
    assert!(observations.contains(&ObservationKey {
        chain_id: 1,
        pool: TWAP_POOL,
        window_seconds: 1_800,
    }));
    assert!(observations.contains(&ObservationKey {
        chain_id: 1,
        pool: TWAP_POOL,
        window_seconds: 900,
    }));
    assert!(pools.contains(&PoolKey {
        chain_id: 2,
        pool: TWAP_POOL,
    }));
}

#[tokio::test]
async fn scheduler_reuses_one_source_fetch_for_different_targets_on_one_chain() {
    let direct_token = address!("0x0000000000000000000000000000000000000410");
    let product_token = address!("0x0000000000000000000000000000000000000411");
    let direct_entry = RuntimeTokenAnchorInfo {
        chain_id: 10,
        token: direct_token,
        anchor_sources: vec![twap_source(1_800)],
    };
    let product_entry = RuntimeTokenAnchorInfo {
        chain_id: 10,
        token: product_token,
        anchor_sources: vec![RuntimeTokenAnchorSource::Product {
            sources: vec![
                twap_source(1_800),
                RuntimeTokenAnchorSource::Fixed {
                    token_fee_per_unit_gas: uint!(1_000_000_000_000_000_000_U256),
                },
            ],
            scale_decimals: 18,
        }],
    };
    let entries = [direct_entry, product_entry];
    let (pool_keys, observation_keys) = twap_keys_for_entries(&entries);
    let cache = TokenAnchorRateCache::new();
    let mut fetch_count = 0;
    refresh_token_anchor_rates_with_fetch(
        &cache,
        &entries,
        BTreeMap::new(),
        pool_keys,
        observation_keys,
        Duration::from_secs(1),
        |plan| {
            let AnchorSourcePlan::Twap {
                chain_id,
                pools,
                observations,
            } = plan
            else {
                unreachable!("TWAP-only scheduler test plan")
            };
            fetch_count += 1;
            Box::pin(async move {
                let mut inputs = TwapFetchedInputs::default();
                inputs.metadata.insert(
                    PoolKey {
                        chain_id,
                        pool: TWAP_POOL,
                    },
                    PoolMetadata {
                        token0: TWAP_BASE,
                        token1: TWAP_QUOTE,
                    },
                );
                inputs.observations.insert(
                    ObservationKey {
                        chain_id,
                        pool: TWAP_POOL,
                        window_seconds: observations[0].window_seconds,
                    },
                    TwapObservation {
                        tick_cumulatives: vec![0, 0],
                    },
                );
                assert_eq!(pools.len(), 1);
                AnchorSourceResult::Twap {
                    chain_id,
                    result: Ok(inputs),
                }
            })
        },
    )
    .await;
    assert_eq!(fetch_count, 1);
    assert_eq!(
        cache.cached_rate(10, direct_token),
        Some(uint!(1_000_000_000_000_000_000_U256))
    );
    assert_eq!(
        cache.cached_rate(10, product_token),
        Some(uint!(1_000_000_000_000_000_000_U256))
    );
}

#[test]
fn twap_observation_abi_preserves_signed_i56_cumulatives() {
    type ObserveReturn = <UniswapV3PoolInterface::observeCall as SolCall>::Return;
    let returns = ObserveReturn {
        tickCumulatives: vec![
            I56::try_from(-123_i128).expect("negative tick cumulative fits I56"),
            I56::try_from(456_i128).expect("positive tick cumulative fits I56"),
        ],
        secondsPerLiquidityCumulativeX128s: vec![U160::ZERO, U160::ZERO],
    };
    let decoded = UniswapV3PoolInterface::observeCall::abi_decode_returns(
        &UniswapV3PoolInterface::observeCall::abi_encode_returns(&returns),
    )
    .expect("decode signed observation");
    let converted = decoded
        .tickCumulatives
        .into_iter()
        .map(i128::try_from)
        .collect::<std::result::Result<Vec<_>, _>>()
        .expect("I56 values fit in i128");
    assert_eq!(converted, vec![-123, 456]);
}

#[test]
fn twap_evaluation_checks_orientation_pair_and_observation_shape() {
    let mut inputs = TwapFetchedInputs::default();
    inputs.metadata.insert(
        PoolKey {
            chain_id: 1,
            pool: TWAP_POOL,
        },
        PoolMetadata {
            token0: TWAP_QUOTE,
            token1: TWAP_BASE,
        },
    );
    inputs.observations.insert(
        ObservationKey {
            chain_id: 1,
            pool: TWAP_POOL,
            window_seconds: 1_800,
        },
        TwapObservation {
            tick_cumulatives: vec![0, 1_800],
        },
    );
    let source = twap_source(1_800);
    assert!(matches!(
        anchor_rate_from_source_with_inputs(1, &source, &BTreeMap::new(), &inputs),
        Some(rate) if rate > U256::ZERO && rate < uint!(1_000_000_000_000_000_000_U256)
    ));
    inputs.metadata.insert(
        PoolKey {
            chain_id: 1,
            pool: TWAP_POOL,
        },
        PoolMetadata {
            token0: TWAP_QUOTE,
            token1: address!("0x0000000000000000000000000000000000000403"),
        },
    );
    assert_eq!(
        anchor_rate_from_source_with_inputs(1, &source, &BTreeMap::new(), &inputs),
        None
    );
    inputs.metadata.insert(
        PoolKey {
            chain_id: 1,
            pool: TWAP_POOL,
        },
        PoolMetadata {
            token0: TWAP_BASE,
            token1: TWAP_QUOTE,
        },
    );
    inputs.observations.insert(
        ObservationKey {
            chain_id: 1,
            pool: TWAP_POOL,
            window_seconds: 1_800,
        },
        TwapObservation {
            tick_cumulatives: vec![0, 1_800],
        },
    );
    assert!(matches!(
        anchor_rate_from_source_with_inputs(1, &source, &BTreeMap::new(), &inputs),
        Some(rate) if rate > uint!(1_000_000_000_000_000_000_U256)
    ));
    inputs.observations.insert(
        ObservationKey {
            chain_id: 1,
            pool: TWAP_POOL,
            window_seconds: 1_800,
        },
        TwapObservation {
            tick_cumulatives: vec![0],
        },
    );
    assert_eq!(
        anchor_rate_from_source_with_inputs(1, &source, &BTreeMap::new(), &inputs),
        None
    );
}

#[test]
fn token_anchor_entries_apply_effective_registry_overrides() {
    let mut settings = crate::settings::WalletSettings::default();
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let custom = address!("0x0000000000000000000000000000000000000002");
    settings
        .tokens
        .built_in_tombstones
        .push(crate::settings::TokenKey {
            chain_id: 1,
            token_address: weth.to_string(),
        });
    settings
        .tokens
        .custom_tokens
        .push(crate::settings::CustomTokenSettings {
            chain_id: 1,
            token_address: custom.to_string(),
            symbol: "CSTM".to_string(),
            decimals: 18,
            icon_path: None,
            price_anchor: Some(crate::settings::PriceAnchorSettings::Oracle {
                chain_id: 42161,
                oracle_address: "0x0000000000000000000000000000000000000100".to_string(),
                token_decimals: 18,
                oracle_decimals: 8,
                is_inversed: false,
            }),
        });
    let registry = crate::settings::build_effective_token_registry(&settings)
        .expect("effective token registry");

    let entries = token_anchor_entries_for_chains(&[1], &registry);

    assert!(!entries.iter().any(|entry| entry.token == weth));
    let custom_entry = entries
        .iter()
        .find(|entry| entry.token == custom)
        .expect("custom anchor entry");
    let oracle_addresses = oracle_addresses_for_entries(std::slice::from_ref(custom_entry));
    assert_eq!(
        oracle_addresses,
        BTreeMap::from([(
            42161,
            vec![address!("0x0000000000000000000000000000000000000100")],
        )])
    );
}

#[test]
fn anchor_rates_from_sources_with_inputs_reuses_oracle_answer() {
    let mut answers = BTreeMap::new();
    answers.insert(
        (1, address!("0x0000000000000000000000000000000000000100")),
        uint!(3_000_00000000_U256),
    );

    assert_eq!(
        anchor_rates_from_sources_with_inputs(
            1,
            &runtime_sources(SHARED_ORACLE_SOURCE_6),
            &answers,
            &TwapFetchedInputs::default(),
        ),
        vec![uint!(3_000_000_000_U256)]
    );
    assert_eq!(
        anchor_rates_from_sources_with_inputs(
            1,
            &runtime_sources(SHARED_ORACLE_SOURCE_18),
            &answers,
            &TwapFetchedInputs::default(),
        ),
        vec![uint!(3_000_000_000_000_000_000_000_U256)]
    );
}

#[test]
fn anchor_rates_from_sources_with_inputs_composes_arb_per_eth_anchor() {
    let mut answers = BTreeMap::new();
    answers.insert(
        (1, address!("0x0000000000000000000000000000000000000200")),
        uint!(3_000_00000000_U256),
    );
    answers.insert(
        (1, address!("0x0000000000000000000000000000000000000300")),
        uint!(70_000000_U256),
    );

    assert_eq!(
        anchor_rates_from_sources_with_inputs(
            1,
            &runtime_sources(ARB_PER_ETH_ANCHOR_SOURCE),
            &answers,
            &TwapFetchedInputs::default(),
        ),
        vec![uint!(4_285_714_285_714_285_713_000_U256)]
    );
}

#[test]
fn anchor_rates_from_sources_with_inputs_discards_composite_with_missing_component() {
    let mut answers = BTreeMap::new();
    answers.insert(
        (1, address!("0x0000000000000000000000000000000000000200")),
        uint!(3_000_00000000_U256),
    );

    assert_eq!(
        anchor_rates_from_sources_with_inputs(
            1,
            &runtime_sources(ARB_PER_ETH_ANCHOR_SOURCE),
            &answers,
            &TwapFetchedInputs::default(),
        ),
        [] as [U256; 0]
    );
}

#[test]
fn mixed_chainlink_and_twap_sources_isolate_failures() {
    let twap = twap_source(1_800);
    let mut inputs = TwapFetchedInputs::default();
    inputs.metadata.insert(
        PoolKey {
            chain_id: 1,
            pool: TWAP_POOL,
        },
        PoolMetadata {
            token0: TWAP_BASE,
            token1: TWAP_QUOTE,
        },
    );
    inputs.observations.insert(
        ObservationKey {
            chain_id: 1,
            pool: TWAP_POOL,
            window_seconds: 1_800,
        },
        TwapObservation {
            tick_cumulatives: vec![0, 0],
        },
    );
    let oracle = RuntimeTokenAnchorSource::ChainlinkOracle {
        chain_id: 1,
        addr: address!("0x0000000000000000000000000000000000000500"),
        token_decimals: 18,
        oracle_decimals: 18,
        is_inversed: false,
    };
    let sources = vec![oracle, twap.clone()];
    let rates = anchor_rates_from_sources_with_inputs(1, &sources, &BTreeMap::new(), &inputs);
    assert_eq!(rates, vec![uint!(1_000_000_000_000_000_000_U256)]);
    let product = RuntimeTokenAnchorSource::Product {
        sources: vec![
            twap,
            RuntimeTokenAnchorSource::ChainlinkOracle {
                chain_id: 1,
                addr: address!("0x0000000000000000000000000000000000000501"),
                token_decimals: 18,
                oracle_decimals: 18,
                is_inversed: false,
            },
        ],
        scale_decimals: 18,
    };
    assert_eq!(
        anchor_rate_from_source_with_inputs(1, &product, &BTreeMap::new(), &inputs),
        None
    );
}

#[tokio::test]
async fn built_in_rail_twap_rate_reaches_cache_consumers() {
    let rail = address!("0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D");
    let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let multicall = address!("0x0000000000000000000000000000000000000603");
    let (rpc_url, _requests, server) =
        spawn_twap_rpc_fixture(multicall, weth, rail, 1_800, 2, false);
    let settings = crate::settings::WalletSettings::default();
    let registry = crate::settings::build_effective_token_registry(&settings)
        .expect("built-in token registry");
    let entry = token_anchor_entries_for_chains(&[1], &registry)
        .into_iter()
        .find(|entry| entry.token == rail)
        .expect("built-in RAIL entry");
    assert!(entry.anchor_sources.iter().any(|source| matches!(
        source,
        RuntimeTokenAnchorSource::UniswapV3Twap {
            window_seconds: 1_800,
            ..
        }
    )));
    let mut effective_chains =
        crate::settings::build_effective_chain_configs(&settings).expect("effective chain configs");
    effective_chains
        .get_mut(1)
        .expect("Ethereum config")
        .rpc_route = RpcChainRoute::new(1, vec![reqwest::Url::parse(&rpc_url).expect("RPC URL")])
        .with_multicall(multicall);
    let data_dir = std::env::temp_dir();
    let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
        network_mode: Some(crate::WalletNetworkMode::Direct),
        proxy: None,
        data_dir: &data_dir,
    })
    .await
    .expect("direct HTTP context");
    let cache = TokenAnchorRateCache::new();
    let (pool_keys, observation_keys) = twap_keys_for_entries(std::slice::from_ref(&entry));
    refresh_token_anchor_rates_with_fetch(
        &cache,
        std::slice::from_ref(&entry),
        BTreeMap::new(),
        pool_keys,
        observation_keys,
        TOKEN_ANCHOR_CHAIN_REFRESH_TIMEOUT,
        |plan| {
            Box::pin(async {
                let AnchorSourcePlan::Twap {
                    chain_id,
                    pools,
                    observations,
                } = plan
                else {
                    unreachable!("TWAP-only test plan")
                };
                AnchorSourceResult::Twap {
                    chain_id,
                    result: fetch_twap_inputs_for_chain(
                        chain_id,
                        &pools,
                        &observations,
                        &effective_chains,
                        None,
                        &http,
                    )
                    .await,
                }
            })
        },
    )
    .await;
    server.join().expect("fixture server");
    cache.store_native_usd_rate(1, uint!(3_000_000_000_U256), 18);
    assert_eq!(
        cache.cached_rate(1, rail),
        Some(uint!(1_000_000_000_000_000_000_U256))
    );
    assert!(
        cache
            .cached_token_usd_micro_value(1, rail, U256::from(1))
            .is_some()
    );
    assert!(!matches!(
        BroadcasterFeePolicy::default().classify_fee(U256::from(1), cache.cached_rate(1, rail)),
        BroadcasterFeePolicyStatus::UnknownAnchor
    ));
}

#[tokio::test]
async fn tron_prices_are_read_on_its_route_only_once_active() {
    use crate::rpc_broker::tests::spawn_recording_rpc_mock;
    use crate::settings::{EffectiveChainConfig, EffectiveTokenInfo, TronEndpointSetting};
    use alloy::primitives::I256;
    use alloy::providers::bindings::IMulticall3;

    // $0.25 per TRX, with 8 decimals, for every feed call.
    let feed = |call_data: &Bytes| -> Bytes {
        if call_data.starts_with(&AggregatorInterface::decimalsCall::SELECTOR) {
            AggregatorInterface::decimalsCall::abi_encode_returns(&8).into()
        } else {
            AggregatorInterface::latestAnswerCall::abi_encode_returns(
                &I256::try_from(25_000_000).unwrap(),
            )
            .into()
        }
    };
    let (url, recorded, server) =
        spawn_recording_rpc_mock(SharedArc::new(move |request: Value| {
            let result = if request["method"] == "eth_chainId" {
                json!("0x2b6653dc")
            } else {
                let call = &request["params"][0];
                let input = call
                    .get("input")
                    .or_else(|| call.get("data"))
                    .and_then(Value::as_str)
                    .unwrap()
                    .parse::<Bytes>()
                    .unwrap();
                match IMulticall3::tryAggregateCall::abi_decode(&input) {
                    Ok(aggregate) => {
                        let returns = aggregate
                            .calls
                            .iter()
                            .map(|call| IMulticall3::Result {
                                success: true,
                                returnData: feed(&call.callData),
                            })
                            .collect::<Vec<_>>();
                        json!(Bytes::from(
                            IMulticall3::tryAggregateCall::abi_encode_returns(&returns)
                        ))
                    }
                    Err(_) => json!(feed(&input)),
                }
            };
            json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }))
        .await;
    let settings = TronSettings {
        endpoints: Some(vec![TronEndpointSetting {
            json_rpc_url: url.to_string(),
            ..TronEndpointSetting::default()
        }]),
        ..TronSettings::default()
    };
    let route = crate::tron::tron_rpc_route(&settings).unwrap();
    // Tron USDT, anchored on the TRX/USD feed.
    let usdt = address!("0xa614f803b6fd780986a42c78ec9c7f77e6ded13c");
    let registry = EffectiveTokenRegistry {
        tokens: [(
            (TRON_CHAIN_ID, usdt.to_string()),
            EffectiveTokenInfo {
                chain_id: TRON_CHAIN_ID,
                token_address: usdt.to_string(),
                symbol: "USDT".to_owned(),
                decimals: 6,
                icon_path: None,
                price_anchor: None,
                built_in: true,
            },
        )]
        .into(),
    };
    let cache = Arc::new(TokenAnchorRateCache::new());
    let mut updates = cache.subscribe_refreshes();
    let worker = spawn_token_anchor_refresh_worker(
        &Handle::current(),
        cache.clone(),
        vec![],
        std::iter::empty::<EffectiveChainConfig>().collect(),
        registry,
        HttpContext::direct_for_tests(),
    );
    worker.reconcile_tron(&settings, Some(&route));

    // Enabled but not selected in this session: nothing is sent, identity checks included.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(recorded.lock().unwrap().is_empty());

    worker.set_active_chains(&[TRON_CHAIN_ID]);
    let (four_trx, one_usdt) = (U256::from(4_000_000), U256::from(1_000_000));
    timeout(Duration::from_secs(10), async {
        while cache
            .cached_native_usd_micro_value(TRON_CHAIN_ID, four_trx)
            .is_none()
            || cache
                .cached_token_usd_micro_value(TRON_CHAIN_ID, usdt, one_usdt)
                .is_none()
        {
            updates.changed().await.unwrap();
        }
    })
    .await
    .expect("Tron prices once Tron is active");
    assert_eq!(
        cache.cached_native_usd_micro_value(TRON_CHAIN_ID, four_trx),
        Some(U256::from(1_000_000))
    );
    assert_eq!(
        cache.cached_token_usd_micro_value(TRON_CHAIN_ID, usdt, one_usdt),
        Some(U256::from(1_000_000))
    );
    // Tron answers calls only at `latest`.
    assert!(
        recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, request)| request["method"] == "eth_call")
            .all(|(_, request)| request["params"][1] == "latest")
    );
    drop(worker);
    server.abort();
}

#[test]
fn policy_classifies_cache_miss_as_allowed_unknown_anchor() {
    let policy = BroadcasterFeePolicy::default();
    let status = policy.classify_fee(uint!(1_501_U256), None);

    assert_eq!(status, BroadcasterFeePolicyStatus::UnknownAnchor);
    assert!(policy.allows_status(status));
}

#[test]
fn policy_classifies_fee_bounds_and_unknown_anchor() {
    let policy = BroadcasterFeePolicy::default();
    let anchor = uint!(1_000_U256);

    assert!(
        policy
            .classify_fee(uint!(899_U256), Some(anchor))
            .is_suspicious()
    );
    assert!(
        !policy
            .classify_fee(uint!(900_U256), Some(anchor))
            .is_suspicious()
    );
    assert!(
        !policy
            .classify_fee(uint!(1_500_U256), Some(anchor))
            .is_suspicious()
    );
    assert!(
        policy
            .classify_fee(uint!(1_501_U256), Some(anchor))
            .is_suspicious()
    );
    assert_eq!(
        policy.classify_fee(uint!(1_501_U256), None),
        BroadcasterFeePolicyStatus::UnknownAnchor
    );
}
