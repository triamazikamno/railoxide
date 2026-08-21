use super::{
    Address, BTreeMap, ChainConfigDefaults, ChainContractSettings, ChainDeploymentSettings,
    ChainGasSettings, DEFAULT_WAKU_DIRECT_PEER_ADDR, DEFAULT_WAKU_DIRECT_PEER_ID,
    EffectiveChainConfig, EffectiveChainGasSettings, EffectiveTokenInfo, EffectiveTokenRegistry,
    FromStr, PriceAnchorSettings, QuickSyncSettings, RAILGUN_TREE, SensitiveUrl, TokenAnchorSource,
    TokenKey, TokenPriceAnchorOverride, Url, WakuDirectPeerSetting, WalletSettings,
    WalletSettingsValidationError,
};

pub const ETHEREUM_SPONSORED_BUNDLE_RELAYS: &[&str] =
    &["https://rpc.titanbuilder.xyz", "https://rpc.quasar.win"];
pub const ETHEREUM_COINBASE_PAYER: Address =
    alloy::primitives::address!("0x381787eBFD112E742fc965289c59630B2e7ce0A4");

/// Builds validated runtime chain configuration.
///
/// # Panics
///
/// Panics only if a value accepted by settings validation cannot be parsed identically here.
pub fn build_effective_chain_configs(
    settings: &WalletSettings,
) -> Result<BTreeMap<u64, EffectiveChainConfig>, WalletSettingsValidationError> {
    settings.validate()?;
    let mut configs = BTreeMap::new();
    for chain_id in railgun_ui::DEFAULT_CHAINS {
        let Some(defaults) = ChainConfigDefaults::for_chain(*chain_id) else {
            return Err(WalletSettingsValidationError::new(vec![format!(
                "chains.per_chain.{chain_id} is not supported"
            )]));
        };
        let override_settings = settings.chains.per_chain.get(chain_id);
        let enabled = override_settings.is_none_or(|settings| settings.enabled);
        let rpc_endpoints = override_settings
            .filter(|settings| !settings.rpc_endpoints.is_empty())
            .map_or_else(
                || defaults.rpc_urls.iter().map(ToString::to_string).collect(),
                |settings| settings.rpc_endpoints.clone(),
            );
        let sponsored_bundle_relays = override_settings
            .and_then(|settings| settings.sponsored_bundle_relays.as_ref())
            .map_or_else(
                || default_sponsored_bundle_relays(*chain_id),
                |relays| {
                    relays
                        .iter()
                        .map(|relay| {
                            SensitiveUrl::from(
                                Url::parse(relay).expect("validated sponsored relay URL"),
                            )
                        })
                        .collect()
                },
            );
        let quick_sync_default = QuickSyncSettings::default();
        let quick_sync =
            override_settings.map_or(&quick_sync_default, |settings| &settings.quick_sync);
        let contracts_default = ChainContractSettings::default();
        let contracts =
            override_settings.map_or(&contracts_default, |settings| &settings.contracts);
        let deployment_default = ChainDeploymentSettings::default();
        let deployment =
            override_settings.map_or(&deployment_default, |settings| &settings.deployment);
        let gas_default = ChainGasSettings::default();
        let gas = override_settings.map_or(&gas_default, |settings| &settings.gas);
        let indexed_artifact_source = settings.indexed_artifacts.source_config();
        configs.insert(
            *chain_id,
            EffectiveChainConfig {
                chain_id: *chain_id,
                enabled,
                rpc_endpoints,
                sponsored_bundle_relays,
                archive_rpc_url: deployment.archive_rpc_url.clone(),
                quick_sync_enabled: quick_sync.enabled,
                quick_sync_endpoint: quick_sync.endpoint.clone().or_else(|| {
                    defaults
                        .quick_sync_endpoint
                        .as_ref()
                        .map(ToString::to_string)
                }),
                indexed_artifact_source_mode: settings.indexed_artifacts.source_mode,
                indexed_artifact_source: indexed_artifact_source.clone(),
                indexed_wallet_block_range: quick_sync
                    .indexed_wallet_block_range
                    .or_else(|| {
                        override_settings.and_then(|settings| settings.indexed_wallet_block_range)
                    })
                    .unwrap_or(defaults.indexed_wallet_block_range),
                deployment_block: deployment
                    .deployment_block
                    .unwrap_or(defaults.deployment_block),
                v2_start_block: deployment.v2_start_block.unwrap_or(defaults.v2_start_block),
                legacy_shield_block: deployment
                    .legacy_shield_block
                    .unwrap_or(defaults.legacy_shield_block),
                archive_until_block: deployment
                    .archive_until_block
                    .unwrap_or(defaults.archive_until_block),
                railgun_contract: contracts
                    .railgun_contract
                    .clone()
                    .unwrap_or_else(|| defaults.contract.to_string()),
                relay_adapt_contract: contracts
                    .relay_adapt_contract
                    .clone()
                    .unwrap_or_else(|| defaults.relay_adapt_contract.to_string()),
                relay_adapt_7702_contract: contracts
                    .relay_adapt_7702_contract
                    .clone()
                    .unwrap_or_else(|| defaults.relay_adapt_7702_contract.to_string()),
                wrapped_native_token: contracts.wrapped_native_token.clone().or_else(|| {
                    crate::amounts::wrapped_native_token_for_chain(*chain_id)
                        .map(|token| token.to_string())
                }),
                multicall_contract: contracts
                    .multicall_contract
                    .clone()
                    .unwrap_or_else(|| defaults.multicall_contract.to_string()),
                coinbase_payer: contracts
                    .coinbase_payer
                    .as_deref()
                    .map(|payer| Address::from_str(payer).expect("validated coinbase payer"))
                    .or_else(|| default_coinbase_payer(*chain_id)),
                finality_depth: override_settings
                    .and_then(|settings| settings.finality_depth)
                    .unwrap_or(defaults.finality_depth),
                block_time: defaults.block_time,
                block_range: override_settings.and_then(|settings| settings.block_range),
                poll_interval_secs: override_settings
                    .and_then(|settings| settings.poll_interval_secs),
                gas: EffectiveChainGasSettings {
                    gas_limit_buffer: gas
                        .gas_limit_buffer
                        .unwrap_or(settings.gas.gas_limit_buffer),
                    gas_price_buffer_numerator: gas
                        .gas_price_buffer_numerator
                        .unwrap_or(settings.gas.gas_price_buffer_numerator),
                    gas_price_buffer_denominator: gas
                        .gas_price_buffer_denominator
                        .unwrap_or(settings.gas.gas_price_buffer_denominator),
                },
            },
        );
    }
    Ok(configs)
}

#[must_use]
pub fn default_chain_rpc_endpoints(chain_id: u64) -> Option<Vec<String>> {
    ChainConfigDefaults::for_chain(chain_id)
        .map(|defaults| defaults.rpc_urls.iter().map(ToString::to_string).collect())
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
    ChainConfigDefaults::for_chain(chain_id)
        .and_then(|defaults| defaults.quick_sync_endpoint)
        .map(|endpoint| endpoint.to_string())
}

#[must_use]
pub fn default_waku_dns_enr_trees() -> Vec<String> {
    vec![RAILGUN_TREE.to_string()]
}

#[must_use]
pub fn default_waku_direct_peers() -> Vec<WakuDirectPeerSetting> {
    vec![WakuDirectPeerSetting {
        peer_id: DEFAULT_WAKU_DIRECT_PEER_ID.to_string(),
        addr: DEFAULT_WAKU_DIRECT_PEER_ADDR.to_string(),
    }]
}

#[must_use]
pub fn default_chain_contract_settings(chain_id: u64) -> Option<ChainContractSettings> {
    let defaults = ChainConfigDefaults::for_chain(chain_id)?;
    Some(ChainContractSettings {
        railgun_contract: Some(defaults.contract.to_string()),
        relay_adapt_contract: Some(defaults.relay_adapt_contract.to_string()),
        relay_adapt_7702_contract: Some(defaults.relay_adapt_7702_contract.to_string()),
        wrapped_native_token: crate::amounts::wrapped_native_token_for_chain(chain_id)
            .map(|token| token.to_string()),
        multicall_contract: Some(defaults.multicall_contract.to_string()),
        coinbase_payer: default_coinbase_payer(chain_id).map(|payer| payer.to_string()),
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
    for chain_id in railgun_ui::DEFAULT_CHAINS {
        for token in railgun_ui::known_tokens_for_chain(*chain_id) {
            tokens.insert(
                (
                    *chain_id,
                    normalize_address_string(&token.token.to_string()),
                ),
                EffectiveTokenInfo {
                    chain_id: *chain_id,
                    token_address: token.token.to_string(),
                    symbol: token.symbol.to_string(),
                    decimals: token.decimals,
                    icon_path: None,
                    price_anchor: price_anchor_from_static_sources(*chain_id, token.anchor_sources),
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
    railgun_ui::DEFAULT_CHAINS.contains(&chain_id)
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
