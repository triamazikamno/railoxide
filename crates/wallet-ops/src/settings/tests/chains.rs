use super::*;

#[test]
fn effective_chain_configs_use_supported_presets_without_overrides() {
    let settings = WalletSettings::default();
    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(1).expect("ethereum config");
    let defaults = crate::settings::presets::EvmPreset::for_chain(1).unwrap();
    let sync = sync_service::RailgunSyncOptions::for_chain(
        1,
        crate::desktop::DEFAULT_BLOCK_RANGE,
        crate::desktop::DEFAULT_POLL_INTERVAL,
    )
    .unwrap();

    for (map_chain_id, config) in configs.iter() {
        assert_eq!(*map_chain_id, config.chain_id);
        assert_eq!(config.chain_id, config.rpc_route.chain_id());
    }

    assert!(ethereum.enabled);
    assert_eq!(
        ethereum.rpc_route.endpoint_urls(),
        defaults
            .rpc_endpoints
            .iter()
            .map(|url| Url::parse(url).unwrap())
            .collect::<Vec<_>>(),
    );
    assert!(ethereum.rpc_route.configured_endpoints().len() > 1);
    assert_eq!(ethereum.finality_depth, defaults.finality_depth);
    assert!(ethereum.has_sponsorship_prerequisites());
    for chain_id in [56, 137, 42161] {
        let chain = configs.get(chain_id).expect("supported chain config");
        assert_eq!(
            chain.require_railgun().unwrap().sponsored_bundle_relays,
            [] as [crate::settings::SensitiveUrl; 0]
        );
        assert_eq!(chain.require_railgun().unwrap().coinbase_payer, None);
        assert!(!chain.has_sponsorship_prerequisites());
    }
    // Protocol facts are built in on every Railgun chain.
    for &chain_id in railgun_ui::DEFAULT_CHAINS {
        let private = configs.get(chain_id).unwrap().railgun.as_ref().unwrap();
        assert_eq!(
            private.deployment,
            broadcaster_core::deployment::RailgunDeployment::for_chain(chain_id).unwrap()
        );
        assert_eq!(
            private.coinbase_payer,
            crate::settings::default_coinbase_payer(chain_id)
        );
    }
    // Base syncs from indexed artifacts and RPC, without a legacy RelayAdapt or Squid.
    let base = configs.get(8453).expect("base config");
    assert!(base.enabled);
    assert_eq!(base.block_time, Some(std::time::Duration::from_secs(2)));
    assert_eq!(base.finality_depth, 10);
    let base_private = base.require_railgun().expect("base railgun");
    assert_eq!(base_private.deployment.relay_adapt_contract, None);
    assert_eq!(base_private.sync.quick_sync_endpoint, None);
    assert!(base_private.sync.indexed_artifact_source.is_some());
    assert_eq!(
        ethereum.require_railgun().unwrap().sync.archive_until_block,
        sync.archive_until_block
    );
    assert_eq!(ethereum.require_railgun().unwrap().archive_rpc_url, None);
    assert_eq!(
        ethereum.require_railgun().unwrap().sync.quick_sync_endpoint,
        sync.quick_sync_endpoint
    );
    assert_eq!(ethereum.rpc_route.multicall(), defaults.multicall);
    assert_eq!(
        ethereum
            .require_railgun()
            .unwrap()
            .indexed_artifact_source_mode,
        IndexedArtifactSourceModeSetting::Official
    );
    let source = ethereum
        .require_railgun()
        .unwrap()
        .sync
        .indexed_artifact_source
        .as_ref()
        .expect("official indexed artifact source");
    assert_eq!(
        alloy::hex::encode(source.trusted_publisher_pubkey.as_slice()),
        OFFICIAL_INDEXED_ARTIFACT_PUBLISHER_PUBKEY.trim_start_matches("0x")
    );
    assert!(matches!(
        &source.manifest_source,
        sync_service::IndexedArtifactManifestSource::IpnsName(name)
            if name == OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME
    ));
    assert_eq!(
        source
            .gateway_urls
            .iter()
            .map(|url| url.as_str().to_string())
            .collect::<Vec<_>>(),
        OFFICIAL_POI_ARTIFACT_GATEWAYS
            .iter()
            .map(|gateway| format!("{gateway}/"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn sepolia_requires_opt_in_and_reset_disables_it_again() {
    use railgun_ui::chain_editor::ChainEditorCommand;
    const SEPOLIA: u64 = 11_155_111;
    let mut settings = WalletSettings::default();
    let assert_disabled = |settings: &WalletSettings| {
        assert!(!settings.chains.enabled_chain_ids().contains(&SEPOLIA));
        let configs = build_effective_chain_configs(settings).unwrap();
        assert!(!configs.get(SEPOLIA).unwrap().enabled);
        assert!(configs.enabled(SEPOLIA).is_err());
        assert!(configs.railgun(SEPOLIA).is_err());
        for id in railgun_ui::built_in_chain_ids().filter(|id| !railgun_ui::is_testnet_chain(*id)) {
            assert!(configs.enabled(id).is_ok(), "mainnet {id} stays enabled");
        }
        let snapshot = crate::settings::chain_editor_snapshot(settings, Some(SEPOLIA)).unwrap();
        let summary = snapshot
            .chains
            .iter()
            .find(|chain| chain.chain_id == SEPOLIA.to_string())
            .unwrap();
        assert!(!summary.enabled);
        assert!(!summary.modified);
        assert!(!snapshot.draft.unwrap().enabled);
    };
    assert_disabled(&settings);
    // Released records with omitted chains must also leave testnets disabled.
    for version in 1..=10 {
        let payload = rmp_serde::to_vec_named(&json!({ "version": version })).unwrap();
        assert_disabled(&decode_wallet_settings(&payload).unwrap());
    }
    // Saved settings from before the preset was added have no entry for Sepolia.
    settings.chains.per_chain.remove(&SEPOLIA);
    settings = decode_wallet_settings(&encode_wallet_settings(&settings).unwrap()).unwrap();
    assert_disabled(&settings);

    let mut draft = crate::settings::chain_editor_draft(&settings, SEPOLIA).unwrap();
    draft.enabled = true;
    settings = crate::settings::chain_editor_mutation(
        &settings,
        &ChainEditorCommand::Save {
            draft,
            existing: true,
        },
    )
    .unwrap()
    .prepare(&settings)
    .unwrap();
    // A stored oracle override must never give test ETH a USD valuation.
    settings
        .chains
        .per_chain
        .get_mut(&SEPOLIA)
        .unwrap()
        .native_usd_pricing = crate::settings::NativeUsdPricing::Oracle {
        contract_address: Address::repeat_byte(1),
    };
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    save_wallet_settings(&store, &settings).unwrap();
    settings = load_wallet_settings(&store).unwrap();
    assert!(settings.chains.enabled_chain_ids().contains(&SEPOLIA));
    let configs = build_effective_chain_configs(&settings).unwrap();
    let sepolia = configs.railgun(SEPOLIA).unwrap();
    let private = sepolia.require_railgun().unwrap();
    assert_eq!(private.sync.archive_until_block, 0);
    assert_eq!(
        private.deployment.relay_adapt_7702_contract,
        alloy::primitives::address!("0x05aE73C5925d843864AE6f261f3175dE2ebCd963")
    );
    assert_eq!(
        private.deployment.relay_adapt_7702_delegates[0].capability,
        broadcaster_core::contracts::executor::ExecutorCapability::NonceBearingV1
    );
    assert!(sepolia.native_usd_oracle.is_none());
    let weth = sepolia.wrapped_native_token.unwrap();
    assert!(crate::is_wrapped_native_token(SEPOLIA, weth));
    let token = railgun_ui::lookup_token(SEPOLIA, &weth).unwrap();
    assert_eq!(
        token.anchor_sources,
        &[railgun_ui::TokenAnchorSource::Fixed {
            token_fee_per_unit_gas: railgun_ui::WRAPPED_NATIVE_FEE_RATE,
        }]
    );
    settings = crate::settings::chain_editor_mutation(
        &settings,
        &ChainEditorCommand::Reset {
            chain_id: SEPOLIA.to_string(),
        },
    )
    .unwrap()
    .prepare(&settings)
    .unwrap();
    save_wallet_settings(&store, &settings).unwrap();
    assert_disabled(&load_wallet_settings(&store).unwrap());
    settings = crate::settings::ChainMutation::SetEnabled {
        chain_id: SEPOLIA,
        enabled: true,
    }
    .prepare(&settings)
    .unwrap();
    settings.reset_chains();
    assert_disabled(&settings);
    drop(store);
    fs::remove_dir_all(root_dir).unwrap();
}

#[test]
fn native_eth_presets_show_the_ethereum_mark() {
    for chain_id in railgun_ui::built_in_chain_ids() {
        let preset = crate::settings::presets::EvmPreset::for_chain(chain_id).unwrap();
        let shows_ethereum_mark = railgun_ui::native_currency_icon_asset_path(chain_id)
            == railgun_ui::chain_icon_asset_path(1);
        assert_eq!(
            shows_ethereum_mark,
            preset.native_currency.symbol == "ETH",
            "chain {chain_id}"
        );
    }
}

#[test]
fn executor_profiles_accept_every_listed_delegate_with_the_newest_as_primary() {
    use alloy::primitives::address;

    let configs = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    for (chain_ids, delegates) in [
        (
            &[1][..],
            [
                address!("05aE73C5925d843864AE6f261f3175dE2ebCd963"),
                address!("1F74b4339990Db761bC0168F20D294A3a141497e"),
            ],
        ),
        (
            &[56, 137, 42161][..],
            [
                address!("48cf4b897f64D81212c1423D78a05E828d0cE19d"),
                address!("a2BFd382c54bb33DF6A13f21acd8770dc1e67D4D"),
            ],
        ),
    ] {
        for &chain_id in chain_ids {
            for delegate in delegates {
                let profile =
                    crate::settings::ExecutorProfile::accepted(chain_id, delegate).unwrap();
                assert_eq!(profile.delegate(), delegate);
                assert_eq!(
                    profile.capability(),
                    crate::settings::ExecutorCapability::NonceBearingV1
                );
            }
            assert!(
                crate::settings::ExecutorProfile::accepted(chain_id, Address::repeat_byte(0x11))
                    .is_none()
            );
            let primary = configs
                .get(chain_id)
                .unwrap()
                .accepted_executor_profile()
                .unwrap();
            assert_eq!(primary.delegate(), delegates[1]);
        }
    }
}

#[test]
fn public_presets_resolve_without_railgun_and_reject_railgun_overrides() {
    let mut settings = WalletSettings::default();
    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let optimism = configs.get(10).expect("optimism config");
    assert!(optimism.built_in);
    assert!(optimism.railgun.is_none());
    assert!(optimism.enabled, "built-in presets ship enabled");
    assert!(optimism.rpc_route.multicall().is_some());
    assert_eq!(
        optimism.native_usd_oracle,
        crate::settings::presets::EvmPreset::for_chain(10)
            .expect("optimism preset")
            .native_usd_oracle,
        "default pricing resolves to the preset oracle"
    );
    assert!(settings.chains.enabled_chain_ids().contains(&10));

    settings.chains.per_chain.insert(
        10,
        crate::settings::ChainSettingsOverride {
            enabled: false,
            ..crate::settings::ChainSettingsOverride::default()
        },
    );
    settings
        .validate()
        .expect("a disabled public preset is a valid override");
    assert!(!settings.chains.enabled_chain_ids().contains(&10));

    settings
        .chains
        .per_chain
        .get_mut(&10)
        .expect("optimism override")
        .railgun
        .block_range = Some(1_000);
    let error = settings
        .validate()
        .expect_err("Railgun settings on a public preset");
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("chains.per_chain.10.railgun")
                && message.contains("no Railgun deployment"))
    );
}

#[test]
fn effective_chain_rpc_route_validates_chain_identities() {
    let settings = WalletSettings::default();
    let configs = build_effective_chain_configs(&settings).expect("build effective configs");
    let ethereum = configs.get(1).expect("ethereum config");

    assert_eq!(
        crate::settings::default_chain_rpc_route(1).expect("default route"),
        ethereum.rpc_route
    );
    assert_eq!(
        resolve_effective_chain_rpc_route(1, ethereum).expect("effective route"),
        ethereum.rpc_route
    );
    assert!(resolve_effective_chain_rpc_route(9_999, ethereum).is_err());

    let mut outer_mismatch = ethereum.clone();
    outer_mismatch.chain_id = 56;
    assert!(resolve_effective_chain_rpc_route(1, &outer_mismatch).is_err());

    let mut embedded_mismatch = ethereum.clone();
    embedded_mismatch.rpc_route = RpcChainRoute::new(56, ethereum.rpc_route.endpoint_urls());
    assert!(resolve_effective_chain_rpc_route(1, &embedded_mismatch).is_err());

    let mut no_endpoints = ethereum.clone();
    no_endpoints.rpc_route = RpcChainRoute::new(1, Vec::<Url>::new());
    let error = resolve_effective_chain_rpc_route(1, &no_endpoints)
        .expect_err("empty endpoint list is rejected");
    assert!(error.to_string().contains("has no RPC endpoints"));
}

#[test]
fn sponsored_relay_overrides_preserve_order_and_explicit_empty_disables() {
    let mut settings = WalletSettings::default();
    let inherited = build_effective_chain_configs(&settings).expect("inherited sponsored relays");
    assert_ne!(
        inherited
            .get(1)
            .expect("ethereum config")
            .require_railgun()
            .unwrap()
            .sponsored_bundle_relays,
        [] as [crate::settings::SensitiveUrl; 0]
    );
    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .railgun
        .sponsored_bundle_relays = Some(vec![
        "https://user:secret@relay-a.example/path".to_string(),
        "http://relay-b.example".to_string(),
    ]);
    let configs = build_effective_chain_configs(&settings).expect("effective sponsored relays");
    let ethereum = configs.get(1).expect("ethereum config");
    assert_eq!(
        ethereum
            .require_railgun()
            .unwrap()
            .sponsored_bundle_relays
            .len(),
        2
    );
    assert_eq!(
        ethereum.require_railgun().unwrap().sponsored_bundle_relays[1]
            .expose_url()
            .as_str(),
        "http://relay-b.example/"
    );
    let debug = format!(
        "{:?}",
        ethereum.require_railgun().unwrap().sponsored_bundle_relays
    );
    assert!(!debug.contains("user"));
    assert!(!debug.contains("secret"));

    settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings")
        .railgun
        .sponsored_bundle_relays = Some(Vec::new());
    let disabled = build_effective_chain_configs(&settings).expect("disabled sponsored relays");
    assert_eq!(
        disabled
            .get(1)
            .expect("ethereum config")
            .require_railgun()
            .unwrap()
            .sponsored_bundle_relays,
        [] as [crate::settings::SensitiveUrl; 0]
    );
}

#[test]
fn sponsored_settings_validate_relay_schemes() {
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    ethereum.railgun.sponsored_bundle_relays = Some(vec!["ws://relay.example".to_string()]);

    let error = settings.validate().expect_err("invalid sponsored settings");
    assert!(error.messages.iter().any(|message| {
        message.contains("sponsored_bundle_relays") && message.contains("http, https")
    }));
}

#[test]
fn direct_endpoint_validation_names_the_field_without_the_url() {
    let invalid = "ftp://user:secret@node.example";
    let mut settings = WalletSettings::default();
    let ethereum = settings
        .chains
        .per_chain
        .get_mut(&1)
        .expect("ethereum settings");
    let direct = crate::settings::RpcEndpointSetting {
        url: invalid.to_string(),
        direct: true,
        ..crate::settings::RpcEndpointSetting::default()
    };
    // A Direct endpoint applies the endpoint cap to a built-in override.
    ethereum.rpc_endpoints = (0..crate::settings::MAX_CHAIN_ENDPOINTS)
        .map(|index| format!("https://rpc{index}.example").into())
        .collect();
    ethereum.rpc_endpoints.push(direct.clone());
    let mut custom = custom_evm_chain();
    custom.rpc_endpoints.push(direct);
    settings.chains.custom.insert(999_999, custom);

    let error = settings.validate().expect_err("invalid direct endpoints");
    let built_in = format!(
        "chains.per_chain.1.rpc_endpoints[{}] ",
        crate::settings::MAX_CHAIN_ENDPOINTS
    );
    for field in [
        built_in.as_str(),
        "chains.custom.999999.rpc_endpoints[1] ",
        "chains.per_chain.1 has too many RPC endpoints",
    ] {
        assert!(
            error
                .messages
                .iter()
                .any(|message| message.starts_with(field)),
            "{field}"
        );
    }
    assert!(
        error
            .messages
            .iter()
            .all(|message| !message.contains("secret"))
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
        ethereum.railgun.sponsored_bundle_relays = relays.clone();

        let encoded = encode_wallet_settings(&settings).expect("encode sponsored settings");
        let decoded = decode_wallet_settings(&encoded).expect("decode sponsored settings");
        let decoded = decoded
            .chains
            .per_chain
            .get(&1)
            .expect("decoded ethereum settings");
        assert_eq!(decoded.railgun.sponsored_bundle_relays, relays);
    }
}
