//! Authenticated, volatile network observations and per-view command admission.
use super::{DappProvider, GatewayWalletState, PeerId, valid_id};
use crate::{
    TorBridgeActivitySnapshot, WalletNetworkHealth, WalletNetworkHealthCause,
    WalletNetworkHealthState, WalletNetworkMode,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const MAX_VIEWS: usize = 16;
const MAX_VIEW_LIFETIMES: usize = 128;
const MAX_REQUESTS: usize = 32;
const MAX_DISPLAY_INTEGER: u64 = (1 << 53) - 1;

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayNetworkStatus {
    TorReady,
    TorReconnecting,
    TorDegraded,
    Proxy,
    Direct,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayNetworkView {
    pub context_revision: String,
    pub status: GatewayNetworkStatus,
    pub detail: String,
    pub runtime_warning: bool,
    pub activity: Option<GatewayNetworkActivity>,
    pub download_rate: Option<u64>,
    /// Enabled-chain RPC and archive endpoints that bypass Tor or the proxy.
    pub direct_rpc_endpoints: usize,
    /// The desktop's explicit Tor rotation, pending or last failed.
    pub rotation: Option<GatewayTorRotation>,
}

/// Explicit Tor rotation state shared by every network control of the wallet root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GatewayTorRotation {
    InProgress,
    Failed { error: GatewayNetworkError },
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayNetworkActivity {
    pub generation: u64,
    pub session_duration_ms: u64,
    pub downloaded_bytes: u64,
    pub recent_connection_sample_count: usize,
    pub recent_successful_sample_count: usize,
    pub successful_connections: u64,
    pub failed_connections: u64,
    pub median_setup_duration_ms: Option<u64>,
    pub last_activity_age_ms: Option<u64>,
}

impl GatewayNetworkView {
    /// Reuses desktop observations and redacted descriptions, excluding the internal Tor bridge.
    #[must_use]
    pub fn new(
        context_revision: String,
        health: &WalletNetworkHealth,
        activity: Option<&TorBridgeActivitySnapshot>,
        download_rate: Option<u64>,
    ) -> Self {
        let status = match (health.mode, health.state) {
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Ready) => {
                GatewayNetworkStatus::TorReady
            }
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Reconnecting) => {
                GatewayNetworkStatus::TorReconnecting
            }
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Degraded) => {
                GatewayNetworkStatus::TorDegraded
            }
            (WalletNetworkMode::Proxy, _) => GatewayNetworkStatus::Proxy,
            (WalletNetworkMode::Direct, _) => GatewayNetworkStatus::Direct,
        };
        let millis = |duration: std::time::Duration| {
            duration.as_millis().min(u128::from(MAX_DISPLAY_INTEGER)) as u64
        };
        Self {
            context_revision,
            status,
            detail: if health.mode == WalletNetworkMode::Tor {
                String::new()
            } else {
                health.detail.chars().take(512).collect()
            },
            runtime_warning: matches!(
                health.cause,
                WalletNetworkHealthCause::TorRuntimeSlow
                    | WalletNetworkHealthCause::TorRuntimeUnreliable
            ),
            activity: (health.mode == WalletNetworkMode::Tor)
                .then_some(activity)
                .flatten()
                .map(|snapshot| GatewayNetworkActivity {
                    generation: snapshot.generation.min(MAX_DISPLAY_INTEGER),
                    session_duration_ms: millis(snapshot.session_duration),
                    downloaded_bytes: snapshot.downloaded_bytes.min(MAX_DISPLAY_INTEGER),
                    recent_connection_sample_count: snapshot.recent_connection_sample_count,
                    recent_successful_sample_count: snapshot.recent_successful_sample_count,
                    successful_connections: snapshot
                        .successful_connections
                        .min(MAX_DISPLAY_INTEGER),
                    failed_connections: snapshot.failed_connections.min(MAX_DISPLAY_INTEGER),
                    median_setup_duration_ms: snapshot.median_setup_duration.map(millis),
                    last_activity_age_ms: snapshot.last_activity_age.map(millis),
                }),
            download_rate: if health.mode == WalletNetworkMode::Tor {
                download_rate.map(|value| value.min(MAX_DISPLAY_INTEGER))
            } else {
                None
            },
            direct_rpc_endpoints: 0,
            rotation: None,
        }
    }
    #[must_use]
    pub const fn is_tor(&self) -> bool {
        matches!(
            self.status,
            GatewayNetworkStatus::TorReady
                | GatewayNetworkStatus::TorReconnecting
                | GatewayNetworkStatus::TorDegraded
        )
    }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayNetworkCommand {
    Open {
        view_id: String,
    },
    Close {
        view_id: String,
    },
    CancelQuery {
        view_id: String,
    },
    Run {
        view_id: String,
        request_id: String,
        operation: GatewayNetworkOperation,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum GatewayNetworkOperation {
    NewTorSession,
    QueryExitIp,
    QuitAndReset,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GatewayNetworkOutcome {
    Done,
    ExitIp { ip: IpAddr },
    Failed { error: GatewayNetworkError },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayNetworkError {
    Unavailable,
    NewSessionFailed,
    /// The Tor session changed, but the replacement Waku runtime did not start.
    WakuUnavailable,
    ExitIpFailed,
    ResetFailed,
}

impl GatewayNetworkError {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "Network control is unavailable. Reopen the popover and try again."
            }
            Self::NewSessionFailed => "Could not start a new Tor session. Try again.",
            Self::WakuUnavailable => {
                "Started a new Tor session, but the broadcaster network could not restart. Try again later."
            }
            Self::ExitIpFailed => "Could not query the exit IP through Tor. Try again.",
            Self::ResetFailed => "Could not request the Tor state reset. The wallet remains open.",
        }
    }
}
impl Serialize for GatewayNetworkError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.message())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayNetworkResult {
    pub view_id: String,
    pub request_id: String,
    pub context_revision: String,
    pub operation: GatewayNetworkOperation,
    pub outcome: GatewayNetworkOutcome,
}

/// Native dispatch and completion retain this identity, never browser-provided runtime handles.
#[derive(Clone)]
pub struct GatewayNetworkRequest {
    session: u64,
    view_id: String,
    request_id: String,
    context_revision: String,
    operation: GatewayNetworkOperation,
    live: Arc<AtomicBool>,
    view_live: Arc<AtomicBool>,
    authority: tokio::sync::watch::Receiver<GatewayWalletState>,
    wallet: Arc<GatewayWalletState>,
}
impl GatewayNetworkRequest {
    #[must_use]
    pub fn is_current(&self, revision: &str) -> bool {
        self.context_revision == revision
            && self.live.load(Ordering::Acquire)
            && self.view_live.load(Ordering::Acquire)
            && self.authority.borrow().same_authority(&self.wallet)
            && self.authority.borrow().view.is_some()
    }
    #[must_use]
    pub const fn operation(&self) -> GatewayNetworkOperation {
        self.operation
    }
}

struct NetworkPending {
    id: String,
    live: Arc<AtomicBool>,
}
impl Drop for NetworkPending {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

struct NetworkView {
    live: Arc<AtomicBool>,
    requests: HashSet<String>,
    pending: BTreeMap<GatewayNetworkOperation, NetworkPending>,
    results: BTreeMap<GatewayNetworkOperation, GatewayNetworkResult>,
}
impl Drop for NetworkView {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}
#[derive(Default)]
pub(super) struct NetworkViews {
    views: HashMap<(u64, String), NetworkView>,
    seen: HashMap<u64, HashSet<String>>,
}
impl NetworkViews {
    pub(super) fn retire_sessions(&mut self, live: impl Fn(u64) -> bool) {
        self.views.retain(|(session, _), _| live(*session));
        self.seen.retain(|session, _| live(*session));
    }
    pub(super) fn retire_context(&mut self) {
        self.views.clear();
    }
}

impl DappProvider {
    pub(in crate::gateway) fn network_command(
        &mut self,
        session: u64,
        peer: PeerId,
        generation: u64,
        revision: &str,
        command: GatewayNetworkCommand,
    ) -> Option<GatewayNetworkRequest> {
        let authority = self.authority.borrow();
        if generation != self.generation
            || self.ui_peers.get(&session) != Some(&peer)
            || self.wallet.view.is_none()
            || !authority.same_authority(&self.wallet)
            || authority
                .network_view
                .as_ref()
                .map(|view| view.context_revision.as_str())
                != Some(revision)
            || self
                .wallet
                .network_view
                .as_ref()
                .map(|view| view.context_revision.as_str())
                != Some(revision)
        {
            return None;
        }
        drop(authority);
        match command {
            GatewayNetworkCommand::Open { view_id } => {
                let seen = self.network_views.seen.entry(session).or_default();
                if !valid_id(&view_id)
                    || seen.contains(&view_id)
                    || seen.len() >= MAX_VIEW_LIFETIMES
                    || self
                        .network_views
                        .views
                        .keys()
                        .filter(|(owner, _)| *owner == session)
                        .count()
                        >= MAX_VIEWS
                {
                    return None;
                }
                seen.insert(view_id.clone());
                self.network_views.views.insert(
                    (session, view_id),
                    NetworkView {
                        live: Arc::new(AtomicBool::new(true)),
                        requests: HashSet::new(),
                        pending: BTreeMap::new(),
                        results: BTreeMap::new(),
                    },
                );
            }
            GatewayNetworkCommand::CancelQuery { view_id } => {
                let view = self.network_views.views.get_mut(&(session, view_id))?;
                view.pending.remove(&GatewayNetworkOperation::QueryExitIp);
                view.results.remove(&GatewayNetworkOperation::QueryExitIp);
            }
            GatewayNetworkCommand::Close { view_id } => {
                self.network_views.views.remove(&(session, view_id));
            }
            GatewayNetworkCommand::Run {
                view_id,
                request_id,
                operation,
            } => {
                if !self
                    .wallet
                    .network_view
                    .as_ref()
                    .is_some_and(GatewayNetworkView::is_tor)
                {
                    return None;
                }
                let view = self
                    .network_views
                    .views
                    .get_mut(&(session, view_id.clone()))?;
                if !valid_id(&request_id)
                    || view.requests.contains(&request_id)
                    || view.requests.len() >= MAX_REQUESTS
                    || view.pending.contains_key(&operation)
                {
                    return None;
                }
                view.requests.insert(request_id.clone());
                let live = Arc::new(AtomicBool::new(true));
                view.pending.insert(
                    operation,
                    NetworkPending {
                        id: request_id.clone(),
                        live: live.clone(),
                    },
                );
                view.results.remove(&operation);
                return Some(GatewayNetworkRequest {
                    session,
                    view_id,
                    request_id,
                    context_revision: revision.to_owned(),
                    operation,
                    live,
                    view_live: view.live.clone(),
                    authority: self.authority.clone(),
                    wallet: Arc::new(self.wallet.clone()),
                });
            }
        }
        self.push_ui(session);
        None
    }

    pub(in crate::gateway) fn complete_network_request(
        &mut self,
        request: GatewayNetworkRequest,
        outcome: GatewayNetworkOutcome,
    ) {
        let authority = self.authority.borrow();
        if !authority.same_authority(&self.wallet)
            || !authority
                .network_view
                .as_ref()
                .is_some_and(|view| request.is_current(&view.context_revision))
        {
            return;
        }
        drop(authority);
        let Some(view) = self
            .network_views
            .views
            .get_mut(&(request.session, request.view_id.clone()))
        else {
            return;
        };
        if !Arc::ptr_eq(&request.view_live, &view.live)
            || !view.pending.get(&request.operation).is_some_and(|pending| {
                pending.id == request.request_id && Arc::ptr_eq(&request.live, &pending.live)
            })
        {
            return;
        }
        view.pending.remove(&request.operation);
        view.results.insert(
            request.operation,
            GatewayNetworkResult {
                view_id: request.view_id,
                request_id: request.request_id,
                context_revision: request.context_revision,
                operation: request.operation,
                outcome,
            },
        );
        self.push_ui(request.session);
    }

    pub(super) fn network_results(&self, session: u64) -> Vec<GatewayNetworkResult> {
        let mut results: Vec<_> = self
            .network_views
            .views
            .iter()
            .filter(|((owner, _), _)| *owner == session)
            .flat_map(|(_, view)| view.results.values().cloned())
            .collect();
        results.sort_by(|left, right| left.view_id.cmp(&right.view_id));
        results
    }
    pub(super) fn retire_network_context(&mut self, wallet: &GatewayWalletState) {
        if !self.wallet.same_authority(wallet)
            || self
                .wallet
                .network_view
                .as_ref()
                .map(|view| &view.context_revision)
                != wallet
                    .network_view
                    .as_ref()
                    .map(|view| &view.context_revision)
        {
            self.network_views.retire_context();
        }
    }
}

#[cfg(test)]
// provider.rs mounts this file with #[path], so a plain `mod tests;` would resolve to gateway/tests.rs
#[path = "network/tests.rs"]
mod tests;
