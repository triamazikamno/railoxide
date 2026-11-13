use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use broadcaster_monitor::{EventTx, Shared, publish_revision};
use tokio::runtime::Handle;
use tokio::sync::{OnceCell, watch};
use wallet_ops::WalletSessionStore;

use super::super::WakuWorkerCompletionToken;
use super::super::proposals::ProposalCleanup;

const WALLET_SYNC_STARTUP_SUPERSEDED: &str = "wallet sync startup superseded";

// Allocation identity keeps same-wallet, same-generation replacement sessions distinct.
#[derive(Clone, Debug)]
pub(in crate::root) struct InstalledObserverToken {
    pub(super) chain_id: u64,
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

pub(in crate::root) struct WalletSyncLifecycle {
    generation: Arc<AtomicU64>,
    next_task_id: u64,
    session_store: Arc<OnceCell<Arc<WalletSessionStore>>>,
    startup_tasks: BTreeMap<u64, WalletSyncStartupTask>,
    wallet_tasks: Vec<tokio::task::JoinHandle<()>>,
    current_task_by_chain: BTreeMap<u64, u64>,
    installed_observers: BTreeMap<u64, InstalledObserverControl>,
}

pub(in crate::root) struct WalletSyncStartupRegistration {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) generation: u64,
    pub(in crate::root) task_id: u64,
    pub(in crate::root) observer_token: InstalledObserverToken,
    pub(in crate::root) generation_token: Arc<AtomicU64>,
    pub(in crate::root) session_store: Arc<OnceCell<Arc<WalletSessionStore>>>,
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

pub(in crate::root) struct InstalledObserverTaskRegistration {
    pub(in crate::root) cancel_rx: watch::Receiver<bool>,
    pub(in crate::root) completed_tx: watch::Sender<bool>,
}

pub(super) struct InstalledObserverCompletion(pub(super) watch::Sender<bool>);

impl Drop for InstalledObserverCompletion {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

pub(in crate::root) struct WalletSyncLifecycleCleanup {
    startup_tasks: Vec<WalletSyncStartupTask>,
    wallet_tasks: Vec<tokio::task::JoinHandle<()>>,
    installed_observers: Vec<InstalledObserverControl>,
    session_store: Option<Arc<OnceCell<Arc<WalletSessionStore>>>>,
}

#[derive(Clone)]
pub(in crate::root) struct WalletSyncLifecycleCleanupTask {
    completed_rx: watch::Receiver<Option<WalletSyncLifecycleCleanupReport>>,
}

#[derive(Clone)]
pub(in crate::root) struct WalletSyncLifecycleCleanupWaitGroup {
    tasks: Vec<WalletSyncLifecycleCleanupTask>,
    pub(super) public_transactions:
        Option<super::super::public_transactions::PublicTransactionCleanup>,
}

#[derive(Clone)]
pub(in crate::root) struct WalletRootReplacementCleanup {
    completed_rx: watch::Receiver<Option<Result<WalletSyncLifecycleCleanupReport, String>>>,
}

pub(in crate::root) struct WalletPublicSyncCacheResetContext {
    pub(super) cleanup: WalletSyncLifecycleCleanupWaitGroup,
    pub(super) session_store: Arc<OnceCell<Arc<WalletSessionStore>>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::root) struct WalletSyncLifecycleCleanupReport {
    pub(in crate::root) stopped_startup_tasks: usize,
    pub(in crate::root) failed_startup_tasks: usize,
    pub(in crate::root) shut_down_session_store: bool,
}

impl WalletSyncLifecycle {
    pub(in crate::root) fn new() -> Self {
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

    pub(in crate::root) fn public_sync_cache_reset_cell(
        &self,
    ) -> Arc<OnceCell<Arc<WalletSessionStore>>> {
        Arc::clone(&self.session_store)
    }

    pub(in crate::root) fn prepare_startup(
        &mut self,
        chain_id: u64,
    ) -> WalletSyncStartupRegistration {
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

    pub(in crate::root) fn register_installed_observer(
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

    pub(in crate::root) fn track_startup(
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

    pub(in crate::root) fn track_wallet_task(&mut self, join: tokio::task::JoinHandle<()>) {
        self.wallet_tasks.retain(|task| !task.is_finished());
        self.wallet_tasks.push(join);
    }

    pub(in crate::root) fn is_current_startup(
        &self,
        chain_id: u64,
        generation: u64,
        task_id: u64,
    ) -> bool {
        self.current_generation() == generation
            && self
                .current_task_by_chain
                .get(&chain_id)
                .is_some_and(|current| *current == task_id)
    }

    pub(in crate::root) fn finish_startup(&mut self, chain_id: u64, generation: u64, task_id: u64) {
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

    pub(in crate::root) fn finish_startup_after_session_installation(
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

    pub(in crate::root) fn invalidate(&mut self) -> WalletSyncLifecycleCleanup {
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        if let Some(store) = self.session_store.get() {
            store.invalidate_executor_owners(generation);
        }
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

    pub(in crate::root) fn supersede_wallet(&mut self) -> WalletSyncLifecycleCleanup {
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        if let Some(store) = self.session_store.get() {
            store.invalidate_executor_owners(generation);
        }
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
    pub(in crate::root) async fn shutdown(
        self,
    ) -> Result<WalletSyncLifecycleCleanupReport, String> {
        Ok(self.shutdown_inner().await)
    }

    pub(in crate::root) fn spawn(self, runtime: &Handle) -> WalletSyncLifecycleCleanupTask {
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
    pub(in crate::root) fn closed_for_test() -> Self {
        let (completed_tx, completed_rx) = watch::channel(None);
        drop(completed_tx);
        Self { completed_rx }
    }

    #[cfg(test)]
    pub(in crate::root) fn channel_for_test() -> (
        Self,
        watch::Sender<Option<WalletSyncLifecycleCleanupReport>>,
    ) {
        let (completed_tx, completed_rx) = watch::channel(None);
        (Self { completed_rx }, completed_tx)
    }

    pub(in crate::root) fn is_finished(&self) -> bool {
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
    pub(in crate::root) const fn new(tasks: Vec<WalletSyncLifecycleCleanupTask>) -> Self {
        Self {
            tasks,
            public_transactions: None,
        }
    }

    pub(in crate::root) async fn shutdown_for_merkle_reset(
        self,
    ) -> Result<WalletSyncLifecycleCleanupReport, String> {
        self.wait().await
    }

    pub(in crate::root) async fn shutdown_for_wallet_deletion(
        self,
    ) -> Result<WalletSyncLifecycleCleanupReport, String> {
        self.wait().await
    }

    pub(in crate::root) async fn shutdown_for_wallet_replacement(
        self,
    ) -> Result<WalletSyncLifecycleCleanupReport, String> {
        self.wait().await
    }

    #[cfg(test)]
    pub(in crate::root) fn is_finished(&self) -> bool {
        self.tasks
            .iter()
            .all(WalletSyncLifecycleCleanupTask::is_finished)
            && self.public_transactions.as_ref().is_none_or(
                super::super::public_transactions::PublicTransactionCleanup::is_finished,
            )
    }

    async fn wait(self) -> Result<WalletSyncLifecycleCleanupReport, String> {
        if let Some(cleanup) = self.public_transactions {
            cleanup.wait().await?;
        }
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
    pub(in crate::root) fn spawn(
        runtime: &Handle,
        sync_cleanup: WalletSyncLifecycleCleanupWaitGroup,
        waku_completion: Option<WakuWorkerCompletionToken>,
        proposal_cleanup: ProposalCleanup,
        gateway: Option<wallet_ops::gateway::GatewayHandle>,
        monitor_state: Shared,
        monitor_event_tx: EventTx,
    ) -> Self {
        let (completed_tx, completed_rx) = watch::channel(None);
        runtime.spawn(async move {
            let (sync_result, waku_result, proposal_result, gateway_result) = tokio::join!(
                sync_cleanup.wait(),
                async {
                    match waku_completion {
                        Some(completion) => completion.wait().await,
                        None => Ok(()),
                    }
                },
                proposal_cleanup.shutdown(),
                async {
                    match gateway {
                        Some(gateway) => gateway.shutdown().await,
                        None => Ok(()),
                    }
                },
            );

            if let Some(rev) = monitor_state.write().clear() {
                publish_revision(&monitor_event_tx, rev);
            }

            let result = match (sync_result, waku_result, proposal_result) {
                (Ok(report), Ok(()), Ok(())) => Ok(report),
                (Err(sync_error), Ok(()), Ok(())) => Err(format!(
                    "wallet sync cleanup failed during root replacement: {sync_error}"
                )),
                (Ok(_), Err(waku_error), Ok(())) => Err(format!(
                    "Waku worker cleanup failed during root replacement: {waku_error}"
                )),
                (Ok(_), Ok(()), Err(proposal_error)) => Err(format!(
                    "Proposal worker cleanup failed during root replacement: {proposal_error}"
                )),
                (Err(sync_error), Err(waku_error), Ok(())) => Err(format!(
                    "wallet sync cleanup failed during root replacement: {sync_error}; Waku worker cleanup failed during root replacement: {waku_error}"
                )),
                (Err(sync_error), Ok(()), Err(proposal_error)) => Err(format!(
                    "wallet sync cleanup failed during root replacement: {sync_error}; Proposal worker cleanup failed during root replacement: {proposal_error}"
                )),
                (Ok(_), Err(waku_error), Err(proposal_error)) => Err(format!(
                    "Waku worker cleanup failed during root replacement: {waku_error}; Proposal worker cleanup failed during root replacement: {proposal_error}"
                )),
                (Err(sync_error), Err(waku_error), Err(proposal_error)) => Err(format!(
                    "wallet sync cleanup failed during root replacement: {sync_error}; Waku worker cleanup failed during root replacement: {waku_error}; Proposal worker cleanup failed during root replacement: {proposal_error}"
                )),
            };
            let result = match (result, gateway_result) {
                (result, Ok(())) => result,
                (Ok(_), Err(error)) => Err(format!("Gateway cleanup failed during root replacement: {error}")),
                (Err(previous), Err(error)) => Err(format!("{previous}; Gateway cleanup failed during root replacement: {error}")),
            };
            let _ = completed_tx.send(Some(result));
        });
        Self { completed_rx }
    }

    pub(in crate::root) fn is_finished(&self) -> bool {
        self.completed_rx.borrow().is_some() || self.completed_rx.has_changed().is_err()
    }

    pub(in crate::root) async fn wait(
        mut self,
    ) -> Result<WalletSyncLifecycleCleanupReport, String> {
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
    pub(in crate::root) async fn shutdown_for_public_reset(
        self,
    ) -> Result<Option<Arc<WalletSessionStore>>, String> {
        self.cleanup.wait().await?;
        Ok(self.session_store.get().cloned())
    }

    pub(in crate::root) async fn shutdown_for_poi_reset(self) -> Result<(), String> {
        self.cleanup.wait().await.map(|_| ())
    }
}

pub(super) fn wallet_sync_startup_superseded(generation: &AtomicU64, expected: u64) -> bool {
    generation.load(Ordering::Acquire) != expected
}

/// Waits for earlier cleanups to release the session store's database ownership before a
/// replacement store claims it. A cleanup that ends without a report is left to the ownership
/// check.
pub(in crate::root) async fn wait_for_wallet_sync_cleanups(
    cleanups: Vec<WalletSyncLifecycleCleanupTask>,
) {
    for cleanup in cleanups {
        if let Err(error) = cleanup.wait().await {
            tracing::warn!(%error, "opening wallet session store after unfinished sync cleanup");
        }
    }
}

pub(super) fn wallet_sync_startup_superseded_error() -> eyre::Report {
    eyre::eyre!(WALLET_SYNC_STARTUP_SUPERSEDED)
}
