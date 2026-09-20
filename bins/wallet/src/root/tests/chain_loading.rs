use super::*;
use crate::root::auto_lock::AutoLockTimestamp;
use crate::root::chain_load::{
    ChainProgressProjection, InstalledObserverProjection, WalletReadinessDisposition,
    WalletSyncLifecycle, WalletSyncLifecycleCleanupTask, WalletSyncLifecycleCleanupWaitGroup,
    chain_load_start_is_allowed, chain_progress_update_is_current,
    destructive_cache_reset_admission_is_allowed, installed_observer_is_exact_current,
    installed_observer_terminal_transition, ppoi_validation_completion_is_current,
    ppoi_validation_toast_scope_is_current, retain_auxiliary_stream, wallet_readiness_disposition,
    wallet_sync_maintenance_allows_start, wallet_sync_start_is_admitted,
};
use crate::root::maintenance::{PublicSyncResetCompletion, public_sync_reset_restart_is_safe};
use crate::root::shell::{
    PoiArtifactCacheRetryAttempts, ppoi_chunk_progress_label, ppoi_replay_progress_label,
    ppoi_retry_completion_is_current,
};
use crate::root::vault::{
    WALLET_REPLACEMENT_TIMEOUT_MESSAGE, WalletReplacementCleanupWaitOutcome,
    wait_for_wallet_replacement_cleanup, wallet_replacement_finalize_is_admitted,
    wallet_replacement_update_is_current,
};
use wallet_ops::{
    PoiArtifactCacheAttemptId, PoiArtifactCacheGraphProgress, PoiArtifactCachePhase,
    PoiArtifactCacheProgress, WalletIndexedCatchUpSource, WalletIndexedCatchUpStatus,
    WalletNetworkMode, WalletPpoiWorkflowStatus, WalletReadiness, WalletReadinessError,
    WalletSessionStore, WalletSyncTip,
};

use std::time::Duration;

#[test]
fn terminal_wallet_readiness_is_not_projected_as_syncing() {
    assert_eq!(
        wallet_readiness_disposition(&WalletReadiness::Failed(
            WalletReadinessError::PersistenceFailed,
        )),
        WalletReadinessDisposition::Error(Arc::from("wallet sync state could not be persisted")),
    );
    assert_eq!(
        wallet_readiness_disposition(&WalletReadiness::Shutdown),
        WalletReadinessDisposition::Error(Arc::from("wallet sync session stopped")),
    );
}

#[test]
fn pending_profile_has_no_wallet_sync_admission_until_verified() {
    assert!(!wallet_sync_start_is_admitted(true, false, false, true));
    assert!(!wallet_sync_start_is_admitted(false, false, true, true));
    assert!(!wallet_sync_start_is_admitted(false, true, false, true));
    assert!(wallet_sync_start_is_admitted(false, true, true, true));
}

fn observer_progress(current_block: u64) -> InitialCatchUpFingerprint {
    InitialCatchUpFingerprint::new(
        Some(SyncProgressUpdate::new(
            SyncProgressStage::IndexingUtxos,
            0,
            current_block,
            100,
        )),
        None,
    )
}

#[test]
fn initial_sync_observation_boundary_tracks_activity_and_first_ready() {
    let started = AutoLockTimestamp::now();
    let timeout = Duration::from_mins(1);
    let mut tracker = InitialSyncActivity::new(1);
    let mut auto_lock = AutoLockState::new(Some(timeout));
    auto_lock.arm_after_view_unlock(started);

    assert!(!apply_initial_sync_observation(
        &mut tracker,
        &mut auto_lock,
        true,
        1,
        1,
        InitialSyncObservation::Started,
        started,
    ));
    let progress_at = started + Duration::from_secs(20);
    assert!(apply_initial_sync_observation(
        &mut tracker,
        &mut auto_lock,
        true,
        1,
        1,
        InitialSyncObservation::Progress(observer_progress(10)),
        progress_at,
    ));
    assert!(!apply_initial_sync_observation(
        &mut tracker,
        &mut auto_lock,
        true,
        1,
        1,
        InitialSyncObservation::Progress(observer_progress(10)),
        progress_at + Duration::from_secs(10),
    ));
    assert!(!apply_initial_sync_observation(
        &mut tracker,
        &mut auto_lock,
        true,
        1,
        1,
        InitialSyncObservation::Error,
        progress_at + Duration::from_secs(20),
    ));
    let ready_at = progress_at + Duration::from_secs(30);
    assert!(apply_initial_sync_observation(
        &mut tracker,
        &mut auto_lock,
        true,
        1,
        1,
        InitialSyncObservation::Ready,
        ready_at,
    ));
    assert!(!apply_initial_sync_observation(
        &mut tracker,
        &mut auto_lock,
        true,
        1,
        1,
        InitialSyncObservation::Progress(observer_progress(20)),
        ready_at + Duration::from_secs(10),
    ));
    assert_eq!(
        auto_lock.deadline_status(
            true,
            ready_at + timeout.saturating_sub(Duration::from_secs(1)),
        ),
        AutoLockDeadlineStatus::Waiting
    );
}

#[test]
fn initial_sync_observation_boundary_resets_wallets_and_tracks_new_chains() {
    let started = AutoLockTimestamp::now();
    let mut tracker = InitialSyncActivity::new(1);
    let mut auto_lock = AutoLockState::new(Some(Duration::from_mins(1)));
    auto_lock.arm_after_view_unlock(started);

    for (generation, chain_id) in [(1, 1), (1, 137), (2, 1)] {
        assert!(!apply_initial_sync_observation(
            &mut tracker,
            &mut auto_lock,
            true,
            generation,
            chain_id,
            InitialSyncObservation::Started,
            started,
        ));
        assert!(apply_initial_sync_observation(
            &mut tracker,
            &mut auto_lock,
            true,
            generation,
            chain_id,
            InitialSyncObservation::Progress(observer_progress(10)),
            started + Duration::from_secs(generation + chain_id),
        ));
        assert!(apply_initial_sync_observation(
            &mut tracker,
            &mut auto_lock,
            true,
            generation,
            chain_id,
            InitialSyncObservation::Ready,
            started + Duration::from_secs(generation + chain_id + 1),
        ));
    }
}

#[test]
fn ppoi_validation_toast_requires_later_exact_observer_revision() {
    let pending = WalletPpoiWorkflowStatus {
        awaiting_validation: 2,
        validation_revision: 3,
        ..WalletPpoiWorkflowStatus::default()
    };
    let partially_complete = WalletPpoiWorkflowStatus {
        awaiting_validation: 1,
        validation_revision: 4,
        ..WalletPpoiWorkflowStatus::default()
    };
    let complete = WalletPpoiWorkflowStatus {
        validation_revision: 5,
        ..WalletPpoiWorkflowStatus::default()
    };
    assert!(!ppoi_validation_completion_is_current(
        pending,
        partially_complete,
        true
    ));
    assert!(ppoi_validation_completion_is_current(
        partially_complete,
        complete,
        true
    ));
    assert!(!ppoi_validation_completion_is_current(
        partially_complete,
        complete,
        false
    ));
}

#[test]
fn ppoi_validation_toast_scope_rejects_wallet_replacement() {
    assert!(ppoi_validation_toast_scope_is_current(
        Some("wallet-a"),
        7,
        "wallet-a",
        7,
    ));
    assert!(!ppoi_validation_toast_scope_is_current(
        Some("wallet-b"),
        8,
        "wallet-a",
        7,
    ));
    assert!(!ppoi_validation_toast_scope_is_current(
        None, 7, "wallet-a", 7,
    ));
}

#[test]
fn chain_load_start_is_blocked_only_for_selected_wallet_deletion() {
    assert!(!chain_load_start_is_allowed(
        Some("selected-wallet"),
        Some("selected-wallet")
    ));
    assert!(chain_load_start_is_allowed(
        Some("hidden-wallet"),
        Some("selected-wallet")
    ));
    assert!(chain_load_start_is_allowed(None, Some("selected-wallet")));
}

#[test]
fn wallet_sync_start_is_blocked_during_either_destructive_cache_reset() {
    assert!(wallet_sync_maintenance_allows_start(false, false));
    assert!(!wallet_sync_maintenance_allows_start(true, false));
    assert!(!wallet_sync_maintenance_allows_start(false, true));
    assert!(!wallet_sync_maintenance_allows_start(true, true));
}

#[test]
fn destructive_cache_reset_admission_rejects_deletion_and_pending_cleanup() {
    assert!(destructive_cache_reset_admission_is_allowed(false, false));
    assert!(!destructive_cache_reset_admission_is_allowed(true, false));
    assert!(!destructive_cache_reset_admission_is_allowed(false, true));
    assert!(!destructive_cache_reset_admission_is_allowed(true, true));
}

fn poi_artifact_progress(
    attempt_id: PoiArtifactCacheAttemptId,
    phase: PoiArtifactCachePhase,
    ready_for_wallet_checks: bool,
) -> PoiArtifactCacheProgress {
    PoiArtifactCacheProgress {
        attempt_id,
        generation: 1,
        chain_id: 1,
        phase,
        completed_lists: usize::from(ready_for_wallet_checks),
        total_lists: 1,
        current_list_key: None,
        current_event_index: None,
        target_event_index: None,
        list_progress: Vec::new(),
        graph: PoiArtifactCacheGraphProgress::default(),
        ready_for_wallet_checks,
        last_error: (phase == PoiArtifactCachePhase::Failed).then(|| "failed".to_string()),
    }
}

fn poi_artifact_attempt_id(value: u64) -> PoiArtifactCacheAttemptId {
    PoiArtifactCacheAttemptId::from_u64(value).expect("nonzero test attempt ID")
}

#[test]
fn chain_load_uses_default_sync_options() {
    let overrides = super::chain_load_overrides();

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

    let loading = ChainUtxoState::Loading { progress: None };
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

#[test]
fn progress_ownership_transfers_from_startup_and_survives_ready_to_syncing() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let registration = lifecycle.prepare_startup(1);
    let startup_is_current = lifecycle.is_current_startup(
        registration.chain_id,
        registration.generation,
        registration.task_id,
    );

    assert!(chain_progress_update_is_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        startup_is_current,
        Some(ChainProgressProjection::Loading),
        &registration.observer_token,
    ));

    lifecycle.finish_startup_after_session_installation(
        1,
        registration.generation,
        registration.task_id,
    );
    assert!(!lifecycle.is_current_startup(1, registration.generation, registration.task_id,));

    for projection in [
        ChainProgressProjection::Ready {
            token: &registration.observer_token,
        },
        ChainProgressProjection::Syncing {
            token: &registration.observer_token,
        },
    ] {
        assert!(chain_progress_update_is_current(
            Some("wallet-1"),
            7,
            "wallet-1",
            7,
            1,
            false,
            Some(projection),
            &registration.observer_token,
        ));
    }

    let replacement = lifecycle.prepare_startup(1);
    lifecycle.finish_startup_after_session_installation(
        1,
        replacement.generation,
        replacement.task_id,
    );
    let replacement_projection = Some(ChainProgressProjection::Syncing {
        token: &replacement.observer_token,
    });
    assert!(!chain_progress_update_is_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        false,
        replacement_projection,
        &registration.observer_token,
    ));
    assert!(chain_progress_update_is_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        false,
        replacement_projection,
        &replacement.observer_token,
    ));
    assert!(!chain_progress_update_is_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        2,
        false,
        replacement_projection,
        &replacement.observer_token,
    ));
}

#[test]
fn replacement_observer_rejects_all_delayed_events_from_previous_observer() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let observer_a = lifecycle.prepare_startup(1);
    let observer_b = lifecycle.prepare_startup(1);
    assert_eq!(observer_a.generation, observer_b.generation);
    assert_ne!(observer_a.observer_token, observer_b.observer_token);
    let installed_observer = observer_b.observer_token.clone();
    lifecycle.finish_startup_after_session_installation(
        observer_b.chain_id,
        observer_b.generation,
        observer_b.task_id,
    );
    assert!(!lifecycle.is_current_startup(
        observer_b.chain_id,
        observer_b.generation,
        observer_b.task_id,
    ));

    let event_names = [
        "snapshot",
        "readiness",
        "sync tip",
        "POI refreshing",
        "POI artifact progress",
    ];
    for event_name in event_names {
        assert!(
            !installed_observer_is_exact_current(
                Some("wallet-1"),
                7,
                "wallet-1",
                7,
                1,
                Some(&installed_observer),
                &observer_a.observer_token,
            ),
            "delayed observer A {event_name} event must be rejected",
        );
        assert!(
            installed_observer_is_exact_current(
                Some("wallet-1"),
                7,
                "wallet-1",
                7,
                1,
                Some(&installed_observer),
                &observer_b.observer_token,
            ),
            "current observer B {event_name} event must be accepted",
        );
    }

    assert!(!installed_observer_is_exact_current(
        Some("wallet-2"),
        7,
        "wallet-1",
        7,
        1,
        Some(&installed_observer),
        &observer_b.observer_token,
    ));
    assert!(!installed_observer_is_exact_current(
        Some("wallet-1"),
        8,
        "wallet-1",
        7,
        1,
        Some(&installed_observer),
        &observer_b.observer_token,
    ));
    assert!(!installed_observer_is_exact_current(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        2,
        Some(&installed_observer),
        &observer_b.observer_token,
    ));
}

#[tokio::test]
async fn replacement_cancels_and_observes_superseded_installed_observer() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let first = lifecycle.prepare_startup(1);
    let mut first_observer = lifecycle.register_installed_observer(&first);

    let second = lifecycle.prepare_startup(1);
    first_observer
        .cancel_rx
        .changed()
        .await
        .expect("first observer cancellation");
    assert!(*first_observer.cancel_rx.borrow());
    let _ = first_observer.completed_tx.send(true);

    let mut second_observer = lifecycle.register_installed_observer(&second);
    let cleanup = lifecycle.supersede_wallet();
    second_observer
        .cancel_rx
        .changed()
        .await
        .expect("second observer cancellation");
    assert!(*second_observer.cancel_rx.borrow());

    let cleanup_join = tokio::spawn(cleanup.shutdown());
    tokio::task::yield_now().await;
    assert!(!cleanup_join.is_finished());
    let _ = second_observer.completed_tx.send(true);
    cleanup_join
        .await
        .expect("cleanup task")
        .expect("cleanup superseded observers");
}

#[tokio::test]
async fn auxiliary_stream_closure_does_not_close_authoritative_observation() {
    let (observation_tx, mut observation_rx) = tokio::sync::watch::channel(0_u64);
    let (auxiliary_tx, auxiliary_rx) = tokio::sync::watch::channel(false);
    let mut auxiliary_rx = Some(auxiliary_rx);
    drop(auxiliary_tx);

    let changed = auxiliary_rx
        .as_mut()
        .expect("auxiliary receiver")
        .changed()
        .await;
    assert!(!retain_auxiliary_stream(&mut auxiliary_rx, &changed));
    assert!(auxiliary_rx.is_none());

    observation_tx.send(1).expect("authoritative observation");
    observation_rx
        .changed()
        .await
        .expect("authoritative stream remains open");
    assert_eq!(*observation_rx.borrow(), 1);
}

#[test]
fn observation_closure_wins_over_ready_projection() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let observer = lifecycle.prepare_startup(1);

    let terminal = installed_observer_terminal_transition(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        Some(InstalledObserverProjection::Ready {
            token: &observer.observer_token,
            start_block: 123,
        }),
        &observer.observer_token,
        Arc::from("wallet session observation stream closed"),
    )
    .expect("observation closure must terminalize the exact ready observer");

    assert_eq!(
        terminal.message.as_ref(),
        "wallet session observation stream closed"
    );
    assert_eq!(terminal.start_block, 123);
}

#[test]
fn observation_closure_wins_over_syncing_projection() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let observer = lifecycle.prepare_startup(1);

    let terminal = installed_observer_terminal_transition(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        Some(InstalledObserverProjection::Syncing {
            token: &observer.observer_token,
            start_block: 456,
        }),
        &observer.observer_token,
        Arc::from("wallet session observation stream closed"),
    )
    .expect("observation closure must terminalize the exact syncing observer");

    assert_eq!(
        terminal.message.as_ref(),
        "wallet session observation stream closed"
    );
    assert_eq!(terminal.start_block, 456);
}

#[test]
fn authoritative_observation_closure_terminalizes_exact_installed_observer() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let observer = lifecycle.prepare_startup(1);

    let terminal = installed_observer_terminal_transition(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        Some(InstalledObserverProjection::Ready {
            token: &observer.observer_token,
            start_block: 789,
        }),
        &observer.observer_token,
        Arc::from("wallet session observation stream closed"),
    )
    .expect("observation closure must terminalize the exact observer");

    assert_eq!(
        terminal.message.as_ref(),
        "wallet session observation stream closed"
    );
    assert_eq!(terminal.start_block, 789);
}

#[test]
fn stale_stream_closure_does_not_replace_newer_observer() {
    let mut lifecycle = WalletSyncLifecycle::new();
    let stale = lifecycle.prepare_startup(1);
    let current = lifecycle.prepare_startup(1);

    let terminal = installed_observer_terminal_transition(
        Some("wallet-1"),
        7,
        "wallet-1",
        7,
        1,
        Some(InstalledObserverProjection::Ready {
            token: &current.observer_token,
            start_block: 999,
        }),
        &stale.observer_token,
        Arc::from("wallet session observation stream closed"),
    );

    assert_eq!(terminal, None);
}

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

#[test]
fn repair_cache_block_parses_zero_as_deployment() {
    assert_eq!(parse_repair_cache_block("0"), Ok(None));
    assert_eq!(parse_repair_cache_block(""), Ok(None));
    assert_eq!(parse_repair_cache_block(" 24936249 "), Ok(Some(24936249)));
    assert!(parse_repair_cache_block("nope").is_err());
}

#[test]
fn repair_cache_help_text_only_mentions_hint_when_available() {
    assert!(repair_cache_help_text(true).contains("wallet start block below"));
    assert!(!repair_cache_help_text(false).contains("wallet start block below"));
    assert!(repair_cache_help_text(false).contains("deployment block"));
}

#[test]
fn chain_error_state_preserves_start_block_hint() {
    let ppoi_workflow_status = WalletPpoiWorkflowStatus {
        awaiting_validation: 2,
        validation_revision: 3,
        ..WalletPpoiWorkflowStatus::default()
    };
    let state = ChainUtxoState::Error {
        message: Arc::from("sync failed"),
        start_block: Some(24936250),
        ppoi_workflow_status,
    };

    assert_eq!(state.start_block(), Some(24936250));
    assert_eq!(state.ppoi_workflow_status(), ppoi_workflow_status);
    assert!(!state.renders_table());
}

#[test]
fn loading_summary_uses_sync_stage_and_percent() {
    let commitments =
        SyncProgressUpdate::new(SyncProgressStage::SynchronizingCommitments, 100, 150, 300);
    let preparing =
        SyncProgressUpdate::artifact_chunk(SyncProgressStage::PreparingUtxoIndex, 25, 100, 3, 12);
    let indexing = SyncProgressUpdate::new(SyncProgressStage::IndexingUtxos, 100, 150, 300);

    assert_eq!(
        loading_summary(Some(commitments)),
        "Synchronizing commitments · 25%"
    );
    assert_eq!(
        loading_summary(Some(preparing)),
        "Preparing UTXO index · 25%"
    );
    assert_eq!(loading_summary(Some(indexing)), "Indexing UTXOs · 25%");
    assert_eq!(loading_summary(None), "Preparing wallet sync...");
}

#[test]
fn wallet_sync_labels_use_deployment_baseline_for_percentage() {
    let progress = SyncProgressUpdate::new(
        SyncProgressStage::IndexingUtxos,
        14_737_691,
        25_305_894,
        25_537_418,
    );

    let labels = sync_status_labels(SyncStatusContext::Syncing, Some(progress));

    assert_eq!(labels.percent, 97);
    assert_eq!(labels.detail, "Block 25305894 of 25537418");
}

#[test]
fn sync_status_labels_describe_no_progress_context() {
    assert_eq!(
        sync_status_labels(SyncStatusContext::Loading, None),
        SyncStatusLabels {
            title: "Preparing wallet sync".to_string(),
            percent: 0,
            detail: "Connecting to chain and loading local wallet state...".to_string(),
        }
    );
    assert_eq!(
        sync_status_labels(SyncStatusContext::Syncing, None),
        SyncStatusLabels {
            title: "Checking wallet sync".to_string(),
            percent: 0,
            detail: "Checking for new wallet events...".to_string(),
        }
    );
}

#[test]
fn sync_status_labels_use_progress_when_available() {
    let progress = SyncProgressUpdate::new(SyncProgressStage::IndexingUtxos, 100, 150, 300);

    assert_eq!(
        sync_status_labels(SyncStatusContext::Loading, Some(progress)),
        SyncStatusLabels {
            title: "Indexing UTXOs".to_string(),
            percent: 25,
            detail: "Block 150 of 300".to_string(),
        }
    );
}

#[test]
fn syncing_status_discards_completed_chain_commitment_progress() {
    let progress =
        SyncProgressUpdate::new(SyncProgressStage::SynchronizingCommitments, 100, 150, 300);

    assert_eq!(
        sync_status_labels(SyncStatusContext::Syncing, Some(progress)),
        SyncStatusLabels {
            title: "Checking wallet sync".to_string(),
            percent: 0,
            detail: "Checking for new wallet events...".to_string(),
        }
    );
}

#[test]
fn syncing_status_retains_wallet_indexing_progress() {
    let progress = SyncProgressUpdate::new(SyncProgressStage::IndexingUtxos, 100, 150, 300);

    assert_eq!(
        sync_status_labels(SyncStatusContext::Syncing, Some(progress)),
        SyncStatusLabels {
            title: "Indexing UTXOs".to_string(),
            percent: 25,
            detail: "Block 150 of 300".to_string(),
        }
    );
}

#[test]
fn loading_chain_state_keeps_utxo_table_available() {
    let state = ChainUtxoState::Loading { progress: None };

    assert!(state.renders_table());
    assert!(state.is_syncing());
    assert!(!matches!(state, ChainUtxoState::Ready { .. }));
    assert!(state.snapshot().is_none());
}

#[test]
fn progress_detail_clamps_current_block() {
    let progress = SyncProgressUpdate::new(SyncProgressStage::IndexingUtxos, 100, 400, 300);

    assert_eq!(progress_detail(progress), "Block 300 of 300");
}

#[test]
fn progress_detail_uses_artifact_chunks_for_utxo_prep() {
    let progress =
        SyncProgressUpdate::artifact_chunk(SyncProgressStage::PreparingUtxoIndex, 58, 100, 7, 12);

    assert_eq!(progress_detail(progress), "Artifact chunks ready: 7 of 12");
}

#[test]
fn progress_detail_identifies_final_artifact_chunk_fetch() {
    let progress =
        SyncProgressUpdate::artifact_chunk(SyncProgressStage::PreparingUtxoIndex, 85, 100, 10, 11);

    assert_eq!(
        progress_detail(progress),
        "Fetching final artifact chunk (10 of 11 ready)..."
    );
}

#[test]
fn progress_detail_marks_all_artifact_chunks_ready() {
    let progress =
        SyncProgressUpdate::artifact_chunk(SyncProgressStage::PreparingUtxoIndex, 90, 100, 11, 11);

    assert_eq!(progress_detail(progress), "Artifact chunks ready");
}

#[test]
fn progress_detail_describes_pending_artifact_chunks() {
    let progress =
        SyncProgressUpdate::artifact_chunk(SyncProgressStage::PreparingUtxoIndex, 25, 100, 0, 11);

    assert_eq!(
        progress_detail(progress),
        "Downloading 11 artifact chunks..."
    );
}

#[test]
fn progress_detail_describes_artifact_metadata() {
    let progress =
        SyncProgressUpdate::artifact_preparation(SyncProgressStage::PreparingUtxoIndex, 5, 100);

    assert_eq!(progress_detail(progress), "Preparing artifact metadata...");
}

#[test]
fn progress_detail_describes_artifact_apply_completion() {
    let progress =
        SyncProgressUpdate::artifact_applied(SyncProgressStage::SynchronizingCommitments);

    assert_eq!(progress_detail(progress), "Commitment artifacts applied");
}

#[test]
fn progress_detail_describes_commitment_tail() {
    let progress = SyncProgressUpdate::commitment_tail(200, 225, 300);

    assert_eq!(
        progress_detail(progress),
        "Checking commitment tail: block 225 of 300"
    );
}

#[test]
fn wallet_status_presence_classifies_sync_and_ppoi_health() {
    let ppoi_attention = WalletStatusCounts {
        recoverable_poi_outputs: 2,
        ..WalletStatusCounts::default()
    };
    let blocked_shield_attention = WalletStatusCounts {
        blocked_shield_outputs: 1,
        ..WalletStatusCounts::default()
    };
    let automatic_recovery = WalletStatusCounts {
        recoverable_poi_outputs: 1,
        ppoi_workflow_status: WalletPpoiWorkflowStatus {
            awaiting_recovery: 2,
            ..WalletPpoiWorkflowStatus::default()
        },
        ..WalletStatusCounts::default()
    };

    assert_eq!(
        ppoi_presence_status(true, true, false, None, WalletStatusCounts::default()),
        PresenceStatus::Active
    );
    assert_eq!(
        ppoi_presence_status(false, true, false, None, WalletStatusCounts::default()),
        PresenceStatus::Healthy
    );
    assert_eq!(
        ppoi_presence_status(false, false, false, None, WalletStatusCounts::default()),
        PresenceStatus::Unknown
    );
    assert_eq!(
        ppoi_presence_status(false, true, true, None, WalletStatusCounts::default()),
        PresenceStatus::Unknown
    );

    let attempt_id = poi_artifact_attempt_id(1);
    let active_cache =
        poi_artifact_progress(attempt_id, PoiArtifactCachePhase::ReplayingRanges, false);
    let ready_cache = poi_artifact_progress(attempt_id, PoiArtifactCachePhase::Ready, true);
    let usable_error = poi_artifact_progress(attempt_id, PoiArtifactCachePhase::Failed, true);
    let blocking_error = poi_artifact_progress(attempt_id, PoiArtifactCachePhase::Failed, false);
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&active_cache),
            WalletStatusCounts::default()
        ),
        PresenceStatus::Active
    );
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&ready_cache),
            WalletStatusCounts::default()
        ),
        PresenceStatus::Healthy
    );
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&usable_error),
            WalletStatusCounts::default()
        ),
        PresenceStatus::Active
    );
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&blocking_error),
            WalletStatusCounts {
                pending_poi_assets: 1,
                ..WalletStatusCounts::default()
            }
        ),
        PresenceStatus::Error
    );
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&blocking_error),
            WalletStatusCounts {
                blocked_shield_outputs: 1,
                ..WalletStatusCounts::default()
            }
        ),
        PresenceStatus::Active
    );
    assert_eq!(ppoi_attention.ppoi_attention_count(), 2);
    assert_eq!(blocked_shield_attention.ppoi_attention_count(), 1);
    assert_eq!(automatic_recovery.ppoi_attention_count(), 1);
    assert_eq!(automatic_recovery.ppoi_status_count(), 3);
    assert_eq!(
        ppoi_presence_status(false, true, false, None, automatic_recovery),
        PresenceStatus::Active
    );
    let distinct_attention = WalletStatusCounts {
        recoverable_poi_outputs: 1,
        ppoi_workflow_status: WalletPpoiWorkflowStatus {
            needs_attention: 1,
            ..WalletPpoiWorkflowStatus::default()
        },
        ..WalletStatusCounts::default()
    };
    assert_eq!(distinct_attention.ppoi_attention_count(), 2);
    assert_eq!(distinct_attention.ppoi_status_count(), 2);
    let candidate_attention = WalletStatusCounts {
        ppoi_workflow_status: WalletPpoiWorkflowStatus {
            awaiting_recovery: 1,
            recovery_needs_attention: 1,
            ..WalletPpoiWorkflowStatus::default()
        },
        ..WalletStatusCounts::default()
    };
    assert_eq!(candidate_attention.ppoi_attention_count(), 1);
    assert_eq!(
        ppoi_presence_status(false, true, false, None, candidate_attention),
        PresenceStatus::Error
    );
}

#[test]
fn ppoi_retry_completion_rejects_stale_wallet_or_session() {
    assert!(ppoi_retry_completion_is_current(7, 7, true));
    assert!(!ppoi_retry_completion_is_current(8, 7, true));
    assert!(!ppoi_retry_completion_is_current(7, 7, false));
}

#[test]
fn cold_ppoi_download_reports_chunk_and_byte_progress_without_readiness() {
    let mut progress = poi_artifact_progress(
        poi_artifact_attempt_id(2),
        PoiArtifactCachePhase::DownloadingChunks,
        false,
    );
    progress.graph = PoiArtifactCacheGraphProgress {
        verified_chunks: 3,
        total_chunks: 10,
        verified_encoded_bytes: 12 * 1024 * 1024,
        total_authenticated_encoded_bytes: Some(31 * 1024 * 1024),
        ..PoiArtifactCacheGraphProgress::default()
    };

    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, Some(&progress), false),
        "Downloading POI chunks"
    );
    assert_eq!(
        ppoi_chunk_progress_label(&progress).as_deref(),
        Some("3/10 chunks · 12 MiB/31 MiB")
    );
    assert!(!progress.ready_for_wallet_checks);
}

#[test]
fn warm_ppoi_refresh_keeps_local_corpus_readiness_visible() {
    let progress = poi_artifact_progress(
        poi_artifact_attempt_id(3),
        PoiArtifactCachePhase::Planning,
        true,
    );

    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, Some(&progress), false),
        "Refreshing PPOI data"
    );
    assert_eq!(
        ppoi_hover_detail(PresenceStatus::Active, Some(&progress), false),
        Some(
            "Refreshing in the background while wallet checks use verified PPOI data saved on this device."
        )
    );
}

#[test]
fn retained_bridge_replay_reports_authenticated_event_range() {
    let mut progress = poi_artifact_progress(
        poi_artifact_attempt_id(4),
        PoiArtifactCachePhase::ReplayingRanges,
        true,
    );
    progress.graph = PoiArtifactCacheGraphProgress {
        replay_start_event_index: Some(32_768),
        replay_end_event_index: Some(33_023),
        replayed_event_count: 128,
        total_replay_event_count: 256,
        ..PoiArtifactCacheGraphProgress::default()
    };

    assert_eq!(
        ppoi_replay_progress_label(&progress).as_deref(),
        Some("Events 32768-33023 · 128/256 replayed")
    );
    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, Some(&progress), false),
        "Refreshing PPOI data"
    );
}

#[test]
fn failed_ppoi_refresh_keeps_serving_corpus_available() {
    let mut progress = poi_artifact_progress(
        poi_artifact_attempt_id(5),
        PoiArtifactCachePhase::Failed,
        true,
    );
    progress.last_error = Some("catalog unavailable".to_string());

    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&progress),
            WalletStatusCounts::default()
        ),
        PresenceStatus::Active
    );
    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, Some(&progress), false),
        "Artifact cache refresh failed"
    );
    assert_eq!(
        ppoi_hover_detail(PresenceStatus::Active, Some(&progress), false),
        Some(
            "Refresh failed; wallet checks continue using verified PPOI data saved on this device."
        )
    );
}

#[test]
fn official_ppoi_artifact_runtime_has_no_legacy_or_proxy_fallback() {
    let settings = wallet_ops::settings::WalletSettings::default();
    let wallet_ops::PoiReadSource::IndexedArtifacts {
        artifact_source,
        wallet_read_fallback,
        ..
    } = settings
        .poi_read_source()
        .expect("official PPOI artifact runtime")
    else {
        panic!("official PPOI artifact runtime should use indexed artifacts");
    };

    assert!(matches!(
        artifact_source.manifest_source,
        wallet_ops::PoiArtifactManifestSource::IpnsName(ref name)
            if name == wallet_ops::settings::OFFICIAL_POI_ARTIFACT_IPNS_NAME
                && name != wallet_ops::settings::LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME
    ));
    assert_eq!(wallet_read_fallback, wallet_ops::PoiProxyFallback::Disabled);
}

#[test]
fn ppoi_retry_pending_admission_owns_chain() {
    let mut attempts = PoiArtifactCacheRetryAttempts::default();
    let request_token = attempts.begin(1).expect("start retry request");

    assert!(attempts.contains(1));
    assert!(attempts.begin(1).is_none());
    assert!(attempts.cancel_pending(1, request_token));
    assert!(!attempts.contains(1));
}

#[test]
fn ppoi_retry_admission_binds_only_matching_provisional_request() {
    let mut attempts = PoiArtifactCacheRetryAttempts::default();
    let stale_request = attempts.begin(1).expect("start stale retry request");
    attempts.clear();
    let replacement_request = attempts.begin(1).expect("start replacement retry request");
    let stale_attempt_id = poi_artifact_attempt_id(10);
    let replacement_attempt_id = poi_artifact_attempt_id(11);

    assert_ne!(stale_request, replacement_request);
    assert!(!attempts.bind(1, stale_request, stale_attempt_id));
    assert!(attempts.contains(1));
    assert!(attempts.bind(1, replacement_request, replacement_attempt_id));
    assert!(!attempts.cancel_pending(1, replacement_request));
}

#[test]
fn ppoi_retry_exact_core_completion_releases_ownership() {
    let mut attempts = PoiArtifactCacheRetryAttempts::default();
    let request_token = attempts.begin(1).expect("start retry request");
    let attempt_id = poi_artifact_attempt_id(20);
    assert!(attempts.bind(1, request_token, attempt_id));

    assert!(attempts.finish(1, attempt_id));
    assert!(!attempts.contains(1));
}

#[test]
fn ppoi_retry_stale_completion_does_not_clear_replacement() {
    let mut attempts = PoiArtifactCacheRetryAttempts::default();
    let stale_request = attempts.begin(1).expect("start stale retry request");
    let stale_attempt_id = poi_artifact_attempt_id(30);
    assert!(attempts.bind(1, stale_request, stale_attempt_id));
    attempts.clear();
    let replacement_request = attempts.begin(1).expect("start replacement retry request");
    let replacement_attempt_id = poi_artifact_attempt_id(31);
    assert!(attempts.bind(1, replacement_request, replacement_attempt_id));

    assert!(!attempts.finish(1, stale_attempt_id));
    assert!(attempts.contains(1));
    assert!(attempts.finish(1, replacement_attempt_id));
}

#[test]
fn balance_sync_presence_degrades_for_stalled_or_lagging_heads() {
    let now = 1_000;
    let ethereum_block_time = Duration::from_secs(12);
    let fresh = WalletSyncTip {
        last_scanned_block: Some(990),
        head_block: Some(1_012),
        safe_head_block: Some(1_000),
        head_last_advanced_at_unix_secs: Some(now - 30),
        indexed_catch_up: None,
    };

    assert_eq!(
        balance_stale_timeout(ethereum_block_time),
        Duration::from_mins(2)
    );
    assert_eq!(balance_lag_threshold_blocks(ethereum_block_time), 10);
    assert_eq!(
        balance_stale_timeout(Duration::from_millis(450)),
        Duration::from_secs(45)
    );
    assert_eq!(
        balance_lag_threshold_blocks(Duration::from_millis(450)),
        100
    );
    assert_eq!(balance_lag_threshold_blocks(Duration::from_secs(1)), 45);
    assert_eq!(
        balance_lag_threshold_blocks(Duration::from_millis(250)),
        180
    );
    assert_eq!(
        balance_sync_issue(Some(fresh), ethereum_block_time, now),
        None
    );
    assert_eq!(
        balances_presence_status(false, true, Some(fresh), ethereum_block_time, now),
        PresenceStatus::Healthy
    );

    let stalled = WalletSyncTip {
        head_last_advanced_at_unix_secs: Some(now - 121),
        ..fresh
    };
    assert_eq!(
        balance_sync_issue(Some(stalled), ethereum_block_time, now),
        Some(BalanceSyncIssue::HeadStalled {
            stale_secs: 121,
            threshold_secs: 120,
        })
    );
    assert_eq!(
        balances_presence_status(false, true, Some(stalled), ethereum_block_time, now),
        PresenceStatus::Active
    );

    let lagging = WalletSyncTip {
        last_scanned_block: Some(989),
        ..fresh
    };
    assert_eq!(
        balance_sync_issue(Some(lagging), ethereum_block_time, now),
        Some(BalanceSyncIssue::Lagging {
            lag_blocks: 11,
            threshold_blocks: 10,
        })
    );
    assert_eq!(
        balances_presence_status(false, true, Some(lagging), ethereum_block_time, now),
        PresenceStatus::Active
    );

    let indexed_catch_up = WalletSyncTip {
        indexed_catch_up: Some(WalletIndexedCatchUpStatus {
            source: WalletIndexedCatchUpSource::Squid,
            from_block: 990,
            target_block: 1_000,
        }),
        ..fresh
    };
    assert_eq!(
        balance_sync_issue(Some(indexed_catch_up), ethereum_block_time, now),
        None
    );
    assert_eq!(
        balances_presence_status(
            false,
            true,
            Some(indexed_catch_up),
            ethereum_block_time,
            now,
        ),
        PresenceStatus::Active
    );

    assert_eq!(
        balance_sync_issue(None, ethereum_block_time, now),
        Some(BalanceSyncIssue::HeadUnavailable)
    );
    assert_eq!(
        balances_presence_status(false, true, None, ethereum_block_time, now),
        PresenceStatus::Unknown
    );
    assert_eq!(
        balances_presence_status(true, false, None, ethereum_block_time, now),
        PresenceStatus::Active
    );
}

#[test]
fn balance_sync_issue_detail_suggests_network_remedies() {
    let lagging = BalanceSyncIssue::Lagging {
        lag_blocks: 186,
        threshold_blocks: 45,
    };
    let stalled = BalanceSyncIssue::HeadStalled {
        stale_secs: 60,
        threshold_secs: 45,
    };

    assert_eq!(
        balance_sync_issue_detail(lagging, WalletNetworkMode::Tor),
        "Wallet state is 186 safe-head blocks behind. Consider generating a new Tor session or using premium RPCs."
    );
    assert_eq!(
        balance_sync_issue_detail(lagging, WalletNetworkMode::Direct),
        "Wallet state is 186 safe-head blocks behind. Consider using premium RPCs."
    );
    assert_eq!(
        balance_sync_issue_detail(stalled, WalletNetworkMode::Proxy),
        "RPC head has not advanced for 1m. Consider using premium RPCs."
    );
    assert_eq!(
        balance_sync_issue_detail(BalanceSyncIssue::HeadUnavailable, WalletNetworkMode::Tor),
        "Waiting for chain head updates."
    );
}

#[test]
fn ready_wallet_status_labels_prioritize_actionable_private_attention() {
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts::default()),
        SyncStatusLabels {
            title: "Wallet ready".to_string(),
            percent: 100,
            detail: "Private wallet synced and ready".to_string(),
        }
    );
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts {
            blocked_shield_outputs: 1,
            recoverable_poi_outputs: 3,
            ..WalletStatusCounts::default()
        }),
        SyncStatusLabels {
            title: "Private assets need attention".to_string(),
            percent: 100,
            detail: "1 blocked Shield needs attention".to_string(),
        }
    );
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts {
            recoverable_poi_outputs: 3,
            ..WalletStatusCounts::default()
        }),
        SyncStatusLabels {
            title: "Not yet spendable".to_string(),
            percent: 100,
            detail: "3 PPOI verification retries are available".to_string(),
        }
    );
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts {
            pending_incoming_outputs: 1,
            pending_outgoing_outputs: 2,
            pending_poi_assets: 1,
            ..WalletStatusCounts::default()
        }),
        SyncStatusLabels {
            title: "Not yet spendable".to_string(),
            percent: 100,
            detail:
                "1 incoming transfer · 2 outgoing transfers · 1 asset awaiting PPOI verification"
                    .to_string(),
        }
    );
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts {
            pending_outgoing_outputs: 2,
            ..WalletStatusCounts::default()
        }),
        SyncStatusLabels {
            title: "Waiting for confirmation".to_string(),
            percent: 100,
            detail: "2 outgoing transfers".to_string(),
        }
    );
}

#[test]
fn ready_wallet_status_text_is_hidden_for_all_ready_states() {
    assert!(!ready_wallet_status_shows_text(
        WalletStatusCounts::default()
    ));
    assert!(!ready_wallet_status_shows_text(WalletStatusCounts {
        pending_incoming_outputs: 1,
        ..WalletStatusCounts::default()
    }));
    assert!(!ready_wallet_status_shows_text(WalletStatusCounts {
        recoverable_poi_outputs: 1,
        ..WalletStatusCounts::default()
    }));
    assert!(!ready_wallet_status_shows_text(WalletStatusCounts {
        blocked_shield_outputs: 1,
        ..WalletStatusCounts::default()
    }));
}

#[test]
fn active_ppoi_hover_uses_exact_submission_copy() {
    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, None, true),
        "Submitting PPOIs…"
    );
    assert_eq!(
        ppoi_hover_detail(PresenceStatus::Active, None, true),
        Some("Submitting sender-created contexts and checking owned private-output PPOI status.")
    );
}
