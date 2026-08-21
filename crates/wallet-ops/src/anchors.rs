use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::providers::{CallItem, Provider};
use alloy::sol;
use alloy::sol_types::SolCall;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, WrapErr};
use futures_util::stream::{FuturesUnordered, StreamExt};
use railgun_ui::{
    NativeUsdAnchorInfo, TokenAnchorInfo, TokenAnchorSource, lookup_token,
    native_usd_anchor_entries, native_usd_micro_value, token_anchor_entries, token_usd_micro_value,
};
use sync_service::ChainConfigDefaults;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::{Instant, sleep_until, timeout};

use crate::settings::{EffectiveChainConfig, EffectiveTokenRegistry, PriceAnchorSettings};
use crate::{HttpContext, effective_rpc_urls_for_chain, query_rpc_pool_with_http_client};

mod uniswap_v3_twap;

const ANCHOR_OUTLIER_THRESHOLD_BPS: U256 = alloy::uint!(5_000_U256);
const BPS_DENOMINATOR: U256 = alloy::uint!(10_000_U256);
const TOKEN_ANCHOR_REFRESH_INTERVAL: Duration = Duration::from_mins(5);
const TOKEN_ANCHOR_MISSING_RATE_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const TOKEN_ANCHOR_WAKE_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(30);
const TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const TOKEN_ANCHOR_CHAIN_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

sol! {
    interface AggregatorInterface {
        function latestAnswer() external view returns (int256);
    }
    interface UniswapV3PoolInterface {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function observe(uint32[] secondsAgos) external view returns (int56[] tickCumulatives, uint160[] secondsPerLiquidityCumulativeX128s);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcasterFeePolicyStatus {
    Normal {
        anchor_rate: U256,
        premium_bps: i128,
    },
    Suspicious {
        anchor_rate: U256,
        premium_bps: Option<i128>,
    },
    UnknownAnchor,
}

impl BroadcasterFeePolicyStatus {
    #[must_use]
    pub const fn is_suspicious(self) -> bool {
        matches!(self, Self::Suspicious { .. })
    }

    #[must_use]
    pub const fn premium_bps(self) -> Option<i128> {
        match self {
            Self::Normal { premium_bps, .. } => Some(premium_bps),
            Self::Suspicious { premium_bps, .. } => premium_bps,
            Self::UnknownAnchor => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcasterFeePolicy {
    pub min_anchor_bps: u64,
    pub max_anchor_bps: u64,
    pub allow_suspicious_broadcasters: bool,
}

impl Default for BroadcasterFeePolicy {
    fn default() -> Self {
        Self {
            min_anchor_bps: 9_000,
            max_anchor_bps: 15_000,
            allow_suspicious_broadcasters: false,
        }
    }
}

impl BroadcasterFeePolicy {
    #[must_use]
    pub const fn with_allow_suspicious_broadcasters(
        mut self,
        allow_suspicious_broadcasters: bool,
    ) -> Self {
        self.allow_suspicious_broadcasters = allow_suspicious_broadcasters;
        self
    }

    #[must_use]
    pub const fn allows_status(self, status: BroadcasterFeePolicyStatus) -> bool {
        !status.is_suspicious() || self.allow_suspicious_broadcasters
    }

    #[must_use]
    pub fn classify_fee(self, fee: U256, anchor_rate: Option<U256>) -> BroadcasterFeePolicyStatus {
        let Some(anchor_rate) = anchor_rate.filter(|rate| !rate.is_zero()) else {
            return BroadcasterFeePolicyStatus::UnknownAnchor;
        };
        let Some(fee_bps) = fee
            .checked_mul(BPS_DENOMINATOR)
            .and_then(|scaled| scaled.checked_div(anchor_rate))
        else {
            return BroadcasterFeePolicyStatus::Suspicious {
                anchor_rate,
                premium_bps: None,
            };
        };
        let min_bps = U256::from(self.min_anchor_bps);
        let max_bps = U256::from(self.max_anchor_bps);
        let premium_bps = i128::try_from(fee_bps).ok().map(|bps| bps - 10_000);
        if fee_bps < min_bps || fee_bps > max_bps {
            return BroadcasterFeePolicyStatus::Suspicious {
                anchor_rate,
                premium_bps,
            };
        }
        BroadcasterFeePolicyStatus::Normal {
            anchor_rate,
            premium_bps: premium_bps.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TokenAnchorKey {
    chain_id: u64,
    token: Address,
}

#[derive(Debug, Clone)]
struct RuntimeTokenAnchorInfo {
    chain_id: u64,
    token: Address,
    anchor_sources: Vec<RuntimeTokenAnchorSource>,
}

#[derive(Debug, Clone)]
struct RuntimeNativeUsdAnchorInfo {
    chain_id: u64,
    anchor_sources: Vec<RuntimeTokenAnchorSource>,
}

#[derive(Debug, Clone)]
enum RuntimeTokenAnchorSource {
    Fixed {
        token_fee_per_unit_gas: U256,
    },
    ChainlinkOracle {
        chain_id: u64,
        addr: Address,
        token_decimals: u8,
        oracle_decimals: u8,
        is_inversed: bool,
    },
    UniswapV3Twap {
        pool: Address,
        base_token: Address,
        quote_token: Address,
        base_token_decimals: u8,
        window_seconds: u32,
    },
    Product {
        sources: Vec<Self>,
        scale_decimals: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PoolKey {
    chain_id: u64,
    pool: Address,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ObservationKey {
    chain_id: u64,
    pool: Address,
    window_seconds: u32,
}

#[derive(Debug, Clone, Copy)]
struct PoolMetadata {
    token0: Address,
    token1: Address,
}

#[derive(Debug, Clone)]
struct TwapObservation {
    tick_cumulatives: Vec<i128>,
}

#[derive(Debug, Default)]
struct TwapFetchedInputs {
    metadata: BTreeMap<PoolKey, PoolMetadata>,
    observations: BTreeMap<ObservationKey, TwapObservation>,
}

impl TokenAnchorKey {
    const fn new(chain_id: u64, token: Address) -> Self {
        Self { chain_id, token }
    }
}

#[derive(Debug)]
pub struct TokenAnchorRateCache {
    rates: RwLock<BTreeMap<TokenAnchorKey, U256>>,
    native_usd_rates: RwLock<BTreeMap<u64, U256>>,
    refresh_tx: watch::Sender<u64>,
}

impl Default for TokenAnchorRateCache {
    fn default() -> Self {
        let (refresh_tx, _refresh_rx) = watch::channel(0_u64);
        Self {
            rates: RwLock::new(BTreeMap::new()),
            native_usd_rates: RwLock::new(BTreeMap::new()),
            refresh_tx,
        }
    }
}

impl TokenAnchorRateCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn cached_rate(&self, chain_id: u64, token: Address) -> Option<U256> {
        self.rates
            .read()
            .ok()
            .and_then(|rates| rates.get(&TokenAnchorKey::new(chain_id, token)).copied())
    }

    pub fn store_rate(&self, chain_id: u64, token: Address, rate: U256) {
        if rate.is_zero() {
            return;
        }
        if let Ok(mut rates) = self.rates.write() {
            rates.insert(TokenAnchorKey::new(chain_id, token), rate);
        }
    }

    #[must_use]
    pub fn cached_native_usd_rate(&self, chain_id: u64) -> Option<U256> {
        self.native_usd_rates
            .read()
            .ok()
            .and_then(|rates| rates.get(&chain_id).copied())
    }

    pub fn store_native_usd_rate(&self, chain_id: u64, rate: U256) {
        if rate.is_zero() {
            return;
        }
        if let Ok(mut rates) = self.native_usd_rates.write() {
            rates.insert(chain_id, rate);
        }
    }

    #[must_use]
    pub fn cached_token_usd_micro_value(
        &self,
        chain_id: u64,
        token: Address,
        amount: U256,
    ) -> Option<U256> {
        let token_anchor_rate = self
            .cached_rate(chain_id, token)
            .or_else(|| fixed_token_anchor_rate(chain_id, token))?;
        let native_usd_rate = self.cached_native_usd_rate(chain_id)?;
        token_usd_micro_value(amount, token_anchor_rate, native_usd_rate)
    }

    #[must_use]
    pub fn cached_native_usd_micro_value(&self, chain_id: u64, amount: U256) -> Option<U256> {
        native_usd_micro_value(amount, self.cached_native_usd_rate(chain_id)?)
    }

    #[must_use]
    pub fn subscribe_refreshes(&self) -> watch::Receiver<u64> {
        self.refresh_tx.subscribe()
    }

    fn notify_refreshed(&self) {
        let current = *self.refresh_tx.borrow();
        let _ = self.refresh_tx.send(current.wrapping_add(1));
    }
}

#[derive(Debug)]
pub struct TokenAnchorRefreshHandle {
    wake_tx: watch::Sender<u64>,
    abort_handle: AbortHandle,
}

impl TokenAnchorRefreshHandle {
    pub fn wake(&self) {
        let current = *self.wake_tx.borrow();
        let _ = self.wake_tx.send(current.wrapping_add(1));
    }
}

impl Drop for TokenAnchorRefreshHandle {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

#[must_use]
pub fn spawn_token_anchor_refresh_worker(
    runtime: &Handle,
    cache: Arc<TokenAnchorRateCache>,
    chain_ids: Vec<u64>,
    effective_chains: BTreeMap<u64, EffectiveChainConfig>,
    token_registry: EffectiveTokenRegistry,
    http: HttpContext,
) -> TokenAnchorRefreshHandle {
    let (wake_tx, wake_rx) = watch::channel(0_u64);
    let task = runtime.spawn(run_token_anchor_refresh_worker(
        cache,
        chain_ids,
        effective_chains,
        token_registry,
        http,
        wake_rx,
    ));
    TokenAnchorRefreshHandle {
        wake_tx,
        abort_handle: task.abort_handle(),
    }
}

async fn run_token_anchor_refresh_worker(
    cache: Arc<TokenAnchorRateCache>,
    chain_ids: Vec<u64>,
    effective_chains: BTreeMap<u64, EffectiveChainConfig>,
    token_registry: EffectiveTokenRegistry,
    http: HttpContext,
    mut wake_rx: watch::Receiver<u64>,
) {
    refresh_token_anchor_rates(
        &cache,
        &chain_ids,
        &effective_chains,
        &token_registry,
        &http,
    )
    .await;
    let mut last_refresh = Instant::now();
    let mut next_refresh =
        last_refresh + token_anchor_refresh_delay(&cache, &chain_ids, &token_registry);
    loop {
        tokio::select! {
            () = sleep_until(next_refresh) => {
                refresh_token_anchor_rates(&cache, &chain_ids, &effective_chains, &token_registry, &http).await;
                last_refresh = Instant::now();
                next_refresh = last_refresh + token_anchor_refresh_delay(&cache, &chain_ids, &token_registry);
            }
            changed = wake_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                if last_refresh.elapsed() >= TOKEN_ANCHOR_WAKE_REFRESH_MIN_INTERVAL {
                    refresh_token_anchor_rates(&cache, &chain_ids, &effective_chains, &token_registry, &http).await;
                    last_refresh = Instant::now();
                    next_refresh = last_refresh + token_anchor_refresh_delay(&cache, &chain_ids, &token_registry);
                }
            }
        }
    }
}

fn token_anchor_refresh_delay(
    cache: &TokenAnchorRateCache,
    chain_ids: &[u64],
    token_registry: &EffectiveTokenRegistry,
) -> Duration {
    let missing_native_rate = chain_ids
        .iter()
        .any(|chain_id| cache.cached_native_usd_rate(*chain_id).is_none());
    let missing_token_rate = token_anchor_entries_for_chains(chain_ids, token_registry)
        .into_iter()
        .any(|entry| cache.cached_rate(entry.chain_id, entry.token).is_none());
    if missing_native_rate || missing_token_rate {
        TOKEN_ANCHOR_MISSING_RATE_RETRY_INTERVAL
    } else {
        TOKEN_ANCHOR_REFRESH_INTERVAL
    }
}

pub async fn refresh_token_anchor_rates(
    cache: &TokenAnchorRateCache,
    chain_ids: &[u64],
    effective_chains: &BTreeMap<u64, EffectiveChainConfig>,
    token_registry: &EffectiveTokenRegistry,
    http: &HttpContext,
) {
    tracing::debug!(
        chain_count = chain_ids.len(),
        "token anchor refresh started"
    );
    let entries = token_anchor_entries_for_chains(chain_ids, token_registry);
    let native_entries = native_usd_anchor_entries_for_chains(chain_ids);
    let oracle_addresses_by_chain =
        oracle_addresses_for_token_and_native_entries(&entries, &native_entries);
    let (pool_keys, observation_keys) = twap_keys_for_entries(&entries, &native_entries);
    refresh_token_anchor_rates_with_fetch(
        cache,
        &entries,
        &native_entries,
        oracle_addresses_by_chain,
        pool_keys,
        observation_keys,
        TOKEN_ANCHOR_CHAIN_REFRESH_TIMEOUT,
        |plan| {
            Box::pin(async move {
                match plan {
                    AnchorSourcePlan::Chainlink {
                        chain_id,
                        addresses,
                    } => AnchorSourceResult::Chainlink {
                        chain_id,
                        result: fetch_oracle_answers_for_chain(
                            chain_id,
                            &addresses,
                            effective_chains,
                            http,
                        )
                        .await,
                    },
                    AnchorSourcePlan::Twap {
                        chain_id,
                        pools,
                        observations,
                    } => AnchorSourceResult::Twap {
                        chain_id,
                        result: fetch_twap_inputs_for_chain(
                            chain_id,
                            &pools,
                            &observations,
                            effective_chains,
                            http,
                        )
                        .await,
                    },
                }
            })
        },
    )
    .await;
    let missing_native_usd_rates = chain_ids
        .iter()
        .filter(|chain_id| cache.cached_native_usd_rate(**chain_id).is_none())
        .count();
    tracing::debug!(
        chain_count = chain_ids.len(),
        missing_native_usd_rates,
        "token anchor refresh complete"
    );
    cache.notify_refreshed();
}

enum AnchorSourcePlan {
    Chainlink {
        chain_id: u64,
        addresses: Vec<Address>,
    },
    Twap {
        chain_id: u64,
        pools: Vec<PoolKey>,
        observations: Vec<ObservationKey>,
    },
}

enum AnchorSourceResult {
    Chainlink {
        chain_id: u64,
        result: Result<BTreeMap<Address, U256>>,
    },
    Twap {
        chain_id: u64,
        result: Result<TwapFetchedInputs>,
    },
}

type AnchorSourceFuture<'a> = Pin<Box<dyn Future<Output = AnchorSourceResult> + Send + 'a>>;

async fn refresh_token_anchor_rates_with_fetch<'a, F>(
    cache: &TokenAnchorRateCache,
    entries: &[RuntimeTokenAnchorInfo],
    native_entries: &[RuntimeNativeUsdAnchorInfo],
    oracle_addresses_by_chain: BTreeMap<u64, Vec<Address>>,
    pool_keys: BTreeSet<PoolKey>,
    observation_keys: BTreeSet<ObservationKey>,
    deadline: Duration,
    mut fetch: F,
) where
    F: FnMut(AnchorSourcePlan) -> AnchorSourceFuture<'a>,
{
    let mut oracle_answers = BTreeMap::new();
    let mut twap_inputs = TwapFetchedInputs::default();
    store_anchor_rates_from_entries_with_inputs(cache, entries, &oracle_answers, &twap_inputs);
    store_native_usd_rates_from_entries_with_inputs(
        cache,
        native_entries,
        &oracle_answers,
        &twap_inputs,
    );

    let mut pending = FuturesUnordered::new();
    for (chain_id, addresses) in oracle_addresses_by_chain {
        pending.push(fetch(AnchorSourcePlan::Chainlink {
            chain_id,
            addresses,
        }));
    }
    let mut twap_chains = BTreeSet::new();
    twap_chains.extend(pool_keys.iter().map(|key| key.chain_id));
    twap_chains.extend(observation_keys.iter().map(|key| key.chain_id));
    for chain_id in twap_chains {
        pending.push(fetch(AnchorSourcePlan::Twap {
            chain_id,
            pools: pool_keys
                .iter()
                .copied()
                .filter(|key| key.chain_id == chain_id)
                .collect(),
            observations: observation_keys
                .iter()
                .copied()
                .filter(|key| key.chain_id == chain_id)
                .collect(),
        }));
    }

    let deadline = Instant::now() + deadline;
    while let Ok(Some(result)) = tokio::time::timeout_at(deadline, pending.next()).await {
        match result {
            AnchorSourceResult::Chainlink { chain_id, result } => match result {
                Ok(answers) => {
                    oracle_answers.extend(
                        answers
                            .into_iter()
                            .map(|(address, answer)| ((chain_id, address), answer)),
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        source_kind = "chainlink",
                        chain_id,
                        "anchor source fetch failed"
                    );
                }
            },
            AnchorSourceResult::Twap { chain_id, result } => match result {
                Ok(inputs) => {
                    twap_inputs.metadata.extend(inputs.metadata);
                    twap_inputs.observations.extend(inputs.observations);
                }
                Err(_) => {
                    tracing::warn!(
                        source_kind = "uniswap_v3_twap",
                        chain_id,
                        "anchor source fetch failed"
                    );
                }
            },
        }
        store_anchor_rates_from_entries_with_inputs(cache, entries, &oracle_answers, &twap_inputs);
        store_native_usd_rates_from_entries_with_inputs(
            cache,
            native_entries,
            &oracle_answers,
            &twap_inputs,
        );
    }
    store_anchor_rates_from_entries_with_inputs(cache, entries, &oracle_answers, &twap_inputs);
    store_native_usd_rates_from_entries_with_inputs(
        cache,
        native_entries,
        &oracle_answers,
        &twap_inputs,
    );
}

async fn fetch_oracle_answers_for_chain(
    chain_id: u64,
    oracle_addresses: &[Address],
    effective_chains: &BTreeMap<u64, EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<BTreeMap<Address, U256>> {
    fetch_oracle_answers_for_chain_with_timeout(
        chain_id,
        oracle_addresses,
        effective_chains,
        http,
        TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT,
    )
    .await
}

async fn fetch_oracle_answers_for_chain_with_timeout(
    chain_id: u64,
    oracle_addresses: &[Address],
    effective_chains: &BTreeMap<u64, EffectiveChainConfig>,
    http: &HttpContext,
    request_timeout: Duration,
) -> Result<BTreeMap<Address, U256>> {
    let (query_rpc_pool, multicall_addr) = provider_for_chain(chain_id, effective_chains, http)?;
    let mut last_error = None;
    let mut results = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        let mut multicall = provider_handle
            .provider
            .multicall()
            .dynamic::<AggregatorInterface::latestAnswerCall>()
            .address(multicall_addr);
        for oracle_address in oracle_addresses {
            multicall = multicall.add_call_dynamic(CallItem::new(
                *oracle_address,
                AggregatorInterface::latestAnswerCall {}.abi_encode().into(),
            ));
        }

        match timeout(request_timeout, multicall.try_aggregate(false)).await {
            Ok(Ok(values)) => {
                results = Some(values);
                break;
            }
            Ok(Err(_)) => {
                tracing::warn!(chain_id, "multicall anchor oracle answers failed");
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre::eyre!("anchor oracle RPC request failed"));
            }
            Err(_) => {
                tracing::warn!(
                    chain_id,
                    timeout_millis = request_timeout.as_millis(),
                    "multicall anchor oracle answers timed out"
                );
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre::eyre!(
                    "anchor oracle multicall timed out after {} milliseconds",
                    request_timeout.as_millis()
                ));
            }
        }
    }
    let results = results.ok_or_else(|| {
        last_error.map_or_else(
            || eyre::eyre!("no healthy query RPC available for chain {chain_id}"),
            |error| error.wrap_err("multicall anchor oracle answers"),
        )
    })?;
    let mut answers = BTreeMap::new();
    for (oracle_address, result) in oracle_addresses.iter().copied().zip(results) {
        match result {
            Ok(answer) => match U256::try_from(answer) {
                Ok(price) if !price.is_zero() => {
                    answers.insert(oracle_address, price);
                }
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!(chain_id, ?oracle_address, %answer, "discarding negative anchor oracle answer");
                }
            },
            Err(error) => {
                tracing::warn!(chain_id, ?oracle_address, %error, "discarding failed anchor oracle call");
            }
        }
    }
    Ok(answers)
}

fn provider_for_chain(
    chain_id: u64,
    effective_chains: &BTreeMap<u64, EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<(Arc<QueryRpcPool>, Address)> {
    let defaults = ChainConfigDefaults::for_chain(chain_id)
        .ok_or_else(|| eyre::eyre!("unsupported chain id {chain_id}"))?;
    let effective_chain = effective_chains.get(&chain_id);
    let rpc_urls = effective_rpc_urls_for_chain(&defaults, effective_chain)?;
    let multicall_contract = if let Some(effective_chain) = effective_chain {
        Address::from_str(&effective_chain.multicall_contract)
            .wrap_err("parse effective multicall contract")?
    } else {
        defaults.multicall_contract
    };
    Ok((
        query_rpc_pool_with_http_client(rpc_urls, http),
        multicall_contract,
    ))
}

fn token_anchor_entries_for_chains(
    chain_ids: &[u64],
    token_registry: &EffectiveTokenRegistry,
) -> Vec<RuntimeTokenAnchorInfo> {
    let chain_ids = chain_ids.iter().copied().collect::<BTreeSet<_>>();
    let registry_tokens = token_registry
        .tokens
        .values()
        .filter(|token| chain_ids.contains(&token.chain_id))
        .filter_map(|token| {
            Address::from_str(&token.token_address)
                .ok()
                .map(|address| ((token.chain_id, address), token.price_anchor.as_ref()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut entries = token_anchor_entries()
        .filter(|entry| chain_ids.contains(&entry.chain_id))
        .filter(|entry| registry_tokens.contains_key(&(entry.chain_id, entry.token)))
        .map(static_anchor_entry_to_runtime)
        .collect::<BTreeMap<_, _>>();
    for ((chain_id, token), anchor) in registry_tokens {
        if let Some(anchor) = anchor.and_then(price_anchor_to_runtime_sources) {
            entries.insert(
                (chain_id, token),
                RuntimeTokenAnchorInfo {
                    chain_id,
                    token,
                    anchor_sources: anchor,
                },
            );
        }
    }
    entries.into_values().collect()
}

fn static_anchor_entry_to_runtime(
    entry: TokenAnchorInfo,
) -> ((u64, Address), RuntimeTokenAnchorInfo) {
    (
        (entry.chain_id, entry.token),
        RuntimeTokenAnchorInfo {
            chain_id: entry.chain_id,
            token: entry.token,
            anchor_sources: entry
                .anchor_sources
                .iter()
                .map(|source| static_anchor_source_to_runtime(entry.chain_id, source))
                .collect(),
        },
    )
}

fn native_usd_anchor_entries_for_chains(chain_ids: &[u64]) -> Vec<RuntimeNativeUsdAnchorInfo> {
    let chain_ids = chain_ids.iter().copied().collect::<BTreeSet<_>>();
    native_usd_anchor_entries()
        .filter(|entry| chain_ids.contains(&entry.chain_id))
        .map(static_native_usd_entry_to_runtime)
        .collect()
}

fn static_native_usd_entry_to_runtime(entry: NativeUsdAnchorInfo) -> RuntimeNativeUsdAnchorInfo {
    RuntimeNativeUsdAnchorInfo {
        chain_id: entry.chain_id,
        anchor_sources: entry
            .anchor_sources
            .iter()
            .map(|source| static_anchor_source_to_runtime(entry.chain_id, source))
            .collect(),
    }
}

fn static_anchor_source_to_runtime(
    chain_id: u64,
    source: &TokenAnchorSource,
) -> RuntimeTokenAnchorSource {
    match source {
        TokenAnchorSource::Fixed {
            token_fee_per_unit_gas,
        } => RuntimeTokenAnchorSource::Fixed {
            token_fee_per_unit_gas: *token_fee_per_unit_gas,
        },
        TokenAnchorSource::ChainlinkOracle {
            addr,
            token_decimals,
            oracle_decimals,
            is_inversed,
        } => RuntimeTokenAnchorSource::ChainlinkOracle {
            chain_id,
            addr: *addr,
            token_decimals: *token_decimals,
            oracle_decimals: *oracle_decimals,
            is_inversed: *is_inversed,
        },
        TokenAnchorSource::UniswapV3Twap {
            pool,
            base_token,
            quote_token,
            base_token_decimals,
            window_seconds,
        } => RuntimeTokenAnchorSource::UniswapV3Twap {
            pool: *pool,
            base_token: *base_token,
            quote_token: *quote_token,
            base_token_decimals: *base_token_decimals,
            window_seconds: *window_seconds,
        },
        TokenAnchorSource::Product {
            sources,
            scale_decimals,
        } => RuntimeTokenAnchorSource::Product {
            sources: sources
                .iter()
                .map(|source| static_anchor_source_to_runtime(chain_id, source))
                .collect(),
            scale_decimals: *scale_decimals,
        },
    }
}

fn price_anchor_to_runtime_sources(
    anchor: &PriceAnchorSettings,
) -> Option<Vec<RuntimeTokenAnchorSource>> {
    Some(vec![price_anchor_to_runtime_source(anchor)?])
}

fn price_anchor_to_runtime_source(
    anchor: &PriceAnchorSettings,
) -> Option<RuntimeTokenAnchorSource> {
    match anchor {
        PriceAnchorSettings::Fixed { rate } => Some(RuntimeTokenAnchorSource::Fixed {
            token_fee_per_unit_gas: U256::from_str_radix(rate, 10).ok()?,
        }),
        PriceAnchorSettings::Oracle {
            chain_id,
            oracle_address,
            token_decimals,
            oracle_decimals,
            is_inversed,
        } => Some(RuntimeTokenAnchorSource::ChainlinkOracle {
            chain_id: *chain_id,
            addr: Address::from_str(oracle_address).ok()?,
            token_decimals: *token_decimals,
            oracle_decimals: *oracle_decimals,
            is_inversed: *is_inversed,
        }),
        PriceAnchorSettings::UniswapV3Twap {
            pool_address,
            base_token_address,
            quote_token_address,
            base_token_decimals,
            window_seconds,
        } => Some(RuntimeTokenAnchorSource::UniswapV3Twap {
            pool: Address::from_str(pool_address).ok()?,
            base_token: Address::from_str(base_token_address).ok()?,
            quote_token: Address::from_str(quote_token_address).ok()?,
            base_token_decimals: *base_token_decimals,
            window_seconds: *window_seconds,
        }),
        PriceAnchorSettings::Product {
            components,
            scale_decimals,
        } => Some(RuntimeTokenAnchorSource::Product {
            sources: components
                .iter()
                .map(price_anchor_to_runtime_source)
                .collect::<Option<Vec<_>>>()?,
            scale_decimals: *scale_decimals,
        }),
    }
}

#[cfg(test)]
fn oracle_addresses_for_entries(entries: &[RuntimeTokenAnchorInfo]) -> BTreeMap<u64, Vec<Address>> {
    let mut addresses: BTreeMap<u64, BTreeSet<Address>> = BTreeMap::new();
    for entry in entries {
        for source in &entry.anchor_sources {
            collect_oracle_addresses_from_source(source, &mut addresses);
        }
    }
    addresses
        .into_iter()
        .map(|(chain_id, addresses)| (chain_id, addresses.into_iter().collect()))
        .collect()
}

fn oracle_addresses_for_token_and_native_entries(
    entries: &[RuntimeTokenAnchorInfo],
    native_entries: &[RuntimeNativeUsdAnchorInfo],
) -> BTreeMap<u64, Vec<Address>> {
    let mut addresses: BTreeMap<u64, BTreeSet<Address>> = BTreeMap::new();
    for entry in entries {
        for source in &entry.anchor_sources {
            collect_oracle_addresses_from_source(source, &mut addresses);
        }
    }
    for entry in native_entries {
        for source in &entry.anchor_sources {
            collect_oracle_addresses_from_source(source, &mut addresses);
        }
    }
    addresses
        .into_iter()
        .map(|(chain_id, addresses)| (chain_id, addresses.into_iter().collect()))
        .collect()
}

fn collect_oracle_addresses_from_source(
    source: &RuntimeTokenAnchorSource,
    addresses: &mut BTreeMap<u64, BTreeSet<Address>>,
) {
    match source {
        RuntimeTokenAnchorSource::Fixed { .. } | RuntimeTokenAnchorSource::UniswapV3Twap { .. } => {
        }
        RuntimeTokenAnchorSource::ChainlinkOracle { chain_id, addr, .. } => {
            addresses.entry(*chain_id).or_default().insert(*addr);
        }
        RuntimeTokenAnchorSource::Product { sources, .. } => {
            for source in sources {
                collect_oracle_addresses_from_source(source, addresses);
            }
        }
    }
}

fn collect_twap_keys_from_source(
    owner_chain_id: u64,
    source: &RuntimeTokenAnchorSource,
    pools: &mut BTreeSet<PoolKey>,
    observations: &mut BTreeSet<ObservationKey>,
) {
    match source {
        RuntimeTokenAnchorSource::UniswapV3Twap {
            pool,
            window_seconds,
            ..
        } => {
            pools.insert(PoolKey {
                chain_id: owner_chain_id,
                pool: *pool,
            });
            observations.insert(ObservationKey {
                chain_id: owner_chain_id,
                pool: *pool,
                window_seconds: *window_seconds,
            });
        }
        RuntimeTokenAnchorSource::Product { sources, .. } => {
            for source in sources {
                collect_twap_keys_from_source(owner_chain_id, source, pools, observations);
            }
        }
        RuntimeTokenAnchorSource::Fixed { .. }
        | RuntimeTokenAnchorSource::ChainlinkOracle { .. } => {}
    }
}

fn twap_keys_for_entries(
    entries: &[RuntimeTokenAnchorInfo],
    native_entries: &[RuntimeNativeUsdAnchorInfo],
) -> (BTreeSet<PoolKey>, BTreeSet<ObservationKey>) {
    let mut pools = BTreeSet::new();
    let mut observations = BTreeSet::new();
    for entry in entries {
        for source in &entry.anchor_sources {
            collect_twap_keys_from_source(entry.chain_id, source, &mut pools, &mut observations);
        }
    }
    for entry in native_entries {
        for source in &entry.anchor_sources {
            collect_twap_keys_from_source(entry.chain_id, source, &mut pools, &mut observations);
        }
    }
    (pools, observations)
}

async fn fetch_twap_inputs_for_chain(
    chain_id: u64,
    pools: &[PoolKey],
    observations: &[ObservationKey],
    effective_chains: &BTreeMap<u64, EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<TwapFetchedInputs> {
    fetch_twap_inputs_for_chain_with_timeout(
        chain_id,
        pools,
        observations,
        effective_chains,
        http,
        TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT,
    )
    .await
}

async fn fetch_twap_inputs_for_chain_with_timeout(
    chain_id: u64,
    pools: &[PoolKey],
    observations: &[ObservationKey],
    effective_chains: &BTreeMap<u64, EffectiveChainConfig>,
    http: &HttpContext,
    request_timeout: Duration,
) -> Result<TwapFetchedInputs> {
    if pools.is_empty() && observations.is_empty() {
        return Ok(TwapFetchedInputs::default());
    }
    let (query_rpc_pool, multicall_addr) = provider_for_chain(chain_id, effective_chains, http)?;
    let mut last_error = None;
    let mut selected = None;
    for provider_handle in query_rpc_pool.available_providers() {
        let mut metadata_call = provider_handle
            .provider
            .multicall()
            .dynamic::<UniswapV3PoolInterface::token0Call>()
            .address(multicall_addr);
        for pool in pools {
            metadata_call = metadata_call.add_call_dynamic(CallItem::new(
                pool.pool,
                UniswapV3PoolInterface::token0Call {}.abi_encode().into(),
            ));
            metadata_call = metadata_call.add_call_dynamic(CallItem::new(
                pool.pool,
                UniswapV3PoolInterface::token1Call {}.abi_encode().into(),
            ));
        }
        let mut observation_call = provider_handle
            .provider
            .multicall()
            .dynamic::<UniswapV3PoolInterface::observeCall>()
            .address(multicall_addr);
        for observation in observations {
            observation_call = observation_call.add_call_dynamic(CallItem::new(
                observation.pool,
                UniswapV3PoolInterface::observeCall {
                    secondsAgos: vec![observation.window_seconds, 0],
                }
                .abi_encode()
                .into(),
            ));
        }
        let calls = async {
            let metadata = metadata_call
                .try_aggregate(false)
                .await
                .map_err(|error| eyre::eyre!("metadata batch: {error}"))?;
            let observations = observation_call
                .try_aggregate(false)
                .await
                .map_err(|error| eyre::eyre!("observation batch: {error}"))?;
            Ok::<_, eyre::Report>((metadata, observations))
        };
        match timeout(request_timeout, calls).await {
            Ok(Ok(values)) => {
                selected = Some(values);
                break;
            }
            Ok(Err(error)) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre::eyre!("uniswap v3 multicall failed: {error}"));
            }
            Err(_) => {
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(eyre::eyre!("uniswap v3 multicall timed out"));
            }
        }
    }
    let (metadata_results, observation_results) = selected.ok_or_else(|| {
        last_error
            .unwrap_or_else(|| eyre::eyre!("no healthy query RPC available for chain {chain_id}"))
    })?;
    let mut fetched = TwapFetchedInputs::default();
    for (pair, key) in metadata_results.chunks_exact(2).zip(pools.iter().copied()) {
        let (Ok(token0), Ok(token1)) = (pair[0].clone(), pair[1].clone()) else {
            continue;
        };
        fetched
            .metadata
            .insert(key, PoolMetadata { token0, token1 });
    }
    for (result, key) in observation_results
        .into_iter()
        .zip(observations.iter().copied())
    {
        let Ok(decoded) = result else { continue };
        if decoded.tickCumulatives.len() == 2 {
            let Ok(tick_cumulatives) = decoded
                .tickCumulatives
                .into_iter()
                .map(i128::try_from)
                .collect::<std::result::Result<Vec<_>, _>>()
            else {
                continue;
            };
            fetched
                .observations
                .insert(key, TwapObservation { tick_cumulatives });
        }
    }
    Ok(fetched)
}

fn store_anchor_rates_from_entries_with_inputs(
    cache: &TokenAnchorRateCache,
    entries: &[RuntimeTokenAnchorInfo],
    oracle_answers: &BTreeMap<(u64, Address), U256>,
    twap_inputs: &TwapFetchedInputs,
) {
    for entry in entries {
        let rates = anchor_rates_from_sources_with_inputs(
            entry.chain_id,
            &entry.anchor_sources,
            oracle_answers,
            twap_inputs,
        );
        if let Some(rate) = average_non_outlier_anchor_rates(&rates) {
            cache.store_rate(entry.chain_id, entry.token, rate);
        }
    }
}

fn store_native_usd_rates_from_entries_with_inputs(
    cache: &TokenAnchorRateCache,
    entries: &[RuntimeNativeUsdAnchorInfo],
    oracle_answers: &BTreeMap<(u64, Address), U256>,
    twap_inputs: &TwapFetchedInputs,
) {
    for entry in entries {
        let rates = anchor_rates_from_sources_with_inputs(
            entry.chain_id,
            &entry.anchor_sources,
            oracle_answers,
            twap_inputs,
        );
        if let Some(rate) = average_non_outlier_anchor_rates(&rates) {
            cache.store_native_usd_rate(entry.chain_id, rate);
        }
    }
}

fn anchor_rates_from_sources_with_inputs(
    owner_chain_id: u64,
    sources: &[RuntimeTokenAnchorSource],
    oracle_answers: &BTreeMap<(u64, Address), U256>,
    twap_inputs: &TwapFetchedInputs,
) -> Vec<U256> {
    sources
        .iter()
        .filter_map(|source| {
            anchor_rate_from_source_with_inputs(owner_chain_id, source, oracle_answers, twap_inputs)
        })
        .collect()
}

fn anchor_rate_from_source_with_inputs(
    owner_chain_id: u64,
    source: &RuntimeTokenAnchorSource,
    oracle_answers: &BTreeMap<(u64, Address), U256>,
    twap_inputs: &TwapFetchedInputs,
) -> Option<U256> {
    match source {
        RuntimeTokenAnchorSource::UniswapV3Twap {
            pool,
            base_token,
            quote_token,
            base_token_decimals,
            window_seconds,
        } => {
            let pool_key = PoolKey {
                chain_id: owner_chain_id,
                pool: *pool,
            };
            let metadata = twap_inputs.metadata.get(&pool_key)?;
            let base_is_token0 =
                if metadata.token0 == *base_token && metadata.token1 == *quote_token {
                    true
                } else if metadata.token0 == *quote_token && metadata.token1 == *base_token {
                    false
                } else {
                    return None;
                };
            let observation_key = ObservationKey {
                chain_id: owner_chain_id,
                pool: *pool,
                window_seconds: *window_seconds,
            };
            let observation = twap_inputs.observations.get(&observation_key)?;
            uniswap_v3_twap::quote_from_observation(
                &observation.tick_cumulatives,
                *window_seconds,
                base_is_token0,
                *base_token_decimals,
            )
        }
        RuntimeTokenAnchorSource::Product {
            sources,
            scale_decimals,
        } => product_anchor_rate_with_inputs(
            owner_chain_id,
            sources,
            *scale_decimals,
            oracle_answers,
            twap_inputs,
        ),
        RuntimeTokenAnchorSource::Fixed {
            token_fee_per_unit_gas,
        } => non_zero_rate(*token_fee_per_unit_gas),
        RuntimeTokenAnchorSource::ChainlinkOracle {
            chain_id,
            addr,
            token_decimals,
            oracle_decimals,
            is_inversed,
        } => oracle_answers.get(&(*chain_id, *addr)).and_then(|price| {
            oracle_answer_to_anchor_rate(*price, *token_decimals, *oracle_decimals, *is_inversed)
        }),
    }
}

fn product_anchor_rate_with_inputs(
    owner_chain_id: u64,
    sources: &[RuntimeTokenAnchorSource],
    scale_decimals: u8,
    oracle_answers: &BTreeMap<(u64, Address), U256>,
    twap_inputs: &TwapFetchedInputs,
) -> Option<U256> {
    let scale = checked_pow10(scale_decimals)?;
    let mut rates = sources.iter().map(|source| {
        anchor_rate_from_source_with_inputs(owner_chain_id, source, oracle_answers, twap_inputs)
    });
    let mut product = rates.next()??;
    for rate in rates {
        product = product.checked_mul(rate?)?.checked_div(scale)?;
    }
    non_zero_rate(product)
}

#[must_use]
pub fn known_token_anchor_sources(
    chain_id: u64,
    token: Address,
) -> Option<&'static [TokenAnchorSource]> {
    lookup_token(chain_id, &token).map(|info| info.anchor_sources)
}

#[must_use]
pub fn fixed_token_anchor_rate(chain_id: u64, token: Address) -> Option<U256> {
    known_token_anchor_sources(chain_id, token)?
        .iter()
        .find_map(|source| match source {
            TokenAnchorSource::Fixed {
                token_fee_per_unit_gas,
            } => Some(*token_fee_per_unit_gas),
            TokenAnchorSource::ChainlinkOracle { .. }
            | TokenAnchorSource::UniswapV3Twap { .. }
            | TokenAnchorSource::Product { .. } => None,
        })
}

#[must_use]
pub fn oracle_answer_to_anchor_rate(
    price: U256,
    token_decimals: u8,
    oracle_decimals: u8,
    is_inversed: bool,
) -> Option<U256> {
    if price.is_zero() {
        return None;
    }
    let token_scale = checked_pow10(token_decimals)?;
    let oracle_scale = checked_pow10(oracle_decimals)?;
    let rate = if is_inversed {
        token_scale.checked_mul(oracle_scale)?.checked_div(price)?
    } else {
        price.checked_mul(token_scale)?.checked_div(oracle_scale)?
    };
    non_zero_rate(rate)
}

#[must_use]
pub fn average_non_outlier_anchor_rates(rates: &[U256]) -> Option<U256> {
    let mut sorted = rates
        .iter()
        .copied()
        .filter(|rate| !rate.is_zero())
        .collect::<Vec<_>>();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_unstable();
    let median = median_rate(&sorted)?;
    if median.is_zero() {
        return None;
    }
    let survivors = sorted
        .into_iter()
        .filter(|rate| within_outlier_threshold(*rate, median))
        .collect::<Vec<_>>();
    checked_average(&survivors)
}

fn median_rate(sorted: &[U256]) -> Option<U256> {
    match sorted.len() {
        0 => None,
        len if len % 2 == 1 => Some(sorted[len / 2]),
        len => Some(checked_average_pair(sorted[len / 2 - 1], sorted[len / 2])),
    }
}

fn checked_average_pair(a: U256, b: U256) -> U256 {
    let half = a / U256::from(2) + b / U256::from(2);
    half + U256::from(u8::from(
        a % U256::from(2) + b % U256::from(2) >= U256::from(2),
    ))
}

fn within_outlier_threshold(rate: U256, median: U256) -> bool {
    let diff = match rate.cmp(&median) {
        Ordering::Less => median - rate,
        Ordering::Equal => return true,
        Ordering::Greater => rate - median,
    };
    let Some(scaled_diff) = diff.checked_mul(BPS_DENOMINATOR) else {
        return false;
    };
    let Some(threshold) = median.checked_mul(ANCHOR_OUTLIER_THRESHOLD_BPS) else {
        return false;
    };
    scaled_diff <= threshold
}

fn checked_average(rates: &[U256]) -> Option<U256> {
    if rates.is_empty() {
        return None;
    }
    let total = rates
        .iter()
        .copied()
        .try_fold(U256::ZERO, U256::checked_add)?;
    non_zero_rate(total / U256::from(rates.len()))
}

fn checked_pow10(exp: u8) -> Option<U256> {
    U256::from(10).checked_pow(U256::from(exp))
}

fn non_zero_rate(rate: U256) -> Option<U256> {
    if rate.is_zero() { None } else { Some(rate) }
}

#[cfg(test)]
mod tests {
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
            for _ in 0..request_count {
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
                    alloy::providers::bindings::IMulticall3::tryAggregateCall::abi_decode(
                        &call_bytes,
                    )
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
                        let decoded =
                            UniswapV3PoolInterface::observeCall::abi_decode(&call.callData)
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
                                UniswapV3PoolInterface::token1Call::abi_encode_returns(&quote)
                                    .into()
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
                                UniswapV3PoolInterface::observeCall::abi_encode_returns(&decoded)
                                    .into()
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
        let rate =
            oracle_answer_to_anchor_rate(uint!(3_000_00000000_U256), 6, 8, false).expect("rate");

        assert_eq!(rate, uint!(3_000_000_000_U256));
    }

    #[test]
    fn oracle_answer_to_anchor_rate_handles_inverted_feed() {
        let rate =
            oracle_answer_to_anchor_rate(uint!(15_000_000_000_000_000_000_U256), 8, 18, true)
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
    fn cache_stores_native_usd_rates_by_chain() {
        let cache = TokenAnchorRateCache::new();

        cache.store_native_usd_rate(1, U256::ZERO);
        assert_eq!(cache.cached_native_usd_rate(1), None);

        cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));

        assert_eq!(
            cache.cached_native_usd_rate(1),
            Some(uint!(3_000_000_000_U256))
        );
        assert_eq!(cache.cached_native_usd_rate(56), None);
    }

    #[test]
    fn missing_native_or_token_rates_use_fast_refresh_retry() {
        let cache = TokenAnchorRateCache::new();
        let settings = crate::settings::WalletSettings::default();
        let token_registry = crate::settings::build_effective_token_registry(&settings)
            .expect("effective token registry");

        assert_eq!(
            token_anchor_refresh_delay(&cache, &[1], &token_registry),
            TOKEN_ANCHOR_MISSING_RATE_RETRY_INTERVAL
        );

        cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
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
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        effective_chains
            .get_mut(&1)
            .expect("Ethereum config")
            .rpc_endpoints = vec![rpc_url];
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
            &http,
            Duration::from_millis(25),
        )
        .await
        .expect_err("unresponsive oracle RPC must time out");

        assert!(format!("{error:#}").contains("timed out after 25 milliseconds"));
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
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        effective_chains
            .get_mut(&1)
            .expect("Ethereum config")
            .rpc_endpoints = vec![rpc_url];
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
            &http,
            Duration::from_millis(25),
        )
        .await
        .expect_err("unresponsive TWAP RPC must time out");
        assert!(format!("{error:#}").contains("timed out"));
    }

    #[tokio::test]
    async fn twap_fetch_decodes_two_typed_batches_and_routes_non_ethereum_chain() {
        let multicall = address!("0x0000000000000000000000000000000000000600");
        let (rpc_url, requests, server) =
            spawn_twap_rpc_fixture(multicall, TWAP_BASE, TWAP_QUOTE, 1_800, 2, false);
        let settings = crate::settings::WalletSettings::default();
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        effective_chains
            .get_mut(&42161)
            .expect("Arbitrum config")
            .rpc_endpoints = vec![rpc_url];
        effective_chains
            .get_mut(&42161)
            .expect("Arbitrum config")
            .multicall_contract = multicall.to_string();
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
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        effective_chains
            .get_mut(&42161)
            .expect("Arbitrum config")
            .rpc_endpoints = vec![rpc_url];
        effective_chains
            .get_mut(&42161)
            .expect("Arbitrum config")
            .multicall_contract = multicall.to_string();
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
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        effective_chains
            .get_mut(&42161)
            .expect("Arbitrum config")
            .rpc_endpoints = vec![dead_url, healthy_url];
        effective_chains
            .get_mut(&42161)
            .expect("Arbitrum config")
            .multicall_contract = multicall.to_string();
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
                anchor_sources: vec![successful_source.clone()],
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
        let native_entries = [RuntimeNativeUsdAnchorInfo {
            chain_id: 42,
            anchor_sources: vec![successful_source],
        }];
        let oracle_addresses_by_chain =
            oracle_addresses_for_token_and_native_entries(&entries, &native_entries);

        let refresh = refresh_token_anchor_rates_with_fetch(
            &cache,
            &entries,
            &native_entries,
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
        assert_eq!(
            cache.cached_native_usd_rate(42),
            Some(uint!(3_000_000_000_U256))
        );
        assert_eq!(cache.cached_rate(42, pending_token), None);
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
                                    result: Err(eyre::eyre!(
                                        "provider failed at {chainlink_sentinel}"
                                    )),
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
    fn native_usd_rates_store_from_oracle_answers() {
        let cache = TokenAnchorRateCache::new();
        let entry = RuntimeNativeUsdAnchorInfo {
            chain_id: 1,
            anchor_sources: runtime_sources(SHARED_ORACLE_SOURCE_6),
        };
        let mut answers = BTreeMap::new();
        answers.insert(
            (1, address!("0x0000000000000000000000000000000000000100")),
            uint!(3_000_00000000_U256),
        );

        store_native_usd_rates_from_entries_with_inputs(
            &cache,
            &[entry],
            &answers,
            &TwapFetchedInputs::default(),
        );

        assert_eq!(
            cache.cached_native_usd_rate(1),
            Some(uint!(3_000_000_000_U256))
        );
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
        let (pools, observations) = twap_keys_for_entries(&entries, &[]);
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
        let (pool_keys, observation_keys) = twap_keys_for_entries(&entries, &[]);
        let cache = TokenAnchorRateCache::new();
        let mut fetch_count = 0;
        refresh_token_anchor_rates_with_fetch(
            &cache,
            &entries,
            &[],
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

        assert!(
            anchor_rates_from_sources_with_inputs(
                1,
                &runtime_sources(ARB_PER_ETH_ANCHOR_SOURCE),
                &answers,
                &TwapFetchedInputs::default(),
            )
            .is_empty()
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
        let mut effective_chains = crate::settings::build_effective_chain_configs(&settings)
            .expect("effective chain configs");
        effective_chains
            .get_mut(&1)
            .expect("Ethereum config")
            .rpc_endpoints = vec![rpc_url];
        effective_chains
            .get_mut(&1)
            .expect("Ethereum config")
            .multicall_contract = multicall.to_string();
        let data_dir = std::env::temp_dir();
        let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
            network_mode: Some(crate::WalletNetworkMode::Direct),
            proxy: None,
            data_dir: &data_dir,
        })
        .await
        .expect("direct HTTP context");
        let cache = TokenAnchorRateCache::new();
        let (pool_keys, observation_keys) =
            twap_keys_for_entries(std::slice::from_ref(&entry), &[]);
        refresh_token_anchor_rates_with_fetch(
            &cache,
            std::slice::from_ref(&entry),
            &[],
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
                            &http,
                        )
                        .await,
                    }
                })
            },
        )
        .await;
        server.join().expect("fixture server");
        cache.store_native_usd_rate(1, uint!(3_000_000_000_U256));
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
}
