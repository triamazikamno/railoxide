use std::str::FromStr;

use alloy::primitives::Address;
use eyre::{Result, WrapErr, eyre};
use sync_service::ChainConfigDefaults;

use super::types::PublicAssetId;
use crate::amounts::wrapped_native_token_for_chain;
use crate::settings::{
    EffectiveChainConfig, EffectiveChainGasSettings, resolve_effective_chain_rpc_route,
};
use crate::{GAS_LIMIT_BUFFER, RpcChainRoute, chain_defaults_for_chain};

pub(super) fn public_shield_token(
    asset: PublicAssetId,
    chain: &PublicChainRuntimeConfig,
) -> Result<Address> {
    match asset {
        PublicAssetId::Native => chain
            .wrapped_native_token
            .ok_or_else(|| eyre!("selected chain does not support native shielding")),
        PublicAssetId::Erc20(token) => Ok(token),
    }
}

pub(super) struct PublicChainRuntimeConfig {
    pub(super) rpc_route: RpcChainRoute,
    pub(super) railgun_contract: Address,
    pub(super) relay_adapt_contract: Address,
    pub(super) wrapped_native_token: Option<Address>,
    pub(super) finality_depth: u64,
    pub(super) gas: EffectiveChainGasSettings,
}

pub(super) fn public_chain_runtime_config(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
) -> Result<PublicChainRuntimeConfig> {
    let defaults = chain_defaults_for_public_chain(chain_id)?;
    let rpc_route = resolve_effective_chain_rpc_route(chain_id, effective_chain)?;
    let Some(effective_chain) = effective_chain else {
        return Ok(PublicChainRuntimeConfig {
            rpc_route,
            railgun_contract: defaults.contract,
            relay_adapt_contract: defaults.relay_adapt_contract,
            wrapped_native_token: wrapped_native_token_for_chain(chain_id),
            finality_depth: defaults.finality_depth,
            gas: EffectiveChainGasSettings {
                gas_limit_buffer: GAS_LIMIT_BUFFER,
                gas_price_buffer_numerator: crate::GAS_PRICE_BUFFER_NUMERATOR as u64,
                gas_price_buffer_denominator: crate::GAS_PRICE_BUFFER_DENOMINATOR as u64,
            },
        });
    };
    if !effective_chain.enabled {
        return Err(eyre!("chain {chain_id} is disabled in wallet settings"));
    }
    let railgun_contract =
        parse_effective_address("railgun contract", &effective_chain.railgun_contract)?;
    let relay_adapt_contract = parse_effective_address(
        "relay adapt contract",
        &effective_chain.relay_adapt_contract,
    )?;
    let wrapped_native_token = effective_chain
        .wrapped_native_token
        .as_deref()
        .map(|value| parse_effective_address("wrapped native token", value))
        .transpose()?
        .or_else(|| wrapped_native_token_for_chain(chain_id));
    Ok(PublicChainRuntimeConfig {
        rpc_route,
        railgun_contract,
        relay_adapt_contract,
        wrapped_native_token,
        finality_depth: effective_chain.finality_depth,
        gas: effective_chain.gas.clone(),
    })
}

fn parse_effective_address(label: &str, value: &str) -> Result<Address> {
    Address::from_str(value).wrap_err_with(|| format!("parse effective {label} address"))
}

pub(super) fn chain_defaults_for_public_chain(chain_id: u64) -> Result<ChainConfigDefaults> {
    chain_defaults_for_chain(chain_id)
}
