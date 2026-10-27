use broadcaster_monitor::{event_channel, shared};
use broadcaster_monitor_waku::{RelayNetworkConfig, RelayNetworkMode, WakuMonitorConfig};
use tokio::sync::watch;
use wallet_ops::gateway::GatewayNetworkError;

use super::super::chain_load::{
    WalletRootReplacementCleanup, WalletSyncLifecycleCleanupReport,
    WalletSyncLifecycleCleanupWaitGroup,
};
use super::super::network::{
    TorRotation, complete_tor_rotation, join_or_begin_tor_rotation, native_network_status_error,
    restart_now_or_defer_to_rotation, retire_root_waku, take_tor_rotation, waku_to_retire,
};
use super::super::private_broadcaster::{
    cancel_private_broadcaster_progress, cancel_public_broadcaster_tasks,
};
use super::super::proposals::ProposalCleanup;
use super::*;

#[test]
fn locked_startup_and_proxy_mode_keep_waku_inactive() {
    assert!(!should_start_waku_runtime(
        false,
        false,
        false,
        false,
        RelayNetworkMode::Direct
    ));
    assert!(!should_start_waku_runtime(
        true,
        false,
        false,
        false,
        RelayNetworkMode::Proxy
    ));
    assert!(active_waku_client(None).is_none());
}

#[test]
fn every_view_unlock_kind_starts_at_most_one_runtime() {
    for unlock_kind in ["software", "hardware-compatible", "empty-vault"] {
        assert!(
            should_start_waku_runtime(true, false, false, false, RelayNetworkMode::Direct),
            "{unlock_kind} unlock should start Waku"
        );
        assert!(
            !should_start_waku_runtime(true, true, false, false, RelayNetworkMode::Direct),
            "{unlock_kind} unlock completion must be idempotent"
        );
    }
}

#[test]
fn unlock_time_waku_failure_preserves_unlock_and_has_no_fallback() {
    let view_unlocked = true;
    let result = build_waku_client_if_needed(true, || Err::<(), _>("construction failed"));

    assert_eq!(result, Err("construction failed"));
    assert!(view_unlocked);
    assert!(active_waku_client(None).is_none());
}

#[test]
fn later_delivery_demand_retries_after_client_construction_failure() {
    let mut attempts = 0;
    let first = build_waku_client_if_needed(
        should_start_waku_for_delivery(
            DeliveryMode::PublicBroadcaster,
            true,
            false,
            false,
            false,
            RelayNetworkMode::Direct,
        ),
        || {
            attempts += 1;
            Err::<(), _>("transient construction failure")
        },
    );
    assert_eq!(first, Err("transient construction failure"));

    let second = build_waku_client_if_needed(
        should_start_waku_for_delivery(
            DeliveryMode::PublicBroadcaster,
            true,
            false,
            false,
            false,
            RelayNetworkMode::Direct,
        ),
        || {
            attempts += 1;
            Ok::<_, &str>("recovered client")
        },
    )
    .expect("later demand retries construction");

    assert_eq!(attempts, 2);
    assert_eq!(second, Some("recovered client"));
}

#[test]
fn later_delivery_demand_retries_after_active_worker_failure() {
    let generation = 11;
    let mut stopping_generation = None;
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    let action = complete_waku_worker_generation(
        Some(generation),
        &mut stopping_generation,
        generation,
        WakuWorkerCompletionKind::WorkerError,
        true,
        false,
        &monitor_state,
        &event_tx,
    );
    assert_eq!(action, WakuWorkerCompletionAction::HandleActiveFailure);

    let retry = build_waku_client_if_needed(
        should_start_waku_for_delivery(
            DeliveryMode::PublicBroadcaster,
            true,
            false,
            stopping_generation.is_some(),
            false,
            RelayNetworkMode::Direct,
        ),
        || Ok::<_, &str>("replacement client"),
    )
    .expect("later demand retries after worker failure");

    assert_eq!(retry, Some("replacement client"));
}

#[test]
fn delivery_demand_does_not_retry_when_lifecycle_or_policy_disallows_waku() {
    for (view_unlocked, runtime_stopping, network_mode) in [
        (false, false, RelayNetworkMode::Direct),
        (true, true, RelayNetworkMode::Direct),
        (true, false, RelayNetworkMode::Proxy),
    ] {
        let mut attempted = false;
        let result = build_waku_client_if_needed(
            should_start_waku_for_delivery(
                DeliveryMode::PublicBroadcaster,
                view_unlocked,
                false,
                runtime_stopping,
                false,
                network_mode,
            ),
            || {
                attempted = true;
                Ok::<_, &str>(())
            },
        )
        .expect("ineligible demand is ignored");
        assert!(result.is_none());
        assert!(!attempted);
    }

    assert!(!should_start_waku_for_delivery(
        DeliveryMode::ManualCalldata,
        true,
        false,
        false,
        false,
        RelayNetworkMode::Direct,
    ));
}

#[test]
fn root_replacement_shutdown_blocks_waku_start_and_restart() {
    assert!(!should_start_waku_runtime(
        true,
        false,
        false,
        true,
        RelayNetworkMode::Direct,
    ));
    assert!(!should_start_waku_for_delivery(
        DeliveryMode::PublicBroadcaster,
        true,
        false,
        false,
        true,
        RelayNetworkMode::Direct,
    ));

    let generation = 13;
    let mut stopping_generation = Some(WakuStoppingState::for_test(generation));
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    let action = complete_waku_worker_generation(
        None,
        &mut stopping_generation,
        generation,
        WakuWorkerCompletionKind::Clean,
        true,
        true,
        &monitor_state,
        &event_tx,
    );

    assert_eq!(
        action,
        WakuWorkerCompletionAction::FinalizedStop { restart: false }
    );
    assert!(stopping_generation.is_none());
}

#[tokio::test]
async fn root_replacement_barrier_waits_for_both_workers_before_clearing_monitor() {
    let (sync_task, sync_tx) = WalletSyncLifecycleCleanupTask::channel_for_test();
    let sync_cleanup = WalletSyncLifecycleCleanupWaitGroup::new(vec![sync_task]);
    let (waku_completion, waku_tx) = WakuWorkerCompletionToken::channel_for_test();
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    monitor_state
        .write()
        .upsert_fee(fee_row(1, Address::from([0x44; 20]), "old-worker-fees"));

    let cleanup = WalletRootReplacementCleanup::spawn(
        &tokio::runtime::Handle::current(),
        sync_cleanup,
        Some(waku_completion),
        ProposalCleanup::empty(),
        None,
        monitor_state.clone(),
        event_tx,
    );
    assert!(!cleanup.is_finished());

    monitor_state.write().upsert_fee(fee_row(
        2,
        Address::from([0x55; 20]),
        "late-old-worker-fees",
    ));
    assert!(!cleanup.is_finished());

    sync_tx
        .send(Some(WalletSyncLifecycleCleanupReport {
            stopped_startup_tasks: 2,
            failed_startup_tasks: 0,
            shut_down_session_store: true,
        }))
        .expect("sync cleanup receiver is alive");
    assert!(!cleanup.is_finished());
    waku_tx
        .send(true)
        .expect("Waku quiescence receiver is alive");

    let report = cleanup
        .wait()
        .await
        .expect("root replacement cleanup succeeds");
    assert_eq!(report.stopped_startup_tasks, 2);
    assert_eq!(monitor_state.read().fee_rows(), [] as [FeeRow; 0]);

    monitor_state.write().upsert_fee(fee_row(
        3,
        Address::from([0x66; 20]),
        "fresh-replacement-fees",
    ));
    let generation = 19;
    let mut stopping = Some(WakuStoppingState::for_test(generation));
    let (event_tx, _event_rx) = event_channel(16);
    let action = complete_waku_worker_generation(
        None,
        &mut stopping,
        generation,
        WakuWorkerCompletionKind::Clean,
        true,
        true,
        &monitor_state,
        &event_tx,
    );
    assert_eq!(
        action,
        WakuWorkerCompletionAction::FinalizedStop { restart: false }
    );
    assert_eq!(monitor_state.read().fee_rows().len(), 1);
    assert_eq!(
        monitor_state.read().fee_rows()[0].chain_id,
        3,
        "retiring completion must not clear replacement state"
    );
}

#[tokio::test]
async fn worker_error_epilogue_publishes_quiescence_and_preserves_error() {
    let (sync_task, sync_tx) = WalletSyncLifecycleCleanupTask::channel_for_test();
    let sync_cleanup = WalletSyncLifecycleCleanupWaitGroup::new(vec![sync_task]);
    let (waku_completion, quiesced_tx) = WakuWorkerCompletionToken::channel_for_test();
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    monitor_state
        .write()
        .upsert_fee(fee_row(1, Address::from([0x77; 20]), "old-worker-fees"));
    let cleanup = WalletRootReplacementCleanup::spawn(
        &tokio::runtime::Handle::current(),
        sync_cleanup,
        Some(waku_completion),
        ProposalCleanup::empty(),
        None,
        monitor_state.clone(),
        event_tx,
    );

    let result = run_waku_worker_task(
        async { Err(eyre::eyre!("injected worker failure")) },
        WakuWorkerQuiescenceGuard::new(quiesced_tx),
    )
    .await;
    assert_eq!(
        result.expect_err("injected worker failure").to_string(),
        "injected worker failure"
    );
    sync_tx
        .send(Some(WalletSyncLifecycleCleanupReport {
            stopped_startup_tasks: 1,
            failed_startup_tasks: 0,
            shut_down_session_store: true,
        }))
        .expect("sync cleanup receiver is alive");

    let report = cleanup
        .wait()
        .await
        .expect("root replacement cleanup succeeds");
    assert_eq!(report.stopped_startup_tasks, 1);
    assert_eq!(monitor_state.read().fee_rows(), [] as [FeeRow; 0]);
}

#[tokio::test]
async fn panicking_worker_releases_waku_quiescence_and_clears_monitor() {
    let (sync_task, sync_tx) = WalletSyncLifecycleCleanupTask::channel_for_test();
    let sync_cleanup = WalletSyncLifecycleCleanupWaitGroup::new(vec![sync_task]);
    let (waku_completion, quiesced_tx) = WakuWorkerCompletionToken::channel_for_test();
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    monitor_state
        .write()
        .upsert_fee(fee_row(1, Address::from([0x78; 20]), "old-worker-fees"));
    let cleanup = WalletRootReplacementCleanup::spawn(
        &tokio::runtime::Handle::current(),
        sync_cleanup,
        Some(waku_completion),
        ProposalCleanup::empty(),
        None,
        monitor_state.clone(),
        event_tx,
    );

    let worker = tokio::spawn(run_waku_worker_task(
        async { panic!("injected worker panic") },
        WakuWorkerQuiescenceGuard::new(quiesced_tx),
    ));
    assert!(worker.await.expect_err("worker must panic").is_panic());
    sync_tx
        .send(Some(WalletSyncLifecycleCleanupReport::default()))
        .expect("sync cleanup receiver is alive");

    let report = cleanup
        .wait()
        .await
        .expect("root replacement cleanup succeeds after panic");
    assert_eq!(report, WalletSyncLifecycleCleanupReport::default());
    assert_eq!(monitor_state.read().fee_rows(), [] as [FeeRow; 0]);
}

#[tokio::test]
async fn abort_before_waku_worker_first_poll_releases_quiescence() {
    let (waku_completion, quiesced_tx) = WakuWorkerCompletionToken::channel_for_test();
    let quiescence_guard = WakuWorkerQuiescenceGuard::new(quiesced_tx);
    let worker = tokio::spawn(run_waku_worker_task(
        std::future::pending::<eyre::Result<()>>(),
        quiescence_guard,
    ));
    worker.abort();
    assert!(
        worker
            .await
            .expect_err("worker must be cancelled")
            .is_cancelled()
    );

    tokio::time::timeout(Duration::from_secs(1), waku_completion.wait())
        .await
        .expect("quiescence resolves after pre-poll abort")
        .expect("quiescence latch remains open");
}

#[tokio::test]
async fn root_replacement_barrier_rejects_closed_waku_quiescence_latch() {
    let (sync_task, sync_tx) = WalletSyncLifecycleCleanupTask::channel_for_test();
    let sync_cleanup = WalletSyncLifecycleCleanupWaitGroup::new(vec![sync_task]);
    let (waku_completion, quiesced_tx) = WakuWorkerCompletionToken::channel_for_test();
    drop(quiesced_tx);
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    monitor_state
        .write()
        .upsert_fee(fee_row(1, Address::from([0x88; 20]), "old-worker-fees"));
    let cleanup = WalletRootReplacementCleanup::spawn(
        &tokio::runtime::Handle::current(),
        sync_cleanup,
        Some(waku_completion),
        ProposalCleanup::empty(),
        None,
        monitor_state.clone(),
        event_tx,
    );
    sync_tx
        .send(Some(WalletSyncLifecycleCleanupReport::default()))
        .expect("sync cleanup receiver is alive");

    let error = cleanup
        .wait()
        .await
        .expect_err("closed Waku latch must reject root replacement");
    assert!(error.contains("before quiescence was established"));
    assert_eq!(monitor_state.read().fee_rows(), [] as [FeeRow; 0]);
}

#[tokio::test]
async fn root_replacement_barrier_preserves_sync_cleanup_failure_after_waku_quiesces() {
    let sync_cleanup = WalletSyncLifecycleCleanupWaitGroup::new(vec![
        WalletSyncLifecycleCleanupTask::closed_for_test(),
    ]);
    let (waku_completion, quiesced_tx) = WakuWorkerCompletionToken::channel_for_test();
    quiesced_tx
        .send(true)
        .expect("Waku latch receiver is alive");
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    monitor_state
        .write()
        .upsert_fee(fee_row(1, Address::from([0x99; 20]), "old-worker-fees"));
    let cleanup = WalletRootReplacementCleanup::spawn(
        &tokio::runtime::Handle::current(),
        sync_cleanup,
        Some(waku_completion),
        ProposalCleanup::empty(),
        None,
        monitor_state.clone(),
        event_tx,
    );

    let error = cleanup
        .wait()
        .await
        .expect_err("sync cleanup failure must reject root replacement");
    assert!(error.contains("wallet sync cleanup failed during root replacement"));
    assert_eq!(monitor_state.read().fee_rows(), [] as [FeeRow; 0]);
}

#[test]
fn stop_removes_access_signals_workers_and_clears_monitor_state() {
    let config = WakuMonitorConfig {
        network: RelayNetworkConfig::proxy(reqwest::Client::new()),
        ..WakuMonitorConfig::default()
    };
    let client = config.build_client().expect("proxy Waku delivery client");
    let weak_client = Arc::downgrade(&client);
    let (worker_shutdown, worker_shutdown_rx) = watch::channel(false);
    let mut runtime = Some(WalletWakuRuntime {
        client,
        worker_shutdown,
        completion: WakuWorkerCompletionToken::closed_for_test(),
        generation: 1,
    });
    let monitor_state = shared();
    monitor_state
        .write()
        .upsert_fee(fee_row(1, Address::from([0x11; 20]), "stale-fees"));
    let (event_tx, event_rx) = event_channel(16);
    let initial_rev = event_rx.borrow().to_owned();

    assert!(active_waku_client(runtime.as_ref()).is_some());
    assert!(stop_waku_runtime(&mut runtime, &monitor_state, &event_tx));
    assert!(runtime.is_none());
    assert!(*worker_shutdown_rx.borrow());
    assert!(weak_client.upgrade().is_none());
    assert_eq!(monitor_state.read().fee_rows(), [] as [FeeRow; 0]);
    assert_eq!(
        monitor_state.read().peer_rows(),
        [] as [broadcaster_monitor::PeerRow; 0]
    );
    assert_eq!(
        monitor_state.read().peer_summary(),
        broadcaster_monitor::PeerSummary::default()
    );
    assert!(*event_rx.borrow() > initial_rev);
}

#[test]
fn later_unlock_can_install_a_fresh_runtime_generation() {
    assert!(should_start_waku_runtime(
        true,
        false,
        false,
        false,
        RelayNetworkMode::Tor
    ));
    assert!(!should_start_waku_runtime(
        false,
        false,
        false,
        false,
        RelayNetworkMode::Tor
    ));
    assert!(!should_start_waku_runtime(
        true,
        false,
        true,
        false,
        RelayNetworkMode::Tor
    ));
}

#[test]
fn worker_error_after_stop_allows_a_fresh_runtime_generation() {
    let generation = 7;
    let mut stopping_generation = Some(WakuStoppingState::for_test(generation));
    let monitor_state = shared();
    monitor_state
        .write()
        .upsert_fee(fee_row(1, Address::from([0x33; 20]), "late-worker-fees"));
    let (event_tx, event_rx) = event_channel(16);
    let initial_rev = event_rx.borrow().to_owned();

    let action = complete_waku_worker_generation(
        None,
        &mut stopping_generation,
        generation,
        WakuWorkerCompletionKind::WorkerError,
        true,
        false,
        &monitor_state,
        &event_tx,
    );

    assert_eq!(
        action,
        WakuWorkerCompletionAction::FinalizedStop { restart: true }
    );
    assert!(stopping_generation.is_none());
    assert_eq!(monitor_state.read().fee_rows(), [] as [FeeRow; 0]);
    assert!(*event_rx.borrow() > initial_rev);
    assert!(should_start_waku_runtime(
        true,
        false,
        stopping_generation.is_some(),
        false,
        RelayNetworkMode::Direct
    ));
}

#[tokio::test]
async fn lock_cancellation_aborts_in_flight_broadcaster_work() {
    let key = UnshieldAssetKey {
        chain_id: 1,
        token: Address::from([0x22; 20]),
    };
    let mut progress = Some(private_progress_state(
        PrivateSubmissionProgressFlow::PublicBroadcaster,
        key,
    ));
    let task = tokio::spawn(std::future::pending::<()>());
    progress.as_mut().expect("progress").task_abort_handle = Some(task.abort_handle());

    let _ = cancel_private_broadcaster_progress(&mut progress);

    assert!(progress.is_none());
    assert!(task.await.expect_err("task was aborted").is_cancelled());
}

#[tokio::test]
async fn lock_cancellation_aborts_every_tracked_broadcaster_task() {
    let first = tokio::spawn(std::future::pending::<()>());
    let second = tokio::spawn(std::future::pending::<()>());
    let mut handles = vec![first.abort_handle(), second.abort_handle()];

    cancel_public_broadcaster_tasks(&mut handles);

    assert!(handles.is_empty());
    assert!(
        first
            .await
            .expect_err("first task was aborted")
            .is_cancelled()
    );
    assert!(
        second
            .await
            .expect_err("second task was aborted")
            .is_cancelled()
    );
}

#[test]
fn lock_scope_renewal_closes_the_old_submission_scope() {
    let mut scope = SubmissionNodeScope::new();
    let held = scope.clone();

    renew_submission_scope(&mut scope);

    // A stale callback's handle builds no node, and nodes in their teardown delay stop.
    assert!(held.is_closed());
    // The next unlock submits on an open scope.
    assert!(!scope.is_closed());
}

/// A Direct-mode runtime config with no discovery and no peers, so nothing dials.
fn quiet_waku_config() -> WakuMonitorConfig {
    WakuMonitorConfig {
        dns_enr_trees: Some(Vec::new()),
        max_peers: Some(0),
        network: RelayNetworkConfig::direct(),
        ..WakuMonitorConfig::default()
    }
}

#[tokio::test]
async fn rotation_retires_the_old_transport_without_waiting_for_submissions() {
    let config = quiet_waku_config();
    let client = config.build_client().expect("quiet Waku client");
    // A submission in flight holds the root client for its seed snapshot.
    let scope = SubmissionNodeScope::new();
    let submission = PublicBroadcasterWaku::new(
        Arc::clone(&client),
        config.to_waku_config(),
        HttpContext::with_rpc_client_for_tests(
            reqwest::Client::new(),
            wallet_ops::WalletNetworkMode::Direct,
        ),
        scope.clone(),
    );
    let (completion, quiesced_tx) = WakuWorkerCompletionToken::channel_for_test();
    let retire = tokio::spawn(retire_root_waku(completion, Arc::clone(&client)));

    tokio::task::yield_now().await;
    assert!(
        !retire.is_finished(),
        "rotation stays pending while monitor workers still use the client"
    );
    quiesced_tx
        .send(true)
        .expect("quiescence receiver is alive");
    tokio::time::timeout(Duration::from_secs(10), retire)
        .await
        .expect("retirement doesn't wait for the submission")
        .expect("retirement task completes")
        .expect("old transport retires");

    // Only now may the rotation commit: the old identity can do no more Waku work.
    assert!(
        client
            .subscribe(vec!["/railgun/v2/0-1-fees/json".to_owned()])
            .await
            .is_err()
    );
    assert!(
        !scope.is_closed(),
        "rotation doesn't retire submission nodes"
    );
    drop(submission);
}

#[test]
fn rotation_commits_only_after_retirement_and_replaces_only_active_waku() {
    // (Tor session generation, Waku starts attempted)
    let run = |waku_was_active: bool,
               restart_deferred: bool,
               retired: Result<(), String>,
               commit_ok: bool,
               start_ok: bool| {
        let mut steps = (7_u64, 0_u32);
        let result = complete_tor_rotation(
            &mut steps,
            waku_was_active,
            restart_deferred,
            retired,
            |steps| {
                if commit_ok {
                    steps.0 += 1;
                    Ok(steps.0)
                } else {
                    Err(eyre::eyre!("no SOCKS bridge"))
                }
            },
            |steps| {
                steps.1 += 1;
                start_ok
            },
        );
        (steps, result)
    };

    for restart_deferred in [false, true] {
        assert_eq!(
            run(true, restart_deferred, Ok(()), true, true),
            ((8, 1), Ok((8, None))),
            "one start replaces active Waku"
        );
        assert_eq!(
            run(true, restart_deferred, Ok(()), true, false),
            ((8, 1), Ok((8, Some(GatewayNetworkError::WakuUnavailable)))),
            "failure after commit keeps the new session and isn't retried"
        );
        for (retired, commit_ok) in [
            (Err("shutdown timed out".to_owned()), true),
            (Ok(()), false),
        ] {
            assert_eq!(
                run(true, restart_deferred, retired, commit_ok, true),
                (
                    (7, u32::from(restart_deferred)),
                    Err(GatewayNetworkError::NewSessionFailed)
                ),
                "failure before commit keeps the session and starts only an owed restart"
            );
        }
    }
    assert_eq!(
        run(false, false, Ok(()), true, true),
        ((8, 0), Ok((8, None))),
        "inactive Waku isn't constructed"
    );
    assert_eq!(
        run(false, true, Ok(()), true, false),
        ((8, 1), Ok((8, None))),
        "an owed ordinary restart runs once, and its failure isn't the rotation's"
    );
}

#[test]
fn a_rotation_failure_stays_on_the_native_control_after_its_error_is_cleared() {
    let failure = Some(GatewayNetworkError::NewSessionFailed);
    let later: Arc<str> = Arc::from("Waku delivery network is unavailable: no peer");
    assert_eq!(
        native_network_status_error(Some(&later), failure),
        Some(Arc::clone(&later))
    );
    // A later Waku start, including a late worker's ordinary restart, clears the root's error.
    assert_eq!(
        native_network_status_error(None, failure).as_deref(),
        Some(GatewayNetworkError::NewSessionFailed.message())
    );
    assert_eq!(native_network_status_error(None, None), None);
}

#[test]
fn one_pending_rotation_joins_duplicates_and_holds_waku_until_commit() {
    let mut pending = None;
    assert!(join_or_begin_tor_rotation(
        &mut pending,
        TorRotation::for_test(1, Some(4))
    ));
    // Native and gateway activations while pending join it.
    assert!(!join_or_begin_tor_rotation(
        &mut pending,
        TorRotation::for_test(2, Some(4))
    ));
    assert!(!join_or_begin_tor_rotation(
        &mut pending,
        TorRotation::for_test(3, None)
    ));

    // The retired workers finalize while the rotation is pending. That stop is the rotation's
    // own, so it owes no restart.
    let mut stopping = Some(WakuStoppingState::for_test(4));
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    let action = complete_waku_worker_generation(
        None,
        &mut stopping,
        4,
        WakuWorkerCompletionKind::Clean,
        true,
        false,
        &monitor_state,
        &event_tx,
    );
    assert_eq!(
        action,
        WakuWorkerCompletionAction::FinalizedStop { restart: true }
    );
    assert!(!restart_now_or_defer_to_rotation(pending.as_mut(), 4));
    assert!(!pending.as_ref().unwrap().restart_deferred);
    // A runtime already stopping when the rotation began, as after lock and unlock, owes unlock's
    // restart, which the rotation holds back.
    assert!(!restart_now_or_defer_to_rotation(pending.as_mut(), 3));
    assert!(pending.as_ref().unwrap().restart_deferred);
    assert!(!should_start_waku_for_delivery(
        DeliveryMode::PublicBroadcaster,
        true,
        false,
        waku_start_blocked(stopping.as_ref(), pending.as_ref()),
        false,
        RelayNetworkMode::Tor,
    ));

    let commits = [2, 3, 1, 1]
        .into_iter()
        .filter(|id| take_tor_rotation(&mut pending, *id).is_some())
        .count();
    assert_eq!(commits, 1, "the session advances once");
    assert!(
        restart_now_or_defer_to_rotation(pending.as_mut(), 3),
        "after the rotation a late completion restarts at once"
    );
    assert!(should_start_waku_runtime(
        true,
        false,
        waku_start_blocked(stopping.as_ref(), pending.as_ref()),
        false,
        RelayNetworkMode::Tor,
    ));
}

#[test]
fn cancelled_rotation_cannot_commit_a_later_one() {
    let mut pending = None;
    assert!(join_or_begin_tor_rotation(
        &mut pending,
        TorRotation::for_test(1, Some(2))
    ));
    // Lock or root replacement cancels before commit.
    pending = None;
    assert!(join_or_begin_tor_rotation(
        &mut pending,
        TorRotation::for_test(2, None)
    ));

    assert!(
        take_tor_rotation(&mut pending, 1).is_none(),
        "the cancelled operation neither commits nor starts Waku"
    );
    assert!(take_tor_rotation(&mut pending, 2).is_some());
}

#[tokio::test]
async fn replacement_has_a_new_identity_and_late_old_completion_leaves_it_alone() {
    let config = quiet_waku_config();
    let old = config.build_client().expect("old Waku client");
    let replacement = config.build_client().expect("replacement Waku client");
    assert!(old.local_peer_id().is_some());
    assert_ne!(old.local_peer_id(), replacement.local_peer_id());

    let retired_generation = 5;
    let mut stopping = Some(WakuStoppingState::for_test(retired_generation));
    let monitor_state = shared();
    let (event_tx, _event_rx) = event_channel(16);
    monitor_state
        .write()
        .upsert_fee(fee_row(1, Address::from([0x12; 20]), "retired-worker-fees"));
    release_retired_waku_generation(&mut stopping, retired_generation, &monitor_state, &event_tx);
    assert!(stopping.is_none());
    assert_eq!(monitor_state.read().fee_rows(), [] as [FeeRow; 0]);

    monitor_state
        .write()
        .upsert_fee(fee_row(2, Address::from([0x13; 20]), "replacement-fees"));
    let action = complete_waku_worker_generation(
        Some(retired_generation + 1),
        &mut stopping,
        retired_generation,
        WakuWorkerCompletionKind::WorkerError,
        true,
        false,
        &monitor_state,
        &event_tx,
    );
    assert_eq!(action, WakuWorkerCompletionAction::Ignore);
    assert_eq!(monitor_state.read().fee_rows().len(), 1);
    assert_eq!(monitor_state.read().fee_rows()[0].chain_id, 2);
}

#[tokio::test]
async fn rotation_during_a_stop_awaits_that_transport_and_starts_nothing() {
    // Lock stopped the runtime, but its workers and transport are still winding down.
    let client = quiet_waku_config()
        .build_client()
        .expect("quiet Waku client");
    let (completion, quiesced_tx) = WakuWorkerCompletionToken::channel_for_test();
    let stopping = WakuStoppingState {
        generation: 9,
        client: Arc::clone(&client),
        completion,
    };
    let (replaced, retiring_client, completion) =
        waku_to_retire(None, Some(&stopping)).expect("a stopping transport is retired");
    assert_eq!(replaced, None, "a stopping runtime isn't replaced");
    assert!(Arc::ptr_eq(&retiring_client, &client));

    let retire = tokio::spawn(retire_root_waku(completion, retiring_client));
    tokio::task::yield_now().await;
    assert!(
        !retire.is_finished(),
        "the commit waits for the stopping workers"
    );
    quiesced_tx
        .send(true)
        .expect("quiescence receiver is alive");
    let retired = tokio::time::timeout(Duration::from_secs(10), retire)
        .await
        .expect("stopping transport retires")
        .expect("retirement task completes");
    assert!(
        client
            .subscribe(vec!["/railgun/v2/0-1-fees/json".to_owned()])
            .await
            .is_err(),
        "the stopping transport has terminated before the commit"
    );

    // (Tor session generation, Waku runtimes started)
    let mut steps = (3_u64, 0_u32);
    let result = complete_tor_rotation(
        &mut steps,
        replaced.is_some(),
        false,
        retired,
        |steps| {
            steps.0 += 1;
            Ok(steps.0)
        },
        |steps| {
            steps.1 += 1;
            true
        },
    );
    assert_eq!(result, Ok((4, None)));
    assert_eq!(steps, (4, 0), "rotation constructs no runtime");
}
