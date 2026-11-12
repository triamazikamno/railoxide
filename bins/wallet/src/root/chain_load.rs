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
    BroadcasterActivityTab, InitialCatchUpFingerprint, InitialSyncObservation, SelectedNetwork,
    WalletRoot, WalletTab, format_report_chain,
};

mod lifecycle;
mod status;

use lifecycle::{
    InstalledObserverCompletion, wallet_sync_startup_superseded,
    wallet_sync_startup_superseded_error,
};
pub(super) use lifecycle::{
    InstalledObserverToken, WalletPublicSyncCacheResetContext, WalletRootReplacementCleanup,
    WalletSyncLifecycle, WalletSyncLifecycleCleanup, WalletSyncLifecycleCleanupReport,
    WalletSyncLifecycleCleanupTask, WalletSyncLifecycleCleanupWaitGroup,
};
use status::syncing_progress;
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

impl WalletRoot {
    pub(super) fn selected_wallet_source(&self) -> WalletSource {
        let Some(selected_wallet_id) = self.selected_wallet_id.as_ref() else {
            return WalletSource::Imported;
        };
        self.wallet_options
            .iter()
            .find(|option| option.wallet_id.as_ref() == selected_wallet_id.as_ref())
            .map_or(WalletSource::Imported, |option| option.source)
    }

    fn selected_wallet_sync_start_policy(&self) -> DesktopWalletSyncStartPolicy {
        let Some(selected_wallet_id) = self.selected_wallet_id.as_ref() else {
            return DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill;
        };
        self.wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == selected_wallet_id.as_ref())
            .map_or(
                DesktopWalletSyncStartPolicy::ImportedHistoricalBackfill,
                DesktopWalletSyncStartPolicy::from,
            )
    }

    pub(super) fn selected_chain_wallet_start_block(&self) -> Option<u64> {
        self.chain_states
            .get(&self.last_evm_chain)
            .and_then(ChainUtxoState::start_block)
    }

    pub(super) fn selected_chain_poi_artifact_progress(&self) -> Option<&PoiArtifactCacheProgress> {
        self.poi_artifact_cache_progress.get(&self.last_evm_chain)
    }

    pub(super) fn is_active_wallet_generation(&self, wallet_id: &str, generation: u64) -> bool {
        wallet_generation_matches(
            self.selected_wallet_id.as_deref(),
            self.active_wallet_generation,
            wallet_id,
            generation,
        )
    }

    pub(super) fn reset_wallet_scoped_state(&mut self, cx: &mut Context<'_, Self>) {
        self.invalidate_governance_context();
        self.governance.proposal_participation.clear_session();
        self.invalidate_proposals_chain(self.last_evm_chain);
        self.clear_protected_software_seed_session(cx);
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preference_error = None;
        self.active_broadcaster_tab = BroadcasterActivityTab::default();
        self.clear_public_wallet_runtime_state(cx);
        self.private_action_form = None;
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        self.blocked_shield_rescue_rows.clear();
        self.blocked_shield_refunds_in_flight.clear();
        self.blocked_shield_rescue_lookup_generation =
            self.blocked_shield_rescue_lookup_generation.wrapping_add(1);
        self.pending_ppoi_validation_toast = None;
        // The next wallet opens on the same tab, unless its chain can't show it.
        if self.active_wallet_tab.needs_railgun() && !self.selected_chain_has_railgun() {
            self.active_wallet_tab = self.default_wallet_tab();
        }
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.poi_artifact_cache_retry_attempts.clear();
        self.sync_utxo_table(cx);
    }

    pub(super) fn shutdown_wallet_session_store(&mut self) {
        self.clear_private_broadcaster_progress_state();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        self.start_wallet_sync_cleanup(cleanup);
    }

    pub(super) fn begin_public_sync_cache_reset(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletPublicSyncCacheResetContext {
        // The forms outlive the aborted submission.
        self.pin_abandoned_private_broadcaster_payload();
        self.begin_public_transaction_shutdown();
        self.clear_private_broadcaster_progress_state();
        self.public_sync_cache_resetting = true;
        self.advance_active_wallet_generation();
        let session_store = self.wallet_sync_lifecycle.public_sync_cache_reset_cell();
        let cleanup = self.wallet_sync_lifecycle.supersede_wallet();
        self.start_wallet_sync_cleanup(cleanup);
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.poi_artifact_cache_retry_attempts.clear();
        self.sync_utxo_table(cx);
        cx.notify();
        WalletPublicSyncCacheResetContext {
            cleanup: self.wallet_sync_cleanup_wait_group(),
            session_store,
        }
    }

    pub(super) fn begin_poi_data_reset(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletPublicSyncCacheResetContext {
        // The forms outlive the aborted submission.
        self.pin_abandoned_private_broadcaster_payload();
        self.begin_public_transaction_shutdown();
        self.clear_private_broadcaster_progress_state();
        self.public_sync_cache_resetting = true;
        self.advance_active_wallet_generation();
        let session_store = self.wallet_sync_lifecycle.public_sync_cache_reset_cell();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        self.start_wallet_sync_cleanup(cleanup);
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.poi_artifact_cache_retry_attempts.clear();
        self.sync_utxo_table(cx);
        cx.notify();
        WalletPublicSyncCacheResetContext {
            cleanup: self.wallet_sync_cleanup_wait_group(),
            session_store,
        }
    }

    pub(super) fn finish_public_sync_cache_reset(
        &mut self,
        restart_safe: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_sync_cache_resetting = false;
        self.resume_public_transactions();
        if restart_safe && self.view_session.is_some() {
            self.ensure_chain_load(self.last_evm_chain, cx);
        } else {
            cx.notify();
        }
    }

    pub(super) fn supersede_wallet_sessions(&mut self) {
        let cleanup = self.wallet_sync_lifecycle.supersede_wallet();
        self.start_wallet_sync_cleanup(cleanup);
    }

    pub(super) fn begin_wallet_deletion_sync_shutdown(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletSyncLifecycleCleanupWaitGroup {
        self.begin_public_transaction_shutdown();
        self.advance_active_wallet_generation();
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        self.revealed_passphrase_context_id = None;
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        self.start_wallet_sync_cleanup(cleanup);
        self.reset_wallet_scoped_state(cx);
        cx.notify();
        self.wallet_sync_cleanup_wait_group()
    }

    pub(super) fn begin_root_replacement_shutdown(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> (
        WalletRootReplacementCleanup,
        HttpContext,
        wallet_ops::PublicTransactionTracker,
    ) {
        self.begin_public_transaction_shutdown();
        self.advance_active_wallet_generation();
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        self.revealed_passphrase_context_id = None;
        self.wallet_sync_lifecycle_shutdown_started = true;
        self.cancel_tor_rotation();
        let waku_completion = self.stop_waku_for_root_replacement();
        let proposal_cleanup = self.proposals.take_cleanup();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        // A retained Tor context must not keep warming the outgoing routes.
        self.rpc_identity_warmup = None;
        self.active_rpc_chains.clear();
        self.public_broadcaster_anchor_refresh
            .set_active_chains(&[]);
        let outgoing_http = self.http.clone();
        self.reset_wallet_scoped_state(cx);
        self.start_wallet_sync_cleanup(cleanup);
        let sync_cleanup = self.wallet_sync_cleanup_wait_group();
        let root_replacement_cleanup = WalletRootReplacementCleanup::spawn(
            &self.runtime,
            sync_cleanup,
            waku_completion,
            proposal_cleanup,
            self.gateway.take_shutdown_handle(),
            self.monitor_state.clone(),
            self.monitor_event_tx.clone(),
        );
        (
            root_replacement_cleanup,
            outgoing_http,
            self.public_transaction_tracker.successor(),
        )
    }

    fn start_wallet_sync_cleanup(
        &mut self,
        cleanup: WalletSyncLifecycleCleanup,
    ) -> WalletSyncLifecycleCleanupTask {
        self.prune_finished_wallet_sync_cleanups();
        let task = cleanup.spawn(&self.runtime);
        self.wallet_sync_cleanup_tasks.push(task.clone());
        task
    }

    fn prune_finished_wallet_sync_cleanups(&mut self) {
        self.wallet_sync_cleanup_tasks
            .retain(|cleanup| !cleanup.is_finished());
    }

    pub(super) fn destructive_cache_reset_is_allowed(&mut self) -> bool {
        self.prune_finished_wallet_sync_cleanups();
        destructive_cache_reset_admission_is_allowed(
            self.manage_wallets.deleting_wallet_id.is_some(),
            !self.wallet_sync_cleanup_tasks.is_empty()
                || self
                    .public_transaction_cleanup
                    .as_ref()
                    .is_some_and(|cleanup| !cleanup.is_finished()),
        )
    }

    pub(super) fn wallet_sync_cleanup_wait_group(&mut self) -> WalletSyncLifecycleCleanupWaitGroup {
        self.prune_finished_wallet_sync_cleanups();
        let mut cleanup =
            WalletSyncLifecycleCleanupWaitGroup::new(self.wallet_sync_cleanup_tasks.clone());
        cleanup
            .public_transactions
            .clone_from(&self.public_transaction_cleanup);
        cleanup
    }

    pub(super) fn begin_merkle_forest_cache_reset(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletSyncLifecycleCleanupWaitGroup {
        self.begin_public_transaction_shutdown();
        self.merkle_forest_cache_resetting = true;
        self.advance_active_wallet_generation();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        self.start_wallet_sync_cleanup(cleanup);
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.private_action_form = None;
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.poi_artifact_cache_retry_attempts.clear();
        self.sync_utxo_table(cx);
        cx.notify();
        self.wallet_sync_cleanup_wait_group()
    }

    pub(super) fn finish_merkle_forest_cache_reset(
        &mut self,
        reset_succeeded: bool,
        cx: &mut Context<'_, Self>,
    ) {
        self.merkle_forest_cache_resetting = false;
        self.resume_public_transactions();
        self.prune_finished_wallet_sync_cleanups();
        if reset_succeeded && self.view_session.is_some() {
            self.ensure_chain_load(self.last_evm_chain, cx);
        } else {
            cx.notify();
        }
    }

    fn is_current_chain_load_startup(
        &self,
        wallet_id: &str,
        active_wallet_generation: u64,
        chain_id: u64,
        lifecycle_generation: u64,
        task_id: u64,
    ) -> bool {
        self.is_active_wallet_generation(wallet_id, active_wallet_generation)
            && self.wallet_sync_lifecycle.is_current_startup(
                chain_id,
                lifecycle_generation,
                task_id,
            )
    }

    fn is_current_installed_observer(
        &self,
        wallet_id: &str,
        active_wallet_generation: u64,
        chain_id: u64,
        observer_token: &InstalledObserverToken,
    ) -> bool {
        installed_observer_is_exact_current(
            self.selected_wallet_id.as_deref(),
            self.active_wallet_generation,
            wallet_id,
            active_wallet_generation,
            chain_id,
            self.chain_states
                .get(&chain_id)
                .and_then(ChainUtxoState::installed_observer_token),
            observer_token,
        )
    }

    /// Drops the broker's cached head for `chain_id`, but only while this observer is still the
    /// wallet's current one, so a superseded observer cannot invalidate a live chain's cache.
    fn invalidate_block_for_current_observer(
        &self,
        wallet_id: &str,
        active_wallet_generation: u64,
        chain_id: u64,
        observer_token: &InstalledObserverToken,
    ) {
        if self.is_current_installed_observer(
            wallet_id,
            active_wallet_generation,
            chain_id,
            observer_token,
        ) {
            self.http.rpc_broker().invalidate_block(chain_id);
        }
    }

    fn transition_current_installed_observer_to_error(
        &mut self,
        wallet_id: &str,
        active_wallet_generation: u64,
        chain_id: u64,
        lifecycle_generation: u64,
        task_id: u64,
        observer_token: &InstalledObserverToken,
        ppoi_workflow_status: Option<WalletPpoiWorkflowStatus>,
        message: Arc<str>,
        cx: &mut Context<'_, Self>,
    ) -> Option<Arc<wallet_ops::WalletSession>> {
        let terminal = installed_observer_terminal_transition(
            self.selected_wallet_id.as_deref(),
            self.active_wallet_generation,
            wallet_id,
            active_wallet_generation,
            chain_id,
            self.chain_states
                .get(&chain_id)
                .and_then(|state| match state {
                    ChainUtxoState::Syncing {
                        session,
                        observer_token,
                        ..
                    } => Some(InstalledObserverProjection::Syncing {
                        token: observer_token,
                        start_block: session.start_block,
                    }),
                    ChainUtxoState::Ready {
                        session,
                        observer_token,
                        ..
                    } => Some(InstalledObserverProjection::Ready {
                        token: observer_token,
                        start_block: session.start_block,
                    }),
                    ChainUtxoState::Idle
                    | ChainUtxoState::Loading { .. }
                    | ChainUtxoState::Error { .. } => None,
                }),
            observer_token,
            message,
        )?;
        self.http.rpc_broker().invalidate_block(chain_id);
        let state = self.chain_states.remove(&chain_id)?;
        let (session, previous_ppoi_workflow_status) = match state {
            ChainUtxoState::Syncing {
                session,
                ppoi_workflow_status,
                ..
            }
            | ChainUtxoState::Ready {
                session,
                ppoi_workflow_status,
                ..
            } => (session, ppoi_workflow_status),
            state @ (ChainUtxoState::Idle
            | ChainUtxoState::Loading { .. }
            | ChainUtxoState::Error { .. }) => {
                self.chain_states.insert(chain_id, state);
                return None;
            }
        };
        self.finish_chain_load_startup(chain_id, lifecycle_generation, task_id);
        self.handle_initial_sync_observation(
            active_wallet_generation,
            chain_id,
            InitialSyncObservation::Error,
        );
        self.chain_states.insert(
            chain_id,
            ChainUtxoState::Error {
                message: terminal.message,
                start_block: Some(terminal.start_block),
                ppoi_workflow_status: ppoi_workflow_status.unwrap_or(previous_ppoi_workflow_status),
            },
        );
        if self.last_evm_chain == chain_id {
            self.sync_utxo_table(cx);
        }
        cx.notify();
        Some(session)
    }

    fn finish_chain_load_startup(
        &mut self,
        chain_id: u64,
        lifecycle_generation: u64,
        task_id: u64,
    ) {
        self.wallet_sync_lifecycle
            .finish_startup(chain_id, lifecycle_generation, task_id);
    }

    pub(super) fn ensure_chain_load(&mut self, chain_id: u64, cx: &mut Context<'_, Self>) {
        let overrides = chain_load_overrides();
        self.start_chain_load(chain_id, &overrides, false, cx);
    }

    pub(super) fn ensure_chain_load_with_start_policy(
        &mut self,
        chain_id: u64,
        sync_start_policy: Option<DesktopWalletSyncStartPolicy>,
        cx: &mut Context<'_, Self>,
    ) {
        let mut overrides = chain_load_overrides();
        overrides.sync_start_policy = sync_start_policy;
        self.start_chain_load(chain_id, &overrides, false, cx);
    }

    pub(super) fn start_chain_load(
        &mut self,
        chain_id: u64,
        overrides: &ChainLoadOverrides,
        force: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(resolved_chain) = self.effective_chain_configs.railgun(chain_id).cloned() else {
            return;
        };
        if !chain_load_start_is_allowed(
            self.manage_wallets.deleting_wallet_id.as_deref(),
            self.selected_wallet_id.as_deref(),
        ) {
            tracing::debug!(chain_id, "skipping wallet sync during wallet deletion");
            return;
        }
        if !wallet_sync_start_is_admitted(
            matches!(
                self.vault_state,
                super::VaultState::PendingSoftwareProfileOpen
            ),
            self.view_session.is_some(),
            self.selected_wallet_id.is_some(),
            wallet_sync_maintenance_allows_start(
                self.public_sync_cache_resetting,
                self.merkle_forest_cache_resetting,
            ),
        ) {
            tracing::debug!(
                chain_id,
                "skipping wallet sync without an admitted wallet context"
            );
            return;
        }
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        if matches!(
            self.chain_states.get(&chain_id),
            Some(
                ChainUtxoState::Loading { .. }
                    | ChainUtxoState::Syncing { .. }
                    | ChainUtxoState::Ready { .. }
            )
        ) && !force
        {
            return;
        }

        let previous_start_block = self
            .chain_states
            .get(&chain_id)
            .and_then(ChainUtxoState::start_block);

        let previous_session = if force {
            match self.chain_states.remove(&chain_id) {
                Some(
                    ChainUtxoState::Syncing { session, .. } | ChainUtxoState::Ready { session, .. },
                ) => Some(session),
                Some(state) => {
                    self.chain_states.insert(chain_id, state);
                    None
                }
                None => None,
            }
        } else {
            None
        };

        self.chain_states.insert(
            chain_id,
            ChainUtxoState::Loading {
                progress: ChainSyncProgress::default(),
            },
        );
        self.handle_initial_sync_observation(
            self.active_wallet_generation,
            chain_id,
            InitialSyncObservation::Started,
        );
        self.sync_utxo_table(cx);

        let active_wallet_id: Arc<str> = Arc::from(view_session.wallet_id().to_owned());
        let active_wallet_generation = self.active_wallet_generation;
        let (progress_tx, mut progress_rx) = watch::channel(None);
        let registration = self.wallet_sync_lifecycle.prepare_startup(chain_id);
        let lifecycle_generation = registration.generation;
        let chain_load_task_id = registration.task_id;
        let observer_token = registration.observer_token.clone();
        let lifecycle_generation_token = Arc::clone(&registration.generation_token);
        let session_store = Arc::clone(&registration.session_store);
        let request = ViewWalletChainSessionRequest {
            view_session,
            wallet_scope_generation: lifecycle_generation,
            chain_id,
            effective_chain: resolved_chain,
            sync_start_policy: overrides
                .sync_start_policy
                .unwrap_or_else(|| self.selected_wallet_sync_start_policy()),
            init_block_number: overrides.init_block_number,
            sync_to_block: overrides.sync_to_block,
            use_indexed_wallet_catch_up: overrides.use_indexed_wallet_catch_up,
            poi_read_source: self.poi_read_source.clone(),
            rewind_wallet_cache: overrides.rewind_wallet_cache,
            progress_tx: Some(progress_tx),
        };
        let db_path = self.options.db_path.clone();
        let http = self.http.clone();
        let poi_read_source = self.poi_read_source.clone();
        let vault_db = self.vault_store.as_ref().map(|store| store.db());
        let (result_tx, result_rx) = oneshot::channel();
        let join = self.runtime.spawn(async move {
            let result = Box::pin(async move {
                if wallet_sync_startup_superseded(&lifecycle_generation_token, lifecycle_generation)
                {
                    return Err(wallet_sync_startup_superseded_error());
                }
                if let Some(previous_session) = previous_session {
                    previous_session.stop().await?;
                }
                if wallet_sync_startup_superseded(&lifecycle_generation_token, lifecycle_generation)
                {
                    return Err(wallet_sync_startup_superseded_error());
                }
                let store = session_store
                    .get_or_try_init(|| {
                        let db_path = db_path.clone();
                        let vault_db = vault_db.clone();
                        async move {
                            Ok::<Arc<WalletSessionStore>, eyre::Report>(Arc::new(match vault_db {
                                Some(db) => {
                                    WalletSessionStore::from_db(db, poi_read_source.clone())?
                                }
                                None => WalletSessionStore::open(db_path, poi_read_source.clone())?,
                            }))
                        }
                    })
                    .await?
                    .clone();
                if wallet_sync_startup_superseded(&lifecycle_generation_token, lifecycle_generation)
                {
                    return Err(wallet_sync_startup_superseded_error());
                }
                let session =
                    Box::pin(store.start_view_wallet_session_immediate(request, &http)).await?;
                if wallet_sync_startup_superseded(&lifecycle_generation_token, lifecycle_generation)
                {
                    if let Err(error) = session.stop().await {
                        tracing::warn!(
                            chain_id,
                            %error,
                            "failed to stop superseded wallet sync session"
                        );
                    }
                    return Err(wallet_sync_startup_superseded_error());
                }
                Ok(session)
            })
            .await;
            let _ = result_tx.send(result);
        });
        self.wallet_sync_lifecycle
            .track_startup(&registration, join);

        let progress_wallet_id = Arc::clone(&active_wallet_id);
        let progress_observer_token = observer_token.clone();
        cx.spawn(async move |this, cx| {
            loop {
                if progress_rx.changed().await.is_err() {
                    break;
                }
                let progress = *progress_rx.borrow();
                let should_continue = this.update(cx, |root, cx| {
                    let projection =
                        root.chain_states
                            .get(&chain_id)
                            .and_then(|state| match state {
                                ChainUtxoState::Loading { .. } => {
                                    Some(ChainProgressProjection::Loading)
                                }
                                ChainUtxoState::Syncing { observer_token, .. } => {
                                    Some(ChainProgressProjection::Syncing {
                                        token: observer_token,
                                    })
                                }
                                ChainUtxoState::Ready { observer_token, .. } => {
                                    Some(ChainProgressProjection::Ready {
                                        token: observer_token,
                                    })
                                }
                                ChainUtxoState::Idle | ChainUtxoState::Error { .. } => None,
                            });
                    let startup_is_current =
                        matches!(projection, Some(ChainProgressProjection::Loading))
                            && root.is_current_chain_load_startup(
                                progress_wallet_id.as_ref(),
                                active_wallet_generation,
                                chain_id,
                                lifecycle_generation,
                                chain_load_task_id,
                            );
                    if !chain_progress_update_is_current(
                        root.selected_wallet_id.as_deref(),
                        root.active_wallet_generation,
                        progress_wallet_id.as_ref(),
                        active_wallet_generation,
                        chain_id,
                        startup_is_current,
                        projection,
                        &progress_observer_token,
                    ) {
                        return false;
                    }
                    let fingerprint = match root.chain_states.get_mut(&chain_id) {
                        Some(ChainUtxoState::Loading { progress: state }) => {
                            state.observe(progress);
                            Some(InitialCatchUpFingerprint::new(progress, None))
                        }
                        Some(ChainUtxoState::Syncing {
                            progress: state,
                            sync_tip,
                            ..
                        }) => {
                            // Once the session is installed, a commitment update can describe
                            // the forest's own live catch-up instead of this sync's remaining
                            // work, so the percent leaves it out as the stage display does. A
                            // chain that was synced a moment ago then doesn't read as half done.
                            // The percent can lag until the next wallet update because of it.
                            let progress = syncing_progress(progress);
                            state.observe(progress);
                            Some(InitialCatchUpFingerprint::new(progress, Some(*sync_tip)))
                        }
                        Some(ChainUtxoState::Ready { .. }) => None,
                        Some(ChainUtxoState::Idle | ChainUtxoState::Error { .. }) | None => {
                            return false;
                        }
                    };
                    if let Some(fingerprint) = fingerprint {
                        root.handle_initial_sync_observation(
                            active_wallet_generation,
                            chain_id,
                            InitialSyncObservation::Progress(fingerprint),
                        );
                    }
                    cx.notify();
                    true
                });
                if !matches!(should_continue, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        let observer_task = self
            .wallet_sync_lifecycle
            .register_installed_observer(&registration);
        let mut observer_cancel_rx = observer_task.cancel_rx;
        let observer_completed_tx = observer_task.completed_tx;
        let result_wallet_id = active_wallet_id;
        cx.spawn(async move |this, cx| {
            let _observer_completion = InstalledObserverCompletion(observer_completed_tx);
            let session = match result_rx.await {
                Ok(Ok(session)) => Arc::new(session),
                Ok(Err(error)) => {
                    let _ = this.update(cx, |root, cx| {
                        let is_current = root.is_current_chain_load_startup(
                            result_wallet_id.as_ref(),
                            active_wallet_generation,
                            chain_id,
                            lifecycle_generation,
                            chain_load_task_id,
                        );
                        root.finish_chain_load_startup(
                            chain_id,
                            lifecycle_generation,
                            chain_load_task_id,
                        );
                        if !is_current {
                            return;
                        }
                        root.http.rpc_broker().invalidate_block(chain_id);
                        let message = format_report_chain(&error);
                        tracing::error!(
                            chain_id,
                            error = %message,
                            "wallet chain sync startup failed"
                        );
                        root.chain_states.insert(
                            chain_id,
                            ChainUtxoState::Error {
                                message: Arc::from(message),
                                start_block: previous_start_block,
                                ppoi_workflow_status: WalletPpoiWorkflowStatus::default(),
                            },
                        );
                        root.handle_initial_sync_observation(
                            active_wallet_generation,
                            chain_id,
                            InitialSyncObservation::Error,
                        );
                        if root.last_evm_chain == chain_id {
                            root.sync_utxo_table(cx);
                        }
                        cx.notify();
                    });
                    return;
                }
                Err(error) => {
                    let _ = this.update(cx, |root, cx| {
                        let is_current = root.is_current_chain_load_startup(
                            result_wallet_id.as_ref(),
                            active_wallet_generation,
                            chain_id,
                            lifecycle_generation,
                            chain_load_task_id,
                        );
                        root.finish_chain_load_startup(
                            chain_id,
                            lifecycle_generation,
                            chain_load_task_id,
                        );
                        if !is_current {
                            return;
                        }
                        root.http.rpc_broker().invalidate_block(chain_id);
                        root.chain_states.insert(
                            chain_id,
                            ChainUtxoState::Error {
                                message: Arc::from(format!("wallet UTXO task failed: {error}")),
                                start_block: previous_start_block,
                                ppoi_workflow_status: WalletPpoiWorkflowStatus::default(),
                            },
                        );
                        root.handle_initial_sync_observation(
                            active_wallet_generation,
                            chain_id,
                            InitialSyncObservation::Error,
                        );
                        if root.last_evm_chain == chain_id {
                            root.sync_utxo_table(cx);
                        }
                        cx.notify();
                    });
                    return;
                }
            };

            let is_current = this
                .update(cx, |root, _cx| {
                    let is_current = root.is_current_chain_load_startup(
                        result_wallet_id.as_ref(),
                        active_wallet_generation,
                        chain_id,
                        lifecycle_generation,
                        chain_load_task_id,
                    );
                    if !is_current {
                        root.finish_chain_load_startup(
                            chain_id,
                            lifecycle_generation,
                            chain_load_task_id,
                        );
                    }
                    is_current
                })
                .unwrap_or(false);
            if !is_current {
                if let Err(error) = session.stop().await {
                    tracing::warn!(chain_id, %error, "failed to stop stale wallet sync session");
                }
                return;
            }

            let mut observation_rx = session.observation_rx.clone();
            let initial_observation = observation_rx.borrow_and_update().clone();
            let mut sync_tip_rx = Some(session.sync_tip_rx.clone());
            let mut poi_refreshing_rx = Some(session.poi_refreshing_rx.clone());
            let mut poi_artifact_cache_progress_rx =
                session.poi_artifact_cache_progress_rx.clone();
            let initial_snapshot = initial_observation.snapshot.clone();
            let initial_readiness = wallet_readiness_disposition(&initial_observation.readiness);
            let initial_ppoi_workflow_status = initial_observation.ppoi_workflow_status;
            let mut last_ppoi_workflow_status = initial_ppoi_workflow_status;
            let initial_sync_tip = *session.sync_tip_rx.borrow();
            let initial_poi_refreshing = *session.poi_refreshing_rx.borrow();
            let initial_poi_artifact_cache_progress = poi_artifact_cache_progress_rx
                .as_ref()
                .and_then(|rx| rx.borrow().get(&chain_id).cloned());

            let installed = this.update(cx, |root, cx| {
                if !root.is_current_chain_load_startup(
                    result_wallet_id.as_ref(),
                    active_wallet_generation,
                    chain_id,
                    lifecycle_generation,
                    chain_load_task_id,
                ) {
                    root.finish_chain_load_startup(
                        chain_id,
                        lifecycle_generation,
                        chain_load_task_id,
                    );
                    return false;
                }
                root.wallet_sync_lifecycle.finish_startup_after_session_installation(
                    chain_id,
                    lifecycle_generation,
                    chain_load_task_id,
                );
                // The sync's percent carries over from the startup.
                let mut progress = match root.chain_states.get(&chain_id) {
                    Some(
                        ChainUtxoState::Loading { progress }
                        | ChainUtxoState::Syncing { progress, .. },
                    ) => *progress,
                    Some(
                        ChainUtxoState::Idle
                        | ChainUtxoState::Ready { .. }
                        | ChainUtxoState::Error { .. },
                    )
                    | None => ChainSyncProgress::default(),
                };
                progress.observe(syncing_progress(progress.latest()));
                let state = match initial_readiness.clone() {
                    WalletReadinessDisposition::Ready => ChainUtxoState::Ready {
                        snapshot: initial_snapshot.clone(),
                        session: session.clone(),
                        observer_token: observer_token.clone(),
                        sync_tip: initial_sync_tip,
                        poi_refreshing: initial_poi_refreshing,
                        ppoi_workflow_status: initial_ppoi_workflow_status,
                    },
                    WalletReadinessDisposition::Syncing => ChainUtxoState::Syncing {
                        snapshot: initial_snapshot.clone(),
                        progress,
                        catching_up: false,
                        session: session.clone(),
                        observer_token: observer_token.clone(),
                        sync_tip: initial_sync_tip,
                        poi_refreshing: initial_poi_refreshing,
                        ppoi_workflow_status: initial_ppoi_workflow_status,
                    },
                    WalletReadinessDisposition::Error(message) => ChainUtxoState::Error {
                        message,
                        start_block: Some(session.start_block),
                        ppoi_workflow_status: initial_ppoi_workflow_status,
                    },
                };
                let initially_ready = matches!(&state, ChainUtxoState::Ready { .. });
                let initially_failed = matches!(&state, ChainUtxoState::Error { .. });
                let initial_sync_fingerprint = match &state {
                    ChainUtxoState::Syncing {
                        progress, sync_tip, ..
                    } => Some(InitialCatchUpFingerprint::new(
                        progress.latest(),
                        Some(*sync_tip),
                    )),
                    ChainUtxoState::Idle
                    | ChainUtxoState::Loading { .. }
                    | ChainUtxoState::Ready { .. }
                    | ChainUtxoState::Error { .. } => None,
                };
                // A session can install with a head already observed, and the
                // sync-tip stream only reports later transitions. Seed the broker
                // here so its block epoch is established before the first read.
                let installed_head_block = state.sync_tip().and_then(|tip| tip.head_block);
                root.chain_states.insert(chain_id, state);
                if let Some(installed_head_block) = installed_head_block {
                    root.http
                        .rpc_broker()
                        .notify_block(chain_id, installed_head_block);
                }
                if let Some(fingerprint) = initial_sync_fingerprint {
                    root.handle_initial_sync_observation(
                        active_wallet_generation,
                        chain_id,
                        InitialSyncObservation::Progress(fingerprint),
                    );
                }
                if initially_ready {
                    root.handle_initial_sync_observation(
                        active_wallet_generation,
                        chain_id,
                        InitialSyncObservation::Ready,
                    );
                    root.init_wallet_activity_last_seen(chain_id);
                }
                if initially_failed {
                    root.http.rpc_broker().invalidate_block(chain_id);
                    root.handle_initial_sync_observation(
                        active_wallet_generation,
                        chain_id,
                        InitialSyncObservation::Error,
                    );
                }
                if let Some(progress) = initial_poi_artifact_cache_progress.clone() {
                    root.poi_artifact_cache_progress.insert(chain_id, progress);
                } else {
                    root.poi_artifact_cache_progress.remove(&chain_id);
                }
                if root.last_evm_chain == chain_id {
                    root.sync_utxo_table(cx);
                    root.focus_utxo_table_on_render = should_focus_utxo_table(
                        root.active_activity,
                        root.active_wallet_tab,
                        root.chain_states.get(&chain_id),
                    );
                }
                cx.notify();
                true
            });
            if !matches!(installed, Ok(true)) {
                if let Err(error) = session.stop().await {
                    tracing::warn!(chain_id, %error, "failed to stop stale wallet sync session");
                }
                return;
            }
            if matches!(initial_observation.readiness, WalletReadiness::Failed(_)) {
                if let Err(error) = session.stop().await {
                    tracing::warn!(chain_id, %error, "failed to stop failed wallet sync session");
                }
                return;
            }
            if initial_observation.readiness == WalletReadiness::Shutdown {
                let _ = this.update(cx, |root, _cx| {
                    root.invalidate_block_for_current_observer(
                        result_wallet_id.as_ref(),
                        active_wallet_generation,
                        chain_id,
                        &observer_token,
                    );
                });
                return;
            }

            loop {
                tokio::select! {
                    changed = observer_cancel_rx.changed() => {
                        if changed.is_err() || *observer_cancel_rx.borrow() {
                            let _ = this.update(cx, |root, _cx| {
                                root.invalidate_block_for_current_observer(
                                    result_wallet_id.as_ref(),
                                    active_wallet_generation,
                                    chain_id,
                                    &observer_token,
                                );
                            });
                            if let Err(error) = session.stop().await {
                                tracing::warn!(
                                    chain_id,
                                    %error,
                                    "failed to stop cancelled wallet sync observer session"
                                );
                            }
                            break;
                        }
                    }
                    changed = observation_rx.changed() => {
                        if changed.is_err() {
                            let session_to_stop = this
                                .update(cx, |root, cx| {
                                    root.transition_current_installed_observer_to_error(
                                        result_wallet_id.as_ref(),
                                        active_wallet_generation,
                                        chain_id,
                                        lifecycle_generation,
                                        chain_load_task_id,
                                        &observer_token,
                                        None,
                                        Arc::from("wallet session observation stream closed"),
                                        cx,
                                    )
                                })
                                .ok()
                                .flatten();
                            if let Some(session_to_stop) = session_to_stop
                                && let Err(error) = session_to_stop.stop().await
                            {
                                tracing::warn!(
                                    chain_id,
                                    %error,
                                    "failed to stop wallet sync session after observation stream closure"
                                );
                            }
                            break;
                        }
                        let observation = observation_rx.borrow_and_update().clone();
                        let latest_poi_refreshing = poi_refreshing_rx
                            .as_ref()
                            .map(|rx| *rx.borrow());
                        let ppoi_workflow_status = observation.ppoi_workflow_status;
                        let validation_completed = ppoi_validation_completion_is_current(
                            last_ppoi_workflow_status,
                            ppoi_workflow_status,
                            true,
                        );
                        let disposition = wallet_readiness_disposition(&observation.readiness);
                        if let WalletReadinessDisposition::Error(message) = &disposition {
                            let session_to_stop = this
                                .update(cx, |root, cx| {
                                    let session = root.transition_current_installed_observer_to_error(
                                        result_wallet_id.as_ref(),
                                        active_wallet_generation,
                                        chain_id,
                                        lifecycle_generation,
                                        chain_load_task_id,
                                        &observer_token,
                                        Some(ppoi_workflow_status),
                                        Arc::clone(message),
                                        cx,
                                    );
                                    if session.is_some() && validation_completed {
                                        root.pending_ppoi_validation_toast = Some((
                                            Arc::clone(&result_wallet_id),
                                            active_wallet_generation,
                                        ));
                                    }
                                    session
                                })
                                .ok()
                                .flatten();
                            if let Some(session_to_stop) = session_to_stop
                                && let Err(error) = session_to_stop.stop().await
                            {
                                tracing::warn!(
                                    chain_id,
                                    %error,
                                    "failed to stop terminal wallet sync session"
                                );
                            }
                            break;
                        }
                        let ready = matches!(disposition, WalletReadinessDisposition::Ready);
                        let snapshot = observation.snapshot;
                        let should_continue = this.update(cx, |root, cx| {
                            if !root.is_current_installed_observer(
                                result_wallet_id.as_ref(),
                                active_wallet_generation,
                                chain_id,
                                &observer_token,
                            ) {
                                return false;
                            }
                            let Some(state) = root.chain_states.remove(&chain_id) else {
                                return false;
                            };
                            let became_ready =
                                ready && matches!(&state, ChainUtxoState::Syncing { .. });
                            let mut state = match state {
                                ChainUtxoState::Syncing {
                                    progress,
                                    session,
                                    observer_token,
                                    sync_tip,
                                    poi_refreshing,
                                    ..
                                } if ready => {
                                    root.finish_chain_load_startup(
                                        chain_id,
                                        lifecycle_generation,
                                        chain_load_task_id,
                                    );
                                    let _ = progress;
                                    ChainUtxoState::Ready {
                                        snapshot: snapshot.clone(),
                                        session,
                                        observer_token,
                                        sync_tip,
                                        poi_refreshing,
                                        ppoi_workflow_status,
                                    }
                                }
                                ChainUtxoState::Syncing {
                                    progress,
                                    catching_up,
                                    session,
                                    observer_token,
                                    sync_tip,
                                    poi_refreshing,
                                    ..
                                } => ChainUtxoState::Syncing {
                                    snapshot: snapshot.clone(),
                                    progress,
                                    catching_up,
                                    session,
                                    observer_token,
                                    sync_tip,
                                    poi_refreshing,
                                    ppoi_workflow_status,
                                },
                                ChainUtxoState::Ready {
                                    session,
                                    observer_token,
                                    sync_tip,
                                    poi_refreshing,
                                    ..
                                } if ready => ChainUtxoState::Ready {
                                    snapshot: snapshot.clone(),
                                    session,
                                    observer_token,
                                    sync_tip,
                                    poi_refreshing,
                                    ppoi_workflow_status,
                                },
                                ChainUtxoState::Ready {
                                    session,
                                    observer_token,
                                    sync_tip,
                                    poi_refreshing,
                                    ..
                                } => ChainUtxoState::Syncing {
                                    snapshot: snapshot.clone(),
                                    progress: ChainSyncProgress::default(),
                                    catching_up: true,
                                    session,
                                    observer_token,
                                    sync_tip,
                                    poi_refreshing,
                                    ppoi_workflow_status,
                                },
                                state @ (ChainUtxoState::Idle
                                | ChainUtxoState::Loading { .. }
                                | ChainUtxoState::Error { .. }) => {
                                    root.chain_states.insert(chain_id, state);
                                    return false;
                                }
                            };
                            if let Some(latest_poi_refreshing) = latest_poi_refreshing {
                                state.set_poi_refreshing(latest_poi_refreshing);
                            }
                            root.chain_states.insert(chain_id, state);
                            if ready {
                                root.handle_initial_sync_observation(
                                    active_wallet_generation,
                                    chain_id,
                                    InitialSyncObservation::Ready,
                                );
                                root.init_wallet_activity_last_seen(chain_id);
                            }
                            if validation_completed {
                                root.pending_ppoi_validation_toast = Some((
                                    Arc::clone(&result_wallet_id),
                                    active_wallet_generation,
                                ));
                            }
                            root.refresh_open_form_assets_for_snapshot(&snapshot, cx);
                            root.refresh_private_swap_assets(chain_id, cx);
                            if became_ready {
                                root.reschedule_ready_public_broadcaster_cost_estimates(chain_id, cx);
                            }
                            if root.last_evm_chain == chain_id {
                                root.sync_utxo_table(cx);
                            }
                            cx.notify();
                            true
                        });
                        if !matches!(should_continue, Ok(true)) {
                            break;
                        }
                        last_ppoi_workflow_status = ppoi_workflow_status;
                    }
                    changed = async {
                        match sync_tip_rx.as_mut() {
                            Some(rx) => rx.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if !retain_auxiliary_stream(&mut sync_tip_rx, &changed) {
                            let _ = this.update(cx, |root, _cx| {
                                root.invalidate_block_for_current_observer(
                                    result_wallet_id.as_ref(),
                                    active_wallet_generation,
                                    chain_id,
                                    &observer_token,
                                );
                            });
                            continue;
                        }
                        let sync_tip = *sync_tip_rx
                            .as_ref()
                            .expect("sync-tip receiver is present after successful change")
                            .borrow();
                        let should_continue = this.update(cx, |root, cx| {
                            if !root.is_current_installed_observer(
                                result_wallet_id.as_ref(),
                                active_wallet_generation,
                                chain_id,
                                &observer_token,
                            ) {
                                return false;
                            }
                            let has_installed_chain_state = root
                                .chain_states
                                .get(&chain_id)
                                .is_some_and(|state| {
                                    matches!(state, ChainUtxoState::Syncing { .. } | ChainUtxoState::Ready { .. })
                                });
                            if has_installed_chain_state
                                && let Some(new_head_block) = sync_tip.head_block
                            {
                                root.http.rpc_broker().notify_block(chain_id, new_head_block);
                            }
                            let Some(state) = root.chain_states.get_mut(&chain_id) else {
                                return false;
                            };
                            let fingerprint = match state {
                                ChainUtxoState::Syncing { sync_tip: state, .. } => {
                                    *state = sync_tip;
                                    Some(InitialCatchUpFingerprint::new(None, Some(sync_tip)))
                                }
                                ChainUtxoState::Ready { sync_tip: state, .. } => {
                                    *state = sync_tip;
                                    None
                                }
                                ChainUtxoState::Idle
                                | ChainUtxoState::Loading { .. }
                                | ChainUtxoState::Error { .. } => return false,
                            };
                            if let Some(fingerprint) = fingerprint {
                                root.handle_initial_sync_observation(
                                    active_wallet_generation,
                                    chain_id,
                                    InitialSyncObservation::Progress(fingerprint),
                                );
                            }
                            if root.last_evm_chain == chain_id {
                                root.sync_utxo_finality_context(cx);
                            }
                            cx.notify();
                            true
                        });
                        if !matches!(should_continue, Ok(true)) {
                            break;
                        }
                    }
                    changed = async {
                        match poi_refreshing_rx.as_mut() {
                            Some(rx) => rx.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if !retain_auxiliary_stream(&mut poi_refreshing_rx, &changed) {
                            continue;
                        }
                        let poi_refreshing = *poi_refreshing_rx
                            .as_ref()
                            .expect("POI-refresh receiver is present after successful change")
                            .borrow();
                        let should_continue = this.update(cx, |root, cx| {
                            if !root.is_current_installed_observer(
                                result_wallet_id.as_ref(),
                                active_wallet_generation,
                                chain_id,
                                &observer_token,
                            ) {
                                return false;
                            }
                            let Some(state) = root.chain_states.get_mut(&chain_id) else {
                                return false;
                            };
                            if !matches!(state, ChainUtxoState::Syncing { .. } | ChainUtxoState::Ready { .. }) {
                                return false;
                            }
                            state.set_poi_refreshing(poi_refreshing);
                            if root.last_evm_chain == chain_id {
                                root.sync_utxo_poi_refreshing(poi_refreshing, cx);
                            }
                            cx.notify();
                            true
                        });
                        if !matches!(should_continue, Ok(true)) {
                            break;
                        }
                    }
                    changed = async {
                        match poi_artifact_cache_progress_rx.as_mut() {
                            Some(rx) => rx.changed().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if !retain_auxiliary_stream(&mut poi_artifact_cache_progress_rx, &changed) {
                            continue;
                        }
                        let progress = poi_artifact_cache_progress_rx
                            .as_ref()
                            .and_then(|rx| rx.borrow().get(&chain_id).cloned());
                        let should_continue = this.update(cx, |root, cx| {
                            if !root.is_current_installed_observer(
                                result_wallet_id.as_ref(),
                                active_wallet_generation,
                                chain_id,
                                &observer_token,
                            ) {
                                return false;
                            }
                            if let Some(progress) = progress.clone() {
                                root.poi_artifact_cache_progress.insert(chain_id, progress);
                            } else {
                                root.poi_artifact_cache_progress.remove(&chain_id);
                            }
                            cx.notify();
                            true
                        });
                        if !matches!(should_continue, Ok(true)) {
                            break;
                        }
                    }
                }
            }

            let _ = this.update(cx, |root, _cx| {
                root.finish_chain_load_startup(chain_id, lifecycle_generation, chain_load_task_id);
            });
        })
        .detach();
    }

    pub(super) fn install_chain_background(&mut self) {
        if self.wallet_sync_lifecycle_shutdown_started {
            return;
        }
        self.rpc_identity_warmup = Some(wallet_ops::start_rpc_identity_warmup(
            &self.http,
            &self.effective_chain_configs,
            self.last_evm_chain,
        ));
        let chains = self
            .effective_chain_configs
            .enabled_chains()
            .filter(|chain| {
                chain.require_railgun().is_ok()
                    || chain.chain_id == self.last_evm_chain
                    || self.active_rpc_chains.contains(&chain.chain_id)
                    || self
                        .public_transaction_tracker
                        .has_pending_observation(chain.chain_id)
            })
            .map(|chain| chain.chain_id)
            .collect::<Vec<_>>();
        self.active_rpc_chains.clear();
        // The new warmup hasn't seen Tron's route yet.
        self.tron_public.route_warmed = false;
        self.activate_chains_background(chains);
    }

    pub(super) fn activate_chain_background(&mut self, chain_id: u64) {
        self.activate_chains_background([chain_id]);
    }

    pub(super) fn activate_chains_background(&mut self, chains: impl IntoIterator<Item = u64>) {
        if self.wallet_sync_lifecycle_shutdown_started {
            return;
        }
        for chain_id in chains {
            let Ok(chain) = self.effective_chain_configs.enabled(chain_id) else {
                continue;
            };
            if self.active_rpc_chains.insert(chain_id)
                && let Some(warmup) = &self.rpc_identity_warmup
            {
                warmup.activate_route(chain.rpc_route.clone());
            }
        }
        self.sync_active_pricing_chains();
    }

    /// Native USD sources refresh for the active EVM chains, and for Tron once it has been
    /// selected this session. A custom EVM chain with Tron's chain id never activates Tron's
    /// pricing.
    pub(super) fn sync_active_pricing_chains(&mut self) {
        let tron_id = wallet_ops::tron::TRON_CHAIN_ID;
        let mut chains: Vec<u64> = self
            .active_rpc_chains
            .iter()
            .copied()
            .filter(|&chain_id| chain_id != tron_id)
            .collect();
        if self.tron_public.activated
            && let Some(route) = &self.tron_public.route
        {
            chains.push(tron_id);
            if !self.tron_public.route_warmed
                && let Some(warmup) = &self.rpc_identity_warmup
            {
                warmup.activate_route(route.clone());
                self.tron_public.route_warmed = true;
            }
        }
        self.public_broadcaster_anchor_refresh
            .set_active_chains(&chains);
    }

    pub(super) fn select_chain(
        &mut self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.effective_chain_configs.enabled(chain_id).is_err() {
            return;
        }
        self.show_network_in_selector(SelectedNetwork::Evm(chain_id), window, cx);
        if self.selected_network == SelectedNetwork::Evm(chain_id) {
            return;
        }
        window.close_all_dialogs(cx);
        self.selected_network = SelectedNetwork::Evm(chain_id);
        self.last_evm_chain = chain_id;
        self.activate_chain_background(chain_id);
        if let Some(warmup) = &self.rpc_identity_warmup {
            warmup.prioritize_chain(chain_id);
        }
        if !self.selected_chain_has_railgun() {
            self.active_wallet_tab = WalletTab::Public;
        }
        self.invalidate_proposals_chain(chain_id);
        self.invalidate_governance_context();
        self.ui_state.last_chain_id = Some(chain_id);
        self.save_ui_state();
        self.sync_broadcaster_monitor_chain_filter(chain_id, window, cx);
        self.clear_private_action_state();
        self.clear_public_chain_balance_state();
        self.restore_public_balance_snapshot();
        self.sync_utxo_table(cx);
        if self.active_wallet_tab == WalletTab::Public || self.gateway_has_connected_browser() {
            self.schedule_automatic_public_balance_refresh(cx);
        }
        self.publish_gateway_desktop_state();
        if should_focus_utxo_table(
            self.active_activity,
            self.active_wallet_tab,
            self.chain_states.get(&chain_id),
        ) {
            self.focus_utxo_table_on_render = true;
        }
        if self.view_session.is_some() {
            self.ensure_chain_load(chain_id, cx);
        }
        if self.active_activity == super::sidebar::Activity::Proposals {
            match self.governance.tab {
                super::governance::GovernanceTab::Proposals => {
                    self.start_proposals_refresh(false, cx);
                }
                super::governance::GovernanceTab::Staking => self.start_staking_refresh(cx),
            }
        }
        cx.notify();
    }

    /// Selects Tron without persisting it. Chain loads, private sync, balances and the gateway
    /// keep following `last_evm_chain`.
    pub(super) fn select_tron(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.tron_enabled() {
            return;
        }
        self.show_network_in_selector(SelectedNetwork::Tron, window, cx);
        if self.selected_network == SelectedNetwork::Tron {
            return;
        }
        window.close_all_dialogs(cx);
        self.selected_network = SelectedNetwork::Tron;
        self.activate_tron();
        self.reset_public_asset_focus();
        self.reconcile_public_account_selection();
        self.schedule_automatic_public_balance_refresh(cx);
        self.active_wallet_tab = WalletTab::Public;
        self.invalidate_proposals_chain(self.last_evm_chain);
        self.invalidate_governance_context();
        self.clear_private_action_state();
        self.refresh_wallet_activity(cx);
        if self.active_activity == super::sidebar::Activity::Proposals
            && self.governance.tab == super::governance::GovernanceTab::Proposals
        {
            self.start_proposals_refresh(false, cx);
        }
        cx.notify();
    }

    /// Drops the private forms and broadcaster state of the network being left.
    fn clear_private_action_state(&mut self) {
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.private_action_form = None;
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        self.local_pending_spent_clear_confirming = false;
    }

    fn show_network_in_selector(
        &self,
        network: SelectedNetwork,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.chain_select.read(cx).selected_value() != Some(&network) {
            self.chain_select.update(cx, |select, cx| {
                select.set_selected_value(&network, window, cx);
                cx.notify();
            });
        }
    }

    pub(super) fn sync_broadcaster_monitor_chain_filter(
        &self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.monitor.update(cx, |monitor, cx| {
            monitor.set_chain_filter(chain_id, window, cx);
        });
    }
}
