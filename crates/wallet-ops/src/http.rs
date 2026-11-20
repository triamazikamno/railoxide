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

mod context;
mod network_build;
mod operation_clients;
mod socks_bridge;
mod tor_health;

pub use network_build::*;
#[cfg(test)]
use socks_bridge::{relay_socks_stream, wait_after_socks_accept_error};
use tor_health::format_tor_runtime_health_detail;

/// Keeps the tracing target of the moved items at this module's path.
const LOG_TARGET: &str = module_path!();

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

/// One operation's HTTP client, opened on first use and shared by every clone.
#[derive(Clone)]
pub struct OperationHttpClientSlot {
    http: HttpContext,
    client: Arc<tokio::sync::OnceCell<OperationHttpClient>>,
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

struct TorConnectingGuard {
    activity: Arc<TorBridgeActivity>,
    started_at: Instant,
}

struct TorActiveStreamGuard {
    activity: Arc<TorBridgeActivity>,
}

struct DownloadCounter<S> {
    stream: S,
    activity: Arc<TorBridgeActivity>,
}

struct SocksTarget {
    host: String,
    port: u16,
}

#[cfg(test)]
mod tests;
