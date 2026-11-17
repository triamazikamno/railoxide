use super::{
    AggregatorInterface, TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT, TOKEN_ANCHOR_REFRESH_INTERVAL,
    TOKEN_ANCHOR_WAKE_REFRESH_MIN_INTERVAL, TokenAnchorRateCache,
};
use crate::settings::EffectiveChainConfig;
use crate::settings::{EffectiveChainRegistry, TronSettings, resolve_effective_chain_rpc_route};
use crate::tron::TRON_CHAIN_ID;
use crate::{HttpContext, RpcChainRoute, RpcRoute, WalletRpcOrigin};
use alloy::{
    primitives::{Address, U256},
    sol_types::SolCall,
};
use eyre::Result;
use futures_util::stream::{FuturesUnordered, StreamExt};
use railgun_ui::chain_editor::{NativeUsdQuote, NativeUsdState, NativeUsdStatus};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::watch,
    time::{Instant, sleep_until},
};

#[derive(Debug, Default)]
pub(super) struct NativePricing {
    next_generation: u64,
    chains: BTreeMap<u64, NativeChain>,
    active_chains: BTreeSet<u64>,
    pub(super) rates: BTreeMap<u64, (U256, u8)>,
}

#[derive(Debug)]
struct NativeChain {
    source: NativeSource,
    generation: u64,
    status: NativeUsdStatus,
}

/// What native USD pricing needs from a chain, Tron included: its feed, precision and route.
#[derive(Debug, Clone)]
struct NativeSource {
    chain_id: u64,
    enabled: bool,
    native_usd_oracle: Option<Address>,
    native_decimals: u8,
    /// `None` without a usable route.
    route: Option<RpcChainRoute>,
}

impl NativeSource {
    fn chain(chain: &EffectiveChainConfig) -> Self {
        Self {
            chain_id: chain.chain_id,
            enabled: chain.enabled,
            native_usd_oracle: chain.native_usd_oracle,
            native_decimals: chain.native_currency.decimals,
            route: resolve_effective_chain_rpc_route(chain.chain_id, chain).ok(),
        }
    }

    /// Tron's source, read over `route`; `None` while Tron is disabled.
    fn tron(settings: &TronSettings, route: Option<&RpcChainRoute>) -> Self {
        Self {
            chain_id: TRON_CHAIN_ID,
            enabled: settings.enabled && route.is_some(),
            native_usd_oracle: settings.native_usd_oracle(),
            native_decimals: railgun_ui::tron_native_currency().decimals,
            route: route.cloned(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct NativeRead {
    chain: NativeSource,
    generation: u64,
}

impl NativePricing {
    /// Installs `source`. A changed feed, enablement or route retires its quote and pending
    /// work; a changed precision revalues the quote it keeps.
    fn reconcile(&mut self, source: NativeSource, route_changed: bool) -> bool {
        let id = source.chain_id;
        let previous = self.chains.get(&id);
        let source_changed = previous.is_none_or(|old| {
            old.source.native_usd_oracle != source.native_usd_oracle
                || old.source.enabled != source.enabled
        });
        let precision_changed =
            previous.is_some_and(|old| old.source.native_decimals != source.native_decimals);
        if !source_changed && !route_changed && !precision_changed {
            return false;
        }
        let mut status = previous.map_or_else(NativeUsdStatus::default, |old| old.status.clone());
        if source_changed || route_changed {
            self.rates.remove(&id);
            status.quote = None;
        }
        if let Some((_, decimals)) = self.rates.get_mut(&id) {
            *decimals = source.native_decimals;
        }
        status.state = if source.enabled && source.native_usd_oracle.is_some() {
            NativeUsdState::Pending
        } else {
            NativeUsdState::Unconfigured
        };
        status.reason = None;
        self.next_generation += 1;
        let generation = self.next_generation;
        self.chains.insert(
            id,
            NativeChain {
                source,
                generation,
                status,
            },
        );
        true
    }
}

impl TokenAnchorRateCache {
    pub(super) fn set_active_native_chains(&self, chain_ids: &[u64]) -> bool {
        let Ok(mut native) = self.native_pricing.write() else {
            return false;
        };
        let active_chains: BTreeSet<_> = chain_ids.iter().copied().collect();
        if native.active_chains == active_chains {
            return false;
        }
        let retired: Vec<_> = native
            .active_chains
            .difference(&active_chains)
            .copied()
            .collect();
        // Retire pending publication while retaining the last usable quote.
        for id in retired {
            native.next_generation += 1;
            let generation = native.next_generation;
            if let Some(chain) = native.chains.get_mut(&id) {
                chain.generation = generation;
            }
        }
        native.active_chains = active_chains;
        true
    }

    /// Retires native work and quotes atomically with publication admission.
    pub(super) fn reconcile_native_sources(
        &self,
        chains: &EffectiveChainRegistry,
        operational_changes: &[u64],
    ) -> bool {
        let Ok(mut native) = self.native_pricing.write() else {
            return false;
        };
        let mut changed = false;
        // Tron's id always means Tron, which has its own reconciliation, and never a registry
        // chain with that id.
        let removed: Vec<_> = native
            .chains
            .keys()
            .copied()
            .filter(|&id| id != TRON_CHAIN_ID && chains.get(id).is_none())
            .collect();
        for id in removed {
            native.chains.remove(&id);
            native.rates.remove(&id);
            changed = true;
        }
        for (&id, config) in chains.iter().filter(|&(&id, _)| id != TRON_CHAIN_ID) {
            changed |= native.reconcile(
                NativeSource::chain(config),
                operational_changes.contains(&id),
            );
        }
        drop(native);
        if changed {
            self.notify_refreshed();
        }
        changed
    }

    /// Installs Tron's source from its settings and route, as `reconcile_native_sources` does
    /// for registry chains.
    pub(super) fn reconcile_tron_source(
        &self,
        settings: &TronSettings,
        route: Option<&RpcChainRoute>,
    ) -> bool {
        let Ok(mut native) = self.native_pricing.write() else {
            return false;
        };
        let source = NativeSource::tron(settings, route);
        let route_changed = native
            .chains
            .get(&TRON_CHAIN_ID)
            .is_some_and(|old| old.source.route != source.route);
        let changed = native.reconcile(source, route_changed);
        drop(native);
        if changed {
            self.notify_refreshed();
        }
        changed
    }

    /// Tron's route while Tron is enabled and active, for its token anchor reads.
    pub(super) fn active_tron_route(&self) -> Option<RpcChainRoute> {
        let native = self.native_pricing.read().ok()?;
        if !native.active_chains.contains(&TRON_CHAIN_ID) {
            return None;
        }
        native
            .chains
            .get(&TRON_CHAIN_ID)
            .filter(|chain| chain.source.enabled)?
            .source
            .route
            .clone()
    }

    fn native_reads(&self) -> Vec<NativeRead> {
        self.native_pricing
            .read()
            .ok()
            .map(|native| {
                native
                    .chains
                    .values()
                    .filter(|entry| {
                        native.active_chains.contains(&entry.source.chain_id)
                            && entry.source.enabled
                            && entry.source.native_usd_oracle.is_some()
                    })
                    .map(|entry| NativeRead {
                        chain: entry.source.clone(),
                        generation: entry.generation,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[must_use]
    pub fn native_usd_statuses(&self) -> BTreeMap<String, NativeUsdStatus> {
        self.native_pricing
            .read()
            .ok()
            .map(|native| {
                native
                    .chains
                    .iter()
                    .map(|(id, entry)| (id.to_string(), entry.status.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn publish_native(&self, read: &NativeRead, result: Result<(U256, u8), String>) {
        let Ok(mut native) = self.native_pricing.write() else {
            return;
        };
        let Some(current) = native
            .chains
            .get_mut(&read.chain.chain_id)
            .filter(|entry| entry.generation == read.generation)
        else {
            return;
        };
        match result {
            Ok((rate, decimals)) => {
                current.status.state = NativeUsdState::Available;
                current.status.reason = None;
                current.status.quote = Some(NativeUsdQuote::new(
                    rate.to_string(),
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    decimals,
                ));
                native
                    .rates
                    .insert(read.chain.chain_id, (rate, read.chain.native_decimals));
            }
            Err(reason) => {
                current.status.state = NativeUsdState::Failed;
                current.status.reason = Some(reason);
            }
        }
        drop(native);
        self.notify_refreshed();
    }
}

/// Reads a draft's resolved source once for the editor's Test action. Nothing is cached,
/// published or persisted, and failures stay endpoint-free.
pub async fn probe_native_usd_quote(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
) -> Result<NativeUsdQuote, String> {
    let (rate, decimals) = fetch_native_quote(&NativeSource::chain(chain), http).await?;
    Ok(NativeUsdQuote::new(
        rate.to_string(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        decimals,
    ))
}

/// Reads at `latest`, the only block Tron accepts.
async fn fetch_native_quote(
    chain: &NativeSource,
    http: &HttpContext,
) -> Result<(U256, u8), String> {
    let address = chain
        .native_usd_oracle
        .ok_or_else(|| "This chain has no USD price source".to_owned())?;
    let route = chain
        .route
        .as_ref()
        .ok_or_else(|| "Check the chain's RPC configuration".to_owned())?
        .for_class(crate::RpcTrafficClass::General)
        .map_err(|error| error.to_string())?;
    let route = RpcRoute::from(route)
        .with_request_timeout(TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT)
        .with_attempt_timeout(Duration::from_secs(5));
    let results = http
        .rpc_broker()
        .submit_eth_calls(
            route,
            vec![
                (
                    address,
                    AggregatorInterface::latestAnswerCall {}.abi_encode().into(),
                ),
                (
                    address,
                    AggregatorInterface::decimalsCall {}.abi_encode().into(),
                ),
            ],
            WalletRpcOrigin::Anchors.into(),
        )
        .await
        // Broker errors name the failure without the endpoint URL.
        .map_err(|error| format!("Could not reach the oracle: {error}."))?;
    let mut results = results.into_iter();
    let answer = results
        .next()
        .expect("broker preserves call count")
        .map_err(|error| format!("latestAnswer() failed: {error}."))?;
    let decimals = results
        .next()
        .expect("broker preserves call count")
        .map_err(|error| format!("decimals() failed: {error}."))?;
    decode_native_quote(chain.chain_id, &answer, &decimals)
}

/// Names the exact reason a reply cannot become a quote. A call to an address
/// with no code returns empty data, which is the most common mistake.
fn decode_native_quote(
    chain_id: u64,
    answer: &[u8],
    decimals: &[u8],
) -> Result<(U256, u8), String> {
    if answer.is_empty() && decimals.is_empty() {
        return Err(format!(
            "No contract answered at this address on chain {chain_id}. Check the address and that the feed is deployed on this chain."
        ));
    }
    let answer = AggregatorInterface::latestAnswerCall::abi_decode_returns_validate(answer)
        .map_err(|_| {
            format!(
                "latestAnswer() returned {} bytes that do not decode as int256.",
                answer.len()
            )
        })?;
    let decimals = AggregatorInterface::decimalsCall::abi_decode_returns_validate(decimals)
        .map_err(|_| {
            format!(
                "decimals() returned {} bytes that do not decode as uint8.",
                decimals.len()
            )
        })?;
    if answer.is_negative() {
        return Err(format!("The feed reported a negative price ({answer})."));
    }
    if answer.is_zero() {
        return Err("The feed reported a price of 0.".to_owned());
    }
    let raw =
        U256::try_from(answer).map_err(|_| format!("The feed's price does not fit ({answer})."))?;
    let scale = |exponent: u8| U256::from(10).checked_pow(U256::from(exponent));
    let rate = if decimals > 6 {
        scale(decimals - 6).and_then(|scale| raw.checked_div(scale))
    } else {
        scale(6 - decimals).and_then(|scale| raw.checked_mul(scale))
    }
    .ok_or_else(|| format!("The feed's price does not fit ({raw} with {decimals} decimals)."))?;
    if rate.is_zero() {
        return Err(format!(
            "The feed's price rounds to $0 ({raw} with {decimals} decimals). Check that it quotes USD per native coin."
        ));
    }
    Ok((rate, decimals))
}

pub(super) async fn run_worker(
    cache: &TokenAnchorRateCache,
    http: &HttpContext,
    configuration_rx: watch::Receiver<u64>,
    wake_rx: watch::Receiver<u64>,
) {
    run_worker_with(cache, configuration_rx, wake_rx, |chain| async move {
        fetch_native_quote(&chain, http).await
    })
    .await;
}

async fn run_worker_with<F, R>(
    cache: &TokenAnchorRateCache,
    mut configuration_rx: watch::Receiver<u64>,
    mut wake_rx: watch::Receiver<u64>,
    fetch: F,
) where
    F: Fn(NativeSource) -> R,
    R: Future<Output = Result<(U256, u8), String>>,
{
    let mut pending = FuturesUnordered::new();
    let mut active = BTreeSet::new();
    let mut scheduled = BTreeSet::new();
    let mut retries = BTreeMap::<_, (Instant, Duration)>::new();
    let mut periodic = true;
    let mut next_refresh = Instant::now();
    let mut last_refresh = Instant::now();
    loop {
        let reads = cache.native_reads();
        let current: BTreeSet<_> = reads
            .iter()
            .map(|read| (read.chain.chain_id, read.generation))
            .collect();
        // Retired generations need no scheduling history. Their completions still face admission.
        scheduled.retain(|key| current.contains(key));
        retries.retain(|key, _| current.contains(key));
        for read in reads {
            let key = (read.chain.chain_id, read.generation);
            let due = retries
                .get(&key)
                .map_or(periodic, |(deadline, _)| *deadline <= Instant::now());
            if !active.contains(&key) && (due || !scheduled.contains(&key)) {
                active.insert(key);
                scheduled.insert(key);
                let request = fetch(read.chain.clone());
                pending.push(async move {
                    let result = request.await;
                    let failed = result.is_err();
                    cache.publish_native(&read, result);
                    (key, failed)
                });
            }
        }
        if periodic {
            last_refresh = Instant::now();
            next_refresh = last_refresh + TOKEN_ANCHOR_REFRESH_INTERVAL;
        }
        periodic = false;
        let next_retry = retries
            .iter()
            .filter(|(key, _)| !active.contains(*key))
            .map(|(_, (deadline, _))| *deadline)
            .min()
            .unwrap_or(next_refresh);
        tokio::select! {
            Some((key, failed)) = pending.next(), if !pending.is_empty() => {
                active.remove(&key);
                if cache.native_reads().iter().any(|read| (read.chain.chain_id, read.generation) == key) {
                    if failed {
                        let delay = retries.get(&key).map_or(Duration::from_secs(5), |(_, delay)| {
                            (*delay * 2).min(Duration::from_secs(30))
                        });
                        retries.insert(key, (Instant::now() + delay, delay));
                        tracing::debug!(
                            chain_id = key.0,
                            retry_in_secs = delay.as_secs(),
                            "scheduling native USD oracle retry"
                        );
                    } else {
                        retries.remove(&key);
                    }
                }
            }
            changed = configuration_rx.changed() => { if changed.is_err() { break; } }
            changed = wake_rx.changed() => {
                if changed.is_err() { break; }
                periodic = last_refresh.elapsed() >= TOKEN_ANCHOR_WAKE_REFRESH_MIN_INTERVAL;
            }
            () = sleep_until(next_refresh.min(next_retry)) => {
                periodic = Instant::now() >= next_refresh;
            }
        }
    }
}

#[cfg(test)]
mod tests;
