//! Fresh price-anchor reads before signing swap hooks.
//!
//! Quote previews use [`TokenAnchorRateCache`](super::TokenAnchorRateCache), which keeps old rates
//! after a failed refresh and stores no observation time. Signing instead fetches each chain's
//! head once and reads every configured source at that block. A source that can't be read fresh
//! is dropped; there is no fallback to earlier values.
//!
//! Rates use the cache's unit, token base units per one whole native token, so token decimals are
//! already part of each rate. Per token, top-level sources are alternatives, combined with
//! [`average_non_outlier_anchor_rates`] like the cache does, and a token is fresh when at least one
//! of them is. `Product` sources compose through the cache's own composition, which needs every leg,
//! so a product is fresh only when all of its legs are. `Fixed` sources are always fresh and record
//! no observation.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::{AnyRpcBlock, primitives::HeaderResponse};
use alloy::primitives::{Address, B256, U256, U512};
use alloy::sol_types::SolCall;
use futures_util::future::join_all;

use super::{
    AggregatorInterface, BPS_DENOMINATOR, ObservationKey, PoolKey, RuntimeTokenAnchorSource,
    TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT, TwapFetchedInputs, anchor_rate_from_source_with_inputs,
    average_non_outlier_anchor_rates, collect_oracle_addresses_from_source,
    collect_twap_keys_from_source, fetch_twap_inputs_at, token_anchor_entries_for_chains,
};
use crate::settings::{
    EffectiveChainRegistry, EffectiveTokenRegistry, resolve_effective_chain_rpc_route,
};
use crate::{HttpContext, RpcRead, RpcRoute, RpcSubmission, WalletRpcOrigin};

/// Freshness limits for [`read_fresh_pair_anchor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshAnchorParams {
    /// Largest allowed age of a Chainlink round's `updatedAt`, measured against local time.
    pub chainlink_max_age: Duration,
    /// Replacements for `chainlink_max_age`, keyed by aggregator chain ID and address.
    pub chainlink_max_age_overrides: BTreeMap<(u64, Address), Duration>,
    /// Largest allowed age of the head block used for Uniswap V3 TWAP reads, measured against
    /// local time.
    pub max_head_age: Duration,
}

/// The head block that a chain's anchor sources were read at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AnchorBlock {
    pub chain_id: u64,
    pub number: u64,
    pub hash: B256,
    /// Block timestamp in Unix seconds.
    pub timestamp: u64,
}

/// One source reading that a fresh rate was computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnchorObservation {
    Chainlink {
        aggregator: Address,
        block: AnchorBlock,
        /// The round's `updatedAt`, in Unix seconds.
        updated_at: u64,
    },
    UniswapV3Twap {
        pool: Address,
        window_seconds: u32,
        block: AnchorBlock,
    },
}

/// Why a configured source didn't produce a fresh reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AnchorReadFailure {
    /// The chain's head block couldn't be read.
    HeadUnavailable { chain_id: u64 },
    /// The head block is older than the maximum head age, so TWAP sources weren't read.
    HeadTooOld { block: AnchorBlock },
    /// The source's call failed, reverted, or returned an unusable value.
    Unreadable { chain_id: u64, source: Address },
    /// The Chainlink round is older than its maximum age.
    ChainlinkStale {
        aggregator: Address,
        block: AnchorBlock,
        updated_at: u64,
    },
    /// The configured sources didn't yield a rate.
    NoRate,
}

/// A token whose configured anchors have no usable rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorBlocked {
    pub token: Address,
    pub failures: Vec<AnchorReadFailure>,
}

/// Anchor rates for a pair, each in token base units per one whole native token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairAnchorRate {
    pub sell_rate: U256,
    pub buy_rate: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshPairAnchor {
    /// Both tokens have fresh rates. `observations` lists the readings they came from.
    Fresh {
        rate: PairAnchorRate,
        observations: Vec<AnchorObservation>,
    },
    /// The sell or buy token has no configured anchor. Nothing was read; the user must
    /// acknowledge an unverified price.
    Unverified,
    /// Anchors are configured, but one token has no fresh source. The caller must treat the
    /// price as unverified and obtain acknowledgement before proceeding.
    Blocked(AnchorBlocked),
}

/// Reads the pair's configured anchors at each involved chain's current head.
pub async fn read_fresh_pair_anchor(
    chain_id: u64,
    sell_token: Address,
    buy_token: Address,
    params: &FreshAnchorParams,
    effective_chains: &EffectiveChainRegistry,
    token_registry: &EffectiveTokenRegistry,
    http: &HttpContext,
) -> FreshPairAnchor {
    read_fresh_pair_anchor_at(
        chain_id,
        sell_token,
        buy_token,
        params,
        effective_chains,
        token_registry,
        http,
        SystemTime::now(),
    )
    .await
}

async fn read_fresh_pair_anchor_at(
    chain_id: u64,
    sell_token: Address,
    buy_token: Address,
    params: &FreshAnchorParams,
    effective_chains: &EffectiveChainRegistry,
    token_registry: &EffectiveTokenRegistry,
    http: &HttpContext,
    now: SystemTime,
) -> FreshPairAnchor {
    let entries = token_anchor_entries_for_chains(&[chain_id], token_registry);
    let sources_for = |token: Address| {
        entries
            .iter()
            .find(|entry| entry.token == token)
            .map(|entry| entry.anchor_sources.as_slice())
            .filter(|sources| !sources.is_empty())
    };
    let (Some(sell_sources), Some(buy_sources)) = (sources_for(sell_token), sources_for(buy_token))
    else {
        return FreshPairAnchor::Unverified;
    };
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let inputs = read_fresh_inputs(
        chain_id,
        sell_sources.iter().chain(buy_sources),
        params,
        effective_chains,
        http,
        now,
    )
    .await;
    let sell = match fresh_token_rate(chain_id, sell_token, sell_sources, &inputs) {
        Ok(sell) => sell,
        Err(blocked) => return FreshPairAnchor::Blocked(blocked),
    };
    let buy = match fresh_token_rate(chain_id, buy_token, buy_sources, &inputs) {
        Ok(buy) => buy,
        Err(blocked) => return FreshPairAnchor::Blocked(blocked),
    };
    let observations = sell
        .1
        .into_iter()
        .chain(buy.1)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    FreshPairAnchor::Fresh {
        rate: PairAnchorRate {
            sell_rate: sell.0,
            buy_rate: buy.0,
        },
        observations,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum InputKey {
    Oracle { chain_id: u64, aggregator: Address },
    Twap(ObservationKey),
}

#[derive(Debug, Default)]
struct FreshInputs {
    oracle_answers: BTreeMap<(u64, Address), U256>,
    twap: TwapFetchedInputs,
    observations: BTreeMap<InputKey, AnchorObservation>,
    failures: BTreeMap<InputKey, AnchorReadFailure>,
}

impl FreshInputs {
    fn extend(&mut self, other: Self) {
        self.oracle_answers.extend(other.oracle_answers);
        self.twap.metadata.extend(other.twap.metadata);
        self.twap.observations.extend(other.twap.observations);
        self.observations.extend(other.observations);
        self.failures.extend(other.failures);
    }

    fn fail_all(&mut self, keys: impl IntoIterator<Item = InputKey>, failure: AnchorReadFailure) {
        self.failures
            .extend(keys.into_iter().map(|key| (key, failure)));
    }
}

fn input_keys(owner_chain_id: u64, source: &RuntimeTokenAnchorSource) -> Vec<InputKey> {
    let mut oracles = BTreeMap::<u64, BTreeSet<Address>>::new();
    let mut pools = BTreeSet::new();
    let mut observations = BTreeSet::new();
    collect_oracle_addresses_from_source(source, &mut oracles);
    collect_twap_keys_from_source(owner_chain_id, source, &mut pools, &mut observations);
    oracles
        .into_iter()
        .flat_map(|(chain_id, aggregators)| {
            aggregators
                .into_iter()
                .map(move |aggregator| InputKey::Oracle {
                    chain_id,
                    aggregator,
                })
        })
        .chain(observations.into_iter().map(InputKey::Twap))
        .collect()
}

fn fresh_token_rate(
    owner_chain_id: u64,
    token: Address,
    sources: &[RuntimeTokenAnchorSource],
    inputs: &FreshInputs,
) -> Result<(U256, Vec<AnchorObservation>), AnchorBlocked> {
    let mut rates = Vec::new();
    let mut observations = Vec::new();
    for source in sources {
        let Some(rate) = anchor_rate_from_source_with_inputs(
            owner_chain_id,
            source,
            &inputs.oracle_answers,
            &inputs.twap,
        ) else {
            continue;
        };
        rates.push(rate);
        observations.extend(
            input_keys(owner_chain_id, source)
                .iter()
                .filter_map(|key| inputs.observations.get(key).copied()),
        );
    }
    if let Some(rate) = average_non_outlier_anchor_rates(&rates) {
        return Ok((rate, observations));
    }
    let mut failures = sources
        .iter()
        .flat_map(|source| input_keys(owner_chain_id, source))
        .filter_map(|key| inputs.failures.get(&key).copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if failures.is_empty() {
        failures.push(AnchorReadFailure::NoRate);
    }
    Err(AnchorBlocked { token, failures })
}

async fn read_fresh_inputs<'a>(
    owner_chain_id: u64,
    sources: impl Iterator<Item = &'a RuntimeTokenAnchorSource>,
    params: &FreshAnchorParams,
    effective_chains: &EffectiveChainRegistry,
    http: &HttpContext,
    now: Duration,
) -> FreshInputs {
    let mut oracles = BTreeMap::<u64, BTreeSet<Address>>::new();
    let mut pools = BTreeSet::new();
    let mut observations = BTreeSet::new();
    for source in sources {
        collect_oracle_addresses_from_source(source, &mut oracles);
        collect_twap_keys_from_source(owner_chain_id, source, &mut pools, &mut observations);
    }
    let chain_ids = oracles
        .keys()
        .copied()
        .chain(observations.iter().map(|key| key.chain_id))
        .collect::<BTreeSet<_>>();
    let reads = chain_ids.into_iter().map(|chain_id| {
        let aggregators = oracles.get(&chain_id).map_or_else(Vec::new, |aggregators| {
            aggregators.iter().copied().collect()
        });
        let observations = observations
            .iter()
            .copied()
            .filter(|key| key.chain_id == chain_id)
            .collect();
        read_chain_inputs(
            chain_id,
            aggregators,
            observations,
            params,
            effective_chains,
            http,
            now,
        )
    });
    let mut inputs = FreshInputs::default();
    for chain_inputs in join_all(reads).await {
        inputs.extend(chain_inputs);
    }
    inputs
}

async fn read_chain_inputs(
    chain_id: u64,
    aggregators: Vec<Address>,
    observations: Vec<ObservationKey>,
    params: &FreshAnchorParams,
    effective_chains: &EffectiveChainRegistry,
    http: &HttpContext,
    now: Duration,
) -> FreshInputs {
    let mut inputs = FreshInputs::default();
    let oracle_keys = aggregators.iter().map(|&aggregator| InputKey::Oracle {
        chain_id,
        aggregator,
    });
    let twap_keys = observations.iter().copied().map(InputKey::Twap);
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "anchor_head", chain_id, "started");
    let head = read_head(chain_id, effective_chains, http).await;
    tracing::debug!(
        target: "swap_quote",
        step = "anchor_head",
        chain_id,
        elapsed_ms = started.elapsed().as_millis(),
        success = head.is_some(),
        "finished"
    );
    let Some((route, block)) = head else {
        inputs.fail_all(
            oracle_keys.chain(twap_keys),
            AnchorReadFailure::HeadUnavailable { chain_id },
        );
        return inputs;
    };
    if !observations.is_empty() {
        let started = Instant::now();
        tracing::debug!(target: "swap_quote", step = "anchor_twap", chain_id, "started");
        read_twap_inputs(&route, block, observations, params, http, now, &mut inputs).await;
        tracing::debug!(
            target: "swap_quote",
            step = "anchor_twap",
            chain_id,
            elapsed_ms = started.elapsed().as_millis(),
            "finished"
        );
    }
    if aggregators.is_empty() {
        return inputs;
    }
    let calls = aggregators
        .iter()
        .map(|&aggregator| {
            (
                aggregator,
                AggregatorInterface::latestRoundDataCall {}
                    .abi_encode()
                    .into(),
            )
        })
        .collect();
    let started = Instant::now();
    tracing::debug!(target: "swap_quote", step = "anchor_chainlink", chain_id, "started");
    let rounds = http
        .rpc_broker()
        .submit_calls_decoded_at::<AggregatorInterface::latestRoundDataCall>(
            route,
            calls,
            BlockId::hash_canonical(block.hash),
            WalletRpcOrigin::Swaps.into(),
        )
        .await
        .ok();
    tracing::debug!(
        target: "swap_quote",
        step = "anchor_chainlink",
        chain_id,
        elapsed_ms = started.elapsed().as_millis(),
        success = rounds.as_ref().is_some_and(|rounds| rounds.iter().all(Result::is_ok)),
        "finished"
    );
    for (index, aggregator) in aggregators.into_iter().enumerate() {
        let key = InputKey::Oracle {
            chain_id,
            aggregator,
        };
        let round = rounds
            .as_ref()
            .and_then(|rounds| rounds.get(index))
            .and_then(|round| round.as_ref().ok());
        let reading = round.and_then(|round| {
            let answer = U256::try_from(round.answer)
                .ok()
                .filter(|answer| !answer.is_zero())?;
            Some((answer, u64::try_from(round.updatedAt).ok()?))
        });
        let Some((answer, updated_at)) = reading else {
            inputs.failures.insert(
                key,
                AnchorReadFailure::Unreadable {
                    chain_id,
                    source: aggregator,
                },
            );
            continue;
        };
        let max_age = params
            .chainlink_max_age_overrides
            .get(&(chain_id, aggregator))
            .copied()
            .unwrap_or(params.chainlink_max_age);
        if now.saturating_sub(Duration::from_secs(updated_at)) > max_age {
            inputs.failures.insert(
                key,
                AnchorReadFailure::ChainlinkStale {
                    aggregator,
                    block,
                    updated_at,
                },
            );
            continue;
        }
        inputs.oracle_answers.insert((chain_id, aggregator), answer);
        inputs.observations.insert(
            key,
            AnchorObservation::Chainlink {
                aggregator,
                block,
                updated_at,
            },
        );
    }
    inputs
}

/// Reads TWAP inputs at `block`, or none when the block is older than the maximum head age.
async fn read_twap_inputs(
    route: &RpcRoute,
    block: AnchorBlock,
    observations: Vec<ObservationKey>,
    params: &FreshAnchorParams,
    http: &HttpContext,
    now: Duration,
    inputs: &mut FreshInputs,
) {
    if now.saturating_sub(Duration::from_secs(block.timestamp)) > params.max_head_age {
        inputs.fail_all(
            observations.into_iter().map(InputKey::Twap),
            AnchorReadFailure::HeadTooOld { block },
        );
        return;
    }
    let chain_id = block.chain_id;
    let pools = observations
        .iter()
        .map(|key| PoolKey {
            chain_id,
            pool: key.pool,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    // Pools and observations that fail to read are absent from the result.
    inputs.twap = fetch_twap_inputs_at(
        http,
        route,
        BlockId::hash_canonical(block.hash),
        WalletRpcOrigin::Swaps,
        &pools,
        &observations,
    )
    .await
    .unwrap_or_default();
    for key in observations {
        let pool = PoolKey {
            chain_id,
            pool: key.pool,
        };
        if inputs.twap.metadata.contains_key(&pool) && inputs.twap.observations.contains_key(&key) {
            inputs.observations.insert(
                InputKey::Twap(key),
                AnchorObservation::UniswapV3Twap {
                    pool: key.pool,
                    window_seconds: key.window_seconds,
                    block,
                },
            );
        } else {
            inputs.failures.insert(
                InputKey::Twap(key),
                AnchorReadFailure::Unreadable {
                    chain_id,
                    source: key.pool,
                },
            );
        }
    }
}

async fn read_head(
    chain_id: u64,
    effective_chains: &EffectiveChainRegistry,
    http: &HttpContext,
) -> Option<(RpcRoute, AnchorBlock)> {
    let chain_route = resolve_effective_chain_rpc_route(chain_id, effective_chains.get(chain_id)?)
        .ok()?
        .for_class(crate::RpcTrafficClass::General)
        .ok()?;
    let route = RpcRoute::from(chain_route)
        .with_request_timeout(TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT)
        .with_attempt_timeout(Duration::from_secs(5));
    let results = http
        .rpc_broker()
        .submit(RpcSubmission::new(
            route.clone(),
            vec![RpcRead::get_block_by_number(
                BlockNumberOrTag::Latest,
                false,
            )],
            WalletRpcOrigin::Swaps.into(),
        ))
        .await
        .ok()?;
    let head: AnyRpcBlock = results
        .into_iter()
        .next()?
        .ok()
        .and_then(|result| serde_json::from_value(result.into_value()).ok())?;
    let head_id = head.header.num_hash();
    Some((
        route,
        AnchorBlock {
            chain_id,
            number: head_id.number,
            hash: head_id.hash,
            timestamp: head.header.inner.timestamp,
        },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QuoteDeviationError {
    #[error("quote price is worse than the anchor price by more than the allowed deviation")]
    ExceedsThreshold,
    #[error("quote or anchor amounts are zero or too large to compare")]
    InvalidInput,
}

/// Checks a quote against an anchor rate.
///
/// `sell_amount` excludes the quote's explicit fee. `buy_amount` is the expected output for that
/// amount, before the wallet's own hook-gas and slippage adjustments. The anchor expects
/// `sell_amount * buy_rate / sell_rate`. The quote is rejected when it pays less than that by more
/// than `max_deviation_bps`; a better price passes.
/// Decimals are already part of the rates.
///
/// The comparison cross-multiplies in 512 bits, so nothing is rounded:
/// `buy_amount * sell_rate * 10_000 >= sell_amount * buy_rate * (10_000 - max_deviation_bps)`.
/// Zero amounts or rates, and products beyond 512 bits, are rejected.
pub fn check_quote_against_anchor(
    sell_amount: U256,
    buy_amount: U256,
    rate: PairAnchorRate,
    max_deviation_bps: u32,
) -> Result<(), QuoteDeviationError> {
    if [sell_amount, buy_amount, rate.sell_rate, rate.buy_rate]
        .iter()
        .any(U256::is_zero)
    {
        return Err(QuoteDeviationError::InvalidInput);
    }
    let floor_bps = BPS_DENOMINATOR.saturating_sub(U256::from(max_deviation_bps));
    let quoted = U512::from(buy_amount)
        .checked_mul(U512::from(rate.sell_rate))
        .and_then(|product| product.checked_mul(U512::from(BPS_DENOMINATOR)));
    let required = U512::from(sell_amount)
        .checked_mul(U512::from(rate.buy_rate))
        .and_then(|product| product.checked_mul(U512::from(floor_bps)));
    let (Some(quoted), Some(required)) = (quoted, required) else {
        return Err(QuoteDeviationError::InvalidInput);
    };
    if quoted >= required {
        Ok(())
    } else {
        Err(QuoteDeviationError::ExceedsThreshold)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use alloy::primitives::aliases::U80;
    use alloy::primitives::{Bytes, I256, address, uint};
    use serde_json::{Value, json};

    use super::*;
    use crate::rpc_broker::tests::spawn_rpc_mock;
    use crate::settings::{
        EffectiveTokenInfo, PriceAnchorSettings, WalletSettings, build_effective_chain_configs,
    };

    const NOW: u64 = 1_000_000;
    const HEAD_HASH: B256 = B256::repeat_byte(0xaa);
    const ORACLE: Address = address!("0x0000000000000000000000000000000000000100");
    const USDC: Address = address!("0x0000000000000000000000000000000000000601");
    const WETH: Address = address!("0x0000000000000000000000000000000000001801");
    const TWAP_TOKEN: Address = address!("0x0000000000000000000000000000000000000701");
    const UNANCHORED: Address = address!("0x0000000000000000000000000000000000000801");
    // USDC/ETH feed: ETH per USDC with 18 decimals, about 3,000 USDC per ETH.
    const USDC_ETH_ANSWER: u64 = 333_333_333_333_333;

    fn token(
        address: Address,
        decimals: u8,
        anchor: Option<PriceAnchorSettings>,
    ) -> EffectiveTokenInfo {
        EffectiveTokenInfo {
            chain_id: 1,
            token_address: address.to_string(),
            symbol: String::new(),
            decimals,
            icon_path: None,
            price_anchor: anchor,
            built_in: false,
        }
    }

    fn registry() -> EffectiveTokenRegistry {
        let tokens = [
            token(
                USDC,
                6,
                Some(PriceAnchorSettings::Oracle {
                    chain_id: 1,
                    oracle_address: ORACLE.to_string(),
                    token_decimals: 6,
                    oracle_decimals: 18,
                    is_inversed: true,
                }),
            ),
            token(
                WETH,
                18,
                Some(PriceAnchorSettings::Fixed {
                    rate: "1000000000000000000".to_string(),
                }),
            ),
            token(
                TWAP_TOKEN,
                18,
                Some(PriceAnchorSettings::UniswapV3Twap {
                    pool_address: Address::repeat_byte(4).to_string(),
                    base_token_address: TWAP_TOKEN.to_string(),
                    quote_token_address: WETH.to_string(),
                    base_token_decimals: 18,
                    window_seconds: 1_800,
                }),
            ),
            token(UNANCHORED, 18, None),
        ];
        EffectiveTokenRegistry {
            tokens: tokens
                .into_iter()
                .map(|token| ((1, token.token_address.clone()), token))
                .collect(),
        }
    }

    const fn params() -> FreshAnchorParams {
        FreshAnchorParams {
            chainlink_max_age: Duration::from_hours(1),
            chainlink_max_age_overrides: BTreeMap::new(),
            max_head_age: Duration::from_mins(1),
        }
    }

    const fn head_block(timestamp: u64) -> AnchorBlock {
        AnchorBlock {
            chain_id: 1,
            number: 10,
            hash: HEAD_HASH,
            timestamp,
        }
    }

    /// Serves one head block and answers `latestRoundData` at that block, or fails every call.
    async fn chain_with_mock(
        head_timestamp: u64,
        updated_at: u64,
        fail_calls: bool,
    ) -> (
        EffectiveChainRegistry,
        Arc<Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let (endpoint, server) = spawn_rpc_mock(
            Arc::new(move |request| {
                recorded.lock().unwrap().push(request.clone());
                let result = match request["method"].as_str().unwrap() {
                    "eth_getBlockByNumber" => {
                        let mut block: alloy::rpc::types::Block =
                            alloy::rpc::types::Block::default();
                        block.header.inner.number = 10;
                        block.header.inner.timestamp = head_timestamp;
                        block.header.hash = HEAD_HASH;
                        serde_json::to_value(block).unwrap()
                    }
                    "eth_call" => {
                        assert_eq!(
                            request["params"][1],
                            json!({"blockHash": HEAD_HASH, "requireCanonical": true})
                        );
                        if fail_calls {
                            return json!({
                                "jsonrpc": "2.0",
                                "id": request["id"],
                                "error": {"code": -32000, "message": "mock RPC failure"}
                            });
                        }
                        json!(Bytes::from(
                            AggregatorInterface::latestRoundDataCall::abi_encode_returns(
                                &AggregatorInterface::latestRoundDataReturn {
                                    roundId: U80::from(1),
                                    answer: I256::try_from(USDC_ETH_ANSWER).unwrap(),
                                    startedAt: U256::from(updated_at),
                                    updatedAt: U256::from(updated_at),
                                    answeredInRound: U80::from(1),
                                },
                            )
                        ))
                    }
                    method => panic!("unexpected anchor RPC {method}"),
                };
                json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
            }),
            Arc::default(),
            Arc::default(),
        )
        .await;
        let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        chains.get_mut(1).unwrap().rpc_route = crate::RpcChainRoute::new(1, vec![endpoint]);
        (chains, requests, server)
    }

    async fn read(
        chains: &EffectiveChainRegistry,
        params: &FreshAnchorParams,
        sell: Address,
        buy: Address,
    ) -> FreshPairAnchor {
        read_fresh_pair_anchor_at(
            1,
            sell,
            buy,
            params,
            chains,
            &registry(),
            &HttpContext::direct_for_tests(),
            UNIX_EPOCH + Duration::from_secs(NOW),
        )
        .await
    }

    #[tokio::test]
    async fn fresh_chainlink_round_records_its_block_and_update_time() {
        let (chains, _, server) = chain_with_mock(NOW - 5, NOW - 60, false).await;
        let outcome = read(&chains, &params(), USDC, WETH).await;
        assert_eq!(
            outcome,
            FreshPairAnchor::Fresh {
                rate: PairAnchorRate {
                    sell_rate: super::super::oracle_answer_to_anchor_rate(
                        U256::from(USDC_ETH_ANSWER),
                        6,
                        18,
                        true
                    )
                    .unwrap(),
                    buy_rate: uint!(1_000_000_000_000_000_000_U256),
                },
                observations: vec![AnchorObservation::Chainlink {
                    aggregator: ORACLE,
                    block: head_block(NOW - 5),
                    updated_at: NOW - 60,
                }],
            }
        );
        server.abort();
    }

    #[tokio::test]
    async fn stale_round_blocks_unless_the_aggregator_allows_a_longer_age() {
        let (chains, _, server) = chain_with_mock(NOW - 5, NOW - 3_601, false).await;
        assert_eq!(
            read(&chains, &params(), USDC, WETH).await,
            FreshPairAnchor::Blocked(AnchorBlocked {
                token: USDC,
                failures: vec![AnchorReadFailure::ChainlinkStale {
                    aggregator: ORACLE,
                    block: head_block(NOW - 5),
                    updated_at: NOW - 3_601,
                }],
            })
        );
        let mut relaxed = params();
        relaxed
            .chainlink_max_age_overrides
            .insert((1, ORACLE), Duration::from_hours(2));
        assert!(matches!(
            read(&chains, &relaxed, USDC, WETH).await,
            FreshPairAnchor::Fresh { .. }
        ));
        server.abort();
    }

    // The fresh read has no access to `TokenAnchorRateCache`, so a failed read can't fall back to
    // a cached rate.
    #[tokio::test]
    async fn unreadable_source_blocks() {
        let (chains, _, server) = chain_with_mock(NOW - 5, NOW - 60, true).await;
        assert_eq!(
            read(&chains, &params(), WETH, USDC).await,
            FreshPairAnchor::Blocked(AnchorBlocked {
                token: USDC,
                failures: vec![AnchorReadFailure::Unreadable {
                    chain_id: 1,
                    source: ORACLE,
                }],
            })
        );
        server.abort();
    }

    #[tokio::test]
    async fn old_head_blocks_twap_without_reading_the_pool() {
        let (chains, requests, server) = chain_with_mock(NOW - 61, NOW - 60, false).await;
        assert_eq!(
            read(&chains, &params(), TWAP_TOKEN, WETH).await,
            FreshPairAnchor::Blocked(AnchorBlocked {
                token: TWAP_TOKEN,
                failures: vec![AnchorReadFailure::HeadTooOld {
                    block: head_block(NOW - 61),
                }],
            })
        );
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| request["method"] == "eth_getBlockByNumber")
        );
        server.abort();
    }

    #[tokio::test]
    async fn pair_without_anchors_is_unverified_without_network_reads() {
        let (chains, requests, server) = chain_with_mock(NOW, NOW, false).await;
        assert_eq!(
            read(&chains, &params(), UNANCHORED, WETH).await,
            FreshPairAnchor::Unverified
        );
        assert!(requests.lock().unwrap().is_empty());
        server.abort();
    }

    #[test]
    fn quote_deviation_rejects_only_worse_prices_beyond_threshold() {
        let usdc_per_eth = U256::from(3_000_000_000_u64);
        let weth_per_eth = uint!(1_000_000_000_000_000_000_U256);
        // Selling 1 WETH (18 decimals) for USDC (6 decimals): the anchor expects 3,000 USDC.
        let weth_to_usdc = PairAnchorRate {
            sell_rate: weth_per_eth,
            buy_rate: usdc_per_eth,
        };
        let one_weth = weth_per_eth;
        for (buy, expected) in [
            (2_970_000_000_u64, Ok(())),
            (2_969_999_999, Err(QuoteDeviationError::ExceedsThreshold)),
            (3_100_000_000, Ok(())),
        ] {
            assert_eq!(
                check_quote_against_anchor(one_weth, U256::from(buy), weth_to_usdc, 100),
                expected
            );
        }
        // The reverse direction: 3,000 USDC should buy 1 WETH.
        let usdc_to_weth = PairAnchorRate {
            sell_rate: usdc_per_eth,
            buy_rate: weth_per_eth,
        };
        let floor = uint!(990_000_000_000_000_000_U256);
        assert_eq!(
            check_quote_against_anchor(usdc_per_eth, floor, usdc_to_weth, 100),
            Ok(())
        );
        assert_eq!(
            check_quote_against_anchor(usdc_per_eth, floor - U256::ONE, usdc_to_weth, 100),
            Err(QuoteDeviationError::ExceedsThreshold)
        );
    }
}
