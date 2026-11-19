//! Public-account disclosure authority. Only the transport actor supplies peer/session identity.
use super::admission::{ReadAdmission, ReadAdmissionDecision, ReadAdmissionUpdates, ReadTicket};
use super::balance_reads::{LocalBalanceAnswer, permits_balance_shortcut};
use super::errors::{LocalProviderFailure, ProviderAvailability, ProviderRpcError};
use super::{GatewayError, GatewayProviderOutcome, GatewayServerMessage, PeerId};
use crate::{
    HttpContext, OperationHttpClientSlot, RpcBrokerError, RpcChainRoute, RpcRead, WalletRpcOrigin,
};
use crate::{
    RpcOrigin,
    vault::{
        DesktopVaultStore, DesktopViewSession, GatewayPermission, PublicAccountMetadata,
        VaultError, WalletConnectSessionAccountResolution,
    },
};
use futures_util::FutureExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::{task::JoinSet, time::Instant};

#[path = "approvals.rs"]
mod approvals;
#[path = "chain_editor.rs"]
mod chain_editor;
pub use chain_editor::{
    GatewayChainEditorCommand, GatewayChainEditorOutcome, GatewayChainEditorRequest,
};
#[path = "network.rs"]
mod network;
pub use network::{
    GatewayNetworkActivity, GatewayNetworkCommand, GatewayNetworkError, GatewayNetworkOperation,
    GatewayNetworkOutcome, GatewayNetworkRequest, GatewayNetworkResult, GatewayNetworkStatus,
    GatewayNetworkView, GatewayTorRotation,
};
#[path = "public_view.rs"]
mod public_view;
use crate::dapp_request::DappRequestControl;
use approvals::{ApprovalDelivery, PendingApproval};
pub use approvals::{GatewayApprovalAccount, GatewayApprovalRequest};
#[path = "wallet_switch.rs"]
mod wallet_switch;
use tokio::sync::{oneshot, watch};
pub use wallet_switch::{GatewayWalletSwitchRequest, GatewayWalletSwitchTransition};
mod delivery;
mod read_dispatch;
mod requests;
mod responses;
mod wallet_state;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

const APPROVAL_WINDOW: Duration = Duration::from_mins(5);
const MAX_APPROVALS: usize = 16;
const MAX_DOCUMENTS: usize = 1024;
// Must stay >= the 256-port cap in extensions/railoxide/gateway-page-bridge.js.
pub(super) const MAX_SESSION_DOCUMENTS: usize = 256;
const MAX_ID_LEN: usize = 256;

/// Ephemeral desktop identity for requests admitted before a usable view is installed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GatewayUnlockState {
    pub cohort: u64,
    pub completed: bool,
}

/// View capability and immutable networking authority. Presentation defaults do not advance the epoch.
#[derive(Clone, Default)]
pub struct GatewayWalletState {
    pub native_usd_pricing: BTreeMap<String, railgun_ui::chain_editor::NativeUsdStatus>,
    pub public_view: super::GatewayPublicView,
    pub network_view: Option<GatewayNetworkView>,
    pub private_view_supported: bool,
    pub private_actions_supported: bool,
    pub private_self_broadcast_supported: bool,
    pub private_view: Option<super::GatewayPrivateView>,
    /// Fences Home selection dispatch without changing dapp grant ownership.
    pub wallet_selection_generation: u64,
    pub wallet_transition: bool,
    pub waiting_unlock: GatewayUnlockState,
    pub wallet_switch: Option<GatewayWalletSwitchTransition>,
    pub view: Option<Arc<DesktopViewSession>>,
    pub active_wallet_generation: u64,
    pub public_accounts: Vec<PublicAccountMetadata>,
    pub chain_ids: Vec<u64>,
    pub configured_chain_ids: Vec<u64>,
    pub default_chain_id: Option<u64>,
    pub http: Option<HttpContext>,
    pub routes: BTreeMap<u64, RpcChainRoute>,
    pub chain_choices: BTreeMap<u64, GatewayChainChoice>,
    pub native_currencies: BTreeMap<u64, crate::settings::NativeCurrency>,
    pub token_registry: Option<Arc<crate::settings::EffectiveTokenRegistry>>,
    pub public_balance_cache: crate::PublicBalanceCache,
    pub public_transaction_tracker: crate::PublicTransactionTracker,
}
impl GatewayWalletState {
    fn chain_choice(&self, id: u64) -> GatewayChainChoice {
        self.chain_choices
            .get(&id)
            .cloned()
            .unwrap_or_else(|| GatewayChainChoice {
                id,
                name: format!("Chain {id}"),
                railgun: false,
            })
    }
    /// Resolve routes with the same settings policy used by wallet operations.
    pub fn set_rpc_context(
        &mut self,
        http: HttpContext,
        configs: &crate::settings::EffectiveChainRegistry,
    ) {
        self.http = Some(http);
        self.configured_chain_ids = configs.keys().copied().collect();
        self.routes = configs
            .iter()
            .filter_map(|(&chain, config)| {
                crate::settings::resolve_effective_chain_rpc_route(chain, config)
                    .ok()
                    .map(|route| (chain, route))
            })
            .collect();
        self.chain_ids = self.routes.keys().copied().collect();
        self.chain_choices = configs
            .enabled_chains()
            .map(|chain| {
                (
                    chain.chain_id,
                    GatewayChainChoice {
                        id: chain.chain_id,
                        name: chain.name.clone(),
                        railgun: chain.railgun.is_some(),
                    },
                )
            })
            .collect();
        self.native_currencies = configs
            .enabled_chains()
            .map(|chain| (chain.chain_id, chain.native_currency.clone()))
            .collect();
    }
    #[must_use]
    pub fn same_state(&self, other: &Self) -> bool {
        self.same_authority(other)
            && self.chain_ids == other.chain_ids
            && self.configured_chain_ids == other.configured_chain_ids
            && self.chain_choices == other.chain_choices
            && self.routes == other.routes
            && self.network_view == other.network_view
            && self.public_view == other.public_view
            && self.native_usd_pricing == other.native_usd_pricing
            && self.private_actions_supported == other.private_actions_supported
            && self.private_self_broadcast_supported == other.private_self_broadcast_supported
            && self.private_view_supported == other.private_view_supported
            && self.private_view == other.private_view
            && self.wallet_selection_generation == other.wallet_selection_generation
            && self.wallet_transition == other.wallet_transition
            && self.wallet_switch == other.wallet_switch
            && self.public_accounts == other.public_accounts
            && self.default_chain_id == other.default_chain_id
            && self.token_registry == other.token_registry
            && self.native_currencies == other.native_currencies
    }
    fn same_accounts(&self, other: &Self) -> bool {
        self.public_accounts.len() == other.public_accounts.len()
            && self.public_accounts.iter().all(|left| {
                other.public_accounts.iter().any(|right| {
                    left.public_account_uuid == right.public_account_uuid
                        && left.address == right.address
                        && left.source == right.source
                        && left.scope == right.scope
                        && left.derivation_index == right.derivation_index
                        && left.hardware_descriptor == right.hardware_descriptor
                        && left.status == right.status
                })
            })
    }
    #[must_use]
    pub fn same_authority(&self, other: &Self) -> bool {
        self.waiting_unlock == other.waiting_unlock
            && self.active_wallet_generation == other.active_wallet_generation
            && self.same_accounts(other)
            && same_http(self.http.as_ref(), other.http.as_ref())
            && match (&self.view, &other.view) {
                (Some(left), Some(right)) => left.is_same_wallet_session(right),
                (None, None) => true,
                _ => false,
            }
    }

    #[must_use]
    pub fn same_chain_authority(&self, other: &Self, chain_id: u64) -> bool {
        self.same_authority(other)
            && self.routes.get(&chain_id) == other.routes.get(&chain_id)
            && self.native_currencies.get(&chain_id) == other.native_currencies.get(&chain_id)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GatewayAccountChoice {
    pub uuid: String,
    pub label: Option<String>,
    pub address: String,
}
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GatewayChainChoice {
    #[serde(serialize_with = "railgun_ui::chain_id::serialize")]
    pub id: u64,
    pub name: String,
    pub railgun: bool,
}
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GatewayConnectPrompt {
    pub request_id: String,
    pub url: String,
    pub paired_peer_id: String,
    pub needs_unlock: bool,
    pub wrong_wallet: bool,
    pub accounts: Vec<GatewayAccountChoice>,
    pub chains: Vec<GatewayChainChoice>,
    #[serde(serialize_with = "railgun_ui::chain_id::optional::serialize")]
    pub default_chain_id: Option<u64>,
}
/// Desktop-local list, never included in the peer's general state snapshot.
#[derive(Clone, PartialEq, Eq)]
pub struct GatewayPermissionSummary {
    pub permission_id: String,
    pub url: String,
    pub paired_peer_id: String,
    pub public_account_uuid: String,
    pub chain_id: u64,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Disclosure {
    accounts: Vec<String>,
    chain_id: Option<String>,
    permission_id: Option<String>,
}
struct Document {
    incarnation: u64,
    origin: RpcOrigin,
    generation: u64,
    disclosure: Disclosure,
}
struct PendingConnect {
    approval_id: String,
    session: u64,
    document: String,
    request_id: String,
    origin: RpcOrigin,
    deadline: Instant,
    wallet_switch: Option<Arc<GatewayWalletSwitchRequest>>,
}

#[derive(Clone)]
struct ReadOwner {
    incarnation: u64,
    session: u64,
    document: String,
    request_id: String,
    origin: RpcOrigin,
    document_generation: u64,
    generation: u64,
    wallet: GatewayWalletState,
    permission: Option<GatewayPermission>,
    local_balance: Option<LocalBalanceAnswer>,
    /// Selected-chain snapshot for an accountless `eth_chainId` query.
    unconnected_chain: Option<u64>,
}
/// One origin's own client and read route, for the context and grant it was opened for.
#[derive(Clone)]
struct OriginRoute {
    http: HttpContext,
    grant: (String, u64),
    slot: OperationHttpClientSlot,
    /// The isolated route and the wallet route it was built from. Every read of the origin
    /// reuses it, since each `isolated` call starts a new isolation id.
    route: Arc<std::sync::Mutex<Option<(RpcChainRoute, RpcChainRoute)>>>,
}
impl OriginRoute {
    fn new(http: HttpContext, permission: &GatewayPermission) -> Self {
        Self {
            slot: OperationHttpClientSlot::new(http.clone()),
            http,
            grant: (permission.public_account_uuid.clone(), permission.chain_id),
            route: Arc::default(),
        }
    }
    fn serves(&self, http: Option<&HttpContext>, permission: &GatewayPermission) -> bool {
        same_http(Some(&self.http), http)
            && self.grant.0 == permission.public_account_uuid
            && self.grant.1 == permission.chain_id
    }
    /// Opens the client on first use, and rebuilds the route from it after a chain edit.
    async fn route(&self, base: &RpcChainRoute) -> Result<RpcChainRoute, RpcBrokerError> {
        let client = self
            .slot
            .client()
            .await
            .map_err(|_| RpcBrokerError::Transport)?;
        let mut route = self
            .route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((built, isolated)) = &*route
            && built == base
        {
            return Ok(isolated.clone());
        }
        let isolated = base.isolated(client);
        *route = Some((base.clone(), isolated.clone()));
        Ok(isolated)
    }
}
enum ReadWork {
    Accounts,
    Chain,
    Remote {
        rpc: RpcRead,
        balance_shortcut: bool,
    },
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadPhase {
    Queued,
    Running,
    Delivery,
}
enum ReadResponse {
    Provider,
    Native {
        approval_id: String,
        reply: Option<oneshot::Sender<Result<Value, RpcBrokerError>>>,
        control: DappRequestControl,
        route: RpcChainRoute,
    },
}
struct PendingRead {
    response: ReadResponse,
    ticket: ReadTicket,
    owner: ReadOwner,
    work: Option<ReadWork>,
    phase: ReadPhase,
    retired: bool,
    remote: bool,
    enqueued: bool,
    delivered: bool,
}
/// Delivery authority survives broker completion and is distinct from admission accounting.
pub(super) struct Delivery {
    pub(super) message: GatewayServerMessage,
    // Set only by fixed-local error construction or explicit stale-payload replacement.
    local_error: bool,
    // Explicit session authority for a fixed registration rejection without a document.
    registration_rejection: bool,
    read: Option<(ReadTicket, ReadOwner, bool)>,
    approval: Option<ApprovalDelivery>,
    incarnation: Option<u64>,
    chain_editor: Option<GatewayChainEditorRequest>,
}
impl Delivery {
    pub(super) const fn control(message: GatewayServerMessage) -> Self {
        Self {
            message,
            local_error: false,
            registration_rejection: false,
            read: None,
            approval: None,
            incarnation: None,
            chain_editor: None,
        }
    }
    pub(super) fn ticket_id(&self) -> Option<u64> {
        self.read.as_ref().map(|(ticket, _, _)| ticket.id)
    }
    pub(super) fn deadline(&self) -> Option<Instant> {
        if self.local_error {
            None
        } else {
            self.read
                .as_ref()
                .map(|(ticket, _, _)| ticket.deadline)
                .or_else(|| self.approval.as_ref().map(|approval| approval.deadline))
        }
    }
}
#[derive(PartialEq, Eq)]
pub(super) enum DeliveryStatus {
    Current,
    Changed,
    Discard,
}

pub(super) type ReadCompletion = (u64, Result<Value, super::reads::ReadError>);

pub(super) struct DappProvider {
    store: DesktopVaultStore,
    wallet: GatewayWalletState,
    generation: u64,
    documents: HashMap<(u64, String), Document>,
    permissions: Vec<GatewayPermission>,
    pending: Vec<PendingConnect>,
    approvals: Vec<PendingApproval>,
    approval_updates: watch::Sender<Vec<Arc<GatewayApprovalRequest>>>,
    switch_updates: watch::Sender<Vec<Arc<GatewayWalletSwitchRequest>>>,
    authority: watch::Receiver<GatewayWalletState>,
    authority_fallback: Option<watch::Sender<GatewayWalletState>>,
    gateway_lifetime: Option<watch::Receiver<super::GatewaySnapshot>>,
    admission: ReadAdmission,
    reads: HashMap<u64, PendingRead>,
    origin_routes: HashMap<RpcOrigin, OriginRoute>,
    pub(super) jobs: JoinSet<ReadCompletion>,
    outbox: Vec<(u64, Delivery)>,
    next_document: u64,
    ui_snapshots: HashMap<u64, Value>,
    ui_peers: HashMap<u64, PeerId>,
    ui_errors: HashMap<u64, String>,
    network_views: network::NetworkViews,
    chain_editor_views: chain_editor::ChainEditorViews,
}

const fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_LEN
}
fn chain_hex(chain_id: u64) -> String {
    format!("{:#x}", alloy::primitives::U64::from(chain_id))
}
fn safe_error(code: i32) -> ProviderRpcError {
    ProviderRpcError::local(match code {
        4001 => LocalProviderFailure::UserRejected,
        4100 => LocalProviderFailure::Unauthorized,
        4200 => LocalProviderFailure::Unsupported,
        4901 => LocalProviderFailure::ChainUnavailable,
        -32602 => LocalProviderFailure::InvalidParams,
        -32002 => LocalProviderFailure::Unavailable,
        -32005 => LocalProviderFailure::LimitExceeded,
        _ => LocalProviderFailure::Disconnected,
    })
}

#[cfg(test)]
pub(super) mod tests;

fn same_http(left: Option<&HttpContext>, right: Option<&HttpContext>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(&left.rpc_broker(), &right.rpc_broker()),
        (None, None) => true,
        _ => false,
    }
}
fn remote_method(method: &str) -> bool {
    matches!(
        method,
        "eth_call"
            | "eth_getBalance"
            | "eth_blockNumber"
            | "eth_getCode"
            | "eth_getStorageAt"
            | "eth_getTransactionCount"
            | "eth_getBlockByHash"
            | "eth_getBlockByNumber"
            | "eth_getBlockTransactionCountByHash"
            | "eth_getBlockTransactionCountByNumber"
            | "eth_getTransactionByHash"
            | "eth_getTransactionReceipt"
            | "eth_getTransactionByBlockHashAndIndex"
            | "eth_getTransactionByBlockNumberAndIndex"
            | "eth_getBlockReceipts"
            | "eth_getLogs"
            | "eth_gasPrice"
            | "eth_maxPriorityFeePerGas"
            | "eth_feeHistory"
            | "eth_estimateGas"
    )
}
