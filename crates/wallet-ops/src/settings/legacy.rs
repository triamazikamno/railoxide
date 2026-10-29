//! Frozen released shapes for versions 1 through 6 and versions 7 through 10. Keep these record
//! shapes independent of current chains.

use super::{
    BTreeMap, ChainGasSettings, CustomChainSettings, Deserialize, GasSettings,
    IndexedArtifactSettings, NativeUsdPricing, NetworkSettings, PoiSettings, PrivacySettings,
    PublicBroadcasterSettings, QuickSyncSettings, RuntimeSettings, Serialize, TokenSettings,
    WALLET_SETTINGS_VERSION, WakuSettings, WalletConnectSettings,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ChainSettings {
    pub per_chain: BTreeMap<u64, ChainSettingsOverride>,
}

impl Default for ChainSettings {
    fn default() -> Self {
        let per_chain = [1, 56, 137, 42161]
            .into_iter()
            .map(|chain_id| (chain_id, ChainSettingsOverride::default()))
            .collect();
        Self { per_chain }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ChainSettingsOverride {
    pub enabled: bool,
    pub rpc_endpoints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sponsored_bundle_relays: Option<Vec<String>>,
    pub quick_sync: QuickSyncSettings,
    pub contracts: ChainContractSettings,
    pub deployment: ChainDeploymentSettings,
    pub finality_depth: Option<u64>,
    pub block_range: Option<u64>,
    pub poll_interval_secs: Option<u64>,
    pub indexed_wallet_block_range: Option<u64>,
    pub gas: ChainGasSettings,
}

impl Default for ChainSettingsOverride {
    fn default() -> Self {
        Self {
            enabled: true,
            rpc_endpoints: Vec::new(),
            sponsored_bundle_relays: None,
            quick_sync: QuickSyncSettings::default(),
            contracts: ChainContractSettings::default(),
            deployment: ChainDeploymentSettings::default(),
            finality_depth: None,
            block_range: None,
            poll_interval_secs: None,
            indexed_wallet_block_range: None,
            gas: ChainGasSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ChainContractSettings {
    pub railgun_contract: Option<String>,
    pub relay_adapt_contract: Option<String>,
    pub relay_adapt_7702_contract: Option<String>,
    pub wrapped_native_token: Option<String>,
    pub multicall_contract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coinbase_payer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct LegacyWalletSettings {
    pub version: u32,
    pub network: NetworkSettings,
    pub privacy: PrivacySettings,
    pub chains: ChainSettings,
    pub indexed_artifacts: IndexedArtifactSettings,
    pub poi: PoiSettings,
    pub broadcaster: PublicBroadcasterSettings,
    pub tokens: TokenSettings,
    pub gas: GasSettings,
    pub runtime: RuntimeSettings,
    pub waku: WakuSettings,
    pub walletconnect: WalletConnectSettings,
}

impl Default for LegacyWalletSettings {
    fn default() -> Self {
        Self {
            version: 6,
            network: NetworkSettings::default(),
            privacy: PrivacySettings::default(),
            chains: ChainSettings::default(),
            indexed_artifacts: IndexedArtifactSettings::default(),
            poi: PoiSettings::default(),
            broadcaster: PublicBroadcasterSettings::default(),
            tokens: TokenSettings::default(),
            gas: GasSettings::default(),
            runtime: RuntimeSettings::default(),
            waku: WakuSettings::default(),
            walletconnect: WalletConnectSettings::default(),
        }
    }
}

impl From<LegacyWalletSettings> for super::WalletSettings {
    fn from(old: LegacyWalletSettings) -> Self {
        Self {
            version: WALLET_SETTINGS_VERSION,
            network: old.network,
            privacy: old.privacy,
            chains: super::ChainSettings {
                per_chain: old
                    .chains
                    .per_chain
                    .into_iter()
                    .map(|(id, chain)| (id, chain.into()))
                    .collect(),
                custom: BTreeMap::new(),
            },
            indexed_artifacts: old.indexed_artifacts,
            poi: old.poi,
            broadcaster: old.broadcaster,
            tokens: old.tokens,
            gas: old.gas,
            runtime: old.runtime,
            waku: old.waku,
            walletconnect: old.walletconnect,
        }
    }
}

impl LegacyWalletSettings {
    pub(super) fn has_protocol_overrides(&self) -> bool {
        self.chains.per_chain.values().any(|chain| {
            chain.contracts.railgun_contract.is_some()
                || chain.contracts.relay_adapt_contract.is_some()
                || chain.contracts.relay_adapt_7702_contract.is_some()
                || chain.contracts.coinbase_payer.is_some()
                || chain.deployment.has_protocol_overrides()
        })
    }
}

impl From<ChainSettingsOverride> for super::ChainSettingsOverride {
    fn from(old: ChainSettingsOverride) -> Self {
        let railgun = super::RailgunSettingsOverride {
            sponsored_bundle_relays: old.sponsored_bundle_relays,
            quick_sync: old.quick_sync,
            deployment: old.deployment.into(),
            block_range: old.block_range,
            poll_interval_secs: old.poll_interval_secs,
            indexed_wallet_block_range: old.indexed_wallet_block_range,
        };
        Self {
            native_usd_pricing: super::NativeUsdPricing::Default,
            enabled: old.enabled,
            rpc_endpoints: old
                .rpc_endpoints
                .into_iter()
                .map(super::RpcEndpointSetting::from)
                .collect(),
            contracts: super::ChainContractSettings {
                wrapped_native_token: old.contracts.wrapped_native_token,
                multicall_contract: old.contracts.multicall_contract,
            },
            finality_depth: old.finality_depth,
            gas: old.gas,
            railgun,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ChainDeploymentSettings {
    pub deployment_block: Option<u64>,
    pub v2_start_block: Option<u64>,
    pub legacy_shield_block: Option<u64>,
    pub archive_until_block: Option<u64>,
    pub archive_rpc_url: Option<String>,
}

impl ChainDeploymentSettings {
    const fn has_protocol_overrides(&self) -> bool {
        self.deployment_block.is_some()
            || self.v2_start_block.is_some()
            || self.legacy_shield_block.is_some()
    }
}

impl From<ChainDeploymentSettings> for super::ChainDeploymentSettings {
    fn from(old: ChainDeploymentSettings) -> Self {
        Self {
            archive_until_block: old.archive_until_block,
            archive_rpc_url: old.archive_rpc_url,
            archive_rpc_direct: false,
        }
    }
}

/// Released versions 7 through 10, which still carried Railgun protocol overrides.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ReleasedWalletSettings {
    pub version: u32,
    pub network: NetworkSettings,
    pub privacy: PrivacySettings,
    pub chains: ReleasedChainSettings,
    pub indexed_artifacts: IndexedArtifactSettings,
    pub poi: PoiSettings,
    pub broadcaster: PublicBroadcasterSettings,
    pub tokens: TokenSettings,
    pub gas: GasSettings,
    pub runtime: RuntimeSettings,
    pub waku: WakuSettings,
    pub walletconnect: WalletConnectSettings,
}

impl Default for ReleasedWalletSettings {
    fn default() -> Self {
        Self {
            version: 10,
            network: NetworkSettings::default(),
            privacy: PrivacySettings::default(),
            chains: ReleasedChainSettings::default(),
            indexed_artifacts: IndexedArtifactSettings::default(),
            poi: PoiSettings::default(),
            broadcaster: PublicBroadcasterSettings::default(),
            tokens: TokenSettings::default(),
            gas: GasSettings::default(),
            runtime: RuntimeSettings::default(),
            waku: WakuSettings::default(),
            walletconnect: WalletConnectSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ReleasedChainSettings {
    pub per_chain: BTreeMap<u64, ReleasedChainSettingsOverride>,
    pub custom: BTreeMap<u64, CustomChainSettings>,
}

impl Default for ReleasedChainSettings {
    fn default() -> Self {
        let per_chain = railgun_ui::built_in_chain_ids()
            .filter(|chain_id| !railgun_ui::is_testnet_chain(*chain_id))
            .map(|chain_id| (chain_id, ReleasedChainSettingsOverride::default()))
            .collect();
        Self {
            per_chain,
            custom: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ReleasedChainSettingsOverride {
    #[serde(skip_serializing_if = "NativeUsdPricing::is_default")]
    pub native_usd_pricing: NativeUsdPricing,
    pub enabled: bool,
    pub rpc_endpoints: Vec<String>,
    pub contracts: super::ChainContractSettings,
    pub finality_depth: Option<u64>,
    pub gas: ChainGasSettings,
    pub railgun: ReleasedRailgunSettingsOverride,
}

impl Default for ReleasedChainSettingsOverride {
    fn default() -> Self {
        Self {
            native_usd_pricing: NativeUsdPricing::Default,
            enabled: true,
            rpc_endpoints: Vec::new(),
            contracts: super::ChainContractSettings::default(),
            finality_depth: None,
            gas: ChainGasSettings::default(),
            railgun: ReleasedRailgunSettingsOverride::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ReleasedRailgunSettingsOverride {
    pub sponsored_bundle_relays: Option<Vec<String>>,
    pub quick_sync: QuickSyncSettings,
    pub contracts: ReleasedRailgunContractSettings,
    pub deployment: ChainDeploymentSettings,
    pub block_range: Option<u64>,
    pub poll_interval_secs: Option<u64>,
    pub indexed_wallet_block_range: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ReleasedRailgunContractSettings {
    pub railgun_contract: Option<String>,
    pub relay_adapt_contract: Option<String>,
    pub relay_adapt_7702_contract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coinbase_payer: Option<String>,
}

impl ReleasedWalletSettings {
    pub(super) fn has_protocol_overrides(&self) -> bool {
        self.chains.per_chain.values().any(|chain| {
            chain.railgun.contracts != ReleasedRailgunContractSettings::default()
                || chain.railgun.deployment.has_protocol_overrides()
        })
    }
}

impl From<ReleasedWalletSettings> for super::WalletSettings {
    fn from(old: ReleasedWalletSettings) -> Self {
        Self {
            version: WALLET_SETTINGS_VERSION,
            network: old.network,
            privacy: old.privacy,
            chains: super::ChainSettings {
                per_chain: old
                    .chains
                    .per_chain
                    .into_iter()
                    .map(|(id, chain)| (id, chain.into()))
                    .collect(),
                custom: old.chains.custom,
            },
            indexed_artifacts: old.indexed_artifacts,
            poi: old.poi,
            broadcaster: old.broadcaster,
            tokens: old.tokens,
            gas: old.gas,
            runtime: old.runtime,
            waku: old.waku,
            walletconnect: old.walletconnect,
        }
    }
}

impl From<ReleasedChainSettingsOverride> for super::ChainSettingsOverride {
    fn from(old: ReleasedChainSettingsOverride) -> Self {
        let railgun = old.railgun;
        Self {
            native_usd_pricing: old.native_usd_pricing,
            enabled: old.enabled,
            rpc_endpoints: old
                .rpc_endpoints
                .into_iter()
                .map(super::RpcEndpointSetting::from)
                .collect(),
            contracts: old.contracts,
            finality_depth: old.finality_depth,
            gas: old.gas,
            railgun: super::RailgunSettingsOverride {
                sponsored_bundle_relays: railgun.sponsored_bundle_relays,
                quick_sync: railgun.quick_sync,
                deployment: railgun.deployment.into(),
                block_range: railgun.block_range,
                poll_interval_secs: railgun.poll_interval_secs,
                indexed_wallet_block_range: railgun.indexed_wallet_block_range,
            },
        }
    }
}
