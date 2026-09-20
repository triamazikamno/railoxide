//! Released versions 1 through 6. Keep these record shapes independent of current chains.

use super::{
    BTreeMap, ChainDeploymentSettings, ChainGasSettings, Deserialize, GasSettings,
    IndexedArtifactSettings, NetworkSettings, PoiSettings, PrivacySettings,
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
        let per_chain = railgun_ui::DEFAULT_CHAINS
            .iter()
            .copied()
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

impl From<ChainSettingsOverride> for super::ChainSettingsOverride {
    fn from(old: ChainSettingsOverride) -> Self {
        let railgun = super::RailgunSettingsOverride {
            sponsored_bundle_relays: old.sponsored_bundle_relays,
            quick_sync: old.quick_sync,
            contracts: super::RailgunContractSettings {
                railgun_contract: old.contracts.railgun_contract,
                relay_adapt_contract: old.contracts.relay_adapt_contract,
                relay_adapt_7702_contract: old.contracts.relay_adapt_7702_contract,
                coinbase_payer: old.contracts.coinbase_payer,
            },
            deployment: old.deployment,
            block_range: old.block_range,
            poll_interval_secs: old.poll_interval_secs,
            indexed_wallet_block_range: old.indexed_wallet_block_range,
        };
        Self {
            enabled: old.enabled,
            rpc_endpoints: old.rpc_endpoints,
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
