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
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
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
    /// Async HTTP client for bounded EVM JSON-RPC requests.
    pub rpc_client: reqwest::Client,
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
    fail_closed: bool,
}

impl HttpContext {
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
            let session: Weak<RwLock<TorBridgeSession>> = Arc::downgrade(&socks_bridge.session);
            return Some(Arc::new(move || {
                session.upgrade().and_then(|session| {
                    ArtiSocksBridge::capture_session(&session)
                        .ok()
                        .map(|captured| captured.client)
                })
            }));
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
        let generation = socks_bridge.new_isolated_session()?;
        self.gateway_pool.reset();
        Ok(generation)
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
        Self {
            client: reqwest::Client::new(),
            rpc_client: reqwest::Client::new(),
            gateway_pool: GatewayPool::new(),
            proxy_url: None,
            user_proxy_url: None,
            mode: WalletNetworkMode::Direct,
            arti_client: None,
            arti_state_dir: None,
            arti_cache_dir: None,
            socks_bridge: None,
            fail_closed: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_rpc_client_for_tests(
        rpc_client: reqwest::Client,
        mode: WalletNetworkMode,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            rpc_client,
            gateway_pool: GatewayPool::new(),
            proxy_url: None,
            user_proxy_url: None,
            mode,
            arti_client: None,
            arti_state_dir: None,
            arti_cache_dir: None,
            socks_bridge: None,
            fail_closed: mode != WalletNetworkMode::Direct,
        }
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
        ArtiSocksBridge::start(arti_client.clone())
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
    Ok(HttpContext {
        client,
        rpc_client,
        gateway_pool: GatewayPool::new(),
        proxy_url,
        user_proxy_url,
        mode,
        arti_client,
        arti_state_dir,
        arti_cache_dir,
        socks_bridge,
        fail_closed: mode != WalletNetworkMode::Direct,
    })
}

fn wallet_reqwest_client_builder(
    mode: WalletNetworkMode,
    proxy_url: Option<&Url>,
) -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder();
    if let Some(proxy_url) = proxy_url {
        let display_proxy_url = redact_url_for_display(proxy_url);
        let proxy = reqwest::Proxy::all(proxy_url.as_str())
            .wrap_err_with(|| format!("invalid proxy URL {display_proxy_url}"))?;
        builder = builder.proxy(proxy);
    }
    if mode == WalletNetworkMode::Tor {
        builder = builder.pool_max_idle_per_host(0);
    }
    Ok(builder)
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

#[derive(Clone)]
struct CapturedTorBridgeSession {
    client: WalletTorClient,
    generation: u64,
    activity: Arc<TorBridgeActivity>,
}

impl ArtiSocksBridge {
    async fn start(arti_client: WalletTorClient) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .wrap_err("bind internal Arti SOCKS bridge")?;
        let local_addr = listener
            .local_addr()
            .wrap_err("read internal Arti SOCKS bridge address")?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let session = Arc::new(RwLock::new(TorBridgeSession {
            client: arti_client,
            generation: 1,
            activity: TorBridgeActivity::new(1),
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

    const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn active_client(&self) -> Result<WalletTorClient> {
        Self::capture_session(&self.session).map(|captured| captured.client)
    }

    fn new_isolated_session(&self) -> Result<u64> {
        let mut session = self
            .session
            .write()
            .map_err(|_| eyre!("active Tor session client lock is poisoned"))?;
        let generation = session.generation.saturating_add(1);
        let isolated_client = session.client.isolated_client();
        *session = TorBridgeSession {
            client: isolated_client,
            generation,
            activity: TorBridgeActivity::new(generation),
        };
        Ok(generation)
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
        let _ = self.activity.connecting_streams.fetch_update(
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
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn proxy_url() -> Url {
        Url::parse("socks5h://127.0.0.1:9050").expect("valid proxy URL")
    }

    fn sensitive_proxy_url() -> Url {
        Url::parse("socks5h://user:pass@example.com:9050/path?token=secret#fragment")
            .expect("valid sensitive proxy URL")
    }

    fn test_data_dir(name: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "wallet-ops-{name}-{}-{timestamp}",
            std::process::id()
        ))
    }

    #[test]
    fn default_wallet_network_mode_is_tor() {
        assert_eq!(
            resolve_wallet_network_mode(None, None).expect("mode"),
            WalletNetworkMode::Tor
        );
    }

    #[test]
    fn proxy_without_explicit_mode_implies_proxy_mode() {
        let proxy = proxy_url();
        assert_eq!(
            resolve_wallet_network_mode(None, Some(&proxy)).expect("mode"),
            WalletNetworkMode::Proxy
        );
    }

    #[test]
    fn explicit_proxy_requires_proxy_url() {
        assert!(
            resolve_wallet_network_mode(Some(WalletNetworkMode::Proxy), None)
                .expect_err("proxy URL required")
                .to_string()
                .contains("requires --proxy")
        );
    }

    #[test]
    fn proxy_conflicts_with_tor_and_direct_modes() {
        let proxy = proxy_url();
        assert!(resolve_wallet_network_mode(Some(WalletNetworkMode::Tor), Some(&proxy)).is_err());
        assert!(
            resolve_wallet_network_mode(Some(WalletNetworkMode::Direct), Some(&proxy)).is_err()
        );
    }

    #[test]
    fn direct_network_health_is_ready() {
        let health = HttpContext::direct_for_tests().network_health();
        assert_eq!(health.mode, WalletNetworkMode::Direct);
        assert_eq!(health.state, WalletNetworkHealthState::Ready);
        assert_eq!(health.label(), "Direct mode");
        assert_eq!(health.cause, WalletNetworkHealthCause::None);
        assert!(
            HttpContext::direct_for_tests()
                .tor_bridge_activity_snapshot()
                .is_none()
        );
    }

    fn record_observation(
        tracker: &mut TorRuntimeHealthTracker,
        generation: u64,
        succeeded: bool,
        completed_at: Instant,
        setup_duration: Duration,
    ) -> bool {
        tracker.record_at(
            TorConnectionObservation::new(generation, succeeded, completed_at, setup_duration),
            completed_at,
        )
    }

    #[test]
    fn runtime_health_rejects_wrong_generation_and_caps_recent_observations() {
        let start = Instant::now();
        let mut tracker = TorRuntimeHealthTracker::new(7);
        record_observation(&mut tracker, 8, true, start, Duration::from_millis(10));
        assert!(tracker.observations(start).is_empty());

        for index in 0..65 {
            let at = start + Duration::from_secs(index);
            record_observation(&mut tracker, 7, true, at, Duration::from_millis(10));
        }

        let observations = tracker.observations(start + Duration::from_secs(65));
        assert_eq!(observations.len(), TOR_OBSERVATION_LIMIT);
        assert_eq!(observations[0].completed_at, start + Duration::from_secs(1));
        assert!(
            observations
                .iter()
                .all(|observation| observation.generation == 7)
        );

        let at_exact_expiry = tracker.observations(start + Duration::from_secs(121));
        assert_eq!(at_exact_expiry.len(), TOR_OBSERVATION_LIMIT);
        assert_eq!(
            at_exact_expiry[0].completed_at,
            start + Duration::from_secs(1)
        );
        let after_expiry = tracker.observations(start + Duration::from_secs(122));
        assert_eq!(after_expiry.len(), TOR_OBSERVATION_LIMIT - 1);
        assert_eq!(after_expiry[0].completed_at, start + Duration::from_secs(2));
    }

    #[test]
    fn runtime_health_preserves_equal_time_admission_order_for_failure_reset() {
        let start = Instant::now();
        let mut tracker = TorRuntimeHealthTracker::new(1);
        for index in 0..8 {
            record_observation(
                &mut tracker,
                1,
                true,
                start + Duration::from_secs(index),
                if index >= 5 {
                    Duration::from_secs(8)
                } else {
                    Duration::from_millis(1)
                },
            );
        }
        let equal_time = start + Duration::from_secs(8);
        for _ in 0..3 {
            record_observation(&mut tracker, 1, true, equal_time, Duration::from_millis(1));
        }
        record_observation(&mut tracker, 1, false, equal_time, Duration::ZERO);
        for _ in 0..3 {
            record_observation(&mut tracker, 1, true, equal_time, Duration::from_millis(1));
        }
        assert_eq!(tracker.health(equal_time), TorRuntimeHealth::Slow);

        record_observation(&mut tracker, 1, true, equal_time, Duration::from_millis(1));
        assert_eq!(tracker.health(equal_time), TorRuntimeHealth::Ready);
    }

    #[test]
    fn runtime_health_requires_sufficient_evidence_and_ignores_isolated_failures() {
        let start = Instant::now();
        let mut insufficient = TorRuntimeHealthTracker::new(7);
        for index in 0..7 {
            record_observation(
                &mut insufficient,
                7,
                false,
                start + Duration::from_secs(index),
                Duration::from_millis(10),
            );
        }
        assert_eq!(
            insufficient.health(start + Duration::from_secs(7)),
            TorRuntimeHealth::Ready
        );

        let mut isolated = TorRuntimeHealthTracker::new(7);
        for index in 0..8 {
            record_observation(
                &mut isolated,
                7,
                index >= 3,
                start + Duration::from_secs(index),
                Duration::from_millis(10),
            );
        }
        assert_eq!(
            isolated.health(start + Duration::from_secs(8)),
            TorRuntimeHealth::Ready
        );

        let mut sustained = TorRuntimeHealthTracker::new(7);
        for index in 0..8 {
            record_observation(
                &mut sustained,
                7,
                false,
                start + Duration::from_secs(index),
                Duration::from_millis(10),
            );
        }
        assert_eq!(
            sustained.health(start + Duration::from_secs(8)),
            TorRuntimeHealth::Unreliable
        );
    }

    #[test]
    fn runtime_health_does_not_credit_establishing_success_and_recovers_after_four_more() {
        let start = Instant::now();
        let mut tracker = TorRuntimeHealthTracker::new(1);
        for index in 0..8 {
            record_observation(
                &mut tracker,
                1,
                index >= 4,
                start + Duration::from_secs(index),
                Duration::from_millis(10),
            );
        }
        assert_eq!(
            tracker.health(start + Duration::from_secs(8)),
            TorRuntimeHealth::Unreliable
        );

        for index in 0..3 {
            let at =
                start + Duration::from_secs(u64::try_from(8 + index).expect("test index fits"));
            record_observation(&mut tracker, 1, true, at, Duration::from_millis(10));
            assert_eq!(tracker.health(at), TorRuntimeHealth::Unreliable);
        }
        let recovered_at = start + Duration::from_secs(11);
        record_observation(
            &mut tracker,
            1,
            true,
            recovered_at,
            Duration::from_millis(10),
        );
        assert_eq!(tracker.health(recovered_at), TorRuntimeHealth::Ready);
    }

    #[test]
    fn runtime_health_detects_p75_setup_delay_and_preserves_slow_cause() {
        let start = Instant::now();
        let mut tracker = TorRuntimeHealthTracker::new(1);
        for index in 0..8 {
            record_observation(
                &mut tracker,
                1,
                true,
                start + Duration::from_secs(index),
                if index >= 5 {
                    Duration::from_secs(8)
                } else {
                    Duration::from_millis(1)
                },
            );
        }
        let failed_at = start + Duration::from_secs(8);
        record_observation(&mut tracker, 1, false, failed_at, Duration::ZERO);
        let report = tracker.health_report(failed_at);
        assert_eq!(report.health, TorRuntimeHealth::Slow);
        assert_eq!(report.health, tracker.classify(failed_at));
        assert_eq!(report.recent_successful_sample_count, 8);
        assert_eq!(
            report.recent_successful_setup_p75,
            Some(Duration::from_secs(8))
        );

        for index in 0..3 {
            let at = start + Duration::from_secs(9 + index);
            record_observation(&mut tracker, 1, true, at, Duration::from_millis(1));
            assert_eq!(tracker.health(at), TorRuntimeHealth::Slow);
        }
        let recovered_at = start + Duration::from_secs(12);
        record_observation(
            &mut tracker,
            1,
            true,
            recovered_at,
            Duration::from_millis(1),
        );
        assert_eq!(tracker.health(recovered_at), TorRuntimeHealth::Ready);
    }

    #[test]
    fn runtime_health_expires_stale_observations_and_resets_success_streak() {
        let start = Instant::now();
        let mut tracker = TorRuntimeHealthTracker::new(1);
        for index in 0..8 {
            record_observation(
                &mut tracker,
                1,
                index < 4,
                start + Duration::from_secs(index),
                Duration::from_millis(1),
            );
        }
        assert_eq!(
            tracker.health(start + Duration::from_secs(8)),
            TorRuntimeHealth::Unreliable
        );
        let stale_at = start + Duration::from_secs(200);
        assert_eq!(tracker.health(stale_at), TorRuntimeHealth::Ready);
        assert!(tracker.observations(stale_at).is_empty());

        for index in 0..3 {
            let at = stale_at + Duration::from_secs(index);
            record_observation(&mut tracker, 1, true, at, Duration::from_millis(1));
        }
        assert_eq!(
            tracker.health(stale_at + Duration::from_secs(3)),
            TorRuntimeHealth::Ready
        );
    }

    #[test]
    fn runtime_health_clamps_credited_successes_after_partial_expiry() {
        let start = Instant::now();
        let mut tracker = TorRuntimeHealthTracker::new(1);
        for index in 0..8 {
            let at = start + Duration::from_secs(100 + index);
            record_observation(&mut tracker, 1, true, at, Duration::from_secs(8));
        }
        for index in 108..=110 {
            let at = start + Duration::from_secs(index);
            record_observation(&mut tracker, 1, true, at, Duration::from_millis(1));
        }

        let after_expiry = start + Duration::from_secs(229);
        let retained = tracker.observations(after_expiry);
        assert_eq!(retained.len(), 2);
        assert_eq!(tracker.health(after_expiry), TorRuntimeHealth::Slow);
        assert!(!tracker.observations(after_expiry).is_empty());

        let first_fresh_success = after_expiry;
        record_observation(
            &mut tracker,
            1,
            true,
            first_fresh_success,
            Duration::from_millis(1),
        );
        assert_eq!(tracker.health(first_fresh_success), TorRuntimeHealth::Slow);

        let second_fresh_success = after_expiry;
        record_observation(
            &mut tracker,
            1,
            true,
            second_fresh_success,
            Duration::from_millis(1),
        );
        assert_eq!(
            tracker.health(second_fresh_success),
            TorRuntimeHealth::Ready
        );
    }

    #[test]
    fn runtime_health_updates_current_cause_through_ready_hysteresis() {
        let start = Instant::now();
        let mut tracker = TorRuntimeHealthTracker::new(1);
        for index in 0..8 {
            record_observation(
                &mut tracker,
                1,
                true,
                start + Duration::from_secs(200 + index),
                Duration::from_secs(8),
            );
        }
        let keep_alive_at = start + Duration::from_secs(210);
        record_observation(&mut tracker, 1, true, keep_alive_at, Duration::from_secs(8));
        assert_eq!(tracker.health(keep_alive_at), TorRuntimeHealth::Slow);

        let ready_at = start + Duration::from_secs(330);
        assert_eq!(tracker.health(ready_at), TorRuntimeHealth::Slow);

        for index in 0..8 {
            let at = ready_at + Duration::from_secs(index);
            record_observation(&mut tracker, 1, false, at, Duration::ZERO);
        }
        assert_eq!(
            tracker.health(ready_at + Duration::from_secs(7)),
            TorRuntimeHealth::Unreliable
        );

        for index in 0..8 {
            let at = ready_at + Duration::from_secs(8 + index);
            record_observation(&mut tracker, 1, true, at, Duration::from_secs(8));
        }
        let boundary_report = tracker.health_report(ready_at + Duration::from_secs(15));
        assert_eq!(boundary_report.health, TorRuntimeHealth::Unreliable);
        assert_eq!(boundary_report.recent_failed_attempt_count, 8);
        assert_eq!(boundary_report.recent_attempt_count, 16);

        let next_at = ready_at + Duration::from_secs(16);
        record_observation(&mut tracker, 1, true, next_at, Duration::from_secs(8));
        let report = tracker.health_report(next_at);
        assert_eq!(report.health, TorRuntimeHealth::Slow);
        assert_eq!(report.recent_successful_sample_count, 9);
    }

    #[test]
    fn proxy_network_health_is_ready() {
        let proxy = proxy_url();
        let context = build_reqwest_context(
            WalletNetworkMode::Proxy,
            Some(proxy.clone()),
            Some(proxy),
            None,
            None,
            None,
            None,
        )
        .expect("proxy context");
        let health = context.network_health();
        assert_eq!(health.mode, WalletNetworkMode::Proxy);
        assert_eq!(health.state, WalletNetworkHealthState::Ready);
        assert_eq!(health.label(), "Proxy mode");
    }

    #[test]
    fn proxy_url_display_redacts_credentials_query_and_fragment() {
        let redacted = redact_url_for_display(&sensitive_proxy_url());

        assert_eq!(redacted, "socks5h://example.com:9050");
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("pass"));
        assert!(!redacted.contains("path"));
        assert!(!redacted.contains("token"));
        assert!(!redacted.contains("fragment"));
    }

    #[test]
    fn proxy_network_health_redacts_configured_proxy_url() {
        let proxy = sensitive_proxy_url();
        let context = build_reqwest_context(
            WalletNetworkMode::Proxy,
            Some(proxy.clone()),
            Some(proxy),
            None,
            None,
            None,
            None,
        )
        .expect("proxy context");
        let detail = context.network_status_detail();

        assert!(detail.contains("socks5h://example.com:9050"));
        assert!(!detail.contains("user"));
        assert!(!detail.contains("pass"));
        assert!(!detail.contains("path"));
        assert!(!detail.contains("token"));
        assert!(!detail.contains("fragment"));
    }

    #[test]
    fn tor_network_health_without_client_is_degraded() {
        let context = build_reqwest_context(
            WalletNetworkMode::Tor,
            Some(proxy_url()),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("tor context");
        let health = context.network_health();
        assert_eq!(health.mode, WalletNetworkMode::Tor);
        assert_eq!(health.state, WalletNetworkHealthState::Degraded);
        assert_eq!(health.label(), "Tor degraded");
        assert!(health.detail.contains("unavailable"));
    }

    #[test]
    fn start_new_tor_session_requires_tor_mode() {
        let error = HttpContext::direct_for_tests()
            .start_new_tor_session()
            .expect_err("direct mode cannot start Tor sessions");
        assert!(error.to_string().contains("only available in Tor mode"));
    }

    #[test]
    fn start_new_tor_session_requires_internal_socks_bridge() {
        let context = build_reqwest_context(
            WalletNetworkMode::Tor,
            Some(proxy_url()),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("tor context");
        assert_eq!(context.tor_session_generation(), 0);
        let error = context
            .start_new_tor_session()
            .expect_err("Tor sessions require the internal SOCKS bridge");
        assert!(error.to_string().contains("internal SOCKS bridge"));
    }

    #[test]
    fn socks_accept_error_retry_continues_without_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_shutdown_tx, mut shutdown_rx) = watch::channel(false);
            let should_continue = tokio::time::timeout(
                SOCKS_ACCEPT_ERROR_BACKOFF * 2,
                wait_after_socks_accept_error(&mut shutdown_rx),
            )
            .await
            .expect("accept error backoff returns");
            assert!(should_continue);
        });
    }

    #[test]
    fn socks_accept_error_retry_stops_on_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
            shutdown_tx.send(true).expect("send shutdown");
            let should_continue = tokio::time::timeout(
                SOCKS_ACCEPT_ERROR_BACKOFF * 2,
                wait_after_socks_accept_error(&mut shutdown_rx),
            )
            .await
            .expect("shutdown returns");
            assert!(!should_continue);
        });
    }

    #[test]
    fn request_tor_state_reset_creates_marker() {
        let data_dir = test_data_dir("reset-marker");
        let marker = request_tor_state_reset(&data_dir).expect("request Tor reset");
        assert_eq!(marker, tor_state_reset_marker_path(&data_dir));
        let marker_text = std::fs::read_to_string(&marker).expect("read marker");
        assert!(marker_text.contains("Reset built-in Tor state"));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn consume_requested_tor_state_reset_removes_only_arti_and_marker() {
        let data_dir = test_data_dir("reset-consume");
        let arti_base = data_dir.join(ARTI_DIR);
        let wallet_file = data_dir.join("wallet.db");
        std::fs::create_dir_all(arti_base.join(ARTI_STATE_DIR)).expect("create Arti state");
        std::fs::write(arti_base.join(ARTI_STATE_DIR).join("state"), b"state")
            .expect("write Arti state");
        std::fs::write(&wallet_file, b"wallet").expect("write wallet file");
        let marker = request_tor_state_reset(&data_dir).expect("request Tor reset");

        consume_requested_tor_state_reset(&data_dir, &arti_base).expect("consume Tor reset");

        assert!(!marker.exists());
        assert!(!arti_base.exists());
        assert_eq!(
            std::fs::read(&wallet_file).expect("read wallet file"),
            b"wallet"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn relay_socks_stream_counts_only_downloads_and_releases_active_stream() {
        let activity = TorBridgeActivity::new(1);
        let active_stream = activity.begin_connection().finish_success();

        let (mut client, inbound) = tokio::io::duplex(64);
        let (outbound, mut tor) = tokio::io::duplex(64);
        let relay = tokio::spawn(relay_socks_stream(inbound, outbound, active_stream));

        let upload = b"client-to-tor";
        client
            .write_all(upload)
            .await
            .expect("write client payload");
        let mut received_upload = vec![0_u8; upload.len()];
        tor.read_exact(&mut received_upload)
            .await
            .expect("read client payload on Tor side");
        assert_eq!(received_upload, upload);

        let after_upload = activity.snapshot();
        assert_eq!(after_upload.downloaded_bytes, 0);
        assert_eq!(after_upload.active_streams, 1);

        let download = b"tor-to-client";
        tor.write_all(download).await.expect("write Tor payload");
        let mut received_download = vec![0_u8; download.len()];
        client
            .read_exact(&mut received_download)
            .await
            .expect("read Tor payload on client side");
        assert_eq!(received_download, download);
        assert!(!relay.is_finished());

        let while_open = activity.snapshot();
        assert_eq!(while_open.downloaded_bytes, download.len() as u64);
        assert_eq!(while_open.active_streams, 1);

        client.shutdown().await.expect("shutdown client side");
        tor.shutdown().await.expect("shutdown Tor side");
        relay
            .await
            .expect("relay task panicked")
            .expect("relay failed");

        let after_relay = activity.snapshot();
        assert_eq!(after_relay.downloaded_bytes, download.len() as u64);
        assert_eq!(after_relay.active_streams, 0);
    }

    #[test]
    fn activity_snapshot_reports_recent_samples_and_preserves_cumulative_counts() {
        let activity = TorBridgeActivity::new(7);
        let origin = activity.activity_origin;
        activity.downloaded_bytes.store(23, Ordering::Relaxed);
        activity.successful_connections.store(2, Ordering::Relaxed);
        activity.failed_connections.store(1, Ordering::Relaxed);
        activity.last_activity_tick.store(
            u64::try_from(Duration::from_secs(15).as_nanos()).expect("activity tick fits"),
            Ordering::Relaxed,
        );

        {
            let mut tracker = activity.tracker.lock().expect("lock activity tracker");
            record_observation(
                &mut tracker,
                7,
                true,
                origin + Duration::from_secs(10),
                Duration::from_secs(2),
            );
            record_observation(
                &mut tracker,
                7,
                false,
                origin + Duration::from_secs(12),
                Duration::ZERO,
            );
            record_observation(
                &mut tracker,
                7,
                true,
                origin + Duration::from_secs(14),
                Duration::from_secs(4),
            );
        }

        let snapshot = activity.snapshot_at(origin + Duration::from_secs(20));
        assert_eq!(snapshot.generation, 7);
        assert_eq!(snapshot.session_duration, Duration::from_secs(20));
        assert_eq!(snapshot.downloaded_bytes, 23);
        assert_eq!(snapshot.successful_connections, 2);
        assert_eq!(snapshot.failed_connections, 1);
        assert_eq!(snapshot.recent_connection_sample_count, 3);
        assert_eq!(snapshot.recent_successful_sample_count, 2);
        assert_eq!(snapshot.median_setup_duration, Some(Duration::from_secs(3)));
        assert_eq!(snapshot.last_activity_age, Some(Duration::from_secs(5)));

        let stale_snapshot = activity.snapshot_at(origin + Duration::from_secs(135));
        assert_eq!(stale_snapshot.session_duration, Duration::from_secs(135));
        assert_eq!(stale_snapshot.recent_connection_sample_count, 0);
        assert_eq!(stale_snapshot.recent_successful_sample_count, 0);
        assert_eq!(stale_snapshot.median_setup_duration, None);
        assert_eq!(stale_snapshot.successful_connections, 2);
        assert_eq!(stale_snapshot.failed_connections, 1);
    }

    #[test]
    fn activity_guards_clean_up_connecting_and_active_streams() {
        let activity = TorBridgeActivity::new(1);

        {
            let connecting = activity.begin_connection();
            assert_eq!(activity.snapshot().connecting_streams, 1);
            drop(connecting);
        }
        assert_eq!(activity.snapshot().connecting_streams, 0);
        assert_eq!(activity.snapshot().successful_connections, 0);
        assert_eq!(activity.snapshot().failed_connections, 0);

        let active = activity.begin_connection().finish_success();
        let snapshot = activity.snapshot();
        assert_eq!(snapshot.connecting_streams, 0);
        assert_eq!(snapshot.active_streams, 1);
        assert_eq!(snapshot.successful_connections, 1);
        drop(active);
        assert_eq!(activity.snapshot().active_streams, 0);

        activity.begin_connection().finish_failure();
        let snapshot = activity.snapshot();
        assert_eq!(snapshot.connecting_streams, 0);
        assert_eq!(snapshot.active_streams, 0);
        assert_eq!(snapshot.failed_connections, 1);
    }

    #[test]
    fn isolated_session_replaces_real_bridge_activity_without_cross_contamination() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let data_dir = test_data_dir("bridge-session");
        let arti_dir = data_dir.join(ARTI_DIR);
        let state_dir = arti_dir.join(ARTI_STATE_DIR);
        let cache_dir = arti_dir.join(ARTI_CACHE_DIR);
        std::fs::create_dir_all(&state_dir).expect("create Arti state directory");
        std::fs::create_dir_all(&cache_dir).expect("create Arti cache directory");

        let result: Result<()> = runtime.block_on(async {
            let tor_config = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir)
                .build()
                .expect("build Arti client config");
            let arti_client = TorClient::builder()
                .config(tor_config)
                .create_unbootstrapped_async()
                .await
                .expect("create unbootstrapped Arti client");
            let bridge = ArtiSocksBridge::start(arti_client)
                .await
                .expect("start loopback Arti SOCKS bridge");
            assert!(bridge.local_addr().ip().is_loopback());

            let old_capture = ArtiSocksBridge::capture_session(&bridge.session)
                .expect("capture initial bridge session");
            assert_eq!(old_capture.generation, 1);
            assert_eq!(old_capture.generation, old_capture.activity.generation);
            let old_connection = old_capture.activity.begin_connection();

            let new_generation = bridge
                .new_isolated_session()
                .expect("replace bridge session with isolated client");
            assert_eq!(new_generation, old_capture.generation + 1);
            let new_capture = ArtiSocksBridge::capture_session(&bridge.session)
                .expect("capture replacement bridge session");
            assert_eq!(new_capture.generation, new_generation);
            assert_eq!(new_capture.generation, new_capture.activity.generation);
            assert!(!Arc::ptr_eq(&old_capture.activity, &new_capture.activity));
            let fresh_snapshot = bridge
                .activity_snapshot()
                .expect("snapshot replacement activity");
            assert_eq!(fresh_snapshot.generation, new_generation);
            assert_eq!(fresh_snapshot.downloaded_bytes, 0);
            assert_eq!(fresh_snapshot.connecting_streams, 0);
            assert_eq!(fresh_snapshot.active_streams, 0);
            assert_eq!(fresh_snapshot.successful_connections, 0);
            assert_eq!(fresh_snapshot.failed_connections, 0);
            assert_eq!(new_capture.activity.snapshot().generation, new_generation);

            let old_active = old_connection.finish_success();
            old_capture.activity.add_downloaded_bytes(8);
            drop(old_active);
            let old_snapshot = old_capture.activity.snapshot();
            assert_eq!(old_snapshot.downloaded_bytes, 8);
            assert_eq!(old_snapshot.connecting_streams, 0);
            assert_eq!(old_snapshot.active_streams, 0);
            assert_eq!(old_snapshot.successful_connections, 1);
            assert_eq!(new_capture.activity.snapshot().downloaded_bytes, 0);
            assert_eq!(new_capture.activity.snapshot().connecting_streams, 0);
            assert_eq!(new_capture.activity.snapshot().active_streams, 0);
            assert_eq!(new_capture.activity.snapshot().successful_connections, 0);

            let current_snapshot = bridge
                .activity_snapshot()
                .expect("snapshot current activity after old completion");
            assert_eq!(current_snapshot.generation, new_generation);
            assert_eq!(current_snapshot.downloaded_bytes, 0);
            assert_eq!(current_snapshot.connecting_streams, 0);
            assert_eq!(current_snapshot.active_streams, 0);
            assert_eq!(current_snapshot.successful_connections, 0);
            assert_eq!(current_snapshot.failed_connections, 0);
            assert_eq!(current_snapshot.recent_connection_sample_count, 0);
            assert_eq!(current_snapshot.recent_successful_sample_count, 0);
            assert_eq!(current_snapshot.median_setup_duration, None);
            assert_eq!(current_snapshot.last_activity_age, None);
            Ok(())
        });
        result.expect("bridge session replacement test");
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
