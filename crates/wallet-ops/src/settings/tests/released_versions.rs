use super::*;

#[test]
fn released_v11_endpoint_urls_load_with_default_flags_and_entries_keep_theirs() {
    let url = "https://rpc-a.example";
    let mut settings = WalletSettings::default();
    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .rpc_endpoints = vec![url.to_owned().into()];
    settings.chains.custom.insert(999_999, custom_evm_chain());
    // Released v11 wrote each endpoint as its bare URL.
    let mut released = encode_wallet_settings(&settings).expect("encode settings");
    for endpoint in [url, "http://127.0.0.1:8545"] {
        let entry = rmp_serde::to_vec_named(&crate::settings::RpcEndpointSetting::from(
            endpoint.to_owned(),
        ))
        .expect("encode entry");
        let bare = rmp_serde::to_vec_named(endpoint).expect("encode URL");
        let at = released
            .windows(entry.len())
            .position(|window| window == entry.as_slice())
            .expect("entry in the record");
        released = [
            &released[..at],
            bare.as_slice(),
            &released[at + entry.len()..],
        ]
        .concat();
    }
    let loaded = decode_wallet_settings(&released).expect("decode released v11 settings");
    for endpoint in [
        &loaded.chains.per_chain[&1].rpc_endpoints[0],
        &loaded.chains.custom[&999_999].rpc_endpoints[0],
    ] {
        assert!(endpoint.general && endpoint.sensitive && endpoint.submit && !endpoint.direct);
    }
    assert_eq!(loaded, settings);

    let ethereum = settings.chains.per_chain.get_mut(&1).unwrap();
    ethereum.rpc_endpoints[0] = crate::settings::RpcEndpointSetting {
        url: url.to_owned(),
        direct: true,
        general: false,
        sensitive: true,
        submit: false,
    };
    let encoded = encode_wallet_settings(&settings).expect("encode flagged settings");
    assert_eq!(decode_wallet_settings(&encoded).unwrap(), settings);
}

#[test]
fn settings_without_direct_endpoints_keep_the_released_shape() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.rpc_endpoints = vec!["https://rpc-a.example".to_string().into()];
    ethereum.railgun.deployment.archive_rpc_url = Some("https://archive.example".to_string());
    settings.chains.custom.insert(999_999, custom_evm_chain());

    let payload = rmp_serde::to_vec_named(&settings).expect("encode settings");
    // Released v11 builds reject unknown fields, so an absent value must not be written.
    let key = b"archive_rpc_direct".as_slice();
    assert!(!payload.windows(key.len()).any(|window| window == key));
    let decoded = decode_wallet_settings(&payload).expect("decode settings");
    assert_eq!(decoded, settings);
    let configs = build_effective_chain_configs(&decoded).expect("build effective configs");
    assert!(configs.values().all(|chain| {
        chain
            .rpc_route
            .transport_endpoints()
            .all(|(_, transport)| transport == RpcTransport::Wallet)
    }));
    assert_eq!(
        configs
            .get(1)
            .unwrap()
            .require_railgun()
            .unwrap()
            .archive_rpc_transport,
        RpcTransport::Wallet
    );
}

#[test]
fn released_waku_peer_choices_migrate_and_stay_independent_on_reload() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    // Frozen released default: migration must still recognize it if future defaults change.
    let old_default = vec![crate::settings::WakuDirectPeerSetting {
        peer_id: "16Uiu2HAkwhijhoc4UxAJD4fmYgSX91FzSDqehAaxJogYFcyo736a".into(),
        addr: "/dns4/baaamooobaaa.mooo.com/tcp/8000/wss".into(),
    }];
    let mut custom = old_default.clone();
    custom[0].addr = "/dns4/custom.example/tcp/8000/wss".into();
    let choices = [
        None,
        Some(Vec::new()),
        Some(old_default.clone()),
        Some(custom.clone()),
        Some([old_default.clone(), custom.clone()].concat()),
    ];
    // No version means released v6. Exercise both legacy conversion branches and absent/null fields.
    for version in 0..=8 {
        for direct in &choices {
            for omit_direct in [false, true] {
                if omit_direct && direct.is_some() {
                    continue;
                }
                let mut wire = json!({
                    "version": version,
                    "runtime": { "public_balance_refresh_interval_secs": 47 },
                    "waku": { "direct_peers": direct, "max_peers": 7 }
                });
                if version == 0 {
                    wire.as_object_mut().unwrap().remove("version");
                }
                if omit_direct {
                    wire["waku"].as_object_mut().unwrap().remove("direct_peers");
                }
                let payload = rmp_serde::to_vec_named(&wire).unwrap();
                store
                    .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
                    .unwrap();

                let migrated = load_wallet_settings(&store).unwrap();
                if direct.as_ref() == Some(&old_default) {
                    assert_eq!(migrated.waku.direct_peers, None);
                    assert_eq!(migrated.waku.backup_peers, Some(old_default.clone()));
                } else {
                    assert_eq!(&migrated.waku.direct_peers, direct);
                    assert_eq!(
                        migrated.waku.backup_peers,
                        direct.as_ref().map(|_| Vec::new())
                    );
                }
                assert_eq!(migrated.runtime.public_balance_refresh_interval_secs, 47);
                assert_eq!(migrated.waku.max_peers, 7);
                let persisted = store
                    .get_app_settings_record(WALLET_SETTINGS_KEY)
                    .unwrap()
                    .unwrap();
                assert_ne!(persisted, payload);
                assert_eq!(load_wallet_settings(&store).unwrap(), migrated);
                assert_eq!(
                    store
                        .get_app_settings_record(WALLET_SETTINGS_KEY)
                        .unwrap()
                        .unwrap(),
                    persisted
                );
            }
        }
    }
    // Current records honor each list independently, including an explicit opt-out.
    for direct in choices {
        for backup in [None, Some(Vec::new()), Some(custom.clone())] {
            let mut saved = WalletSettings::default();
            saved.waku.direct_peers = direct.clone();
            saved.waku.backup_peers = backup;
            save_wallet_settings(&store, &saved).unwrap();
            assert_eq!(load_wallet_settings(&store).unwrap(), saved);
        }
    }
    drop(store);
    fs::remove_dir_all(root_dir).unwrap();
}

#[test]
fn released_v6_overrides_migrate_without_losing_repairable_or_unrelated_settings() {
    let root_dir = temp_db_root();
    let mut released = crate::settings::legacy::LegacyWalletSettings::default();
    released.network.mode = crate::settings::NetworkModeSetting::Direct;
    released.runtime.auto_lock_timeout_secs = Some(600);
    released.poi.artifact.gateway_urls = vec![
        "https://dweb.link".into(),
        "https://ipfs.filebase.io".into(),
        "https://ipfs.io".into(),
    ];
    let ethereum = released.chains.per_chain.get_mut(&1).unwrap();
    ethereum.rpc_endpoints = vec!["https://user:secret@rpc.example/path".into()];
    ethereum.sponsored_bundle_relays = Some(vec![]);
    ethereum.contracts.multicall_contract = Some(Address::repeat_byte(2).to_string());
    ethereum.deployment.archive_until_block = Some(44);
    ethereum.deployment.archive_rpc_url = Some("https://archive.example".into());
    ethereum.quick_sync.enabled = false;
    ethereum.block_range = Some(0); // Released semantic error must remain repairable.
    ethereum.gas.gas_limit_buffer = Some(123_456);
    released.chains.per_chain.get_mut(&56).unwrap().enabled = false;
    // Released built-in overrides had no endpoint count or URL-length cap.
    let mut arbitrum_endpoints = (0..17)
        .map(|index| format!("https://rpc{index}.example/"))
        .collect::<Vec<_>>();
    arbitrum_endpoints[0].push_str(&"a".repeat(4097));
    released
        .chains
        .per_chain
        .get_mut(&42161)
        .unwrap()
        .rpc_endpoints = arbitrum_endpoints.clone();
    // Protocol overrides are discarded; everything else survives.
    let kept_chains = released.chains.clone();
    let ethereum = released.chains.per_chain.get_mut(&1).unwrap();
    ethereum.contracts.railgun_contract = Some(Address::repeat_byte(3).to_string());
    ethereum.contracts.coinbase_payer = Some(Address::repeat_byte(4).to_string());
    ethereum.deployment.deployment_block = Some(11);
    ethereum.deployment.v2_start_block = Some(22);
    ethereum.deployment.legacy_shield_block = Some(33);
    let payload = rmp_serde::to_vec_named(&released).unwrap();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .unwrap();
    let mut migrated = load_wallet_settings(&store).unwrap();
    assert!(migrated.validate().is_err());
    assert_eq!(released_chains(&migrated.chains), kept_chains);
    assert_eq!(migrated.network, released.network);
    assert_eq!(migrated.runtime, released.runtime);
    assert_eq!(migrated.poi, released.poi);
    drop(store);
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    assert_eq!(load_wallet_settings(&store).unwrap(), migrated);
    migrated
        .chains
        .per_chain
        .get_mut(&1)
        .unwrap()
        .railgun
        .block_range = Some(2_000);
    save_wallet_settings(&store, &migrated).unwrap();
    let configs = build_effective_chain_configs(&load_wallet_settings(&store).unwrap()).unwrap();
    let private = configs.get(1).unwrap().require_railgun().unwrap();
    assert_eq!(
        private.deployment,
        broadcaster_core::deployment::RailgunDeployment::for_chain(1).unwrap()
    );
    assert_eq!(
        private.coinbase_payer,
        crate::settings::default_coinbase_payer(1)
    );
    assert_eq!(private.sync.block_range, 2_000);
    assert!(private.sync.quick_sync_endpoint.is_none());
    assert_eq!(
        private.sponsored_bundle_relays,
        [] as [crate::settings::SensitiveUrl; 0]
    );
    assert_eq!(
        configs.get(1).unwrap().rpc_route.multicall(),
        Some(Address::repeat_byte(2))
    );
    assert_eq!(configs.get(1).unwrap().gas.gas_limit_buffer, 123_456);
    assert_eq!(
        configs.get(42161).unwrap().rpc_route.endpoint_urls(),
        arbitrum_endpoints
            .iter()
            .map(|endpoint| Url::parse(endpoint).unwrap())
            .collect::<Vec<_>>()
    );
    drop(store);
    fs::remove_dir_all(root_dir).unwrap();
}

fn released_v11_payload(settings: &WalletSettings) -> Vec<u8> {
    rmp_serde::to_vec_named(&ReleasedV11WalletSettingsWire {
        version: settings.version,
        network: &settings.network,
        privacy: &settings.privacy,
        chains: &settings.chains,
        indexed_artifacts: &settings.indexed_artifacts,
        poi: &settings.poi,
        broadcaster: &settings.broadcaster,
        tokens: &settings.tokens,
        gas: &settings.gas,
        runtime: &settings.runtime,
        waku: &settings.waku,
        walletconnect: &settings.walletconnect,
    })
    .expect("encode released v11 settings")
}

#[test]
fn released_beta13_settings_load_unchanged_with_tron_defaults() {
    let mut expected = WalletSettings::default();
    expected.network.mode = crate::settings::NetworkModeSetting::Direct;
    expected.privacy.unwrap_unshields_by_default = true;
    let ethereum = expected.chains.per_chain.get_mut(&1).unwrap();
    ethereum.native_usd_pricing = crate::settings::NativeUsdPricing::Disabled;
    ethereum.rpc_endpoints = vec![crate::settings::RpcEndpointSetting {
        url: "https://rpc.example".into(),
        direct: true,
        general: true,
        sensitive: true,
        submit: false,
    }];
    expected.chains.custom.insert(31337, custom_evm_chain());
    expected.runtime.auto_lock_timeout_secs = Some(600);
    expected.walletconnect.project_id_override = Some("custom-project".into());
    let payload = released_v11_payload(&expected);

    let loaded = decode_wallet_settings(&payload).expect("decode beta.13 settings");
    assert_eq!(loaded, expected);
    assert_eq!(loaded.tron, crate::settings::TronSettings::default());
}

/// beta.13 accepted Tron's id as a custom EVM chain; adding it is refused only in the editors.
#[test]
fn released_custom_chain_with_trons_id_loads_and_other_settings_save() {
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open db");
    let mut saved = WalletSettings::default();
    saved
        .chains
        .custom
        .insert(crate::tron::TRON_CHAIN_ID, custom_evm_chain());
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &released_v11_payload(&saved))
        .expect("store beta.13 settings");

    let mut loaded = load_wallet_settings(&store).expect("load beta.13 settings");
    assert_eq!(loaded, saved);
    loaded.runtime.auto_lock_timeout_secs = Some(600);
    save_wallet_settings(&store, &loaded).expect("save other settings");
    assert_eq!(
        load_wallet_settings(&store).expect("reload settings"),
        loaded
    );

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

/// `settings` in the released v7–v10 shape, whose built-in overrides list endpoint URLs.
fn released_v7_to_v10_settings(
    settings: &WalletSettings,
) -> crate::settings::legacy::ReleasedWalletSettings {
    let mut without_endpoints = settings.clone();
    for chain in without_endpoints.chains.per_chain.values_mut() {
        chain.rpc_endpoints.clear();
    }
    let mut released: crate::settings::legacy::ReleasedWalletSettings =
        rmp_serde::from_slice(&released_v11_payload(&without_endpoints)).unwrap();
    for (chain_id, chain) in &settings.chains.per_chain {
        released
            .chains
            .per_chain
            .get_mut(chain_id)
            .unwrap()
            .rpc_endpoints = chain
            .rpc_endpoints
            .iter()
            .map(|endpoint| endpoint.url.clone())
            .collect();
    }
    released
}

#[test]
fn released_v7_pricing_defaults_preserve_populated_settings_on_reopen() {
    let root_dir = temp_db_root();
    let mut expected = WalletSettings::default();
    expected.chains.custom.insert(31337, custom_evm_chain());
    expected.chains.per_chain.get_mut(&1).unwrap().rpc_endpoints =
        vec!["https://rpc.example".to_owned().into()];
    expected.chains.per_chain.get_mut(&56).unwrap().enabled = false;
    expected.runtime.auto_lock_timeout_secs = Some(600);
    expected
        .tokens
        .custom_tokens
        .push(crate::settings::CustomTokenSettings {
            chain_id: 31337,
            token_address: Address::repeat_byte(7).to_string(),
            symbol: "TOKEN".into(),
            decimals: 6,
            icon_path: None,
            price_anchor: Some(crate::settings::PriceAnchorSettings::Fixed {
                rate: "123456".into(),
            }),
        });
    expected.waku.direct_peers = Some(Vec::new());
    expected.waku.backup_peers = Some(Vec::new());
    // Default choices are omitted, producing the released v7 field shape.
    let mut released = expected.clone();
    released.version = 7;
    released.waku.backup_peers = None;
    let payload = rmp_serde::to_vec_named(&released_v7_to_v10_settings(&released)).unwrap();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .unwrap();
    assert!(
        crate::settings::storage::load_wallet_settings_with_writer(&store, |_| {
            Err(WalletSettingsError::WriteUnavailable)
        })
        .is_err()
    );
    assert_eq!(
        store
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .unwrap()
            .unwrap(),
        payload
    );
    drop(store);
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    let migrated = load_wallet_settings(&store).unwrap();
    assert_eq!(migrated, expected);
    assert_ne!(
        store
            .get_app_settings_record(WALLET_SETTINGS_KEY)
            .unwrap()
            .unwrap(),
        payload
    );
    assert!(
        migrated
            .chains
            .per_chain
            .values()
            .all(|chain| chain.native_usd_pricing == crate::settings::NativeUsdPricing::Default)
    );
    assert!(
        migrated
            .chains
            .custom
            .values()
            .all(|chain| chain.native_usd_pricing == crate::settings::NativeUsdPricing::Default)
    );
    drop(store);
    let reopened = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    assert_eq!(load_wallet_settings(&reopened).unwrap(), expected);
    drop(reopened);
    fs::remove_dir_all(root_dir).unwrap();
}

#[test]
fn released_v10_protocol_overrides_are_dropped_and_other_settings_kept() {
    let mut expected = WalletSettings::default();
    let ethereum = expected.chains.per_chain.get_mut(&1).unwrap();
    ethereum.rpc_endpoints = vec!["https://rpc.example".to_owned().into()];
    ethereum.railgun.sponsored_bundle_relays = Some(vec!["https://relay.example".into()]);
    ethereum.railgun.quick_sync.enabled = false;
    ethereum.railgun.quick_sync.endpoint = Some("https://quick.example".into());
    ethereum.railgun.deployment.archive_until_block = Some(44);
    ethereum.railgun.deployment.archive_rpc_url = Some("https://archive.example".into());
    let mut released = expected.clone();
    released.version = 10;
    let mut released = released_v7_to_v10_settings(&released);
    // Released builds wrote the removed keys as nil.
    let without_overrides = rmp_serde::to_vec_named(&released).unwrap();
    let railgun = &mut released.chains.per_chain.get_mut(&1).unwrap().railgun;
    railgun.contracts = crate::settings::legacy::ReleasedRailgunContractSettings {
        railgun_contract: Some(Address::repeat_byte(1).to_string()),
        relay_adapt_contract: Some(Address::repeat_byte(2).to_string()),
        relay_adapt_7702_contract: Some(Address::repeat_byte(3).to_string()),
        coinbase_payer: Some(Address::repeat_byte(4).to_string()),
    };
    railgun.deployment.deployment_block = Some(11);
    railgun.deployment.v2_start_block = Some(22);
    railgun.deployment.legacy_shield_block = Some(33);
    let with_overrides = rmp_serde::to_vec_named(&released).unwrap();

    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    for payload in [without_overrides, with_overrides] {
        store
            .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
            .unwrap();
        let mut written = None;
        let migrated =
            crate::settings::storage::load_wallet_settings_with_writer(&store, |payload| {
                written = Some(payload.to_vec());
                Ok(())
            })
            .unwrap();
        assert_eq!(migrated, expected);
        let written = written.expect("the migrated record is written back");
        assert_eq!(
            rmp_serde::from_slice::<WalletSettings>(&written).unwrap(),
            expected
        );
        let configs = build_effective_chain_configs(&migrated).unwrap();
        let private = configs.get(1).unwrap().require_railgun().unwrap();
        assert_eq!(
            private.deployment,
            broadcaster_core::deployment::RailgunDeployment::for_chain(1).unwrap()
        );
        assert_eq!(
            private.coinbase_payer,
            crate::settings::default_coinbase_payer(1)
        );
    }

    // The current version stays strict about the removed keys.
    released.version = WALLET_SETTINGS_VERSION;
    assert!(decode_wallet_settings(&rmp_serde::to_vec_named(&released).unwrap()).is_err());

    drop(store);
    fs::remove_dir_all(root_dir).unwrap();
}
