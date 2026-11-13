use std::future::Future;
use std::sync::Arc;

use broadcaster_monitor::{EventTx, Shared, publish_revision};
#[cfg(test)]
use broadcaster_monitor_waku::WakuMonitorConfig;
use broadcaster_monitor_waku::{RelayNetworkMode, spawn_workers_until_shutdown};
use gpui::Context;
use tokio::sync::watch;
use wallet_ops::{PublicBroadcasterWaku, SubmissionNodeScope, WakuDeliveryClient};

use super::{DeliveryMode, WalletRoot, format_report_chain, network};

pub(super) struct WalletWakuRuntime {
    pub(super) client: Arc<WakuDeliveryClient>,
    pub(super) worker_shutdown: watch::Sender<bool>,
    pub(super) completion: WakuWorkerCompletionToken,
    pub(super) generation: u64,
}

#[derive(Clone)]
pub(super) struct WakuWorkerCompletionToken {
    quiesced_rx: watch::Receiver<bool>,
}

impl WakuWorkerCompletionToken {
    pub(super) async fn wait(mut self) -> Result<(), String> {
        loop {
            if *self.quiesced_rx.borrow() {
                return Ok(());
            }
            self.quiesced_rx
                .changed()
                .await
                .map_err(|_| "Waku worker ended before quiescence was established".to_string())?;
        }
    }

    #[cfg(test)]
    pub(super) fn closed_for_test() -> Self {
        let (_quiesced_tx, quiesced_rx) = watch::channel(false);
        Self { quiesced_rx }
    }

    #[cfg(test)]
    pub(super) fn channel_for_test() -> (Self, watch::Sender<bool>) {
        let (quiesced_tx, quiesced_rx) = watch::channel(false);
        (Self { quiesced_rx }, quiesced_tx)
    }
}

pub(super) struct WakuWorkerQuiescenceGuard {
    quiesced_tx: watch::Sender<bool>,
}

impl WakuWorkerQuiescenceGuard {
    pub(super) const fn new(quiesced_tx: watch::Sender<bool>) -> Self {
        Self { quiesced_tx }
    }
}

impl Drop for WakuWorkerQuiescenceGuard {
    fn drop(&mut self) {
        let _ = self.quiesced_tx.send(true);
    }
}

pub(super) async fn run_waku_worker_task(
    worker: impl Future<Output = eyre::Result<()>>,
    quiescence_guard: WakuWorkerQuiescenceGuard,
) -> eyre::Result<()> {
    let result = worker.await;
    drop(quiescence_guard);
    result
}

pub(super) struct WakuStoppingState {
    pub(super) generation: u64,
    /// Kept so a Tor rotation can await this transport's termination before committing.
    pub(super) client: Arc<WakuDeliveryClient>,
    pub(super) completion: WakuWorkerCompletionToken,
}

#[cfg(test)]
impl WakuStoppingState {
    pub(super) fn for_test(generation: u64) -> Self {
        let config = WakuMonitorConfig {
            network: broadcaster_monitor_waku::RelayNetworkConfig::proxy(reqwest::Client::new()),
            ..WakuMonitorConfig::default()
        };
        Self {
            generation,
            client: config.build_client().expect("proxy Waku delivery client"),
            completion: WakuWorkerCompletionToken::closed_for_test(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WakuWorkerCompletionKind {
    Clean,
    WorkerError,
    TaskError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WakuWorkerCompletionAction {
    FinalizedStop { restart: bool },
    HandleActiveFailure,
    Ignore,
}

pub(super) fn should_start_waku_runtime(
    view_unlocked: bool,
    runtime_active: bool,
    runtime_stopping: bool,
    lifecycle_shutdown_started: bool,
    network_mode: RelayNetworkMode,
) -> bool {
    view_unlocked
        && !runtime_active
        && !runtime_stopping
        && !lifecycle_shutdown_started
        && network_mode != RelayNetworkMode::Proxy
}

pub(super) fn should_start_waku_for_delivery(
    delivery_mode: DeliveryMode,
    view_unlocked: bool,
    runtime_active: bool,
    runtime_stopping: bool,
    lifecycle_shutdown_started: bool,
    network_mode: RelayNetworkMode,
) -> bool {
    delivery_mode == DeliveryMode::PublicBroadcaster
        && should_start_waku_runtime(
            view_unlocked,
            runtime_active,
            runtime_stopping,
            lifecycle_shutdown_started,
            network_mode,
        )
}

/// A pending Tor rotation holds Waku stopped until it commits, like a runtime still stopping.
pub(super) const fn waku_start_blocked(
    stopping: Option<&WakuStoppingState>,
    rotation: Option<&network::TorRotation>,
) -> bool {
    stopping.is_some() || rotation.is_some()
}

pub(super) fn build_waku_client_if_needed<C, E>(
    should_start: bool,
    build: impl FnOnce() -> Result<C, E>,
) -> Result<Option<C>, E> {
    if should_start {
        build().map(Some)
    } else {
        Ok(None)
    }
}

pub(super) fn active_waku_client(
    runtime: Option<&WalletWakuRuntime>,
) -> Option<Arc<WakuDeliveryClient>> {
    runtime.map(|runtime| Arc::clone(&runtime.client))
}

/// Closes `scope`, which ends every submission node's teardown delay and refuses nodes to
/// callbacks still holding it, and leaves a fresh open scope in its place.
pub(super) fn renew_submission_scope(scope: &mut SubmissionNodeScope) {
    std::mem::take(scope).close();
}

fn take_waku_runtime_for_stop(
    runtime: &mut Option<WalletWakuRuntime>,
) -> Option<WakuStoppingState> {
    let runtime = runtime.take()?;
    let _ = runtime.worker_shutdown.send(true);
    Some(WakuStoppingState {
        generation: runtime.generation,
        client: runtime.client,
        completion: runtime.completion,
    })
}

#[cfg(test)]
pub(super) fn stop_waku_runtime(
    runtime: &mut Option<WalletWakuRuntime>,
    monitor_state: &Shared,
    monitor_event_tx: &EventTx,
) -> bool {
    let stopped = take_waku_runtime_for_stop(runtime).is_some();
    if stopped && let Some(rev) = monitor_state.write().clear() {
        publish_revision(monitor_event_tx, rev);
    }
    stopped
}

pub(super) fn complete_waku_worker_generation(
    active_generation: Option<u64>,
    stopping_generation: &mut Option<WakuStoppingState>,
    generation: u64,
    completion: WakuWorkerCompletionKind,
    view_unlocked: bool,
    lifecycle_shutdown_started: bool,
    monitor_state: &Shared,
    monitor_event_tx: &EventTx,
) -> WakuWorkerCompletionAction {
    if stopping_generation
        .as_ref()
        .is_some_and(|stopping| stopping.generation == generation)
    {
        *stopping_generation = None;
        if !lifecycle_shutdown_started && let Some(rev) = monitor_state.write().clear() {
            publish_revision(monitor_event_tx, rev);
        }
        return WakuWorkerCompletionAction::FinalizedStop {
            restart: view_unlocked && !lifecycle_shutdown_started,
        };
    }

    if active_generation == Some(generation) && completion != WakuWorkerCompletionKind::Clean {
        WakuWorkerCompletionAction::HandleActiveFailure
    } else {
        WakuWorkerCompletionAction::Ignore
    }
}

/// Ends the stopping state of `generation`, whose workers and transport the caller has retired,
/// and clears rows those workers wrote while stopping. Its late completion then changes nothing.
pub(super) fn release_retired_waku_generation(
    stopping: &mut Option<WakuStoppingState>,
    generation: u64,
    monitor_state: &Shared,
    monitor_event_tx: &EventTx,
) {
    if stopping
        .as_ref()
        .is_some_and(|stopping| stopping.generation == generation)
    {
        *stopping = None;
    }
    if let Some(rev) = monitor_state.write().clear() {
        publish_revision(monitor_event_tx, rev);
    }
}

impl WalletRoot {
    /// Public broadcaster delivery over the active runtime, which supplies only seed peers.
    pub(super) fn public_broadcaster_waku(&self) -> Option<PublicBroadcasterWaku> {
        active_waku_client(self.waku_runtime.as_ref()).map(|root| {
            PublicBroadcasterWaku::new(
                root,
                self.waku_config.to_waku_config(),
                self.http.clone(),
                self.submission_scope.clone(),
            )
        })
    }

    pub(in crate::root) fn ensure_waku_for_delivery(
        &mut self,
        delivery_mode: DeliveryMode,
        cx: &mut Context<'_, Self>,
    ) {
        if should_start_waku_for_delivery(
            delivery_mode,
            self.vault_view_unlock.is_some(),
            self.waku_runtime.is_some(),
            waku_start_blocked(
                self.waku_stopping_generation.as_ref(),
                self.tor_rotation.as_ref(),
            ),
            self.wallet_sync_lifecycle_shutdown_started,
            self.waku_config.network.mode,
        ) {
            self.ensure_waku_started(cx);
        }
    }

    pub(super) fn ensure_waku_started(&mut self, cx: &mut Context<'_, Self>) {
        let should_start = should_start_waku_runtime(
            self.vault_view_unlock.is_some(),
            self.waku_runtime.is_some(),
            waku_start_blocked(
                self.waku_stopping_generation.as_ref(),
                self.tor_rotation.as_ref(),
            ),
            self.wallet_sync_lifecycle_shutdown_started,
            self.waku_config.network.mode,
        );
        let client =
            match build_waku_client_if_needed(should_start, || self.waku_config.build_client()) {
                Ok(Some(client)) => client,
                Ok(None) => return,
                Err(error) => {
                    tracing::error!(%error, "wallet Waku delivery runtime failed to start");
                    self.network_status_error = Some(Arc::from(format!(
                        "Waku delivery network is unavailable: {}",
                        format_report_chain(&error)
                    )));
                    cx.notify();
                    return;
                }
            };
        self.waku_session_generation = self.waku_session_generation.wrapping_add(1);
        let generation = self.waku_session_generation;
        let (worker_shutdown, shutdown_rx) = watch::channel(false);
        let (quiesced_tx, quiesced_rx) = watch::channel(false);
        let completion = WakuWorkerCompletionToken { quiesced_rx };
        let config = self.waku_config.clone();
        let worker_client = Arc::clone(&client);
        let worker_monitor_state = self.monitor_state.clone();
        let worker_event_tx = self.monitor_event_tx.clone();
        let quiescence_guard = WakuWorkerQuiescenceGuard::new(quiesced_tx);
        let worker = self.runtime.spawn(async move {
            run_waku_worker_task(
                spawn_workers_until_shutdown(
                    config,
                    worker_client,
                    worker_monitor_state,
                    worker_event_tx,
                    shutdown_rx,
                ),
                quiescence_guard,
            )
            .await
        });
        self.waku_runtime = Some(WalletWakuRuntime {
            client,
            worker_shutdown,
            completion,
            generation,
        });
        self.network_status_error = None;

        cx.spawn(async move |this, cx| {
            let result = worker.await;
            let completion = match &result {
                Ok(Ok(())) => WakuWorkerCompletionKind::Clean,
                Ok(Err(_)) => WakuWorkerCompletionKind::WorkerError,
                Err(_) => WakuWorkerCompletionKind::TaskError,
            };
            let _ = this.update(cx, |root, cx| {
                let action = complete_waku_worker_generation(
                    root.waku_runtime.as_ref().map(|runtime| runtime.generation),
                    &mut root.waku_stopping_generation,
                    generation,
                    completion,
                    root.vault_view_unlock.is_some(),
                    root.wallet_sync_lifecycle_shutdown_started,
                    &root.monitor_state,
                    &root.monitor_event_tx,
                );
                match (action, result) {
                    (
                        WakuWorkerCompletionAction::FinalizedStop { restart },
                        stopping_result,
                    ) => {
                        match stopping_result {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                tracing::debug!(%error, "Waku delivery worker failed while stopping");
                            }
                            Err(error) => {
                                tracing::warn!(%error, "Waku delivery worker task failed while stopping");
                            }
                        }
                        if restart
                            && network::restart_now_or_defer_to_rotation(
                                root.tor_rotation.as_mut(),
                                generation,
                            )
                        {
                            root.ensure_waku_started(cx);
                        } else {
                            cx.notify();
                        }
                    }
                    (WakuWorkerCompletionAction::HandleActiveFailure, Ok(Err(error))) => {
                        root.stop_waku();
                        root.waku_stopping_generation = None;
                        tracing::error!(%error, "wallet Waku delivery monitor workers failed to start");
                        root.network_status_error = Some(Arc::from(format!(
                            "Waku delivery network is unavailable: {}",
                            format_report_chain(&error)
                        )));
                        cx.notify();
                    }
                    (WakuWorkerCompletionAction::HandleActiveFailure, Err(error)) => {
                        root.stop_waku();
                        root.waku_stopping_generation = None;
                        tracing::error!(%error, "wallet Waku delivery monitor task failed");
                        root.network_status_error = Some(Arc::from(format!(
                            "Waku delivery network monitor failed: {error}"
                        )));
                        cx.notify();
                    }
                    (
                        WakuWorkerCompletionAction::HandleActiveFailure,
                        Ok(Ok(())),
                    )
                    | (WakuWorkerCompletionAction::Ignore, _) => {}
                }
            });
        })
        .detach();
    }

    pub(super) fn stop_waku(&mut self) -> bool {
        let stopping = take_waku_runtime_for_stop(&mut self.waku_runtime);
        let stopped = stopping.is_some();
        if stopped {
            self.waku_stopping_generation = stopping;
            if !self.wallet_sync_lifecycle_shutdown_started
                && let Some(rev) = self.monitor_state.write().clear()
            {
                publish_revision(&self.monitor_event_tx, rev);
            }
        }
        stopped
    }

    pub(super) fn stop_waku_for_root_replacement(&mut self) -> Option<WakuWorkerCompletionToken> {
        if let Some(stopping) = take_waku_runtime_for_stop(&mut self.waku_runtime) {
            self.waku_stopping_generation = Some(stopping);
        }
        self.waku_stopping_generation
            .as_ref()
            .map(|stopping| stopping.completion.clone())
    }
}
