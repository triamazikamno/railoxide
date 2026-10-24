use super::*;
use crate::rpc_broker::RpcCandidates;
use eyre::eyre;

#[derive(Clone)]
pub(super) struct EffectiveDesktopChainConfig {
    /// The resolved route, whose clones share its endpoint identity checks.
    pub(super) rpc_route: RpcChainRoute,
    pub(super) railgun_contract: Address,
    pub(super) relay_adapt_contract: Option<Address>,
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
        rpc_route,
        railgun_contract: private.deployment.contract,
        relay_adapt_contract: private.deployment.relay_adapt_contract,
        wrapped_native_token: effective_chain.wrapped_native_token,
        finality_depth: effective_chain.finality_depth,
        gas: effective_chain.gas.clone(),
    })
}

/// A pool over `route`'s endpoints, each sending through its transport's client.
pub(crate) fn query_rpc_pool_for_route(
    route: &crate::ClassRoute,
    http: &HttpContext,
) -> Arc<QueryRpcPool> {
    Arc::new(route.query_rpc_pool(&http.rpc_clients(), DEFAULT_QUERY_RPC_COOLDOWN))
}

/// Starts identity warmup for enabled privacy chains and `selected_chain`, prioritizing it.
/// Other routes can be activated later through the returned handle. The registry's route objects
/// are passed, so successful checks reach the caches that later reads use. Dropping every
/// returned handle stops the pass.
#[must_use]
pub fn start_rpc_identity_warmup(
    http: &HttpContext,
    chains: &settings::EffectiveChainRegistry,
    selected_chain: u64,
) -> crate::RpcIdentityWarmup {
    http.rpc_broker().warm_identities(
        chains
            .enabled_chains()
            .filter(|chain| chain.chain_id == selected_chain || chain.require_railgun().is_ok())
            .map(|chain| chain.rpc_route.clone())
            .collect(),
        Some(selected_chain),
    )
}

/// Captures `route`'s fee candidates for one operation, without an enclosing deadline unless
/// `deadline` gives one. A failed lookup never falls back to the route's raw endpoints.
pub(crate) async fn acquire_fee_candidates(
    http: &HttpContext,
    route: &crate::ClassRoute,
    deadline: Option<tokio::time::Instant>,
) -> Result<RpcCandidates> {
    RpcCandidates::acquire(http, route, deadline)
        .await
        .wrap_err("select fee query RPC candidates")
}

pub(crate) async fn buffered_gas_price_from_candidates(
    candidates: &RpcCandidates,
    gas: &settings::EffectiveChainGasSettings,
) -> Result<u128> {
    gas_price_from_candidates_with_policy(
        candidates,
        u128::from(gas.gas_price_buffer_numerator),
        u128::from(gas.gas_price_buffer_denominator),
    )
    .await
}

pub(crate) async fn gas_price_from_candidates_with_policy(
    candidates: &RpcCandidates,
    numerator: u128,
    denominator: u128,
) -> Result<u128> {
    let mut last_error = None;
    for _ in 0..candidates.len() {
        let Some(provider_handle) = candidates.random_ready().await else {
            break;
        };
        match buffered_gas_price_with_policy(&provider_handle.provider, numerator, denominator)
            .await
        {
            Ok(gas_price) => return Ok(gas_price),
            Err(error) => {
                let rpc = http::redact_url_for_display(&provider_handle.url);
                tracing::warn!(%error, %rpc, "fetch gas price failed");
                candidates.pool().mark_bad_provider(&provider_handle);
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
