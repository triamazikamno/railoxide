use super::*;

#[tokio::test]
async fn chain_startups_overlap_but_wallet_replacement_waits_for_them() {
    use futures_util::poll;
    use tokio::net::TcpListener;

    let root_dir = std::env::temp_dir().join(format!(
        "concurrent-chain-startup-{}",
        vault::generate_opaque_id().unwrap()
    ));
    let vault = vault::DesktopVaultStore::open(root_dir.clone()).unwrap();
    let password = "synthetic startup concurrency password";
    vault
        .create_vault_with_params(password, vault::KdfParams::new(1024, 1, 1))
        .unwrap();
    let wallet_id = vault::generate_opaque_id().unwrap();
    let metadata = vault
        .new_wallet_metadata(
            password,
            &wallet_id,
            0,
            vault::WalletSource::Imported,
            "Test",
        )
        .unwrap();
    vault.import_wallet_mnemonic_with_metadata(
        password, &wallet_id, 0, "english",
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", &metadata,
    ).unwrap();
    let view = Arc::new(vault.load_view_session(password, &wallet_id).unwrap());
    let poi = PoiReadSource::PoiProxy {
        rpc_url: Url::parse("http://127.0.0.1:1").unwrap().into(),
    };
    let sessions = Arc::new(WalletSessionStore::from_db(vault.db(), poi.clone()).unwrap());
    let http = HttpContext::direct_for_tests();
    let chains =
        settings::build_effective_chain_configs(&settings::WalletSettings::default()).unwrap();
    let owner = sessions
        .create_executor_owner(
            1,
            view.clone(),
            chains.get(1).unwrap().clone(),
            http.clone(),
        )
        .unwrap();
    let request = |chain_id, generation, listener: &TcpListener| {
        let mut chain = chains.get(chain_id).unwrap().clone();
        chain.rpc_route = crate::RpcChainRoute::new(
            chain_id,
            vec![Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap()],
        );
        let private = chain.railgun.as_mut().unwrap();
        private.archive_rpc_url = None;
        private.sync.quick_sync_endpoint = None;
        private.sync.indexed_artifact_source = None;
        ViewWalletChainSessionRequest {
            view_session: view.clone(),
            wallet_scope_generation: generation,
            chain_id,
            effective_chain: chain,
            sync_start_policy: DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
            init_block_number: Some(0),
            sync_to_block: Some(0),
            use_indexed_wallet_catch_up: false,
            poi_read_source: poi.clone(),
            rewind_wallet_cache: false,
            progress_tx: None,
        }
    };
    let start = |request| {
        let sessions = Arc::clone(&sessions);
        let http = http.clone();
        tokio::spawn(async move {
            Box::pin(sessions.start_view_wallet_session_immediate(request, &http)).await
        })
    };

    let polygon = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let first = start(request(137, 1, &polygon));
    let held_polygon = tokio::time::timeout(Duration::from_secs(2), polygon.accept())
        .await
        .unwrap()
        .unwrap();
    // Polygon never answers. Arbitrum must still reach its own RPC independently.
    let arbitrum = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let second = start(request(42161, 1, &arbitrum));
    let held_arbitrum = tokio::time::timeout(Duration::from_secs(2), arbitrum.accept())
        .await
        .expect("a stalled chain must not block another chain's startup")
        .unwrap();
    assert!(!first.is_finished() && !second.is_finished());
    second.abort();
    assert!(matches!(second.await, Err(error) if error.is_cancelled()));
    drop(held_arbitrum);

    // A new wallet generation must wait for the old startup before retiring its owners.
    let replacement_rpc = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let mut replacement = Box::pin(
        sessions.start_view_wallet_session_immediate(request(56, 2, &replacement_rpc), &http),
    );
    assert!(poll!(replacement.as_mut()).is_pending());
    assert!(
        owner.records().is_ok(),
        "replacement must wait for the old startup"
    );
    first.abort();
    assert!(matches!(first.await, Err(error) if error.is_cancelled()));
    drop(held_polygon);
    let held_replacement = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            _ = &mut replacement => panic!("replacement RPC should still be pending"),
            connection = replacement_rpc.accept() => connection.unwrap(),
        }
    })
    .await
    .expect("cancelling the old startup must unblock replacement");
    assert!(
        owner.records().is_err(),
        "the old owner is retired before replacement starts"
    );
    drop(replacement);
    drop(held_replacement);
    sessions.shutdown().await;
    drop(owner);
    drop(sessions);
    drop(view);
    drop(vault);
    std::fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn executor_lifecycle_cancels_reads_and_rejects_stale_owners_without_losing_records() {
    let root_dir = std::env::temp_dir().join(format!(
        "executor-lifecycle-{}",
        vault::generate_opaque_id().unwrap()
    ));
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .unwrap(),
    );
    let vault = vault::DesktopVaultStore::from_db(db.clone());
    let password = "synthetic executor lifecycle password";
    vault
        .create_vault_with_params(password, vault::KdfParams::new(1024, 1, 1))
        .unwrap();
    let wallet_id = vault::generate_opaque_id().unwrap();
    let metadata = vault
        .new_wallet_metadata(
            password,
            &wallet_id,
            0,
            vault::WalletSource::Imported,
            "Test",
        )
        .unwrap();
    vault.import_wallet_mnemonic_with_metadata(
        password, &wallet_id, 0, "english",
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about", &metadata,
    ).unwrap();
    let view = Arc::new(vault.load_view_session(password, &wallet_id).unwrap());
    let sessions = WalletSessionStore::from_db(
        db.clone(),
        PoiReadSource::PoiProxy {
            rpc_url: Url::parse("http://127.0.0.1:1").unwrap().into(),
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let mut chain = settings::build_effective_chain_configs(&settings::WalletSettings::default())
        .unwrap()
        .get(1)
        .cloned()
        .unwrap();
    chain.rpc_route = crate::RpcChainRoute::new(
        1,
        vec![Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap()],
    );
    let http = HttpContext::direct_for_tests();
    let owner = sessions
        .create_executor_owner(0, view.clone(), chain.clone(), http.clone())
        .unwrap();
    let mut public_only = chain.clone();
    public_only.railgun = None;
    let rejected = Box::pin(sessions.start_view_wallet_session_immediate(
        ViewWalletChainSessionRequest {
            view_session: view.clone(),
            wallet_scope_generation: 99,
            chain_id: 1,
            effective_chain: public_only,
            sync_start_policy: DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
            init_block_number: None,
            sync_to_block: None,
            use_indexed_wallet_catch_up: false,
            poi_read_source: PoiReadSource::PoiProxy {
                rpc_url: Url::parse("http://127.0.0.1:1").unwrap().into(),
            },
            rewind_wallet_cache: false,
            progress_tx: None,
        },
        &http,
    ))
    .await;
    assert!(rejected.is_err());
    // The rejected request must not retire the current executor before the checks below.
    let records = vault::ExecutorStore::new(db.clone(), view.clone(), 1).unwrap();
    let record = records
        .restore_index(
            7,
            Address::repeat_byte(7),
            chain.accepted_executor_profile().unwrap().delegate(),
            &[],
        )
        .unwrap();
    let observed = vault::ExecutorNonceObservation::new(
        alloy::eips::BlockNumHash::new(10, FixedBytes::repeat_byte(10)),
        U256::from(3),
    );
    records
        .record_account_read(record.operation(), observed)
        .unwrap();
    let payload_hash = FixedBytes::repeat_byte(4);
    records
        .record_issued(
            record.operation(),
            vault::IssuedExecutorPayload::new(
                observed.nonce(),
                record.delegate(),
                payload_hash,
                vault::ExecutorPayloadPurpose::Operation,
                vault::ExecutorPayloadContext::new(
                    Bytes::from_static(b"issued lifecycle fixture"),
                    observed,
                    Vec::new(),
                ),
            ),
        )
        .unwrap();
    records
        .record_account_read(
            record.operation(),
            vault::ExecutorNonceObservation::new(
                alloy::eips::BlockNumHash::new(12, FixedBytes::repeat_byte(12)),
                U256::from(4),
            ),
        )
        .unwrap();
    let reading_owner = owner.clone();
    let reading = tokio::spawn(async move {
        reading_owner
            .inspect_record(
                record.operation(),
                &[],
                crate::OperationHttpClient::direct_for_tests(),
            )
            .await
    });
    let (_connection, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();

    sessions.invalidate_executor_owners(1);
    assert!(owner.records().is_err());
    assert!(
        sessions
            .create_executor_owner(0, view.clone(), chain.clone(), http.clone())
            .is_err()
    );
    assert_eq!(
        records.records().unwrap()[0].payload_state(payload_hash),
        Some(vault::ExecutorPayloadState::Resolved)
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), reading)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    sessions.shutdown_superseded_executor_owners(1).await;
    let replacement = sessions
        .create_executor_owner(1, view.clone(), chain.clone(), http.clone())
        .unwrap();
    assert_eq!(replacement.records().unwrap().len(), 1);
    assert_eq!(
        replacement.records().unwrap()[0].payload_state(payload_hash),
        Some(vault::ExecutorPayloadState::Resolved)
    );
    assert_eq!(
        records.records().unwrap()[0].payload_state(payload_hash),
        Some(vault::ExecutorPayloadState::Resolved)
    );
    assert_eq!(records.next_index().unwrap(), 8);
    sessions.shutdown().await;
    assert!(replacement.records().is_err());
    assert!(
        sessions
            .create_executor_owner(2, view.clone(), chain, http)
            .is_err()
    );
    drop(replacement);
    drop(owner);
    drop(records);
    drop(sessions);
    drop(view);
    drop(vault);
    drop(db);
    std::fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn offline_poi_data_reset_accepts_database_without_active_owners() {
    let root_dir = std::env::temp_dir().join(format!(
        "wallet-ops-offline-poi-reset-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ));
    let db = DbStore::open(DbConfig {
        root_dir: root_dir.clone(),
    })
    .expect("open local DB");

    let report = reset_persisted_poi_data(&db)
        .await
        .expect("reset empty PPOI data");

    assert_eq!(report, PersistedPoiDataResetReport::default());
    drop(db);
    std::fs::remove_dir_all(root_dir).expect("remove local DB");
}

#[test]
fn wallet_session_observation_keeps_ppoi_workflow_separate_from_snapshot() {
    let status = WalletPpoiWorkflowStatus {
        awaiting_recovery: 5,
        awaiting_public_txid_data: 0,
        awaiting_poi_data: 0,
        retrying_recovery: 0,
        recovery_needs_attention: 0,
        awaiting_submission: 1,
        awaiting_validation: 2,
        needs_attention: 3,
        validation_revision: 4,
    };
    let projected = wallet_session_observation_from_parts(
        Arc::new(empty_wallet_snapshot(1, "wallet")),
        WalletReadiness::Syncing,
        status,
    );

    assert_eq!(projected.ppoi_workflow_status, status);
    assert_eq!(projected.snapshot.utxo_count, 0);
}

#[test]
fn active_wallet_scope_rejects_stale_and_same_generation_replacement() {
    let mut scope = ActiveWalletScope::default();
    assert!(
        scope
            .requires_replacement(0, "wallet-a")
            .expect("initial scope")
    );
    scope.replace(0, "wallet-a".to_string());
    assert!(
        !scope
            .requires_replacement(0, "wallet-a")
            .expect("same scope")
    );
    assert!(scope.requires_replacement(0, "wallet-b").is_err());
    assert!(
        scope
            .requires_replacement(1, "wallet-a")
            .expect("new generation of same wallet")
    );

    assert!(
        scope
            .requires_replacement(1, "wallet-b")
            .expect("new scope")
    );
    scope.replace(1, "wallet-b".to_string());
    assert!(scope.requires_replacement(0, "wallet-a").is_err());
}

#[test]
fn head_advance_state_uses_monotonic_observed_head() {
    let mut max_observed_head_block = 100;
    let mut advanced_at = Some(10);

    update_head_advance_state(&mut max_observed_head_block, &mut advanced_at, 99, 20);
    assert_eq!(max_observed_head_block, 100);
    assert_eq!(advanced_at, Some(10));

    update_head_advance_state(&mut max_observed_head_block, &mut advanced_at, 100, 30);
    assert_eq!(max_observed_head_block, 100);
    assert_eq!(advanced_at, Some(10));

    update_head_advance_state(&mut max_observed_head_block, &mut advanced_at, 101, 40);
    assert_eq!(max_observed_head_block, 101);
    assert_eq!(advanced_at, Some(40));
}

#[test]
fn wallet_sync_tip_publish_forces_time_driven_refresh() {
    let tip = WalletSyncTip {
        last_scanned_block: Some(100),
        head_block: Some(112),
        safe_head_block: Some(100),
        head_last_advanced_at_unix_secs: Some(10),
        indexed_catch_up: None,
    };
    let (tx, mut rx) = watch::channel(tip);
    let mut last_tip = tip;

    assert!(publish_wallet_sync_tip(&tx, &mut last_tip, tip, false));
    assert!(!rx.has_changed().expect("watch receiver open"));

    assert!(publish_wallet_sync_tip(&tx, &mut last_tip, tip, true));
    assert!(rx.has_changed().expect("watch receiver notified"));
    assert_eq!(*rx.borrow_and_update(), tip);

    let advanced_tip = WalletSyncTip {
        last_scanned_block: Some(101),
        ..tip
    };
    assert!(publish_wallet_sync_tip(
        &tx,
        &mut last_tip,
        advanced_tip,
        false,
    ));
    assert_eq!(last_tip, advanced_tip);
    assert!(rx.has_changed().expect("watch receiver notified"));
}

fn ethereum_statistics_status(
    wallet_settings: &settings::WalletSettings,
    running_chain_status: impl FnOnce() -> Option<AmountStatisticsStatus>,
) -> AmountStatisticsStatus {
    let chains = settings::build_effective_chain_configs(wallet_settings).unwrap();
    amount_statistics_status_for_chain(chains.get(1), running_chain_status)
}

#[test]
fn amount_statistics_status_asks_the_running_chain_only_when_a_source_names_statistics() {
    let not_asked = || -> Option<AmountStatisticsStatus> {
        panic!("a chain without an amount-statistics source must not ask the sync manager")
    };
    let mut wallet_settings = settings::WalletSettings::default();
    wallet_settings.indexed_artifacts.source_mode =
        settings::IndexedArtifactSourceModeSetting::Custom;
    wallet_settings.indexed_artifacts.publisher_pubkey = Some(format!("0x{}", "11".repeat(32)));
    wallet_settings.indexed_artifacts.manifest_source =
        Some(settings::IndexedArtifactManifestSourceSetting::IpnsName(
            settings::OFFICIAL_INDEXED_ARTIFACT_IPNS_NAME.to_string(),
        ));
    assert_eq!(
        ethereum_statistics_status(&wallet_settings, not_asked),
        AmountStatisticsStatus::NotConfigured
    );

    wallet_settings
        .indexed_artifacts
        .amount_statistics_ipns_name =
        Some("k51qzi5uqu5dicmabkge4lkunc4bkd198u9xicp5espmw5zdzbafkez7hyh5ft".to_string());
    assert_eq!(
        ethereum_statistics_status(&wallet_settings, || None),
        AmountStatisticsStatus::NotYetAvailable
    );
    assert_eq!(
        ethereum_statistics_status(&wallet_settings, || Some(AmountStatisticsStatus::OutOfDate)),
        AmountStatisticsStatus::OutOfDate
    );

    wallet_settings.indexed_artifacts.source_mode =
        settings::IndexedArtifactSourceModeSetting::Disabled;
    assert_eq!(
        ethereum_statistics_status(&wallet_settings, not_asked),
        AmountStatisticsStatus::NotConfigured
    );
}
