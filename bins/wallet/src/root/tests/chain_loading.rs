use super::*;
use crate::root::auto_lock::AutoLockTimestamp;
use crate::root::chain_load::{
    ChainProgressProjection, ChainSyncProgress, InstalledObserverProjection,
    WalletReadinessDisposition, WalletSyncLifecycle, WalletSyncLifecycleCleanupReport,
    WalletSyncLifecycleCleanupTask, WalletSyncLifecycleCleanupWaitGroup,
    chain_load_start_is_allowed, chain_progress_update_is_current,
    destructive_cache_reset_admission_is_allowed, installed_observer_is_exact_current,
    installed_observer_terminal_transition, ppoi_validation_completion_is_current,
    ppoi_validation_toast_scope_is_current, retain_auxiliary_stream, wait_for_wallet_sync_cleanups,
    wallet_readiness_disposition, wallet_sync_maintenance_allows_start,
    wallet_sync_start_is_admitted,
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

mod balance_status;
mod cleanup;
mod lifecycle;
mod observers;
mod ppoi_status;
mod status_labels;
mod sync_admission;
