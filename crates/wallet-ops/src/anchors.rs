use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, WrapErr};
use futures_util::stream::{FuturesUnordered, StreamExt};
use railgun_ui::{
    TokenAnchorInfo, TokenAnchorSource, lookup_token, native_usd_micro_value, token_anchor_entries,
    token_usd_micro_value,
};
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::{Instant, sleep_until};

use crate::rpc_broker::total_failure;
use crate::settings::{
    EffectiveChainRegistry, EffectiveTokenRegistry, PriceAnchorSettings, TronSettings,
    resolve_effective_chain_rpc_route,
};
use crate::tron::TRON_CHAIN_ID;
use crate::{HttpContext, RpcBrokerError, RpcChainRoute, RpcRoute, WalletRpcOrigin};

mod fresh;
mod native_usd;
mod uniswap_v3_twap;

pub use fresh::{
    AnchorBlock, AnchorBlocked, AnchorObservation, AnchorReadFailure, FreshAnchorParams,
    FreshPairAnchor, PairAnchorRate, QuoteDeviationError, check_quote_against_anchor,
    read_fresh_pair_anchor,
};
pub use native_usd::probe_native_usd_quote;

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
        function latestRoundData() external view returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
        function decimals() external view returns (uint8);
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
    native_pricing: RwLock<native_usd::NativePricing>,
    refresh_tx: watch::Sender<u64>,
}

impl Default for TokenAnchorRateCache {
    fn default() -> Self {
        let (refresh_tx, _refresh_rx) = watch::channel(0_u64);
        Self {
            rates: RwLock::new(BTreeMap::new()),
            native_pricing: RwLock::new(native_usd::NativePricing::default()),
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

    /// Uses background rates for swap quote and signing checks without a network read.
    /// A missing configured rate returns its read failure; absent configuration returns `None`.
    pub(crate) fn cached_pair_rate(
        &self,
        chain_id: u64,
        sell_token: Address,
        buy_token: Address,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<Option<PairAnchorRate>, AnchorBlocked> {
        let entries = token_anchor_entries_for_chains(&[chain_id], token_registry);
        let (Some(sell_sources), Some(buy_sources)) = (
            configured_anchor_sources(&entries, chain_id, sell_token),
            configured_anchor_sources(&entries, chain_id, buy_token),
        ) else {
            return Ok(None);
        };
        Ok(Some(PairAnchorRate {
            sell_rate: self.cached_rate_from(chain_id, sell_token, sell_sources)?,
            buy_rate: self.cached_rate_from(chain_id, buy_token, buy_sources)?,
        }))
    }

    /// Anchor rates for a bridge leg from `origin_token` on `origin_chain` to
    /// `destination_token` on `destination_chain`, from background rates only. Each token's rate
    /// is scaled by the other chain's native USD rate, so [`check_quote_against_anchor`]
    /// compares USD values without rounding. Missing configuration returns `None`; a missing
    /// token or native USD rate returns its read failure.
    pub(crate) fn cached_bridge_leg_rate(
        &self,
        origin_chain: u64,
        origin_token: Address,
        destination_chain: u64,
        destination_token: Address,
        token_registry: &EffectiveTokenRegistry,
    ) -> Result<Option<PairAnchorRate>, AnchorBlocked> {
        let entries =
            token_anchor_entries_for_chains(&[origin_chain, destination_chain], token_registry);
        let (Some(origin_sources), Some(destination_sources)) = (
            configured_anchor_sources(&entries, origin_chain, origin_token),
            configured_anchor_sources(&entries, destination_chain, destination_token),
        ) else {
            return Ok(None);
        };
        let no_rate = |token| AnchorBlocked {
            token,
            failures: vec![AnchorReadFailure::NoRate],
        };
        let origin_rate = self.cached_rate_from(origin_chain, origin_token, origin_sources)?;
        let destination_rate =
            self.cached_rate_from(destination_chain, destination_token, destination_sources)?;
        let origin_native_usd = self
            .cached_native_usd_rate(origin_chain)
            .ok_or_else(|| no_rate(origin_token))?;
        let destination_native_usd = self
            .cached_native_usd_rate(destination_chain)
            .ok_or_else(|| no_rate(destination_token))?;
        Ok(Some(PairAnchorRate {
            sell_rate: origin_rate
                .checked_mul(destination_native_usd)
                .ok_or_else(|| no_rate(origin_token))?,
            buy_rate: destination_rate
                .checked_mul(origin_native_usd)
                .ok_or_else(|| no_rate(destination_token))?,
        }))
    }

    /// The cached rate of a token with configured `sources`, or the rate of its fixed sources.
    fn cached_rate_from(
        &self,
        chain_id: u64,
        token: Address,
        sources: &[RuntimeTokenAnchorSource],
    ) -> Result<U256, AnchorBlocked> {
        self.cached_rate(chain_id, token)
            .or_else(|| {
                // Fixed sources need no background read, including user-configured ones.
                average_non_outlier_anchor_rates(&anchor_rates_from_sources_with_inputs(
                    chain_id,
                    sources,
                    &BTreeMap::new(),
                    &TwapFetchedInputs::default(),
                ))
            })
            .ok_or_else(|| AnchorBlocked {
                token,
                failures: vec![AnchorReadFailure::NoRate],
            })
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
        if railgun_ui::is_testnet_chain(chain_id) {
            return None;
        }
        self.native_pricing
            .read()
            .ok()
            .and_then(|native| native.rates.get(&chain_id).map(|(rate, _)| *rate))
    }

    pub fn store_native_usd_rate(&self, chain_id: u64, rate: U256, native_decimals: u8) {
        if rate.is_zero() {
            return;
        }
        if let Ok(mut rates) = self.native_pricing.write() {
            rates.rates.insert(chain_id, (rate, native_decimals));
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
        if railgun_ui::is_testnet_chain(chain_id) {
            return None;
        }
        let native = self.native_pricing.read().ok()?;
        let &(rate, decimals) = native.rates.get(&chain_id)?;
        native_usd_micro_value(amount, rate, decimals)
    }

    #[must_use]
    pub fn subscribe_refreshes(&self) -> watch::Receiver<u64> {
        self.refresh_tx.subscribe()
    }

    fn notify_refreshed(&self) {
        self.refresh_tx
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

#[derive(Debug)]
pub struct TokenAnchorRefreshHandle {
    wake_tx: watch::Sender<u64>,
    configuration_tx: watch::Sender<u64>,
    cache: Arc<TokenAnchorRateCache>,
    abort_handle: AbortHandle,
}

impl TokenAnchorRefreshHandle {
    /// Selects chains whose native USD sources refresh in the background.
    pub fn set_active_chains(&self, chain_ids: &[u64]) {
        if self.cache.set_active_native_chains(chain_ids) {
            self.configuration_tx
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    pub fn reconcile_native_sources(
        &self,
        chains: &EffectiveChainRegistry,
        operational_changes: &[u64],
    ) {
        if self
            .cache
            .reconcile_native_sources(chains, operational_changes)
        {
            self.configuration_tx
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

    /// Installs Tron's native USD source and the route for its native USD and token anchor
    /// reads. Call it at startup and after each save of Tron settings, with the installation's
    /// [`crate::tron::tron_rpc_route`] (`None` while Tron is disabled). A changed source or route
    /// drops Tron's native USD quote. Nothing is read until [`Self::set_active_chains`] includes
    /// [`TRON_CHAIN_ID`], which the app does once Tron is selected in the session; Tron's token
    /// anchors are then read at once and refresh with the others.
    pub fn reconcile_tron(&self, settings: &TronSettings, route: Option<&RpcChainRoute>) {
        if self.cache.reconcile_tron_source(settings, route) {
            self.configuration_tx
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }
    }

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
    effective_chains: EffectiveChainRegistry,
    token_registry: EffectiveTokenRegistry,
    http: HttpContext,
) -> TokenAnchorRefreshHandle {
    cache.reconcile_native_sources(&effective_chains, &[]);
    let (configuration_tx, configuration_rx) = watch::channel(0_u64);
    let (wake_tx, wake_rx) = watch::channel(0_u64);
    let task = runtime.spawn(run_token_anchor_refresh_worker(
        Arc::clone(&cache),
        chain_ids,
        effective_chains,
        token_registry,
        http,
        wake_rx,
        configuration_rx,
    ));
    TokenAnchorRefreshHandle {
        wake_tx,
        configuration_tx,
        cache,
        abort_handle: task.abort_handle(),
    }
}

async fn run_token_anchor_refresh_worker(
    cache: Arc<TokenAnchorRateCache>,
    chain_ids: Vec<u64>,
    effective_chains: EffectiveChainRegistry,
    token_registry: EffectiveTokenRegistry,
    http: HttpContext,
    mut wake_rx: watch::Receiver<u64>,
    configuration_rx: watch::Receiver<u64>,
) {
    let mut tron_configuration_rx = configuration_rx.clone();
    let native_worker = native_usd::run_worker(&cache, &http, configuration_rx, wake_rx.clone());
    let token_worker = async {
        let mut tron = cache.active_tron_route();
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
                changed = tron_configuration_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let current = cache.active_tron_route();
                    if current != tron {
                        tron = current;
                        if tron.is_some() {
                            // Without other chains, this reads only Tron's anchors.
                            refresh_token_anchor_rates(&cache, &[], &effective_chains, &token_registry, &http).await;
                            next_refresh = next_refresh.min(Instant::now() + token_anchor_refresh_delay(&cache, &chain_ids, &token_registry));
                        }
                    }
                }
            }
        }
    };
    tokio::select! { () = native_worker => {}, () = token_worker => {} }
}

fn token_anchor_refresh_delay(
    cache: &TokenAnchorRateCache,
    chain_ids: &[u64],
    token_registry: &EffectiveTokenRegistry,
) -> Duration {
    let chain_ids = with_active_tron(chain_ids, cache.active_tron_route().as_ref());
    let missing_token_rate = token_anchor_entries_for_chains(&chain_ids, token_registry)
        .into_iter()
        .any(|entry| cache.cached_rate(entry.chain_id, entry.token).is_none());
    if missing_token_rate {
        TOKEN_ANCHOR_MISSING_RATE_RETRY_INTERVAL
    } else {
        TOKEN_ANCHOR_REFRESH_INTERVAL
    }
}

/// `chain_ids`, with Tron while it has an active route.
fn with_active_tron(chain_ids: &[u64], tron: Option<&RpcChainRoute>) -> Vec<u64> {
    let mut chain_ids = chain_ids.to_vec();
    if tron.is_some() {
        chain_ids.push(TRON_CHAIN_ID);
    }
    chain_ids
}

pub async fn refresh_token_anchor_rates(
    cache: &TokenAnchorRateCache,
    chain_ids: &[u64],
    effective_chains: &EffectiveChainRegistry,
    token_registry: &EffectiveTokenRegistry,
    http: &HttpContext,
) {
    let tron = cache.active_tron_route();
    let chain_ids = &with_active_tron(chain_ids, tron.as_ref());
    tracing::debug!(
        chain_count = chain_ids.len(),
        "token anchor refresh started"
    );
    let entries = token_anchor_entries_for_chains(chain_ids, token_registry);
    let oracle_addresses_by_chain = oracle_addresses_for_entries(&entries);
    let (pool_keys, observation_keys) = twap_keys_for_entries(&entries);
    let tron = tron.as_ref();
    refresh_token_anchor_rates_with_fetch(
        cache,
        &entries,
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
                            tron,
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
                            tron,
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
    }
    store_anchor_rates_from_entries_with_inputs(cache, entries, &oracle_answers, &twap_inputs);
}

async fn fetch_oracle_answers_for_chain(
    chain_id: u64,
    oracle_addresses: &[Address],
    effective_chains: &EffectiveChainRegistry,
    tron: Option<&RpcChainRoute>,
    http: &HttpContext,
) -> Result<BTreeMap<Address, U256>> {
    fetch_oracle_answers_for_chain_with_timeout(
        chain_id,
        oracle_addresses,
        effective_chains,
        tron,
        http,
        TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT,
    )
    .await
}

/// The General route of `chain_id`'s anchor reads. Tron's id always means `tron`, its route
/// while active, and never a registry chain with that id. Reads use `latest`, the only block
/// Tron accepts.
fn anchor_read_route(
    chain_id: u64,
    effective_chains: &EffectiveChainRegistry,
    tron: Option<&RpcChainRoute>,
    request_timeout: Duration,
) -> Result<RpcRoute> {
    let chain_route = if chain_id == TRON_CHAIN_ID {
        tron.cloned()
            .ok_or_else(|| eyre::eyre!("Tron is not active"))?
    } else {
        resolve_effective_chain_rpc_route(
            chain_id,
            effective_chains
                .get(chain_id)
                .ok_or_else(|| eyre::eyre!("chain {chain_id} is not configured"))?,
        )?
    };
    Ok(
        RpcRoute::from(chain_route.for_class(crate::RpcTrafficClass::General)?)
            .with_request_timeout(request_timeout)
            .with_attempt_timeout(Duration::from_secs(5)),
    )
}

async fn fetch_oracle_answers_for_chain_with_timeout(
    chain_id: u64,
    oracle_addresses: &[Address],
    effective_chains: &EffectiveChainRegistry,
    tron: Option<&RpcChainRoute>,
    http: &HttpContext,
    request_timeout: Duration,
) -> Result<BTreeMap<Address, U256>> {
    let route = anchor_read_route(chain_id, effective_chains, tron, request_timeout)?;
    let calls = oracle_addresses
        .iter()
        .map(|&oracle_address| {
            (
                oracle_address,
                AggregatorInterface::latestAnswerCall {}.abi_encode().into(),
            )
        })
        .collect();
    let results = rpc_broker_values::<AggregatorInterface::latestAnswerCall>(
        http,
        &route,
        calls,
        "anchors::chainlink_oracle",
    )
    .await
    .wrap_err("multicall anchor oracle answers")?;
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

/// The anchor sources `entries` configure for `token` on `chain_id`, `None` without any.
fn configured_anchor_sources(
    entries: &[RuntimeTokenAnchorInfo],
    chain_id: u64,
    token: Address,
) -> Option<&[RuntimeTokenAnchorSource]> {
    entries
        .iter()
        .find(|entry| entry.chain_id == chain_id && entry.token == token)
        .map(|entry| entry.anchor_sources.as_slice())
        .filter(|sources| !sources.is_empty())
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
) -> (BTreeSet<PoolKey>, BTreeSet<ObservationKey>) {
    let mut pools = BTreeSet::new();
    let mut observations = BTreeSet::new();
    for entry in entries {
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
    effective_chains: &EffectiveChainRegistry,
    tron: Option<&RpcChainRoute>,
    http: &HttpContext,
) -> Result<TwapFetchedInputs> {
    fetch_twap_inputs_for_chain_with_timeout(
        chain_id,
        pools,
        observations,
        effective_chains,
        tron,
        http,
        TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT,
    )
    .await
}

async fn fetch_twap_inputs_for_chain_with_timeout(
    chain_id: u64,
    pools: &[PoolKey],
    observations: &[ObservationKey],
    effective_chains: &EffectiveChainRegistry,
    tron: Option<&RpcChainRoute>,
    http: &HttpContext,
    request_timeout: Duration,
) -> Result<TwapFetchedInputs> {
    if pools.is_empty() && observations.is_empty() {
        return Ok(TwapFetchedInputs::default());
    }
    let route = anchor_read_route(chain_id, effective_chains, tron, request_timeout)?;
    fetch_twap_inputs_at(
        http,
        &route,
        BlockId::latest(),
        WalletRpcOrigin::Anchors,
        pools,
        observations,
    )
    .await
}

/// Reads pool metadata and observations at `block`. Pools or observations that fail to read or
/// decode are omitted from the result.
async fn fetch_twap_inputs_at(
    http: &HttpContext,
    route: &RpcRoute,
    block: BlockId,
    origin: WalletRpcOrigin,
    pools: &[PoolKey],
    observations: &[ObservationKey],
) -> Result<TwapFetchedInputs> {
    let metadata_calls = pools
        .iter()
        .flat_map(|pool| {
            [
                (
                    pool.pool,
                    UniswapV3PoolInterface::token0Call {}.abi_encode().into(),
                ),
                (
                    pool.pool,
                    UniswapV3PoolInterface::token1Call {}.abi_encode().into(),
                ),
            ]
        })
        .collect();
    let observation_calls = observations
        .iter()
        .map(|observation| {
            (
                observation.pool,
                UniswapV3PoolInterface::observeCall {
                    secondsAgos: vec![observation.window_seconds, 0],
                }
                .abi_encode()
                .into(),
            )
        })
        .collect();
    let metadata_results = rpc_broker_values_at::<UniswapV3PoolInterface::token0Call>(
        http,
        route,
        block,
        origin,
        metadata_calls,
        "anchors::uniswap_v3_twap.metadata",
    )
    .await?;
    let observation_results = rpc_broker_values_at::<UniswapV3PoolInterface::observeCall>(
        http,
        route,
        block,
        origin,
        observation_calls,
        "anchors::uniswap_v3_twap.observations",
    )
    .await?;
    let mut fetched = TwapFetchedInputs::default();
    for (pair, key) in metadata_results
        .as_chunks::<2>()
        .0
        .iter()
        .zip(pools.iter().copied())
    {
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

async fn rpc_broker_values<C: SolCall + 'static>(
    http: &HttpContext,
    route: &RpcRoute,
    calls: Vec<(Address, Bytes)>,
    caller: &'static str,
) -> Result<Vec<std::result::Result<C::Return, RpcBrokerError>>> {
    rpc_broker_values_at::<C>(
        http,
        route,
        BlockId::latest(),
        WalletRpcOrigin::Anchors,
        calls,
        caller,
    )
    .await
}

async fn rpc_broker_values_at<C: SolCall + 'static>(
    http: &HttpContext,
    route: &RpcRoute,
    block: BlockId,
    origin: WalletRpcOrigin,
    calls: Vec<(Address, Bytes)>,
    caller: &'static str,
) -> Result<Vec<std::result::Result<C::Return, RpcBrokerError>>> {
    let results = http
        .rpc_broker()
        .submit_calls_decoded_at::<C>(route.clone(), calls, block, origin.into())
        .await?;
    if let Some(error) = total_failure(&results) {
        return Err(
            eyre::Report::new(error.clone()).wrap_err(format!("{caller} RPC request failed"))
        );
    }
    Ok(results)
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
mod tests;
