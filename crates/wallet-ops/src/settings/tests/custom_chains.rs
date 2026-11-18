use super::*;

#[test]
fn custom_chains_fold_into_built_in_presets_on_load() {
    let root_dir = temp_db_root();
    let mut saved = WalletSettings::default();
    let mut adopted = custom_evm_chain();
    adopted.rpc_endpoints = vec!["https://base.example".to_owned().into()];
    adopted.enabled = false;
    adopted.finality_depth = Some(32);
    saved.chains.custom.insert(8453, adopted);
    // An explicitly enabled custom Sepolia entry is an opt-in and survives preset adoption.
    let mut sepolia = custom_evm_chain();
    sepolia.rpc_endpoints = vec!["https://sepolia.example".to_owned().into()];
    saved.chains.custom.insert(11_155_111, sepolia);

    let payload = encode_wallet_settings(&saved).unwrap();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    store
        .put_app_settings_record(WALLET_SETTINGS_KEY, &payload)
        .unwrap();

    let mut written = None;
    let migrated = crate::settings::storage::load_wallet_settings_with_writer(&store, |payload| {
        written = Some(payload.to_vec());
        Ok(())
    })
    .unwrap();

    migrated
        .validate()
        .expect("a chain id covered by a preset no longer duplicates a custom chain");
    assert!(!migrated.chains.custom.contains_key(&8453));
    assert!(!migrated.chains.custom.contains_key(&11_155_111));
    let sepolia = &migrated.chains.per_chain[&11_155_111];
    assert!(sepolia.enabled);
    assert_eq!(
        sepolia.rpc_endpoints,
        vec![crate::settings::RpcEndpointSetting::from(
            "https://sepolia.example".to_owned()
        )]
    );

    let adopted = migrated.chains.per_chain.get(&8453).expect("base override");
    assert_eq!(
        adopted.rpc_endpoints,
        vec![crate::settings::RpcEndpointSetting::from(
            "https://base.example".to_owned()
        )]
    );
    assert!(!adopted.enabled);
    assert_eq!(adopted.finality_depth, Some(32));
    assert_eq!(
        decode_wallet_settings(&written.expect("the fold is written back")).unwrap(),
        migrated
    );
    // Base's saved public fields apply on top of its default Railgun settings, and it stays off.
    let configs = build_effective_chain_configs(&migrated).expect("build effective configs");
    let base = configs.get(8453).expect("base config");
    assert!(!base.enabled);
    assert!(!migrated.chains.enabled_chain_ids().contains(&8453));
    assert_eq!(base.finality_depth, 32);
    assert_eq!(
        base.railgun.as_ref().expect("base railgun").deployment,
        broadcaster_core::deployment::RailgunDeployment::for_chain(8453).unwrap()
    );

    drop(store);
    fs::remove_dir_all(root_dir).unwrap();
}

#[test]
fn custom_registry_retains_token_and_oracle_references_across_disable() {
    const CHAIN: u64 = 9_007_199_254_740_993;
    let mut settings = WalletSettings::default();
    settings.chains.custom.insert(CHAIN, custom_evm_chain());
    for chain in settings.chains.per_chain.values_mut() {
        chain.enabled = false;
    }
    settings
        .tokens
        .custom_tokens
        .push(crate::settings::CustomTokenSettings {
            chain_id: CHAIN,
            token_address: Address::repeat_byte(7).to_string(),
            symbol: "TOKEN".into(),
            decimals: 6,
            icon_path: None,
            price_anchor: Some(crate::settings::PriceAnchorSettings::Oracle {
                chain_id: CHAIN,
                oracle_address: Address::repeat_byte(8).to_string(),
                token_decimals: 6,
                oracle_decimals: 8,
                is_inversed: false,
            }),
        });
    let configs = build_effective_chain_configs(&settings).unwrap();
    let custom = configs.get(CHAIN).unwrap();
    assert_eq!(configs.enabled(CHAIN).unwrap(), custom);
    assert!(
        configs
            .railgun(CHAIN)
            .unwrap_err()
            .to_string()
            .contains("no Railgun deployment")
    );
    // Metadata survives disable, while both public and private admission reject it.
    assert!(configs.get(1).is_some());
    assert!(configs.get(31337).is_none());
    for (id, reason) in [(1, "disabled"), (31337, "not configured")] {
        assert!(
            configs
                .enabled(id)
                .unwrap_err()
                .to_string()
                .contains(reason)
        );
        assert!(
            configs
                .railgun(id)
                .unwrap_err()
                .to_string()
                .contains(reason)
        );
    }
    assert_eq!(
        configs
            .enabled_chains()
            .map(|chain| chain.chain_id)
            .collect::<Vec<_>>(),
        vec![CHAIN]
    );
    assert_eq!(configs.railgun_chains().count(), 0);
    assert!(custom.railgun.is_none());
    assert!(custom.block_time.is_none());
    assert!(custom.wrapped_native_token.is_none());
    assert!(custom.rpc_route.multicall().is_none());
    assert!(custom.require_railgun().is_err());
    assert_eq!(custom.native_currency.symbol, "TST");
    assert_eq!(custom.native_currency.decimals, 6);
    assert_eq!(settings.chains.enabled_chain_ids(), vec![CHAIN]);
    assert_eq!(
        resolve_effective_chain_rpc_route(CHAIN, custom)
            .unwrap()
            .chain_id(),
        CHAIN
    );
    let token_registry = build_effective_token_registry(&settings).unwrap();
    settings.chains.custom.get_mut(&CHAIN).unwrap().enabled = false;
    assert!(settings.validate().is_err()); // Cannot disable the last available chain.
    settings.chains.per_chain.get_mut(&1).unwrap().enabled = true;
    settings.validate().unwrap();
    assert_eq!(
        build_effective_token_registry(&settings).unwrap(),
        token_registry
    );
    let configs = build_effective_chain_configs(&settings).unwrap();
    assert!(resolve_effective_chain_rpc_route(CHAIN, configs.get(CHAIN).unwrap()).is_err());
    assert!(configs.enabled(CHAIN).is_err());
    assert_eq!(configs.railgun(1).unwrap(), configs.get(1).unwrap());
    assert_eq!(
        configs
            .railgun_chains()
            .map(|chain| chain.chain_id)
            .collect::<Vec<_>>(),
        vec![1]
    );
    settings.chains.custom.get_mut(&CHAIN).unwrap().enabled = true;
    assert_eq!(
        build_effective_token_registry(&settings).unwrap(),
        token_registry
    );
    let saved = settings.chains.custom.remove(&CHAIN).unwrap();
    let error = settings.validate().unwrap_err();
    assert!(
        error
            .messages
            .iter()
            .any(|message| message.contains("tokens.custom_tokens[0].chain_id"))
    );
    settings.chains.custom.insert(CHAIN, saved);
    settings.validate().unwrap();
    settings.chains.custom.insert(1, custom_evm_chain());
    assert!(settings.validate().is_err());
    settings.chains.custom.remove(&1);
    let mut oversized = custom_evm_chain();
    oversized.rpc_endpoints = (0..=crate::settings::MAX_CHAIN_ENDPOINTS)
        .map(|index| format!("https://rpc{index}.example").into())
        .collect();
    assert!(
        crate::settings::ChainMutation::Add {
            chain_id: CHAIN + 1,
            definition: oversized,
        }
        .prepare(&settings)
        .is_err()
    );
    settings
        .chains
        .custom
        .get_mut(&CHAIN)
        .unwrap()
        .rpc_endpoints
        .clear();
    assert!(settings.validate().is_err());
}

#[test]
fn targeted_chain_and_whole_settings_commits_cannot_overwrite_each_other() {
    use crate::settings::{ChainMutation, commit_wallet_settings, settings_revision};
    let root_dir = temp_db_root();
    let store = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    let saved = load_wallet_settings(&store).unwrap();
    let revision = settings_revision(&saved).unwrap();
    let addition = ChainMutation::Add {
        chain_id: 9_007_199_254_740_993,
        definition: custom_evm_chain(),
    }
    .prepare(&saved)
    .unwrap();
    let mut whole = saved.clone();
    whole.runtime.auto_lock_timeout_secs = Some(600);
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let targeted = scope.spawn(|| {
            barrier.wait();
            commit_wallet_settings(&store, revision, &addition)
        });
        let full = scope.spawn(|| {
            barrier.wait();
            commit_wallet_settings(&store, revision, &whole)
        });
        (targeted.join().unwrap(), full.join().unwrap())
    });
    let expected = match results {
        (Ok(()), Err(WalletSettingsError::Conflict)) => addition,
        (Err(WalletSettingsError::Conflict), Ok(())) => whole,
        results => panic!("one stale writer must fail: {results:?}"),
    };
    assert_eq!(load_wallet_settings(&store).unwrap(), expected);
    assert!(matches!(
        commit_wallet_settings(&store, revision, &saved),
        Err(WalletSettingsError::Conflict)
    ));
    let mut invalid = expected.clone();
    for chain in invalid.chains.per_chain.values_mut() {
        chain.enabled = false;
    }
    for chain in invalid.chains.custom.values_mut() {
        chain.enabled = false;
    }
    assert!(matches!(
        commit_wallet_settings(&store, settings_revision(&expected).unwrap(), &invalid),
        Err(WalletSettingsError::Validation(_))
    ));
    drop(store);
    let reopened = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .unwrap();
    assert_eq!(load_wallet_settings(&reopened).unwrap(), expected);
    drop(reopened);
    fs::remove_dir_all(root_dir).unwrap();
}
