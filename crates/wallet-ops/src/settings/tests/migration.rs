use super::*;

fn put_unmigrated_settings(store: &DbStore, settings: &WalletSettings) {
    let payload = released_v4_settings_payload(settings);
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .expect("store unmigrated settings");
}

fn released_settings_payload(settings: &WalletSettings, version: u32) -> Vec<u8> {
    rmp_serde::to_vec_named(&ReleasedV4WalletSettingsWire {
        version,
        network: settings.network.clone(),
        privacy: settings.privacy.clone(),
        chains: released_chains(&settings.chains),
        indexed_artifacts: settings.indexed_artifacts.clone(),
        poi: settings.poi.clone(),
        broadcaster: settings.broadcaster.clone(),
        tokens: settings.tokens.clone(),
        gas: settings.gas.clone(),
        runtime: settings.runtime.clone(),
        waku: settings.waku.clone(),
        walletconnect: settings.walletconnect.clone(),
    })
    .expect("encode released v4 settings")
}

fn released_v4_settings_payload(settings: &WalletSettings) -> Vec<u8> {
    released_settings_payload(settings, 4)
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
    ethereum.rpc_endpoints = vec!["https://existing-rpc.example".to_string().into()];
    ethereum.contracts.wrapped_native_token =
        Some("0x0000000000000000000000000000000000000001".to_string());
    let chains = released_chains(&settings.chains)
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
fn version_5_previous_poi_defaults_migrate_to_pinata_and_persist() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.poi.artifact.gateway_urls = vec![
        "https://dweb.link".to_string(),
        "https://ipfs.filebase.io".to_string(),
        "https://ipfs.io".to_string(),
    ];
    let released_payload = released_settings_payload(&settings, 5);
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &released_payload)
        .expect("store released v5 settings");

    let migrated = load_wallet_settings(&store).expect("migrate released v5 settings");
    assert_eq!(migrated.version, WALLET_SETTINGS_VERSION);
    assert_eq!(
        migrated.poi.artifact.gateway_urls,
        OFFICIAL_POI_ARTIFACT_GATEWAYS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
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
fn version_5_customized_poi_gateways_are_preserved() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.poi.artifact.gateway_urls = vec![
        "https://custom-second.example".to_string(),
        "https://custom-first.example/path".to_string(),
    ];
    let released_payload = released_settings_payload(&settings, 5);
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &released_payload)
        .expect("store released v5 settings");

    let migrated = load_wallet_settings(&store).expect("migrate released v5 settings");
    assert_eq!(migrated.version, WALLET_SETTINGS_VERSION);
    assert_eq!(
        migrated.poi.artifact.gateway_urls,
        settings.poi.artifact.gateway_urls
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
fn version_5_current_poi_default_is_not_overwritten_by_legacy_gateways() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.poi.artifact.gateway_urls = OFFICIAL_POI_ARTIFACT_GATEWAYS
        .iter()
        .map(ToString::to_string)
        .collect();
    settings.indexed_artifacts.gateway_urls = vec![
        "https://legacy-first.example/path".to_string(),
        "http://legacy-second.example".to_string(),
    ];
    let released_payload = released_settings_payload(&settings, 5);
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &released_payload)
        .expect("store released v5 settings");

    let migrated = load_wallet_settings(&store).expect("migrate released v5 settings");
    assert_eq!(migrated.version, WALLET_SETTINGS_VERSION);
    assert_eq!(
        migrated.poi.artifact.gateway_urls,
        settings.poi.artifact.gateway_urls
    );
    assert_eq!(
        migrated.indexed_artifacts.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );

    let persisted = store
        .get_app_settings_record(WALLET_SETTINGS_KEY)
        .expect("read migrated settings")
        .expect("migrated settings record");
    let decoded = decode_wallet_settings(&persisted).expect("decode migrated settings");
    assert_eq!(
        decoded.poi.artifact.gateway_urls,
        settings.poi.artifact.gateway_urls
    );
    assert_eq!(
        decoded.indexed_artifacts.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn legacy_indexed_gateways_promote_into_default_poi_gateways_and_persist() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.poi.artifact.gateway_urls = vec![
        "https://dweb.link".to_string(),
        "https://ipfs.filebase.io".to_string(),
        "https://ipfs.io".to_string(),
    ];
    settings.indexed_artifacts.gateway_urls = vec![
        "https://legacy-first.example".to_string(),
        "http://legacy-second.example/path".to_string(),
    ];
    put_unmigrated_settings(&store, &settings);

    let migrated = load_wallet_settings(&store).expect("load and migrate settings");
    assert_eq!(
        migrated.poi.artifact.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );
    assert_eq!(
        migrated.indexed_artifacts.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );
    let persisted = store
        .get_app_settings_record(WALLET_SETTINGS_KEY)
        .expect("read migrated settings")
        .expect("migrated settings record");
    assert_eq!(
        decode_wallet_settings(&persisted).expect("decode migrated settings"),
        migrated
    );

    let mut reset = migrated;
    reset.poi.artifact.gateway_urls = OFFICIAL_POI_ARTIFACT_GATEWAYS
        .iter()
        .map(ToString::to_string)
        .collect();
    save_wallet_settings(&store, &reset).expect("save reset gateways");
    let reloaded = load_wallet_settings(&store).expect("reload reset gateways");
    assert_eq!(
        reloaded.poi.artifact.gateway_urls,
        reset.poi.artifact.gateway_urls
    );
    assert_eq!(
        reloaded.indexed_artifacts.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn customized_poi_gateways_win_over_legacy_indexed_gateways() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.poi.artifact.gateway_urls = vec!["https://custom-poi.example".to_string()];
    settings.indexed_artifacts.gateway_urls = vec!["https://legacy-indexed.example".to_string()];
    put_unmigrated_settings(&store, &settings);

    let loaded = load_wallet_settings(&store).expect("load customized settings");
    assert_eq!(
        loaded.poi.artifact.gateway_urls,
        settings.poi.artifact.gateway_urls
    );
    assert_eq!(
        loaded.indexed_artifacts.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn invalid_legacy_indexed_gateways_are_not_promoted() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.gateway_urls = vec!["ftp://legacy-indexed.example".to_string()];
    put_unmigrated_settings(&store, &settings);

    let loaded = load_wallet_settings(&store).expect("load settings");
    assert_eq!(
        loaded.poi.artifact.gateway_urls,
        OFFICIAL_POI_ARTIFACT_GATEWAYS
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        loaded.indexed_artifacts.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn released_indexed_gateway_field_roundtrips_without_runtime_use() {
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.gateway_urls = vec!["https://legacy-indexed.example".to_string()];

    let encoded = encode_wallet_settings(&settings).expect("encode settings");
    let decoded = decode_wallet_settings(&encoded).expect("decode settings");
    assert_eq!(
        decoded.indexed_artifacts.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );
}

#[test]
fn current_settings_do_not_promote_legacy_indexed_gateways() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut settings = WalletSettings::default();
    settings.indexed_artifacts.gateway_urls = vec!["https://legacy-indexed.example".to_string()];
    let payload = encode_wallet_settings(&settings).expect("encode current settings");
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .expect("store current settings");

    let loaded = load_wallet_settings(&store).expect("load current settings");
    assert_eq!(
        loaded.poi.artifact.gateway_urls,
        settings.poi.artifact.gateway_urls
    );
    assert_eq!(
        loaded.indexed_artifacts.gateway_urls,
        settings.indexed_artifacts.gateway_urls
    );
    assert_eq!(
        store
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .expect("read current settings")
            .expect("current settings record"),
        payload
    );

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
        let crate::settings::PoiReadSource::IndexedArtifacts {
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
        assert_eq!(
            wallet_read_fallback,
            crate::settings::PoiProxyFallback::Disabled
        );
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
    let official = crate::settings::IndexedArtifactSettings::official_preset();
    let default = crate::settings::IndexedArtifactSettings::default();

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
    settings.network.mode = crate::settings::NetworkModeSetting::Direct;
    settings.privacy.mimic_railway_shields_by_default = true;
    settings.privacy.unwrap_unshields_by_default = true;
    settings.poi.read_source = PoiReadSourceSetting::PoiProxy;

    save_wallet_settings(&store, &settings).expect("save settings");
    let loaded = load_wallet_settings(&store).expect("load settings");
    assert!(loaded.privacy.mimic_railway_shields_by_default);
    assert_eq!(loaded, settings);

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn version_9_settings_migrate_with_unwrap_disabled_and_preserve_preferences() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut expected = WalletSettings::default();
    // The released record has no entry for the newly added testnet.
    expected.chains.per_chain.remove(&11_155_111);
    expected.privacy.mimic_railway_shields_by_default = true;
    expected.waku.direct_peers = Some(Vec::new());
    expected.waku.backup_peers = Some(vec![WakuDirectPeerSetting {
        peer_id: crate::settings::DEFAULT_WAKU_BACKUP_PEER_ID.into(),
        addr: "/dns4/custom.example/tcp/8000/wss".into(),
    }]);
    let released = json!({
        "version": 9,
        "privacy": { "mimic_railway_shields_by_default": true },
        "waku": expected.waku,
    });
    let payload = rmp_serde::to_vec_named(&released).expect("encode released settings");
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .expect("store released settings");

    let migrated = load_wallet_settings(&store).expect("migrate settings");
    assert!(!migrated.privacy.unwrap_unshields_by_default);
    assert_eq!(migrated, expected);
    let rewritten = store
        .get_app_settings_record(WALLET_SETTINGS_KEY)
        .unwrap()
        .unwrap();
    assert_ne!(rewritten, payload);
    assert_eq!(load_wallet_settings(&store).unwrap(), expected);

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
    assert_eq!(
        ethereum.rpc_endpoints,
        vec![crate::settings::RpcEndpointSetting::from(
            "https://existing-rpc.example".to_owned()
        )]
    );
    assert_eq!(ethereum.railgun.sponsored_bundle_relays, None);
    let effective = build_effective_chain_configs(&migrated).expect("effective migrated settings");
    let ethereum = effective.get(1).expect("effective ethereum");
    assert_eq!(
        ethereum
            .require_railgun()
            .unwrap()
            .sponsored_bundle_relays
            .len(),
        2
    );
    assert_eq!(
        ethereum.require_railgun().unwrap().coinbase_payer,
        crate::settings::default_coinbase_payer(1)
    );
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
        chains: released_chains(&settings.chains),
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
