use super::{
    AggregatorInterface, TOKEN_ANCHOR_ORACLE_REQUEST_TIMEOUT, TOKEN_ANCHOR_REFRESH_INTERVAL,
    TOKEN_ANCHOR_WAKE_REFRESH_MIN_INTERVAL, TokenAnchorRateCache,
};
use crate::settings::EffectiveChainConfig;
use crate::settings::{EffectiveChainRegistry, resolve_effective_chain_rpc_route};
use crate::{HttpContext, RpcRoute, WalletRpcOrigin};
use alloy::{primitives::U256, sol_types::SolCall};
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
    config: EffectiveChainConfig,
    generation: u64,
    status: NativeUsdStatus,
}

#[derive(Debug, Clone)]
pub(super) struct NativeRead {
    chain: EffectiveChainConfig,
    generation: u64,
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
        let removed: Vec<_> = native
            .chains
            .keys()
            .copied()
            .filter(|id| chains.get(*id).is_none())
            .collect();
        for id in removed {
            native.chains.remove(&id);
            native.rates.remove(&id);
            changed = true;
        }
        for (&id, config) in chains.iter() {
            let previous = native.chains.get(&id);
            let source_changed = previous.is_none_or(|old| {
                old.config.native_usd_oracle != config.native_usd_oracle
                    || old.config.enabled != config.enabled
            });
            let route_changed = operational_changes.contains(&id);
            let precision_changed = previous.is_some_and(|old| {
                old.config.native_currency.decimals != config.native_currency.decimals
            });
            if !source_changed && !route_changed && !precision_changed {
                continue;
            }
            let mut status =
                previous.map_or_else(NativeUsdStatus::default, |old| old.status.clone());
            if source_changed || route_changed {
                native.rates.remove(&id);
                status.quote = None;
            }
            if let Some((_, decimals)) = native.rates.get_mut(&id) {
                *decimals = config.native_currency.decimals;
            }
            status.state = if config.enabled && config.native_usd_oracle.is_some() {
                NativeUsdState::Pending
            } else {
                NativeUsdState::Unconfigured
            };
            status.reason = None;
            native.next_generation += 1;
            let generation = native.next_generation;
            native.chains.insert(
                id,
                NativeChain {
                    config: config.clone(),
                    generation,
                    status,
                },
            );
            changed = true;
        }
        drop(native);
        if changed {
            self.notify_refreshed();
        }
        changed
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
                        native.active_chains.contains(&entry.config.chain_id)
                            && entry.config.enabled
                            && entry.config.native_usd_oracle.is_some()
                    })
                    .map(|entry| NativeRead {
                        chain: entry.config.clone(),
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
                native.rates.insert(
                    read.chain.chain_id,
                    (rate, read.chain.native_currency.decimals),
                );
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
    let (rate, decimals) = fetch_native_quote(chain, http).await?;
    Ok(NativeUsdQuote::new(
        rate.to_string(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        decimals,
    ))
}

async fn fetch_native_quote(
    chain: &EffectiveChainConfig,
    http: &HttpContext,
) -> Result<(U256, u8), String> {
    let address = chain
        .native_usd_oracle
        .ok_or_else(|| "This chain has no USD price source".to_owned())?;
    let route = resolve_effective_chain_rpc_route(chain.chain_id, chain)
        .map_err(|_| "Check the chain's RPC configuration".to_owned())?
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
    F: Fn(EffectiveChainConfig) -> R,
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
mod tests {
    use super::*;
    use crate::settings::{NativeUsdPricing, WalletSettings, build_effective_chain_configs};
    use alloy::primitives::{Address, I256, uint};
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn failed_sources_back_off_independently_and_recovery_resets_retries() {
        use futures_util::FutureExt;
        use std::cell::RefCell;

        let settings = WalletSettings::default();
        let configs = build_effective_chain_configs(&settings).unwrap();
        let configs: EffectiveChainRegistry = configs
            .iter()
            .filter(|(id, _)| matches!(**id, 1 | 56))
            .map(|(_, chain)| chain.clone())
            .collect();
        let cache = TokenAnchorRateCache::new();
        cache.reconcile_native_sources(&configs, &[]);
        cache.set_active_native_chains(&[1, 56]);
        let attempts = RefCell::new(BTreeMap::<u64, usize>::new());
        let (_configuration_tx, configuration_rx) = watch::channel(0);
        let (_wake_tx, wake_rx) = watch::channel(0);
        let worker = run_worker_with(&cache, configuration_rx, wake_rx, |chain| {
            let mut attempts = attempts.borrow_mut();
            let attempt = attempts.entry(chain.chain_id).or_default();
            *attempt += 1;
            std::future::ready(if chain.chain_id == 1 && (*attempt <= 5 || *attempt == 7) {
                Err("Oracle unavailable".to_owned())
            } else {
                Ok((U256::from(3_000_000_000_u64), 8))
            })
        });
        tokio::pin!(worker);
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(attempts.borrow()[&1], 1);
        assert_eq!(attempts.borrow()[&56], 1);
        assert_eq!(cache.cached_native_usd_rate(1), None);

        // Each deadline belongs only to the failing source, and sustained failure caps at 30s.
        for (delay, expected_attempts) in [(5, 2), (10, 3), (20, 4), (30, 5), (30, 6)] {
            tokio::time::advance(Duration::from_secs(delay - 1)).await;
            assert!(worker.as_mut().now_or_never().is_none());
            assert_eq!(attempts.borrow()[&1], expected_attempts - 1);
            tokio::time::advance(Duration::from_secs(1)).await;
            assert!(worker.as_mut().now_or_never().is_none());
            assert_eq!(attempts.borrow()[&1], expected_attempts);
            assert_eq!(attempts.borrow()[&56], 1);
        }
        assert_eq!(
            cache.native_usd_statuses()["1"].state,
            NativeUsdState::Available
        );
        let recovered = cache.cached_native_usd_rate(1).unwrap();

        // Recovery restores the normal refresh cadence; its next failure retains the quote.
        tokio::time::advance(Duration::from_secs(204)).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(attempts.borrow()[&1], 6);
        assert_eq!(attempts.borrow()[&56], 1);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(attempts.borrow()[&1], 7);
        assert_eq!(attempts.borrow()[&56], 2);
        assert_eq!(
            cache.native_usd_statuses()["1"].state,
            NativeUsdState::Failed
        );
        assert_eq!(cache.cached_native_usd_rate(1), Some(recovered));
        assert!(cache.native_usd_statuses()["1"].quote.is_some());

        // A new failure after success starts again at five seconds, even with a cached quote.
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(attempts.borrow()[&1], 7);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(attempts.borrow()[&1], 8);
        assert_eq!(attempts.borrow()[&56], 2);
        assert_eq!(
            cache.native_usd_statuses()["1"].state,
            NativeUsdState::Available
        );
    }

    #[tokio::test(start_paused = true)]
    async fn native_worker_waits_for_activation_and_fences_retired_reads() {
        use futures_util::FutureExt;
        use std::cell::RefCell;

        let mut chain = build_effective_chain_configs(&WalletSettings::default())
            .unwrap()
            .get(1)
            .unwrap()
            .clone();
        chain.chain_id = 31337;
        chain.railgun = None;
        let mut configs: EffectiveChainRegistry = std::iter::once(chain).collect();
        let cache = TokenAnchorRateCache::new();
        cache.reconcile_native_sources(&configs, &[]);
        let requests = RefCell::new(Vec::new());
        let (configuration_tx, configuration_rx) = watch::channel(0_u64);
        let (_wake_tx, wake_rx) = watch::channel(0);
        let set_active = |ids: &[u64]| {
            if cache.set_active_native_chains(ids) {
                configuration_tx.send_modify(|revision| *revision += 1);
            }
        };
        let worker = run_worker_with(&cache, configuration_rx, wake_rx, |chain| {
            let (send, receive) = tokio::sync::oneshot::channel();
            requests.borrow_mut().push((chain.native_usd_oracle, send));
            async move { receive.await.unwrap() }
        });
        tokio::pin!(worker);

        assert!(worker.as_mut().now_or_never().is_none());
        assert!(requests.borrow().is_empty());
        assert_eq!(
            cache.native_usd_statuses()["31337"].state,
            NativeUsdState::Pending
        );
        tokio::time::advance(TOKEN_ANCHOR_REFRESH_INTERVAL).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert!(requests.borrow().is_empty());
        set_active(&[31337]);
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(requests.borrow().len(), 1);
        set_active(&[31337, 31337]);
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(requests.borrow().len(), 1);
        requests
            .borrow_mut()
            .pop()
            .unwrap()
            .1
            .send(Ok((U256::from(2_000_000), 8)))
            .unwrap();
        assert!(worker.as_mut().now_or_never().is_none());
        let published = cache.native_usd_statuses();
        assert_eq!(
            cache.cached_native_usd_rate(31337),
            Some(U256::from(2_000_000))
        );
        set_active(&[31337]);
        assert!(worker.as_mut().now_or_never().is_none());
        assert!(requests.borrow().is_empty());

        tokio::time::advance(TOKEN_ANCHOR_REFRESH_INTERVAL).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(requests.borrow().len(), 1);
        set_active(&[]);
        assert!(worker.as_mut().now_or_never().is_none());
        requests
            .borrow_mut()
            .pop()
            .unwrap()
            .1
            .send(Ok((U256::MAX, 8)))
            .unwrap();
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(cache.native_usd_statuses(), published);
        assert_eq!(
            cache.cached_native_usd_rate(31337),
            Some(U256::from(2_000_000))
        );

        let replacement_source = Address::repeat_byte(7);
        configs.get_mut(31337).unwrap().native_usd_oracle = Some(replacement_source);
        assert!(cache.reconcile_native_sources(&configs, &[]));
        configuration_tx.send_modify(|revision| *revision += 1);
        assert_eq!(cache.cached_native_usd_rate(31337), None);
        assert!(worker.as_mut().now_or_never().is_none());
        tokio::time::advance(TOKEN_ANCHOR_REFRESH_INTERVAL).await;
        assert!(worker.as_mut().now_or_never().is_none());
        assert!(requests.borrow().is_empty());

        set_active(&[31337]);
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(
            requests.borrow().len(),
            1,
            "reactivation refreshes immediately"
        );
        let (source, send) = requests.borrow_mut().pop().unwrap();
        assert_eq!(source, Some(replacement_source));
        send.send(Ok((U256::from(3_000_000), 8))).unwrap();
        assert!(worker.as_mut().now_or_never().is_none());
        assert_eq!(
            cache.cached_native_usd_rate(31337),
            Some(U256::from(3_000_000))
        );
    }

    #[tokio::test]
    async fn native_worker_batches_metadata_and_refreshes_saved_sources_without_token_work() {
        use crate::rpc_broker::tests::{RpcResponder, spawn_rpc_mock};
        use alloy::primitives::Bytes;
        use alloy::providers::bindings::IMulticall3;
        use serde_json::json;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let multicall = Address::repeat_byte(9);
        let batches = Arc::new(AtomicUsize::new(0));
        let responder: RpcResponder = {
            let batches = batches.clone();
            Arc::new(move |request| {
                assert_eq!(request["method"], "eth_call");
                assert_eq!(request["params"][0]["to"], multicall.to_string());
                let call = &request["params"][0];
                let bytes = call
                    .get("input")
                    .or_else(|| call.get("data"))
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .parse::<Bytes>()
                    .unwrap();
                let calls = IMulticall3::tryAggregateCall::abi_decode(&bytes)
                    .unwrap()
                    .calls;
                assert_eq!(calls.len(), 2, "price and precision share a multicall");
                assert_eq!(calls[0].target, calls[1].target);
                let returns = calls
                    .iter()
                    .map(|call| {
                        let data = if call.callData.as_ref()
                            == AggregatorInterface::latestAnswerCall::SELECTOR
                        {
                            AggregatorInterface::latestAnswerCall::abi_encode_returns(
                                &I256::try_from(200_000_000).unwrap(),
                            )
                        } else {
                            assert_eq!(
                                call.callData.as_ref(),
                                AggregatorInterface::decimalsCall::SELECTOR
                            );
                            AggregatorInterface::decimalsCall::abi_encode_returns(&8)
                        };
                        IMulticall3::Result {
                            success: true,
                            returnData: data.into(),
                        }
                    })
                    .collect::<Vec<_>>();
                batches.fetch_add(1, Ordering::SeqCst);
                json!({"jsonrpc": "2.0", "id": request["id"], "result": Bytes::from(IMulticall3::tryAggregateCall::abi_encode_returns(&returns))})
            })
        };
        let (endpoint, server) = spawn_rpc_mock(
            responder,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        let settings = WalletSettings::default();
        let mut chain = build_effective_chain_configs(&settings)
            .unwrap()
            .get(1)
            .unwrap()
            .clone();
        chain.chain_id = 31337;
        chain.native_currency.decimals = 6;
        chain.railgun = None;
        chain.wrapped_native_token = None;
        chain.rpc_route =
            crate::RpcChainRoute::new(31337, vec![endpoint]).with_multicall(multicall);
        let mut configs: EffectiveChainRegistry = std::iter::once(chain).collect();
        let http = crate::build_wallet_network_context(crate::WalletNetworkConfig {
            network_mode: Some(crate::WalletNetworkMode::Direct),
            proxy: None,
            data_dir: &std::env::temp_dir(),
        })
        .await
        .unwrap();
        let cache = Arc::new(TokenAnchorRateCache::new());
        let mut updates = cache.subscribe_refreshes();
        let probe_http = http.clone();
        let worker = super::super::spawn_token_anchor_refresh_worker(
            &tokio::runtime::Handle::current(),
            cache.clone(),
            vec![],
            configs.clone(),
            crate::settings::build_effective_token_registry(&settings).unwrap(),
            http,
        );
        worker.set_active_chains(&[31337]);
        for replacement in [None, Some(Address::repeat_byte(7))] {
            if let Some(address) = replacement {
                configs.get_mut(31337).unwrap().native_usd_oracle = Some(address);
                worker.reconcile_native_sources(&configs, &[]);
                assert_eq!(cache.cached_native_usd_rate(31337), None);
            }
            tokio::time::timeout(Duration::from_secs(3), async {
                while cache.cached_native_usd_rate(31337).is_none() {
                    updates.changed().await.unwrap();
                }
            })
            .await
            .expect("startup and replacement refresh promptly");
            assert_eq!(
                cache.cached_native_usd_micro_value(31337, U256::from(1_500_000)),
                Some(U256::from(3_000_000))
            );
        }
        assert_eq!(batches.load(Ordering::SeqCst), 2);
        // A Test reads the same source through the broker and publishes nothing.
        worker.set_active_chains(&[]);
        let published = cache.native_usd_statuses();
        let quote = probe_native_usd_quote(configs.get(31337).unwrap(), &probe_http)
            .await
            .unwrap();
        assert_eq!(quote.micro_usd, "2000000");
        assert_eq!(quote.feed_decimals, 8);
        assert_eq!(cache.native_usd_statuses(), published);
        configs.get_mut(31337).unwrap().enabled = false;
        worker.reconcile_native_sources(&configs, &[]);
        assert_eq!(cache.cached_native_usd_rate(31337), None);
        assert!(cache.native_reads().is_empty());
        drop(worker);
        server.abort();
    }

    #[test]
    fn native_quote_decoding_discovers_precision_and_rejects_unusable_answers() {
        let decode = |answer: I256, decimals: u8| {
            decode_native_quote(
                1,
                &AggregatorInterface::latestAnswerCall::abi_encode_returns(&answer),
                &AggregatorInterface::decimalsCall::abi_encode_returns(&decimals),
            )
        };
        for decimals in [0, 6, 8, 18] {
            let answer = U256::from(3000) * U256::from(10).pow(U256::from(decimals));
            assert_eq!(
                decode(I256::try_from(answer).unwrap(), decimals),
                Ok((uint!(3_000_000_000_U256), decimals))
            );
        }
        for (answer, decimals, expected) in [
            (I256::ZERO, 8, "price of 0"),
            (I256::MINUS_ONE, 8, "negative price"),
            (I256::ONE, 8, "rounds to $0"),
            (I256::MAX, 0, "does not fit"),
            (I256::MAX, 255, "does not fit"),
        ] {
            let error = decode(answer, decimals).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
        assert!(
            decode_native_quote(8453, &[], &[])
                .unwrap_err()
                .contains("No contract answered at this address on chain 8453")
        );
        assert!(
            decode_native_quote(1, &[0], &[8])
                .unwrap_err()
                .contains("do not decode as int256")
        );
        let answer = AggregatorInterface::latestAnswerCall::abi_encode_returns(
            &I256::try_from(3000).unwrap(),
        );
        assert!(
            decode_native_quote(1, &answer, &[])
                .unwrap_err()
                .contains("do not decode as uint8")
        );
    }

    #[test]
    fn public_only_precision_changes_revalue_amounts_without_losing_quotes() {
        let cache = TokenAnchorRateCache::new();
        let mut settings = WalletSettings::default();
        let definition = crate::settings::CustomChainSettings {
            native_usd_pricing: NativeUsdPricing::Oracle {
                contract_address: Address::repeat_byte(7),
            },
            name: "Public EVM".into(),
            native_currency: crate::settings::NativeCurrency {
                name: "Coin".into(),
                symbol: "COIN".into(),
                decimals: 6,
            },
            rpc_endpoints: vec!["https://rpc.example".to_owned().into()],
            explorer_urls: vec![],
            enabled: true,
            contracts: crate::settings::ChainContractSettings::default(),
            finality_depth: None,
            gas: crate::settings::ChainGasSettings::default(),
        };
        settings.chains.custom.insert(31337, definition);
        let mut configs = build_effective_chain_configs(&settings).unwrap();
        cache.reconcile_native_sources(&configs, &[]);
        cache.set_active_native_chains(&[31337]);
        let read = cache
            .native_reads()
            .into_iter()
            .find(|read| read.chain.chain_id == 31337)
            .unwrap();
        assert!(read.chain.railgun.is_none());
        assert!(read.chain.wrapped_native_token.is_none());
        cache.publish_native(&read, Ok((U256::from(2_000_000), 8)));
        assert_eq!(
            cache.cached_native_usd_micro_value(31337, U256::from(1_500_000)),
            Some(U256::from(3_000_000))
        );
        settings
            .chains
            .custom
            .get_mut(&31337)
            .unwrap()
            .native_currency
            .decimals = 3;
        configs = build_effective_chain_configs(&settings).unwrap();
        cache.reconcile_native_sources(&configs, &[]);
        assert_eq!(
            cache.cached_native_usd_micro_value(31337, U256::from(1500)),
            Some(U256::from(3_000_000))
        );
        cache.publish_native(&read, Err("Old precision".to_owned()));
        assert_eq!(
            cache.native_usd_statuses()["31337"].state,
            NativeUsdState::Pending
        );
        let current = cache
            .native_reads()
            .into_iter()
            .find(|read| read.chain.chain_id == 31337)
            .unwrap();
        settings.chains.custom.remove(&31337);
        cache
            .reconcile_native_sources(&build_effective_chain_configs(&settings).unwrap(), &[31337]);
        cache.publish_native(&current, Ok((U256::MAX, 77)));
        assert_eq!(cache.cached_native_usd_rate(31337), None);
        assert!(!cache.native_usd_statuses().contains_key("31337"));
    }

    #[tokio::test]
    async fn native_source_changes_fence_late_results_and_preserve_unrelated_quotes() {
        let cache = Arc::new(TokenAnchorRateCache::new());
        let mut settings = WalletSettings::default();
        let mut configs = build_effective_chain_configs(&settings).unwrap();
        cache.reconcile_native_sources(&configs, &[]);
        cache.set_active_native_chains(&[1, 56]);
        let read = |id| {
            cache
                .native_reads()
                .into_iter()
                .find(|read| read.chain.chain_id == id)
                .unwrap()
        };
        let original = read(1);
        cache.publish_native(&original, Ok((U256::from(3_000_000_000_u64), 8)));
        cache.publish_native(&read(56), Ok((U256::from(500_000_000), 8)));
        cache.publish_native(&original, Err("Connection unavailable".to_owned()));
        assert_eq!(
            cache.cached_native_usd_rate(1),
            Some(U256::from(3_000_000_000_u64))
        );
        assert_eq!(
            cache.native_usd_statuses()["1"].state,
            NativeUsdState::Failed
        );
        let mut notifications = cache.subscribe_refreshes();
        // An unrelated settings revision must admit an already pending read.
        settings.runtime.auto_lock_timeout_secs = Some(600);
        configs = build_effective_chain_configs(&settings).unwrap();
        assert!(!cache.reconcile_native_sources(&configs, &[]));
        cache.publish_native(&original, Ok((U256::from(3_001_000_000_u64), 18)));
        assert_eq!(
            cache.native_usd_statuses()["1"]
                .quote
                .as_ref()
                .unwrap()
                .feed_decimals,
            18
        );
        for replacement in [
            NativeUsdPricing::Oracle {
                contract_address: Address::repeat_byte(7),
            },
            NativeUsdPricing::Default,
            NativeUsdPricing::Disabled,
        ] {
            let old = read(1);
            let (finish, pending) = tokio::sync::oneshot::channel();
            let stale_cache = cache.clone();
            let stale = tokio::spawn(async move {
                pending.await.unwrap();
                stale_cache.publish_native(&old, Ok((U256::MAX, 77)));
                stale_cache.publish_native(&old, Err("Retired failure".to_owned()));
            });
            settings
                .chains
                .per_chain
                .get_mut(&1)
                .unwrap()
                .native_usd_pricing = replacement;
            configs = build_effective_chain_configs(&settings).unwrap();
            notifications.borrow_and_update();
            cache.reconcile_native_sources(&configs, &[]);
            assert!(notifications.has_changed().unwrap());
            assert_eq!(cache.cached_native_usd_rate(1), None);
            let current_status = cache.native_usd_statuses()["1"].clone();
            finish.send(()).unwrap();
            stale.await.unwrap();
            assert_eq!(cache.native_usd_statuses()["1"], current_status);
            assert_eq!(cache.cached_native_usd_rate(1), None);
            assert_eq!(
                cache.cached_native_usd_rate(56),
                Some(U256::from(500_000_000))
            );
            if replacement != NativeUsdPricing::Disabled {
                cache.publish_native(&read(1), Err("Replacement unavailable".to_owned()));
                assert_eq!(cache.cached_native_usd_rate(1), None);
            }
        }
        assert!(
            !cache
                .native_reads()
                .iter()
                .any(|read| read.chain.chain_id == 1)
        );
        // Even the original A generation cannot return after A -> B -> A -> Disabled.
        cache.publish_native(&original, Ok((U256::MAX, 77)));
        assert_eq!(cache.cached_native_usd_rate(1), None);
    }
}
