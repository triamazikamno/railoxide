use alloy::primitives::Address;
use broadcaster_core::deployment::RailgunDeployment;
use eyre::{Result, eyre};

use super::types::PublicAssetId;
use crate::RpcChainRoute;
use crate::settings::{
    EffectiveChainConfig, EffectiveChainGasSettings, resolve_effective_chain_rpc_route,
};

pub(super) fn public_shield_token(
    asset: PublicAssetId,
    chain: &PublicChainRuntimeConfig,
) -> Result<Address> {
    chain.require_railgun()?;
    match asset {
        PublicAssetId::Native => chain
            .wrapped_native_token
            .ok_or_else(|| eyre!("selected chain does not support native shielding")),
        PublicAssetId::Erc20(token) => Ok(token),
    }
}

pub(super) struct PublicChainRuntimeConfig {
    pub(super) rpc_route: RpcChainRoute,
    pub(super) railgun: Option<RailgunDeployment>,
    pub(super) wrapped_native_token: Option<Address>,
    pub(super) finality_depth: u64,
    pub(super) gas: EffectiveChainGasSettings,
}

impl PublicChainRuntimeConfig {
    pub(super) fn require_railgun(&self) -> Result<&RailgunDeployment> {
        self.railgun
            .as_ref()
            .ok_or_else(|| eyre!("selected chain does not support Shield"))
    }
}

pub(super) fn public_chain_runtime_config(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
) -> Result<PublicChainRuntimeConfig> {
    let rpc_route = resolve_effective_chain_rpc_route(chain_id, effective_chain)?;
    Ok(PublicChainRuntimeConfig {
        rpc_route,
        railgun: effective_chain
            .railgun
            .as_ref()
            .map(|private| private.deployment),
        wrapped_native_token: effective_chain.wrapped_native_token,
        finality_depth: effective_chain.finality_depth,
        gas: effective_chain.gas.clone(),
    })
}

pub(super) async fn verified_public_chain_runtime_config(
    chain_id: u64,
    effective_chain: &EffectiveChainConfig,
    http: &crate::HttpContext,
    rpc_reads: Option<&super::DappRpcReadClient>,
) -> Result<PublicChainRuntimeConfig> {
    let mut chain = public_chain_runtime_config(chain_id, effective_chain)?;
    // Native dapp reads retain the route's verification flag and verify inside
    // gateway/broker admission, where retirement and deadlines can stop probes.
    if rpc_reads.is_none() {
        chain.rpc_route = chain.rpc_route.verify_identity(&http.rpc_client).await?;
    }
    Ok(chain)
}
