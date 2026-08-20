use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use broadcaster_monitor::{EventTx, Shared, publish_revision};
use gpui::{
    AnyElement, Context, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{WindowExt, progress::Progress as UiProgress};
use tokio::runtime::Handle;
use tokio::sync::{OnceCell, oneshot, watch};
use ui::theme::{self, APP_TEXT_SIZE};
use wallet_ops::{
    DesktopWalletSyncStartPolicy, HttpContext, ListUtxosOutput, PoiArtifactCacheProgress,
    SyncProgressStage, SyncProgressUnit, SyncProgressUpdate, ViewWalletChainSessionRequest,
    WalletPpoiWorkflowStatus, WalletReadiness, WalletSessionStore, WalletSyncTip,
    vault::WalletSource,
};

use super::WakuWorkerCompletionToken;
use super::utxo::should_focus_utxo_table;
use super::{
    BroadcasterActivityTab, InitialCatchUpFingerprint, InitialSyncObservation, WalletRoot,
    WalletTab, count_label, format_report_chain,
};

pub(super) enum ChainUtxoState {
    Idle,
    Loading {
        progress: Option<SyncProgressUpdate>,
    },
    Syncing {
        snapshot: Arc<ListUtxosOutput>,
        progress: Option<SyncProgressUpdate>,
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

const WALLET_SYNC_STARTUP_SUPERSEDED: &str = "wallet sync startup superseded";

// Allocation identity keeps same-wallet, same-generation replacement sessions distinct.
#[derive(Clone, Debug)]
pub(super) struct InstalledObserverToken {
    chain_id: u64,
    identity: Arc<()>,
}

impl InstalledObserverToken {
    fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            identity: Arc::new(()),
        }
    }
}

impl PartialEq for InstalledObserverToken {
    fn eq(&self, other: &Self) -> bool {
        self.chain_id == other.chain_id && Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl Eq for InstalledObserverToken {}

pub(super) struct WalletSyncLifecycle {
    generation: Arc<AtomicU64>,
    next_task_id: u64,
    session_store: Arc<OnceCell<Arc<WalletSessionStore>>>,
    startup_tasks: BTreeMap<u64, WalletSyncStartupTask>,
    wallet_tasks: Vec<tokio::task::JoinHandle<()>>,
    current_task_by_chain: BTreeMap<u64, u64>,
    installed_observers: BTreeMap<u64, InstalledObserverControl>,
}

pub(super) struct WalletSyncStartupRegistration {
    pub(super) chain_id: u64,
    pub(super) generation: u64,
    pub(super) task_id: u64,
    pub(super) observer_token: InstalledObserverToken,
    pub(super) generation_token: Arc<AtomicU64>,
    pub(super) session_store: Arc<OnceCell<Arc<WalletSessionStore>>>,
}

struct WalletSyncStartupTask {
    chain_id: u64,
    generation: u64,
    task_id: u64,
    join: tokio::task::JoinHandle<()>,
}

struct InstalledObserverControl {
    chain_id: u64,
    cancel_tx: watch::Sender<bool>,
    completed_rx: watch::Receiver<bool>,
}

pub(super) struct InstalledObserverTaskRegistration {
    pub(super) cancel_rx: watch::Receiver<bool>,
    pub(super) completed_tx: watch::Sender<bool>,
}

struct InstalledObserverCompletion(watch::Sender<bool>);

impl Drop for InstalledObserverCompletion {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

pub(super) struct WalletSyncLifecycleCleanup {
    startup_tasks: Vec<WalletSyncStartupTask>,
    wallet_tasks: Vec<tokio::task::JoinHandle<()>>,
    installed_observers: Vec<InstalledObserverControl>,
    session_store: Option<Arc<OnceCell<Arc<WalletSessionStore>>>>,
}

#[derive(Clone)]
pub(super) struct WalletSyncLifecycleCleanupTask {
    completed_rx: watch::Receiver<Option<WalletSyncLifecycleCleanupReport>>,
}

#[derive(Clone)]
pub(super) struct WalletSyncLifecycleCleanupWaitGroup {
    tasks: Vec<WalletSyncLifecycleCleanupTask>,
}

#[derive(Clone)]
pub(super) struct WalletRootReplacementCleanup {
    completed_rx: watch::Receiver<Option<Result<WalletSyncLifecycleCleanupReport, String>>>,
}

pub(super) struct WalletPublicSyncCacheResetContext {
    cleanup: WalletSyncLifecycleCleanupWaitGroup,
    session_store: Arc<OnceCell<Arc<WalletSessionStore>>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct WalletSyncLifecycleCleanupReport {
    pub(super) stopped_startup_tasks: usize,
    pub(super) failed_startup_tasks: usize,
    pub(super) shut_down_session_store: bool,
}

impl WalletSyncLifecycle {
    pub(super) fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            next_task_id: 0,
            session_store: Arc::new(OnceCell::new()),
            startup_tasks: BTreeMap::new(),
            wallet_tasks: Vec::new(),
            current_task_by_chain: BTreeMap::new(),
            installed_observers: BTreeMap::new(),
        }
    }

    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(super) fn public_sync_cache_reset_cell(&self) -> Arc<OnceCell<Arc<WalletSessionStore>>> {
        Arc::clone(&self.session_store)
    }

    pub(super) fn prepare_startup(&mut self, chain_id: u64) -> WalletSyncStartupRegistration {
        self.prune_completed_observers();
        let generation = self.current_generation();
        self.next_task_id = self.next_task_id.wrapping_add(1).max(1);
        let task_id = self.next_task_id;

        for task in self
            .startup_tasks
            .values()
            .filter(|task| task.chain_id == chain_id && task.generation == generation)
        {
            task.join.abort();
        }
        for observer in self
            .installed_observers
            .values()
            .filter(|observer| observer.chain_id == chain_id)
        {
            let _ = observer.cancel_tx.send(true);
        }
        self.current_task_by_chain.insert(chain_id, task_id);

        WalletSyncStartupRegistration {
            chain_id,
            generation,
            task_id,
            observer_token: InstalledObserverToken::new(chain_id),
            generation_token: Arc::clone(&self.generation),
            session_store: Arc::clone(&self.session_store),
        }
    }

    pub(super) fn register_installed_observer(
        &mut self,
        registration: &WalletSyncStartupRegistration,
    ) -> InstalledObserverTaskRegistration {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (completed_tx, completed_rx) = watch::channel(false);
        self.installed_observers.insert(
            registration.task_id,
            InstalledObserverControl {
                chain_id: registration.chain_id,
                cancel_tx,
                completed_rx,
            },
        );
        InstalledObserverTaskRegistration {
            cancel_rx,
            completed_tx,
        }
    }

    fn prune_completed_observers(&mut self) {
        self.installed_observers
            .retain(|_, observer| !*observer.completed_rx.borrow());
    }

    pub(super) fn track_startup(
        &mut self,
        registration: &WalletSyncStartupRegistration,
        join: tokio::task::JoinHandle<()>,
    ) {
        self.startup_tasks.insert(
            registration.task_id,
            WalletSyncStartupTask {
                chain_id: registration.chain_id,
                generation: registration.generation,
                task_id: registration.task_id,
                join,
            },
        );
    }

    pub(super) fn track_wallet_task(&mut self, join: tokio::task::JoinHandle<()>) {
        self.wallet_tasks.retain(|task| !task.is_finished());
        self.wallet_tasks.push(join);
    }

    pub(super) fn is_current_startup(&self, chain_id: u64, generation: u64, task_id: u64) -> bool {
        self.current_generation() == generation
            && self
                .current_task_by_chain
                .get(&chain_id)
                .is_some_and(|current| *current == task_id)
    }

    pub(super) fn finish_startup(&mut self, chain_id: u64, generation: u64, task_id: u64) {
        if self
            .startup_tasks
            .get(&task_id)
            .is_some_and(|task| task.chain_id == chain_id && task.generation == generation)
        {
            self.startup_tasks.remove(&task_id);
        }
        if self
            .current_task_by_chain
            .get(&chain_id)
            .is_some_and(|current| *current == task_id)
        {
            self.current_task_by_chain.remove(&chain_id);
        }
    }

    pub(super) fn finish_startup_after_session_installation(
        &mut self,
        chain_id: u64,
        generation: u64,
        task_id: u64,
    ) {
        if self
            .startup_tasks
            .get(&task_id)
            .is_some_and(|task| task.chain_id == chain_id && task.generation == generation)
        {
            self.startup_tasks.remove(&task_id);
        }
        if self
            .current_task_by_chain
            .get(&chain_id)
            .is_some_and(|current| *current == task_id)
        {
            self.current_task_by_chain.remove(&chain_id);
        }
    }

    pub(super) fn invalidate(&mut self) -> WalletSyncLifecycleCleanup {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.current_task_by_chain.clear();
        let startup_tasks = std::mem::take(&mut self.startup_tasks)
            .into_values()
            .collect::<Vec<_>>();
        let wallet_tasks = std::mem::take(&mut self.wallet_tasks);
        let session_store = std::mem::replace(&mut self.session_store, Arc::new(OnceCell::new()));
        let installed_observers = std::mem::take(&mut self.installed_observers)
            .into_values()
            .collect::<Vec<_>>();
        for task in &startup_tasks {
            task.join.abort();
        }
        for task in &wallet_tasks {
            task.abort();
        }
        for observer in &installed_observers {
            let _ = observer.cancel_tx.send(true);
        }
        WalletSyncLifecycleCleanup {
            startup_tasks,
            wallet_tasks,
            installed_observers,
            session_store: Some(session_store),
        }
    }

    pub(super) fn supersede_wallet(&mut self) -> WalletSyncLifecycleCleanup {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.current_task_by_chain.clear();
        let startup_tasks = std::mem::take(&mut self.startup_tasks)
            .into_values()
            .collect::<Vec<_>>();
        let wallet_tasks = std::mem::take(&mut self.wallet_tasks);
        let installed_observers = std::mem::take(&mut self.installed_observers)
            .into_values()
            .collect::<Vec<_>>();
        for task in &startup_tasks {
            task.join.abort();
        }
        for task in &wallet_tasks {
            task.abort();
        }
        for observer in &installed_observers {
            let _ = observer.cancel_tx.send(true);
        }
        WalletSyncLifecycleCleanup {
            startup_tasks,
            wallet_tasks,
            installed_observers,
            session_store: None,
        }
    }
}

impl WalletSyncLifecycleCleanup {
    #[cfg(test)]
    pub(super) async fn shutdown(self) -> Result<WalletSyncLifecycleCleanupReport, String> {
        Ok(self.shutdown_inner().await)
    }

    pub(super) fn spawn(self, runtime: &Handle) -> WalletSyncLifecycleCleanupTask {
        let (completed_tx, completed_rx) = watch::channel(None);
        runtime.spawn(async move {
            let report = self.shutdown_inner().await;
            let _ = completed_tx.send(Some(report));
        });
        WalletSyncLifecycleCleanupTask { completed_rx }
    }

    async fn shutdown_inner(self) -> WalletSyncLifecycleCleanupReport {
        for task in &self.startup_tasks {
            task.join.abort();
        }
        for task in &self.wallet_tasks {
            task.abort();
        }

        let stopped_startup_tasks = self.startup_tasks.len() + self.wallet_tasks.len();
        let mut failed_startup_tasks = 0;
        for task in self.startup_tasks {
            match task.join.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    failed_startup_tasks += 1;
                    tracing::warn!(
                        chain_id = task.chain_id,
                        task_id = task.task_id,
                        %error,
                        "wallet sync startup task failed during cleanup"
                    );
                }
            }
        }
        for task in self.wallet_tasks {
            match task.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    failed_startup_tasks += 1;
                    tracing::warn!(%error, "wallet-scoped task failed during cleanup");
                }
            }
        }

        let observer_count = self.installed_observers.len();
        let observer_cleanup_started_at = Instant::now();
        tracing::debug!(
            observer_count,
            "wallet sync cleanup cancelling installed observers"
        );
        let mut completed_observer_count = 0;
        for (observer_index, mut observer) in self.installed_observers.into_iter().enumerate() {
            let _ = observer.cancel_tx.send(true);
            while !*observer.completed_rx.borrow() {
                tracing::debug!(
                    observer_index = observer_index + 1,
                    observer_count,
                    elapsed_ms = observer_cleanup_started_at.elapsed().as_millis(),
                    "wallet sync cleanup waiting for installed observer"
                );
                if observer.completed_rx.changed().await.is_err() {
                    break;
                }
            }
            if *observer.completed_rx.borrow() {
                completed_observer_count += 1;
            }
        }
        tracing::debug!(
            observer_count,
            completed_observer_count,
            elapsed_ms = observer_cleanup_started_at.elapsed().as_millis(),
            "wallet sync cleanup installed observers cancelled"
        );

        let shut_down_session_store = if let Some(store) = self
            .session_store
            .as_ref()
            .and_then(|session_store| session_store.get().cloned())
        {
            let session_store_shutdown_started_at = Instant::now();
            tracing::debug!("wallet sync cleanup shutting down session store");
            store.shutdown().await;
            tracing::debug!(
                elapsed_ms = session_store_shutdown_started_at.elapsed().as_millis(),
                "wallet sync cleanup session store shut down"
            );
            true
        } else {
            false
        };

        WalletSyncLifecycleCleanupReport {
            stopped_startup_tasks,
            failed_startup_tasks,
            shut_down_session_store,
        }
    }
}

impl WalletSyncLifecycleCleanupTask {
    #[cfg(test)]
    pub(super) fn closed_for_test() -> Self {
        let (completed_tx, completed_rx) = watch::channel(None);
        drop(completed_tx);
        Self { completed_rx }
    }

    #[cfg(test)]
    pub(super) fn channel_for_test() -> (
        Self,
        watch::Sender<Option<WalletSyncLifecycleCleanupReport>>,
    ) {
        let (completed_tx, completed_rx) = watch::channel(None);
        (Self { completed_rx }, completed_tx)
    }

    pub(super) fn is_finished(&self) -> bool {
        self.completed_rx.borrow().is_some() || self.completed_rx.has_changed().is_err()
    }

    async fn wait(mut self) -> Result<WalletSyncLifecycleCleanupReport, String> {
        loop {
            let report = { *self.completed_rx.borrow() };
            if let Some(report) = report {
                return Ok(report);
            }
            self.completed_rx
                .changed()
                .await
                .map_err(|_| "wallet sync cleanup task ended before completion".to_string())?;
        }
    }
}

impl WalletSyncLifecycleCleanupWaitGroup {
    pub(super) const fn new(tasks: Vec<WalletSyncLifecycleCleanupTask>) -> Self {
        Self { tasks }
    }

    pub(super) async fn shutdown_for_merkle_reset(
        self,
    ) -> Result<WalletSyncLifecycleCleanupReport, String> {
        self.wait().await
    }

    pub(super) async fn shutdown_for_wallet_deletion(
        self,
    ) -> Result<WalletSyncLifecycleCleanupReport, String> {
        self.wait().await
    }

    pub(super) async fn shutdown_for_wallet_replacement(
        self,
    ) -> Result<WalletSyncLifecycleCleanupReport, String> {
        self.wait().await
    }

    #[cfg(test)]
    pub(super) fn is_finished(&self) -> bool {
        self.tasks
            .iter()
            .all(WalletSyncLifecycleCleanupTask::is_finished)
    }

    async fn wait(self) -> Result<WalletSyncLifecycleCleanupReport, String> {
        let mut combined = WalletSyncLifecycleCleanupReport::default();
        for task in self.tasks {
            let report = task.wait().await?;
            combined.stopped_startup_tasks += report.stopped_startup_tasks;
            combined.failed_startup_tasks += report.failed_startup_tasks;
            combined.shut_down_session_store |= report.shut_down_session_store;
        }
        Ok(combined)
    }
}

impl WalletRootReplacementCleanup {
    pub(super) fn spawn(
        runtime: &Handle,
        sync_cleanup: WalletSyncLifecycleCleanupWaitGroup,
        waku_completion: Option<WakuWorkerCompletionToken>,
        monitor_state: Shared,
        monitor_event_tx: EventTx,
    ) -> Self {
        let (completed_tx, completed_rx) = watch::channel(None);
        runtime.spawn(async move {
            let (sync_result, waku_result) = tokio::join!(
                sync_cleanup.wait(),
                async {
                    match waku_completion {
                        Some(completion) => completion.wait().await,
                        None => Ok(()),
                    }
                },
            );

            if let Some(rev) = monitor_state.write().clear() {
                publish_revision(&monitor_event_tx, rev);
            }

            let result = match (sync_result, waku_result) {
                (Ok(report), Ok(())) => Ok(report),
                (Err(sync_error), Ok(())) => Err(format!(
                    "wallet sync cleanup failed during root replacement: {sync_error}"
                )),
                (Ok(_), Err(waku_error)) => Err(format!(
                    "Waku worker cleanup failed during root replacement: {waku_error}"
                )),
                (Err(sync_error), Err(waku_error)) => Err(format!(
                    "wallet sync cleanup failed during root replacement: {sync_error}; Waku worker cleanup failed during root replacement: {waku_error}"
                )),
            };
            let _ = completed_tx.send(Some(result));
        });
        Self { completed_rx }
    }

    pub(super) fn is_finished(&self) -> bool {
        self.completed_rx.borrow().is_some() || self.completed_rx.has_changed().is_err()
    }

    pub(super) async fn wait(mut self) -> Result<WalletSyncLifecycleCleanupReport, String> {
        loop {
            if let Some(result) = self.completed_rx.borrow().clone() {
                return result;
            }
            self.completed_rx
                .changed()
                .await
                .map_err(|_| "root replacement cleanup ended before completion".to_string())?;
        }
    }
}

impl WalletPublicSyncCacheResetContext {
    pub(super) async fn shutdown_for_public_reset(
        self,
    ) -> Result<Option<Arc<WalletSessionStore>>, String> {
        self.cleanup.wait().await?;
        Ok(self.session_store.get().cloned())
    }

    pub(super) async fn shutdown_for_poi_reset(self) -> Result<(), String> {
        self.cleanup.wait().await.map(|_| ())
    }
}

fn wallet_sync_startup_superseded(generation: &AtomicU64, expected: u64) -> bool {
    generation.load(Ordering::Acquire) != expected
}

fn wallet_sync_startup_superseded_error() -> eyre::Report {
    eyre::eyre!(WALLET_SYNC_STARTUP_SUPERSEDED)
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
            Self::Loading { progress } | Self::Syncing { progress, .. } => *progress,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SyncStatusContext {
    Loading,
    Syncing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SyncStatusLabels {
    pub(super) title: String,
    pub(super) percent: u8,
    pub(super) detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PresenceStatus {
    Healthy,
    Active,
    Error,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BalanceSyncIssue {
    HeadUnavailable,
    HeadStalled {
        stale_secs: u64,
        threshold_secs: u64,
    },
    Lagging {
        lag_blocks: u64,
        threshold_blocks: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct WalletStatusCounts {
    pub(super) pending_incoming_outputs: usize,
    pub(super) pending_outgoing_outputs: usize,
    pub(super) pending_poi_assets: usize,
    pub(super) recoverable_poi_outputs: usize,
    pub(super) blocked_shield_outputs: usize,
    pub(super) ppoi_workflow_status: WalletPpoiWorkflowStatus,
}

impl WalletStatusCounts {
    pub(super) fn ppoi_attention_count(self) -> usize {
        let workflow_attention = usize::try_from(
            self.ppoi_workflow_status
                .needs_attention
                .saturating_add(self.ppoi_workflow_status.recovery_needs_attention),
        )
        .unwrap_or(usize::MAX);
        self.recoverable_poi_outputs
            .saturating_add(workflow_attention)
            .saturating_add(self.blocked_shield_outputs)
    }

    pub(super) fn ppoi_status_count(self) -> usize {
        let workflow_count =
            usize::try_from(self.ppoi_workflow_status.outstanding_count()).unwrap_or(usize::MAX);
        workflow_count
            .saturating_add(self.recoverable_poi_outputs)
            .saturating_add(self.blocked_shield_outputs)
    }

    pub(super) const fn has_ppoi_blocking_checks(self) -> bool {
        self.pending_poi_assets > 0
            || self.recoverable_poi_outputs > 0
            || self.ppoi_workflow_status.has_outstanding()
    }
}

pub(super) fn balances_presence_status(
    syncing: bool,
    ready: bool,
    sync_tip: Option<WalletSyncTip>,
    block_time: Duration,
    now_secs: u64,
) -> PresenceStatus {
    if syncing {
        return PresenceStatus::Active;
    }
    if !ready {
        return PresenceStatus::Unknown;
    }
    if sync_tip.is_some_and(|tip| tip.indexed_catch_up.is_some()) {
        return PresenceStatus::Active;
    }
    match balance_sync_issue(sync_tip, block_time, now_secs) {
        None => PresenceStatus::Healthy,
        Some(BalanceSyncIssue::HeadUnavailable) => PresenceStatus::Unknown,
        Some(BalanceSyncIssue::HeadStalled { .. } | BalanceSyncIssue::Lagging { .. }) => {
            PresenceStatus::Active
        }
    }
}

pub(super) fn balance_sync_issue(
    sync_tip: Option<WalletSyncTip>,
    block_time: Duration,
    now_secs: u64,
) -> Option<BalanceSyncIssue> {
    let Some(sync_tip) = sync_tip else {
        return Some(BalanceSyncIssue::HeadUnavailable);
    };
    if sync_tip.head_block.is_none() {
        return Some(BalanceSyncIssue::HeadUnavailable);
    }
    let Some(safe_head_block) = sync_tip.safe_head_block else {
        return Some(BalanceSyncIssue::HeadUnavailable);
    };
    let Some(head_last_advanced_at) = sync_tip.head_last_advanced_at_unix_secs else {
        return Some(BalanceSyncIssue::HeadUnavailable);
    };

    let threshold_secs = balance_stale_timeout(block_time).as_secs();
    let stale_secs = now_secs.saturating_sub(head_last_advanced_at);
    if stale_secs > threshold_secs {
        return Some(BalanceSyncIssue::HeadStalled {
            stale_secs,
            threshold_secs,
        });
    }

    let Some(last_scanned_block) = sync_tip.last_scanned_block else {
        return Some(BalanceSyncIssue::HeadUnavailable);
    };
    let lag_blocks = safe_head_block.saturating_sub(last_scanned_block);
    let threshold_blocks = balance_lag_threshold_blocks(block_time);
    if lag_blocks > threshold_blocks {
        return Some(BalanceSyncIssue::Lagging {
            lag_blocks,
            threshold_blocks,
        });
    }

    None
}

pub(super) fn balance_stale_timeout(block_time: Duration) -> Duration {
    block_time.saturating_mul(10).max(Duration::from_secs(45))
}

pub(super) fn balance_lag_threshold_blocks(block_time: Duration) -> u64 {
    let threshold = balance_stale_timeout(block_time).as_nanos() / block_time.as_nanos().max(1);
    u64::try_from(threshold).unwrap_or(u64::MAX).max(2)
}

pub(super) fn ppoi_presence_status(
    refreshing: bool,
    source_available: bool,
    artifact_cache_expected: bool,
    artifact_progress: Option<&PoiArtifactCacheProgress>,
    counts: WalletStatusCounts,
) -> PresenceStatus {
    if !source_available {
        return PresenceStatus::Unknown;
    }

    if let Some(progress) = artifact_progress {
        if progress.is_error() {
            return if !progress.ready_for_wallet_checks && counts.has_ppoi_blocking_checks() {
                PresenceStatus::Error
            } else {
                PresenceStatus::Active
            };
        }
        if progress.is_active() {
            return PresenceStatus::Active;
        }
        if !progress.is_ready() {
            return PresenceStatus::Unknown;
        }
    } else if artifact_cache_expected {
        return if refreshing {
            PresenceStatus::Active
        } else {
            PresenceStatus::Unknown
        };
    }

    if refreshing {
        PresenceStatus::Active
    } else if counts.blocked_shield_outputs > 0
        || counts.ppoi_workflow_status.needs_attention > 0
        || counts.ppoi_workflow_status.recovery_needs_attention > 0
    {
        PresenceStatus::Error
    } else if counts.ppoi_status_count() > 0 {
        PresenceStatus::Active
    } else {
        PresenceStatus::Healthy
    }
}

pub(super) fn ready_wallet_status_labels(counts: WalletStatusCounts) -> SyncStatusLabels {
    let title = if counts.blocked_shield_outputs > 0 {
        "Private assets need attention"
    } else if counts.recoverable_poi_outputs > 0
        || counts.pending_poi_assets > 0
        || counts.pending_incoming_outputs > 0
    {
        "Not yet spendable"
    } else if counts.pending_outgoing_outputs > 0 {
        "Waiting for confirmation"
    } else {
        "Wallet ready"
    };
    SyncStatusLabels {
        title: title.to_string(),
        percent: 100,
        detail: ready_wallet_status_detail(counts),
    }
}

fn ready_wallet_status_detail(counts: WalletStatusCounts) -> String {
    if counts.blocked_shield_outputs > 0 {
        let verb = if counts.blocked_shield_outputs == 1 {
            " needs attention"
        } else {
            " need attention"
        };
        return count_label(counts.blocked_shield_outputs, "blocked Shield") + verb;
    }
    if counts.recoverable_poi_outputs > 0 {
        return if counts.recoverable_poi_outputs == 1 {
            "1 PPOI verification retry is available".to_string()
        } else {
            format!(
                "{} PPOI verification retries are available",
                counts.recoverable_poi_outputs
            )
        };
    }
    let mut parts = Vec::new();
    if counts.pending_incoming_outputs > 0 {
        parts.push(count_label(
            counts.pending_incoming_outputs,
            "incoming transfer",
        ));
    }
    if counts.pending_outgoing_outputs > 0 {
        parts.push(count_label(
            counts.pending_outgoing_outputs,
            "outgoing transfer",
        ));
    }
    if counts.pending_poi_assets > 0 {
        parts.push(format!(
            "{} awaiting PPOI verification",
            count_label(counts.pending_poi_assets, "asset")
        ));
    }
    if parts.is_empty() {
        "Private wallet synced and ready".to_string()
    } else {
        parts.join(" · ")
    }
}

impl SyncStatusContext {
    const fn fallback_title(self) -> &'static str {
        match self {
            Self::Loading => "Preparing wallet sync",
            Self::Syncing => "Checking wallet sync",
        }
    }

    const fn fallback_detail(self) -> &'static str {
        match self {
            Self::Loading => "Connecting to chain and loading local wallet state...",
            Self::Syncing => "Checking for new wallet events...",
        }
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

pub(super) fn loading_summary(progress: Option<SyncProgressUpdate>) -> String {
    progress.map_or_else(
        || "Preparing wallet sync...".to_string(),
        |progress| format!("{} · {}%", progress.stage.label(), progress.percent()),
    )
}

pub(super) fn sync_status_labels(
    context: SyncStatusContext,
    progress: Option<SyncProgressUpdate>,
) -> SyncStatusLabels {
    let progress = match context {
        SyncStatusContext::Loading => progress,
        SyncStatusContext::Syncing => syncing_progress(progress),
    };
    SyncStatusLabels {
        title: progress.map_or_else(
            || context.fallback_title().to_string(),
            |progress| progress.stage.label().to_string(),
        ),
        percent: progress.map_or(0, SyncProgressUpdate::percent),
        detail: progress.map_or_else(|| context.fallback_detail().to_string(), progress_detail),
    }
}

const fn syncing_progress(progress: Option<SyncProgressUpdate>) -> Option<SyncProgressUpdate> {
    match progress {
        Some(progress) if matches!(progress.stage, SyncProgressStage::SynchronizingCommitments) => {
            None
        }
        progress => progress,
    }
}

pub(super) fn sync_status_bar(
    context: SyncStatusContext,
    progress: Option<SyncProgressUpdate>,
    right_children: Vec<AnyElement>,
) -> gpui::Div {
    let labels = sync_status_labels(context, progress);
    wallet_status_bar(labels, true, true, right_children)
}

pub(super) fn ready_status_bar(
    counts: WalletStatusCounts,
    right_children: Vec<AnyElement>,
) -> gpui::Div {
    wallet_status_bar(
        ready_wallet_status_labels(counts),
        false,
        ready_wallet_status_shows_text(counts),
        right_children,
    )
}

pub(super) const fn ready_wallet_status_shows_text(_counts: WalletStatusCounts) -> bool {
    false
}

fn wallet_status_bar(
    labels: SyncStatusLabels,
    show_progress: bool,
    show_text: bool,
    right_children: Vec<AnyElement>,
) -> gpui::Div {
    div()
        .h(px(36.0))
        .flex_none()
        .flex()
        .items_center()
        .gap_3()
        .px(px(12.0))
        .bg(rgb(theme::SURFACE))
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .when(show_text, |bar| {
            bar.child(
                div()
                    .min_w(px(170.0))
                    .text_color(rgb(theme::TEXT))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::from(labels.title)),
            )
        })
        .when(show_progress, |bar| {
            bar.child(
                UiProgress::new()
                    .w(px(190.0))
                    .h(px(6.0))
                    .value(f32::from(labels.percent)),
            )
            .child(
                div()
                    .w(px(42.0))
                    .text_color(rgb(theme::INFO))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::from(format!("{}%", labels.percent))),
            )
        })
        .when(show_text, |bar| {
            bar.child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .text_size(APP_TEXT_SIZE)
                    .child(SharedString::from(labels.detail)),
            )
        })
        .when(!show_text, |bar| bar.child(div().flex_1()))
        .when(!right_children.is_empty(), |bar| {
            bar.child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .children(right_children),
            )
        })
}

pub(super) fn progress_detail(progress: SyncProgressUpdate) -> String {
    match progress.unit {
        SyncProgressUnit::Block => {}
        SyncProgressUnit::ArtifactPreparation => {
            return "Preparing artifact metadata...".to_string();
        }
        SyncProgressUnit::ArtifactChunk { completed, total } => {
            if total == 0 {
                return "Artifact chunks prepared".to_string();
            }
            if completed == 0 {
                return format!("Downloading {total} artifact chunks...");
            }
            let completed = completed.min(total);
            if completed == total {
                return "Artifact chunks ready".to_string();
            }
            if completed.saturating_add(1) == total {
                return format!("Fetching final artifact chunk ({completed} of {total} ready)...");
            }
            return format!("Artifact chunks ready: {completed} of {total}");
        }
        SyncProgressUnit::ArtifactApplied => {
            return match progress.stage {
                wallet_ops::SyncProgressStage::SynchronizingCommitments => {
                    "Commitment artifacts applied".to_string()
                }
                wallet_ops::SyncProgressStage::PreparingUtxoIndex => {
                    "UTXO index artifacts prepared".to_string()
                }
                wallet_ops::SyncProgressStage::IndexingUtxos => "Artifacts applied".to_string(),
            };
        }
        SyncProgressUnit::CommitmentTail => {
            let current = progress
                .current_block
                .max(progress.start_block)
                .min(progress.target_block);
            return format!(
                "Checking commitment tail: block {current} of {}",
                progress.target_block
            );
        }
    }
    let current = progress
        .current_block
        .max(progress.start_block)
        .min(progress.target_block);
    format!("Block {current} of {}", progress.target_block)
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
            .get(&self.selected_chain)
            .and_then(ChainUtxoState::start_block)
    }

    pub(super) fn selected_chain_poi_artifact_progress(&self) -> Option<&PoiArtifactCacheProgress> {
        self.poi_artifact_cache_progress.get(&self.selected_chain)
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
        self.clear_protected_software_seed_session(cx);
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preference_error = None;
        self.active_broadcaster_tab = BroadcasterActivityTab::default();
        self.clear_public_wallet_runtime_state();
        self.private_action_form = None;
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        self.blocked_shield_rescue_rows.clear();
        self.blocked_shield_refunds_in_flight.clear();
        self.blocked_shield_rescue_lookup_generation =
            self.blocked_shield_rescue_lookup_generation.wrapping_add(1);
        self.pending_ppoi_validation_toast = None;
        self.active_wallet_tab = WalletTab::default();
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
        if restart_safe && self.view_session.is_some() {
            self.ensure_chain_load(self.selected_chain, cx);
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
    ) -> (WalletRootReplacementCleanup, HttpContext) {
        self.advance_active_wallet_generation();
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        self.revealed_passphrase_context_id = None;
        self.wallet_sync_lifecycle_shutdown_started = true;
        let waku_completion = self.stop_waku_for_root_replacement();
        let cleanup = self.wallet_sync_lifecycle.invalidate();
        let outgoing_http = self.http.clone();
        self.reset_wallet_scoped_state(cx);
        self.start_wallet_sync_cleanup(cleanup);
        let sync_cleanup = self.wallet_sync_cleanup_wait_group();
        let root_replacement_cleanup = WalletRootReplacementCleanup::spawn(
            &self.runtime,
            sync_cleanup,
            waku_completion,
            self.monitor_state.clone(),
            self.monitor_event_tx.clone(),
        );
        (root_replacement_cleanup, outgoing_http)
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
            !self.wallet_sync_cleanup_tasks.is_empty(),
        )
    }

    pub(super) fn wallet_sync_cleanup_wait_group(&mut self) -> WalletSyncLifecycleCleanupWaitGroup {
        self.prune_finished_wallet_sync_cleanups();
        WalletSyncLifecycleCleanupWaitGroup::new(self.wallet_sync_cleanup_tasks.clone())
    }

    pub(super) fn begin_merkle_forest_cache_reset(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> WalletSyncLifecycleCleanupWaitGroup {
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
        self.prune_finished_wallet_sync_cleanups();
        if reset_succeeded && self.view_session.is_some() {
            self.ensure_chain_load(self.selected_chain, cx);
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
        if self.selected_chain == chain_id {
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

        self.chain_states
            .insert(chain_id, ChainUtxoState::Loading { progress: None });
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
            effective_chain: self.effective_chain_configs.get(&chain_id).cloned(),
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
                let session = store
                    .start_view_wallet_session_immediate(request, None, &http)
                    .await?;
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
                            *state = progress;
                            Some(InitialCatchUpFingerprint::new(progress, None))
                        }
                        Some(ChainUtxoState::Syncing {
                            progress: state,
                            sync_tip,
                            ..
                        }) => {
                            *state = syncing_progress(progress);
                            Some(InitialCatchUpFingerprint::new(*state, Some(*sync_tip)))
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
                        if root.selected_chain == chain_id {
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
                        if root.selected_chain == chain_id {
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
                let progress = root
                    .chain_states
                    .get(&chain_id)
                    .and_then(ChainUtxoState::progress);
                let progress = syncing_progress(progress);
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
                    } => Some(InitialCatchUpFingerprint::new(*progress, Some(*sync_tip))),
                    ChainUtxoState::Idle
                    | ChainUtxoState::Loading { .. }
                    | ChainUtxoState::Ready { .. }
                    | ChainUtxoState::Error { .. } => None,
                };
                root.chain_states.insert(chain_id, state);
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
                }
                if initially_failed {
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
                if root.selected_chain == chain_id {
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
                return;
            }

            loop {
                tokio::select! {
                    changed = observer_cancel_rx.changed() => {
                        if changed.is_err() || *observer_cancel_rx.borrow() {
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
                                    session,
                                    observer_token,
                                    sync_tip,
                                    poi_refreshing,
                                    ..
                                } => ChainUtxoState::Syncing {
                                    snapshot: snapshot.clone(),
                                    progress,
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
                                    progress: None,
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
                            }
                            if validation_completed {
                                root.pending_ppoi_validation_toast = Some((
                                    Arc::clone(&result_wallet_id),
                                    active_wallet_generation,
                                ));
                            }
                            root.refresh_open_form_assets_for_snapshot(&snapshot, cx);
                            if became_ready {
                                root.reschedule_ready_public_broadcaster_cost_estimates(chain_id, cx);
                            }
                            if root.selected_chain == chain_id {
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
                            if root.selected_chain == chain_id {
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
                            if root.selected_chain == chain_id {
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

    pub(super) fn select_chain(
        &mut self,
        chain_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.selected_chain == chain_id {
            return;
        }
        window.close_all_dialogs(cx);
        self.selected_chain = chain_id;
        self.ui_state.last_chain_id = Some(chain_id);
        self.save_ui_state();
        self.sync_broadcaster_monitor_chain_filter(chain_id, window, cx);
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.private_action_form = None;
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        self.local_pending_spent_clear_confirming = false;
        self.clear_public_chain_balance_state();
        self.sync_utxo_table(cx);
        if self.active_wallet_tab == WalletTab::Public {
            self.schedule_public_balance_refresh(cx);
        }
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
        cx.notify();
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
