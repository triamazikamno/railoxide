//! Tron's broker route, built from Tron settings. Tron never enters
//! `EffectiveChainRegistry`; reads for chain id [`super::TRON_CHAIN_ID`] use this route.

use crate::settings::TronSettings;
use crate::{RpcChainRoute, RpcEndpointRoles, RpcTransport};
use alloy::primitives::{Address, address};
use poi::SensitiveUrl;
use url::Url;

/// Multicall3 on Tron, `TEazPvZwDjDtFeJupyo7QunvnrnUjPH8ED`.
const MULTICALL3: Address = address!("0x32a4f47a74a6810bd0bf861cabab99656a75de9e");

/// Tron's route, or `None` while Tron is disabled. Building it sends nothing: each endpoint is
/// identity-checked on its first read, or when the route is activated in an identity warmup.
/// Build it once per settings installation and pass clones, so that they share those checks.
///
/// # Panics
///
/// Panics on an endpoint URL that settings validation refuses.
#[must_use]
pub fn tron_rpc_route(settings: &TronSettings) -> Option<RpcChainRoute> {
    if !settings.enabled {
        return None;
    }
    let url =
        |text: &str| SensitiveUrl::from(Url::parse(text).expect("validated Tron endpoint URL"));
    let endpoints = settings
        .effective_endpoints()
        .iter()
        .map(|endpoint| {
            let transport = if endpoint.direct {
                RpcTransport::Direct
            } else {
                RpcTransport::Wallet
            };
            let roles = RpcEndpointRoles {
                general: endpoint.general,
                sensitive: endpoint.sensitive,
                submit: endpoint.submit,
            };
            (
                url(&endpoint.json_rpc_url),
                endpoint.http_api_url.as_deref().map(url),
                transport,
                roles,
            )
        })
        .collect();
    Some(RpcChainRoute::new_tron(endpoints).with_multicall(MULTICALL3))
}
