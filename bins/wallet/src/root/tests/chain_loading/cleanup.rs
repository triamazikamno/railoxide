use super::*;

#[tokio::test]
async fn wallet_sync_lifecycle_cleanup_detects_late_initialized_store() {
    let root_dir = temp_wallet_db_root("wallet-sync-lifecycle-late-store");
    let mut lifecycle = WalletSyncLifecycle::new();
    let registration = lifecycle.prepare_startup(1);
    let old_session_store = Arc::clone(&registration.session_store);
    let cleanup = lifecycle.invalidate();
    let poi_policy = wallet_ops::settings::WalletSettings::default()
        .poi_read_source()
        .expect("default POI policy");
    let vault_store = DesktopVaultStore::open(root_dir.clone()).expect("open wallet DB");
    let db = vault_store.db();
    let store = Arc::new(
        WalletSessionStore::from_db(Arc::clone(&db), poi_policy)
            .expect("acquire test sync manager ownership"),
    );

    assert!(old_session_store.set(Arc::clone(&store)).is_ok());
    let report = cleanup.shutdown().await.expect("shutdown lifecycle");

    assert_eq!(report.stopped_startup_tasks, 0);
    assert!(report.shut_down_session_store);
    let reset = wallet_ops::reset_persisted_poi_data(db.as_ref())
        .await
        .expect("offline PPOI reset after owner shutdown");
    assert_eq!(reset.data_records_removed, 0);
    drop(store);
    drop(db);
    drop(vault_store);
    let _ = fs::remove_dir_all(root_dir);
}

#[tokio::test]
async fn session_store_open_waits_for_pending_wallet_sync_cleanup() {
    let (pending, pending_tx) = WalletSyncLifecycleCleanupTask::channel_for_test();
    let mut wait = Box::pin(wait_for_wallet_sync_cleanups(vec![
        WalletSyncLifecycleCleanupTask::closed_for_test(),
        pending,
    ]));

    assert!(futures_util::poll!(wait.as_mut()).is_pending());
    pending_tx
        .send(Some(WalletSyncLifecycleCleanupReport::default()))
        .expect("cleanup receiver is alive");
    assert!(futures_util::poll!(wait.as_mut()).is_ready());
}

#[tokio::test]
async fn installed_chain_state_must_be_released_before_sync_manager_replacement() {
    const PASSWORD: &str = "sync replacement test password";
    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    let root_dir = temp_wallet_db_root("wallet-sync-replacement-ownership");
    let vault_store = DesktopVaultStore::open(root_dir.clone()).expect("open wallet DB");
    vault_store
        .create_vault_with_params(PASSWORD, wallet_ops::vault::KdfParams::new(1024, 1, 1))
        .expect("create vault");
    let wallet_id = "sync-replacement-wallet";
    let metadata = vault_store
        .new_wallet_metadata(
            PASSWORD,
            wallet_id,
            0,
            wallet_ops::vault::WalletSource::Imported,
            "Sync replacement wallet",
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
    let poi_policy = wallet_ops::settings::WalletSettings::default()
        .poi_read_source()
        .expect("default POI policy");
    let db = vault_store.db();
    let store = Arc::new(
        WalletSessionStore::from_db(Arc::clone(&db), poi_policy.clone())
            .expect("acquire initial sync manager ownership"),
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
    let session = Arc::new(
        Box::pin(store.start_view_wallet_session_immediate(
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
                sync_start_policy:
                    wallet_ops::DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
                init_block_number: Some(0),
                sync_to_block: Some(0),
                use_indexed_wallet_catch_up: false,
                poi_read_source: poi_policy.clone(),
                rewind_wallet_cache: false,
                progress_tx: None,
            },
            &http,
        ))
        .await
        .expect("start wallet session"),
    );
    let observation = session.observation_rx.borrow().clone();
    let mut chain_state = ChainUtxoState::Ready {
        snapshot: observation.snapshot,
        session,
        observer_token: registration.observer_token.clone(),
        sync_tip: WalletSyncTip::default(),
        poi_refreshing: false,
        ppoi_workflow_status: observation.ppoi_workflow_status,
    };
    assert!(matches!(chain_state, ChainUtxoState::Ready { .. }));

    lifecycle
        .invalidate()
        .shutdown()
        .await
        .expect("shutdown original lifecycle");
    let ownership_error = WalletSessionStore::from_db(Arc::clone(&db), poi_policy.clone())
        .err()
        .expect("installed chain state retains database ownership");
    let ownership_error = format!("{ownership_error:#}");
    assert!(ownership_error.contains("database is already owned by an active runtime operation"));

    chain_state = ChainUtxoState::Idle;
    assert!(matches!(chain_state, ChainUtxoState::Idle));
    let replacement = WalletSessionStore::from_db(Arc::clone(&db), poi_policy)
        .expect("acquire replacement sync manager ownership");
    replacement.shutdown().await;

    drop(replacement);
    drop(store);
    drop(registration);
    drop(db);
    drop(vault_store);
    let _ = fs::remove_dir_all(root_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wallet_sync_lifecycle_cleanup_timeout_keeps_cleanup_retryable() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let registration = lifecycle.prepare_startup(1);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let join = tokio::task::spawn_blocking(move || {
        let _ = started_tx.send(());
        std::thread::sleep(Duration::from_millis(100));
    });
    lifecycle.track_startup(&registration, join);
    started_rx.await.expect("blocking cleanup task started");
    let cleanup = lifecycle.invalidate();
    let cleanup_task = cleanup.spawn(&tokio::runtime::Handle::current());
    let admission_barrier = WalletSyncLifecycleCleanupWaitGroup::new(vec![cleanup_task.clone()]);
    assert!(!admission_barrier.is_finished());
    assert!(!wallet_replacement_finalize_is_admitted(
        7,
        7,
        1,
        1,
        false,
        admission_barrier.is_finished(),
    ));

    assert!(matches!(
        wait_for_wallet_replacement_cleanup(
            WalletSyncLifecycleCleanupWaitGroup::new(vec![cleanup_task.clone()]),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await,
        WalletReplacementCleanupWaitOutcome::PresentationTimedOut
    ));
    assert!(!cleanup_task.is_finished());
    assert!(!wallet_replacement_finalize_is_admitted(
        7,
        7,
        1,
        1,
        false,
        admission_barrier.is_finished(),
    ));

    let report = match wait_for_wallet_replacement_cleanup(
        WalletSyncLifecycleCleanupWaitGroup::new(vec![cleanup_task]),
        tokio::time::sleep(Duration::from_secs(1)),
    )
    .await
    {
        WalletReplacementCleanupWaitOutcome::Completed(Ok(report)) => report,
        WalletReplacementCleanupWaitOutcome::Completed(Err(error)) => {
            panic!("retry cleanup failed: {error}")
        }
        WalletReplacementCleanupWaitOutcome::PresentationTimedOut => {
            panic!("retry should wait on retained cleanup")
        }
    };
    assert_eq!(report.stopped_startup_tasks, 1);
    assert!(admission_barrier.is_finished());
    assert!(wallet_replacement_finalize_is_admitted(
        7,
        7,
        1,
        1,
        false,
        admission_barrier.is_finished(),
    ));
    assert!(!wallet_replacement_finalize_is_admitted(
        6,
        7,
        1,
        1,
        false,
        admission_barrier.is_finished(),
    ));
}

#[test]
fn wallet_replacement_timeout_contract_rejects_stale_updates() {
    assert_eq!(
        WALLET_REPLACEMENT_TIMEOUT_MESSAGE,
        "Your wallet is still closing. Try again in a moment."
    );

    let current = |switch, operation, lifetime, switching| {
        wallet_replacement_update_is_current(
            7, 7, switch, 11, operation, 13, lifetime, 17, 1, 1, false, switching,
        )
    };

    assert!(current(11, 13, 17, true));
    assert!(!current(10, 13, 17, true));
    assert!(!current(11, 12, 17, true));
    assert!(!current(11, 13, 16, true));
    assert!(!current(11, 13, 17, false));
}

#[tokio::test]
async fn closed_cleanup_is_terminal_but_still_surfaces_explicit_error() {
    let cleanup = WalletSyncLifecycleCleanupWaitGroup::new(vec![
        WalletSyncLifecycleCleanupTask::closed_for_test(),
    ]);
    assert!(cleanup.is_finished());

    let error = cleanup
        .shutdown_for_wallet_deletion()
        .await
        .expect_err("closed cleanup channel should fail deletion");

    assert_eq!(error, "wallet sync cleanup task ended before completion");
    assert!(!public_sync_reset_restart_is_safe(
        true,
        PublicSyncResetCompletion::CleanupFailed,
    ));
}
