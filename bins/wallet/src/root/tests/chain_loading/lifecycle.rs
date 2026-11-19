use super::*;

#[test]
fn chain_load_uses_default_sync_options() {
    let overrides = crate::root::tests::chain_load_overrides();

    assert_eq!(overrides.init_block_number, None);
    assert_eq!(overrides.sync_to_block, None);
    assert_eq!(overrides.sync_start_policy, None);
    assert!(overrides.use_indexed_wallet_catch_up);
    assert!(!overrides.rewind_wallet_cache);
}

#[tokio::test]
async fn wallet_sync_lifecycle_cleanup_aborts_in_flight_startups() {
    struct NotifyOnDrop(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    let mut lifecycle = WalletSyncLifecycle::new();
    let registration = lifecycle.prepare_startup(1);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        let _notify = NotifyOnDrop(Some(dropped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    });
    lifecycle.track_startup(&registration, join);

    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .expect("startup notification timeout")
        .expect("startup notification");
    assert!(lifecycle.is_current_startup(1, registration.generation, registration.task_id));
    let cleanup = lifecycle.invalidate();

    assert!(!lifecycle.is_current_startup(1, registration.generation, registration.task_id));
    let report = cleanup.shutdown().await.expect("shutdown lifecycle");
    assert_eq!(report.stopped_startup_tasks, 1);
    assert!(!report.shut_down_session_store);
    tokio::time::timeout(Duration::from_secs(1), dropped_rx)
        .await
        .expect("abort notification timeout")
        .expect("abort notification");
}

#[tokio::test]
async fn wallet_sync_lifecycle_cleanup_aborts_wallet_scoped_tasks() {
    struct NotifyOnDrop(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    let mut lifecycle = WalletSyncLifecycle::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    lifecycle.track_wallet_task(tokio::spawn(async move {
        let _notify = NotifyOnDrop(Some(dropped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    }));

    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .expect("wallet task notification timeout")
        .expect("wallet task notification");
    let report = lifecycle
        .supersede_wallet()
        .shutdown()
        .await
        .expect("shutdown lifecycle");

    assert_eq!(report.stopped_startup_tasks, 1);
    tokio::time::timeout(Duration::from_secs(1), dropped_rx)
        .await
        .expect("wallet task abort notification timeout")
        .expect("wallet task abort notification");
}

#[tokio::test]
async fn wallet_sync_lifecycle_supersede_retains_session_store() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let first = lifecycle.prepare_startup(1);
    let first_store = Arc::clone(&first.session_store);

    let cleanup = lifecycle.supersede_wallet();
    let second = lifecycle.prepare_startup(1);
    let report = cleanup.shutdown().await.expect("cleanup superseded wallet");

    assert!(Arc::ptr_eq(&first_store, &second.session_store));
    assert_eq!(second.generation, first.generation + 1);
    assert!(!report.shut_down_session_store);
}

#[tokio::test]
async fn wallet_sync_lifecycle_reset_inventory_survives_loading_and_error_without_session() {
    const PASSWORD: &str = "sync reset test password";
    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    let root_dir = temp_wallet_db_root("wallet-sync-reset-lifecycle");
    let vault_store =
        wallet_ops::vault::DesktopVaultStore::open(root_dir.clone()).expect("open wallet store");
    vault_store
        .create_vault_with_params(PASSWORD, wallet_ops::vault::KdfParams::new(1024, 1, 1))
        .expect("create vault");
    let wallet_id = "sync-reset-wallet";
    let metadata = vault_store
        .new_wallet_metadata(
            PASSWORD,
            wallet_id,
            0,
            wallet_ops::vault::WalletSource::Imported,
            "Sync reset wallet",
        )
        .expect("wallet metadata");
    vault_store
        .import_wallet_mnemonic_with_metadata(
            PASSWORD, wallet_id, 0, "english", MNEMONIC, &metadata,
        )
        .expect("import wallet");
    let view_session = Arc::new(
        vault_store
            .load_view_session(PASSWORD, wallet_id)
            .expect("load view session"),
    );
    let poi_rpc_url = reqwest::Url::parse("http://127.0.0.1:1").expect("POI RPC URL");
    let poi_policy = wallet_ops::PoiReadSource::PoiProxy {
        rpc_url: poi_rpc_url.clone().into(),
    };
    let store = Arc::new(
        WalletSessionStore::from_db(vault_store.db(), poi_policy.clone())
            .expect("acquire test sync manager ownership"),
    );
    let http = wallet_ops::build_wallet_network_context(wallet_ops::WalletNetworkConfig {
        network_mode: Some(WalletNetworkMode::Direct),
        proxy: None,
        data_dir: &root_dir,
    })
    .await
    .expect("build direct HTTP context");
    let mut lifecycle = WalletSyncLifecycle::new();
    let registration = lifecycle.prepare_startup(1);
    assert!(registration.session_store.set(Arc::clone(&store)).is_ok());
    let session = Box::pin(store.start_view_wallet_session_immediate(
        wallet_ops::ViewWalletChainSessionRequest {
            view_session,
            wallet_scope_generation: registration.generation,
            chain_id: 1,
            effective_chain: {
                let mut chain = wallet_ops::settings::build_effective_chain_configs(
                    &wallet_ops::settings::WalletSettings::default(),
                )
                .unwrap()
                .get(1)
                .cloned()
                .unwrap();
                chain.rpc_route = wallet_ops::RpcChainRoute::new(
                    1,
                    vec![reqwest::Url::parse("http://127.0.0.1:1").expect("RPC URL")],
                );
                let private = chain.railgun.as_mut().unwrap();
                private.archive_rpc_url = None;
                private.sync.quick_sync_endpoint = None;
                private.sync.indexed_artifact_source = None;
                chain
            },
            sync_start_policy: wallet_ops::DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
            init_block_number: Some(0),
            sync_to_block: Some(0),
            use_indexed_wallet_catch_up: false,
            poi_read_source: poi_policy,
            rewind_wallet_cache: false,
            progress_tx: None,
        },
        &http,
    ))
    .await
    .expect("register chain and wallet session");
    let mut observation_rx = session.observation_rx.clone();
    let initial_observation = observation_rx.borrow_and_update().clone();
    assert_eq!(initial_observation.snapshot.chain_id, 1);
    tokio::time::timeout(Duration::from_secs(1), session.stop())
        .await
        .expect("wallet session stop timeout")
        .expect("stop wallet session");
    assert_eq!(
        observation_rx.borrow_and_update().readiness,
        WalletReadiness::Shutdown
    );
    assert_eq!(observation_rx.borrow().snapshot.utxo_count, 0);
    drop(session);

    let loading = ChainUtxoState::Loading {
        progress: ChainSyncProgress::default(),
    };
    assert!(loading.poi_refresh_session().is_none());
    let reset_store = lifecycle
        .public_sync_cache_reset_cell()
        .get()
        .cloned()
        .expect("manager reset inventory remains available while loading");
    let loading_report = reset_store
        .reset_public_sync_caches()
        .await
        .expect("loading-state reset admitted");
    assert_eq!(loading_report.chains.len(), 1);
    assert_eq!(loading_report.failed_chain_count(), 0);
    assert_eq!(loading_report.chains[0].chain.chain_id, 1);
    assert_eq!(
        loading_report.chains[0]
            .result
            .as_ref()
            .expect("loading reset succeeds")
            .new_epoch
            .value,
        1,
    );

    let error = ChainUtxoState::Error {
        message: Arc::from("sync failed after chain registration"),
        start_block: Some(1),
        ppoi_workflow_status: WalletPpoiWorkflowStatus::default(),
    };
    assert!(error.poi_refresh_session().is_none());
    let error_report = reset_store
        .reset_public_sync_caches()
        .await
        .expect("error-state reset admitted");
    assert_eq!(error_report.chains.len(), 1);
    assert_eq!(error_report.failed_chain_count(), 0);
    assert_eq!(
        error_report.chains[0]
            .result
            .as_ref()
            .expect("error-state reset succeeds")
            .new_epoch
            .value,
        2,
    );

    lifecycle
        .invalidate()
        .shutdown()
        .await
        .expect("shutdown lifecycle");
    drop(reset_store);
    drop(store);
    drop(registration);
    drop(vault_store);
    let _ = fs::remove_dir_all(root_dir);
}
