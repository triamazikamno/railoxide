use alloy::primitives::address;

use super::network_chains::{validate_endpoint_cap, validate_endpoint_length};
use super::{Address, Deserialize, NativeUsdPricing, Serialize, Url, validate_url_scheme};

/// Chainlink TRX/USD on Tron.
const NATIVE_USD_PRESET: Address = address!("0x175fa5B93CA066D9Acd2E4c9Bf68c50865713C4b");

/// Tron's section of the settings. Tron is not an EVM chain and stays out of `chains`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TronSettings {
    pub enabled: bool,
    /// `None` uses the presets.
    pub endpoints: Option<Vec<TronEndpointSetting>>,
    pub native_usd: NativeUsdPricing,
}

impl Default for TronSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoints: None,
            native_usd: NativeUsdPricing::Default,
        }
    }
}

/// One Tron endpoint. The HTTP API serves what JSON-RPC can't, including broadcast.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TronEndpointSetting {
    pub json_rpc_url: String,
    pub http_api_url: Option<String>,
    /// Reached without Tor or the proxy.
    pub direct: bool,
    pub general: bool,
    pub sensitive: bool,
    pub submit: bool,
}

impl Default for TronEndpointSetting {
    fn default() -> Self {
        Self {
            json_rpc_url: String::new(),
            http_api_url: None,
            direct: false,
            general: true,
            sensitive: true,
            submit: true,
        }
    }
}

fn endpoint_presets() -> Vec<TronEndpointSetting> {
    [
        // Pocket
        (
            "https://tron.api.pocket.network",
            "https://tron.api.pocket.network",
        ),
        // TronGrid
        ("https://api.trongrid.io/jsonrpc", "https://api.trongrid.io"),
        // PublicNode
        (
            "https://tron-rpc.publicnode.com/jsonrpc",
            "https://tron-rpc.publicnode.com",
        ),
    ]
    .into_iter()
    .map(|(json_rpc_url, http_api_url)| TronEndpointSetting {
        json_rpc_url: json_rpc_url.to_owned(),
        http_api_url: Some(http_api_url.to_owned()),
        ..TronEndpointSetting::default()
    })
    .collect()
}

impl TronSettings {
    /// The saved endpoints, or the presets when none are saved.
    #[must_use]
    pub fn effective_endpoints(&self) -> Vec<TronEndpointSetting> {
        self.endpoints.clone().unwrap_or_else(endpoint_presets)
    }

    #[must_use]
    pub const fn native_usd_oracle(&self) -> Option<Address> {
        self.native_usd.resolve(Some(NATIVE_USD_PRESET))
    }

    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        let field = "tron.endpoints";
        let endpoints = self.effective_endpoints();
        validate_endpoint_cap(field, endpoints.len(), errors);
        // The broker finds an endpoint's roles by its parsed JSON-RPC URL, which a duplicate
        // would shadow.
        let mut json_rpc_urls: Vec<(usize, Url)> = Vec::new();
        for (index, endpoint) in endpoints.iter().enumerate() {
            let endpoint_field = format!("{field}[{index}]");
            if let Ok(url) = Url::parse(&endpoint.json_rpc_url) {
                if let Some((earlier, _)) = json_rpc_urls.iter().find(|(_, seen)| *seen == url) {
                    errors.push(format!(
                        "{endpoint_field}.json_rpc_url duplicates {field}[{earlier}]"
                    ));
                }
                json_rpc_urls.push((index, url));
            }
            for (label, url) in [
                ("json_rpc_url", Some(&endpoint.json_rpc_url)),
                ("http_api_url", endpoint.http_api_url.as_ref()),
            ] {
                let Some(url) = url else { continue };
                let url_field = format!("{endpoint_field}.{label}");
                validate_endpoint_length(&url_field, url, errors);
                validate_url_scheme(&url_field, url, &["http", "https"], errors);
            }
            if endpoint.submit && endpoint.http_api_url.is_none() {
                errors.push(format!(
                    "{endpoint_field} accepts Submit without an HTTP API URL; broadcasting needs the HTTP API"
                ));
            }
        }
        for (class, covered) in [
            ("General", endpoints.iter().any(|endpoint| endpoint.general)),
            (
                "Sensitive",
                endpoints.iter().any(|endpoint| endpoint.sensitive),
            ),
            ("Submit", endpoints.iter().any(|endpoint| endpoint.submit)),
        ] {
            if !covered {
                errors.push(format!("{field} must include a {class} endpoint"));
            }
        }
        if !endpoints
            .iter()
            .any(|endpoint| endpoint.sensitive && endpoint.http_api_url.is_some())
        {
            errors.push(format!(
                "{field}: sending needs a Sensitive endpoint with the HTTP API"
            ));
        }
    }
}
