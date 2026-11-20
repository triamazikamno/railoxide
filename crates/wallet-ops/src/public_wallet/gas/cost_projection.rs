#[allow(clippy::wildcard_imports)]
use super::*;

pub fn estimate_public_action_gas_cost(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    kind: PublicActionKind,
    asset: PublicAssetId,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
) -> Result<Eip1559GasCostProjection> {
    estimate_public_action_gas_cost_with_profile(
        chain_id,
        effective_chain,
        kind,
        asset,
        PublicShieldTransactionProfile::Railoxide,
        gas_fee,
        quote,
    )
}

pub fn estimate_public_action_gas_cost_with_profile(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    kind: PublicActionKind,
    asset: PublicAssetId,
    profile: PublicShieldTransactionProfile,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
) -> Result<Eip1559GasCostProjection> {
    estimate_public_action_gas_cost_with_profile_and_ceiling(
        chain_id,
        effective_chain,
        kind,
        asset,
        profile,
        gas_fee,
        quote,
        None,
    )
}

pub fn estimate_public_action_gas_cost_with_profile_and_ceiling(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    kind: PublicActionKind,
    asset: PublicAssetId,
    profile: PublicShieldTransactionProfile,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
    authorization_ceiling: Option<PublicActionGasFeeSelection>,
) -> Result<Eip1559GasCostProjection> {
    let chain = public_chain_runtime_config(chain_id, effective_chain)?;
    let resolved = resolve_public_action_gas_fee(chain_id, profile, asset, gas_fee, quote)?;
    let maximum_resolved = authorization_ceiling.map_or(Ok(resolved), |ceiling| {
        resolve_public_action_gas_fee(chain_id, profile, asset, ceiling, None)
    })?;
    let model = RailgunGasModel::for_chain(chain_id);
    let expected_gas_units = public_action_estimated_gas_usage_units(model, kind, asset);
    let maximum_gas_units = public_action_estimated_gas_units_with_buffer(
        model,
        kind,
        asset,
        profile,
        chain.gas.gas_limit_buffer,
    )
    .ok_or_else(|| eyre!("a stealth-route native shield's fees come from its prepared estimate"))?;
    if public_action_uses_legacy_envelope(chain_id, profile, asset, quote) {
        return Ok(legacy_gas_cost_projection(
            expected_gas_units,
            maximum_gas_units,
            resolved.max_fee_per_gas,
            maximum_resolved.max_fee_per_gas,
        ));
    }
    let quote = quote
        .unwrap_or_else(|| SelfBroadcastGasFeeQuote::from_rpc_gas_price(resolved.max_fee_per_gas));
    Ok(public_action_eip1559_gas_cost_projection(
        expected_gas_units,
        maximum_gas_units,
        quote,
        resolved.max_fee_per_gas,
        resolved.max_priority_fee_per_gas,
        maximum_resolved.max_fee_per_gas,
    ))
}

#[must_use]
fn public_action_eip1559_gas_cost_projection(
    expected_gas_units: u64,
    maximum_gas_units: u64,
    quote: PublicActionGasFeeQuote,
    expected_max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    maximum_fee_per_gas: u128,
) -> Eip1559GasCostProjection {
    let expected_fee_per_gas =
        expected_eip1559_fee_per_gas(quote, expected_max_fee_per_gas, max_priority_fee_per_gas);
    Eip1559GasCostProjection {
        expected_fee_per_gas,
        maximum_fee_per_gas,
        expected_cost: U256::from(expected_gas_units) * U256::from(expected_fee_per_gas),
        maximum_cost: U256::from(maximum_gas_units) * U256::from(maximum_fee_per_gas),
    }
}

#[must_use]
pub fn project_public_action_fee(
    raw_gas_limit: u64,
    gas_limit: u64,
    quote: PublicActionGasFeeQuote,
    resolved_fee: PublicActionResolvedGasFee,
    source: PublicActionFeeSource,
    native_usd_micro_rate: Option<U256>,
    native_decimals: u8,
) -> PublicActionFeeProjection {
    let projection = public_action_eip1559_gas_cost_projection(
        raw_gas_limit,
        gas_limit,
        quote,
        resolved_fee.max_fee_per_gas,
        resolved_fee.max_priority_fee_per_gas,
        resolved_fee.max_fee_per_gas,
    );
    PublicActionFeeProjection {
        source,
        raw_gas_limit,
        gas_limit,
        max_fee_per_gas: resolved_fee.max_fee_per_gas,
        max_priority_fee_per_gas: resolved_fee.max_priority_fee_per_gas,
        expected_fee_per_gas: projection.expected_fee_per_gas,
        expected_gas_cost: projection.expected_cost,
        maximum_gas_cost: projection.maximum_cost,
        expected_native_usd_micro_value: native_usd_micro_rate.and_then(|rate| {
            native_usd_micro_value(projection.expected_cost, rate, native_decimals)
        }),
        maximum_native_usd_micro_value: native_usd_micro_rate.and_then(|rate| {
            native_usd_micro_value(projection.maximum_cost, rate, native_decimals)
        }),
    }
}

impl PublicAdvancedTransactionEstimate {
    #[must_use]
    pub fn fee_projection(
        &self,
        native_usd_micro_rate: Option<U256>,
        native_decimals: u8,
    ) -> PublicActionFeeProjection {
        PublicActionFeeProjection {
            source: PublicActionFeeSource::NetworkSimulation,
            raw_gas_limit: self.raw_gas_limit,
            gas_limit: self.gas_limit,
            max_fee_per_gas: self.max_fee_per_gas,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            expected_fee_per_gas: self.expected_fee_per_gas,
            expected_gas_cost: self.expected_gas_cost,
            maximum_gas_cost: self.max_gas_cost,
            expected_native_usd_micro_value: native_usd_micro_rate.and_then(|rate| {
                native_usd_micro_value(self.expected_gas_cost, rate, native_decimals)
            }),
            maximum_native_usd_micro_value: native_usd_micro_rate
                .and_then(|rate| native_usd_micro_value(self.max_gas_cost, rate, native_decimals)),
        }
    }
}

#[must_use]
fn legacy_gas_cost_projection(
    expected_gas_units: u64,
    maximum_gas_units: u64,
    expected_gas_price: u128,
    maximum_gas_price: u128,
) -> Eip1559GasCostProjection {
    Eip1559GasCostProjection {
        expected_fee_per_gas: expected_gas_price,
        maximum_fee_per_gas: maximum_gas_price,
        expected_cost: U256::from(expected_gas_units) * U256::from(expected_gas_price),
        maximum_cost: U256::from(maximum_gas_units) * U256::from(maximum_gas_price),
    }
}

pub fn resolve_public_action_gas_fee(
    chain_id: u64,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_fee: PublicActionGasFeeSelection,
    quote: Option<PublicActionGasFeeQuote>,
) -> Result<SelfBroadcastResolvedGasFee> {
    let quote = match quote {
        Some(quote) => quote,
        None => match gas_fee {
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas, ..
            } => SelfBroadcastGasFeeQuote::from_rpc_gas_price(max_fee_per_gas),
            PublicActionGasFeeSelection::Auto => {
                return Err(eyre!("public action gas fee quote is not ready"));
            }
        },
    };
    let resolved = resolve_self_broadcast_gas_fee(gas_fee, quote)?;
    if !public_action_uses_legacy_envelope(chain_id, profile, asset, Some(quote)) {
        return Ok(resolved);
    }

    let gas_price = match gas_fee {
        PublicActionGasFeeSelection::Auto => quote.rpc_gas_price,
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas, ..
        } => max_fee_per_gas,
    };
    if gas_price == 0 {
        return Err(eyre!("legacy gas price must be greater than zero"));
    }
    Ok(SelfBroadcastResolvedGasFee {
        rpc_gas_price: quote.rpc_gas_price,
        max_fee_per_gas: gas_price,
        max_priority_fee_per_gas: 0,
    })
}
