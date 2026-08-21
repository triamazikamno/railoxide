use std::time::Duration;

use super::{
    Address, BTreeMap, DEFAULT_WAKU_CLUSTER_ID, DEFAULT_WAKU_MAX_PEERS,
    DEFAULT_WAKU_PEER_CONNECTION_TIMEOUT_SECS, DEFAULT_WAKU_SHARD_ID, Deserialize,
    GAS_LIMIT_BUFFER, GAS_PRICE_BUFFER_DENOMINATOR, GAS_PRICE_BUFFER_NUMERATOR,
    IndexedArtifactSourceConfig, IndexedArtifactSourceModeSetting, MAX_INTERVAL_SECS, SensitiveUrl,
    Serialize, U256, normalize_address_string, public_balance_refresh_interval_secs,
    supported_chain_id, validate_address, validate_enr_tree, validate_optional_non_empty,
    validate_optional_range, validate_range, validate_url_scheme, validate_waku_direct_peer,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TokenSettings {
    pub built_in_overrides: Vec<BuiltInTokenOverride>,
    pub built_in_tombstones: Vec<TokenKey>,
    pub custom_tokens: Vec<CustomTokenSettings>,
    pub price_anchors: Vec<TokenPriceAnchorOverride>,
}

impl TokenSettings {
    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        for (index, override_settings) in self.built_in_overrides.iter().enumerate() {
            override_settings.validate(&format!("tokens.built_in_overrides[{index}]"), errors);
        }
        for (index, key) in self.built_in_tombstones.iter().enumerate() {
            key.validate(&format!("tokens.built_in_tombstones[{index}]"), errors);
        }
        for (index, token) in self.custom_tokens.iter().enumerate() {
            token.validate(&format!("tokens.custom_tokens[{index}]"), errors);
        }
        for (index, anchor) in self.price_anchors.iter().enumerate() {
            anchor.validate(&format!("tokens.price_anchors[{index}]"), errors);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(default, deny_unknown_fields)]
pub struct TokenKey {
    pub chain_id: u64,
    pub token_address: String,
}

impl Default for TokenKey {
    fn default() -> Self {
        Self {
            chain_id: railgun_ui::DEFAULT_CHAINS[0],
            token_address: Address::ZERO.to_string(),
        }
    }
}

impl TokenKey {
    pub(super) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        if !supported_chain_id(self.chain_id) {
            errors.push(format!("{field}.chain_id is not supported"));
        }
        validate_address(
            &format!("{field}.token_address"),
            &self.token_address,
            errors,
        );
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BuiltInTokenOverride {
    pub key: TokenKey,
    pub symbol: Option<String>,
    pub decimals: Option<u8>,
    pub icon_path: Option<String>,
    pub price_anchor: Option<PriceAnchorSettings>,
}

impl BuiltInTokenOverride {
    pub(super) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        self.key.validate(&format!("{field}.key"), errors);
        validate_optional_non_empty(&format!("{field}.symbol"), self.symbol.as_deref(), errors);
        if let Some(anchor) = &self.price_anchor {
            anchor.validate(&format!("{field}.price_anchor"), errors);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CustomTokenSettings {
    pub chain_id: u64,
    pub token_address: String,
    pub symbol: String,
    pub decimals: u8,
    pub icon_path: Option<String>,
    pub price_anchor: Option<PriceAnchorSettings>,
}

impl Default for CustomTokenSettings {
    fn default() -> Self {
        Self {
            chain_id: railgun_ui::DEFAULT_CHAINS[0],
            token_address: Address::ZERO.to_string(),
            symbol: String::new(),
            decimals: 18,
            icon_path: None,
            price_anchor: None,
        }
    }
}

impl CustomTokenSettings {
    pub(super) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        if !supported_chain_id(self.chain_id) {
            errors.push(format!("{field}.chain_id is not supported"));
        }
        validate_address(
            &format!("{field}.token_address"),
            &self.token_address,
            errors,
        );
        if self.symbol.trim().is_empty() {
            errors.push(format!("{field}.symbol must not be empty"));
        }
        if let Some(anchor) = &self.price_anchor {
            anchor.validate(&format!("{field}.price_anchor"), errors);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TokenPriceAnchorOverride {
    pub key: TokenKey,
    pub price_anchor: PriceAnchorSettings,
}

impl TokenPriceAnchorOverride {
    pub(super) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        self.key.validate(&format!("{field}.key"), errors);
        self.price_anchor
            .validate(&format!("{field}.price_anchor"), errors);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PriceAnchorSettings {
    Fixed {
        rate: String,
    },
    Oracle {
        chain_id: u64,
        oracle_address: String,
        token_decimals: u8,
        oracle_decimals: u8,
        is_inversed: bool,
    },
    UniswapV3Twap {
        pool_address: String,
        base_token_address: String,
        quote_token_address: String,
        base_token_decimals: u8,
        window_seconds: u32,
    },
    Product {
        components: Vec<Self>,
        scale_decimals: u8,
    },
}

impl Default for PriceAnchorSettings {
    fn default() -> Self {
        Self::Fixed {
            rate: "1000000000000000000".to_string(),
        }
    }
}

impl PriceAnchorSettings {
    pub(super) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        match self {
            Self::Fixed { rate } => {
                if U256::from_str_radix(rate, 10).is_err() {
                    errors.push(format!("{field}.rate must be a decimal integer"));
                }
            }
            Self::Oracle {
                chain_id,
                oracle_address,
                token_decimals,
                oracle_decimals,
                is_inversed: _,
            } => {
                if !supported_chain_id(*chain_id) {
                    errors.push(format!("{field}.chain_id is not supported"));
                }
                validate_address(&format!("{field}.oracle_address"), oracle_address, errors);
                if *token_decimals > 36 {
                    errors.push(format!("{field}.token_decimals must be at most 36"));
                }
                if *oracle_decimals > 36 {
                    errors.push(format!("{field}.oracle_decimals must be at most 36"));
                }
            }
            Self::UniswapV3Twap {
                pool_address,
                base_token_address,
                quote_token_address,
                base_token_decimals,
                window_seconds,
            } => {
                validate_address(&format!("{field}.pool_address"), pool_address, errors);
                validate_address(
                    &format!("{field}.base_token_address"),
                    base_token_address,
                    errors,
                );
                validate_address(
                    &format!("{field}.quote_token_address"),
                    quote_token_address,
                    errors,
                );
                if *base_token_decimals > 36 {
                    errors.push(format!("{field}.base_token_decimals must be at most 36"));
                }
                if *window_seconds == 0 {
                    errors.push(format!("{field}.window_seconds must be greater than 0"));
                }
            }
            Self::Product {
                components,
                scale_decimals,
            } => {
                if components.is_empty() {
                    errors.push(format!("{field}.components must not be empty"));
                }
                if *scale_decimals > 36 {
                    errors.push(format!("{field}.scale_decimals must be at most 36"));
                }
                for (index, component) in components.iter().enumerate() {
                    component.validate(&format!("{field}.components[{index}]"), errors);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GasSettings {
    pub gas_limit_buffer: u64,
    pub gas_price_buffer_numerator: u64,
    pub gas_price_buffer_denominator: u64,
}

impl Default for GasSettings {
    fn default() -> Self {
        Self {
            gas_limit_buffer: GAS_LIMIT_BUFFER,
            gas_price_buffer_numerator: GAS_PRICE_BUFFER_NUMERATOR as u64,
            gas_price_buffer_denominator: GAS_PRICE_BUFFER_DENOMINATOR as u64,
        }
    }
}

impl GasSettings {
    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        if self.gas_price_buffer_denominator == 0 {
            errors.push("gas.gas_price_buffer_denominator must be greater than 0".to_string());
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ChainGasSettings {
    pub gas_limit_buffer: Option<u64>,
    pub gas_price_buffer_numerator: Option<u64>,
    pub gas_price_buffer_denominator: Option<u64>,
}

impl ChainGasSettings {
    pub(super) fn validate(&self, field: &str, errors: &mut Vec<String>) {
        if self.gas_price_buffer_denominator == Some(0) {
            errors.push(format!(
                "{field}.gas_price_buffer_denominator must be greater than 0"
            ));
        }
    }
}

pub const DEFAULT_AUTO_LOCK_TIMEOUT_SECS: u64 = 15 * 60;
pub const MIN_AUTO_LOCK_TIMEOUT_SECS: u64 = 60;
pub const MAX_AUTO_LOCK_TIMEOUT_SECS: u64 = 24 * 60 * 60;
pub const AUTO_LOCK_TIMEOUT_PRESETS_SECS: &[u64] = &[5 * 60, 15 * 60, 30 * 60, 60 * 60];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeSettings {
    pub auto_lock_timeout_secs: Option<u64>,
    pub public_balance_refresh_interval_secs: u64,
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        Self {
            auto_lock_timeout_secs: Some(DEFAULT_AUTO_LOCK_TIMEOUT_SECS),
            public_balance_refresh_interval_secs: public_balance_refresh_interval_secs(),
        }
    }
}

impl RuntimeSettings {
    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        validate_optional_range(
            "runtime.auto_lock_timeout_secs",
            self.auto_lock_timeout_secs,
            MIN_AUTO_LOCK_TIMEOUT_SECS,
            MAX_AUTO_LOCK_TIMEOUT_SECS,
            errors,
        );
        validate_range(
            "runtime.public_balance_refresh_interval_secs",
            self.public_balance_refresh_interval_secs,
            1,
            MAX_INTERVAL_SECS,
            errors,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WakuSettings {
    pub cluster_id: u32,
    pub shard_id: u32,
    pub dns_enr_trees: Option<Vec<String>>,
    pub direct_peers: Option<Vec<WakuDirectPeerSetting>>,
    pub doh_endpoint: Option<String>,
    pub doh_fallback_endpoints: Option<Vec<String>>,
    pub max_peers: usize,
    pub peer_connection_timeout_secs: u64,
    pub nwaku_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WakuDirectPeerSetting {
    pub peer_id: String,
    pub addr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveChainConfig {
    pub chain_id: u64,
    pub enabled: bool,
    pub rpc_endpoints: Vec<String>,
    pub sponsored_bundle_relays: Vec<SensitiveUrl>,
    pub archive_rpc_url: Option<String>,
    pub quick_sync_enabled: bool,
    pub quick_sync_endpoint: Option<String>,
    pub indexed_artifact_source_mode: IndexedArtifactSourceModeSetting,
    pub indexed_artifact_source: Option<IndexedArtifactSourceConfig>,
    pub indexed_wallet_block_range: u64,
    pub deployment_block: u64,
    pub v2_start_block: u64,
    pub legacy_shield_block: u64,
    pub archive_until_block: u64,
    pub railgun_contract: String,
    pub relay_adapt_contract: String,
    pub relay_adapt_7702_contract: String,
    pub wrapped_native_token: Option<String>,
    pub multicall_contract: String,
    pub coinbase_payer: Option<Address>,
    pub finality_depth: u64,
    pub block_time: Duration,
    pub block_range: Option<u64>,
    pub poll_interval_secs: Option<u64>,
    pub gas: EffectiveChainGasSettings,
}

impl EffectiveChainConfig {
    #[must_use]
    pub const fn has_sponsorship_prerequisites(&self) -> bool {
        !self.sponsored_bundle_relays.is_empty()
            && self.wrapped_native_token.is_some()
            && self.coinbase_payer.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveChainGasSettings {
    pub gas_limit_buffer: u64,
    pub gas_price_buffer_numerator: u64,
    pub gas_price_buffer_denominator: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveTokenRegistry {
    pub tokens: BTreeMap<(u64, String), EffectiveTokenInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveTokenInfo {
    pub chain_id: u64,
    pub token_address: String,
    pub symbol: String,
    pub decimals: u8,
    pub icon_path: Option<String>,
    pub price_anchor: Option<PriceAnchorSettings>,
    pub built_in: bool,
}

impl EffectiveTokenRegistry {
    #[must_use]
    pub fn get(&self, chain_id: u64, token: &Address) -> Option<&EffectiveTokenInfo> {
        self.tokens
            .get(&(chain_id, normalize_address_string(&token.to_string())))
    }
}

impl Default for WakuSettings {
    fn default() -> Self {
        Self {
            cluster_id: DEFAULT_WAKU_CLUSTER_ID,
            shard_id: DEFAULT_WAKU_SHARD_ID,
            dns_enr_trees: None,
            direct_peers: None,
            doh_endpoint: None,
            doh_fallback_endpoints: None,
            max_peers: DEFAULT_WAKU_MAX_PEERS,
            peer_connection_timeout_secs: DEFAULT_WAKU_PEER_CONNECTION_TIMEOUT_SECS,
            nwaku_url: None,
        }
    }
}

impl WakuSettings {
    pub(super) fn validate(&self, errors: &mut Vec<String>) {
        if let Some(dns_enr_trees) = self.dns_enr_trees.as_deref() {
            for (index, tree) in dns_enr_trees.iter().enumerate() {
                validate_enr_tree(&format!("waku.dns_enr_trees[{index}]"), tree, errors);
            }
        }
        if let Some(direct_peers) = self.direct_peers.as_deref() {
            for (index, peer) in direct_peers.iter().enumerate() {
                validate_waku_direct_peer(index, peer, errors);
            }
        }
        if let Some(doh_endpoint) = self.doh_endpoint.as_deref() {
            validate_url_scheme(
                "waku.doh_endpoint",
                doh_endpoint,
                &["http", "https"],
                errors,
            );
        }
        if let Some(endpoints) = self.doh_fallback_endpoints.as_deref() {
            for (index, endpoint) in endpoints.iter().enumerate() {
                validate_url_scheme(
                    &format!("waku.doh_fallback_endpoints[{index}]"),
                    endpoint,
                    &["http", "https"],
                    errors,
                );
            }
        }
        if self.max_peers == 0 {
            errors.push("waku.max_peers must be greater than 0".to_string());
        }
        validate_range(
            "waku.peer_connection_timeout_secs",
            self.peer_connection_timeout_secs,
            1,
            MAX_INTERVAL_SECS,
            errors,
        );
        if let Some(nwaku_url) = self.nwaku_url.as_deref() {
            validate_url_scheme("waku.nwaku_url", nwaku_url, &["http", "https"], errors);
        }
    }
}
