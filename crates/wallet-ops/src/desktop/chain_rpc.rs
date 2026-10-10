use super::*;
use eyre::eyre;

#[derive(Clone)]
pub(super) struct EffectiveDesktopChainConfig {
    pub(super) rpc_urls: Vec<Url>,
    pub(super) railgun_contract: Address,
    pub(super) relay_adapt_contract: Address,
    pub(super) wrapped_native_token: Option<Address>,
    pub(super) finality_depth: u64,
    pub(super) gas: settings::EffectiveChainGasSettings,
}

pub(super) fn effective_desktop_chain_config(
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
) -> Result<EffectiveDesktopChainConfig> {
    let private = effective_chain.require_railgun()?;
    let rpc_route = settings::resolve_effective_chain_rpc_route(chain_id, effective_chain)?;
    Ok(EffectiveDesktopChainConfig {
        rpc_urls: rpc_route.endpoint_urls(),
        railgun_contract: private.deployment.contract,
        relay_adapt_contract: private.deployment.relay_adapt_contract,
        wrapped_native_token: effective_chain.wrapped_native_token,
        finality_depth: effective_chain.finality_depth,
        gas: effective_chain.gas.clone(),
    })
}

pub(crate) fn query_rpc_pool_with_http_client(
    rpc_urls: Vec<Url>,
    http: &HttpContext,
) -> Arc<QueryRpcPool> {
    Arc::new(QueryRpcPool::with_http_client(
        rpc_urls,
        DEFAULT_QUERY_RPC_COOLDOWN,
        http.rpc_client.clone(),
    ))
}

pub(crate) async fn buffered_gas_price_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    gas: &settings::EffectiveChainGasSettings,
) -> Result<u128> {
    gas_price_from_rpc_pool_with_policy(
        query_rpc_pool,
        u128::from(gas.gas_price_buffer_numerator),
        u128::from(gas.gas_price_buffer_denominator),
    )
    .await
}

pub(crate) async fn gas_price_from_rpc_pool_with_policy(
    query_rpc_pool: &QueryRpcPool,
    numerator: u128,
    denominator: u128,
) -> Result<u128> {
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        match buffered_gas_price_with_policy(&provider_handle.provider, numerator, denominator)
            .await
        {
            Ok(gas_price) => return Ok(gas_price),
            Err(error) => {
                let rpc = http::redact_url_for_display(&provider_handle.url);
                tracing::warn!(%error, %rpc, "fetch gas price failed");
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(error).wrap_err("all query RPC gas price attempts failed")
    } else {
        Err(eyre!("no healthy query RPC available"))
    }
}

pub(super) fn is_effective_wrapped_native_token(
    chain_id: u64,
    token: Address,
    chain: &EffectiveDesktopChainConfig,
) -> bool {
    chain.wrapped_native_token.map_or_else(
        || is_wrapped_native_token(chain_id, token),
        |wrapped| wrapped == token,
    )
}
