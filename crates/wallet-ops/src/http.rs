use std::collections::VecDeque;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{
    Arc, Mutex, RwLock, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{io, pin::Pin};

use arti_client::TorClient;
use arti_client::config::TorClientConfigBuilder;
use arti_client::status::BootstrapStatus;
use eyre::{Result, WrapErr, eyre};
use reqwest::Url;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tor_rtcompat::PreferredRuntime;
use trustless_artifacts::GatewayPool;

use crate::rpc_broker::{RpcBroker, RpcClients, RpcTransport};

const ARTI_DIR: &str = "arti";
const ARTI_STATE_DIR: &str = "state";
const ARTI_CACHE_DIR: &str = "cache";
const TOR_STATE_RESET_MARKER_FILE: &str = ".reset-tor-state";
const SOCKS_VERSION: u8 = 0x05;
const SOCKS_NO_AUTH: u8 = 0x00;
const SOCKS_NO_ACCEPTABLE_METHODS: u8 = 0xff;
const SOCKS_CMD_CONNECT: u8 = 0x01;
const SOCKS_ADDR_IPV4: u8 = 0x01;
const SOCKS_ADDR_DOMAIN: u8 = 0x03;
const SOCKS_ADDR_IPV6: u8 = 0x04;
const SOCKS_REPLY_SUCCEEDED: u8 = 0x00;
const SOCKS_REPLY_GENERAL_FAILURE: u8 = 0x01;
const SOCKS_REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const SOCKS_REPLY_ADDR_NOT_SUPPORTED: u8 = 0x08;
const TOR_BOOTSTRAP_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const SOCKS_ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
const DIRECT_PROXY_RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TOR_RPC_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

pub type WalletTorClient = Arc<TorClient<PreferredRuntime>>;
pub type WalletTorClientProvider = Arc<dyn Fn() -> Option<WalletTorClient> + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalletNetworkMode {
    Tor,
    Proxy,
    Direct,
}

impl WalletNetworkMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tor => "tor",
            Self::Proxy => "proxy",
            Self::Direct => "direct",
        }
    }

    #[must_use]
    pub const fn status_label(self) -> &'static str {
        match self {
            Self::Tor => "Tor",
            Self::Proxy => "Proxy mode",
            Self::Direct => "Direct mode",
        }
    }
}

impl fmt::Display for WalletNetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WalletNetworkMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "tor" => Ok(Self::Tor),
            "proxy" => Ok(Self::Proxy),
            "direct" => Ok(Self::Direct),
            other => Err(format!(
                "unsupported network mode {other:?}; expected tor, proxy, or direct"
            )),
        }
    }
}

pub struct WalletNetworkConfig<'a> {
    pub network_mode: Option<WalletNetworkMode>,
    pub proxy: Option<&'a Url>,
    pub data_dir: &'a Path,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalletNetworkProgressStage {
    ResolvingMode,
    ConfiguringNetwork,
    PreparingTorStorage,
    BootstrappingTor,
    StartingTorBridge,
    Ready,
}

impl WalletNetworkProgressStage {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ResolvingMode => "Resolving network mode",
            Self::ConfiguringNetwork => "Configuring network",
            Self::PreparingTorStorage => "Preparing Tor storage",
            Self::BootstrappingTor => "Bootstrapping Tor",
            Self::StartingTorBridge => "Starting Tor bridge",
            Self::Ready => "Network ready",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletNetworkProgress {
    pub mode: Option<WalletNetworkMode>,
    pub stage: WalletNetworkProgressStage,
    pub percent: Option<u8>,
    pub detail: Arc<str>,
}

impl WalletNetworkProgress {
    #[must_use]
    pub fn initial() -> Self {
        Self::new(
            None,
            WalletNetworkProgressStage::ResolvingMode,
            None,
            "Preparing wallet network",
        )
    }

    #[must_use]
    pub fn new(
        mode: Option<WalletNetworkMode>,
        stage: WalletNetworkProgressStage,
        percent: Option<u8>,
        detail: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            mode,
            stage,
            percent,
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalletNetworkHealthState {
    Ready,
    Reconnecting,
    Degraded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalletNetworkHealthCause {
    None,
    TorBootstrap,
    TorRuntimeSlow,
    TorRuntimeUnreliable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletNetworkHealth {
    pub mode: WalletNetworkMode,
    pub state: WalletNetworkHealthState,
    pub detail: Arc<str>,
    pub cause: WalletNetworkHealthCause,
}

impl WalletNetworkHealth {
    #[must_use]
    pub fn new(
        mode: WalletNetworkMode,
        state: WalletNetworkHealthState,
        detail: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            mode,
            state,
            detail: detail.into(),
            cause: WalletNetworkHealthCause::None,
        }
    }

    #[must_use]
    pub fn with_cause(
        mode: WalletNetworkMode,
        state: WalletNetworkHealthState,
        detail: impl Into<Arc<str>>,
        cause: WalletNetworkHealthCause,
    ) -> Self {
        Self {
            mode,
            state,
            detail: detail.into(),
            cause,
        }
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        match (self.mode, self.state) {
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Ready) => "Tor",
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Reconnecting) => "Tor reconnecting",
            (WalletNetworkMode::Tor, WalletNetworkHealthState::Degraded) => "Tor degraded",
            (WalletNetworkMode::Proxy, _) => "Proxy mode",
            (WalletNetworkMode::Direct, _) => "Direct mode",
        }
    }
}

const TOR_OBSERVATION_LIMIT: usize = 64;
const TOR_OBSERVATION_AGE: Duration = Duration::from_mins(2);
const TOR_MIN_OBSERVATIONS: usize = 8;
const TOR_FAILURE_MINIMUM: usize = 4;
const TOR_SETUP_THRESHOLD: Duration = Duration::from_secs(8);
const TOR_RECOVERY_SUCCESS_STREAK: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TorConnectionObservation {
    generation: u64,
    succeeded: bool,
    completed_at: Instant,
    setup_duration: Duration,
    admitted_sequence: u64,
}

impl TorConnectionObservation {
    const fn new(
        generation: u64,
        succeeded: bool,
        completed_at: Instant,
        setup_duration: Duration,
    ) -> Self {
        Self {
            generation,
            succeeded,
            completed_at,
            setup_duration,
            admitted_sequence: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TorRuntimeHealth {
    Ready,
    Slow,
    Unreliable,
}

#[derive(Debug)]
struct TorRuntimeHealthTracker {
    generation: u64,
    observations: VecDeque<TorConnectionObservation>,
    last_admitted_completed_at: Option<Instant>,
    next_admitted_sequence: u64,
    episode_boundary_sequence: Option<u64>,
    latched_cause: Option<TorRuntimeHealth>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TorRuntimeHealthReport {
    health: TorRuntimeHealth,
    recent_attempt_count: usize,
    recent_failed_attempt_count: usize,
    recent_successful_sample_count: usize,
    recent_successful_setup_p75: Option<Duration>,
}

impl TorRuntimeHealthTracker {
    const fn new(generation: u64) -> Self {
        Self {
            generation,
            observations: VecDeque::new(),
            last_admitted_completed_at: None,
            next_admitted_sequence: 1,
            episode_boundary_sequence: None,
            latched_cause: None,
        }
    }

    fn record_at(&mut self, mut observation: TorConnectionObservation, now: Instant) -> bool {
        if observation.generation != self.generation {
            return false;
        }
        if observation.completed_at > now {
            return false;
        }
        if self
            .last_admitted_completed_at
            .is_some_and(|last| observation.completed_at < last)
        {
            return false;
        }
        self.expire(now);
        if now.saturating_duration_since(observation.completed_at) > TOR_OBSERVATION_AGE {
            return false;
        }
        let Some(next_admitted_sequence) = self.next_admitted_sequence.checked_add(1) else {
            return false;
        };
        observation.admitted_sequence = self.next_admitted_sequence;
        self.next_admitted_sequence = next_admitted_sequence;
        self.last_admitted_completed_at = Some(observation.completed_at);
        self.observations.push_back(observation);
        if self.observations.len() > TOR_OBSERVATION_LIMIT {
            self.observations.pop_front();
        }
        self.transition(now);
        true
    }

    fn expire(&mut self, now: Instant) {
        self.observations.retain(|observation| {
            observation.completed_at <= now
                && now.duration_since(observation.completed_at) <= TOR_OBSERVATION_AGE
        });
        if self.observations.is_empty() {
            self.latched_cause = None;
            self.episode_boundary_sequence = None;
        }
    }

    fn classify(&self, now: Instant) -> TorRuntimeHealth {
        let (attempts, failures, mut successful_samples) = self.current_evidence(now);
        if attempts < TOR_MIN_OBSERVATIONS {
            return TorRuntimeHealth::Ready;
        }
        if failures >= TOR_FAILURE_MINIMUM && failures * 2 >= attempts {
            return TorRuntimeHealth::Unreliable;
        }

        if successful_samples.len() >= TOR_MIN_OBSERVATIONS
            && tor_setup_p75(&mut successful_samples).is_some_and(|p75| p75 >= TOR_SETUP_THRESHOLD)
        {
            return TorRuntimeHealth::Slow;
        }
        TorRuntimeHealth::Ready
    }

    #[cfg(test)]
    fn health(&mut self, now: Instant) -> TorRuntimeHealth {
        self.health_report(now).health
    }

    fn health_report(&mut self, now: Instant) -> TorRuntimeHealthReport {
        self.expire(now);
        self.transition(now);

        let health = self.latched_cause.unwrap_or(TorRuntimeHealth::Ready);
        let (recent_attempt_count, recent_failed_attempt_count, mut successful_samples) =
            self.current_evidence(now);
        let recent_successful_setup_p75 = tor_setup_p75(&mut successful_samples);
        TorRuntimeHealthReport {
            health,
            recent_attempt_count,
            recent_failed_attempt_count,
            recent_successful_sample_count: successful_samples.len(),
            recent_successful_setup_p75,
        }
    }

    fn observations(&mut self, now: Instant) -> Vec<TorConnectionObservation> {
        self.expire(now);
        self.observations.iter().copied().collect()
    }

    fn current_evidence(&self, now: Instant) -> (usize, usize, Vec<Duration>) {
        let mut attempts = 0;
        let mut failures = 0;
        let mut successful_samples = Vec::new();
        for observation in &self.observations {
            if observation.completed_at > now
                || now.duration_since(observation.completed_at) > TOR_OBSERVATION_AGE
            {
                continue;
            }
            attempts += 1;
            if observation.succeeded {
                successful_samples.push(observation.setup_duration);
            } else {
                failures += 1;
            }
        }
        (attempts, failures, successful_samples)
    }

    fn transition(&mut self, now: Instant) {
        match self.classify(now) {
            TorRuntimeHealth::Ready => {
                if self.latched_cause.is_some()
                    && self.trailing_post_boundary_successes() >= TOR_RECOVERY_SUCCESS_STREAK
                {
                    self.latched_cause = None;
                    self.episode_boundary_sequence = None;
                }
            }
            cause => {
                if self.latched_cause.is_none() {
                    self.episode_boundary_sequence = self
                        .observations
                        .back()
                        .map(|observation| observation.admitted_sequence);
                }
                self.latched_cause = Some(cause);
            }
        }
    }

    fn trailing_post_boundary_successes(&self) -> usize {
        let Some(boundary) = self.episode_boundary_sequence else {
            return 0;
        };
        let mut successes = 0_usize;
        for observation in self.observations.iter().rev() {
            if observation.admitted_sequence <= boundary || !observation.succeeded {
                break;
            }
            successes = successes.saturating_add(1);
        }
        successes.min(TOR_RECOVERY_SUCCESS_STREAK)
    }
}

fn tor_setup_p75(successful_samples: &mut [Duration]) -> Option<Duration> {
    if successful_samples.is_empty() {
        return None;
    }
    successful_samples.sort_unstable();
    let rank = (successful_samples.len() * 3).div_ceil(4);
    Some(successful_samples[rank - 1])
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TorBridgeActivitySnapshot {
    pub generation: u64,
    pub session_duration: Duration,
    pub downloaded_bytes: u64,
    pub connecting_streams: u64,
    pub active_streams: u64,
    pub successful_connections: u64,
    pub failed_connections: u64,
    pub recent_connection_sample_count: usize,
    pub recent_successful_sample_count: usize,
    pub median_setup_duration: Option<Duration>,
    pub last_activity_age: Option<Duration>,
}

struct TorBridgeActivity {
    generation: u64,
    activity_origin: Instant,
    downloaded_bytes: AtomicU64,
    connecting_streams: AtomicU64,
    active_streams: AtomicU64,
    successful_connections: AtomicU64,
    failed_connections: AtomicU64,
    last_activity_tick: AtomicU64,
    tracker: Mutex<TorRuntimeHealthTracker>,
}

impl TorBridgeActivity {
    fn new(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            generation,
            activity_origin: Instant::now(),
            downloaded_bytes: AtomicU64::new(0),
            connecting_streams: AtomicU64::new(0),
            active_streams: AtomicU64::new(0),
            successful_connections: AtomicU64::new(0),
            failed_connections: AtomicU64::new(0),
            last_activity_tick: AtomicU64::new(0),
            tracker: Mutex::new(TorRuntimeHealthTracker::new(generation)),
        })
    }

    fn begin_connection(self: &Arc<Self>) -> TorConnectingGuard {
        self.connecting_streams.fetch_add(1, Ordering::Relaxed);
        self.touch();
        TorConnectingGuard {
            activity: Arc::clone(self),
            started_at: Instant::now(),
        }
    }

    fn touch(&self) {
        let elapsed_nanos = Instant::now()
            .saturating_duration_since(self.activity_origin)
            .as_nanos();
        let tick = u64::try_from(elapsed_nanos).unwrap_or(u64::MAX).max(1);
        self.last_activity_tick.fetch_max(tick, Ordering::Relaxed);
    }

    fn record_connection(&self, succeeded: bool, setup_duration: Duration) {
        if succeeded {
            self.successful_connections.fetch_add(1, Ordering::Relaxed);
            self.active_streams.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed_connections.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut tracker) = self.tracker.lock() {
            let completed_at = Instant::now();
            let _ = tracker.record_at(
                TorConnectionObservation::new(
                    self.generation,
                    succeeded,
                    completed_at,
                    setup_duration,
                ),
                completed_at,
            );
        }
        self.touch();
    }

    fn finish_active_stream(&self) {
        let _ = self
            .active_streams
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_sub(1)
            });
        self.touch();
    }

    fn add_downloaded_bytes(&self, bytes: usize) {
        self.downloaded_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.touch();
    }

    fn snapshot(&self) -> TorBridgeActivitySnapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> TorBridgeActivitySnapshot {
        let observations = self
            .tracker
            .lock()
            .map(|mut tracker| tracker.observations(now))
            .unwrap_or_default();
        let mut successful: Vec<_> = observations
            .iter()
            .filter(|observation| observation.succeeded)
            .map(|observation| observation.setup_duration)
            .collect();
        successful.sort_unstable();
        let median_setup_duration = median_duration(&successful);
        let last_activity_age = self.last_activity_age(now);
        TorBridgeActivitySnapshot {
            generation: self.generation,
            session_duration: now.saturating_duration_since(self.activity_origin),
            downloaded_bytes: self.downloaded_bytes.load(Ordering::Relaxed),
            connecting_streams: self.connecting_streams.load(Ordering::Relaxed),
            active_streams: self.active_streams.load(Ordering::Relaxed),
            successful_connections: self.successful_connections.load(Ordering::Relaxed),
            failed_connections: self.failed_connections.load(Ordering::Relaxed),
            recent_connection_sample_count: observations.len(),
            recent_successful_sample_count: successful.len(),
            median_setup_duration,
            last_activity_age,
        }
    }

    fn runtime_health_report(&self) -> TorRuntimeHealthReport {
        let default_report = TorRuntimeHealthReport {
            health: TorRuntimeHealth::Ready,
            recent_attempt_count: 0,
            recent_failed_attempt_count: 0,
            recent_successful_sample_count: 0,
            recent_successful_setup_p75: None,
        };
        self.tracker.lock().map_or(default_report, |mut tracker| {
            tracker.health_report(Instant::now())
        })
    }

    fn last_activity_age(&self, now: Instant) -> Option<Duration> {
        let tick = self.last_activity_tick.load(Ordering::Relaxed);
        if tick == 0 {
            return None;
        }
        let elapsed_since_origin = now.saturating_duration_since(self.activity_origin);
        Some(elapsed_since_origin.saturating_sub(Duration::from_nanos(tick)))
    }
}

fn median_duration(values: &[Duration]) -> Option<Duration> {
    let middle = values.len() / 2;
    match values.len() {
        0 => None,
        length if length % 2 == 1 => values.get(middle).copied(),
        _ => {
            let lower = values.get(middle - 1).copied()?;
            let upper = values.get(middle).copied()?;
            Some(lower + upper.saturating_sub(lower) / 2)
        }
    }
}

/// Shared wallet network context built once from the selected privacy mode and
/// passed into wallet operations that issue network requests.
#[derive(Clone)]
pub struct HttpContext {
    /// Async HTTP client for non-chain HTTP traffic.
    pub client: reqwest::Client,
    /// Shared EVM JSON-RPC client used by the RPC broker and pool-backed workflows.
    pub rpc_client: reqwest::Client,
    /// JSON-RPC client for user-marked direct endpoints. It never uses Tor or a proxy; in direct
    /// mode it is `rpc_client`.
    pub direct_rpc_client: reqwest::Client,
    broker: Arc<RpcBroker>,
    gateway_pool: GatewayPool,
    /// Proxy URL retained for components that need an endpoint value. In Tor
    /// mode this is the internal SOCKS bridge URL, not a user-supplied
    /// external proxy.
    pub proxy_url: Option<Url>,
    pub user_proxy_url: Option<Url>,
    mode: WalletNetworkMode,
    arti_client: Option<WalletTorClient>,
    arti_state_dir: Option<PathBuf>,
    arti_cache_dir: Option<PathBuf>,
    socks_bridge: Option<Arc<ArtiSocksBridge>>,
    /// Live operation bridges, moved onto each new Tor session.
    operation_bridges: Arc<Mutex<Vec<Weak<ArtiSocksBridge>>>>,
    fail_closed: bool,
    /// Opens operation clients in place of [`Self::operation_http_client`]'s own.
    #[cfg(any(test, feature = "test-support"))]
    operation_clients_for_tests: Option<OperationClientsForTests>,
}

#[cfg(any(test, feature = "test-support"))]
type OperationClientsForTests = Arc<dyn Fn() -> Result<OperationHttpClient> + Send + Sync>;

impl HttpContext {
    /// Return the broker for submitted `eth_call` and `eth_getBalance` reads.
    #[must_use]
    pub fn rpc_broker(&self) -> Arc<RpcBroker> {
        self.broker.clone()
    }

    /// The JSON-RPC client for an endpoint with `transport`.
    #[must_use]
    pub const fn rpc_client_for(&self, transport: RpcTransport) -> &reqwest::Client {
        match transport {
            RpcTransport::Wallet => &self.rpc_client,
            RpcTransport::Direct => &self.direct_rpc_client,
        }
    }

    pub(crate) fn rpc_clients(&self) -> RpcClients {
        RpcClients::new(self.rpc_client.clone(), self.direct_rpc_client.clone())
    }

    #[must_use]
    pub const fn network_mode(&self) -> WalletNetworkMode {
        self.mode
    }

    #[must_use]
    pub const fn fail_closed(&self) -> bool {
        self.fail_closed
    }

    #[must_use]
    pub const fn network_status_label(&self) -> &'static str {
        self.mode.status_label()
    }

    #[must_use]
    pub fn network_status_detail(&self) -> String {
        self.network_health().detail.to_string()
    }

    #[must_use]
    pub fn network_health(&self) -> WalletNetworkHealth {
        match self.mode {
            WalletNetworkMode::Tor => self.tor_network_health(),
            WalletNetworkMode::Proxy | WalletNetworkMode::Direct => WalletNetworkHealth::new(
                self.mode,
                WalletNetworkHealthState::Ready,
                self.configured_network_status_detail(),
            ),
        }
    }

    #[must_use]
    pub fn tor_bridge_activity_snapshot(&self) -> Option<TorBridgeActivitySnapshot> {
        self.socks_bridge
            .as_ref()
            .and_then(|bridge| bridge.activity_snapshot())
    }

    fn configured_network_status_detail(&self) -> String {
        match self.mode {
            WalletNetworkMode::Tor => match self.proxy_url.as_ref() {
                Some(proxy) => format!(
                    "Ready. HTTP/RPC session #{} is routed through {proxy}",
                    self.tor_session_generation(),
                    proxy = redact_url_for_display(proxy)
                ),
                None => "HTTP bridge is unavailable".to_string(),
            },
            WalletNetworkMode::Proxy => match self.user_proxy_url.as_ref() {
                Some(proxy) => format!(
                    "HTTP is routed through {proxy}",
                    proxy = redact_url_for_display(proxy)
                ),
                None => "Missing proxy URL".to_string(),
            },
            WalletNetworkMode::Direct => {
                "Not Tor-protected; outbound requests use the network directly".to_string()
            }
        }
    }

    fn tor_network_health(&self) -> WalletNetworkHealth {
        let Some(arti_client) = self.arti_client() else {
            return WalletNetworkHealth::with_cause(
                WalletNetworkMode::Tor,
                WalletNetworkHealthState::Degraded,
                "Degraded. Tor client is unavailable",
                WalletNetworkHealthCause::TorBootstrap,
            );
        };

        self.tor_network_health_for_status(&arti_client.bootstrap_status())
    }

    fn tor_network_health_for_status(&self, status: &BootstrapStatus) -> WalletNetworkHealth {
        if status.ready_for_traffic() {
            if let Some(activity) = self.socks_bridge.as_ref() {
                let report = activity.runtime_health_report();
                match report.health {
                    TorRuntimeHealth::Slow => {
                        return WalletNetworkHealth::with_cause(
                            WalletNetworkMode::Tor,
                            WalletNetworkHealthState::Degraded,
                            format_tor_runtime_health_detail("slow", report),
                            WalletNetworkHealthCause::TorRuntimeSlow,
                        );
                    }
                    TorRuntimeHealth::Unreliable => {
                        return WalletNetworkHealth::with_cause(
                            WalletNetworkMode::Tor,
                            WalletNetworkHealthState::Degraded,
                            format_tor_runtime_health_detail("unreliable", report),
                            WalletNetworkHealthCause::TorRuntimeUnreliable,
                        );
                    }
                    TorRuntimeHealth::Ready => {}
                }
            }
            return WalletNetworkHealth::new(
                WalletNetworkMode::Tor,
                WalletNetworkHealthState::Ready,
                self.configured_network_status_detail(),
            );
        }

        let (state, prefix) = if status.blocked().is_some() {
            (WalletNetworkHealthState::Degraded, "Degraded")
        } else {
            (WalletNetworkHealthState::Reconnecting, "Reconnecting")
        };

        WalletNetworkHealth::with_cause(
            WalletNetworkMode::Tor,
            state,
            format!("{prefix}. {status}"),
            WalletNetworkHealthCause::TorBootstrap,
        )
    }

    #[must_use]
    pub fn arti_client(&self) -> Option<WalletTorClient> {
        if let Some(socks_bridge) = self.socks_bridge.as_ref() {
            match socks_bridge.active_client() {
                Ok(client) => return Some(client),
                Err(error) => {
                    tracing::warn!(%error, "failed to read active Tor session client");
                }
            }
        }
        self.arti_client.clone()
    }

    #[must_use]
    pub fn arti_client_provider(&self) -> Option<WalletTorClientProvider> {
        if let Some(socks_bridge) = self.socks_bridge.as_ref() {
            return Some(ArtiSocksBridge::session_client_provider(
                &socks_bridge.session,
            ));
        }

        let arti_client = self.arti_client.clone()?;
        Some(Arc::new(move || Some(arti_client.clone())))
    }

    pub fn start_new_tor_session(&self) -> Result<u64> {
        if self.mode != WalletNetworkMode::Tor {
            return Err(eyre!("new Tor session is only available in Tor mode"));
        }
        let socks_bridge = self
            .socks_bridge
            .as_ref()
            .ok_or_else(|| eyre!("new Tor session requires the internal SOCKS bridge"))?;
        // Held across both steps so a concurrently opened operation bridge joins this session.
        let mut operation_bridges = self.lock_operation_bridges()?;
        let generation = socks_bridge.new_isolated_session()?;
        operation_bridges.retain(|bridge| bridge.strong_count() > 0);
        for bridge in operation_bridges.iter().filter_map(Weak::upgrade) {
            bridge.join_session(generation)?;
        }
        drop(operation_bridges);
        self.gateway_pool.reset();
        Ok(generation)
    }

    fn lock_operation_bridges(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Vec<Weak<ArtiSocksBridge>>>> {
        self.operation_bridges
            .lock()
            .map_err(|_| eyre!("operation Tor bridge list lock is poisoned"))
    }

    /// Build an HTTP client for the traffic of one wallet operation, such as one swap.
    ///
    /// In built-in Tor mode the operation gets its own loopback SOCKS bridge over an
    /// `isolated_client()` handle of the shared Arti instance. Arti gives each `TorClient`
    /// handle its own `IsolationToken`, and streams with different tokens never share a
    /// circuit. Sync, RPC, broadcaster, and other operations' traffic use other handles, so
    /// none of it shares circuits with this operation. The clients use the same proxy-only
    /// builder as the shared clients, so a failed bridge fails the request rather than
    /// bypassing Tor. Each client's connector holds the bridge, which stops when the last clone
    /// of either client is dropped. [`Self::start_new_tor_session`] moves every live bridge onto
    /// a fresh isolated client.
    ///
    /// Proxy and direct modes can't isolate one operation's traffic. They return the shared
    /// clients and report isolation as unavailable so that callers can disclose it.
    pub async fn operation_http_client(&self) -> Result<OperationHttpClient> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(open) = &self.operation_clients_for_tests {
            return open();
        }
        match self.mode {
            WalletNetworkMode::Tor => {
                if self.socks_bridge.is_none() {
                    return Err(eyre!(
                        "operation network isolation requires the internal SOCKS bridge"
                    ));
                }
                let arti_client = self
                    .arti_client()
                    .ok_or_else(|| eyre!("Tor client is unavailable"))?;
                let bridge = Arc::new(
                    ArtiSocksBridge::start(
                        arti_client.isolated_client(),
                        self.tor_session_generation(),
                    )
                    .await
                    .wrap_err("start operation Arti SOCKS bridge")?,
                );
                {
                    let mut operation_bridges = self.lock_operation_bridges()?;
                    // A session started while the bridge opened moves it on at once.
                    let generation = self.tor_session_generation();
                    if bridge.session_generation() != generation {
                        bridge.join_session(generation)?;
                    }
                    operation_bridges.retain(|bridge| bridge.strong_count() > 0);
                    operation_bridges.push(Arc::downgrade(&bridge));
                }
                let proxy_url = Url::parse(&format!("socks5h://{}", bridge.local_addr()))
                    .wrap_err("build operation Arti SOCKS proxy URL")?;
                let builder = || -> Result<reqwest::ClientBuilder> {
                    let bridge = Arc::clone(&bridge);
                    // A no-op connector layer whose services keep the bridge running.
                    let hold_bridge = tower::util::MapErrLayer::new(
                        move |error: Box<dyn std::error::Error + Send + Sync>| {
                            let _ = &bridge;
                            error
                        },
                    );
                    Ok(
                        wallet_reqwest_client_builder(WalletNetworkMode::Tor, Some(&proxy_url))?
                            .connector_layer(hold_bridge),
                    )
                };
                Ok(OperationHttpClient {
                    client: builder()?.build().wrap_err("build operation HTTP client")?,
                    rpc_client: builder()?
                        .timeout(rpc_request_timeout(WalletNetworkMode::Tor))
                        .build()
                        .wrap_err("build operation RPC HTTP client")?,
                    isolation: OperationNetworkIsolation::Dedicated,
                    tor_client: Some(ArtiSocksBridge::session_client_provider(&bridge.session)),
                    bridge: Some(Arc::downgrade(&bridge)),
                    #[cfg(test)]
                    reisolations: Arc::default(),
                })
            }
            WalletNetworkMode::Proxy | WalletNetworkMode::Direct => Ok(OperationHttpClient {
                client: self.client.clone(),
                rpc_client: self.rpc_client.clone(),
                isolation: OperationNetworkIsolation::Unavailable(self.mode),
                tor_client: None,
                bridge: None,
                #[cfg(test)]
                reisolations: Arc::default(),
            }),
        }
    }

    #[must_use]
    pub fn gateway_pool(&self) -> GatewayPool {
        self.gateway_pool.clone()
    }

    #[must_use]
    pub fn tor_session_generation(&self) -> u64 {
        self.socks_bridge
            .as_ref()
            .map_or(0, |socks_bridge| socks_bridge.session_generation())
    }

    #[must_use]
    pub fn arti_state_dir(&self) -> Option<&Path> {
        self.arti_state_dir.as_deref()
    }

    #[must_use]
    pub fn arti_cache_dir(&self) -> Option<&Path> {
        self.arti_cache_dir.as_deref()
    }

    #[must_use]
    pub const fn has_internal_socks_bridge(&self) -> bool {
        self.socks_bridge.is_some()
    }

    #[cfg(test)]
    pub(crate) fn direct_for_tests() -> Self {
        Self::direct_for_tests_with_candidate_order(|_| {})
    }

    /// Like `direct_for_tests`, with `candidate_order` arranging the broker's endpoint ties.
    #[cfg(test)]
    pub(crate) fn direct_for_tests_with_candidate_order(candidate_order: fn(&mut [usize])) -> Self {
        let rpc_client = reqwest::Client::new();
        Self {
            client: reqwest::Client::new(),
            rpc_client: rpc_client.clone(),
            direct_rpc_client: rpc_client.clone(),
            broker: RpcBroker::spawn_with_candidate_order(
                rpc_client.clone(),
                rpc_client,
                candidate_order,
            )
            .expect("HttpContext test constructor requires an active Tokio runtime"),
            gateway_pool: GatewayPool::new(),
            proxy_url: None,
            user_proxy_url: None,
            mode: WalletNetworkMode::Direct,
            arti_client: None,
            arti_state_dir: None,
            arti_cache_dir: None,
            socks_bridge: None,
            operation_bridges: Arc::default(),
            fail_closed: false,
            operation_clients_for_tests: None,
        }
    }

    /// # Panics
    ///
    /// Panics without an active Tokio runtime.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_rpc_client_for_tests(rpc_client: reqwest::Client, mode: WalletNetworkMode) -> Self {
        Self::with_rpc_clients_for_tests(rpc_client.clone(), rpc_client, mode)
    }

    /// Like [`Self::with_rpc_client_for_tests`], with a separate client for direct endpoints.
    ///
    /// # Panics
    ///
    /// Panics without an active Tokio runtime.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn with_rpc_clients_for_tests(
        rpc_client: reqwest::Client,
        direct_rpc_client: reqwest::Client,
        mode: WalletNetworkMode,
    ) -> Self {
        #[cfg(test)]
        let broker = RpcBroker::spawn_with_candidate_order(
            rpc_client.clone(),
            direct_rpc_client.clone(),
            |_| {},
        );
        // The fixed-order constructor is crate-test-only; downstream tests use the default order.
        #[cfg(not(test))]
        let broker = RpcBroker::spawn(rpc_client.clone(), direct_rpc_client.clone());
        let broker = broker.expect("HttpContext test constructor requires an active Tokio runtime");
        Self {
            client: reqwest::Client::new(),
            direct_rpc_client,
            rpc_client,
            broker,
            gateway_pool: GatewayPool::new(),
            proxy_url: None,
            user_proxy_url: None,
            mode,
            arti_client: None,
            arti_state_dir: None,
            arti_cache_dir: None,
            socks_bridge: None,
            operation_bridges: Arc::default(),
            fail_closed: mode != WalletNetworkMode::Direct,
            operation_clients_for_tests: None,
        }
    }

    /// Opens every operation client with `open` instead of from the network mode.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_operation_clients_for_tests(
        mut self,
        open: impl Fn() -> Result<OperationHttpClient> + Send + Sync + 'static,
    ) -> Self {
        self.operation_clients_for_tests = Some(Arc::new(open));
        self
    }
}

/// Whether one wallet operation's traffic is isolated from other wallet traffic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationNetworkIsolation {
    /// Built-in Tor with circuits dedicated to this operation.
    Dedicated,
    /// The network mode can't isolate this operation from other wallet traffic.
    Unavailable(WalletNetworkMode),
}

/// HTTP route for one wallet operation, built by [`HttpContext::operation_http_client`].
///
/// It holds two clients over the same route: one for the operation's general HTTP, and one with
/// the wallet's RPC request timeout for its JSON-RPC. In built-in Tor mode the operation's
/// bridge runs as long as any clone of either client.
#[derive(Clone)]
pub struct OperationHttpClient {
    client: reqwest::Client,
    rpc_client: reqwest::Client,
    isolation: OperationNetworkIsolation,
    /// The operation bridge's current Tor client, for libp2p dials on the same isolation.
    tor_client: Option<WalletTorClientProvider>,
    bridge: Option<Weak<ArtiSocksBridge>>,
    /// Shared by clones, so tests can see that a clone was re-isolated.
    #[cfg(test)]
    reisolations: Arc<std::sync::atomic::AtomicUsize>,
}

impl OperationHttpClient {
    #[must_use]
    pub const fn isolation(&self) -> OperationNetworkIsolation {
        self.isolation
    }

    pub(crate) const fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(crate) const fn rpc_client(&self) -> &reqwest::Client {
        &self.rpc_client
    }

    /// Yields the client of the operation's own Tor bridge, which a new Tor session
    /// re-isolates. `None` for the shared clients of proxy and direct modes.
    #[must_use]
    pub fn tor_client_provider(&self) -> Option<WalletTorClientProvider> {
        self.tor_client.clone()
    }

    /// Gives the operation's own Tor bridge new circuits in the current session; a later
    /// [`HttpContext::start_new_tor_session`] still moves it on. Without a bridge it does nothing.
    pub fn reisolate(&self) -> Result<()> {
        #[cfg(test)]
        self.reisolations.fetch_add(1, Ordering::SeqCst);
        if let Some(bridge) = self.bridge.as_ref().and_then(Weak::upgrade) {
            bridge.reisolate()?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn reisolations_for_tests(&self) -> usize {
        self.reisolations.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn bridge(&self) -> Option<Arc<ArtiSocksBridge>> {
        self.bridge.as_ref().and_then(Weak::upgrade)
    }

    /// Uses `client` for both general HTTP and JSON-RPC.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_tests(client: reqwest::Client, isolation: OperationNetworkIsolation) -> Self {
        Self {
            rpc_client: client.clone(),
            client,
            isolation,
            tor_client: None,
            bridge: None,
            #[cfg(test)]
            reisolations: Arc::default(),
        }
    }

    /// Reports `provider` as this client's Tor provider.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_tor_client_provider_for_tests(
        mut self,
        provider: WalletTorClientProvider,
    ) -> Self {
        self.tor_client = Some(provider);
        self
    }

    /// A direct client of its own, leaked so that test closures and spawned futures can borrow
    /// it.
    #[cfg(test)]
    pub(crate) fn direct_for_tests() -> &'static Self {
        Box::leak(Box::new(Self::for_tests(
            reqwest::Client::new(),
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        )))
    }
}

impl fmt::Debug for OperationHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationHttpClient")
            .field("isolation", &self.isolation)
            .finish_non_exhaustive()
    }
}

/// One operation's HTTP client, opened on first use and shared by every clone.
#[derive(Clone)]
pub struct OperationHttpClientSlot {
    http: HttpContext,
    client: Arc<tokio::sync::OnceCell<OperationHttpClient>>,
}

impl OperationHttpClientSlot {
    #[must_use]
    pub fn new(http: HttpContext) -> Self {
        Self {
            http,
            client: Arc::default(),
        }
    }

    /// Opens the client on the first call; a failed open leaves the slot empty for a later retry.
    pub async fn client(&self) -> Result<OperationHttpClient> {
        self.client
            .get_or_try_init(|| self.http.operation_http_client())
            .await
            .cloned()
    }

    /// A slot already holding `client`.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_tests(http: HttpContext, client: OperationHttpClient) -> Self {
        Self {
            http,
            client: Arc::new(tokio::sync::OnceCell::from(client)),
        }
    }
}

impl fmt::Debug for OperationHttpClientSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperationHttpClientSlot")
            .finish_non_exhaustive()
    }
}

fn format_tor_runtime_health_detail(cause: &'static str, report: TorRuntimeHealthReport) -> String {
    let setup_duration = report
        .recent_successful_setup_p75
        .map_or_else(String::new, |p75| {
            format!(
                "; measured p75 connection setup: {} ms ({} successful samples)",
                p75.as_millis(),
                report.recent_successful_sample_count,
            )
        });
    format!(
        "Degraded. Recent Tor connections are {cause} ({} of {} recent Tor connection attempts failed{setup_duration})",
        report.recent_failed_attempt_count, report.recent_attempt_count,
    )
}

/// Compatibility constructor for non-wallet call sites. Wallet binaries should
/// use [`build_wallet_network_context`] so the default is built-in Tor.
pub fn build_http_client(proxy: Option<&Url>) -> Result<HttpContext> {
    let mode = if proxy.is_some() {
        WalletNetworkMode::Proxy
    } else {
        WalletNetworkMode::Direct
    };
    build_reqwest_context(mode, proxy.cloned(), proxy.cloned(), None, None, None, None)
}

pub fn request_tor_state_reset(data_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(data_dir)
        .wrap_err_with(|| format!("create wallet data directory {}", data_dir.display()))?;
    let marker_path = tor_state_reset_marker_path(data_dir);
    std::fs::write(
        &marker_path,
        b"Reset built-in Tor state on next wallet startup. Wallet data is not deleted.\n",
    )
    .wrap_err_with(|| format!("write Tor state reset marker {}", marker_path.display()))?;
    Ok(marker_path)
}

pub async fn build_wallet_network_context(config: WalletNetworkConfig<'_>) -> Result<HttpContext> {
    build_wallet_network_context_inner(config, None).await
}

pub async fn build_wallet_network_context_with_progress(
    config: WalletNetworkConfig<'_>,
    progress_tx: watch::Sender<WalletNetworkProgress>,
) -> Result<HttpContext> {
    build_wallet_network_context_inner(config, Some(progress_tx)).await
}

async fn build_wallet_network_context_inner(
    config: WalletNetworkConfig<'_>,
    progress_tx: Option<watch::Sender<WalletNetworkProgress>>,
) -> Result<HttpContext> {
    let mode = resolve_wallet_network_mode(config.network_mode, config.proxy)?;
    send_network_progress(
        progress_tx.as_ref(),
        WalletNetworkProgress::new(
            Some(mode),
            WalletNetworkProgressStage::ResolvingMode,
            Some(0),
            format!("Using {} network mode", mode.as_str()),
        ),
    );
    match mode {
        WalletNetworkMode::Tor => build_tor_context(config.data_dir, progress_tx.as_ref()).await,
        WalletNetworkMode::Proxy => {
            send_network_progress(
                progress_tx.as_ref(),
                WalletNetworkProgress::new(
                    Some(WalletNetworkMode::Proxy),
                    WalletNetworkProgressStage::ConfiguringNetwork,
                    Some(50),
                    "Configuring proxy-routed wallet HTTP client",
                ),
            );
            let context = build_reqwest_context(
                WalletNetworkMode::Proxy,
                config.proxy.cloned(),
                config.proxy.cloned(),
                None,
                None,
                None,
                None,
            )?;
            send_network_ready(progress_tx.as_ref(), &context);
            Ok(context)
        }
        WalletNetworkMode::Direct => {
            send_network_progress(
                progress_tx.as_ref(),
                WalletNetworkProgress::new(
                    Some(WalletNetworkMode::Direct),
                    WalletNetworkProgressStage::ConfiguringNetwork,
                    Some(50),
                    "Configuring direct wallet HTTP client",
                ),
            );
            let context = build_reqwest_context(
                WalletNetworkMode::Direct,
                None,
                None,
                None,
                None,
                None,
                None,
            )?;
            send_network_ready(progress_tx.as_ref(), &context);
            Ok(context)
        }
    }
}

pub fn resolve_wallet_network_mode(
    network_mode: Option<WalletNetworkMode>,
    proxy: Option<&Url>,
) -> Result<WalletNetworkMode> {
    match (network_mode, proxy) {
        (None, None) => Ok(WalletNetworkMode::Tor),
        (None | Some(WalletNetworkMode::Proxy), Some(_)) => Ok(WalletNetworkMode::Proxy),
        (Some(WalletNetworkMode::Proxy), None) => Err(eyre!(
            "--network-mode proxy requires --proxy <url> so proxy routing can fail closed"
        )),
        (Some(WalletNetworkMode::Tor), Some(_)) => Err(eyre!(
            "--network-mode tor cannot be combined with --proxy; omit --proxy to use built-in Tor"
        )),
        (Some(WalletNetworkMode::Direct), Some(_)) => Err(eyre!(
            "--network-mode direct cannot be combined with --proxy; remove --proxy or select --network-mode proxy"
        )),
        (Some(mode), None) => Ok(mode),
    }
}

async fn build_tor_context(
    data_dir: &Path,
    progress_tx: Option<&watch::Sender<WalletNetworkProgress>>,
) -> Result<HttpContext> {
    let arti_base = data_dir.join(ARTI_DIR);
    let state_dir = arti_base.join(ARTI_STATE_DIR);
    let cache_dir = arti_base.join(ARTI_CACHE_DIR);
    send_network_progress(
        progress_tx,
        WalletNetworkProgress::new(
            Some(WalletNetworkMode::Tor),
            WalletNetworkProgressStage::PreparingTorStorage,
            Some(5),
            format!("Preparing Arti state under {}", arti_base.display()),
        ),
    );
    consume_requested_tor_state_reset(data_dir, &arti_base)?;
    std::fs::create_dir_all(&state_dir)
        .wrap_err_with(|| format!("create Arti state directory {}", state_dir.display()))?;
    std::fs::create_dir_all(&cache_dir)
        .wrap_err_with(|| format!("create Arti cache directory {}", cache_dir.display()))?;

    tracing::info!(
        state_dir = %state_dir.display(),
        cache_dir = %cache_dir.display(),
        "bootstrapping built-in Tor network context"
    );
    let tor_config = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir)
        .build()
        .wrap_err("build Arti client config")?;
    send_network_progress(
        progress_tx,
        WalletNetworkProgress::new(
            Some(WalletNetworkMode::Tor),
            WalletNetworkProgressStage::BootstrappingTor,
            Some(10),
            "Starting Arti bootstrap",
        ),
    );
    let arti_client = TorClient::builder()
        .config(tor_config)
        .create_unbootstrapped_async()
        .await
        .wrap_err("create unbootstrapped Arti client")?;
    bootstrap_tor_client(&arti_client, progress_tx).await?;
    send_network_progress(
        progress_tx,
        WalletNetworkProgress::new(
            Some(WalletNetworkMode::Tor),
            WalletNetworkProgressStage::StartingTorBridge,
            Some(95),
            "Starting internal Arti SOCKS bridge",
        ),
    );
    let socks_bridge = Arc::new(
        ArtiSocksBridge::start(arti_client.clone(), 1)
            .await
            .wrap_err("start internal Arti SOCKS bridge")?,
    );
    let proxy_url = Url::parse(&format!("socks5h://{}", socks_bridge.local_addr()))
        .wrap_err("build internal Arti SOCKS proxy URL")?;

    tracing::info!(proxy_url = %proxy_url, "built-in Tor network context ready");
    let context = build_reqwest_context(
        WalletNetworkMode::Tor,
        Some(proxy_url),
        None,
        Some(arti_client),
        Some(state_dir),
        Some(cache_dir),
        Some(socks_bridge),
    )?;
    send_network_ready(progress_tx, &context);
    Ok(context)
}

fn tor_state_reset_marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOR_STATE_RESET_MARKER_FILE)
}

fn consume_requested_tor_state_reset(data_dir: &Path, arti_base: &Path) -> Result<()> {
    let marker_path = tor_state_reset_marker_path(data_dir);
    if !marker_path
        .try_exists()
        .wrap_err_with(|| format!("check Tor state reset marker {}", marker_path.display()))?
    {
        return Ok(());
    }

    tracing::warn!(
        marker_path = %marker_path.display(),
        arti_dir = %arti_base.display(),
        "resetting built-in Tor state before startup"
    );
    if arti_base
        .try_exists()
        .wrap_err_with(|| format!("check Arti directory {}", arti_base.display()))?
    {
        std::fs::remove_dir_all(arti_base)
            .wrap_err_with(|| format!("remove Arti directory {}", arti_base.display()))?;
    }
    std::fs::remove_file(&marker_path)
        .wrap_err_with(|| format!("remove Tor state reset marker {}", marker_path.display()))?;
    Ok(())
}

async fn bootstrap_tor_client(
    arti_client: &WalletTorClient,
    progress_tx: Option<&watch::Sender<WalletNetworkProgress>>,
) -> Result<()> {
    let mut ticker = tokio::time::interval(TOR_BOOTSTRAP_PROGRESS_INTERVAL);
    let bootstrap = arti_client.bootstrap();
    tokio::pin!(bootstrap);
    loop {
        tokio::select! {
            result = &mut bootstrap => {
                result.wrap_err("bootstrap built-in Tor")?;
                send_network_progress(
                    progress_tx,
                    WalletNetworkProgress::new(
                        Some(WalletNetworkMode::Tor),
                        WalletNetworkProgressStage::BootstrappingTor,
                        Some(90),
                        "Tor bootstrap complete",
                    ),
                );
                return Ok(());
            }
            _ = ticker.tick() => {
                let status = arti_client.bootstrap_status();
                send_network_progress(
                    progress_tx,
                    WalletNetworkProgress::new(
                        Some(WalletNetworkMode::Tor),
                        WalletNetworkProgressStage::BootstrappingTor,
                        Some(tor_bootstrap_percent(status.as_frac())),
                        status.to_string(),
                    ),
                );
            }
        }
    }
}

fn send_network_ready(
    progress_tx: Option<&watch::Sender<WalletNetworkProgress>>,
    context: &HttpContext,
) {
    send_network_progress(
        progress_tx,
        WalletNetworkProgress::new(
            Some(context.network_mode()),
            WalletNetworkProgressStage::Ready,
            Some(100),
            context.network_status_detail(),
        ),
    );
}

fn send_network_progress(
    progress_tx: Option<&watch::Sender<WalletNetworkProgress>>,
    progress: WalletNetworkProgress,
) {
    if let Some(progress_tx) = progress_tx {
        let _ = progress_tx.send(progress);
    }
}

fn tor_bootstrap_percent(frac: f32) -> u8 {
    let raw = rounded_percent(frac);
    let scaled = 10_u16 + (u16::from(raw) * 80 / 100);
    u8::try_from(scaled).unwrap_or(90)
}

fn rounded_percent(frac: f32) -> u8 {
    let rounded = (frac.clamp(0.0, 1.0) * 100.0).round();
    let mut percent = 0_u8;
    while f32::from(percent) < rounded && percent < 100 {
        percent += 1;
    }
    percent
}

fn build_reqwest_context(
    mode: WalletNetworkMode,
    proxy_url: Option<Url>,
    user_proxy_url: Option<Url>,
    arti_client: Option<WalletTorClient>,
    arti_state_dir: Option<PathBuf>,
    arti_cache_dir: Option<PathBuf>,
    socks_bridge: Option<Arc<ArtiSocksBridge>>,
) -> Result<HttpContext> {
    if let Some(proxy_url) = &proxy_url {
        let display_proxy_url = redact_url_for_display(proxy_url);
        tracing::info!(network_mode = %mode, proxy_url = %display_proxy_url, "routing wallet HTTP traffic through proxy");
    }
    if mode == WalletNetworkMode::Direct {
        tracing::warn!(
            "wallet direct network mode selected; outbound requests are not Tor-protected"
        );
    }
    let client = wallet_reqwest_client_builder(mode, proxy_url.as_ref())?
        .build()
        .wrap_err("build HTTP client")?;
    let rpc_client = wallet_reqwest_client_builder(mode, proxy_url.as_ref())?
        .timeout(rpc_request_timeout(mode))
        .build()
        .wrap_err("build RPC HTTP client")?;
    let direct_rpc_client = if mode == WalletNetworkMode::Direct {
        rpc_client.clone()
    } else {
        // `no_proxy` also keeps proxy environment variables from redirecting it.
        reqwest::Client::builder()
            .no_proxy()
            .referer(false)
            .timeout(rpc_request_timeout(WalletNetworkMode::Direct))
            .build()
            .wrap_err("build direct RPC HTTP client")?
    };
    let broker = RpcBroker::spawn(rpc_client.clone(), direct_rpc_client.clone())
        .wrap_err("start RPC broker")?;
    Ok(HttpContext {
        client,
        rpc_client,
        direct_rpc_client,
        broker,
        gateway_pool: GatewayPool::new(),
        proxy_url,
        user_proxy_url,
        mode,
        arti_client,
        arti_state_dir,
        arti_cache_dir,
        socks_bridge,
        operation_bridges: Arc::default(),
        fail_closed: mode != WalletNetworkMode::Direct,
        #[cfg(any(test, feature = "test-support"))]
        operation_clients_for_tests: None,
    })
}

fn wallet_reqwest_client_builder(
    mode: WalletNetworkMode,
    proxy_url: Option<&Url>,
) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder().referer(false);
    if let Some(proxy_url) = proxy_url {
        let display_proxy_url = redact_url_for_display(proxy_url);
        let proxy = reqwest::Proxy::all(proxy_url.as_str())
            .wrap_err_with(|| format!("invalid proxy URL {display_proxy_url}"))?;
        builder = builder.proxy(proxy);
    }
    if mode == WalletNetworkMode::Tor {
        builder = builder.pool_max_idle_per_host(0);
    }
    if mode != WalletNetworkMode::Direct {
        builder = builder
            .tls_backend_preconfigured(tls_config_without_resumption(&[b"h2", b"http/1.1"])?);
    }
    Ok(builder)
}

/// Reqwest's default rustls setup with session resumption off, so a server cannot link
/// connections that arrive over different Tor circuits or proxy exits.
pub(crate) fn tls_config_without_resumption(alpn: &[&[u8]]) -> Result<rustls::ClientConfig> {
    // Mirrors the `TlsBackend::Rustls` branch of reqwest 0.13's `ClientBuilder::build`.
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let verifier = rustls_platform_verifier::Verifier::new(Arc::clone(&provider))
        .wrap_err("load platform TLS verifier")?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::ALL_VERSIONS)
        .wrap_err("configure TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    config.resumption = rustls::client::Resumption::disabled();
    Ok(config)
}

const fn rpc_request_timeout(mode: WalletNetworkMode) -> Duration {
    match mode {
        WalletNetworkMode::Tor => TOR_RPC_REQUEST_TIMEOUT,
        WalletNetworkMode::Proxy | WalletNetworkMode::Direct => DIRECT_PROXY_RPC_REQUEST_TIMEOUT,
    }
}

pub(crate) fn redact_url_for_display(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_username("");
    let _ = redacted.set_password(None);
    if !redacted.cannot_be_a_base() {
        redacted.set_path("");
    }
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

struct ArtiSocksBridge {
    local_addr: SocketAddr,
    session: Arc<RwLock<TorBridgeSession>>,
    shutdown_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct TorBridgeSession {
    client: WalletTorClient,
    generation: u64,
    activity: Arc<TorBridgeActivity>,
}

impl TorBridgeSession {
    fn isolate(&mut self, generation: u64) {
        *self = Self {
            client: self.client.isolated_client(),
            generation,
            activity: TorBridgeActivity::new(generation),
        };
    }
}

#[derive(Clone)]
struct CapturedTorBridgeSession {
    client: WalletTorClient,
    generation: u64,
    activity: Arc<TorBridgeActivity>,
}

impl ArtiSocksBridge {
    async fn start(arti_client: WalletTorClient, generation: u64) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .wrap_err("bind internal Arti SOCKS bridge")?;
        let local_addr = listener
            .local_addr()
            .wrap_err("read internal Arti SOCKS bridge address")?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let session = Arc::new(RwLock::new(TorBridgeSession {
            client: arti_client,
            generation,
            activity: TorBridgeActivity::new(generation),
        }));
        let task = tokio::spawn(run_arti_socks_bridge(
            listener,
            Arc::clone(&session),
            shutdown_rx,
        ));
        Ok(Self {
            local_addr,
            session,
            shutdown_tx,
            task,
        })
    }

    fn capture_session(
        session: &Arc<RwLock<TorBridgeSession>>,
    ) -> Result<CapturedTorBridgeSession> {
        session
            .read()
            .map(|session| CapturedTorBridgeSession {
                client: session.client.clone(),
                generation: session.generation,
                activity: Arc::clone(&session.activity),
            })
            .map_err(|_| eyre!("active Tor session client lock is poisoned"))
    }

    /// Yields the session's current client, so a new Tor session re-isolates later dials.
    fn session_client_provider(session: &Arc<RwLock<TorBridgeSession>>) -> WalletTorClientProvider {
        let session: Weak<RwLock<TorBridgeSession>> = Arc::downgrade(session);
        Arc::new(move || {
            session.upgrade().and_then(|session| {
                Self::capture_session(&session)
                    .ok()
                    .map(|captured| captured.client)
            })
        })
    }

    const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn active_client(&self) -> Result<WalletTorClient> {
        Self::capture_session(&self.session).map(|captured| captured.client)
    }

    fn new_isolated_session(&self) -> Result<u64> {
        let mut session = self.write_session()?;
        let generation = session.generation.saturating_add(1);
        session.isolate(generation);
        Ok(generation)
    }

    /// Moves this bridge onto a fresh isolated client numbered `generation`.
    fn join_session(&self, generation: u64) -> Result<()> {
        self.write_session()?.isolate(generation);
        Ok(())
    }

    /// Moves this bridge onto a fresh isolated client in its current session.
    fn reisolate(&self) -> Result<()> {
        let mut session = self.write_session()?;
        let generation = session.generation;
        session.isolate(generation);
        Ok(())
    }

    fn write_session(&self) -> Result<std::sync::RwLockWriteGuard<'_, TorBridgeSession>> {
        self.session
            .write()
            .map_err(|_| eyre!("active Tor session client lock is poisoned"))
    }

    fn session_generation(&self) -> u64 {
        self.session.read().map_or(0, |session| session.generation)
    }

    fn activity_snapshot(&self) -> Option<TorBridgeActivitySnapshot> {
        self.session
            .read()
            .ok()
            .map(|session| session.activity.snapshot())
    }

    fn runtime_health_report(&self) -> TorRuntimeHealthReport {
        let default_report = TorRuntimeHealthReport {
            health: TorRuntimeHealth::Ready,
            recent_attempt_count: 0,
            recent_failed_attempt_count: 0,
            recent_successful_sample_count: 0,
            recent_successful_setup_p75: None,
        };
        self.session.read().map_or(default_report, |session| {
            session.activity.runtime_health_report()
        })
    }
}

impl Drop for ArtiSocksBridge {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.task.abort();
    }
}

async fn run_arti_socks_bridge(
    listener: TcpListener,
    session: Arc<RwLock<TorBridgeSession>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let captured = match ArtiSocksBridge::capture_session(&session) {
                            Ok(captured) => captured,
                            Err(error) => {
                                tracing::warn!(%error, "active Tor session client lock is poisoned");
                                continue;
                            }
                        };
                        tokio::spawn(async move {
                            if let Err(error) = handle_socks_connection(stream, captured).await {
                                tracing::debug!(?error, "internal Arti SOCKS connection failed");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "internal Arti SOCKS accept failed; retrying");
                        if !wait_after_socks_accept_error(&mut shutdown_rx).await {
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn wait_after_socks_accept_error(shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        changed = shutdown_rx.changed() => changed.is_ok() && !*shutdown_rx.borrow(),
        () = tokio::time::sleep(SOCKS_ACCEPT_ERROR_BACKOFF) => true,
    }
}

async fn handle_socks_connection(
    mut inbound: TcpStream,
    captured: CapturedTorBridgeSession,
) -> Result<()> {
    let CapturedTorBridgeSession {
        client: arti_client,
        generation,
        activity,
    } = captured;
    debug_assert_eq!(generation, activity.generation);
    negotiate_socks_no_auth(&mut inbound).await?;
    let target = read_socks_connect_target(&mut inbound).await?;
    let connecting = activity.begin_connection();
    let outbound = match arti_client
        .connect((target.host.as_str(), target.port))
        .await
    {
        Ok(outbound) => outbound,
        Err(_error) => {
            connecting.finish_failure();
            send_socks_reply(&mut inbound, SOCKS_REPLY_GENERAL_FAILURE).await?;
            return Err(eyre!("Tor connection failed"));
        }
    };
    let active_stream = connecting.finish_success();
    send_socks_reply(&mut inbound, SOCKS_REPLY_SUCCEEDED).await?;
    relay_socks_stream(inbound, outbound, active_stream).await
}

async fn relay_socks_stream<Inbound, Outbound>(
    mut inbound: Inbound,
    outbound: Outbound,
    active_stream: TorActiveStreamGuard,
) -> Result<()>
where
    Inbound: AsyncRead + AsyncWrite + Unpin,
    Outbound: AsyncRead + AsyncWrite + Unpin,
{
    let mut outbound = DownloadCounter::new(outbound, Arc::clone(&active_stream.activity));
    match tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await {
        Ok(_) => Ok(()),
        Err(error) if is_expected_socks_stream_close(&error) => {
            tracing::trace!(%error, "internal Arti SOCKS stream closed");
            Ok(())
        }
        Err(error) => Err(error).wrap_err("relay SOCKS stream through Arti"),
    }
}

struct TorConnectingGuard {
    activity: Arc<TorBridgeActivity>,
    started_at: Instant,
}

impl TorConnectingGuard {
    fn finish_success(self) -> TorActiveStreamGuard {
        let activity = Arc::clone(&self.activity);
        activity.record_connection(true, self.started_at.elapsed());
        TorActiveStreamGuard { activity }
    }

    fn finish_failure(self) {
        self.activity
            .record_connection(false, self.started_at.elapsed());
    }
}

impl Drop for TorConnectingGuard {
    fn drop(&mut self) {
        let _ = self.activity.connecting_streams.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |count| count.checked_sub(1),
        );
        self.activity.touch();
    }
}

struct TorActiveStreamGuard {
    activity: Arc<TorBridgeActivity>,
}

impl Drop for TorActiveStreamGuard {
    fn drop(&mut self) {
        self.activity.finish_active_stream();
    }
}

struct DownloadCounter<S> {
    stream: S,
    activity: Arc<TorBridgeActivity>,
}

impl<S> DownloadCounter<S> {
    const fn new(stream: S, activity: Arc<TorBridgeActivity>) -> Self {
        Self { stream, activity }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for DownloadCounter<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        if matches!(&result, Poll::Ready(Ok(()))) {
            self.activity
                .add_downloaded_bytes(buf.filled().len().saturating_sub(before));
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for DownloadCounter<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

fn is_expected_socks_stream_close(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
    )
}

async fn negotiate_socks_no_auth(inbound: &mut TcpStream) -> Result<()> {
    let mut header = [0_u8; 2];
    inbound
        .read_exact(&mut header)
        .await
        .wrap_err("read SOCKS greeting")?;
    if header[0] != SOCKS_VERSION {
        return Err(eyre!("unsupported SOCKS version {}", header[0]));
    }
    let mut methods = vec![0_u8; usize::from(header[1])];
    inbound
        .read_exact(&mut methods)
        .await
        .wrap_err("read SOCKS auth methods")?;
    if !methods.contains(&SOCKS_NO_AUTH) {
        inbound
            .write_all(&[SOCKS_VERSION, SOCKS_NO_ACCEPTABLE_METHODS])
            .await
            .wrap_err("write SOCKS auth rejection")?;
        return Err(eyre!("SOCKS client did not offer no-auth mode"));
    }
    inbound
        .write_all(&[SOCKS_VERSION, SOCKS_NO_AUTH])
        .await
        .wrap_err("write SOCKS auth selection")?;
    Ok(())
}

struct SocksTarget {
    host: String,
    port: u16,
}

async fn read_socks_connect_target(inbound: &mut TcpStream) -> Result<SocksTarget> {
    let mut header = [0_u8; 4];
    inbound
        .read_exact(&mut header)
        .await
        .wrap_err("read SOCKS connect header")?;
    if header[0] != SOCKS_VERSION {
        send_socks_reply(inbound, SOCKS_REPLY_GENERAL_FAILURE).await?;
        return Err(eyre!("unsupported SOCKS request version {}", header[0]));
    }
    if header[1] != SOCKS_CMD_CONNECT {
        send_socks_reply(inbound, SOCKS_REPLY_COMMAND_NOT_SUPPORTED).await?;
        return Err(eyre!("unsupported SOCKS command {}", header[1]));
    }
    let host = match header[3] {
        SOCKS_ADDR_IPV4 => {
            let mut addr = [0_u8; 4];
            inbound
                .read_exact(&mut addr)
                .await
                .wrap_err("read SOCKS IPv4 address")?;
            Ipv4Addr::from(addr).to_string()
        }
        SOCKS_ADDR_DOMAIN => {
            let len = inbound
                .read_u8()
                .await
                .wrap_err("read SOCKS domain length")?;
            let mut domain = vec![0_u8; usize::from(len)];
            inbound
                .read_exact(&mut domain)
                .await
                .wrap_err("read SOCKS domain")?;
            String::from_utf8(domain).wrap_err("SOCKS domain is not UTF-8")?
        }
        SOCKS_ADDR_IPV6 => {
            let mut addr = [0_u8; 16];
            inbound
                .read_exact(&mut addr)
                .await
                .wrap_err("read SOCKS IPv6 address")?;
            Ipv6Addr::from(addr).to_string()
        }
        other => {
            send_socks_reply(inbound, SOCKS_REPLY_ADDR_NOT_SUPPORTED).await?;
            return Err(eyre!("unsupported SOCKS address type {other}"));
        }
    };
    let port = inbound
        .read_u16()
        .await
        .wrap_err("read SOCKS target port")?;
    Ok(SocksTarget { host, port })
}

async fn send_socks_reply(inbound: &mut TcpStream, reply: u8) -> Result<()> {
    inbound
        .write_all(&[
            SOCKS_VERSION,
            reply,
            0x00,
            SOCKS_ADDR_IPV4,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ])
        .await
        .wrap_err("write SOCKS reply")
}

#[cfg(test)]
mod tests;
