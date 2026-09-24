use super::helpers::*;

#[test]
fn desktop_wallet_start_policy_generated_defaults_to_historical_backfill() {
    let resolved = resolve_desktop_wallet_chain_start(
        DesktopWalletSyncStartPolicy::from(vault::WalletSource::Generated),
        None,
        None,
        100,
        Some(250),
        false,
    )
    .expect("resolve generated start");

    assert_eq!(
        resolved,
        DesktopWalletChainStart {
            start_block: 100,
            last_scanned_block: 99,
        }
    );
}

#[test]
fn desktop_wallet_start_policy_imported_uses_deployment_block() {
    let resolved = resolve_desktop_wallet_chain_start(
        DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
        None,
        None,
        100,
        Some(250),
        false,
    )
    .expect("resolve imported start");

    assert_eq!(
        resolved,
        DesktopWalletChainStart {
            start_block: 100,
            last_scanned_block: 99,
        }
    );
}

#[test]
fn desktop_wallet_start_policy_new_hardware_defaults_to_historical_backfill() {
    let metadata = hardware_wallet_metadata(HardwareWalletSyncIntent::CreateNew);
    assert_eq!(
        DesktopWalletSyncStartPolicy::from(&metadata),
        DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill
    );

    let resolved = resolve_desktop_wallet_chain_start(
        DesktopWalletSyncStartPolicy::from(&metadata),
        None,
        None,
        100,
        Some(250),
        false,
    )
    .expect("resolve new hardware start");

    assert_eq!(
        resolved,
        DesktopWalletChainStart {
            start_block: 100,
            last_scanned_block: 99,
        }
    );
}

#[test]
fn desktop_wallet_creation_override_uses_safe_head_no_backfill() {
    let resolved = resolve_desktop_wallet_chain_start(
        DesktopWalletSyncStartPolicy::CurrentSafeHeadNoBackfill,
        None,
        None,
        100,
        Some(250),
        false,
    )
    .expect("resolve generated creation start");

    assert_eq!(
        resolved,
        DesktopWalletChainStart {
            start_block: 251,
            last_scanned_block: 250,
        }
    );
}

#[test]
fn new_wallet_chain_start_helpers_use_expected_baselines() {
    assert_eq!(
        new_wallet_chain_start_from_deployment(100),
        DesktopWalletChainStart {
            start_block: 100,
            last_scanned_block: 99,
        }
    );
    assert_eq!(
        new_wallet_chain_start_from_head(100, 10, 250),
        DesktopWalletChainStart {
            start_block: 241,
            last_scanned_block: 240,
        }
    );
    assert_eq!(
        new_wallet_chain_start_from_head(100, 10, 50),
        DesktopWalletChainStart {
            start_block: 101,
            last_scanned_block: 100,
        }
    );
}

#[test]
fn new_wallet_chain_metadata_initializer_resumes_from_deployment_and_retains_disabled_chains() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = vault::DesktopVaultStore::from_db(Arc::clone(&db));
    store
        .create_vault_with_params(TEST_PASSWORD, vault::KdfParams::new(1024, 1, 1))
        .expect("create vault");
    let wallet_id = "generated-wallet";
    let metadata = store
        .new_wallet_metadata_with_pending_create_new_chain_ids(
            TEST_PASSWORD,
            wallet_id,
            0,
            vault::WalletSource::Generated,
            "Generated",
            BTreeSet::from([1, 56, 137, 999]),
        )
        .expect("wallet metadata");
    store
        .import_wallet_mnemonic_with_metadata(
            TEST_PASSWORD,
            wallet_id,
            0,
            "english",
            TEST_MNEMONIC,
            &metadata,
        )
        .expect("store wallet");
    let session = store
        .load_view_session(TEST_PASSWORD, wallet_id)
        .expect("load view session");
    let http = runtime
        .block_on(crate::build_wallet_network_context(
            crate::WalletNetworkConfig {
                network_mode: Some(crate::WalletNetworkMode::Direct),
                proxy: None,
                data_dir: &root_dir,
            },
        ))
        .expect("direct HTTP context");
    let contract = RailgunDeployment::for_chain(1)
        .expect("ethereum defaults")
        .contract
        .to_checksum(None);
    assert!(
        !store
            .complete_pending_create_new_chain_for_session(&session, 0, 1, &contract)
            .expect("check incomplete chain initialization")
    );
    assert!(
        store
            .load_wallet_metadata_for_session(&session)
            .expect("reload incomplete pending metadata")
            .pending_create_new_chain_ids
            .contains(&1)
    );
    let interrupted_chain = store
        .create_wallet_chain_metadata_for_session(&session, 0, 1, &contract, 251, 250)
        .expect("persist chain metadata before pending completion");
    let mut disabled_chain = effective_chain_config_with_rpc_endpoints(137, Vec::new(), 12_345);
    disabled_chain.enabled = false;
    let configs: crate::settings::EffectiveChainRegistry = [
        effective_chain_config_with_rpc_endpoints(1, Vec::new(), 12_345),
        effective_chain_config_with_rpc_endpoints(56, Vec::new(), 5_000),
        disabled_chain,
    ]
    .into_iter()
    .collect();

    drop(session);
    drop(store);
    let reopened_store = vault::DesktopVaultStore::from_db(Arc::clone(&db));
    let reopened_session = Arc::new(
        reopened_store
            .load_view_session(TEST_PASSWORD, wallet_id)
            .expect("reopen view session"),
    );

    let report = runtime.block_on(initialize_new_wallet_chain_metadata_for_session(
        Arc::clone(&reopened_session),
        configs.clone(),
        Arc::clone(&db),
        http.clone(),
        None,
        CreatedWalletChainInitPolicy::Resumed,
    ));

    assert_eq!(report.initialized, 1);
    assert_eq!(report.skipped_existing, 1);
    assert_eq!(report.skipped_disabled, 1);
    assert_eq!(report.skipped_unavailable, 1);
    assert_eq!(report.failed, 0);
    let pending = reopened_store
        .load_wallet_metadata_for_session(&reopened_session)
        .expect("load pending metadata");
    assert_eq!(
        pending.pending_create_new_chain_ids,
        BTreeSet::from([137, 999])
    );
    let chain_metadata = reopened_store
        .find_wallet_chain_metadata_for_session(reopened_session.as_ref(), 0, 1, &contract)
        .expect("load chain metadata")
        .expect("chain metadata exists");
    assert_eq!(
        chain_metadata.wallet_chain_uuid,
        interrupted_chain.wallet_chain_uuid
    );
    assert_eq!(chain_metadata.start_block, 251);
    assert_eq!(chain_metadata.last_scanned_block, 250);
    let resumed_chain_contract = RailgunDeployment::for_chain(56)
        .expect("bsc defaults")
        .contract
        .to_checksum(None);
    let resumed_chain_metadata = reopened_store
        .find_wallet_chain_metadata_for_session(
            reopened_session.as_ref(),
            0,
            56,
            &resumed_chain_contract,
        )
        .expect("load resumed chain metadata")
        .expect("resumed chain metadata exists");
    assert_eq!(resumed_chain_metadata.start_block, 5_000);
    assert_eq!(resumed_chain_metadata.last_scanned_block, 4_999);

    let report = runtime.block_on(initialize_new_wallet_chain_metadata_for_session(
        Arc::clone(&reopened_session),
        configs,
        Arc::clone(&db),
        http,
        None,
        CreatedWalletChainInitPolicy::Resumed,
    ));
    assert_eq!(report.initialized, 0);
    assert_eq!(report.skipped_existing, 0);
    assert_eq!(report.skipped_disabled, 1);
    assert_eq!(report.skipped_unavailable, 1);
    assert_eq!(report.failed, 0);
    assert_eq!(
        reopened_store
            .load_wallet_metadata_for_session(&reopened_session)
            .expect("reload pending metadata")
            .pending_create_new_chain_ids,
        BTreeSet::from([137, 999])
    );

    drop(reopened_session);
    drop(reopened_store);
    drop(db);
    let _ = fs::remove_dir_all(root_dir);
}

#[test]
fn pending_create_new_chain_uses_fresh_head_only_during_initial_installation() {
    assert_eq!(
        CreatedWalletChainInitPolicy::InitialCreate.sync_start_policy(),
        DesktopWalletSyncStartPolicy::CurrentSafeHeadNoBackfill
    );
    assert_eq!(
        CreatedWalletChainInitPolicy::Resumed.sync_start_policy(),
        DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill
    );

    let safe_head = 250;
    let received_block = 175;
    let resumed = resolve_desktop_wallet_chain_start(
        CreatedWalletChainInitPolicy::Resumed.sync_start_policy(),
        None,
        None,
        100,
        Some(safe_head),
        false,
    )
    .expect("resolve resumed pending chain start");
    assert_eq!(resumed.start_block, 100);
    assert!((resumed.start_block..=safe_head).contains(&received_block));
}

#[test]
fn desktop_wallet_start_policy_recovered_hardware_uses_deployment_block() {
    let metadata = hardware_wallet_metadata(HardwareWalletSyncIntent::RecoverExisting);
    assert_eq!(
        DesktopWalletSyncStartPolicy::from(&metadata),
        DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill
    );

    let resolved = resolve_desktop_wallet_chain_start(
        DesktopWalletSyncStartPolicy::from(&metadata),
        None,
        None,
        100,
        Some(250),
        false,
    )
    .expect("resolve recovered hardware start");

    assert_eq!(
        resolved,
        DesktopWalletChainStart {
            start_block: 100,
            last_scanned_block: 99,
        }
    );
}

#[test]
fn chain_config_uses_effective_rpc_pool_and_sync_tuning() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let root_dir = temp_db_root();
    let http = runtime
        .block_on(crate::build_wallet_network_context(
            crate::WalletNetworkConfig {
                network_mode: Some(crate::WalletNetworkMode::Direct),
                proxy: None,
                data_dir: &root_dir,
            },
        ))
        .expect("direct HTTP context");
    let mut effective = effective_chain_config_with_rpc_endpoints(
        1,
        vec![
            "https://rpc-a.example".into(),
            "https://rpc-b.example".into(),
        ],
        12_000,
    );
    effective.finality_depth = 99;
    effective.block_time = Some(Duration::from_secs(7));
    effective.gas.gas_limit_buffer = 250_000;
    effective.gas.gas_price_buffer_numerator = 110;
    let private = effective.railgun.as_mut().unwrap();
    private.archive_rpc_url = Some(
        reqwest::Url::parse("https://archive.example")
            .unwrap()
            .into(),
    );
    private.sync.quick_sync_endpoint = None;
    private.sync.indexed_wallet_block_range = 12_345;
    private.deployment.v2_start_block = 13_000;
    private.deployment.legacy_shield_block = 14_000;
    private.sync.archive_until_block = 12_500;
    private.sync.block_range = 2_000;
    private.sync.poll_interval = Duration::from_secs(30);

    let cfg = crate::chain_config(&effective, &http, None).expect("chain config");

    assert_eq!(cfg.sync.quick_sync_endpoint, None);
    assert!(cfg.sync.indexed_artifact_source.is_none());
    assert_eq!(cfg.sync.indexed_wallet_block_range, 12_345);
    assert_eq!(cfg.finality_depth, 99);
    assert_eq!(cfg.block_time, Duration::from_secs(7));
    assert_eq!(cfg.sync.block_range, 2_000);
    assert_eq!(cfg.sync.poll_interval, Duration::from_secs(30));
    assert_eq!(
        cfg.archive_rpc_url.as_ref().map(reqwest::Url::as_str),
        Some("https://archive.example/")
    );
    assert_eq!(cfg.deployment.deployment_block, 12_000);
    assert_eq!(cfg.deployment.v2_start_block, 13_000);
    assert_eq!(cfg.deployment.legacy_shield_block, 14_000);
    assert_eq!(cfg.sync.archive_until_block, 12_500);

    let first = cfg.rpcs.random_provider().expect("first provider");
    cfg.rpcs.mark_bad_provider(&first);
    let second = cfg.rpcs.random_provider().expect("fallback provider");
    assert_ne!(first.url, second.url);

    drop(http);
    let _ = fs::remove_dir_all(root_dir);
}

#[test]
fn chain_config_threads_indexed_artifact_source() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");
    let root_dir = temp_db_root();
    let http = runtime
        .block_on(crate::build_wallet_network_context(
            crate::WalletNetworkConfig {
                network_mode: Some(crate::WalletNetworkMode::Direct),
                proxy: None,
                data_dir: &root_dir,
            },
        ))
        .expect("direct HTTP context");
    let defaults = RailgunDeployment::for_chain(1).expect("ethereum defaults");
    let mut effective = effective_chain_config_with_rpc_endpoints(
        1,
        vec!["https://rpc.example".to_string()],
        defaults.deployment_block,
    );
    effective
        .railgun
        .as_mut()
        .unwrap()
        .indexed_artifact_source_mode = crate::settings::IndexedArtifactSourceModeSetting::Custom;
    effective
        .railgun
        .as_mut()
        .unwrap()
        .sync
        .indexed_artifact_source = Some(sync_service::IndexedArtifactSourceConfig {
        trusted_publisher_pubkey: FixedBytes::from([0x42; 32]),
        manifest_source: sync_service::IndexedArtifactManifestSource::IpnsName(
            "k51qzi5uqu5artifact".to_string(),
        ),
        gateway_urls: vec![reqwest::Url::parse("https://gateway.example").expect("url")],
        gateway_pool: None,
        manifest_reuse: sync_service::IndexedArtifactManifestReuse::default(),
        max_manifest_age: Some(Duration::from_mins(10)),
        concurrency: 5,
        max_in_flight_bytes: 8 * 1024 * 1024,
    });

    let cfg = crate::chain_config(&effective, &http, None).expect("chain config");
    let source = cfg
        .sync
        .indexed_artifact_source
        .as_ref()
        .expect("indexed artifact source");
    let gateway_pool = http.gateway_pool();

    assert_eq!(
        source.trusted_publisher_pubkey,
        FixedBytes::from([0x42; 32])
    );
    assert!(matches!(
        &source.manifest_source,
        sync_service::IndexedArtifactManifestSource::IpnsName(name)
            if name == "k51qzi5uqu5artifact"
    ));
    assert_eq!(source.gateway_urls[0].as_str(), "https://gateway.example/");
    assert_eq!(source.max_manifest_age, Some(Duration::from_mins(10)));
    assert_eq!(source.concurrency, 5);
    assert_eq!(source.max_in_flight_bytes, 8 * 1024 * 1024);
    assert_eq!(source.gateway_pool.as_ref(), Some(&gateway_pool));

    drop(http);
    let _ = fs::remove_dir_all(root_dir);
}

#[test]
fn desktop_wallet_start_policy_reuses_existing_metadata() {
    let existing = crate::vault::WalletChainMetadataBundle {
        wallet_chain_uuid: "wallet-chain".to_string(),
        wallet_uuid: "wallet".to_string(),
        chain_type: 0,
        chain_id: 1,
        contract: "0x1111111111111111111111111111111111111111".to_string(),
        start_block: 251,
        last_scanned_block: 300,
        last_scanned_block_hash: None,
        poi_read_source: None,
    };

    let resolved = resolve_desktop_wallet_chain_start(
        DesktopWalletSyncStartPolicy::CurrentSafeHeadNoBackfill,
        Some(&existing),
        None,
        100,
        None,
        false,
    )
    .expect("resolve existing start");

    assert_eq!(
        resolved,
        DesktopWalletChainStart {
            start_block: 251,
            last_scanned_block: 300,
        }
    );
}

#[test]
fn desktop_wallet_start_policy_generated_requires_safe_head() {
    let error = resolve_desktop_wallet_chain_start(
        DesktopWalletSyncStartPolicy::CurrentSafeHeadNoBackfill,
        None,
        None,
        100,
        None,
        false,
    )
    .expect_err("safe head required");

    assert!(error.to_string().contains("safe head unavailable"));
}

#[test]
fn desktop_wallet_rewind_uses_explicit_init_block() {
    let existing = crate::vault::WalletChainMetadataBundle {
        wallet_chain_uuid: "wallet-chain".to_string(),
        wallet_uuid: "wallet".to_string(),
        chain_type: 0,
        chain_id: 1,
        contract: "0x1111111111111111111111111111111111111111".to_string(),
        start_block: 251,
        last_scanned_block: 300,
        last_scanned_block_hash: None,
        poi_read_source: None,
    };

    let resolved = resolve_desktop_wallet_chain_start(
        DesktopWalletSyncStartPolicy::CurrentSafeHeadNoBackfill,
        Some(&existing),
        Some(existing.start_block),
        100,
        None,
        true,
    )
    .expect("resolve explicit rewind start");

    assert_eq!(
        resolved,
        DesktopWalletChainStart {
            start_block: existing.start_block,
            last_scanned_block: existing.start_block.saturating_sub(1),
        }
    );
}
