use super::{
    Address, BTreeMap, ChainContractSettings, ChainGasSettings, DEFAULT_WAKU_BACKUP_PEER_ADDR,
    DEFAULT_WAKU_BACKUP_PEER_ID, EffectiveChainConfig, EffectiveChainGasSettings,
    EffectiveTokenInfo, EffectiveTokenRegistry, FromStr, PriceAnchorSettings, RAILGUN_TREE,
    SensitiveUrl, TokenAnchorSource, TokenKey, TokenPriceAnchorOverride, Url,
    WakuDirectPeerSetting, WalletSettings, WalletSettingsValidationError,
};
use crate::RpcChainRoute;
use eyre::{Result as EyreResult, eyre};

pub const ETHEREUM_SPONSORED_BUNDLE_RELAYS: &[&str] =
    &["https://rpc.titanbuilder.xyz", "https://rpc.quasar.win"];
pub const ETHEREUM_COINBASE_PAYER: Address =
    alloy::primitives::address!("0x381787eBFD112E742fc965289c59630B2e7ce0A4");

/// Resolved chain settings, including disabled chains retained for metadata and editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveChainRegistry {
    chains: BTreeMap<u64, EffectiveChainConfig>,
}

impl FromIterator<EffectiveChainConfig> for EffectiveChainRegistry {
    fn from_iter<T: IntoIterator<Item = EffectiveChainConfig>>(chains: T) -> Self {
        Self {
            chains: chains
                .into_iter()
                .map(|chain| (chain.chain_id, chain))
                .collect(),
        }
    }
}

impl EffectiveChainRegistry {
    /// Look up configured metadata without admitting an operation.
    #[must_use]
    pub fn get(&self, chain_id: u64) -> Option<&EffectiveChainConfig> {
        self.chains.get(&chain_id)
    }

    /// Require a configured, enabled EVM chain.
    pub fn enabled(&self, chain_id: u64) -> EyreResult<&EffectiveChainConfig> {
        let chain = self.configured(chain_id)?;
        if !chain.enabled {
            return Err(eyre!("chain {chain_id} is disabled"));
        }
        Ok(chain)
    }

    /// Require an enabled chain with Railgun capability.
    pub fn railgun(&self, chain_id: u64) -> EyreResult<&EffectiveChainConfig> {
        let chain = self.configured(chain_id)?;
        chain.require_railgun()?;
        Ok(chain)
    }

    fn configured(&self, chain_id: u64) -> EyreResult<&EffectiveChainConfig> {
        self.get(chain_id)
            .ok_or_else(|| eyre!("chain {chain_id} is not configured"))
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&u64, &EffectiveChainConfig)> {
        self.chains.iter()
    }

    #[must_use]
    pub fn values(&self) -> impl ExactSizeIterator<Item = &EffectiveChainConfig> {
        self.chains.values()
    }

    #[must_use]
    pub fn keys(&self) -> impl ExactSizeIterator<Item = &u64> {
        self.chains.keys()
    }

    pub fn enabled_chains(&self) -> impl Iterator<Item = &EffectiveChainConfig> {
        self.values().filter(|chain| chain.enabled)
    }

    pub fn railgun_chains(&self) -> impl Iterator<Item = &EffectiveChainConfig> {
        self.values()
            .filter(|chain| chain.require_railgun().is_ok())
    }

    #[must_use]
    pub fn into_values(self) -> impl ExactSizeIterator<Item = EffectiveChainConfig> {
        self.chains.into_values()
    }

    #[cfg(test)]
    pub(crate) fn get_mut(&mut self, chain_id: u64) -> Option<&mut EffectiveChainConfig> {
        self.chains.get_mut(&chain_id)
    }
}

/// Resolve persisted definitions once before operational admission.
///
/// # Panics
/// Panics only if a compiled-in preset is missing or a validated value cannot be parsed.
pub fn build_effective_chain_configs(
    settings: &WalletSettings,
) -> Result<EffectiveChainRegistry, WalletSettingsValidationError> {
    settings.validate()?;
    let mut configs = BTreeMap::new();
    for &chain_id in railgun_ui::DEFAULT_CHAINS {
        let preset = super::presets::EvmPreset::for_chain(chain_id).expect("built-in EVM preset");
        let defaults = super::ChainSettingsOverride::default();
        let saved = settings
            .chains
            .per_chain
            .get(&chain_id)
            .unwrap_or(&defaults);
        let mut config = built_in_chain_config(chain_id, preset, saved, settings);
        let private = &saved.railgun;
        let mut deployment =
            super::RailgunDeployment::for_chain(chain_id).expect("built-in Railgun deployment");
        deployment.contract = parse_address(private.contracts.railgun_contract.as_deref())
            .unwrap_or(deployment.contract);
        deployment.relay_adapt_contract =
            parse_address(private.contracts.relay_adapt_contract.as_deref())
                .unwrap_or(deployment.relay_adapt_contract);
        deployment.relay_adapt_7702_contract =
            parse_address(private.contracts.relay_adapt_7702_contract.as_deref())
                .unwrap_or(deployment.relay_adapt_7702_contract);
        deployment.deployment_block = private
            .deployment
            .deployment_block
            .unwrap_or(deployment.deployment_block);
        deployment.v2_start_block = private
            .deployment
            .v2_start_block
            .unwrap_or(deployment.v2_start_block);
        deployment.legacy_shield_block = private
            .deployment
            .legacy_shield_block
            .unwrap_or(deployment.legacy_shield_block);
        let mut sync = super::RailgunSyncOptions::for_chain(
            chain_id,
            private
                .block_range
                .unwrap_or(crate::desktop::DEFAULT_BLOCK_RANGE),
            private.poll_interval_secs.map_or(
                crate::desktop::DEFAULT_POLL_INTERVAL,
                std::time::Duration::from_secs,
            ),
        )
        .expect("built-in sync preset");
        sync.archive_until_block = private
            .deployment
            .archive_until_block
            .unwrap_or(sync.archive_until_block);
        sync.indexed_wallet_block_range = private
            .quick_sync
            .indexed_wallet_block_range
            .or(private.indexed_wallet_block_range)
            .unwrap_or(sync.indexed_wallet_block_range);
        sync.quick_sync_endpoint = if private.quick_sync.enabled {
            private
                .quick_sync
                .endpoint
                .as_deref()
                .map(|url| Url::parse(url).expect("validated quick-sync endpoint"))
                .or(sync.quick_sync_endpoint)
        } else {
            None
        };
        sync.indexed_artifact_source = settings
            .indexed_artifacts
            .source_config(&settings.poi.artifact.gateway_urls)
            .map(|source| sync_service::IndexedArtifactSourceConfig {
                trusted_publisher_pubkey: source.trusted_publisher_pubkey,
                manifest_source: match source.manifest_source {
                    super::IndexedArtifactManifestSource::Url(url) => {
                        sync_service::IndexedArtifactManifestSource::Url(url)
                    }
                    super::IndexedArtifactManifestSource::Cid(cid) => {
                        sync_service::IndexedArtifactManifestSource::Cid(cid)
                    }
                    super::IndexedArtifactManifestSource::IpnsName(name) => {
                        sync_service::IndexedArtifactManifestSource::IpnsName(name)
                    }
                },
                gateway_urls: source.gateway_urls,
                gateway_pool: None,
                manifest_reuse: sync_service::IndexedArtifactManifestReuse::default(),
                max_manifest_age: source.max_manifest_age,
                concurrency: source.concurrency,
                max_in_flight_bytes: source.max_in_flight_bytes,
            });
        config.railgun = Some(super::EffectiveRailgunConfig {
            deployment,
            sync,
            archive_rpc_url: private
                .deployment
                .archive_rpc_url
                .as_deref()
                .map(|url| SensitiveUrl::from(Url::parse(url).expect("validated archive URL"))),
            indexed_artifact_source_mode: settings.indexed_artifacts.source_mode,
            sponsored_bundle_relays: private.sponsored_bundle_relays.as_ref().map_or_else(
                || default_sponsored_bundle_relays(chain_id),
                |relays| {
                    relays
                        .iter()
                        .map(|url| {
                            SensitiveUrl::from(Url::parse(url).expect("validated relay URL"))
                        })
                        .collect()
                },
            ),
            coinbase_payer: parse_address(private.contracts.coinbase_payer.as_deref())
                .or_else(|| default_coinbase_payer(chain_id)),
        });
        configs.insert(chain_id, config);
    }
    for &chain_id in railgun_ui::PUBLIC_CHAINS {
        let preset = super::presets::EvmPreset::for_chain(chain_id).expect("built-in EVM preset");
        let defaults = super::ChainSettingsOverride::default();
        let saved = settings
            .chains
            .per_chain
            .get(&chain_id)
            .unwrap_or(&defaults);
        configs.insert(
            chain_id,
            built_in_chain_config(chain_id, preset, saved, settings),
        );
    }
    for (&chain_id, saved) in &settings.chains.custom {
        configs.insert(
            chain_id,
            EffectiveChainConfig {
                chain_id,
                native_usd_oracle: saved.native_usd_pricing.resolve(None),
                name: saved.name.clone(),
                native_currency: saved.native_currency.clone(),
                explorer_urls: saved.explorer_urls.clone(),
                built_in: false,
                enabled: saved.enabled,
                rpc_route: route(
                    chain_id,
                    &saved.rpc_endpoints,
                    parse_address(saved.contracts.multicall_contract.as_deref()),
                )
                .with_identity_verification(),
                wrapped_native_token: parse_address(
                    saved.contracts.wrapped_native_token.as_deref(),
                ),
                finality_depth: saved.finality_depth.unwrap_or(12),
                block_time: None,
                gas: resolved_gas(&saved.gas, settings),
                railgun: None,
            },
        );
    }
    Ok(EffectiveChainRegistry { chains: configs })
}

/// Shared resolution for every built-in preset. Railgun capability is layered on afterwards.
fn built_in_chain_config(
    chain_id: u64,
    preset: super::presets::EvmPreset,
    saved: &super::ChainSettingsOverride,
    settings: &WalletSettings,
) -> EffectiveChainConfig {
    let rpc_values = if saved.rpc_endpoints.is_empty() {
        &preset.rpc_endpoints
    } else {
        &saved.rpc_endpoints
    };
    let mut rpc_route = route(
        chain_id,
        rpc_values,
        parse_address(saved.contracts.multicall_contract.as_deref()).or(preset.multicall),
    );
    if !saved.rpc_endpoints.is_empty() {
        rpc_route = rpc_route.with_identity_verification();
    }
    EffectiveChainConfig {
        chain_id,
        native_usd_oracle: saved.native_usd_pricing.resolve(preset.native_usd_oracle),
        name: preset.name.to_owned(),
        native_currency: preset.native_currency,
        explorer_urls: preset
            .explorer_urls
            .iter()
            .map(|url| (*url).to_owned())
            .collect(),
        built_in: true,
        enabled: saved.enabled,
        rpc_route,
        wrapped_native_token: parse_address(saved.contracts.wrapped_native_token.as_deref())
            .or(preset.wrapped_native),
        finality_depth: saved.finality_depth.unwrap_or(preset.finality_depth),
        block_time: Some(preset.block_time),
        gas: resolved_gas(&saved.gas, settings),
        railgun: None,
    }
}

fn parse_address(value: Option<&str>) -> Option<Address> {
    value.map(|value| value.parse().expect("validated contract address"))
}

fn route(chain_id: u64, endpoints: &[String], multicall: Option<Address>) -> RpcChainRoute {
    let route = RpcChainRoute::new(
        chain_id,
        endpoints
            .iter()
            .map(|url| SensitiveUrl::from(Url::parse(url).expect("validated RPC URL")))
            .collect(),
    );
    if let Some(contract) = multicall {
        route.with_multicall(contract)
    } else {
        route
    }
}

fn resolved_gas(saved: &ChainGasSettings, settings: &WalletSettings) -> EffectiveChainGasSettings {
    EffectiveChainGasSettings {
        gas_limit_buffer: saved
            .gas_limit_buffer
            .unwrap_or(settings.gas.gas_limit_buffer),
        gas_price_buffer_numerator: saved
            .gas_price_buffer_numerator
            .unwrap_or(settings.gas.gas_price_buffer_numerator),
        gas_price_buffer_denominator: saved
            .gas_price_buffer_denominator
            .unwrap_or(settings.gas.gas_price_buffer_denominator),
    }
}

#[must_use]
pub fn default_chain_rpc_endpoints(chain_id: u64) -> Option<Vec<String>> {
    super::presets::EvmPreset::for_chain(chain_id).map(|preset| preset.rpc_endpoints)
}

#[must_use]
pub fn default_chain_rpc_route(chain_id: u64) -> Option<RpcChainRoute> {
    build_effective_chain_configs(&WalletSettings::default())
        .ok()?
        .get(chain_id)
        .map(|chain| chain.rpc_route.clone())
}

/// Validate the selected authoritative configuration before routing an operation.
pub fn resolve_effective_chain_rpc_route(
    requested_chain_id: u64,
    chain: &EffectiveChainConfig,
) -> EyreResult<RpcChainRoute> {
    if !chain.enabled {
        return Err(eyre!("chain {requested_chain_id} is disabled"));
    }
    if chain.chain_id != requested_chain_id || chain.rpc_route.chain_id() != requested_chain_id {
        return Err(eyre!(
            "effective configuration does not match chain {requested_chain_id}"
        ));
    }
    if chain.rpc_route.endpoints().is_empty() {
        return Err(eyre!("chain {requested_chain_id} has no RPC endpoints"));
    }
    Ok(chain.rpc_route.clone())
}

#[must_use]
pub fn default_sponsored_bundle_relay_endpoints(chain_id: u64) -> Vec<String> {
    if chain_id == 1 {
        ETHEREUM_SPONSORED_BUNDLE_RELAYS
            .iter()
            .map(ToString::to_string)
            .collect()
    } else {
        Vec::new()
    }
}

#[must_use]
/// Returns built-in sponsored relays within redacted runtime URL boundaries.
///
/// # Panics
///
/// Panics if a compiled-in sponsored relay URL is invalid.
pub fn default_sponsored_bundle_relays(chain_id: u64) -> Vec<SensitiveUrl> {
    default_sponsored_bundle_relay_endpoints(chain_id)
        .into_iter()
        .map(|relay| {
            SensitiveUrl::from(Url::parse(&relay).expect("built-in sponsored relay URL is valid"))
        })
        .collect()
}

#[must_use]
/// Returns the reviewed built-in coinbase payer for a supported deployment.
pub const fn default_coinbase_payer(chain_id: u64) -> Option<Address> {
    match chain_id {
        1 => Some(ETHEREUM_COINBASE_PAYER),
        _ => None,
    }
}

#[must_use]
pub fn default_chain_quick_sync_endpoint(chain_id: u64) -> Option<String> {
    super::RailgunSyncOptions::for_chain(
        chain_id,
        crate::desktop::DEFAULT_BLOCK_RANGE,
        crate::desktop::DEFAULT_POLL_INTERVAL,
    )
    .and_then(|options| options.quick_sync_endpoint)
    .map(|url| url.to_string())
}

#[must_use]
pub fn default_waku_dns_enr_trees() -> Vec<String> {
    vec![RAILGUN_TREE.to_string()]
}

#[must_use]
pub const fn default_waku_direct_peers() -> Vec<WakuDirectPeerSetting> {
    Vec::new()
}

#[must_use]
pub fn default_waku_backup_peers() -> Vec<WakuDirectPeerSetting> {
    vec![WakuDirectPeerSetting {
        peer_id: DEFAULT_WAKU_BACKUP_PEER_ID.to_string(),
        addr: DEFAULT_WAKU_BACKUP_PEER_ADDR.to_string(),
    }]
}

#[must_use]
pub fn default_chain_contract_settings(chain_id: u64) -> Option<ChainContractSettings> {
    let preset = super::presets::EvmPreset::for_chain(chain_id)?;
    Some(ChainContractSettings {
        wrapped_native_token: preset.wrapped_native.map(|address| address.to_string()),
        multicall_contract: preset.multicall.map(|address| address.to_string()),
    })
}

#[must_use]
pub fn default_token_price_anchor(chain_id: u64, token: &Address) -> Option<PriceAnchorSettings> {
    railgun_ui::lookup_token(chain_id, token)
        .and_then(|token| price_anchor_from_static_sources(chain_id, token.anchor_sources))
}

#[must_use]
pub fn default_token_price_anchor_overrides() -> Vec<TokenPriceAnchorOverride> {
    railgun_ui::token_anchor_entries()
        .filter_map(|entry| {
            let price_anchor =
                price_anchor_from_static_sources(entry.chain_id, entry.anchor_sources)?;
            Some(TokenPriceAnchorOverride {
                key: TokenKey {
                    chain_id: entry.chain_id,
                    token_address: entry.token.to_string(),
                },
                price_anchor,
            })
        })
        .collect()
}

pub fn build_effective_token_registry(
    settings: &WalletSettings,
) -> Result<EffectiveTokenRegistry, WalletSettingsValidationError> {
    settings.validate()?;
    let mut tokens = BTreeMap::new();
    for chain_id in railgun_ui::built_in_chain_ids() {
        for token in railgun_ui::known_tokens_for_chain(chain_id) {
            tokens.insert(
                (chain_id, normalize_address_string(&token.token.to_string())),
                EffectiveTokenInfo {
                    chain_id,
                    token_address: token.token.to_string(),
                    symbol: token.symbol.to_string(),
                    decimals: token.decimals,
                    icon_path: None,
                    price_anchor: price_anchor_from_static_sources(chain_id, token.anchor_sources),
                    built_in: true,
                },
            );
        }
    }

    for tombstone in &settings.tokens.built_in_tombstones {
        tokens.remove(&token_key_tuple(tombstone));
    }

    for override_settings in &settings.tokens.built_in_overrides {
        if let Some(token) = tokens.get_mut(&token_key_tuple(&override_settings.key)) {
            if let Some(symbol) = override_settings.symbol.as_ref() {
                token.symbol.clone_from(symbol);
            }
            if let Some(decimals) = override_settings.decimals {
                token.decimals = decimals;
            }
            if let Some(icon_path) = override_settings.icon_path.as_ref() {
                token.icon_path = Some(icon_path.clone());
            }
            if let Some(anchor) = override_settings.price_anchor.as_ref() {
                token.price_anchor = Some(anchor.clone());
            }
        }
    }

    for custom in &settings.tokens.custom_tokens {
        tokens.insert(
            (
                custom.chain_id,
                normalize_address_string(&custom.token_address),
            ),
            EffectiveTokenInfo {
                chain_id: custom.chain_id,
                token_address: custom.token_address.clone(),
                symbol: custom.symbol.clone(),
                decimals: custom.decimals,
                icon_path: custom.icon_path.clone(),
                price_anchor: custom.price_anchor.clone(),
                built_in: false,
            },
        );
    }

    for anchor in &settings.tokens.price_anchors {
        if let Some(token) = tokens.get_mut(&token_key_tuple(&anchor.key)) {
            token.price_anchor = Some(anchor.price_anchor.clone());
        }
    }

    Ok(EffectiveTokenRegistry { tokens })
}

pub(super) fn supported_chain_id(chain_id: u64) -> bool {
    railgun_ui::is_built_in_chain(chain_id)
}

fn token_key_tuple(key: &TokenKey) -> (u64, String) {
    (key.chain_id, normalize_address_string(&key.token_address))
}

pub(super) fn normalize_address_string(address: &str) -> String {
    Address::from_str(address).map_or_else(
        |_| address.to_ascii_lowercase(),
        |address| address.to_string().to_ascii_lowercase(),
    )
}

fn price_anchor_from_static_sources(
    chain_id: u64,
    sources: &[TokenAnchorSource],
) -> Option<PriceAnchorSettings> {
    let [source] = sources else {
        return None;
    };
    price_anchor_from_static_source(chain_id, source)
}

fn price_anchor_from_static_source(
    chain_id: u64,
    source: &TokenAnchorSource,
) -> Option<PriceAnchorSettings> {
    match source {
        TokenAnchorSource::Fixed {
            token_fee_per_unit_gas,
        } => Some(PriceAnchorSettings::Fixed {
            rate: token_fee_per_unit_gas.to_string(),
        }),
        TokenAnchorSource::ChainlinkOracle {
            addr,
            token_decimals,
            oracle_decimals,
            is_inversed,
        } => Some(PriceAnchorSettings::Oracle {
            chain_id,
            oracle_address: addr.to_string(),
            token_decimals: *token_decimals,
            oracle_decimals: *oracle_decimals,
            is_inversed: *is_inversed,
        }),
        TokenAnchorSource::UniswapV3Twap {
            pool,
            base_token,
            quote_token,
            base_token_decimals,
            window_seconds,
        } => Some(PriceAnchorSettings::UniswapV3Twap {
            pool_address: pool.to_string(),
            base_token_address: base_token.to_string(),
            quote_token_address: quote_token.to_string(),
            base_token_decimals: *base_token_decimals,
            window_seconds: *window_seconds,
        }),
        TokenAnchorSource::Product {
            sources,
            scale_decimals,
        } => Some(PriceAnchorSettings::Product {
            components: sources
                .iter()
                .map(|source| price_anchor_from_static_source(chain_id, source))
                .collect::<Option<Vec<_>>>()?,
            scale_decimals: *scale_decimals,
        }),
    }
}

#[must_use]
pub fn default_railgun_contract_settings(chain_id: u64) -> Option<super::RailgunContractSettings> {
    let deployment = super::RailgunDeployment::for_chain(chain_id)?;
    Some(super::RailgunContractSettings {
        railgun_contract: Some(deployment.contract.to_string()),
        relay_adapt_contract: Some(deployment.relay_adapt_contract.to_string()),
        relay_adapt_7702_contract: Some(deployment.relay_adapt_7702_contract.to_string()),
        coinbase_payer: default_coinbase_payer(chain_id).map(|address| address.to_string()),
    })
}
