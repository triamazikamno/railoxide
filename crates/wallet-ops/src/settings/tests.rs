use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::Address;
use local_db::{DbConfig, DbStore};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    AUTO_LOCK_TIMEOUT_PRESETS_SECS, DEFAULT_AUTO_LOCK_TIMEOUT_SECS,
    DEFAULT_INDEXED_ARTIFACT_CONCURRENCY, DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES,
    IndexedArtifactManifestSourceSetting, IndexedArtifactSourceModeSetting,
    LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME, MAX_AUTO_LOCK_TIMEOUT_SECS, MIN_AUTO_LOCK_TIMEOUT_SECS,
    OFFICIAL_INDEXED_ARTIFACT_GATEWAYS, OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME,
    OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY, OFFICIAL_POI_ARTIFACT_GATEWAYS,
    OFFICIAL_POI_ARTIFACT_IPNS_NAME, OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY,
    PoiArtifactManifestSourceSetting, PoiReadSourceSetting, RememberedWalletKind,
    WALLET_FIRST_START_KEY, WALLET_FIRST_START_VERSION, WALLET_SETTINGS_KEY,
    WALLET_SETTINGS_VERSION, WALLET_UI_STATE_KEY, WALLET_UI_STATE_VERSION, WakuDirectPeerSetting,
    WalletFirstStart, WalletSettings, WalletSettingsError, WalletUiState, WalletUiStateError,
    build_effective_chain_configs, build_effective_token_registry, decode_wallet_settings,
    decode_wallet_ui_state, encode_wallet_settings, load_first_start, load_wallet_settings,
    load_wallet_ui_state, resolve_effective_chain_rpc_route, save_first_start,
    save_wallet_settings, save_wallet_ui_state,
};
use crate::AppSettingsDbExt as _;
use crate::WALLETCONNECT_DEFAULT_PROJECT_ID;
use crate::{RpcChainRoute, RpcTransport};

static TEMP_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_root() -> PathBuf {
    let dir = std::env::temp_dir().join("railoxide-wallet-settings-tests");
    fs::create_dir_all(&dir).expect("create temp db dir");
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = TEMP_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("db-{pid}-{nanos}-{counter}"))
}

fn legacy_official_settings() -> WalletSettings {
    let mut settings = WalletSettings::default();
    settings.poi.artifact.manifest_source = PoiArtifactManifestSourceSetting::IpnsName(
        LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string(),
    );
    settings
}

#[derive(Debug, Serialize)]
struct ReleasedV4WalletSettingsWire {
    version: u32,
    network: super::NetworkSettings,
    privacy: super::PrivacySettings,
    chains: super::legacy::ChainSettings,
    indexed_artifacts: super::IndexedArtifactSettings,
    poi: super::PoiSettings,
    broadcaster: super::PublicBroadcasterSettings,
    tokens: super::TokenSettings,
    gas: super::GasSettings,
    runtime: super::RuntimeSettings,
    waku: super::WakuSettings,
    walletconnect: super::WalletConnectSettings,
}

fn released_chains(chains: &super::ChainSettings) -> super::legacy::ChainSettings {
    super::legacy::ChainSettings {
        per_chain: chains
            .per_chain
            .iter()
            .map(|(&id, chain)| {
                let private = &chain.railgun;
                (
                    id,
                    super::legacy::ChainSettingsOverride {
                        enabled: chain.enabled,
                        rpc_endpoints: chain
                            .rpc_endpoints
                            .iter()
                            .map(|endpoint| endpoint.url.clone())
                            .collect(),
                        sponsored_bundle_relays: private.sponsored_bundle_relays.clone(),
                        quick_sync: private.quick_sync.clone(),
                        contracts: super::legacy::ChainContractSettings {
                            wrapped_native_token: chain.contracts.wrapped_native_token.clone(),
                            multicall_contract: chain.contracts.multicall_contract.clone(),
                            ..Default::default()
                        },
                        deployment: super::legacy::ChainDeploymentSettings {
                            archive_until_block: private.deployment.archive_until_block,
                            archive_rpc_url: private.deployment.archive_rpc_url.clone(),
                            ..Default::default()
                        },
                        finality_depth: chain.finality_depth,
                        block_range: private.block_range,
                        poll_interval_secs: private.poll_interval_secs,
                        indexed_wallet_block_range: private.indexed_wallet_block_range,
                        gas: chain.gas.clone(),
                    },
                )
            })
            .collect(),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReleasedV1WalletSettingsWire {
    version: u32,
    network: super::NetworkSettings,
    chains: super::legacy::ChainSettings,
    indexed_artifacts: super::IndexedArtifactSettings,
    poi: ReleasedV1PoiSettingsWire,
    broadcaster: super::PublicBroadcasterSettings,
    tokens: super::TokenSettings,
    gas: super::GasSettings,
    runtime: ReleasedV1RuntimeSettingsWire,
    waku: super::WakuSettings,
    walletconnect: super::WalletConnectSettings,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReleasedV1RuntimeSettingsWire {
    public_balance_refresh_interval_secs: u64,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictV2WalletSettingsWire {
    version: u32,
    network: super::NetworkSettings,
    chains: StrictV2ChainSettingsWire,
    indexed_artifacts: super::IndexedArtifactSettings,
    poi: super::PoiSettings,
    broadcaster: super::PublicBroadcasterSettings,
    tokens: super::TokenSettings,
    gas: super::GasSettings,
    runtime: super::RuntimeSettings,
    waku: super::WakuSettings,
    walletconnect: super::WalletConnectSettings,
}

#[allow(dead_code)]
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StrictV2ChainSettingsWire {
    per_chain: std::collections::BTreeMap<u64, StrictV2ChainSettingsOverrideWire>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StrictV2ChainSettingsOverrideWire {
    enabled: bool,
    rpc_endpoints: Vec<String>,
    quick_sync: super::QuickSyncSettings,
    contracts: StrictV2ChainContractSettingsWire,
    deployment: super::legacy::ChainDeploymentSettings,
    finality_depth: Option<u64>,
    block_range: Option<u64>,
    poll_interval_secs: Option<u64>,
    indexed_wallet_block_range: Option<u64>,
    gas: super::ChainGasSettings,
}

#[allow(dead_code)]
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StrictV2ChainContractSettingsWire {
    railgun_contract: Option<String>,
    relay_adapt_contract: Option<String>,
    relay_adapt_7702_contract: Option<String>,
    wrapped_native_token: Option<String>,
    multicall_contract: Option<String>,
}

impl Default for ReleasedV1RuntimeSettingsWire {
    fn default() -> Self {
        Self {
            public_balance_refresh_interval_secs: crate::public_balance_refresh_interval_secs(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReleasedV1PoiSettingsWire {
    read_source: PoiReadSourceSetting,
    artifact: ReleasedV1PoiArtifactSettingsWire,
    proxy: super::PoiProxySettings,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReleasedV1PoiArtifactSettingsWire {
    publisher_pubkey: String,
    manifest_source: ReleasedV1PoiArtifactManifestSourceWire,
    gateway_urls: Vec<String>,
    max_manifest_age_secs: Option<u64>,
}

impl Default for ReleasedV1PoiArtifactSettingsWire {
    fn default() -> Self {
        Self {
            publisher_pubkey: OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY.to_string(),
            manifest_source: ReleasedV1PoiArtifactManifestSourceWire::default(),
            gateway_urls: OFFICIAL_POI_ARTIFACT_GATEWAYS
                .iter()
                .map(ToString::to_string)
                .collect(),
            max_manifest_age_secs: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
enum ReleasedV1PoiArtifactManifestSourceWire {
    Url(String),
    Cid(String),
    IpnsName(String),
}

impl Default for ReleasedV1PoiArtifactManifestSourceWire {
    fn default() -> Self {
        Self::IpnsName(LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string())
    }
}

#[derive(Debug, Serialize)]
struct ReleasedV3WalletSettingsWire {
    version: u32,
    network: super::NetworkSettings,
    chains: super::legacy::ChainSettings,
    indexed_artifacts: super::IndexedArtifactSettings,
    poi: super::PoiSettings,
    broadcaster: super::PublicBroadcasterSettings,
    tokens: super::TokenSettings,
    gas: super::GasSettings,
    runtime: super::RuntimeSettings,
    waku: super::WakuSettings,
    walletconnect: super::WalletConnectSettings,
}

pub(super) fn custom_evm_chain() -> super::CustomChainSettings {
    super::CustomChainSettings {
        native_usd_pricing: crate::settings::NativeUsdPricing::Default,
        name: "Test EVM".into(),
        native_currency: super::NativeCurrency {
            name: "Test coin".into(),
            symbol: "TST".into(),
            decimals: 6,
        },
        rpc_endpoints: vec!["http://127.0.0.1:8545".to_owned().into()],
        explorer_urls: vec![],
        enabled: true,
        contracts: super::ChainContractSettings::default(),
        finality_depth: None,
        gas: super::ChainGasSettings::default(),
    }
}

/// The top-level record of released versions 7 through 11 (`v0.2.0-beta.13`), before `tron`.
/// Its sections are unchanged since `v0.2.0-beta.13`.
#[derive(Serialize)]
struct ReleasedV11WalletSettingsWire<'a> {
    version: u32,
    network: &'a super::NetworkSettings,
    privacy: &'a super::PrivacySettings,
    chains: &'a super::ChainSettings,
    indexed_artifacts: &'a super::IndexedArtifactSettings,
    poi: &'a super::PoiSettings,
    broadcaster: &'a super::PublicBroadcasterSettings,
    tokens: &'a super::TokenSettings,
    gas: &'a super::GasSettings,
    runtime: &'a super::RuntimeSettings,
    waku: &'a super::WakuSettings,
    walletconnect: &'a super::WalletConnectSettings,
}

mod chain_endpoints;
mod chains;
mod custom_chains;
mod migration;
mod poi_sources;
mod released_versions;
mod storage;
mod tokens_pricing;
mod validation;
mod waku;
