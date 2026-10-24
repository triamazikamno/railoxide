use super::{
    BTreeMap, ChainGasSettings, Deserialize, MAX_BLOCK_RANGE, MAX_FINALITY_DEPTH,
    MAX_INTERVAL_SECS, SUPPORTED_PROXY_SCHEMES, Serialize, WalletNetworkMode, supported_chain_id,
    validate_optional_address, validate_optional_range, validate_url_scheme,
};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkModeSetting {
    #[default]
    Tor,
    Proxy,
    Direct,
}

impl From<NetworkModeSetting> for WalletNetworkMode {
    fn from(value: NetworkModeSetting) -> Self {
        match value {
            NetworkModeSetting::Tor => Self::Tor,
            NetworkModeSetting::Proxy => Self::Proxy,
            NetworkModeSetting::Direct => Self::Direct,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkSettings {
    pub mode: NetworkModeSetting,
    pub proxy_url: Option<String>,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            mode: NetworkModeSetting::Tor,
            proxy_url: None,
        }
    }
}

impl NetworkSettings {
    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        match (self.mode, self.proxy_url.as_deref()) {
            (NetworkModeSetting::Proxy, Some(proxy)) => {
                validate_url_scheme("network.proxy_url", proxy, SUPPORTED_PROXY_SCHEMES, errors);
            }
            (NetworkModeSetting::Proxy, None) => {
                errors.push("network.proxy_url is required when network.mode is proxy".to_string());
            }
            (NetworkModeSetting::Tor | NetworkModeSetting::Direct, Some(_)) => {
                errors.push(
                    "network.proxy_url may only be configured when network.mode is proxy"
                        .to_string(),
                );
            }
            (NetworkModeSetting::Tor | NetworkModeSetting::Direct, None) => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ChainSettings {
    pub per_chain: BTreeMap<u64, ChainSettingsOverride>,
    pub custom: BTreeMap<u64, CustomChainSettings>,
}

impl Default for ChainSettings {
    fn default() -> Self {
        let per_chain = railgun_ui::built_in_chain_ids()
            .map(|chain_id| (chain_id, ChainSettingsOverride::default()))
            .collect();
        Self {
            per_chain,
            custom: BTreeMap::new(),
        }
    }
}

impl ChainSettings {
    #[must_use]
    pub fn enabled_chain_ids(&self) -> Vec<u64> {
        railgun_ui::built_in_chain_ids()
            .filter(|chain_id| {
                self.per_chain
                    .get(chain_id)
                    .is_none_or(|settings| settings.enabled)
            })
            .chain(
                self.custom
                    .iter()
                    .filter_map(|(id, chain)| chain.enabled.then_some(*id)),
            )
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[must_use]
    pub fn contains(&self, chain_id: u64) -> bool {
        supported_chain_id(chain_id) || self.custom.contains_key(&chain_id)
    }

    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        if self.custom.len() > super::MAX_CUSTOM_CHAINS {
            errors.push("Too many custom chains configured".to_owned());
        }
        for (chain_id, settings) in &self.per_chain {
            if !supported_chain_id(*chain_id) {
                errors.push(format!(
                    "chains.per_chain.{chain_id} has no built-in preset; provide a complete custom definition"
                ));
                continue;
            }
            settings.validate(*chain_id, errors);
        }
        for (chain_id, chain) in &self.custom {
            if supported_chain_id(*chain_id) || self.per_chain.contains_key(chain_id) {
                errors.push(format!(
                    "chains.custom.{chain_id} duplicates an existing chain"
                ));
            }
            chain.validate(*chain_id, errors);
        }
        if self.enabled_chain_ids().is_empty() {
            errors.push("chains must leave at least one supported chain enabled".to_string());
        }
    }
}

/// A same-chain oracle quoting USD per whole native coin.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum NativeUsdPricing {
    #[default]
    Default,
    Disabled,
    Oracle {
        contract_address: super::Address,
    },
}

impl NativeUsdPricing {
    pub(super) const fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }

    pub(super) const fn resolve(self, preset: Option<super::Address>) -> Option<super::Address> {
        match self {
            Self::Default => preset,
            Self::Disabled => None,
            Self::Oracle { contract_address } => Some(contract_address),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ChainSettingsOverride {
    #[serde(skip_serializing_if = "NativeUsdPricing::is_default")]
    pub native_usd_pricing: NativeUsdPricing,
    pub enabled: bool,
    pub rpc_endpoints: Vec<String>,
    /// Endpoints reached without Tor or the proxy, joined after `rpc_endpoints` or the presets.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub direct_rpc_endpoints: Vec<String>,
    pub contracts: ChainContractSettings,
    pub finality_depth: Option<u64>,
    pub gas: ChainGasSettings,
    pub railgun: RailgunSettingsOverride,
}

impl Default for ChainSettingsOverride {
    fn default() -> Self {
        Self {
            native_usd_pricing: NativeUsdPricing::Default,
            enabled: true,
            rpc_endpoints: Vec::new(),
            direct_rpc_endpoints: Vec::new(),
            contracts: ChainContractSettings::default(),
            finality_depth: None,
            gas: ChainGasSettings::default(),
            railgun: RailgunSettingsOverride::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RailgunSettingsOverride {
    pub sponsored_bundle_relays: Option<Vec<String>>,
    pub quick_sync: QuickSyncSettings,
    pub deployment: ChainDeploymentSettings,
    pub block_range: Option<u64>,
    pub poll_interval_secs: Option<u64>,
    pub indexed_wallet_block_range: Option<u64>,
}

impl ChainSettingsOverride {
    pub(super) fn validate(&self, chain_id: u64, errors: &mut Vec<String>) {
        let field = format!("chains.per_chain.{chain_id}");
        // Preserve released built-in overrides; custom-chain limits must not block startup.
        for (index, endpoint) in self.rpc_endpoints.iter().enumerate() {
            validate_url_scheme(
                &format!("{field}.rpc_endpoints[{index}]"),
                endpoint,
                &["http", "https"],
                errors,
            );
        }
        if !self.direct_rpc_endpoints.is_empty() {
            validate_endpoint_cap(
                &field,
                self.rpc_endpoints.len() + self.direct_rpc_endpoints.len(),
                errors,
            );
            validate_endpoints(
                &format!("{field}.direct_rpc_endpoints"),
                &self.direct_rpc_endpoints,
                errors,
            );
        }
        self.contracts.validate(&field, errors);
        validate_optional_range(
            &format!("{field}.finality_depth"),
            self.finality_depth,
            1,
            MAX_FINALITY_DEPTH,
            errors,
        );
        self.gas.validate(&format!("{field}.gas"), errors);
        if !railgun_ui::PUBLIC_CHAINS.contains(&chain_id) {
            self.railgun.validate(chain_id, errors);
        } else if self.railgun != RailgunSettingsOverride::default() {
            errors.push(format!(
                "{field}.railgun is not supported: chain has no Railgun deployment"
            ));
        }
    }
}

impl RailgunSettingsOverride {
    pub(super) fn validate(&self, chain_id: u64, errors: &mut Vec<String>) {
        let field = format!("chains.per_chain.{chain_id}.railgun");
        if let Some(relays) = &self.sponsored_bundle_relays {
            for (index, relay) in relays.iter().enumerate() {
                validate_url_scheme(
                    &format!("{field}.sponsored_bundle_relays[{index}]"),
                    relay,
                    &["http", "https"],
                    errors,
                );
            }
        }
        self.quick_sync.validate(chain_id, errors);
        self.deployment.validate(chain_id, errors);
        validate_optional_range(
            &format!("{field}.block_range"),
            self.block_range,
            1,
            MAX_BLOCK_RANGE,
            errors,
        );
        validate_optional_range(
            &format!("{field}.poll_interval_secs"),
            self.poll_interval_secs,
            1,
            MAX_INTERVAL_SECS,
            errors,
        );
        validate_optional_range(
            &format!("{field}.indexed_wallet_block_range"),
            self.indexed_wallet_block_range,
            1,
            MAX_BLOCK_RANGE,
            errors,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct QuickSyncSettings {
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub indexed_wallet_block_range: Option<u64>,
}

impl Default for QuickSyncSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: None,
            indexed_wallet_block_range: None,
        }
    }
}

impl QuickSyncSettings {
    pub(super) fn validate(&self, chain_id: u64, errors: &mut Vec<String>) {
        if let Some(endpoint) = self.endpoint.as_deref() {
            validate_url_scheme(
                &format!("chains.per_chain.{chain_id}.quick_sync.endpoint"),
                endpoint,
                &["http", "https"],
                errors,
            );
        }
        validate_optional_range(
            &format!("chains.per_chain.{chain_id}.quick_sync.indexed_wallet_block_range"),
            self.indexed_wallet_block_range,
            1,
            MAX_BLOCK_RANGE,
            errors,
        );
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ChainContractSettings {
    pub wrapped_native_token: Option<String>,
    pub multicall_contract: Option<String>,
}

impl ChainContractSettings {
    pub(super) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        validate_optional_address(
            &format!("{field}.contracts.wrapped_native_token"),
            self.wrapped_native_token.as_deref(),
            errors,
        );
        validate_optional_address(
            &format!("{field}.contracts.multicall_contract"),
            self.multicall_contract.as_deref(),
            errors,
        );
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ChainDeploymentSettings {
    pub archive_until_block: Option<u64>,
    pub archive_rpc_url: Option<String>,
    /// Reach `archive_rpc_url` without Tor or the proxy; no effect without an archive URL.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub archive_rpc_direct: bool,
}

impl ChainDeploymentSettings {
    pub(super) fn validate(&self, chain_id: u64, errors: &mut Vec<String>) {
        if let Some(archive_rpc_url) = self.archive_rpc_url.as_deref() {
            validate_url_scheme(
                &format!("chains.per_chain.{chain_id}.deployment.archive_rpc_url"),
                archive_rpc_url,
                &["http", "https"],
                errors,
            );
        }
    }
}

pub use railgun_ui::NativeCurrency;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CustomChainSettings {
    #[serde(default)]
    #[serde(skip_serializing_if = "NativeUsdPricing::is_default")]
    pub native_usd_pricing: NativeUsdPricing,
    pub name: String,
    pub native_currency: NativeCurrency,
    pub rpc_endpoints: Vec<String>,
    /// Endpoints reached without Tor or the proxy, joined after `rpc_endpoints`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub direct_rpc_endpoints: Vec<String>,
    #[serde(default)]
    pub explorer_urls: Vec<String>,
    pub enabled: bool,
    #[serde(default)]
    pub contracts: ChainContractSettings,
    #[serde(default)]
    pub finality_depth: Option<u64>,
    #[serde(default)]
    pub gas: ChainGasSettings,
}

/// The RPC and direct lists of one chain share the endpoint cap.
fn validate_endpoint_cap(field: &str, endpoints: usize, errors: &mut Vec<String>) {
    if endpoints > super::MAX_CHAIN_ENDPOINTS {
        errors.push(format!("{field} has too many RPC endpoints"));
    }
}

fn validate_endpoints(field: &str, endpoints: &[String], errors: &mut Vec<String>) {
    for (index, endpoint) in endpoints.iter().enumerate() {
        if endpoint.len() > 4096 {
            errors.push(format!("{field}[{index}] is too long"));
        }
        validate_url_scheme(
            &format!("{field}[{index}]"),
            endpoint,
            &["http", "https"],
            errors,
        );
    }
}

impl CustomChainSettings {
    fn validate(&self, chain_id: u64, errors: &mut Vec<String>) {
        let field = format!("chains.custom.{chain_id}");
        for (label, value) in [
            ("name", &self.name),
            ("native_currency.name", &self.native_currency.name),
            ("native_currency.symbol", &self.native_currency.symbol),
        ] {
            if value.trim().is_empty() {
                errors.push(format!("{field}.{label} must not be empty"));
            }
            if value.len() > 128 {
                errors.push(format!("{field}.{label} must be at most 128 bytes"));
            }
        }
        if self.rpc_endpoints.is_empty() {
            errors.push(format!("{field}.rpc_endpoints must not be empty"));
        }
        validate_endpoint_cap(
            &field,
            self.rpc_endpoints.len() + self.direct_rpc_endpoints.len(),
            errors,
        );
        validate_endpoints(
            &format!("{field}.rpc_endpoints"),
            &self.rpc_endpoints,
            errors,
        );
        validate_endpoints(
            &format!("{field}.direct_rpc_endpoints"),
            &self.direct_rpc_endpoints,
            errors,
        );
        for (index, url) in self.explorer_urls.iter().enumerate() {
            validate_url_scheme(
                &format!("{field}.explorer_urls[{index}]"),
                url,
                &["http", "https"],
                errors,
            );
        }
        self.contracts.validate(&field, errors);
        self.gas.validate(&format!("{field}.gas"), errors);
        validate_optional_range(
            &format!("{field}.finality_depth"),
            self.finality_depth,
            1,
            MAX_FINALITY_DEPTH,
            errors,
        );
    }
}
