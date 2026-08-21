use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use local_db::{DbConfig, DbStore};
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
    WALLET_SETTINGS_KEY, WALLET_SETTINGS_VERSION, WALLET_UI_STATE_KEY, WALLET_UI_STATE_VERSION,
    WakuDirectPeerSetting, WalletSettings, WalletSettingsError, WalletUiState, WalletUiStateError,
    build_effective_chain_configs, build_effective_token_registry, decode_wallet_settings,
    decode_wallet_ui_state, encode_wallet_settings, load_wallet_settings, load_wallet_ui_state,
    save_wallet_settings, save_wallet_ui_state, should_show_chain_deployment_metadata_settings,
};
use crate::WALLETCONNECT_DEFAULT_PROJECT_ID;
use sync_service::ChainConfigDefaults;

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

fn put_unmigrated_settings(store: &DbStore, settings: &WalletSettings) {
    let payload = encode_wallet_settings(settings).expect("encode unmigrated settings");
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .expect("store unmigrated settings");
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ReleasedV1WalletSettingsWire {
    version: u32,
    network: super::NetworkSettings,
    chains: super::ChainSettings,
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
    deployment: super::ChainDeploymentSettings,
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

fn released_v1_settings_payload() -> Vec<u8> {
    let settings = ReleasedV1WalletSettingsWire {
        version: 1,
        ..ReleasedV1WalletSettingsWire::default()
    };
    rmp_serde::to_vec_named(&settings).expect("encode released v1 settings")
}

fn released_v2_settings_payload() -> Vec<u8> {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.rpc_endpoints = vec!["https://existing-rpc.example".to_string()];
    ethereum.contracts.wrapped_native_token =
        Some("0x0000000000000000000000000000000000000001".to_string());
    let chains = settings
        .chains
        .per_chain
        .into_iter()
        .map(|(chain_id, chain)| {
            let contracts = StrictV2ChainContractSettingsWire {
                railgun_contract: chain.contracts.railgun_contract,
                relay_adapt_contract: chain.contracts.relay_adapt_contract,
                relay_adapt_7702_contract: chain.contracts.relay_adapt_7702_contract,
                wrapped_native_token: chain.contracts.wrapped_native_token,
                multicall_contract: chain.contracts.multicall_contract,
            };
            (
                chain_id,
                StrictV2ChainSettingsOverrideWire {
                    enabled: chain.enabled,
                    rpc_endpoints: chain.rpc_endpoints,
                    quick_sync: chain.quick_sync,
                    contracts,
                    deployment: chain.deployment,
                    finality_depth: chain.finality_depth,
                    block_range: chain.block_range,
                    poll_interval_secs: chain.poll_interval_secs,
                    indexed_wallet_block_range: chain.indexed_wallet_block_range,
                    gas: chain.gas,
                },
            )
        })
        .collect();
    let wire = StrictV2WalletSettingsWire {
        version: 2,
        network: settings.network,
        chains: StrictV2ChainSettingsWire { per_chain: chains },
        indexed_artifacts: settings.indexed_artifacts,
        poi: settings.poi,
        broadcaster: settings.broadcaster,
        tokens: settings.tokens,
        gas: settings.gas,
        runtime: settings.runtime,
        waku: settings.waku,
        walletconnect: settings.walletconnect,
    };
    rmp_serde::to_vec_named(&wire).expect("encode released v2 settings")
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

#[test]
fn missing_settings_synthesizes_official_indexed_artifact_defaults() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");

    let settings = load_wallet_settings(&store).expect("load settings");
    assert_eq!(settings.version, WALLET_SETTINGS_VERSION);
    assert!(!settings.privacy.mimic_railway_shields_by_default);
    assert_eq!(
        settings.runtime.auto_lock_timeout_secs,
        Some(DEFAULT_AUTO_LOCK_TIMEOUT_SECS)
    );
    assert_eq!(
        settings.poi.read_source,
        PoiReadSourceSetting::IndexedArtifacts
    );
    assert_eq!(
        settings.poi.artifact.publisher_pubkey,
        OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY
    );
    assert_eq!(
        settings.poi.artifact.manifest_source,
        PoiArtifactManifestSourceSetting::IpnsName(OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string())
    );
    assert_eq!(
        settings.poi.artifact.gateway_urls,
        OFFICIAL_POI_ARTIFACT_GATEWAYS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        settings.indexed_artifacts.source_mode,
        IndexedArtifactSourceModeSetting::Official
    );
    assert_eq!(
        settings.indexed_artifacts.publisher_pubkey.as_deref(),
        Some(OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY)
    );
    assert_eq!(
        settings.indexed_artifacts.manifest_source,
        Some(IndexedArtifactManifestSourceSetting::IpnsName(
            OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME.to_string()
        ))
    );
    assert_eq!(
        settings.indexed_artifacts.gateway_urls,
        OFFICIAL_INDEXED_ARTIFACT_GATEWAYS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert!(
        store
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .expect("load raw settings")
            .is_none()
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn exact_legacy_official_identity_migrates_in_place() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    put_unmigrated_settings(&store, &legacy_official_settings());

    let migrated = load_wallet_settings(&store).expect("load and migrate settings");
    assert_eq!(migrated.version, WALLET_SETTINGS_VERSION);
    assert_eq!(
        migrated.poi.artifact.publisher_pubkey,
        OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY
    );
    assert_eq!(
        migrated.poi.artifact.manifest_source,
        PoiArtifactManifestSourceSetting::IpnsName(OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string())
    );

    let persisted = store
        .get_app_settings_record(WALLET_SETTINGS_KEY)
        .expect("read migrated settings")
        .expect("migrated settings record");
    assert_eq!(
        decode_wallet_settings(&persisted).expect("decode migrated settings"),
        migrated
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn official_identity_migration_preserves_user_preferences() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = legacy_official_settings();
    settings.poi.artifact.gateway_urls = vec![
        "https://second.example".to_string(),
        "https://first.example".to_string(),
    ];
    settings.poi.artifact.max_manifest_age_secs = Some(7_200);
    settings.poi.proxy.rpc_url = "https://poi-proxy.example/rpc".to_string();
    put_unmigrated_settings(&store, &settings);

    let migrated = load_wallet_settings(&store).expect("load and migrate settings");

    assert_eq!(
        migrated.poi.artifact.gateway_urls,
        settings.poi.artifact.gateway_urls
    );
    assert_eq!(
        migrated.poi.artifact.max_manifest_age_secs,
        settings.poi.artifact.max_manifest_age_secs
    );
    assert_eq!(migrated.poi.proxy, settings.poi.proxy);

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn nonmatching_identity_is_untouched_and_constructs_a_v4_source() {
    for settings in [
        {
            let mut settings = legacy_official_settings();
            settings.poi.artifact.publisher_pubkey = format!("0x{}", "11".repeat(32));
            settings
        },
        {
            let mut settings = legacy_official_settings();
            settings.poi.artifact.manifest_source = PoiArtifactManifestSourceSetting::IpnsName(
                "k51qzi5uqu5dicmabkge4lkunc4bkd198u9xicp5espmw5zdzbafkez7hyh5ft".to_string(),
            );
            settings
        },
    ] {
        let root_dir = temp_db_root();
        let store = DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db");
        put_unmigrated_settings(&store, &settings);

        let loaded = load_wallet_settings(&store).expect("load nonmatching settings");
        assert_eq!(loaded, settings);
        let super::PoiReadSource::IndexedArtifacts {
            artifact_source,
            wallet_read_fallback,
            ..
        } = loaded
            .poi_read_source()
            .expect("custom PPOI artifact graph source")
        else {
            panic!("nonmatching identity should remain an indexed artifact source");
        };
        assert_eq!(
            alloy::hex::encode_prefixed(artifact_source.trusted_publisher_pubkey.as_slice()),
            settings.poi.artifact.publisher_pubkey
        );
        assert_eq!(
            artifact_source.manifest_source,
            settings.poi.artifact.manifest_source.to_runtime()
        );
        assert_eq!(wallet_read_fallback, super::PoiProxyFallback::Disabled);
        let persisted = store
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .expect("read nonmatching settings")
            .expect("nonmatching settings record");
        assert_eq!(
            decode_wallet_settings(&persisted).expect("decode nonmatching settings"),
            settings
        );

        drop(store);
        fs::remove_dir_all(root_dir).expect("remove temp db dir");
    }
}

#[test]
fn migrated_official_settings_survive_database_restart() {
    let root_dir = temp_db_root();
    {
        let store = DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db");
        put_unmigrated_settings(&store, &legacy_official_settings());
        load_wallet_settings(&store).expect("migrate settings");
    }

    let reopened = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("reopen db");
    let settings = load_wallet_settings(&reopened).expect("load migrated settings after restart");
    assert_eq!(
        settings.poi.artifact.manifest_source,
        PoiArtifactManifestSourceSetting::IpnsName(OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string())
    );

    drop(reopened);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn frozen_released_v1_decoder_rejects_current_settings() {
    let current_payload =
        encode_wallet_settings(&WalletSettings::default()).expect("encode current settings");

    rmp_serde::from_slice::<ReleasedV1WalletSettingsWire>(&current_payload)
        .expect_err("released v1 decoder rejects the version 2 auto-lock field");
}

#[test]
fn indexed_artifact_official_preset_is_default_enabled() {
    let official = super::IndexedArtifactSettings::official_preset();
    let default = super::IndexedArtifactSettings::default();

    assert_eq!(
        official.source_mode,
        IndexedArtifactSourceModeSetting::Official
    );
    assert_eq!(
        official.publisher_pubkey.as_deref(),
        Some(OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY)
    );
    assert_eq!(
        official.manifest_source,
        Some(IndexedArtifactManifestSourceSetting::IpnsName(
            OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME.to_string()
        ))
    );
    assert_eq!(
        official.gateway_urls,
        OFFICIAL_INDEXED_ARTIFACT_GATEWAYS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        default.source_mode,
        IndexedArtifactSourceModeSetting::Official
    );
    assert_eq!(default, official);
}

#[test]
fn settings_roundtrip_through_local_db() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.network.mode = super::NetworkModeSetting::Direct;
    settings.privacy.mimic_railway_shields_by_default = true;
    settings.poi.read_source = PoiReadSourceSetting::PoiProxy;

    save_wallet_settings(&store, &settings).expect("save settings");
    let loaded = load_wallet_settings(&store).expect("load settings");
    assert!(loaded.privacy.mimic_railway_shields_by_default);
    assert_eq!(loaded, settings);

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn version_1_settings_migrate_and_rewrite_with_auto_lock_default() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let released_payload = released_v1_settings_payload();
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &released_payload)
        .expect("store released v1 settings");

    let migrated = load_wallet_settings(&store).expect("migrate released v1 settings");
    assert_eq!(migrated.version, WALLET_SETTINGS_VERSION);
    assert_eq!(
        migrated.runtime.auto_lock_timeout_secs,
        Some(DEFAULT_AUTO_LOCK_TIMEOUT_SECS)
    );
    assert_eq!(
        migrated.poi.artifact.manifest_source,
        PoiArtifactManifestSourceSetting::IpnsName(OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string())
    );

    let rewritten = store
        .get_app_settings_record(WALLET_SETTINGS_KEY)
        .expect("read rewritten settings")
        .expect("rewritten settings record");
    assert_ne!(rewritten, released_payload);
    let rewritten = decode_wallet_settings(&rewritten).expect("decode rewritten settings");
    assert_eq!(
        rewritten.poi.artifact.manifest_source,
        PoiArtifactManifestSourceSetting::IpnsName(OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string())
    );
    assert_eq!(rewritten, migrated);

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn version_2_settings_migrate_with_sponsored_fields_unset() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let released_payload = released_v2_settings_payload();
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &released_payload)
        .expect("store version 2 settings");

    let migrated = load_wallet_settings(&store).expect("migrate version 2 settings");
    let ethereum = migrated
        .chains
        .per_chain
        .get(&1)
        .expect("migrated ethereum settings");
    assert_eq!(migrated.version, WALLET_SETTINGS_VERSION);
    assert_eq!(ethereum.rpc_endpoints, vec!["https://existing-rpc.example"]);
    assert_eq!(ethereum.sponsored_bundle_relays, None);
    assert_eq!(ethereum.contracts.coinbase_payer, None);
    let effective = build_effective_chain_configs(&migrated).expect("effective migrated settings");
    let ethereum = effective.get(&1).expect("effective ethereum");
    assert_eq!(ethereum.sponsored_bundle_relays.len(), 2);
    assert_eq!(ethereum.coinbase_payer, super::default_coinbase_payer(1));
    let rewritten = store
        .get_app_settings_record(WALLET_SETTINGS_KEY)
        .expect("read rewritten settings")
        .expect("rewritten settings record");
    assert_ne!(rewritten, released_payload);
    assert_eq!(
        decode_wallet_settings(&rewritten).expect("decode rewritten settings"),
        migrated
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[derive(Debug, Serialize)]
struct ReleasedV3WalletSettingsWire {
    version: u32,
    network: super::NetworkSettings,
    chains: super::ChainSettings,
    indexed_artifacts: super::IndexedArtifactSettings,
    poi: super::PoiSettings,
    broadcaster: super::PublicBroadcasterSettings,
    tokens: super::TokenSettings,
    gas: super::GasSettings,
    runtime: super::RuntimeSettings,
    waku: super::WakuSettings,
    walletconnect: super::WalletConnectSettings,
}

#[test]
fn version_3_settings_migrate_with_railway_preference_disabled() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let settings = WalletSettings::default();
    let released_payload = rmp_serde::to_vec_named(&ReleasedV3WalletSettingsWire {
        version: 3,
        network: settings.network,
        chains: settings.chains,
        indexed_artifacts: settings.indexed_artifacts,
        poi: settings.poi,
        broadcaster: settings.broadcaster,
        tokens: settings.tokens,
        gas: settings.gas,
        runtime: settings.runtime,
        waku: settings.waku,
        walletconnect: settings.walletconnect,
    })
    .expect("encode released v3 settings");
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &released_payload)
        .expect("store released v3 settings");

    let migrated = load_wallet_settings(&store).expect("migrate released v3 settings");
    assert_eq!(migrated.version, WALLET_SETTINGS_VERSION);
    assert!(!migrated.privacy.mimic_railway_shields_by_default);
    let rewritten = store
        .get_app_settings_record(WALLET_SETTINGS_KEY)
        .expect("read rewritten settings")
        .expect("rewritten settings record");
    assert_ne!(rewritten, released_payload);
    assert_eq!(
        decode_wallet_settings(&rewritten).expect("decode rewritten settings"),
        migrated
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn version_3_sponsored_fields_are_not_strict_version_2_readable() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.sponsored_bundle_relays = Some(vec!["https://relay.example".to_string()]);
    ethereum.contracts.coinbase_payer =
        Some("0x381787eBFD112E742fc965289c59630B2e7ce0A4".to_string());
    let encoded = encode_wallet_settings(&settings).expect("encode version 3 settings");

    assert!(rmp_serde::from_slice::<StrictV2WalletSettingsWire>(&encoded).is_err());
}

#[test]
fn current_auto_lock_timeout_and_disabled_policy_are_preserved() {
    for policy in [Some(30 * 60), None] {
        let root_dir = temp_db_root();
        let store = DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db");
        let mut settings = WalletSettings::default();
        settings.runtime.auto_lock_timeout_secs = policy;
        save_wallet_settings(&store, &settings).expect("save current settings");
        let persisted = store
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .expect("read current settings")
            .expect("current settings record");

        let loaded = load_wallet_settings(&store).expect("load current settings");
        assert_eq!(loaded.runtime.auto_lock_timeout_secs, policy);
        assert_eq!(
            store
                .get_app_settings_record(WALLET_SETTINGS_KEY)
                .expect("reread current settings")
                .expect("current settings record"),
            persisted,
            "loading current settings must not rewrite the record"
        );

        drop(store);
        fs::remove_dir_all(root_dir).expect("remove temp db dir");
    }
}

#[test]
fn stored_invalid_settings_load_for_repair_but_cannot_be_saved() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.runtime.auto_lock_timeout_secs = Some(MIN_AUTO_LOCK_TIMEOUT_SECS - 1);
    settings.walletconnect.project_id_override = Some("preserved-project-id".to_string());
    let payload = rmp_serde::to_vec_named(&settings).expect("encode invalid settings fixture");
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .expect("store invalid settings fixture");

    let loaded = load_wallet_settings(&store).expect("load invalid settings for repair");
    assert_eq!(loaded, settings);
    assert_eq!(
        loaded.walletconnect.project_id_override.as_deref(),
        Some("preserved-project-id")
    );
    assert!(loaded.validate().is_err());
    let error =
        save_wallet_settings(&store, &loaded).expect_err("invalid settings rejected on save");
    assert!(matches!(error, WalletSettingsError::Validation(_)));
    assert_eq!(
        store
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .expect("reread invalid settings")
            .expect("invalid settings record"),
        payload,
        "loading and rejected saving must not rewrite a current-version record"
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn missing_ui_state_defaults_to_empty_without_persisting() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");

    let state = load_wallet_ui_state(&store).expect("load UI state");
    assert_eq!(state, WalletUiState::default());
    assert!(
        store
            .get_app_settings_record(WALLET_UI_STATE_KEY)
            .expect("load raw UI state")
            .is_none()
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn ui_state_roundtrip_through_local_db() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let state = WalletUiState {
        version: 0,
        last_wallet_id: Some("wallet-123".to_owned()),
        last_chain_id: Some(137),
        last_wallet_kind: RememberedWalletKind::SoftwareProfile,
    };

    save_wallet_ui_state(&store, &state).expect("save UI state");
    let loaded = load_wallet_ui_state(&store).expect("load UI state");

    assert_eq!(
        loaded,
        WalletUiState {
            version: WALLET_UI_STATE_VERSION,
            last_wallet_id: Some("wallet-123".to_owned()),
            last_chain_id: Some(137),
            last_wallet_kind: RememberedWalletKind::SoftwareProfile,
        }
    );

    save_wallet_ui_state(
        &store,
        &WalletUiState {
            version: WALLET_UI_STATE_VERSION,
            last_wallet_id: Some("hardware-123".to_owned()),
            last_chain_id: Some(137),
            last_wallet_kind: RememberedWalletKind::HardwareWallet,
        },
    )
    .expect("save hardware UI state");
    assert_eq!(
        load_wallet_ui_state(&store).expect("load hardware UI state"),
        WalletUiState {
            version: WALLET_UI_STATE_VERSION,
            last_wallet_id: Some("hardware-123".to_owned()),
            last_chain_id: Some(137),
            last_wallet_kind: RememberedWalletKind::HardwareWallet,
        }
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn released_v1_ui_state_migrates_to_unknown_kind_and_persists_v2() {
    #[derive(Serialize)]
    struct ReleasedV1WalletUiState {
        version: u32,
        last_wallet_id: Option<String>,
        last_chain_id: Option<u64>,
    }

    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let payload = rmp_serde::to_vec_named(&ReleasedV1WalletUiState {
        version: 1,
        last_wallet_id: Some("legacy-wallet".to_owned()),
        last_chain_id: Some(56),
    })
    .expect("encode released v1 UI state");
    store
        .put_app_settings_record(WALLET_UI_STATE_KEY, &payload)
        .expect("store released v1 UI state");

    let loaded = load_wallet_ui_state(&store).expect("migrate UI state");
    assert_eq!(loaded.version, WALLET_UI_STATE_VERSION);
    assert_eq!(loaded.last_wallet_id.as_deref(), Some("legacy-wallet"));
    assert_eq!(loaded.last_chain_id, Some(56));
    assert_eq!(loaded.last_wallet_kind, RememberedWalletKind::Unknown);
    let persisted = store
        .get_app_settings_record(WALLET_UI_STATE_KEY)
        .expect("load migrated UI state")
        .expect("migrated UI state");
    assert_eq!(
        decode_wallet_ui_state(&persisted).expect("decode migrated UI state"),
        loaded
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn unsupported_future_ui_state_version_falls_back_to_empty() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let state = WalletUiState {
        version: WALLET_UI_STATE_VERSION + 1,
        last_wallet_id: Some("wallet-123".to_owned()),
        last_chain_id: Some(137),
        last_wallet_kind: RememberedWalletKind::Unknown,
    };
    let data = rmp_serde::to_vec_named(&state).expect("encode future UI state");
    store
        .put_app_settings_record(WALLET_UI_STATE_KEY, &data)
        .expect("store future UI state");

    let err = decode_wallet_ui_state(&data).expect_err("future version rejected");
    assert!(matches!(
        err,
        WalletUiStateError::UnsupportedVersion { version }
            if version == WALLET_UI_STATE_VERSION + 1
    ));
    let loaded = load_wallet_ui_state(&store).expect("load UI state");
    assert_eq!(loaded, WalletUiState::default());

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn corrupt_ui_state_falls_back_to_empty() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    store
        .put_app_settings_record(WALLET_UI_STATE_KEY, &[0xc1])
        .expect("store corrupt UI state");

    let loaded = load_wallet_ui_state(&store).expect("load UI state");
    assert_eq!(loaded, WalletUiState::default());

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn unsupported_future_settings_version_is_rejected() {
    let settings = WalletSettings {
        version: WALLET_SETTINGS_VERSION + 1,
        ..WalletSettings::default()
    };
    let data = rmp_serde::to_vec_named(&settings).expect("encode future settings");

    let err = decode_wallet_settings(&data).expect_err("future version rejected");
    assert!(matches!(
        err,
        WalletSettingsError::UnsupportedVersion { version }
            if version == WALLET_SETTINGS_VERSION + 1
    ));
}

#[test]
fn validation_rejects_ambiguous_proxy_and_disabled_chains() {
    let mut settings = WalletSettings::default();
    settings.network.proxy_url = Some("http://127.0.0.1:9050".to_string());
    for chain in settings.chains.per_chain.values_mut() {
        chain.enabled = false;
    }

    let err = settings.validate().expect_err("settings invalid");
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("proxy_url"))
    );
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("at least one supported chain"))
    );
}

#[test]
fn auto_lock_settings_default_disabled_and_bounds_are_validated() {
    let mut settings = WalletSettings::default();
    assert_eq!(
        settings.runtime.auto_lock_timeout_secs,
        Some(DEFAULT_AUTO_LOCK_TIMEOUT_SECS)
    );
    assert!(
        AUTO_LOCK_TIMEOUT_PRESETS_SECS.contains(&DEFAULT_AUTO_LOCK_TIMEOUT_SECS),
        "the default must be exposed as a Settings preset"
    );

    settings.runtime.auto_lock_timeout_secs = None;
    settings.validate().expect("Disabled is valid");

    for timeout in [MIN_AUTO_LOCK_TIMEOUT_SECS, MAX_AUTO_LOCK_TIMEOUT_SECS] {
        settings.runtime.auto_lock_timeout_secs = Some(timeout);
        settings.validate().expect("timeout bound is valid");
    }

    for timeout in [
        MIN_AUTO_LOCK_TIMEOUT_SECS - 1,
        MAX_AUTO_LOCK_TIMEOUT_SECS + 1,
    ] {
        settings.runtime.auto_lock_timeout_secs = Some(timeout);
        let error = settings
            .validate()
            .expect_err("out-of-range timeout rejected");
        assert!(
            error
                .messages
                .iter()
                .any(|message| message.contains("runtime.auto_lock_timeout_secs"))
        );
    }
}

#[test]
fn reset_helpers_restore_defaults() {
    let mut settings = WalletSettings::default();
    settings.network.mode = super::NetworkModeSetting::Direct;
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.poi.artifact.gateway_urls.clear();
    settings.walletconnect.project_id_override = Some("custom-project".to_owned());

    settings.reset_network();
    settings.reset_indexed_artifacts();
    settings.reset_poi();
    settings.reset_walletconnect();

    assert_eq!(settings.network, super::NetworkSettings::default());
    assert_eq!(
        settings.indexed_artifacts,
        super::IndexedArtifactSettings::default()
    );
    assert_eq!(settings.poi, super::PoiSettings::default());
    assert_eq!(
        settings.walletconnect,
        super::WalletConnectSettings::default()
    );
}

#[test]
fn walletconnect_settings_use_default_or_project_id_override() {
    let mut settings = WalletSettings::default();

    assert_eq!(
        settings.walletconnect.effective_project_id(),
        WALLETCONNECT_DEFAULT_PROJECT_ID
    );

    settings.walletconnect.project_id_override = Some("user-project-id".to_owned());

    assert_eq!(
        settings.walletconnect.effective_project_id(),
        "user-project-id"
    );
}

#[test]
fn walletconnect_settings_reject_empty_project_id_override() {
    let mut settings = WalletSettings::default();
    settings.walletconnect.project_id_override = Some("   ".to_owned());

    let err = settings.validate().expect_err("empty override rejected");
    assert!(err.messages.iter().any(|message| {
        message.contains("walletconnect.project_id_override")
            && message.contains("must not be empty")
    }));
}

#[test]
fn walletconnect_settings_do_not_persist_custom_relay_url() {
    let mut settings = WalletSettings::default();
    settings.walletconnect.project_id_override = Some("user-project-id".to_owned());

    let encoded = encode_wallet_settings(&settings).expect("encode settings");
    let serialized = serde_json::to_value(&settings).expect("serialize settings");

    assert_eq!(
        serialized["walletconnect"]["project_id_override"],
        "user-project-id"
    );
    assert!(serialized["walletconnect"].get("relay_url").is_none());
    assert!(!String::from_utf8_lossy(&encoded).contains("relay_url"));
}

#[test]
fn effective_chain_configs_use_supported_presets_without_overrides() {
    let settings = WalletSettings::default();
    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(&1).expect("ethereum config");
    let defaults = ChainConfigDefaults::for_chain(1).expect("ethereum defaults");

    assert!(ethereum.enabled);
    assert_eq!(
        ethereum.rpc_endpoints,
        defaults
            .rpc_urls
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert!(ethereum.rpc_endpoints.len() > 1);
    assert_eq!(ethereum.finality_depth, defaults.finality_depth);
    assert!(ethereum.has_sponsorship_prerequisites());
    for chain_id in [56, 137, 42161] {
        let chain = configs.get(&chain_id).expect("supported chain config");
        assert!(chain.sponsored_bundle_relays.is_empty());
        assert_eq!(chain.coinbase_payer, None);
        assert!(!chain.has_sponsorship_prerequisites());
    }
    assert_eq!(ethereum.deployment_block, defaults.deployment_block);
    assert_eq!(ethereum.v2_start_block, defaults.v2_start_block);
    assert_eq!(ethereum.legacy_shield_block, defaults.legacy_shield_block);
    assert_eq!(ethereum.archive_until_block, defaults.archive_until_block);
    assert_eq!(ethereum.archive_rpc_url, None);
    assert_eq!(
        ethereum.quick_sync_endpoint,
        defaults.quick_sync_endpoint.map(|url| url.to_string())
    );
    assert_eq!(
        ethereum.multicall_contract,
        defaults.multicall_contract.to_string()
    );
    assert_eq!(
        ethereum.indexed_artifact_source_mode,
        IndexedArtifactSourceModeSetting::Official
    );
    let source = ethereum
        .indexed_artifact_source
        .as_ref()
        .expect("official indexed artifact source");
    assert_eq!(
        alloy::hex::encode(source.trusted_publisher_pubkey.as_slice()),
        OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY.trim_start_matches("0x")
    );
    assert!(matches!(
        &source.manifest_source,
        super::IndexedArtifactManifestSource::IpnsName(name)
            if name == OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME
    ));
    assert_eq!(
        source.gateway_urls.len(),
        OFFICIAL_INDEXED_ARTIFACT_GATEWAYS.len()
    );
}

#[test]
fn sponsored_relay_overrides_preserve_order_and_explicit_empty_disables() {
    let mut settings = WalletSettings::default();
    let inherited = build_effective_chain_configs(&settings).expect("inherited sponsored relays");
    assert!(
        !inherited
            .get(&1)
            .expect("ethereum config")
            .sponsored_bundle_relays
            .is_empty()
    );
    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .sponsored_bundle_relays = Some(vec![
        "https://user:secret@relay-a.example/path".to_string(),
        "http://relay-b.example".to_string(),
    ]);
    let configs = build_effective_chain_configs(&settings).expect("effective sponsored relays");
    let ethereum = configs.get(&1).expect("ethereum config");
    assert_eq!(ethereum.sponsored_bundle_relays.len(), 2);
    assert_eq!(
        ethereum.sponsored_bundle_relays[1].expose_url().as_str(),
        "http://relay-b.example/"
    );
    let debug = format!("{:?}", ethereum.sponsored_bundle_relays);
    assert!(!debug.contains("user"));
    assert!(!debug.contains("secret"));

    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .sponsored_bundle_relays = Some(Vec::new());
    let disabled = build_effective_chain_configs(&settings).expect("disabled sponsored relays");
    assert!(
        disabled
            .get(&1)
            .expect("ethereum config")
            .sponsored_bundle_relays
            .is_empty()
    );
}

#[test]
fn sponsored_settings_validate_relay_schemes_and_payer_addresses() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.sponsored_bundle_relays = Some(vec!["ws://relay.example".to_string()]);
    ethereum.contracts.coinbase_payer = Some("not-an-address".to_string());

    let error = settings.validate().expect_err("invalid sponsored settings");
    assert!(error.messages.iter().any(|message| {
        message.contains("sponsored_bundle_relays") && message.contains("http, https")
    }));
    assert!(
        error.messages.iter().any(|message| {
            message.contains("coinbase_payer") && message.contains("EVM address")
        })
    );
}

#[test]
fn sponsored_settings_roundtrip_all_override_states() {
    for relays in [
        None,
        Some(Vec::new()),
        Some(vec!["https://relay.example".to_string()]),
    ] {
        let mut settings = WalletSettings::default();
        let ethereum = settings
            .chains
            .per_chain
            .get_mut(&1)
            .expect("ethereum settings");
        ethereum.sponsored_bundle_relays = relays.clone();

        let encoded = encode_wallet_settings(&settings).expect("encode sponsored settings");
        let decoded = decode_wallet_settings(&encoded).expect("decode sponsored settings");
        let decoded = decoded
            .chains
            .per_chain
            .get(&1)
            .expect("decoded ethereum settings");
        assert_eq!(decoded.sponsored_bundle_relays, relays);
    }

    let mut settings = WalletSettings::default();
    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .contracts
        .coinbase_payer = Some("0x0000000000000000000000000000000000000001".to_string());
    let encoded = encode_wallet_settings(&settings).expect("encode payer override");
    let decoded = decode_wallet_settings(&encoded).expect("decode payer override");
    assert_eq!(
        decoded
            .chains
            .per_chain
            .get(&1)
            .expect("decoded ethereum settings")
            .contracts
            .coinbase_payer
            .as_deref(),
        Some("0x0000000000000000000000000000000000000001")
    );
}

#[test]
fn indexed_artifact_official_source_ignores_stored_custom_fields() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Official;
    settings.indexed_artifacts.publisher_pubkey = Some("not-hex".to_string());
    settings.indexed_artifacts.manifest_source = Some(IndexedArtifactManifestSourceSetting::Url(
        "ftp://artifacts.example/manifest.json".to_string(),
    ));
    settings.indexed_artifacts.gateway_urls = vec!["ftp://gateway.example".to_string()];
    settings.indexed_artifacts.max_manifest_age_secs = Some(0);
    settings.indexed_artifacts.concurrency = Some(0);
    settings.indexed_artifacts.max_in_flight_bytes = Some(1024 * 1024 * 1024 + 1);

    settings
        .validate()
        .expect("official mode ignores stored custom-only fields");
    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let source = configs
        .get(&1)
        .and_then(|config| config.indexed_artifact_source.as_ref())
        .expect("official indexed artifact source");

    assert_eq!(
        alloy::hex::encode(source.trusted_publisher_pubkey.as_slice()),
        OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY.trim_start_matches("0x")
    );
    assert!(matches!(
        &source.manifest_source,
        super::IndexedArtifactManifestSource::IpnsName(name)
            if name == OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME
    ));
    assert_eq!(
        source
            .gateway_urls
            .iter()
            .map(|url| url.as_str().to_string())
            .collect::<Vec<_>>(),
        OFFICIAL_INDEXED_ARTIFACT_GATEWAYS
            .iter()
            .map(|gateway| format!("{gateway}/"))
            .collect::<Vec<_>>()
    );
    assert_eq!(source.concurrency, DEFAULT_INDEXED_ARTIFACT_CONCURRENCY);
    assert_eq!(
        source.max_in_flight_bytes,
        DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES
    );
    assert_eq!(source.max_manifest_age, None);
}

#[test]
fn indexed_artifact_custom_source_builds_effective_config() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.indexed_artifacts.publisher_pubkey = Some(format!("0x{}", "11".repeat(32)));
    settings.indexed_artifacts.manifest_source = Some(IndexedArtifactManifestSourceSetting::Url(
        "https://artifacts.example/manifest.json".to_string(),
    ));
    settings.indexed_artifacts.gateway_urls = vec!["https://gateway.example".to_string()];
    settings.indexed_artifacts.concurrency = Some(5);
    settings.indexed_artifacts.max_in_flight_bytes = Some(8 * 1024 * 1024);
    settings.indexed_artifacts.max_manifest_age_secs = Some(3_600);

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(&1).expect("ethereum config");
    let source = ethereum
        .indexed_artifact_source
        .as_ref()
        .expect("indexed artifact source");

    assert_eq!(
        ethereum.indexed_artifact_source_mode,
        IndexedArtifactSourceModeSetting::Custom
    );
    assert_eq!(
        alloy::hex::encode(source.trusted_publisher_pubkey.as_slice()),
        "11".repeat(32)
    );
    assert!(matches!(
        &source.manifest_source,
        super::IndexedArtifactManifestSource::Url(url)
            if url.as_str() == "https://artifacts.example/manifest.json"
    ));
    assert_eq!(source.gateway_urls[0].as_str(), "https://gateway.example/");
    assert_eq!(source.concurrency, 5);
    assert_eq!(source.max_in_flight_bytes, 8 * 1024 * 1024);
    assert_eq!(
        source.max_manifest_age,
        Some(std::time::Duration::from_hours(1))
    );
}

#[test]
fn indexed_artifact_ipns_source_is_trimmed_in_effective_config() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.indexed_artifacts.publisher_pubkey = Some(format!("0x{}", "22".repeat(32)));
    settings.indexed_artifacts.manifest_source =
        Some(IndexedArtifactManifestSourceSetting::IpnsName(format!(
            "  {OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME}  "
        )));
    settings.indexed_artifacts.gateway_urls = vec!["https://gateway.example".to_string()];

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let source = configs
        .get(&1)
        .and_then(|config| config.indexed_artifact_source.as_ref())
        .expect("indexed artifact source");

    assert!(matches!(
        &source.manifest_source,
        super::IndexedArtifactManifestSource::IpnsName(name)
            if name == OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME
    ));
}

#[test]
fn indexed_artifact_defaults_apply_to_custom_source_limits() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.indexed_artifacts.publisher_pubkey = Some(format!("0x{}", "22".repeat(32)));
    settings.indexed_artifacts.manifest_source = Some(
        IndexedArtifactManifestSourceSetting::IpnsName("k51qzi5uqu5custom".to_string()),
    );
    settings.indexed_artifacts.gateway_urls = vec!["https://gateway.example".to_string()];

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let source = configs
        .get(&1)
        .and_then(|config| config.indexed_artifact_source.as_ref())
        .expect("indexed artifact source");

    assert_eq!(source.concurrency, DEFAULT_INDEXED_ARTIFACT_CONCURRENCY);
    assert_eq!(
        source.max_in_flight_bytes,
        DEFAULT_INDEXED_ARTIFACT_MAX_IN_FLIGHT_BYTES
    );
}

#[test]
fn indexed_artifact_custom_source_validation_rejects_missing_source() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.source_mode = IndexedArtifactSourceModeSetting::Custom;
    settings.indexed_artifacts.publisher_pubkey = Some("not-hex".to_string());
    settings.indexed_artifacts.manifest_source = Some(IndexedArtifactManifestSourceSetting::Url(
        "ftp://artifacts.example/manifest.json".to_string(),
    ));
    settings.indexed_artifacts.gateway_urls.clear();
    settings
        .indexed_artifacts
        .gateway_urls
        .push("ftp://gateway.example".to_string());
    settings.indexed_artifacts.concurrency = Some(0);
    settings.indexed_artifacts.max_in_flight_bytes = Some(1024 * 1024 * 1024 + 1);

    let err = settings
        .validate()
        .expect_err("bad indexed source rejected");

    assert!(err.messages.iter().any(|message| {
        message.contains("indexed_artifacts.publisher_pubkey") && message.contains("32-byte hex")
    }));
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("indexed_artifacts.manifest_source"))
    );
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("indexed_artifacts.gateway_urls"))
    );
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("indexed_artifacts.concurrency"))
    );
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("indexed_artifacts.max_in_flight_bytes"))
    );
}

#[test]
fn effective_chain_configs_apply_supported_overrides_in_order() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.rpc_endpoints = vec![
        "https://rpc-a.example".to_string(),
        "https://rpc-b.example".to_string(),
    ];
    ethereum.quick_sync.endpoint = Some("https://quick.example/graphql".to_string());
    ethereum.finality_depth = Some(64);
    ethereum.contracts.multicall_contract =
        Some("0x0000000000000000000000000000000000000001".to_string());
    ethereum.deployment.deployment_block = Some(11);
    ethereum.deployment.v2_start_block = Some(22);
    ethereum.deployment.legacy_shield_block = Some(33);
    ethereum.deployment.archive_until_block = Some(44);
    ethereum.deployment.archive_rpc_url = Some("https://archive.example".to_string());
    ethereum.gas.gas_limit_buffer = Some(250_000);

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(&1).expect("ethereum config");

    assert_eq!(
        ethereum.rpc_endpoints,
        vec!["https://rpc-a.example", "https://rpc-b.example"]
    );
    assert_eq!(
        ethereum.quick_sync_endpoint.as_deref(),
        Some("https://quick.example/graphql")
    );
    assert_eq!(ethereum.finality_depth, 64);
    assert_eq!(ethereum.deployment_block, 11);
    assert_eq!(ethereum.v2_start_block, 22);
    assert_eq!(ethereum.legacy_shield_block, 33);
    assert_eq!(ethereum.archive_until_block, 44);
    assert_eq!(
        ethereum.archive_rpc_url.as_deref(),
        Some("https://archive.example")
    );
    assert_eq!(
        ethereum.multicall_contract,
        "0x0000000000000000000000000000000000000001"
    );
    assert_eq!(ethereum.gas.gas_limit_buffer, 250_000);
}

#[test]
fn custom_railgun_contract_requires_deployment_metadata() {
    let mut settings = WalletSettings::default();
    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .contracts
        .railgun_contract = Some("0x0000000000000000000000000000000000000001".to_string());

    let err = settings
        .validate()
        .expect_err("deployment metadata required");
    assert!(
        err.messages
            .iter()
            .any(|message| { message.contains("chains.per_chain.1.deployment.deployment_block") })
    );
    assert!(should_show_chain_deployment_metadata_settings(
        1,
        settings
            .chains
            .per_chain
            .get(&1)
            .expect("ethereum settings")
    ));

    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.deployment.deployment_block = Some(11);
    ethereum.deployment.v2_start_block = Some(22);
    ethereum.deployment.legacy_shield_block = Some(33);
    ethereum.deployment.archive_until_block = Some(0);

    settings.validate().expect("metadata supplied");
}

#[test]
fn effective_chain_configs_apply_quick_sync_bounds_and_disabled_state() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.quick_sync.enabled = false;
    ethereum.quick_sync.indexed_wallet_block_range = Some(25_000);
    ethereum.block_range = Some(2_000);
    ethereum.poll_interval_secs = Some(30);

    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(&1).expect("ethereum config");

    assert!(!ethereum.quick_sync_enabled);
    assert_eq!(ethereum.indexed_wallet_block_range, 25_000);
    assert_eq!(ethereum.block_range, Some(2_000));
    assert_eq!(ethereum.poll_interval_secs, Some(30));
}

#[test]
fn chain_reset_restores_supported_chain_defaults() {
    let mut settings = WalletSettings::default();
    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .enabled = false;

    settings.reset_chains();

    assert_eq!(settings.chains, super::ChainSettings::default());
    assert!(settings.chains.enabled_chain_ids().contains(&1));
}

#[test]
fn effective_chain_configs_reject_unsupported_chain_ids() {
    let mut settings = WalletSettings::default();
    settings
        .chains
        .per_chain
        .insert(999, super::ChainSettingsOverride::default());

    let err = build_effective_chain_configs(&settings).expect_err("unsupported chain rejected");
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("custom chain IDs are out of scope"))
    );
}

#[test]
fn effective_token_registry_applies_overrides_tombstones_and_custom_tokens() {
    let mut settings = WalletSettings::default();
    let weth = super::TokenKey {
        chain_id: 1,
        token_address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
    };
    settings
        .tokens
        .built_in_overrides
        .push(super::BuiltInTokenOverride {
            key: weth,
            symbol: Some("WETHx".to_string()),
            decimals: Some(18),
            icon_path: None,
            price_anchor: Some(super::PriceAnchorSettings::Fixed {
                rate: "2000000000000000000".to_string(),
            }),
        });
    settings.tokens.built_in_tombstones.push(super::TokenKey {
        chain_id: 1,
        token_address: "0xdAC17F958D2ee523a2206206994597C13D831ec7".to_string(),
    });
    settings
        .tokens
        .custom_tokens
        .push(super::CustomTokenSettings {
            chain_id: 1,
            token_address: "0x0000000000000000000000000000000000000002".to_string(),
            symbol: "CSTM".to_string(),
            decimals: 9,
            icon_path: None,
            price_anchor: None,
        });

    let registry = build_effective_token_registry(&settings).expect("build token registry");
    let weth = registry
        .tokens
        .get(&(1, "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string()))
        .expect("weth token");
    assert_eq!(weth.symbol, "WETHx");
    assert!(weth.price_anchor.is_some());
    assert!(
        !registry
            .tokens
            .contains_key(&(1, "0xdac17f958d2ee523a2206206994597c13d831ec7".to_string()))
    );
    let custom = registry
        .tokens
        .get(&(1, "0x0000000000000000000000000000000000000002".to_string()))
        .expect("custom token");
    assert!(!custom.built_in);
    assert_eq!(custom.decimals, 9);
}

#[test]
fn effective_token_registry_includes_static_price_anchor_defaults() {
    let settings = WalletSettings::default();

    let registry = build_effective_token_registry(&settings).expect("build token registry");

    let weth = registry
        .tokens
        .get(&(1, "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".to_string()))
        .expect("weth token");
    assert_eq!(
        weth.price_anchor,
        Some(super::PriceAnchorSettings::Fixed {
            rate: "1000000000000000000".to_string(),
        })
    );

    let usdt = registry
        .tokens
        .get(&(1, "0xdac17f958d2ee523a2206206994597c13d831ec7".to_string()))
        .expect("usdt token");
    assert!(matches!(
        usdt.price_anchor,
        Some(super::PriceAnchorSettings::Oracle {
            chain_id: 1,
            token_decimals: 6,
            oracle_decimals: 8,
            is_inversed: false,
            ..
        })
    ));
}

#[test]
fn broadcaster_settings_build_fee_policy_and_validate_thresholds() {
    let mut settings = WalletSettings::default();
    settings.broadcaster.min_anchor_bps = 9_000;
    settings.broadcaster.max_anchor_bps = 11_000;
    settings
        .broadcaster
        .allow_suspicious_broadcasters_by_default = true;
    settings.broadcaster.response_timeout_secs = 45;

    settings.validate().expect("broadcaster settings valid");
    let policy = settings.broadcaster.fee_policy();
    assert_eq!(policy.min_anchor_bps, 9_000);
    assert_eq!(policy.max_anchor_bps, 11_000);
    assert!(policy.allow_suspicious_broadcasters);

    settings.broadcaster.min_anchor_bps = 12_000;
    let err = settings
        .validate()
        .expect_err("invalid thresholds rejected");
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("min_anchor_bps"))
    );
}

#[test]
fn price_anchor_validation_covers_oracle_and_product_metadata() {
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(super::TokenPriceAnchorOverride {
            key: super::TokenKey {
                chain_id: 1,
                token_address: "0x0000000000000000000000000000000000000002".to_string(),
            },
            price_anchor: super::PriceAnchorSettings::Product {
                scale_decimals: 18,
                components: vec![super::PriceAnchorSettings::Oracle {
                    chain_id: 1,
                    oracle_address: "0x0000000000000000000000000000000000000003".to_string(),
                    token_decimals: 18,
                    oracle_decimals: 8,
                    is_inversed: false,
                }],
            },
        });

    settings.validate().expect("anchor metadata valid");

    let super::PriceAnchorSettings::Product { components, .. } = &mut settings
        .tokens
        .price_anchors
        .first_mut()
        .expect("price anchor")
        .price_anchor
    else {
        panic!("expected product anchor");
    };
    let super::PriceAnchorSettings::Oracle {
        oracle_decimals, ..
    } = &mut components[0]
    else {
        panic!("expected oracle anchor");
    };
    *oracle_decimals = 37;

    let err = settings
        .validate()
        .expect_err("bad oracle decimals rejected");
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("oracle_decimals"))
    );
}

#[test]
fn uniswap_v3_twap_settings_round_trip_and_validate_boundaries() {
    let anchor = super::PriceAnchorSettings::UniswapV3Twap {
        pool_address: "0x0000000000000000000000000000000000000400".to_string(),
        base_token_address: "0x0000000000000000000000000000000000000401".to_string(),
        quote_token_address: "0x0000000000000000000000000000000000000402".to_string(),
        base_token_decimals: 18,
        window_seconds: 1_800,
    };
    let wire = serde_json::to_value(&anchor).expect("serialize TWAP settings");
    assert_eq!(wire["type"], json!("uniswap-v3-twap"));
    assert!(wire.get("chain_id").is_none());
    assert_eq!(
        wire["pool_address"],
        json!("0x0000000000000000000000000000000000000400")
    );
    assert_eq!(
        serde_json::from_value::<super::PriceAnchorSettings>(wire).expect("deserialize"),
        anchor
    );

    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(super::TokenPriceAnchorOverride {
            key: super::TokenKey {
                chain_id: 1,
                token_address: "0x0000000000000000000000000000000000000002".to_string(),
            },
            price_anchor: anchor.clone(),
        });
    settings.validate().expect("supported TWAP accepted");
    let mut invalid = anchor;
    let super::PriceAnchorSettings::UniswapV3Twap {
        pool_address,
        base_token_address,
        quote_token_address,
        base_token_decimals,
        window_seconds,
    } = &mut invalid
    else {
        unreachable!();
    };
    *pool_address = "not-an-address".to_string();
    *base_token_address = "not-an-address".to_string();
    *quote_token_address = "not-an-address".to_string();
    *base_token_decimals = 37;
    *window_seconds = 0;
    settings.tokens.price_anchors[0].price_anchor = invalid;
    let error = settings.validate().expect_err("invalid TWAP rejected");
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("pool_address"))
    );
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("base_token_address"))
    );
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("quote_token_address"))
    );
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("base_token_decimals"))
    );
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("window_seconds"))
    );
}

#[test]
fn legacy_anchor_settings_remain_compatible_and_rail_twap_override_resets() {
    let legacy = vec![
        json!({"type":"fixed", "rate":"1000000000000000000"}),
        json!({"type":"oracle", "chain_id":1, "oracle_address":"0x0000000000000000000000000000000000000003", "token_decimals":18, "oracle_decimals":8, "is_inversed":false}),
        json!({"type":"product", "components":[{"type":"fixed", "rate":"2"}], "scale_decimals":18}),
        json!({"type":"uniswap-v3-twap", "chain_id":999, "pool_address":"0x0000000000000000000000000000000000000400", "base_token_address":"0x0000000000000000000000000000000000000401", "quote_token_address":"0x0000000000000000000000000000000000000402", "base_token_decimals":18, "window_seconds":1800}),
    ];
    for value in legacy {
        serde_json::from_value::<super::PriceAnchorSettings>(value).expect("legacy anchor");
    }
    let rail = super::TokenKey {
        chain_id: 1,
        token_address: "0xe76C6c83af64e4C60245D8C7dE953DF673a7A33D".to_string(),
    };
    let mut settings = WalletSettings::default();
    settings
        .tokens
        .price_anchors
        .push(super::TokenPriceAnchorOverride {
            key: rail.clone(),
            price_anchor: super::PriceAnchorSettings::Fixed {
                rate: "7".to_string(),
            },
        });
    let registry = build_effective_token_registry(&settings).expect("override registry");
    assert!(
        matches!(registry.tokens.get(&(1, rail.token_address.to_ascii_lowercase())).and_then(|token| token.price_anchor.as_ref()), Some(super::PriceAnchorSettings::Fixed { rate }) if rate == "7")
    );
    settings.tokens.price_anchors.clear();
    let registry = build_effective_token_registry(&settings).expect("reset registry");
    assert!(matches!(
        registry
            .tokens
            .get(&(1, rail.token_address.to_ascii_lowercase()))
            .and_then(|token| token.price_anchor.as_ref()),
        Some(super::PriceAnchorSettings::UniswapV3Twap {
            window_seconds: 1_800,
            ..
        })
    ));
}

#[test]
fn default_poi_read_source_converts_to_official_indexed_artifacts() {
    let settings = WalletSettings::default();
    let super::PoiReadSource::IndexedArtifacts {
        artifact_source: source,
        wallet_read_fallback,
        ..
    } = settings.poi_read_source().expect("POI source")
    else {
        panic!("default POI source should be indexed artifacts");
    };

    assert_eq!(
        alloy::hex::encode(source.trusted_publisher_pubkey.as_slice()),
        OFFICIAL_POI_ARTIFACT_PUBLISHER_PUBKEY.trim_start_matches("0x")
    );
    assert_eq!(
        source.manifest_source,
        super::PoiArtifactManifestSource::IpnsName(OFFICIAL_POI_ARTIFACT_IPNS_NAME.to_string())
    );
    assert_eq!(
        source.gateway_urls.len(),
        OFFICIAL_POI_ARTIFACT_GATEWAYS.len()
    );
    assert_eq!(wallet_read_fallback, super::PoiProxyFallback::Disabled);
    assert_ne!(
        OFFICIAL_POI_ARTIFACT_IPNS_NAME,
        LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME
    );
    assert_ne!(
        OFFICIAL_POI_ARTIFACT_IPNS_NAME,
        OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME
    );
}

#[test]
fn custom_ppoi_artifact_source_converts_without_official_substitution() {
    let mut settings = WalletSettings::default();
    let custom_publisher = format!("0x{}", "42".repeat(32));
    let custom_ipns = "k51qzi5uqu5dicmabkge4lkunc4bkd198u9xicp5espmw5zdzbafkez7hyh5ft".to_string();
    settings
        .poi
        .artifact
        .publisher_pubkey
        .clone_from(&custom_publisher);
    settings.poi.artifact.manifest_source =
        PoiArtifactManifestSourceSetting::IpnsName(custom_ipns.clone());

    let super::PoiReadSource::IndexedArtifacts {
        artifact_source: source,
        wallet_read_fallback,
        ..
    } = settings.poi_read_source().expect("custom POI source")
    else {
        panic!("custom POI source should use indexed artifacts");
    };

    assert_eq!(
        alloy::hex::encode_prefixed(source.trusted_publisher_pubkey.as_slice()),
        custom_publisher
    );
    assert_eq!(
        source.manifest_source,
        super::PoiArtifactManifestSource::IpnsName(custom_ipns)
    );
    assert_eq!(wallet_read_fallback, super::PoiProxyFallback::Disabled);
}

#[test]
fn invalid_or_non_ipns_ppoi_artifact_sources_are_rejected_in_settings() {
    for source in [
        PoiArtifactManifestSourceSetting::IpnsName("not-an-ipns-name".to_string()),
        PoiArtifactManifestSourceSetting::Cid(
            "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku".to_string(),
        ),
        PoiArtifactManifestSourceSetting::Url("https://example.invalid/manifest".to_string()),
    ] {
        let mut settings = WalletSettings::default();
        settings.poi.artifact.manifest_source = source;

        let error = settings
            .poi_read_source()
            .expect_err("non-IPNS PPOI source must fail settings validation");
        assert!(
            error
                .messages
                .iter()
                .any(|message| message.contains("manifest_source"))
        );
    }
}

#[test]
fn explicit_poi_proxy_mode_remains_selectable_without_artifact_fallback() {
    let mut settings = legacy_official_settings();
    settings.poi.read_source = PoiReadSourceSetting::PoiProxy;
    settings.poi.proxy.rpc_url = "https://explicit-poi-proxy.example/rpc".to_string();

    let super::PoiReadSource::PoiProxy { rpc_url } =
        settings.poi_read_source().expect("explicit proxy source")
    else {
        panic!("explicit proxy mode should remain selected");
    };
    assert_eq!(
        rpc_url,
        super::SensitiveUrl::from(
            reqwest::Url::parse("https://explicit-poi-proxy.example/rpc")
                .expect("explicit proxy URL")
        )
    );
}

#[test]
fn default_poi_rpc_url_matches_default_poi_service() {
    let settings = WalletSettings::default();

    assert_eq!(
        settings.poi_rpc_url().expect("POI RPC URL"),
        super::SensitiveUrl::from(
            reqwest::Url::parse(poi::poi::DEFAULT_WALLET_POI_RPC_URL).expect("default POI RPC URL")
        )
    );
}

#[test]
fn custom_poi_rpc_url_is_runtime_url() {
    let mut settings = WalletSettings::default();
    settings.poi.proxy.rpc_url = "https://poi.example/rpc".to_string();

    assert_eq!(
        settings.poi_rpc_url().expect("custom POI RPC URL"),
        super::SensitiveUrl::from(
            reqwest::Url::parse("https://poi.example/rpc").expect("custom POI RPC URL")
        )
    );
}

#[test]
fn poi_runtime_policy_formatting_redacts_configured_endpoints() {
    let mut settings = WalletSettings::default();
    settings.poi.proxy.rpc_url =
        "https://rpc-user-sentinel:rpc-password-sentinel@rpc-host-sentinel.invalid/rpc-path-sentinel?rpc-query-sentinel#rpc-fragment-sentinel".to_string();
    settings.poi.artifact.manifest_source = super::PoiArtifactManifestSourceSetting::Url(
        "https://manifest-user-sentinel:manifest-password-sentinel@manifest-host-sentinel.invalid/manifest-path-sentinel?manifest-query-sentinel#manifest-fragment-sentinel".to_string(),
    );
    settings.poi.artifact.gateway_urls = vec![
        "https://gateway-user-sentinel:gateway-password-sentinel@gateway-host-sentinel.invalid/gateway-path-sentinel?gateway-query-sentinel#gateway-fragment-sentinel".to_string(),
    ];

    let policy = super::PoiReadSource::IndexedArtifacts {
        artifact_source: settings.poi.artifact.source_config(),
        rpc_url: settings.poi_rpc_url().expect("sensitive POI RPC URL"),
        wallet_read_fallback: super::PoiProxyFallback::Disabled,
    };
    let formatted = format!("{policy:?}");
    for sentinel in [
        "rpc-user-sentinel",
        "rpc-password-sentinel",
        "rpc-host-sentinel",
        "rpc-path-sentinel",
        "rpc-query-sentinel",
        "rpc-fragment-sentinel",
        "manifest-user-sentinel",
        "manifest-password-sentinel",
        "manifest-host-sentinel",
        "manifest-path-sentinel",
        "manifest-query-sentinel",
        "manifest-fragment-sentinel",
        "gateway-user-sentinel",
        "gateway-password-sentinel",
        "gateway-host-sentinel",
        "gateway-path-sentinel",
        "gateway-query-sentinel",
        "gateway-fragment-sentinel",
    ] {
        assert!(
            !formatted.contains(sentinel),
            "leaked {sentinel}: {formatted}"
        );
    }
    assert!(settings.poi.proxy.rpc_url.contains("rpc-user-sentinel"));
    assert!(matches!(
        settings.poi.artifact.manifest_source,
        super::PoiArtifactManifestSourceSetting::Url(ref url)
            if url.contains("manifest-user-sentinel")
    ));
    assert!(settings.poi.artifact.gateway_urls[0].contains("gateway-user-sentinel"));
}

#[test]
fn waku_settings_defaults_match_startup_defaults() {
    let settings = WalletSettings::default();
    assert_eq!(settings.waku.cluster_id, super::DEFAULT_WAKU_CLUSTER_ID);
    assert_eq!(settings.waku.shard_id, super::DEFAULT_WAKU_SHARD_ID);
    assert!(settings.waku.dns_enr_trees.is_none());
    assert!(settings.waku.direct_peers.is_none());
    assert!(settings.waku.doh_endpoint.is_none());
    assert!(settings.waku.doh_fallback_endpoints.is_none());
    assert_eq!(settings.waku.max_peers, super::DEFAULT_WAKU_MAX_PEERS);
    assert_eq!(
        settings.waku.peer_connection_timeout_secs,
        super::DEFAULT_WAKU_PEER_CONNECTION_TIMEOUT_SECS
    );
}

#[test]
fn waku_dns_enr_trees_validate_scheme() {
    let mut settings = WalletSettings::default();
    settings.waku.dns_enr_trees = Some(vec!["https://bad.example".to_string()]);

    let err = settings.validate().expect_err("bad DNS ENR tree rejected");

    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("waku.dns_enr_trees[0]"))
    );
}

#[test]
fn default_waku_direct_peer_is_valid() {
    let peers = super::default_waku_direct_peers();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].peer_id, super::DEFAULT_WAKU_DIRECT_PEER_ID);
    assert_eq!(peers[0].addr, super::DEFAULT_WAKU_DIRECT_PEER_ADDR);

    let mut settings = WalletSettings::default();
    settings.waku.direct_peers = Some(peers);
    settings.validate().expect("default direct peer is valid");
}

#[test]
fn waku_direct_peers_validate_peer_id_and_multiaddr() {
    let mut settings = WalletSettings::default();
    settings.waku.direct_peers = Some(vec![WakuDirectPeerSetting {
        peer_id: "not-a-peer-id".to_string(),
        addr: "not-a-multiaddr".to_string(),
    }]);

    let err = settings.validate().expect_err("bad direct peer rejected");

    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("waku.direct_peers[0].peer_id"))
    );
    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("waku.direct_peers[0].addr"))
    );
}

#[test]
fn waku_doh_fallback_endpoints_validate_url_schemes() {
    let mut settings = WalletSettings::default();
    settings.waku.doh_fallback_endpoints = Some(vec!["ftp://bad.example/dns-query".to_string()]);

    let err = settings
        .validate()
        .expect_err("bad DoH fallback scheme rejected");

    assert!(
        err.messages
            .iter()
            .any(|message| message.contains("waku.doh_fallback_endpoints[0]"))
    );
}

#[test]
fn encoded_settings_decode_without_db() {
    let settings = WalletSettings::default();
    let data = encode_wallet_settings(&settings).expect("encode settings");
    let decoded = decode_wallet_settings(&data).expect("decode settings");
    assert_eq!(decoded, settings);
}
