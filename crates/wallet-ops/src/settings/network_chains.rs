use super::{
    Address, BTreeMap, ChainGasSettings, Deserialize, FromStr, MAX_BLOCK_RANGE, MAX_FINALITY_DEPTH,
    MAX_INTERVAL_SECS, RailgunDeployment, SUPPORTED_PROXY_SCHEMES, Serialize, WalletNetworkMode,
    supported_chain_id, validate_optional_address, validate_optional_range, validate_required_u64,
    validate_url_scheme,
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
        let per_chain = railgun_ui::DEFAULT_CHAINS
            .iter()
            .copied()
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
        railgun_ui::DEFAULT_CHAINS
            .iter()
            .copied()
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ChainSettingsOverride {
    pub enabled: bool,
    pub rpc_endpoints: Vec<String>,
    pub contracts: ChainContractSettings,
    pub finality_depth: Option<u64>,
    pub gas: ChainGasSettings,
    pub railgun: RailgunSettingsOverride,
}

impl Default for ChainSettingsOverride {
    fn default() -> Self {
        Self {
            enabled: true,
            rpc_endpoints: Vec::new(),
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
    pub contracts: RailgunContractSettings,
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
        self.contracts.validate(&field, errors);
        validate_optional_range(
            &format!("{field}.finality_depth"),
            self.finality_depth,
            1,
            MAX_FINALITY_DEPTH,
            errors,
        );
        self.gas.validate(&format!("{field}.gas"), errors);
        self.railgun.validate(chain_id, errors);
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
        self.contracts.validate(chain_id, errors);
        self.deployment.validate(
            chain_id,
            self.contracts
                .railgun_contract_differs_from_default(chain_id),
            errors,
        );
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
pub struct RailgunContractSettings {
    pub railgun_contract: Option<String>,
    pub relay_adapt_contract: Option<String>,
    pub relay_adapt_7702_contract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coinbase_payer: Option<String>,
}

impl RailgunContractSettings {
    #[must_use]
    pub fn railgun_contract_differs_from_default(&self, chain_id: u64) -> bool {
        let Some(contract) = self
            .railgun_contract
            .as_deref()
            .map(str::trim)
            .filter(|contract| !contract.is_empty())
        else {
            return false;
        };
        let Ok(contract) = Address::from_str(contract) else {
            return false;
        };
        let Some(defaults) = RailgunDeployment::for_chain(chain_id) else {
            return false;
        };
        contract != defaults.contract
    }

    pub(super) fn validate(&self, chain_id: u64, errors: &mut Vec<String>) {
        validate_optional_address(
            &format!("chains.per_chain.{chain_id}.contracts.railgun_contract"),
            self.railgun_contract.as_deref(),
            errors,
        );
        validate_optional_address(
            &format!("chains.per_chain.{chain_id}.contracts.relay_adapt_contract"),
            self.relay_adapt_contract.as_deref(),
            errors,
        );
        validate_optional_address(
            &format!("chains.per_chain.{chain_id}.contracts.relay_adapt_7702_contract"),
            self.relay_adapt_7702_contract.as_deref(),
            errors,
        );
        validate_optional_address(
            &format!("chains.per_chain.{chain_id}.contracts.coinbase_payer"),
            self.coinbase_payer.as_deref(),
            errors,
        );
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ChainDeploymentSettings {
    pub deployment_block: Option<u64>,
    pub v2_start_block: Option<u64>,
    pub legacy_shield_block: Option<u64>,
    pub archive_until_block: Option<u64>,
    pub archive_rpc_url: Option<String>,
}

impl ChainDeploymentSettings {
    #[must_use]
    pub const fn has_any_override(&self) -> bool {
        self.deployment_block.is_some()
            || self.v2_start_block.is_some()
            || self.legacy_shield_block.is_some()
            || self.archive_until_block.is_some()
            || self.archive_rpc_url.is_some()
    }

    pub(super) fn validate(&self, chain_id: u64, required: bool, errors: &mut Vec<String>) {
        if required {
            validate_required_u64(
                &format!("chains.per_chain.{chain_id}.deployment.deployment_block"),
                self.deployment_block,
                errors,
            );
            validate_required_u64(
                &format!("chains.per_chain.{chain_id}.deployment.v2_start_block"),
                self.v2_start_block,
                errors,
            );
            validate_required_u64(
                &format!("chains.per_chain.{chain_id}.deployment.legacy_shield_block"),
                self.legacy_shield_block,
                errors,
            );
            validate_required_u64(
                &format!("chains.per_chain.{chain_id}.deployment.archive_until_block"),
                self.archive_until_block,
                errors,
            );
        }
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

#[must_use]
pub fn should_show_chain_deployment_metadata_settings(
    chain_id: u64,
    settings: &ChainSettingsOverride,
) -> bool {
    {
        let railgun = &settings.railgun;
        railgun
            .contracts
            .railgun_contract_differs_from_default(chain_id)
            || railgun.deployment.has_any_override()
    }
}

pub use railgun_ui::NativeCurrency;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CustomChainSettings {
    pub name: String,
    pub native_currency: NativeCurrency,
    pub rpc_endpoints: Vec<String>,
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

fn validate_custom_endpoints(field: &str, endpoints: &[String], errors: &mut Vec<String>) {
    if endpoints.len() > super::MAX_CHAIN_ENDPOINTS {
        errors.push(format!("{field} has too many RPC endpoints"));
    }
    for (index, endpoint) in endpoints.iter().enumerate() {
        if endpoint.len() > 4096 {
            errors.push(format!("{field}.rpc_endpoints[{index}] is too long"));
        }
        validate_url_scheme(
            &format!("{field}.rpc_endpoints[{index}]"),
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
        validate_custom_endpoints(&field, &self.rpc_endpoints, errors);
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
