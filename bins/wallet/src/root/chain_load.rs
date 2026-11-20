use std::sync::Arc;

use gpui::{Context, Window};
use gpui_component::WindowExt;
use tokio::sync::{oneshot, watch};
use wallet_ops::{
    DesktopWalletSyncStartPolicy, HttpContext, ListUtxosOutput, PoiArtifactCacheProgress,
    SyncProgressStage, SyncProgressUnit, SyncProgressUpdate, ViewWalletChainSessionRequest,
    WalletPpoiWorkflowStatus, WalletReadiness, WalletSessionStore, WalletSyncTip,
    vault::WalletSource,
};

use super::utxo::should_focus_utxo_table;
use super::{
    BroadcasterActivityTab, InitialSyncObservation, SelectedNetwork, WalletRoot, WalletTab,
};

mod lifecycle;
mod load_start;
mod observer;
mod observer_checks;
mod selection;
mod session_resets;
mod status;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

pub(super) use lifecycle::{
    InstalledObserverToken, WalletPublicSyncCacheResetContext, WalletRootReplacementCleanup,
    WalletSyncLifecycle, WalletSyncLifecycleCleanup, WalletSyncLifecycleCleanupReport,
    WalletSyncLifecycleCleanupTask, WalletSyncLifecycleCleanupWaitGroup,
    wait_for_wallet_sync_cleanups,
};
use lifecycle::{wallet_sync_startup_superseded, wallet_sync_startup_superseded_error};
use observer::ChainLoadScope;
pub(super) use status::{
    BalanceSyncIssue, PresenceStatus, SyncStatusContext, SyncStatusLabels, WalletStatusCounts,
    balance_sync_issue, balances_presence_status, loading_summary, ppoi_presence_status,
    ready_status_center, sync_status_center, sync_status_labels,
};
#[cfg(test)]
pub(super) use status::{
    balance_lag_threshold_blocks, balance_stale_timeout, progress_detail,
    ready_wallet_status_labels, ready_wallet_status_shows_text,
};

pub(super) enum ChainUtxoState {
    Idle,
    Loading {
        progress: ChainSyncProgress,
    },
    Syncing {
        snapshot: Arc<ListUtxosOutput>,
        progress: ChainSyncProgress,
        /// The session was ready before, and this sync takes in newer blocks.
        catching_up: bool,
        session: Arc<wallet_ops::WalletSession>,
        observer_token: InstalledObserverToken,
        sync_tip: WalletSyncTip,
        poi_refreshing: bool,
        ppoi_workflow_status: WalletPpoiWorkflowStatus,
    },
    Ready {
        snapshot: Arc<ListUtxosOutput>,
        session: Arc<wallet_ops::WalletSession>,
        observer_token: InstalledObserverToken,
        sync_tip: WalletSyncTip,
        poi_refreshing: bool,
        ppoi_workflow_status: WalletPpoiWorkflowStatus,
    },
    Error {
        message: Arc<str>,
        start_block: Option<u64>,
        ppoi_workflow_status: WalletPpoiWorkflowStatus,
    },
}

/// A chain sync's progress: its stage's latest update, and how far the whole sync has come.
/// Each stage reports its progress from zero, and updates of different stages can arrive out
/// of order, so `percent` puts the stages on one scale and keeps the furthest point reached.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ChainSyncProgress {
    latest: Option<SyncProgressUpdate>,
    percent: Option<u8>,
}

impl ChainSyncProgress {
    /// Take the sync's next update, or `None` while its stage has none to show. The percent
    /// rises to the update's and never falls.
    pub(super) fn observe(&mut self, update: Option<SyncProgressUpdate>) {
        self.latest = update;
        if let Some(update) = update {
            self.percent = self.percent.max(Some(overall_sync_percent(update)));
        }
    }

    pub(super) const fn latest(self) -> Option<SyncProgressUpdate> {
        self.latest
    }

    /// How far the whole sync has come, in percent. `None` until an update was observed.
    pub(super) const fn percent(self) -> Option<u8> {
        self.percent
    }
}

/// Where `update` stands in a whole chain sync, in percent: its own percent within its
/// stage's share of the sync.
pub(super) fn overall_sync_percent(update: SyncProgressUpdate) -> u8 {
    // Estimates of each stage's share of a cold sync, as the percent it starts and ends at.
    let (start, end) = match (update.stage, update.unit) {
        (SyncProgressStage::SynchronizingCommitments, SyncProgressUnit::CommitmentTail) => {
            (45_u16, 50)
        }
        (SyncProgressStage::SynchronizingCommitments, _) => (0, 45),
        (SyncProgressStage::PreparingUtxoIndex, _) => (50, 80),
        (SyncProgressStage::IndexingUtxos, _) => (80, 100),
    };
    let scaled = start + (end - start) * u16::from(update.percent()) / 100;
    u8::try_from(scaled).unwrap_or(100)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WalletReadinessDisposition {
    Syncing,
    Ready,
    Error(Arc<str>),
}

pub(super) fn wallet_readiness_disposition(
    readiness: &WalletReadiness,
) -> WalletReadinessDisposition {
    match readiness {
        WalletReadiness::Syncing => WalletReadinessDisposition::Syncing,
        WalletReadiness::Ready => WalletReadinessDisposition::Ready,
        WalletReadiness::Failed(reason) => {
            WalletReadinessDisposition::Error(Arc::from(reason.to_string()))
        }
        WalletReadiness::Shutdown => {
            WalletReadinessDisposition::Error(Arc::from("wallet sync session stopped"))
        }
    }
}

pub(super) fn chain_load_start_is_allowed(
    deleting_wallet_id: Option<&str>,
    selected_wallet_id: Option<&str>,
) -> bool {
    match deleting_wallet_id {
        Some(deleting_wallet_id) => selected_wallet_id != Some(deleting_wallet_id),
        None => true,
    }
}

pub(super) const fn wallet_sync_start_is_admitted(
    pending_software_profile_open: bool,
    has_view_session: bool,
    has_selected_wallet: bool,
    maintenance_allows_start: bool,
) -> bool {
    !pending_software_profile_open
        && has_view_session
        && has_selected_wallet
        && maintenance_allows_start
}

pub(super) const fn wallet_sync_maintenance_allows_start(
    public_sync_cache_resetting: bool,
    merkle_forest_cache_resetting: bool,
) -> bool {
    !public_sync_cache_resetting && !merkle_forest_cache_resetting
}

pub(super) const fn destructive_cache_reset_admission_is_allowed(
    wallet_deletion_in_progress: bool,
    sync_cleanup_in_progress: bool,
) -> bool {
    !wallet_deletion_in_progress && !sync_cleanup_in_progress
}

impl ChainUtxoState {
    const fn installed_observer_token(&self) -> Option<&InstalledObserverToken> {
        match self {
            Self::Syncing { observer_token, .. } | Self::Ready { observer_token, .. } => {
                Some(observer_token)
            }
            Self::Idle | Self::Loading { .. } | Self::Error { .. } => None,
        }
    }

    pub(super) const fn snapshot(&self) -> Option<&Arc<ListUtxosOutput>> {
        match self {
            Self::Syncing { snapshot, .. } | Self::Ready { snapshot, .. } => Some(snapshot),
            Self::Idle | Self::Loading { .. } | Self::Error { .. } => None,
        }
    }

    pub(super) const fn progress(&self) -> Option<SyncProgressUpdate> {
        match self {
            Self::Loading { progress } | Self::Syncing { progress, .. } => progress.latest(),
            Self::Idle | Self::Ready { .. } | Self::Error { .. } => None,
        }
    }

    /// How far the chain's whole sync has come, in percent, while it is under way and an
    /// update has arrived.
    pub(super) const fn sync_percent(&self) -> Option<u8> {
        match self {
            Self::Loading { progress } | Self::Syncing { progress, .. } => progress.percent(),
            Self::Idle | Self::Ready { .. } | Self::Error { .. } => None,
        }
    }

    pub(super) fn start_block(&self) -> Option<u64> {
        match self {
            Self::Syncing { session, .. } | Self::Ready { session, .. } => {
                Some(session.start_block)
            }
            Self::Error { start_block, .. } => *start_block,
            Self::Idle | Self::Loading { .. } => None,
        }
    }

    pub(super) const fn renders_table(&self) -> bool {
        matches!(
            self,
            Self::Loading { .. } | Self::Syncing { .. } | Self::Ready { .. }
        )
    }

    pub(super) const fn is_syncing(&self) -> bool {
        matches!(self, Self::Loading { .. } | Self::Syncing { .. })
    }

    pub(super) const fn poi_refreshing(&self) -> bool {
        match self {
            Self::Syncing { poi_refreshing, .. } | Self::Ready { poi_refreshing, .. } => {
                *poi_refreshing
            }
            Self::Idle | Self::Loading { .. } | Self::Error { .. } => false,
        }
    }

    pub(super) const fn set_poi_refreshing(&mut self, refreshing: bool) {
        match self {
            Self::Syncing { poi_refreshing, .. } | Self::Ready { poi_refreshing, .. } => {
                *poi_refreshing = refreshing;
            }
            Self::Idle | Self::Loading { .. } | Self::Error { .. } => {}
        }
    }

    pub(super) const fn ppoi_workflow_status(&self) -> WalletPpoiWorkflowStatus {
        match self {
            Self::Syncing {
                ppoi_workflow_status,
                ..
            }
            | Self::Ready {
                ppoi_workflow_status,
                ..
            }
            | Self::Error {
                ppoi_workflow_status,
                ..
            } => *ppoi_workflow_status,
            Self::Idle | Self::Loading { .. } => WalletPpoiWorkflowStatus {
                awaiting_recovery: 0,
                awaiting_public_txid_data: 0,
                awaiting_poi_data: 0,
                retrying_recovery: 0,
                recovery_needs_attention: 0,
                awaiting_submission: 0,
                awaiting_validation: 0,
                needs_attention: 0,
                validation_revision: 0,
            },
        }
    }

    pub(super) fn poi_refresh_session(&self) -> Option<Arc<wallet_ops::WalletSession>> {
        match self {
            Self::Syncing { session, .. } | Self::Ready { session, .. } => Some(session.clone()),
            Self::Idle | Self::Loading { .. } | Self::Error { .. } => None,
        }
    }

    pub(super) const fn sync_tip(&self) -> Option<WalletSyncTip> {
        match self {
            Self::Syncing { sync_tip, .. } | Self::Ready { sync_tip, .. } => Some(*sync_tip),
            Self::Idle | Self::Loading { .. } | Self::Error { .. } => None,
        }
    }

    pub(super) const fn private_action_forms_available(&self) -> bool {
        matches!(self, Self::Syncing { .. } | Self::Ready { .. })
    }

    pub(super) const fn private_action_generation_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

#[derive(Clone)]
pub(super) struct ChainLoadOverrides {
    pub(super) init_block_number: Option<u64>,
    pub(super) sync_to_block: Option<u64>,
    pub(super) sync_start_policy: Option<DesktopWalletSyncStartPolicy>,
    pub(super) use_indexed_wallet_catch_up: bool,
    pub(super) rewind_wallet_cache: bool,
}

pub(super) const fn chain_load_overrides() -> ChainLoadOverrides {
    ChainLoadOverrides {
        init_block_number: None,
        sync_to_block: None,
        sync_start_policy: None,
        use_indexed_wallet_catch_up: true,
        rewind_wallet_cache: false,
    }
}

pub(super) fn wallet_generation_matches(
    selected_wallet_id: Option<&str>,
    active_wallet_generation: u64,
    wallet_id: &str,
    generation: u64,
) -> bool {
    active_wallet_generation == generation && selected_wallet_id == Some(wallet_id)
}

pub(super) fn installed_observer_is_exact_current(
    selected_wallet_id: Option<&str>,
    active_wallet_generation: u64,
    wallet_id: &str,
    wallet_generation: u64,
    chain_id: u64,
    installed_token: Option<&InstalledObserverToken>,
    observer_token: &InstalledObserverToken,
) -> bool {
    wallet_generation_matches(
        selected_wallet_id,
        active_wallet_generation,
        wallet_id,
        wallet_generation,
    ) && observer_token.chain_id == chain_id
        && installed_token == Some(observer_token)
}

pub(super) const fn ppoi_validation_completion_is_current(
    previous: WalletPpoiWorkflowStatus,
    current: WalletPpoiWorkflowStatus,
    observer_is_exact_current: bool,
) -> bool {
    observer_is_exact_current
        && current.validation_revision > previous.validation_revision
        && previous.has_outstanding()
        && !current.has_outstanding()
}

pub(super) fn ppoi_validation_toast_scope_is_current(
    selected_wallet_id: Option<&str>,
    active_wallet_generation: u64,
    queued_wallet_id: &str,
    queued_wallet_generation: u64,
) -> bool {
    wallet_generation_matches(
        selected_wallet_id,
        active_wallet_generation,
        queued_wallet_id,
        queued_wallet_generation,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChainProgressProjection<'a> {
    Loading,
    Syncing { token: &'a InstalledObserverToken },
    Ready { token: &'a InstalledObserverToken },
}

pub(super) fn chain_progress_update_is_current(
    selected_wallet_id: Option<&str>,
    active_wallet_generation: u64,
    wallet_id: &str,
    wallet_generation: u64,
    chain_id: u64,
    startup_is_current: bool,
    projection: Option<ChainProgressProjection<'_>>,
    observer_token: &InstalledObserverToken,
) -> bool {
    match projection {
        Some(ChainProgressProjection::Loading) => startup_is_current,
        Some(
            ChainProgressProjection::Syncing { token } | ChainProgressProjection::Ready { token },
        ) => installed_observer_is_exact_current(
            selected_wallet_id,
            active_wallet_generation,
            wallet_id,
            wallet_generation,
            chain_id,
            Some(token),
            observer_token,
        ),
        None => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InstalledObserverProjection<'a> {
    Syncing {
        token: &'a InstalledObserverToken,
        start_block: u64,
    },
    Ready {
        token: &'a InstalledObserverToken,
        start_block: u64,
    },
}

impl InstalledObserverProjection<'_> {
    const fn token(&self) -> &InstalledObserverToken {
        match self {
            Self::Syncing { token, .. } | Self::Ready { token, .. } => token,
        }
    }

    const fn start_block(&self) -> u64 {
        match self {
            Self::Syncing { start_block, .. } | Self::Ready { start_block, .. } => *start_block,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InstalledObserverTerminalState {
    pub(super) message: Arc<str>,
    pub(super) start_block: u64,
}

pub(super) fn installed_observer_terminal_transition(
    selected_wallet_id: Option<&str>,
    active_wallet_generation: u64,
    wallet_id: &str,
    wallet_generation: u64,
    chain_id: u64,
    installed: Option<InstalledObserverProjection<'_>>,
    observer_token: &InstalledObserverToken,
    message: Arc<str>,
) -> Option<InstalledObserverTerminalState> {
    let installed = installed?;
    installed_observer_is_exact_current(
        selected_wallet_id,
        active_wallet_generation,
        wallet_id,
        wallet_generation,
        chain_id,
        Some(installed.token()),
        observer_token,
    )
    .then(|| InstalledObserverTerminalState {
        message,
        start_block: installed.start_block(),
    })
}

pub(super) fn retain_auxiliary_stream<T>(
    receiver: &mut Option<watch::Receiver<T>>,
    changed: &Result<(), watch::error::RecvError>,
) -> bool {
    if changed.is_err() {
        *receiver = None;
        false
    } else {
        true
    }
}
