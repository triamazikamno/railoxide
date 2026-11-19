use super::*;

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
