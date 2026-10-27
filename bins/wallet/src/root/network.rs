use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eyre::WrapErr;
use gpui::Context;
use tokio::runtime::Handle;
use tokio::sync::watch;
pub(super) use ui::network_status::TorExitIpQueryState;
use ui::network_status::{NetworkActivity, NetworkStatus, NetworkStatusIntent, NetworkStatusKind};
use wallet_ops::gateway::{
    GatewayNetworkError, GatewayNetworkOutcome, GatewayNetworkRequest, GatewayNetworkView,
    GatewayTorRotation,
};
use wallet_ops::{
    HttpContext, TorBridgeActivitySnapshot, WakuDeliveryClient, WalletNetworkHealth,
    WalletNetworkHealthCause, WalletNetworkHealthState, WalletNetworkMode, request_tor_state_reset,
};

use super::{
    NETWORK_HEALTH_REFRESH_INTERVAL, TOR_EXIT_IP_QUERY_TIMEOUT, TOR_EXIT_IP_QUERY_URL,
    TOR_HEALTH_RETRY_TIMEOUT, VaultState, WakuStoppingState, WakuWorkerCompletionToken, WalletRoot,
    WalletWakuRuntime,
};

const TOR_ACTIVITY_RATE_WINDOW: Duration = Duration::from_secs(5);
const TOR_ACTIVITY_INTERVAL_LIMIT: usize = 8;
/// Bounds the old embedded Waku node's termination. Rotation stays pending meanwhile, and a node
/// that can't stop within it fails the rotation and keeps the current Tor session.
const TOR_ROTATION_WAKU_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// The root's pending explicit Tor rotation.
pub(super) struct TorRotation {
    id: u64,
    /// The active Waku runtime this rotation retires and replaces; `None` when Waku was
    /// inactive or only stopping, so the rotation starts none.
    retired_generation: Option<u64>,
    retired_peer_id: Option<String>,
    /// Gateway activations completed when the rotation finishes.
    requests: Vec<GatewayNetworkRequest>,
    /// An ordinary restart that this rotation held back, run when it finishes.
    pub(super) restart_deferred: bool,
}

#[cfg(test)]
impl TorRotation {
    pub(super) const fn for_test(id: u64, retired_generation: Option<u64>) -> Self {
        Self {
            id,
            retired_generation,
            retired_peer_id: None,
            requests: Vec::new(),
            restart_deferred: false,
        }
    }
}

/// Installs `next` when no rotation is pending and reports whether it did. Otherwise `next`'s
/// requests join the pending rotation, which never schedules another.
pub(super) fn join_or_begin_tor_rotation(
    pending: &mut Option<TorRotation>,
    next: TorRotation,
) -> bool {
    if let Some(pending) = pending {
        pending.requests.extend(next.requests);
        return false;
    }
    *pending = Some(next);
    true
}

/// Takes rotation `id` if it is still the pending one.
pub(super) fn take_tor_rotation(pending: &mut Option<TorRotation>, id: u64) -> Option<TorRotation> {
    pending.take_if(|rotation| rotation.id == id)
}

/// Decides the ordinary restart that finalizing worker `generation` asks for, and returns
/// whether it runs now. A pending rotation holds it back until it finishes. The rotation's own
/// stop of the runtime it replaces owes none, since the rotation decides about a replacement.
pub(super) fn restart_now_or_defer_to_rotation(
    rotation: Option<&mut TorRotation>,
    generation: u64,
) -> bool {
    let Some(rotation) = rotation else {
        return true;
    };
    if rotation.retired_generation != Some(generation) {
        rotation.restart_deferred = true;
    }
    false
}

/// Orders a rotation's last steps: commit only once the old transport has retired, and start
/// Waku at most once, only after the commit, when it was active or a held-back ordinary restart
/// is owed. Failure before the commit leaves the session unchanged and starts only an owed
/// restart; after it, the new session stays and Waku that was active is reported unavailable if
/// it can't start.
pub(super) fn complete_tor_rotation<C, T>(
    steps: &mut C,
    waku_was_active: bool,
    restart_deferred: bool,
    retired: Result<(), String>,
    commit: impl FnOnce(&mut C) -> eyre::Result<T>,
    start_waku: impl FnOnce(&mut C) -> bool,
) -> Result<(T, Option<GatewayNetworkError>), GatewayNetworkError> {
    let committed = match retired
        .map_err(eyre::Report::msg)
        .and_then(|()| commit(steps))
    {
        Ok(committed) => committed,
        Err(error) => {
            // The retired identity stays retired; the rotation starts nothing on the old
            // session. An ordinary restart it held back still runs, with a fresh identity.
            tracing::warn!(%error, "Tor rotation failed before changing isolation");
            if restart_deferred {
                start_waku(steps);
            }
            return Err(GatewayNetworkError::NewSessionFailed);
        }
    };
    let started = (waku_was_active || restart_deferred).then(|| start_waku(steps));
    let waku_failed = waku_was_active && started == Some(false);
    Ok((
        committed,
        waku_failed.then_some(GatewayNetworkError::WakuUnavailable),
    ))
}

/// The native control's error: the latest network error, or else the last rotation's failure,
/// which the gateway shows too and which a later Waku start or a closed popover doesn't clear.
pub(super) fn native_network_status_error(
    network_status_error: Option<&Arc<str>>,
    rotation_failure: Option<GatewayNetworkError>,
) -> Option<Arc<str>> {
    network_status_error
        .cloned()
        .or_else(|| rotation_failure.map(|error| Arc::from(error.message())))
}

/// The Waku transport a rotation retires before committing, with the generation it replaces:
/// the active runtime, or else a runtime still stopping, which the rotation awaits but doesn't
/// replace. The root's ordinary rules decide whether the stopped one starts again.
pub(super) fn waku_to_retire(
    runtime: Option<&WalletWakuRuntime>,
    stopping: Option<&WakuStoppingState>,
) -> Option<(
    Option<u64>,
    Arc<WakuDeliveryClient>,
    WakuWorkerCompletionToken,
)> {
    if let Some(runtime) = runtime {
        return Some((
            Some(runtime.generation),
            Arc::clone(&runtime.client),
            runtime.completion.clone(),
        ));
    }
    stopping.map(|stopping| {
        (
            None,
            Arc::clone(&stopping.client),
            stopping.completion.clone(),
        )
    })
}

/// Waits for the monitor workers to stop using `client`, then for its transport to terminate,
/// so the old identity can't dial through the next session.
pub(super) async fn retire_root_waku(
    completion: WakuWorkerCompletionToken,
    client: Arc<WakuDeliveryClient>,
) -> Result<(), String> {
    completion.wait().await?;
    client
        .shutdown(TOR_ROTATION_WAKU_SHUTDOWN_TIMEOUT)
        .await
        .map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug)]
struct DownloadInterval {
    start: Instant,
    end: Instant,
    downloaded_bytes: u64,
}

struct TorBridgeActivitySampler {
    baseline: Option<(u64, u64, Instant)>,
    intervals: VecDeque<DownloadInterval>,
}

impl TorBridgeActivitySampler {
    const fn new() -> Self {
        Self {
            baseline: None,
            intervals: VecDeque::new(),
        }
    }

    fn sample(
        &mut self,
        snapshot: Option<&TorBridgeActivitySnapshot>,
        now: Instant,
    ) -> Option<u64> {
        let Some(snapshot) = snapshot else {
            self.reset();
            return None;
        };

        let Some((generation, downloaded_bytes, previous_at)) = self.baseline else {
            self.reset_to(snapshot, now);
            return None;
        };

        if generation != snapshot.generation || snapshot.downloaded_bytes < downloaded_bytes {
            self.reset_to(snapshot, now);
            return None;
        }

        if now <= previous_at {
            self.reset_to(snapshot, now);
            return None;
        }

        self.baseline = Some((snapshot.generation, snapshot.downloaded_bytes, now));
        self.intervals.push_back(DownloadInterval {
            start: previous_at,
            end: now,
            downloaded_bytes: snapshot.downloaded_bytes - downloaded_bytes,
        });
        self.evict_old_intervals(now);
        self.weighted_rate(now)
    }

    fn reset(&mut self) {
        self.baseline = None;
        self.intervals.clear();
    }

    fn reset_to(&mut self, snapshot: &TorBridgeActivitySnapshot, now: Instant) {
        self.baseline = Some((snapshot.generation, snapshot.downloaded_bytes, now));
        self.intervals.clear();
    }

    fn evict_old_intervals(&mut self, now: Instant) {
        let cutoff = now
            .checked_sub(TOR_ACTIVITY_RATE_WINDOW)
            .or_else(|| self.intervals.front().map(|interval| interval.start))
            .unwrap_or(now);
        while self
            .intervals
            .front()
            .is_some_and(|interval| interval.end <= cutoff)
        {
            self.intervals.pop_front();
        }
        while self.intervals.len() > TOR_ACTIVITY_INTERVAL_LIMIT {
            self.intervals.pop_front();
        }
    }

    fn weighted_rate(&self, now: Instant) -> Option<u64> {
        let cutoff = now
            .checked_sub(TOR_ACTIVITY_RATE_WINDOW)
            .or_else(|| self.intervals.front().map(|interval| interval.start))
            .unwrap_or(now);
        let mut weighted_bytes = 0_u128;
        let mut elapsed_nanos = 0_u128;

        for interval in &self.intervals {
            let overlap_start = interval.start.max(cutoff);
            let overlap_end = interval.end.min(now);
            if overlap_end <= overlap_start {
                continue;
            }
            let interval_duration = interval.end.saturating_duration_since(interval.start);
            let overlap_duration = overlap_end.saturating_duration_since(overlap_start);
            let interval_nanos = interval_duration.as_nanos();
            let overlap_nanos = overlap_duration.as_nanos();
            if interval_nanos == 0 || overlap_nanos == 0 {
                continue;
            }

            weighted_bytes = weighted_bytes.saturating_add(
                u128::from(interval.downloaded_bytes).saturating_mul(overlap_nanos)
                    / interval_nanos,
            );
            elapsed_nanos = elapsed_nanos.saturating_add(overlap_nanos);
        }

        if elapsed_nanos == 0 {
            return None;
        }

        Some(
            weighted_bytes
                .saturating_mul(1_000_000_000)
                .checked_div(elapsed_nanos)
                .unwrap_or(0)
                .min(u128::from(u64::MAX)) as u64,
        )
    }
}

const fn next_tor_exit_ip_query_generation(current: u64) -> u64 {
    current.saturating_add(1)
}

const fn tor_exit_ip_query_completion_is_current(
    current_generation: u64,
    completion_generation: u64,
    state: &TorExitIpQueryState,
) -> bool {
    current_generation == completion_generation && matches!(state, TorExitIpQueryState::Querying)
}

impl WalletRoot {
    pub(super) fn network_context_revision(&self) -> String {
        format!(
            "{}:{}",
            self.network_context_id,
            self.http.tor_session_generation()
        )
    }

    pub(super) fn gateway_network_view(&self) -> GatewayNetworkView {
        let mut view = GatewayNetworkView::new(
            self.network_context_revision(),
            &self.network_health,
            self.tor_bridge_activity.as_ref(),
            self.tor_download_rate,
        );
        view.direct_rpc_endpoints = self.effective_chain_configs.direct_rpc_endpoint_count();
        view.rotation = self.tor_rotation_presentation();
        view
    }

    pub(super) fn network_status_presentation(&self) -> NetworkStatus {
        let kind = match (self.network_health.mode, self.network_health.state) {
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Ready) => {
                NetworkStatusKind::TorReady
            }
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Reconnecting) => {
                NetworkStatusKind::TorReconnecting
            }
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Degraded) => {
                NetworkStatusKind::TorDegraded
            }
            (WalletNetworkMode::Proxy, _) => NetworkStatusKind::Proxy,
            (WalletNetworkMode::Direct, _) => NetworkStatusKind::Direct,
        };
        NetworkStatus::new(
            kind,
            self.network_health.detail.to_string(),
            matches!(
                self.network_health.cause,
                WalletNetworkHealthCause::TorRuntimeSlow
                    | WalletNetworkHealthCause::TorRuntimeUnreliable
            ),
        )
        .with_direct_rpc_endpoints(self.effective_chain_configs.direct_rpc_endpoint_count())
    }

    pub(super) fn network_activity_presentation(&self) -> Option<NetworkActivity> {
        self.tor_bridge_activity.as_ref().map(|snapshot| {
            let mut activity = NetworkActivity::default();
            activity.generation = snapshot.generation;
            activity.session_duration = snapshot.session_duration;
            activity.downloaded_bytes = snapshot.downloaded_bytes;
            activity.recent_connection_sample_count = snapshot.recent_connection_sample_count;
            activity.recent_successful_sample_count = snapshot.recent_successful_sample_count;
            activity.successful_connections = snapshot.successful_connections;
            activity.failed_connections = snapshot.failed_connections;
            activity.median_setup_duration = snapshot.median_setup_duration;
            activity.last_activity_age = snapshot.last_activity_age;
            activity
        })
    }

    pub(super) fn network_status_intent(
        &mut self,
        intent: NetworkStatusIntent,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.network_status_popover_open {
            return;
        }
        match intent {
            NetworkStatusIntent::NewTorSession => self.request_tor_rotation(None, cx),
            NetworkStatusIntent::QueryExitIp => self.query_tor_exit_ip(cx),
            NetworkStatusIntent::BeginReset => self.begin_tor_state_reset_confirmation(cx),
            NetworkStatusIntent::CancelReset => self.cancel_tor_state_reset_confirmation(cx),
            NetworkStatusIntent::QuitAndReset => self.quit_and_reset_tor_state(cx),
        }
    }

    pub(super) fn spawn_network_health_monitor(&self, cx: &Context<'_, Self>) {
        if self.http.network_mode() != WalletNetworkMode::Tor {
            return;
        }

        let http = self.http.clone();
        let runtime = self.runtime.clone();
        let mut shutdown = self.root_shutdown.subscribe();
        cx.spawn(async move |this, cx| {
            loop {
                tokio::select! {
                    () = cx.background_executor().timer(NETWORK_HEALTH_REFRESH_INTERVAL) => {}
                    should_shutdown = wallet_root_shutdown_requested(&mut shutdown) => {
                        if should_shutdown {
                            break;
                        }
                        continue;
                    }
                }
                let generation = http.tor_session_generation();
                let health = http.network_health();
                let Ok(should_retry) = this.update(cx, |root, cx| {
                    if root.http.tor_session_generation() != generation {
                        return false;
                    }
                    let should_retry = health.cause == WalletNetworkHealthCause::TorBootstrap;
                    root.set_network_health(health, cx);
                    should_retry
                }) else {
                    break;
                };

                if should_retry {
                    tokio::select! {
                        () = retry_tor_bootstrap(&http, &runtime) => {}
                        should_shutdown = wallet_root_shutdown_requested(&mut shutdown) => {
                            if should_shutdown {
                                break;
                            }
                            continue;
                        }
                    }
                    let generation = http.tor_session_generation();
                    let health = http.network_health();
                    if this
                        .update(cx, |root, cx| {
                            if root.http.tor_session_generation() == generation {
                                root.set_network_health(health, cx);
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
        .detach();
    }

    pub(super) fn spawn_tor_bridge_activity_sampler(&self, cx: &Context<'_, Self>) {
        if self.http.network_mode() != WalletNetworkMode::Tor {
            return;
        }
        let http = self.http.clone();
        let mut shutdown = self.root_shutdown.subscribe();
        cx.spawn(async move |this, cx| {
            let mut sampler = TorBridgeActivitySampler::new();
            loop {
                tokio::select! {
                    () = cx.background_executor().timer(Duration::from_secs(1)) => {}
                    should_shutdown = wallet_root_shutdown_requested(&mut shutdown) => {
                        if should_shutdown {
                            break;
                        }
                        continue;
                    }
                }
                let snapshot = http.tor_bridge_activity_snapshot();
                let rate = sampler.sample(snapshot.as_ref(), Instant::now());
                if this
                    .update(cx, |root, cx| {
                        if snapshot.as_ref().is_some_and(|snapshot| {
                            root.http.tor_session_generation() != snapshot.generation
                        }) {
                            return;
                        }
                        if root.tor_bridge_activity != snapshot || root.tor_download_rate != rate {
                            root.tor_bridge_activity = snapshot;
                            root.tor_download_rate = rate;
                            root.publish_gateway_network_state();
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn set_network_health(&mut self, health: WalletNetworkHealth, cx: &mut Context<'_, Self>) {
        if self.network_health != health {
            self.network_health = health;
            self.publish_gateway_network_state();
            cx.notify();
        }
    }

    pub(super) fn set_network_status_popover_open(
        &mut self,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if !open {
            self.network_status_error = None;
            self.invalidate_tor_exit_ip_query();
            self.tor_state_reset_confirming = false;
        }
        if self.network_status_popover_open != open {
            self.network_status_popover_open = open;
            cx.notify();
        } else if !open {
            cx.notify();
        }
    }

    pub(super) fn toggle_network_status_from_shortcut(&mut self, cx: &mut Context<'_, Self>) {
        if !matches!(self.vault_state, VaultState::ViewUnlocked) {
            return;
        }
        // The next render opens the popover, which remembers the focused field, or closes it and
        // returns focus there.
        self.set_network_status_popover_open(!self.network_status_popover_open, cx);
    }

    /// Starts the explicit Tor rotation, or joins `request` to the pending one. Native and
    /// gateway activations both enter here.
    pub(super) fn request_tor_rotation(
        &mut self,
        request: Option<GatewayNetworkRequest>,
        cx: &mut Context<'_, Self>,
    ) {
        // A retiring root must not rotate the network context it hands to its replacement.
        if self.wallet_sync_lifecycle_shutdown_started || *self.root_shutdown.borrow() {
            return;
        }
        // Outside Tor mode the commit fails before any Waku change.
        let retiring = waku_to_retire(
            self.waku_runtime.as_ref(),
            self.waku_stopping_generation.as_ref(),
        )
        .filter(|_| self.http.network_mode() == WalletNetworkMode::Tor);
        let id = self.tor_rotation_id.wrapping_add(1);
        let rotation = TorRotation {
            id,
            retired_generation: retiring.as_ref().and_then(|(replaced, ..)| *replaced),
            retired_peer_id: retiring
                .as_ref()
                .and_then(|(_, client, _)| client.local_peer_id())
                .map(|peer_id| peer_id.to_string()),
            requests: request.into_iter().collect(),
            restart_deferred: false,
        };
        if !join_or_begin_tor_rotation(&mut self.tor_rotation, rotation) {
            return;
        }
        self.tor_rotation_id = id;
        self.tor_rotation_failure = None;
        self.network_status_error = None;
        if let Some(waku_failed) = super::private_swap::ui_fixture_tor_rotation() {
            if waku_failed && let Some(rotation) = take_tor_rotation(&mut self.tor_rotation, id) {
                let error = GatewayNetworkError::WakuUnavailable;
                self.tor_rotation_failure = Some(error);
                self.network_status_error = Some(Arc::from(error.message()));
                self.complete_gateway_network_requests(
                    rotation.requests,
                    &GatewayNetworkOutcome::Failed { error },
                );
            }
            self.publish_gateway_network_state();
            cx.notify();
            return;
        }
        let Some((_, client, completion)) = retiring else {
            // Inactive Waku stays inactive: there is nothing to retire or restart.
            self.finish_tor_rotation(id, Ok(()), cx);
            return;
        };
        // Only an active runtime needs stopping here; a stopping one is just awaited.
        self.stop_waku();
        self.publish_gateway_network_state();
        cx.notify();
        let retire = self.runtime.spawn(retire_root_waku(completion, client));
        cx.spawn(async move |this, cx| {
            let retired = retire
                .await
                .map_err(|error| error.to_string())
                .and_then(|retired| retired);
            let _ = this.update(cx, |root, cx| root.finish_tor_rotation(id, retired, cx));
        })
        .detach();
    }

    /// Commits rotation `id` once the old Waku transport has retired, then starts a fresh
    /// runtime. Lock, root replacement and shutdown cancel the rotation first, so a retired
    /// operation finds nothing to commit and starts nothing.
    fn finish_tor_rotation(
        &mut self,
        id: u64,
        retired: Result<(), String>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(rotation) = take_tor_rotation(&mut self.tor_rotation, id) else {
            return;
        };
        if let Some(generation) = rotation.retired_generation {
            super::release_retired_waku_generation(
                &mut self.waku_stopping_generation,
                generation,
                &self.monitor_state,
                &self.monitor_event_tx,
            );
        }
        // The ordinary restart policy decides any start; the retired identity is never revived.
        let completed = complete_tor_rotation(
            &mut (&mut *self, &mut *cx),
            rotation.retired_generation.is_some(),
            rotation.restart_deferred,
            retired,
            |(root, cx)| root.commit_tor_session(cx),
            |(root, cx)| {
                root.ensure_waku_started(cx);
                root.waku_runtime.is_some()
            },
        );
        let failure = match completed {
            Ok(((generation, walletconnect_refreshed), failure)) => {
                let waku_peer_id = self
                    .waku_runtime
                    .as_ref()
                    .and_then(|runtime| runtime.client.local_peer_id())
                    .map(|peer_id| peer_id.to_string());
                tracing::info!(
                    tor_session_generation = generation,
                    retired_waku_peer_id = ?rotation.retired_peer_id,
                    waku_peer_id = ?waku_peer_id,
                    walletconnect_refreshed,
                    "started new Tor session"
                );
                failure
            }
            Err(failure) => Some(failure),
        };
        self.tor_rotation_failure = failure;
        if let Some(error) = failure {
            self.network_status_error = Some(Arc::from(error.message()));
        }
        self.complete_gateway_network_requests(
            rotation.requests,
            &failure.map_or(GatewayNetworkOutcome::Done, |error| {
                GatewayNetworkOutcome::Failed { error }
            }),
        );
        self.publish_gateway_network_state();
        cx.notify();
    }

    /// The only step that advances the Tor session generation. Returns the new generation and
    /// how many `WalletConnect` relay workers restart.
    fn commit_tor_session(&mut self, cx: &mut Context<'_, Self>) -> eyre::Result<(u64, usize)> {
        let generation = self.http.start_new_tor_session()?;
        self.network_health = self.http.network_health();
        self.tor_bridge_activity = self.http.tor_bridge_activity_snapshot();
        self.tor_download_rate = None;
        self.invalidate_tor_exit_ip_query();
        self.tor_state_reset_confirming = false;
        let walletconnect_refreshed =
            self.restart_walletconnect_relay_workers_for_network_session(cx);
        self.publish_gateway_network_state();
        Ok((generation, walletconnect_refreshed))
    }

    /// Retires an uncommitted rotation and its presentation on lock and root replacement. A
    /// dropped root's continuation can't reach it at all.
    pub(super) fn cancel_tor_rotation(&mut self) {
        self.tor_rotation = None;
        self.tor_rotation_failure = None;
    }

    fn tor_rotation_presentation(&self) -> Option<GatewayTorRotation> {
        if self.tor_rotation.is_some() {
            Some(GatewayTorRotation::InProgress)
        } else {
            self.tor_rotation_failure
                .map(|error| GatewayTorRotation::Failed { error })
        }
    }

    fn invalidate_tor_exit_ip_query(&mut self) {
        self.tor_exit_ip_query_generation =
            next_tor_exit_ip_query_generation(self.tor_exit_ip_query_generation);
        self.tor_exit_ip_query = TorExitIpQueryState::Idle;
    }

    fn query_tor_exit_ip(&mut self, cx: &mut Context<'_, Self>) {
        if self.http.network_mode() != WalletNetworkMode::Tor
            || matches!(self.tor_exit_ip_query, TorExitIpQueryState::Querying)
        {
            return;
        }

        self.network_status_error = None;
        self.tor_exit_ip_query_generation =
            next_tor_exit_ip_query_generation(self.tor_exit_ip_query_generation);
        let query_generation = self.tor_exit_ip_query_generation;
        self.tor_exit_ip_query = TorExitIpQueryState::Querying;
        cx.notify();

        let Some(proxy_url) = self.http.proxy_url.clone() else {
            self.tor_exit_ip_query = TorExitIpQueryState::Error(Arc::from(
                "Exit IP query requires the built-in Tor SOCKS bridge",
            ));
            cx.notify();
            return;
        };
        let query = self
            .runtime
            .spawn(async move { query_exit_ip_through_tor(proxy_url).await });
        cx.spawn(async move |this, cx| {
            let state = match query.await {
                Ok(Ok(ip)) => TorExitIpQueryState::Success(ip),
                Ok(Err(_)) | Err(_) => TorExitIpQueryState::Error(Arc::from(
                    GatewayNetworkError::ExitIpFailed.message(),
                )),
            };
            let _ = this.update(cx, |root, cx| {
                if tor_exit_ip_query_completion_is_current(
                    root.tor_exit_ip_query_generation,
                    query_generation,
                    &root.tor_exit_ip_query,
                ) {
                    root.tor_exit_ip_query = state;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn begin_tor_state_reset_confirmation(&mut self, cx: &mut Context<'_, Self>) {
        self.network_status_error = None;
        self.invalidate_tor_exit_ip_query();
        self.tor_state_reset_confirming = true;
        cx.notify();
    }

    fn cancel_tor_state_reset_confirmation(&mut self, cx: &mut Context<'_, Self>) {
        self.tor_state_reset_confirming = false;
        cx.notify();
    }

    fn quit_and_reset_tor_state(&mut self, cx: &mut Context<'_, Self>) {
        if !self.tor_state_reset_confirming {
            return;
        }
        if reset_tor_state_and_quit(&self.options.db_path, || cx.quit()).is_err() {
            self.network_status_error = Some(Arc::from(GatewayNetworkError::ResetFailed.message()));
            self.tor_state_reset_confirming = false;
            cx.notify();
        }
    }
}

pub(super) fn reset_tor_state_and_quit(
    path: &std::path::Path,
    quit: impl FnOnce(),
) -> eyre::Result<()> {
    request_tor_state_reset(path)?;
    quit();
    Ok(())
}

async fn wallet_root_shutdown_requested(shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    shutdown.changed().await.is_err() || *shutdown.borrow()
}

pub(super) async fn query_exit_ip_through_tor(proxy_url: reqwest::Url) -> eyre::Result<IpAddr> {
    let proxy = reqwest::Proxy::all(proxy_url.as_str())
        .wrap_err_with(|| format!("invalid Tor proxy URL {proxy_url}"))?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .pool_max_idle_per_host(0)
        .build()
        .wrap_err("build one-shot Tor exit IP query client")?;
    let response = client
        .get(TOR_EXIT_IP_QUERY_URL)
        .timeout(TOR_EXIT_IP_QUERY_TIMEOUT)
        .send()
        .await
        .wrap_err("query Tor exit IP")?
        .error_for_status()
        .wrap_err("check.torproject.org returned an error status")?;
    let body = response
        .text()
        .await
        .wrap_err("read Tor exit IP response")?;
    parse_tor_exit_ip_response(&body)
}

fn parse_tor_exit_ip_response(body: &str) -> eyre::Result<IpAddr> {
    let response: serde_json::Value =
        serde_json::from_str(body).wrap_err("parse check.torproject.org response")?;
    let ip = response
        .get("IP")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| eyre::eyre!("check.torproject.org response did not include an IP field"))?;
    ip.parse::<IpAddr>()
        .wrap_err("check.torproject.org returned a non-IP address")
}

pub(super) async fn retry_tor_bootstrap(http: &HttpContext, runtime: &Handle) {
    let Some(arti_client) = http.arti_client() else {
        return;
    };

    let retry = runtime.spawn(async move {
        tokio::time::timeout(TOR_HEALTH_RETRY_TIMEOUT, arti_client.bootstrap()).await
    });
    match retry.await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => {
            tracing::debug!(%error, "Tor bootstrap retry failed during health check");
        }
        Ok(Err(_elapsed)) => {
            tracing::debug!(
                timeout_secs = TOR_HEALTH_RETRY_TIMEOUT.as_secs(),
                "Tor bootstrap retry still pending during health check"
            );
        }
        Err(error) => {
            tracing::warn!(%error, "Tor bootstrap retry task failed during health check");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity_snapshot(generation: u64, downloaded_bytes: u64) -> TorBridgeActivitySnapshot {
        TorBridgeActivitySnapshot {
            generation,
            downloaded_bytes,
            connecting_streams: 0,
            active_streams: 0,
            successful_connections: 0,
            failed_connections: 0,
            recent_connection_sample_count: 0,
            recent_successful_sample_count: 0,
            median_setup_duration: None,
            last_activity_age: None,
            session_duration: Duration::ZERO,
        }
    }

    #[test]
    fn reset_quits_only_after_the_marker_is_written() {
        let path =
            std::env::temp_dir().join(format!("network-reset-{:032x}", rand::random::<u128>()));
        let quits = std::cell::Cell::new(0);
        std::fs::write(&path, b"blocked directory").unwrap();
        assert!(reset_tor_state_and_quit(&path, || quits.set(quits.get() + 1)).is_err());
        assert_eq!(quits.get(), 0);
        std::fs::remove_file(&path).unwrap();
        reset_tor_state_and_quit(&path, || {
            assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
            quits.set(quits.get() + 1);
        })
        .unwrap();
        assert_eq!(quits.get(), 1);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn download_rate_sampler_reports_first_sample_and_idle_zero() {
        let mut sampler = TorBridgeActivitySampler::new();
        let start = Instant::now();
        let first = activity_snapshot(1, 100);

        assert_eq!(sampler.sample(Some(&first), start), None);
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(1, 100)),
                start + Duration::from_secs(1)
            ),
            Some(0)
        );
    }

    #[test]
    fn download_rate_sampler_smooths_weighted_intervals_and_clips_oldest() {
        let start = Instant::now();
        let mut sampler = TorBridgeActivitySampler::new();
        assert_eq!(sampler.sample(Some(&activity_snapshot(1, 0)), start), None);
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(1, 100)),
                start + Duration::from_secs(1)
            ),
            Some(100)
        );
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(1, 300)),
                start + Duration::from_secs(2)
            ),
            Some(150)
        );
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(1, 600)),
                start + Duration::from_secs(6)
            ),
            Some(100)
        );

        let mut partial_sampler = TorBridgeActivitySampler::new();
        assert_eq!(
            partial_sampler.sample(Some(&activity_snapshot(1, 0)), start),
            None
        );
        assert_eq!(
            partial_sampler.sample(
                Some(&activity_snapshot(1, 100)),
                start + Duration::from_secs(4),
            ),
            Some(25)
        );
        assert_eq!(
            partial_sampler.sample(
                Some(&activity_snapshot(1, 200)),
                start + Duration::from_secs(6),
            ),
            Some(35)
        );

        let mut long_sampler = TorBridgeActivitySampler::new();
        assert_eq!(
            long_sampler.sample(Some(&activity_snapshot(1, 0)), start),
            None
        );
        assert_eq!(
            long_sampler.sample(
                Some(&activity_snapshot(1, 1_000)),
                start + Duration::from_secs(10),
            ),
            Some(100)
        );
        assert!(long_sampler.intervals.len() <= TOR_ACTIVITY_INTERVAL_LIMIT);

        let mut bounded_sampler = TorBridgeActivitySampler::new();
        assert_eq!(
            bounded_sampler.sample(Some(&activity_snapshot(1, 0)), start),
            None
        );
        for index in 1..=32 {
            let _ = bounded_sampler.sample(
                Some(&activity_snapshot(1, index * 100)),
                start + Duration::from_secs(index),
            );
        }
        assert!(bounded_sampler.intervals.len() <= TOR_ACTIVITY_INTERVAL_LIMIT);
    }

    #[test]
    fn download_rate_sampler_resets_on_missing_generation_and_rollback() {
        let start = Instant::now();
        let mut sampler = TorBridgeActivitySampler::new();
        assert_eq!(
            sampler.sample(Some(&activity_snapshot(1, 100)), start),
            None
        );
        assert_eq!(sampler.sample(None, start + Duration::from_secs(1)), None);
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(1, 200)),
                start + Duration::from_secs(2)
            ),
            None
        );
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(2, 0)),
                start + Duration::from_secs(3)
            ),
            None
        );
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(2, 20)),
                start + Duration::from_secs(4)
            ),
            Some(20)
        );
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(2, 10)),
                start + Duration::from_secs(5)
            ),
            None
        );
        assert_eq!(
            sampler.sample(
                Some(&activity_snapshot(2, 30)),
                start + Duration::from_secs(6)
            ),
            Some(20)
        );
    }

    #[test]
    fn exit_ip_query_completion_requires_current_query_token() {
        let generation = next_tor_exit_ip_query_generation(7);
        let querying = TorExitIpQueryState::Querying;
        assert!(tor_exit_ip_query_completion_is_current(
            generation, generation, &querying
        ));
        assert!(!tor_exit_ip_query_completion_is_current(
            generation,
            generation - 1,
            &querying
        ));
        assert!(!tor_exit_ip_query_completion_is_current(
            generation,
            generation,
            &TorExitIpQueryState::Idle
        ));
        assert_eq!(next_tor_exit_ip_query_generation(u64::MAX), u64::MAX);
    }

    #[test]
    fn tor_exit_ip_response_requires_a_valid_json_ip_field() {
        assert_eq!(
            parse_tor_exit_ip_response(r#"{"IsTor":false,"IP":"1.2.3.4"}"#).unwrap(),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );

        for response in ["not json", r#"{"IsTor":true}"#, r#"{"IP":"not-an-ip"}"#] {
            assert!(parse_tor_exit_ip_response(response).is_err());
        }
    }
}
