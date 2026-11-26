//! Token review metadata uses the requesting origin's admitted reads.
use crate::rpc_broker::RpcBrokerViewCalls;
use crate::settings::CustomTokenSettings;
use crate::{DappRpcReadClient, RpcBrokerError, SensitiveUrl};
use alloy::{
    primitives::{Address, Bytes, U256},
    providers::{MULTICALL3_ADDRESS, Provider},
    sol_types::SolCall,
};
use broadcaster_core::provider::connect_reqwest_capped;

/// Alloy has no add-chain request type. Read only the required metadata here;
/// the original request, including extension fields, remains on the approval.
pub(super) fn proposed_chain(
    chain_id: u64,
    raw: &serde_json::Value,
) -> Result<crate::settings::CustomChainSettings, &'static str> {
    use crate::settings::{ChainMutation, CustomChainSettings, NativeCurrency, WalletSettings};
    // Persisted NativeCurrency rejects unknown settings fields. The Ethereum adapter
    // must accept currency extension fields as well as top-level extensions.
    #[derive(serde::Deserialize)]
    struct RequestedCurrency {
        name: String,
        symbol: String,
        decimals: u8,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Metadata {
        chain_name: String,
        native_currency: RequestedCurrency,
        rpc_urls: Vec<String>,
        #[serde(default)]
        block_explorer_urls: Vec<String>,
    }
    let value = raw
        .as_array()
        .and_then(|values| values.first())
        .ok_or("Missing chain metadata")?;
    let metadata: Metadata =
        serde_json::from_value(value.clone()).map_err(|_| "Invalid chain metadata")?;
    // This is the wallet's dapp-add policy, separate from manual Settings URLs.
    for endpoint in &metadata.rpc_urls {
        if endpoint.len() > 4096
            || !url::Url::parse(endpoint).is_ok_and(|url| url.scheme() == "https")
        {
            return Err("Dapp-proposed endpoints must use HTTPS");
        }
    }
    if metadata.block_explorer_urls.len() > crate::settings::MAX_CHAIN_ENDPOINTS {
        return Err("Too many explorer URLs");
    }
    let definition = CustomChainSettings {
        native_usd_pricing: crate::settings::NativeUsdPricing::Default,
        name: metadata.chain_name,
        native_currency: NativeCurrency {
            name: metadata.native_currency.name,
            symbol: metadata.native_currency.symbol,
            decimals: metadata.native_currency.decimals,
        },
        rpc_endpoints: metadata
            .rpc_urls
            .into_iter()
            .map(crate::settings::RpcEndpointSetting::from)
            .collect(),
        explorer_urls: metadata.block_explorer_urls,
        enabled: true,
        contracts: crate::settings::ChainContractSettings::default(),
        finality_depth: None,
        gas: crate::settings::ChainGasSettings::default(),
    };
    ChainMutation::Add {
        chain_id,
        definition: definition.clone(),
    }
    .prepare(&WalletSettings::default())
    .map_err(|_| "Invalid chain metadata")?;
    Ok(definition)
}

/// Called only after the bounded native approval enters its executing state.
/// Each endpoint must agree; a failed endpoint never leaves a partial definition.
/// Returns the common Multicall address only when its optional probe succeeds.
/// Probes go over the approval request's own client, which needs no account grant.
pub async fn verify_proposed_chain(
    operation: &crate::OperationHttpClientSlot,
    control: &crate::dapp_request::DappRequestControl,
    chain_id: u64,
    definition: &crate::settings::CustomChainSettings,
) -> Result<Option<Address>, RpcBrokerError> {
    control.ensure_current()?;
    let endpoints = definition
        .rpc_endpoints
        .iter()
        .map(|endpoint| {
            let url =
                url::Url::parse(&endpoint.url).map_err(|_| RpcBrokerError::InvalidResponse)?;
            if url.scheme() != "https" {
                return Err(RpcBrokerError::OriginRejected);
            }
            Ok(SensitiveUrl::from(url))
        })
        .collect::<Result<Vec<_>, _>>()?;
    verify_chain_endpoints(operation, control, chain_id, endpoints).await
}

async fn verify_chain_endpoints(
    operation: &crate::OperationHttpClientSlot,
    control: &crate::dapp_request::DappRequestControl,
    chain_id: u64,
    endpoints: Vec<SensitiveUrl>,
) -> Result<Option<Address>, RpcBrokerError> {
    if endpoints.is_empty() {
        return Err(RpcBrokerError::NoEndpoint { chain_id });
    }
    let client = tokio::select! {
        biased;
        () = control.cancelled() => return Err(RpcBrokerError::OriginRejected),
        client = operation.client() => client.map_err(|_| RpcBrokerError::Transport)?,
    };
    let rpc = client.rpc_client();
    // Proposed endpoints all use the wallet transport.
    let clients = crate::rpc_broker::RpcClients::new(rpc.clone(), rpc.clone());
    for endpoint in &endpoints {
        control.ensure_current()?;
        let route = crate::RpcChainRoute::new(chain_id, vec![endpoint.clone()])
            .with_identity_verification();
        tokio::select! {
            biased;
            () = control.cancelled() => return Err(RpcBrokerError::OriginRejected),
            result = route.verify_identity(&clients) => { result?; }
        }
        control.ensure_current()?;
    }
    probe_common_multicall(rpc, control, chain_id, &endpoints).await
}

async fn probe_common_multicall(
    rpc: &reqwest::Client,
    control: &crate::dapp_request::DappRequestControl,
    chain_id: u64,
    endpoints: &[SensitiveUrl],
) -> Result<Option<Address>, RpcBrokerError> {
    control.ensure_current()?;
    // Exercise the same tryAggregate entry point used by the RPC broker, with
    // a self-call that needs no account data. Every saved endpoint must support it.
    let probe = async {
        for endpoint in endpoints {
            control.ensure_current()?;
            let provider = connect_reqwest_capped(rpc.clone(), endpoint.expose_url().clone());
            let result = provider
                .multicall()
                .get_chain_id()
                .try_aggregate(false)
                .await;
            control.ensure_current()?;
            if !matches!(result, Ok((Ok(id),)) if id == U256::from(chain_id)) {
                return Ok(None);
            }
        }
        Ok(Some(MULTICALL3_ADDRESS))
    };
    // Bound the whole optional probe, rather than adding a timeout per endpoint.
    let result = tokio::select! {
        biased;
        () = control.cancelled() => return Err(RpcBrokerError::OriginRejected),
        result = tokio::time::timeout(std::time::Duration::from_secs(5), probe) => result,
    };
    control.ensure_current()?;
    result.unwrap_or(Ok(None))
}

pub async fn watch_asset_metadata(
    reads: &DappRpcReadClient,
    control: &crate::dapp_request::DappRequestControl,
    endpoint: SensitiveUrl,
    chain_id: u64,
    address: Address,
) -> Result<CustomTokenSettings, RpcBrokerError> {
    control.ensure_current()?;
    let symbol: Bytes = reads
        .eth_call(
            endpoint.clone(),
            chain_id,
            address,
            RpcBrokerViewCalls::symbolCall {}.abi_encode().into(),
        )
        .await?;
    control.ensure_current()?;
    let symbol = RpcBrokerViewCalls::symbolCall::abi_decode_returns_validate(&symbol)
        .map_err(|_| RpcBrokerError::InvalidResponse)?;
    let decimals: Bytes = reads
        .eth_call(
            endpoint,
            chain_id,
            address,
            RpcBrokerViewCalls::decimalsCall {}.abi_encode().into(),
        )
        .await?;
    control.ensure_current()?;
    let decimals = RpcBrokerViewCalls::decimalsCall::abi_decode_returns_validate(&decimals)
        .map_err(|_| RpcBrokerError::InvalidResponse)?;
    Ok(CustomTokenSettings {
        chain_id,
        token_address: address.to_string(),
        symbol,
        decimals,
        icon_path: None,
        price_anchor: None,
    })
}

#[cfg(test)]
mod tests;
