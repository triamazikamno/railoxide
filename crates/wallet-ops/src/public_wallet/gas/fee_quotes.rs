#[allow(clippy::wildcard_imports)]
use super::*;

pub async fn quote_public_action_gas_fee(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    http: &HttpContext,
) -> Result<PublicActionGasFeeQuote> {
    quote_public_action_gas_fee_with_reads(chain_id, effective_chain, http, None).await
}

pub async fn quote_public_action_gas_fee_with_reads(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    http: &HttpContext,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
) -> Result<PublicActionGasFeeQuote> {
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    // A quote for a dapp's request belongs to that request's Sensitive flow.
    let class = if rpc_reads.is_some() {
        RpcTrafficClass::Sensitive
    } else {
        RpcTrafficClass::General
    };
    let candidates =
        public_fee_candidates(http, &chain.rpc_route.for_class(class)?, rpc_reads).await?;
    public_action_gas_fee_quote_from_candidates_with_reads(
        &candidates,
        http.network_mode(),
        chain_id,
        rpc_reads,
    )
    .await
}

pub async fn quote_public_action_gas_fee_with_profile(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    http: &HttpContext,
) -> Result<PublicActionGasFeeQuote> {
    Ok(quote_public_action_gas_fee_bundle_with_profile(
        chain_id,
        effective_chain,
        profile,
        asset,
        http,
    )
    .await?
    .standard)
}

/// `asset` selects the profile's route: Mimic Railway prices BNB ERC-20 actions with a legacy
/// gas price and its native shields with fee history.
pub async fn quote_public_action_gas_fee_bundle_with_profile(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    http: &HttpContext,
) -> Result<PublicActionGasFeeQuoteBundle> {
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let candidates = public_fee_candidates(
        http,
        &chain.rpc_route.for_class(RpcTrafficClass::General)?,
        None,
    )
    .await?;
    public_action_gas_fee_quote_bundle_from_candidates_with_profile(
        &candidates,
        http.network_mode(),
        chain_id,
        profile,
        asset,
    )
    .await
}

/// Captures `route`'s fee candidates for one quote, estimate or preflight. Gateway reads
/// verify each endpoint's identity in their own admission, so their source probes none. A
/// failed lookup never falls back to the route's raw endpoints.
pub(in crate::public_wallet) async fn public_fee_candidates(
    http: &HttpContext,
    route: &ClassRoute,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
) -> Result<RpcCandidates> {
    match rpc_reads {
        Some(_) => RpcCandidates::acquire_for_gateway(http, route, None)
            .await
            .wrap_err("select fee query RPC candidates"),
        None => crate::acquire_fee_candidates(http, route, None).await,
    }
}

pub(in crate::public_wallet) async fn public_action_gas_fee_quote_from_candidates(
    candidates: &RpcCandidates,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
) -> Result<PublicActionGasFeeQuote> {
    public_action_gas_fee_quote_from_candidates_with_reads(candidates, network_mode, chain_id, None)
        .await
}

/// Only the Railoxide profile follows the observed fee model; Mimic Railway's route decides.
pub(in crate::public_wallet) fn public_action_uses_legacy_envelope(
    chain_id: u64,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    quote: Option<PublicActionGasFeeQuote>,
) -> bool {
    profile.uses_legacy_envelope(chain_id, asset)
        || (profile == PublicShieldTransactionProfile::Railoxide
            && quote
                .is_some_and(|quote| quote.observed_fee_model == Some(crate::EvmFeeModel::Legacy)))
}

/// Observes a header, then samples fees. Both stages draw from `candidates`, and sampling's
/// collection window starts once the header is observed.
pub(in crate::public_wallet) async fn public_action_gas_fee_quote_from_candidates_with_reads(
    candidates: &RpcCandidates,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
) -> Result<PublicActionGasFeeQuote> {
    let base_fee = observed_public_base_fee(candidates, chain_id, rpc_reads).await?;
    let mut quote = crate::self_broadcast_gas_fee_quote_from_candidates_with_reads(
        candidates,
        tokio::time::Instant::now(),
        network_mode,
        public_action_tip_fallback(chain_id),
        rpc_reads.map(|reads| (reads, chain_id)),
    )
    .await?;
    quote.observed_fee_model = Some(if base_fee.is_some() {
        crate::EvmFeeModel::Eip1559
    } else {
        crate::EvmFeeModel::Legacy
    });
    quote.current_base_fee_per_gas = base_fee;
    if base_fee.is_none() {
        quote.suggested_max_fee_per_gas = quote.rpc_gas_price;
        quote.suggested_max_priority_fee_per_gas = 0;
    }
    Ok(quote)
}

/// Tries ready candidates once each, including endpoints admitted while earlier ones fail,
/// until one returns a header or discovery has ended.
async fn observed_public_base_fee(
    candidates: &RpcCandidates,
    chain_id: u64,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
) -> Result<Option<u128>> {
    let mut tried = HashSet::new();
    while let Some(provider) = candidates.next_untried(&mut tried).await {
        let observation = async {
            match rpc_reads {
                Some(reads) => reads
                    .latest_block(provider.url.clone().into(), chain_id)
                    .await
                    .map_err(eyre::Report::new),
                None => provider
                    .provider
                    .get_block_by_number(BlockNumberOrTag::Latest)
                    .await
                    .map_err(eyre::Report::new),
            }
        };
        let observation =
            tokio::time::timeout(std::time::Duration::from_secs(15), observation).await;
        match &observation {
            Ok(Ok(_)) => candidates.succeeded(&provider),
            Ok(Err(error)) => candidates.failed(&provider, error),
            Err(_) => candidates.report(&provider, EndpointHealthOutcome::Strike),
        }
        match observation {
            Ok(Ok(Some(block))) => return Ok(block.header.base_fee_per_gas.map(u128::from)),
            Ok(Err(error))
                if error
                    .downcast_ref::<crate::RpcBrokerError>()
                    .is_some_and(crate::public_wallet::stops_dapp_read_retries) =>
            {
                return Err(error);
            }
            _ => {}
        }
    }
    Err(eyre!(
        "Could not observe the chain fee model from a current block header"
    ))
}

pub(in crate::public_wallet) async fn public_action_gas_fee_quote_from_candidates_with_profile(
    candidates: &RpcCandidates,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
) -> Result<PublicActionGasFeeQuote> {
    Ok(
        public_action_gas_fee_quote_bundle_from_candidates_with_profile(
            candidates,
            network_mode,
            chain_id,
            profile,
            asset,
        )
        .await?
        .standard,
    )
}

pub(in crate::public_wallet) async fn public_action_gas_fee_quote_bundle_from_candidates_with_profile(
    candidates: &RpcCandidates,
    network_mode: crate::WalletNetworkMode,
    chain_id: u64,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
) -> Result<PublicActionGasFeeQuoteBundle> {
    if profile == PublicShieldTransactionProfile::Railoxide {
        let standard =
            public_action_gas_fee_quote_from_candidates(candidates, network_mode, chain_id).await?;
        return Ok(public_action_gas_fee_quote_bundle_from_standard(standard));
    }
    // Ready candidates are tried once each, including endpoints admitted while earlier ones fail.
    let mut tried = HashSet::new();
    while let Some(provider_handle) = candidates.next_untried(&mut tried).await {
        match railway_gas_fee_quote_from_provider(&provider_handle.provider, chain_id, asset).await
        {
            Ok(quote) => {
                candidates.succeeded(&provider_handle);
                return Ok(quote);
            }
            Err(error) => {
                candidates.failed(&provider_handle, &error);
                tracing::debug!(target: LOG_TARGET,"Railway gas fee quote provider attempt failed");
            }
        }
    }
    if tried.is_empty() {
        return Err(eyre!("no healthy query RPC available"));
    }
    Err(eyre!("all Railway gas quote RPC attempts failed"))
}

/// An unusable response fails only this provider, so the caller can try the next.
async fn railway_gas_fee_quote_from_provider(
    provider: &impl Provider,
    chain_id: u64,
    asset: PublicAssetId,
) -> Result<PublicActionGasFeeQuoteBundle> {
    if PublicShieldTransactionProfile::Railway.uses_legacy_envelope(chain_id, asset) {
        let provider_gas_price = provider
            .get_gas_price()
            .await
            .wrap_err("fetch Railway BNB gas price")?;
        return Ok(railway_bnb_gas_fee_quote_bundle(provider_gas_price));
    }
    let fee_history = provider
        .get_fee_history(
            RAILWAY_FEE_HISTORY_BLOCKS,
            BlockNumberOrTag::Latest,
            &RAILWAY_FEE_HISTORY_REWARD_PERCENTILES,
        )
        .await
        .wrap_err("fetch Railway fee history")?;
    // Railway caps BNB's next base fee before its 110% multiplier, for native type-4 shields.
    railway_standard_gas_fee_quote_bundle(
        &fee_history.base_fee_per_gas,
        fee_history.reward.as_deref(),
        (chain_id == PUBLIC_ACTION_BNB_CHAIN_ID).then_some(RAILWAY_BNB_GAS_PRICE_CAP),
    )
}

#[cfg(test)]
pub(in crate::public_wallet) fn railway_standard_gas_fee_quote(
    base_fee_per_gas: &[u128],
    rewards: Option<&[Vec<u128>]>,
) -> Result<PublicActionGasFeeQuote> {
    Ok(railway_standard_gas_fee_quote_bundle(base_fee_per_gas, rewards, None)?.standard)
}

pub(in crate::public_wallet) fn railway_standard_gas_fee_quote_bundle(
    base_fee_per_gas: &[u128],
    rewards: Option<&[Vec<u128>]>,
    next_base_fee_cap: Option<u128>,
) -> Result<PublicActionGasFeeQuoteBundle> {
    let next_base_fee_per_gas = base_fee_per_gas
        .last()
        .copied()
        .ok_or_else(|| eyre!("Railway fee history returned no base fee"))?;
    let next_base_fee_per_gas =
        next_base_fee_cap.map_or(next_base_fee_per_gas, |cap| next_base_fee_per_gas.min(cap));
    let rewards = rewards.ok_or_else(|| eyre!("Railway fee history returned no rewards"))?;
    let mut priority_fees = rewards
        .iter()
        .map(|reward| reward.get(1).copied())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| eyre!("Railway fee history reward columns are incomplete"))?;
    let max_priority_fee_per_gas = railway_lower_median(&mut priority_fees)
        .ok_or_else(|| eyre!("Railway fee history returned no priority fees"))?;
    let mut aggressive_priority_fees = rewards
        .iter()
        .map(|reward| reward.get(3).copied())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| eyre!("Railway fee history reward columns are incomplete"))?;
    let aggressive_priority_fee_per_gas = railway_lower_median(&mut aggressive_priority_fees)
        .ok_or_else(|| eyre!("Railway fee history returned no priority fees"))?;
    let max_base_fee_per_gas = next_base_fee_per_gas
        .checked_mul(110)
        .ok_or_else(|| eyre!("Railway base fee overflow"))?
        / 100;
    let max_fee_per_gas = max_base_fee_per_gas
        .checked_add(max_priority_fee_per_gas)
        .ok_or_else(|| eyre!("Railway max fee overflow"))?;
    let aggressive_max_base_fee_per_gas = next_base_fee_per_gas
        .checked_mul(140)
        .ok_or_else(|| eyre!("Railway aggressive base fee overflow"))?
        / 100;
    let aggressive_max_fee_per_gas = aggressive_max_base_fee_per_gas
        .checked_add(aggressive_priority_fee_per_gas)
        .ok_or_else(|| eyre!("Railway aggressive max fee overflow"))?;
    let standard = PublicActionGasFeeQuote {
        observed_fee_model: None,
        rpc_gas_price: max_fee_per_gas,
        current_base_fee_per_gas: base_fee_per_gas
            .len()
            .checked_sub(2)
            .and_then(|index| base_fee_per_gas.get(index))
            .copied(),
        suggested_max_fee_per_gas: max_fee_per_gas,
        suggested_max_priority_fee_per_gas: max_priority_fee_per_gas,
    };
    Ok(PublicActionGasFeeQuoteBundle {
        standard,
        authorization_ceiling: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: aggressive_max_fee_per_gas,
            max_priority_fee_per_gas: aggressive_priority_fee_per_gas,
        },
    })
}

#[cfg(test)]
pub(in crate::public_wallet) fn railway_bnb_gas_fee_quote(
    provider_gas_price: u128,
) -> PublicActionGasFeeQuote {
    railway_bnb_gas_fee_quote_bundle(provider_gas_price).standard
}

pub(in crate::public_wallet) fn railway_bnb_gas_fee_quote_bundle(
    provider_gas_price: u128,
) -> PublicActionGasFeeQuoteBundle {
    let capped_gas_price = provider_gas_price.min(RAILWAY_BNB_GAS_PRICE_CAP);
    let standard_gas_price = capped_gas_price * 110 / 100;
    let aggressive_gas_price = capped_gas_price * 140 / 100;
    let standard = PublicActionGasFeeQuote {
        observed_fee_model: None,
        rpc_gas_price: standard_gas_price,
        current_base_fee_per_gas: None,
        suggested_max_fee_per_gas: standard_gas_price,
        suggested_max_priority_fee_per_gas: 0,
    };
    PublicActionGasFeeQuoteBundle {
        standard,
        authorization_ceiling: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: aggressive_gas_price,
            max_priority_fee_per_gas: 0,
        },
    }
}

const fn public_action_gas_fee_quote_bundle_from_standard(
    standard: PublicActionGasFeeQuote,
) -> PublicActionGasFeeQuoteBundle {
    PublicActionGasFeeQuoteBundle {
        authorization_ceiling: PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: standard.suggested_max_fee_per_gas,
            max_priority_fee_per_gas: standard.suggested_max_priority_fee_per_gas,
        },
        standard,
    }
}

pub(in crate::public_wallet) fn railway_lower_median(values: &mut [u128]) -> Option<u128> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    values.get((values.len() - 1) / 2).copied()
}

pub(in crate::public_wallet) const fn public_action_tip_fallback(
    chain_id: u64,
) -> SelfBroadcastTipFallback {
    if chain_id == PUBLIC_ACTION_BNB_CHAIN_ID {
        SelfBroadcastTipFallback::RpcGasPrice
    } else {
        SelfBroadcastTipFallback::Minimum
    }
}
