use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfBroadcastResolvedGasFee {
    pub rpc_gas_price: u128,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::desktop) struct SelfBroadcastFeeQuoteTimeoutPolicy {
    pub(in crate::desktop) grace_after_first_usable: Duration,
    pub(in crate::desktop) hard_deadline: Duration,
}

impl SelfBroadcastFeeQuoteTimeoutPolicy {
    pub(super) const fn for_network_mode(network_mode: WalletNetworkMode) -> Self {
        match network_mode {
            WalletNetworkMode::Tor => Self {
                grace_after_first_usable: SELF_BROADCAST_TOR_FEE_QUOTE_GRACE,
                hard_deadline: SELF_BROADCAST_TOR_FEE_QUOTE_DEADLINE,
            },
            WalletNetworkMode::Proxy | WalletNetworkMode::Direct => Self {
                grace_after_first_usable: SELF_BROADCAST_DIRECT_FEE_QUOTE_GRACE,
                hard_deadline: SELF_BROADCAST_DIRECT_FEE_QUOTE_DEADLINE,
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SelfBroadcastFeeSample {
    pub(crate) rpc_gas_price: Option<u128>,
    pub(crate) max_priority_fee_per_gas: Option<u128>,
    pub(crate) current_base_fee_per_gas: Option<u128>,
    pub(crate) next_base_fee_per_gas: Option<u128>,
    pub(crate) priority_fee_rewards: Vec<u128>,
}

impl SelfBroadcastFeeSample {
    pub(super) fn from_parts(
        rpc_gas_price: Option<u128>,
        max_priority_fee_per_gas: Option<u128>,
        fee_history: Option<FeeHistory>,
    ) -> Self {
        let Some(fee_history) = fee_history else {
            return Self {
                rpc_gas_price,
                max_priority_fee_per_gas,
                current_base_fee_per_gas: None,
                next_base_fee_per_gas: None,
                priority_fee_rewards: Vec::new(),
            };
        };
        let current_base_fee_per_gas = fee_history
            .base_fee_per_gas
            .len()
            .checked_sub(2)
            .and_then(|index| fee_history.base_fee_per_gas.get(index))
            .copied();
        let next_base_fee_per_gas = fee_history.base_fee_per_gas.last().copied();
        let priority_fee_rewards = fee_history
            .reward
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect();
        Self {
            rpc_gas_price,
            max_priority_fee_per_gas,
            current_base_fee_per_gas,
            next_base_fee_per_gas,
            priority_fee_rewards,
        }
    }

    fn has_non_zero_fee_history_tip(&self) -> bool {
        self.priority_fee_rewards.iter().any(|value| *value > 0)
    }

    const fn has_non_zero_priority_tip(&self) -> bool {
        matches!(self.max_priority_fee_per_gas, Some(value) if value > 0)
    }

    fn has_usable_tip(&self) -> bool {
        self.has_non_zero_fee_history_tip() || self.has_non_zero_priority_tip()
    }
}

/// Samples every candidate that is ready or becomes ready before collection ends. Returns
/// `None` when no candidate became ready.
pub(in crate::desktop) async fn self_broadcast_parallel_fee_samples(
    candidates: &RpcCandidates,
    started_at: tokio::time::Instant,
    policy: SelfBroadcastFeeQuoteTimeoutPolicy,
) -> Option<Vec<SelfBroadcastFeeSample>> {
    self_broadcast_parallel_fee_samples_with_reads(candidates, started_at, policy, None)
        .await
        .expect("native fee sampling does not return broker errors")
}

type SelfBroadcastFeeSampleJobs = JoinSet<Result<SelfBroadcastFeeSample, RpcBrokerError>>;

/// The hard cutoff counts from `started_at` and the grace period from the first usable tip;
/// neither restarts when a candidate becomes ready.
pub(super) async fn self_broadcast_parallel_fee_samples_with_reads(
    candidates: &RpcCandidates,
    started_at: tokio::time::Instant,
    policy: SelfBroadcastFeeQuoteTimeoutPolicy,
    rpc_reads: Option<(crate::DappRpcReadClient, u64)>,
) -> Result<Option<Vec<SelfBroadcastFeeSample>>, RpcBrokerError> {
    // Subscribed before the first launch, so no admission between the two is missed.
    let mut progress = candidates.subscribe();
    let mut more_candidates = true;
    let mut launched = HashSet::new();
    let mut join_set = JoinSet::new();
    launch_ready_fee_samples(
        candidates,
        &mut launched,
        &mut join_set,
        policy,
        rpc_reads.as_ref(),
    );

    let hard_deadline = started_at + policy.hard_deadline;
    let mut last_error = None;
    let mut samples = Vec::new();
    let mut grace_deadline = None;
    // Without running jobs, collection continues only while a candidate may still arrive.
    while !join_set.is_empty() || more_candidates {
        let deadline = grace_deadline.map_or(hard_deadline, |grace: tokio::time::Instant| {
            grace.min(hard_deadline)
        });
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            biased;
            joined = join_set.join_next(), if !join_set.is_empty() => match joined {
                Some(Ok(Ok(sample))) => {
                    let usable_tip = sample.has_usable_tip();
                    samples.push(sample);
                    if usable_tip && grace_deadline.is_none() {
                        grace_deadline =
                            Some(tokio::time::Instant::now() + policy.grace_after_first_usable);
                    }
                    let non_zero_fee_history_sources = samples
                        .iter()
                        .filter(|sample| sample.has_non_zero_fee_history_tip())
                        .count();
                    if non_zero_fee_history_sources >= 2 {
                        break;
                    }
                }
                Some(Ok(Err(error))) => {
                    if crate::public_wallet::stops_dapp_read_retries(&error) {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
                Some(Err(error)) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        %error,
                        "self-broadcast gas fee quote task failed"
                    );
                }
                None => {}
            },
            changed = progress.changed(), if more_candidates => {
                if changed.is_ok() {
                    launch_ready_fee_samples(
                        candidates,
                        &mut launched,
                        &mut join_set,
                        policy,
                        rpc_reads.as_ref(),
                    );
                } else {
                    more_candidates = false;
                }
            }
            () = tokio::time::sleep_until(deadline) => break,
        }
    }
    // Stops the sample jobs only; discovery belongs to the candidates' owner.
    join_set.abort_all();
    if launched.is_empty() {
        return Ok(None);
    }
    if samples.is_empty()
        && let Some(error) = last_error
    {
        return Err(error);
    }
    Ok(Some(samples))
}

/// Starts one sample job for each ready candidate not yet in `launched`.
fn launch_ready_fee_samples(
    candidates: &RpcCandidates,
    launched: &mut HashSet<usize>,
    join_set: &mut SelfBroadcastFeeSampleJobs,
    policy: SelfBroadcastFeeQuoteTimeoutPolicy,
    rpc_reads: Option<&(crate::DappRpcReadClient, u64)>,
) {
    for provider_handle in candidates.pool().available_providers() {
        if !launched.insert(provider_handle.index) {
            continue;
        }
        let rpc_reads = rpc_reads.cloned();
        let candidates = candidates.clone();
        join_set.spawn(async move {
            match rpc_reads {
                Some((reads, chain_id)) => {
                    self_broadcast_admitted_fee_sample(provider_handle, &reads, chain_id).await
                }
                None => Ok(self_broadcast_provider_fee_sample(
                    &candidates,
                    provider_handle,
                    policy.hard_deadline,
                )
                .await),
            }
        });
    }
}

async fn self_broadcast_admitted_fee_sample(
    provider_handle: ProviderHandle,
    reads: &crate::DappRpcReadClient,
    chain_id: u64,
) -> Result<SelfBroadcastFeeSample, crate::rpc_broker::RpcBrokerError> {
    // Optional provider failures still contribute partial samples; admission failures stop
    // the sample immediately so the shared hard deadline cannot hide their provenance.
    fn fee_result<T>(
        result: Result<T, crate::rpc_broker::RpcBrokerError>,
    ) -> Result<Result<T, crate::rpc_broker::RpcBrokerError>, crate::rpc_broker::RpcBrokerError>
    {
        if let Err(error) = &result
            && crate::public_wallet::stops_dapp_read_retries(error)
        {
            return Err(error.clone());
        }
        Ok(result)
    }
    let endpoint = poi::SensitiveUrl::from(provider_handle.url);
    let (gas_price, priority_fee, fee_history) = tokio::try_join!(
        async { fee_result(reads.get_gas_price(endpoint.clone(), chain_id).await,) },
        async {
            fee_result(
                reads
                    .get_max_priority_fee_per_gas(endpoint.clone(), chain_id)
                    .await,
            )
        },
        async {
            fee_result(
                reads
                    .get_fee_history(
                        endpoint.clone(),
                        chain_id,
                        SELF_BROADCAST_FEE_HISTORY_BLOCKS,
                        BlockNumberOrTag::Latest,
                        &SELF_BROADCAST_FEE_HISTORY_REWARD_PERCENTILES,
                    )
                    .await,
            )
        },
    )?;
    if gas_price.is_err() && priority_fee.is_err() && fee_history.is_err() {
        return Err(gas_price.expect_err("all fee reads failed"));
    }
    Ok(SelfBroadcastFeeSample::from_parts(
        gas_price.ok(),
        priority_fee.ok(),
        fee_history.ok(),
    ))
}

/// Reports the sample's requests as one outcome once all have settled; a sample dropped at the
/// collection cutoff reports nothing.
pub(in crate::desktop) async fn self_broadcast_provider_fee_sample(
    candidates: &RpcCandidates,
    provider_handle: ProviderHandle,
    timeout: Duration,
) -> SelfBroadcastFeeSample {
    fn outcome<T>(
        result: &std::result::Result<
            std::result::Result<T, alloy::transports::TransportError>,
            tokio::time::error::Elapsed,
        >,
    ) -> EndpointHealthOutcome {
        match result {
            Ok(Ok(_)) => EndpointHealthOutcome::Healthy,
            Ok(Err(error)) => EndpointHealthOutcome::for_direct_error(error),
            Err(_) => EndpointHealthOutcome::Strike,
        }
    }
    let provider = &provider_handle.provider;
    let gas_price = tokio::time::timeout(timeout, provider.get_gas_price());
    let max_priority_fee = tokio::time::timeout(timeout, provider.get_max_priority_fee_per_gas());
    let fee_history = tokio::time::timeout(
        timeout,
        provider.get_fee_history(
            SELF_BROADCAST_FEE_HISTORY_BLOCKS,
            BlockNumberOrTag::Latest,
            &SELF_BROADCAST_FEE_HISTORY_REWARD_PERCENTILES,
        ),
    );
    let (gas_price, max_priority_fee, fee_history) =
        tokio::join!(gas_price, max_priority_fee, fee_history);
    candidates.report(
        &provider_handle,
        outcome(&gas_price)
            .combine(outcome(&max_priority_fee))
            .combine(outcome(&fee_history)),
    );
    let url = crate::http::redact_url_for_display(&provider_handle.url);
    let rpc_gas_price = match gas_price {
        Ok(Ok(value)) => Some(value),
        Ok(Err(_)) => {
            tracing::warn!(target: LOG_TARGET, %url, "self-broadcast eth_gasPrice failed");
            None
        }
        Err(_) => {
            tracing::warn!(target: LOG_TARGET, %url, "self-broadcast eth_gasPrice timed out");
            None
        }
    };
    let max_priority_fee_per_gas = match max_priority_fee {
        Ok(Ok(value)) => Some(value),
        Ok(Err(_)) => {
            tracing::debug!(
                target: LOG_TARGET,
                %url,
                "self-broadcast eth_maxPriorityFeePerGas failed"
            );
            None
        }
        Err(_) => {
            tracing::debug!(
                target: LOG_TARGET,
                %url,
                "self-broadcast eth_maxPriorityFeePerGas timed out"
            );
            None
        }
    };
    let fee_history = match fee_history {
        Ok(Ok(value)) => Some(value),
        Ok(Err(_)) => {
            tracing::debug!(target: LOG_TARGET, %url, "self-broadcast eth_feeHistory failed");
            None
        }
        Err(_) => {
            tracing::debug!(target: LOG_TARGET, %url, "self-broadcast eth_feeHistory timed out");
            None
        }
    };
    SelfBroadcastFeeSample::from_parts(rpc_gas_price, max_priority_fee_per_gas, fee_history)
}

pub(crate) fn self_broadcast_quote_from_fee_samples(
    samples: &[SelfBroadcastFeeSample],
) -> Option<SelfBroadcastGasFeeQuote> {
    self_broadcast_quote_from_fee_samples_with_tip_fallback(
        samples,
        SelfBroadcastTipFallback::Minimum,
    )
}

pub(crate) fn self_broadcast_quote_from_fee_samples_with_tip_fallback(
    samples: &[SelfBroadcastFeeSample],
    tip_fallback: SelfBroadcastTipFallback,
) -> Option<SelfBroadcastGasFeeQuote> {
    if samples.is_empty() {
        return None;
    }
    let mut gas_prices = non_zero_values(samples.iter().filter_map(|sample| sample.rpc_gas_price));
    let mut fee_history_rewards = non_zero_values(
        samples
            .iter()
            .flat_map(|sample| sample.priority_fee_rewards.iter().copied()),
    );
    let mut priority_fee_suggestions = non_zero_values(
        samples
            .iter()
            .filter_map(|sample| sample.max_priority_fee_per_gas),
    );
    let mut next_base_fees = non_zero_values(
        samples
            .iter()
            .filter_map(|sample| sample.next_base_fee_per_gas),
    );
    let mut current_base_fees = non_zero_values(
        samples
            .iter()
            .filter_map(|sample| sample.current_base_fee_per_gas),
    );

    let rpc_gas_price = upper_quartile(&mut gas_prices);
    let fallback_tip = match tip_fallback {
        SelfBroadcastTipFallback::Minimum => SELF_BROADCAST_MIN_PRIORITY_FEE_PER_GAS,
        SelfBroadcastTipFallback::RpcGasPrice => rpc_gas_price
            .unwrap_or(SELF_BROADCAST_MIN_PRIORITY_FEE_PER_GAS)
            .max(SELF_BROADCAST_MIN_PRIORITY_FEE_PER_GAS),
    };
    let selected_tip = lower_quartile(&mut fee_history_rewards)
        .or_else(|| lower_quartile(&mut priority_fee_suggestions))
        .unwrap_or(fallback_tip)
        .max(SELF_BROADCAST_MIN_PRIORITY_FEE_PER_GAS);
    let gas_price_max_fee = rpc_gas_price.map_or(0, self_broadcast_auto_max_fee_per_gas);
    let fee_history_max_fee = upper_quartile(&mut next_base_fees).map_or(0, |base_fee| {
        self_broadcast_auto_max_fee_per_gas(base_fee).saturating_add(selected_tip)
    });
    let suggested_max_fee_per_gas = gas_price_max_fee.max(fee_history_max_fee).max(selected_tip);
    Some(SelfBroadcastGasFeeQuote {
        observed_fee_model: None,
        rpc_gas_price: rpc_gas_price.unwrap_or(suggested_max_fee_per_gas),
        current_base_fee_per_gas: upper_quartile(&mut current_base_fees),
        suggested_max_fee_per_gas,
        suggested_max_priority_fee_per_gas: selected_tip.min(suggested_max_fee_per_gas),
    })
}

pub(in crate::desktop) fn non_zero_values(values: impl IntoIterator<Item = u128>) -> Vec<u128> {
    values.into_iter().filter(|value| *value > 0).collect()
}

pub(in crate::desktop) fn lower_quartile(values: &mut [u128]) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let index = values.len().saturating_sub(1) / 4;
    values.get(index).copied()
}

pub(in crate::desktop) fn upper_quartile(values: &mut [u128]) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let index = values.len().saturating_mul(3).saturating_sub(1) / 4;
    values.get(index).copied()
}

/// Quotes from a candidate source owned by this quote. The broker lookup and any cold identity
/// checks count against the sampler's hard window, which starts here.
pub(in crate::desktop) async fn self_broadcast_gas_fee_quote_from_route(
    http: &HttpContext,
    route: &ClassRoute,
) -> Result<SelfBroadcastGasFeeQuote> {
    let network_mode = http.network_mode();
    let started_at = tokio::time::Instant::now();
    let policy = SelfBroadcastFeeQuoteTimeoutPolicy::for_network_mode(network_mode);
    let candidates =
        acquire_fee_candidates(http, route, Some(started_at + policy.hard_deadline)).await?;
    self_broadcast_gas_fee_quote_from_candidates_with_reads(
        &candidates,
        started_at,
        network_mode,
        SelfBroadcastTipFallback::Minimum,
        None,
    )
    .await
}

/// `started_at` begins the sampler's hard window: the source's acquisition when the quote owns
/// it, or the sampler's entry when it follows other stages.
pub(crate) async fn self_broadcast_gas_fee_quote_from_candidates_with_reads(
    candidates: &RpcCandidates,
    started_at: tokio::time::Instant,
    network_mode: WalletNetworkMode,
    tip_fallback: SelfBroadcastTipFallback,
    rpc_reads: Option<(&crate::DappRpcReadClient, u64)>,
) -> Result<SelfBroadcastGasFeeQuote> {
    let policy = SelfBroadcastFeeQuoteTimeoutPolicy::for_network_mode(network_mode);
    let samples = match rpc_reads {
        Some((reads, chain_id)) => {
            self_broadcast_parallel_fee_samples_with_reads(
                candidates,
                started_at,
                policy,
                Some((reads.clone(), chain_id)),
            )
            .await?
        }
        None => self_broadcast_parallel_fee_samples(candidates, started_at, policy).await,
    };
    let Some(samples) = samples else {
        return Err(eyre!("no healthy query RPC available"));
    };
    if let Some(quote) =
        self_broadcast_quote_from_fee_samples_with_tip_fallback(&samples, tip_fallback)
    {
        return Ok(quote);
    }

    Err(eyre!("all self-broadcast gas quote RPC attempts failed"))
}

/// Reports the quote's requests as one outcome, a quote of unusable answers included. A failed
/// gas price is reported alone, before the other requests are sent.
pub(in crate::desktop) async fn self_broadcast_gas_fee_quote(
    candidates: &RpcCandidates,
    provider_handle: &ProviderHandle,
) -> Result<SelfBroadcastGasFeeQuote> {
    let provider = &provider_handle.provider;
    let rpc_gas_price = match provider.get_gas_price().await {
        Ok(gas_price) => gas_price,
        Err(error) => {
            candidates.report(
                provider_handle,
                EndpointHealthOutcome::for_direct_error(&error),
            );
            return Err(error).wrap_err("fetch gas price");
        }
    };
    let max_priority_fee_per_gas = provider.get_max_priority_fee_per_gas().await;
    let fee_history = provider
        .get_fee_history(
            SELF_BROADCAST_FEE_HISTORY_BLOCKS,
            BlockNumberOrTag::Latest,
            &SELF_BROADCAST_FEE_HISTORY_REWARD_PERCENTILES,
        )
        .await;
    let outcome = |error: Option<&alloy::transports::TransportError>| {
        error.map_or(
            EndpointHealthOutcome::Healthy,
            EndpointHealthOutcome::for_direct_error,
        )
    };
    candidates.report(
        provider_handle,
        outcome(max_priority_fee_per_gas.as_ref().err())
            .combine(outcome(fee_history.as_ref().err())),
    );
    let sample = SelfBroadcastFeeSample::from_parts(
        Some(rpc_gas_price),
        max_priority_fee_per_gas.ok(),
        fee_history.ok(),
    );
    self_broadcast_quote_from_fee_samples(&[sample])
        .ok_or_else(|| eyre!("self-broadcast gas fee quote returned no usable values"))
}

pub(crate) fn resolve_self_broadcast_gas_fee(
    selection: SelfBroadcastGasFeeSelection,
    quote: SelfBroadcastGasFeeQuote,
) -> Result<SelfBroadcastResolvedGasFee> {
    let (max_fee_per_gas, max_priority_fee_per_gas) = match selection {
        SelfBroadcastGasFeeSelection::Auto => (
            quote.suggested_max_fee_per_gas,
            quote.suggested_max_priority_fee_per_gas,
        ),
        SelfBroadcastGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } => (max_fee_per_gas, max_priority_fee_per_gas),
    };
    validate_self_broadcast_gas_fee(max_fee_per_gas, max_priority_fee_per_gas)?;
    Ok(SelfBroadcastResolvedGasFee {
        rpc_gas_price: quote.rpc_gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
    })
}

pub(crate) fn validate_self_broadcast_gas_fee(
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Result<()> {
    if max_fee_per_gas == 0 {
        return Err(eyre!(
            "self-broadcast max fee per gas must be greater than zero"
        ));
    }
    if max_priority_fee_per_gas > max_fee_per_gas {
        return Err(eyre!(
            "self-broadcast max priority fee per gas cannot exceed max fee per gas"
        ));
    }
    Ok(())
}
