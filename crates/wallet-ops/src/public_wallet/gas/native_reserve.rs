#[allow(clippy::wildcard_imports)]
use super::*;

pub async fn estimate_public_native_action_gas_reserve(
    chain_id: u64,
    steps: &[PublicActionProgressStep],
    effective_chain: &EffectiveChainConfig,
    gas_fee: PublicActionGasFeeSelection,
    http: &HttpContext,
) -> Result<U256> {
    estimate_public_native_action_gas_reserve_with_profile_and_ceiling(
        chain_id,
        steps,
        PublicShieldTransactionProfile::Railoxide,
        effective_chain,
        gas_fee,
        http,
        None,
    )
    .await
}

pub async fn estimate_public_native_action_gas_reserve_with_profile_and_ceiling(
    chain_id: u64,
    steps: &[PublicActionProgressStep],
    profile: PublicShieldTransactionProfile,
    effective_chain: &EffectiveChainConfig,
    gas_fee: PublicActionGasFeeSelection,
    http: &HttpContext,
    authorization_ceiling: Option<PublicActionGasFeeSelection>,
) -> Result<U256> {
    if profile == PublicShieldTransactionProfile::Railway {
        return Err(eyre!(
            "a stealth-route native shield's Max comes from its prepared estimate"
        ));
    }
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let candidates = public_fee_candidates(
        http,
        &chain.rpc_route.for_class(RpcTrafficClass::General)?,
        None,
    )
    .await?;
    let quote_bundle = public_action_gas_fee_quote_bundle_from_candidates_with_profile(
        &candidates,
        http.network_mode(),
        chain_id,
        profile,
        PublicAssetId::Native,
    )
    .await
    .wrap_err("fetch public action gas price")?;
    let quote = quote_bundle.standard;
    let gas = resolve_public_action_gas_fee(
        chain_id,
        profile,
        PublicAssetId::Native,
        gas_fee,
        Some(quote),
    )?;
    let maximum_gas = authorization_ceiling.map_or(Ok(gas), |ceiling| {
        resolve_public_action_gas_fee(chain_id, profile, PublicAssetId::Native, ceiling, None)
    })?;
    Ok(public_native_action_gas_reserve_with_buffer(
        RailgunGasModel::for_chain(chain_id),
        maximum_gas.max_fee_per_gas,
        steps,
        chain.gas.gas_limit_buffer,
    ))
}

pub async fn estimate_public_native_action_gas_reserve_with_profile(
    chain_id: u64,
    steps: &[PublicActionProgressStep],
    profile: PublicShieldTransactionProfile,
    effective_chain: &EffectiveChainConfig,
    gas_fee: PublicActionGasFeeSelection,
    http: &HttpContext,
) -> Result<U256> {
    estimate_public_native_action_gas_reserve_with_profile_and_ceiling(
        chain_id,
        steps,
        profile,
        effective_chain,
        gas_fee,
        http,
        None,
    )
    .await
}
