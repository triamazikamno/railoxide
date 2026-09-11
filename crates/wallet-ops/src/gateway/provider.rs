//! Public-account disclosure authority. Only the transport actor supplies peer/session identity.
use super::admission::{ReadAdmission, ReadAdmissionDecision, ReadAdmissionUpdates, ReadTicket};
use super::balance_reads::{LocalBalanceAnswer, permits_balance_shortcut};
use super::errors::{LocalProviderFailure, ProviderAvailability, ProviderRpcError};
use super::{GatewayError, GatewayProviderOutcome, GatewayServerMessage, PeerId};
use crate::{HttpContext, RpcBrokerError, RpcChainRoute, RpcRead, WalletRpcOrigin};
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
#[path = "public_view.rs"]
mod public_view;
use crate::dapp_request::DappRequestControl;
pub use approvals::GatewayApprovalRequest;
use approvals::{ApprovalDelivery, PendingApproval};
#[path = "wallet_switch.rs"]
mod wallet_switch;
use tokio::sync::{oneshot, watch};
pub use wallet_switch::{GatewayWalletSwitchRequest, GatewayWalletSwitchTransition};

const APPROVAL_WINDOW: Duration = Duration::from_mins(5);
const MAX_APPROVALS: usize = 16;
const MAX_DOCUMENTS: usize = 1024;
const MAX_SESSION_DOCUMENTS: usize = 128;
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
    pub public_view: super::GatewayPublicView,
    pub waiting_unlock: GatewayUnlockState,
    pub wallet_switch: Option<GatewayWalletSwitchTransition>,
    pub view: Option<Arc<DesktopViewSession>>,
    pub active_wallet_generation: u64,
    pub public_accounts: Vec<PublicAccountMetadata>,
    pub chain_ids: Vec<u64>,
    pub default_chain_id: Option<u64>,
    pub http: Option<HttpContext>,
    pub routes: BTreeMap<u64, RpcChainRoute>,
    pub token_registry: Option<Arc<crate::settings::EffectiveTokenRegistry>>,
    pub public_balance_cache: crate::PublicBalanceCache,
    pub public_transaction_tracker: crate::PublicTransactionTracker,
}
impl GatewayWalletState {
    /// Resolve routes with the same settings policy used by wallet operations.
    pub fn set_rpc_context(
        &mut self,
        http: HttpContext,
        configs: &BTreeMap<u64, crate::settings::EffectiveChainConfig>,
    ) {
        self.http = Some(http);
        self.routes = configs
            .iter()
            .filter_map(|(&chain, config)| {
                crate::settings::resolve_effective_chain_rpc_route(chain, Some(config))
                    .ok()
                    .map(|route| (chain, route))
            })
            .collect();
        self.chain_ids = self.routes.keys().copied().collect();
    }
    #[must_use]
    pub fn same_state(&self, other: &Self) -> bool {
        self.same_authority(other)
            && self.public_view == other.public_view
            && self.wallet_switch == other.wallet_switch
            && self.public_accounts == other.public_accounts
            && self.default_chain_id == other.default_chain_id
            && self.token_registry == other.token_registry
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
            && self.chain_ids == other.chain_ids
            && self.routes == other.routes
            && same_http(self.http.as_ref(), other.http.as_ref())
            && match (&self.view, &other.view) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
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
    pub id: u64,
    pub name: String,
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
    pub(super) jobs: JoinSet<ReadCompletion>,
    outbox: Vec<(u64, Delivery)>,
    next_document: u64,
    ui_snapshots: HashMap<u64, Value>,
    ui_peers: HashMap<u64, PeerId>,
    ui_errors: HashMap<u64, String>,
}
impl DappProvider {
    pub(super) fn new(store: DesktopVaultStore, generation: u64) -> Self {
        let (authority_fallback, authority) = watch::channel(GatewayWalletState::default());
        Self {
            store,
            wallet: GatewayWalletState::default(),
            generation,
            documents: HashMap::new(),
            permissions: Vec::new(),
            pending: Vec::new(),
            approvals: Vec::new(),
            approval_updates: watch::channel(Vec::new()).0,
            switch_updates: watch::channel(Vec::new()).0,
            authority,
            authority_fallback: Some(authority_fallback),
            gateway_lifetime: None,
            admission: ReadAdmission::new(),
            reads: HashMap::new(),
            jobs: JoinSet::new(),
            outbox: Vec::new(),
            next_document: 0,
            ui_snapshots: HashMap::new(),
            ui_peers: HashMap::new(),
            ui_errors: HashMap::new(),
        }
    }
    pub(super) fn retire_sessions(&mut self, live: impl Fn(u64) -> bool) {
        self.ui_snapshots.retain(|session, _| live(*session));
        self.ui_peers.retain(|session, _| live(*session));
        self.ui_errors.retain(|session, _| live(*session));
        self.documents.retain(|(session, _), _| live(*session));
        self.pending.retain(|pending| live(pending.session));
        self.outbox.retain(|(session, _)| live(*session));
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        // Origin rate charges survive reconnect and document replacement.
    }
    pub(super) fn drain(&mut self) -> Vec<(u64, Delivery)> {
        let mut outbox = std::mem::take(&mut self.outbox);
        for (session, delivery) in &mut outbox {
            if let GatewayServerMessage::ProviderResponse {
                document,
                request_id,
                ..
            } = &delivery.message
                && let Some(read) = self.reads.values_mut().find(|read| {
                    matches!(read.response, ReadResponse::Provider)
                        && read.owner.session == *session
                        && read.owner.document == *document
                        && read.owner.request_id == *request_id
                })
            {
                delivery.read = Some((read.ticket.clone(), read.owner.clone(), read.remote));
                read.enqueued = true;
            }
        }
        outbox
    }
    pub(super) fn ui_event(
        &self,
        generation: u64,
        kind: super::GatewayUiEventKind,
        live: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<super::GatewayUiEvent> {
        if generation != self.generation || !self.authority.borrow().same_authority(&self.wallet) {
            return None;
        }
        let live = Arc::downgrade(live);
        let authority = self.authority.clone();
        let wallet = self.wallet.clone();
        let captured = wallet.clone();
        Some(super::GatewayUiEvent {
            generation,
            kind,
            wallet,
            control: DappRequestControl::new(Instant::now() + Duration::from_secs(2), move || {
                if !live
                    .upgrade()
                    .is_some_and(|live| live.load(std::sync::atomic::Ordering::Acquire))
                    || authority.has_changed().is_err()
                {
                    return Err(RpcBrokerError::Shutdown);
                }
                if !authority.borrow().same_authority(&captured) {
                    return Err(RpcBrokerError::OriginRejected);
                }
                Ok(())
            }),
        })
    }
    pub(super) fn same_authority(&self, wallet: &GatewayWalletState) -> bool {
        self.wallet.same_authority(wallet)
    }
    pub(super) fn permissions(&self) -> Vec<GatewayPermissionSummary> {
        if self.wallet.view.is_none() {
            return Vec::new();
        }
        self.permissions
            .iter()
            .filter_map(|permission| {
                Some(GatewayPermissionSummary {
                    permission_id: permission.permission_id.clone(),
                    url: permission.origin.web_origin()?.as_str().to_owned(),
                    paired_peer_id: permission.origin.paired_peer_id()?.to_owned(),
                    public_account_uuid: permission.public_account_uuid.clone(),
                    chain_id: permission.chain_id,
                })
            })
            .collect()
    }
    pub(super) fn update_wallet(&mut self, mut wallet: GatewayWalletState, generation: u64) {
        if wallet.view.is_none() {
            wallet.public_view = super::GatewayPublicView::default();
            self.ui_errors.clear();
            wallet.public_accounts.clear();
            wallet.token_registry = None;
            wallet.chain_ids.clear();
            wallet.default_chain_id = None;
            wallet.http = None;
            wallet.routes.clear();
        }
        if let Some(authority) = &self.authority_fallback {
            authority.send_replace(wallet.clone());
        }
        self.wallet = wallet;
        self.generation = generation;
        self.reload_permissions();
        self.refresh_wallet_switches(Instant::now());
        self.refresh_documents(true);
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        self.push_pending_ui();
    }
    fn reload_permissions(&mut self) {
        self.permissions = self
            .wallet
            .view
            .as_ref()
            .and_then(|view| self.store.list_gateway_permissions(view).ok())
            .unwrap_or_default();
    }
    fn resolve(
        &self,
        origin: &RpcOrigin,
    ) -> Result<(GatewayPermission, PublicAccountMetadata), i32> {
        let view = self.wallet.view.as_ref().ok_or(4100)?;
        let permission = self
            .permissions
            .iter()
            .find(|permission| &permission.origin == origin)
            .ok_or(4100)?;
        match self.store.resolve_dapp_session_account(
            view,
            &permission.public_account_uuid,
            &permission.public_account_scope,
            permission.owning_private_wallet_uuid.as_deref(),
        ) {
            Ok(WalletConnectSessionAccountResolution::Usable(account)) => {
                Ok((permission.clone(), account))
            }
            _ => Err(4100),
        }
    }
    fn disclosure(&self, origin: &RpcOrigin) -> Disclosure {
        let Ok((permission, account)) = self.resolve(origin) else {
            return Disclosure::default();
        };
        Disclosure {
            accounts: vec![account.address.to_string()],
            chain_id: self
                .wallet
                .chain_ids
                .contains(&permission.chain_id)
                .then(|| chain_hex(permission.chain_id)),
            permission_id: Some(permission.permission_id),
        }
    }
    fn push_state(&mut self, key: &(u64, String), invalidation_code: Option<i32>) {
        if let Some(document) = self.documents.get(key) {
            self.outbox.push((
                key.0,
                Delivery {
                    incarnation: Some(document.incarnation),
                    local_error: false,
                    registration_rejection: false,
                    read: None,
                    approval: None,
                    message: GatewayServerMessage::ProviderState {
                        version: 1,
                        document: key.1.clone(),
                        generation: self.generation,
                        document_generation: document.generation,
                        accounts: document.disclosure.accounts.clone(),
                        chain_id: document.disclosure.chain_id.clone(),
                        invalidation_code,
                    },
                },
            ));
        }
    }
    fn refresh_documents(&mut self, force: bool) {
        let keys: Vec<_> = self.documents.keys().cloned().collect();
        for key in keys {
            let disclosure = self.disclosure(&self.documents[&key].origin);
            let document = self.documents.get_mut(&key).expect("known document");
            let changed = document.disclosure != disclosure;
            let invalidation_code = changed.then(|| {
                if disclosure.accounts.is_empty()
                    || disclosure.accounts != document.disclosure.accounts
                {
                    4100
                } else if disclosure.chain_id.is_none() {
                    4901
                } else {
                    -32002
                }
            });
            if changed {
                document.generation += 1;
                document.disclosure = disclosure;
            }
            if changed || force {
                self.push_state(&key, invalidation_code);
            }
        }
    }
    pub(super) fn register(
        &mut self,
        session: u64,
        peer: PeerId,
        document: String,
        url: &str,
    ) -> Result<(), GatewayError> {
        if !valid_id(&document) {
            return Err(GatewayError::Unavailable);
        }
        let Ok(origin) = RpcOrigin::dapp_web_origin(alloy::hex::encode(peer.to_bytes()), url)
        else {
            self.reject_registration(session, &document, -32602);
            return Ok(());
        };
        let key = (session, document);
        self.ui_peers.insert(session, peer);
        if let Some(existing) = self.documents.get(&key) {
            if existing.origin != origin {
                return Err(GatewayError::Unavailable);
            }
            self.push_state(&key, None);
            return Ok(());
        }
        if self.documents.len() >= MAX_DOCUMENTS
            || self
                .documents
                .keys()
                .filter(|(owner, _)| *owner == session)
                .count()
                >= MAX_SESSION_DOCUMENTS
        {
            self.reject_registration(session, &key.1, -32005);
            return Ok(());
        }
        let disclosure = self.disclosure(&origin);
        self.next_document += 1;
        self.documents.insert(
            key.clone(),
            Document {
                incarnation: self.next_document,
                origin,
                generation: 1,
                disclosure,
            },
        );
        self.push_state(&key, None);
        Ok(())
    }
    pub(super) fn unregister(&mut self, session: u64, document: &str) {
        self.documents.remove(&(session, document.to_owned()));
        self.pending
            .retain(|pending| pending.session != session || pending.document != document);
        self.outbox.retain(|(owner, message)| *owner != session || !matches!(&message.message, GatewayServerMessage::ProviderResponse { document: target, .. } | GatewayServerMessage::ProviderState { document: target, .. } if target == document));
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        self.push_ui(session);
    }
    pub(super) fn request(
        &mut self,
        session: u64,
        document: String,
        request_id: String,
        method: &str,
        params: Value,
        now: Instant,
    ) -> Result<(), GatewayError> {
        self.tick(now);
        if !valid_id(&document) || !valid_id(&request_id) {
            return Err(GatewayError::Unavailable);
        }
        let Some(owner) = self.documents.get(&(session, document.clone())) else {
            self.respond(session, &document, &request_id, Err(4100));
            return Ok(());
        };
        let origin = owner.origin.clone();
        if self.pending.iter().any(|pending| pending.session == session && pending.document == document && pending.request_id == request_id)
            || self.approvals.iter().any(|pending| pending.session == session && pending.document == document && pending.request_id == request_id)
            || self.reads.values().any(|read| matches!(read.response, ReadResponse::Provider) && read.owner.session == session && read.owner.document == document && read.owner.request_id == request_id)
            || self.outbox.iter().any(|(owner, message)| *owner == session && matches!(&message.message, GatewayServerMessage::ProviderResponse { document: target, request_id: id, .. } if target == &document && id == &request_id))
        { return Err(GatewayError::Unavailable); }
        let local = matches!(
            method,
            "eth_accounts" | "eth_chainId" | "eth_requestAccounts"
        );
        if matches!(
            method,
            "personal_sign"
                | "eth_signTypedData"
                | "eth_signTypedData_v4"
                | "eth_sendTransaction"
                | "wallet_switchEthereumChain"
                | "wallet_addEthereumChain"
                | "wallet_watchAsset"
        ) {
            return self.request_approval(session, document, request_id, method, params, now);
        }
        if !local && !remote_method(method) {
            self.respond(session, &document, &request_id, Err(4200));
            return Ok(());
        }
        if local && !params.is_null() && !params.as_array().is_some_and(Vec::is_empty) {
            self.respond(session, &document, &request_id, Err(-32602));
            return Ok(());
        }
        if method == "eth_requestAccounts" {
            self.reload_permissions();
        }
        if method == "eth_requestAccounts" && self.resolve(&origin).is_err() {
            if self
                .pending
                .iter()
                .filter(|pending| pending.origin == origin)
                .count()
                + self
                    .approvals
                    .iter()
                    .filter(|pending| pending.origin == origin)
                    .count()
                >= MAX_APPROVALS
            {
                self.respond(session, &document, &request_id, Err(-32005));
                return Ok(());
            }
            let mut id = [0; 16];
            getrandom::fill(&mut id).map_err(|_| GatewayError::Unavailable)?;
            self.pending.push(PendingConnect {
                approval_id: alloy::hex::encode(id),
                session,
                document,
                request_id,
                origin,
                deadline: now + APPROVAL_WINDOW,
                wallet_switch: None,
            });
            self.push_ui(session);
            return Ok(());
        }
        self.admit_read(session, document, request_id, method, params, now);
        Ok(())
    }
    fn admit_read(
        &mut self,
        session: u64,
        document: String,
        request_id: String,
        method: &str,
        params: Value,
        now: Instant,
    ) {
        self.reload_permissions();
        self.refresh_documents(false);
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        let Some(doc) = self.documents.get(&(session, document.clone())) else {
            return;
        };
        let origin = doc.origin.clone();
        let permission = match self.resolve(&origin) {
            Ok((permission, _)) => Some(permission),
            Err(_) if method == "eth_accounts" => None,
            Err(code) => {
                self.respond(session, &document, &request_id, Err(code));
                return;
            }
        };
        if permission
            .as_ref()
            .is_some_and(|grant| !self.wallet.chain_ids.contains(&grant.chain_id))
        {
            self.respond(session, &document, &request_id, Err(4901));
            return;
        }
        let work = match method {
            "eth_accounts" | "eth_requestAccounts" => ReadWork::Accounts,
            "eth_chainId" => ReadWork::Chain,
            _ => {
                let chain = permission
                    .as_ref()
                    .expect("authorized remote read")
                    .chain_id;
                if !self.wallet.routes.contains_key(&chain) {
                    self.respond(session, &document, &request_id, Err(4901));
                    return;
                }
                if self.wallet.http.is_none() {
                    self.respond(session, &document, &request_id, Err(4900));
                    return;
                }
                let balance_shortcut = permits_balance_shortcut(method, &params);
                match super::reads::parse_read(method, params, chain) {
                    Ok(rpc) => ReadWork::Remote {
                        rpc,
                        balance_shortcut,
                    },
                    Err(error) => {
                        self.respond_error(session, &document, &request_id, error);
                        return;
                    }
                }
            }
        };
        let owner = ReadOwner {
            incarnation: doc.incarnation,
            session,
            document,
            request_id,
            origin: origin.clone(),
            document_generation: doc.generation,
            generation: self.generation,
            wallet: self.wallet.clone(),
            permission,
            local_balance: None,
        };
        let (ticket, phase) = match self.admission.admit(origin, now) {
            Ok(ReadAdmissionDecision::Ready(ticket)) => (ticket, ReadPhase::Delivery),
            Ok(ReadAdmissionDecision::Queued(ticket)) => (ticket, ReadPhase::Queued),
            Err(_) => {
                self.respond(
                    owner.session,
                    &owner.document,
                    &owner.request_id,
                    Err(-32005),
                );
                return;
            }
        };
        let id = ticket.id;
        let remote = matches!(work, ReadWork::Remote { .. });
        self.reads.insert(
            id,
            PendingRead {
                response: ReadResponse::Provider,
                ticket,
                owner,
                work: Some(work),
                phase,
                retired: false,
                remote,
                enqueued: false,
                delivered: false,
            },
        );
        if phase != ReadPhase::Queued {
            self.dispatch_read(id);
        }
    }
    fn validate_read(&self, read: &PendingRead, now: Instant) -> Result<(), i32> {
        self.validate_owner(&read.owner, read.remote, read.ticket.deadline, now)?;
        if let ReadResponse::Native {
            approval_id,
            reply,
            control,
            ..
        } = &read.response
        {
            if reply.as_ref().is_some_and(oneshot::Sender::is_closed) {
                return Err(-32002);
            }
            if !self
                .approvals
                .iter()
                .any(|pending| &pending.id == approval_id)
            {
                return Err(4100);
            }
            control
                .ensure_current()
                .map_err(|error| approvals::broker_code(&error))?;
        }
        Ok(())
    }
    fn validate_owner(
        &self,
        owner: &ReadOwner,
        remote: bool,
        deadline: Instant,
        now: Instant,
    ) -> Result<(), i32> {
        let doc = self
            .documents
            .get(&(owner.session, owner.document.clone()))
            .ok_or(4900)?;
        if doc.origin != owner.origin || doc.incarnation != owner.incarnation {
            return Err(4900);
        }
        let authority = self.authority.borrow();
        if !authority.same_authority(&owner.wallet) {
            return Err(if authority.view.is_none() && owner.wallet.view.is_some() {
                4100
            } else {
                -32002
            });
        }
        drop(authority);
        let resolved = self.resolve(&owner.origin).ok();
        if resolved.as_ref().map(|(permission, _)| permission) != owner.permission.as_ref() {
            return Err(4100);
        }
        if let Some(permission) = &owner.permission {
            if !self.wallet.chain_ids.contains(&permission.chain_id) {
                return Err(4901);
            }
            if remote {
                if self.wallet.http.is_none() {
                    return Err(4900);
                }
                if !self.wallet.routes.contains_key(&permission.chain_id) {
                    return Err(4901);
                }
            }
        }
        if (self.generation, doc.generation) != (owner.generation, owner.document_generation)
            || !self.wallet.same_authority(&owner.wallet)
            || now >= deadline
        {
            return Err(
                if self.wallet.view.is_none() && owner.wallet.view.is_some() {
                    4100
                } else {
                    -32002
                },
            );
        }
        if let Some(answer) = &owner.local_balance {
            let account = &resolved.as_ref().ok_or(4100)?.1;
            if !answer.is_current(&self.wallet, account, now) {
                return Err(-32002);
            }
        }
        Ok(())
    }
    fn dispatch_read(&mut self, id: u64) {
        let Some(mut read) = self.reads.remove(&id) else {
            return;
        };
        if let Err(code) = self.validate_read(&read, Instant::now()) {
            if let ReadResponse::Native { reply, .. } = &mut read.response {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(approvals::code_broker(code, &read.owner)));
                }
                self.finish_ticket(id);
                return;
            }
            read.phase = ReadPhase::Delivery;
            read.retired = true;
            self.respond(
                read.owner.session,
                &read.owner.document,
                &read.owner.request_id,
                Err(code),
            );
            self.reads.insert(id, read);
            return;
        }
        match read.work.take().expect("undispatched read") {
            ReadWork::Remote {
                rpc,
                balance_shortcut,
            } => {
                let answer = balance_shortcut
                    .then(|| {
                        let candidate = rpc.balance_candidate()?;
                        let (permission, account) = self.resolve(&read.owner.origin).ok()?;
                        LocalBalanceAnswer::prepare(
                            &self.wallet,
                            permission.chain_id,
                            account,
                            candidate,
                            Instant::now(),
                        )
                    })
                    .flatten();
                if let Some(answer) = answer {
                    let value = answer.value();
                    read.owner.local_balance = Some(answer);
                    read.phase = ReadPhase::Delivery;
                    self.respond(
                        read.owner.session,
                        &read.owner.document,
                        &read.owner.request_id,
                        Ok(value),
                    );
                    self.reads.insert(id, read);
                    return;
                }
                let permission = read
                    .owner
                    .permission
                    .as_ref()
                    .expect("authorized remote read");
                let route = match &read.response {
                    ReadResponse::Provider => {
                        read.owner.wallet.routes[&permission.chain_id].clone()
                    }
                    ReadResponse::Native { route, .. } => route.clone(),
                };
                let http = read.owner.wallet.http.clone().expect("available context");
                let origin = match &read.response {
                    ReadResponse::Provider => read.owner.origin.clone(),
                    ReadResponse::Native { .. } => WalletRpcOrigin::PublicWallet.into(),
                };
                let deadline = read.ticket.deadline;
                let lookup = matches!(read.response, ReadResponse::Provider)
                    .then(|| rpc.transaction_hash_lookup())
                    .flatten()
                    .map(|(kind, hash)| {
                        (
                            kind,
                            hash,
                            read.owner
                                .wallet
                                .public_transaction_tracker
                                .lookup(permission.chain_id, hash),
                        )
                    });
                read.phase = ReadPhase::Running;
                self.jobs.spawn(async move {
                    let outcome = std::panic::AssertUnwindSafe(super::reads::submit_read(
                        http, route, origin, rpc, lookup, deadline,
                    ))
                    .catch_unwind()
                    .await
                    .unwrap_or(Err(super::reads::ReadError::Broker(
                        RpcBrokerError::InvalidResponse,
                    )));
                    (id, outcome)
                });
            }
            local => {
                let outcome = match local {
                    ReadWork::Accounts => Ok(json!(self.disclosure(&read.owner.origin).accounts)),
                    ReadWork::Chain => Ok(json!(chain_hex(
                        read.owner
                            .permission
                            .as_ref()
                            .expect("authorized chain read")
                            .chain_id
                    ))),
                    ReadWork::Remote { .. } => unreachable!(),
                };
                read.phase = ReadPhase::Delivery;
                self.respond(
                    read.owner.session,
                    &read.owner.document,
                    &read.owner.request_id,
                    outcome,
                );
            }
        }
        self.reads.insert(id, read);
    }
    pub(super) fn complete_read(&mut self, (id, result): ReadCompletion) {
        self.reload_permissions();
        self.refresh_documents(false);
        let Some(mut read) = self.reads.remove(&id) else {
            return;
        };
        if matches!(read.response, ReadResponse::Native { .. }) {
            let validation = self
                .validate_read(&read, Instant::now())
                .map_err(|code| approvals::code_broker(code, &read.owner));
            if let ReadResponse::Native {
                reply: Some(reply), ..
            } = read.response
            {
                let outcome = validation.and_then(|()| {
                    result.map_err(|error| match error {
                        super::reads::ReadError::Broker(error) => error,
                        super::reads::ReadError::Unavailable => RpcBrokerError::Timeout,
                    })
                });
                let _ = reply.send(outcome);
            }
            self.finish_ticket(id);
            return;
        }
        if read.retired {
            // Broker capacity is released only now. Retain unsent terminal metadata.
            self.finish_ticket(id);
            if !read.delivered
                && self
                    .documents
                    .get(&(read.owner.session, read.owner.document.clone()))
                    .is_some_and(|doc| doc.incarnation == read.owner.incarnation)
            {
                read.phase = ReadPhase::Delivery;
                self.reads.insert(id, read);
            }
            return;
        }
        read.phase = ReadPhase::Delivery;
        if let Err(code) = self.validate_read(&read, Instant::now()) {
            self.respond(
                read.owner.session,
                &read.owner.document,
                &read.owner.request_id,
                Err(code),
            );
        } else if matches!(&result, Err(super::reads::ReadError::Unavailable)) {
            self.respond(
                read.owner.session,
                &read.owner.document,
                &read.owner.request_id,
                Err(-32002),
            );
        } else {
            let availability = self.availability(&read.owner);
            let outcome = result.map_err(|error| match error {
                super::reads::ReadError::Broker(error) => {
                    ProviderRpcError::from_broker(error, availability)
                }
                super::reads::ReadError::Unavailable => unreachable!(),
            });
            self.respond_outcome(
                read.owner.session,
                &read.owner.document,
                &read.owner.request_id,
                outcome,
            );
        }
        self.reads.insert(id, read);
    }
    fn availability(&self, owner: &ReadOwner) -> ProviderAvailability {
        if self.wallet.http.is_none()
            || !self
                .documents
                .contains_key(&(owner.session, owner.document.clone()))
        {
            ProviderAvailability::Disconnected
        } else if owner
            .permission
            .as_ref()
            .is_none_or(|permission| !self.wallet.routes.contains_key(&permission.chain_id))
        {
            ProviderAvailability::ChainUnavailable
        } else {
            ProviderAvailability::Available
        }
    }
    fn finish_ticket(&mut self, id: u64) {
        let updates = self.admission.complete(id, Instant::now());
        self.apply_updates(updates);
    }
    fn apply_updates(&mut self, updates: ReadAdmissionUpdates) {
        for ticket in updates.expired {
            if let Some(mut read) = self.reads.remove(&ticket.id) {
                if let ReadResponse::Native { reply, .. } = &mut read.response {
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(Err(RpcBrokerError::Timeout));
                    }
                    continue;
                }
                self.respond(
                    read.owner.session,
                    &read.owner.document,
                    &read.owner.request_id,
                    Err(-32002),
                );
                read.phase = ReadPhase::Delivery;
                read.retired = true;
                self.reads.insert(ticket.id, read);
            }
        }
        for ticket in updates.ready {
            self.dispatch_read(ticket.id);
        }
    }
    fn invalidate_reads(&mut self, now: Instant) {
        let invalid: Vec<_> = self
            .reads
            .iter()
            .filter_map(|(&id, read)| {
                (!read.retired
                    || (read.phase == ReadPhase::Delivery
                        && !self
                            .documents
                            .contains_key(&(read.owner.session, read.owner.document.clone()))))
                .then(|| self.validate_read(read, now).err().map(|code| (id, code)))
                .flatten()
            })
            .collect();
        for (id, _) in &invalid {
            self.admission.cancel_queued(*id);
        }
        for (id, code) in invalid {
            let Some(mut read) = self.reads.remove(&id) else {
                continue;
            };
            let owner = &read.owner;
            if let ReadResponse::Native { reply, .. } = &mut read.response {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(approvals::code_broker(code, owner)));
                }
                if read.phase == ReadPhase::Running {
                    read.retired = true;
                    self.reads.insert(id, read);
                } else {
                    self.finish_ticket(id);
                }
                continue;
            }
            self.outbox.retain(|(session, message)| *session != owner.session || !matches!(&message.message,
                GatewayServerMessage::ProviderResponse { document, request_id, .. } if document == &owner.document && request_id == &owner.request_id));
            if !read.enqueued
                && self
                    .documents
                    .contains_key(&(owner.session, owner.document.clone()))
            {
                self.respond(owner.session, &owner.document, &owner.request_id, Err(code));
            }
            match read.phase {
                ReadPhase::Queued => {
                    self.admission.cancel_queued(id);
                    if self
                        .documents
                        .contains_key(&(read.owner.session, read.owner.document.clone()))
                    {
                        read.phase = ReadPhase::Delivery;
                        read.retired = true;
                        self.reads.insert(id, read);
                    }
                }
                ReadPhase::Running => {
                    read.retired = true;
                    self.reads.insert(id, read);
                }
                ReadPhase::Delivery => {
                    if self
                        .documents
                        .contains_key(&(read.owner.session, read.owner.document.clone()))
                    {
                        read.retired = true;
                        self.reads.insert(id, read);
                    } else {
                        self.finish_ticket(id);
                    }
                }
            }
        }
    }
    pub(super) const fn authority(&self) -> &watch::Receiver<GatewayWalletState> {
        &self.authority
    }

    /// Final synchronous transport check. Never borrow the watch or approval control here:
    /// the caller holds its watch read guard through the socket operation.
    pub(super) fn delivery_authority(
        &self,
        session: u64,
        delivery: &mut Delivery,
        authority: &GatewayWalletState,
    ) -> DeliveryStatus {
        match &mut delivery.message {
            GatewayServerMessage::ProviderResponse {
                document,
                generation,
                document_generation,
                outcome,
                ..
            } => {
                if delivery.registration_rejection && delivery.incarnation.is_none() {
                    return DeliveryStatus::Current;
                }
                let Some(doc) = self.documents.get(&(session, document.clone())) else {
                    return DeliveryStatus::Discard;
                };
                if delivery.incarnation != Some(doc.incarnation) {
                    return DeliveryStatus::Discard;
                }
                if delivery.local_error || authority.same_authority(&self.wallet) {
                    return DeliveryStatus::Current;
                }
                *outcome = GatewayProviderOutcome::Failure {
                    error: safe_error(if authority.view.is_none() && self.wallet.view.is_some() {
                        4100
                    } else {
                        -32002
                    }),
                };
                *generation = self.generation;
                *document_generation = doc.generation;
                delivery.local_error = true;
                DeliveryStatus::Changed
            }
            GatewayServerMessage::ProviderState { .. }
            | GatewayServerMessage::UiSnapshot { .. }
            | GatewayServerMessage::State { .. }
                if !authority.same_authority(&self.wallet) =>
            {
                DeliveryStatus::Discard
            }
            _ => DeliveryStatus::Current,
        }
    }

    /// Revalidate before sealing and before each bounded socket progress step.
    pub(super) fn delivery(&self, session: u64, delivery: &mut Delivery) -> DeliveryStatus {
        match &mut delivery.message {
            GatewayServerMessage::ProviderResponse {
                document,
                generation,
                document_generation,
                outcome,
                ..
            } => {
                if delivery.registration_rejection && delivery.incarnation.is_none() {
                    return DeliveryStatus::Current;
                }
                let Some(doc) = self.documents.get(&(session, document.clone())) else {
                    return DeliveryStatus::Discard;
                };
                if delivery.incarnation != Some(doc.incarnation) {
                    return DeliveryStatus::Discard;
                }
                if delivery.local_error {
                    return DeliveryStatus::Current;
                }
                let invalid = if let Some(approval) = &delivery.approval {
                    approval
                        .control
                        .ensure_current()
                        .err()
                        .map(|error| approvals::broker_code(&error))
                        .or_else(|| {
                            self.validate_owner(
                                &approval.owner,
                                true,
                                approval.deadline,
                                Instant::now(),
                            )
                            .err()
                        })
                } else if let Some((ticket, owner, remote)) = &delivery.read {
                    self.validate_owner(owner, *remote, ticket.deadline, Instant::now())
                        .err()
                } else if *generation != self.generation || *document_generation != doc.generation {
                    Some(if self.wallet.view.is_none() {
                        4100
                    } else {
                        -32002
                    })
                } else {
                    None
                };
                if let Some(code) = invalid {
                    *outcome = GatewayProviderOutcome::Failure {
                        error: safe_error(code),
                    };
                    *generation = self.generation;
                    *document_generation = doc.generation;
                    delivery.local_error = true;
                    return DeliveryStatus::Changed;
                }
            }
            GatewayServerMessage::ProviderState {
                document,
                generation,
                document_generation,
                accounts,
                chain_id,
                ..
            } => {
                let Some(doc) = self.documents.get(&(session, document.clone())) else {
                    return DeliveryStatus::Discard;
                };
                if delivery.incarnation != Some(doc.incarnation)
                    || *generation != self.generation
                    || *document_generation != doc.generation
                    || *accounts != doc.disclosure.accounts
                    || *chain_id != doc.disclosure.chain_id
                {
                    return DeliveryStatus::Discard;
                }
            }
            GatewayServerMessage::UiSnapshot { .. } => {
                // Includes account labels and pending approval ownership, not just the wallet epoch.
                if serde_json::to_value(&delivery.message).ok()
                    != serde_json::to_value(self.ui(session)).ok()
                {
                    return DeliveryStatus::Discard;
                }
            }
            GatewayServerMessage::State { generation, .. } if *generation != self.generation => {
                return DeliveryStatus::Discard;
            }
            _ => {}
        }
        self.delivery_authority(session, delivery, &self.authority.borrow())
    }
    pub(super) fn delivered(&mut self, id: u64) {
        if let Some(read) = self.reads.get_mut(&id) {
            if read.phase == ReadPhase::Running {
                read.delivered = true;
            } else if read.phase == ReadPhase::Delivery {
                self.reads.remove(&id);
                self.finish_ticket(id);
            }
        }
    }
    pub(super) fn resolve_connect(
        &mut self,
        session: u64,
        peer: PeerId,
        approval_id: &str,
        account_uuid: Option<&str>,
        chain_id: u64,
        now: Instant,
    ) {
        let peer_id = alloy::hex::encode(peer.to_bytes());
        let Some(index) = self.pending.iter().position(|pending| {
            pending.session == session
                && pending.approval_id == approval_id
                && pending.origin.paired_peer_id() == Some(peer_id.as_str())
        }) else {
            return;
        };
        let pending = self.pending.remove(index);
        let outcome = if now >= pending.deadline {
            Err(if self.wallet.view.is_none() {
                4100
            } else {
                -32002
            })
        } else if account_uuid.is_none() {
            Err(4001)
        } else if let Some(view) = self.wallet.view.as_ref() {
            if self.wallet.chain_ids.contains(&chain_id) {
                self.store
                    .grant_gateway_permission(
                        view,
                        &pending.origin,
                        account_uuid.expect("selected account"),
                        chain_id,
                    )
                    .map(|_| {
                        tracing::info!("gateway connection permission saved");
                    })
                    .map_err(|error| {
                        let error_kind = match error {
                            VaultError::Decrypt => "decrypt",
                            VaultError::Encrypt => "encrypt",
                            VaultError::Decode(_) => "decode",
                            VaultError::Encode(_) => "encode",
                            VaultError::Db(_) => "database",
                            VaultError::InvalidGatewayPermission => "invalid_gateway_permission",
                            VaultError::PublicAccountNotFound => "public_account_not_found",
                            VaultError::InvalidPublicAccountOperation => {
                                "invalid_public_account_operation"
                            }
                            VaultError::Random => "random",
                            _ => "other",
                        };
                        tracing::warn!(error_kind, "gateway connection permission save failed");
                        4100
                    })
            } else {
                Err(4901)
            }
        } else {
            Err(4100)
        };
        self.reload_permissions();
        // Permission state precedes the successful connect response on every owning document.
        self.refresh_documents(false);
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        self.push_pending_ui();
        self.push_ui(session);
        match outcome {
            Ok(()) => self.admit_read(
                pending.session,
                pending.document,
                pending.request_id,
                "eth_requestAccounts",
                json!([]),
                Instant::now(),
            ),
            Err(code) => self.respond(
                pending.session,
                &pending.document,
                &pending.request_id,
                Err(code),
            ),
        }
    }
    pub(super) fn revoke(&mut self, permission_id: &str) -> Result<(), GatewayError> {
        let view = self.wallet.view.as_ref().ok_or(GatewayError::Unavailable)?;
        let permission = self
            .permissions
            .iter()
            .find(|permission| permission.permission_id == permission_id)
            .ok_or(GatewayError::Unavailable)?
            .clone();
        self.store
            .delete_gateway_permission(view, permission_id)
            .map_err(|_| GatewayError::Storage)?;
        self.reload_permissions();
        self.refresh_documents(false);
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        let mut retained = Vec::new();
        let mut changed = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            if pending.origin == permission.origin {
                self.respond(
                    pending.session,
                    &pending.document,
                    &pending.request_id,
                    Err(4100),
                );
                changed.push(pending.session);
            } else {
                retained.push(pending);
            }
        }
        self.pending = retained;
        changed.sort_unstable();
        changed.dedup();
        for session in changed {
            self.push_ui(session);
        }
        self.push_pending_ui();
        Ok(())
    }
    pub(super) fn tick(&mut self, now: Instant) {
        self.reload_permissions();
        self.refresh_wallet_switches(now);
        self.refresh_documents(false);
        self.refresh_approvals(now);
        self.invalidate_reads(now);
        let expired = self.admission.expire_queued(now);
        self.apply_updates(ReadAdmissionUpdates {
            ready: Vec::new(),
            expired,
        });
        let mut retained = Vec::new();
        let mut changed = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            if now >= pending.deadline {
                self.respond(
                    pending.session,
                    &pending.document,
                    &pending.request_id,
                    Err(if self.wallet.view.is_none() {
                        4100
                    } else {
                        -32002
                    }),
                );
                changed.push(pending.session);
            } else {
                retained.push(pending);
            }
        }
        self.pending = retained;
        for session in changed {
            self.push_ui(session);
        }
    }
    fn reject_registration(&mut self, session: u64, document: &str, code: i32) {
        self.respond(session, document, "", Err(code));
        if let Some((_, delivery)) = self.outbox.last_mut() {
            // Existing document IDs keep their captured incarnation guard.
            delivery.registration_rejection = true;
        }
    }
    fn respond(
        &mut self,
        session: u64,
        document: &str,
        request_id: &str,
        outcome: Result<Value, i32>,
    ) {
        let local_error = outcome.is_err();
        self.respond_outcome(session, document, request_id, outcome.map_err(safe_error));
        if let Some((_, delivery)) = self.outbox.last_mut() {
            delivery.local_error = local_error;
        }
    }
    fn respond_error(
        &mut self,
        session: u64,
        document: &str,
        request_id: &str,
        error: ProviderRpcError,
    ) {
        self.respond_outcome(session, document, request_id, Err(error));
    }
    fn respond_outcome(
        &mut self,
        session: u64,
        document: &str,
        request_id: &str,
        outcome: Result<Value, ProviderRpcError>,
    ) {
        let document_generation = self
            .documents
            .get(&(session, document.to_owned()))
            .map_or(0, |owner| owner.generation);
        self.outbox.push((
            session,
            Delivery {
                incarnation: self
                    .documents
                    .get(&(session, document.to_owned()))
                    .map(|doc| doc.incarnation),
                local_error: false,
                registration_rejection: false,
                read: None,
                approval: None,
                message: GatewayServerMessage::ProviderResponse {
                    version: 1,
                    document: document.to_owned(),
                    generation: self.generation,
                    document_generation,
                    request_id: request_id.to_owned(),
                    outcome: match outcome {
                        Ok(result) => GatewayProviderOutcome::Success { result },
                        Err(error) => GatewayProviderOutcome::Failure { error },
                    },
                },
            },
        ));
    }
    fn push_pending_ui(&mut self) {
        self.publish_wallet_switches();
        let mut sessions: Vec<_> = self
            .pending
            .iter()
            .map(|pending| pending.session)
            .chain(self.approvals.iter().map(|pending| pending.session))
            .chain(self.ui_snapshots.keys().copied())
            .collect();
        sessions.sort_unstable();
        sessions.dedup();
        for session in sessions {
            self.push_ui(session);
        }
    }
    pub(super) fn push_ui(&mut self, session: u64) {
        self.publish_wallet_switches();
        let message = self.ui(session);
        let serialized = serde_json::to_value(&message).expect("serializable UI snapshot");
        if self.ui_snapshots.get(&session) != Some(&serialized) {
            self.ui_snapshots.insert(session, serialized);
            self.outbox.push((session, Delivery::control(message)));
        }
    }
    fn ui(&self, session: u64) -> GatewayServerMessage {
        let accounts = if self
            .pending
            .iter()
            .any(|pending| pending.session == session)
        {
            self.wallet
                .view
                .as_ref()
                .and_then(|view| {
                    self.store
                        .list_active_public_accounts_for_session(view)
                        .ok()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let pending_connects = self.pending.iter().filter(|pending| pending.session == session).map(|pending| {
            let wrong_wallet = self.wallet.view.as_ref().is_some_and(|view| self.permissions.iter().find(|permission| permission.origin == pending.origin).is_some_and(|permission| matches!(self.store.resolve_dapp_session_account(view, &permission.public_account_uuid, &permission.public_account_scope, permission.owning_private_wallet_uuid.as_deref()), Ok(WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet { .. }))));
            GatewayConnectPrompt {
                request_id: pending.approval_id.clone(), url: pending.origin.web_origin().expect("dapp origin").as_str().to_owned(), paired_peer_id: pending.origin.paired_peer_id().expect("dapp peer").to_owned(),
                needs_unlock: self.wallet.view.is_none(), wrong_wallet,
                accounts: accounts.iter().map(|account| GatewayAccountChoice { uuid: account.public_account_uuid.clone(), label: account.label.clone(), address: account.address.to_string() }).collect(),
                chains: self.wallet.chain_ids.iter().map(|&id| GatewayChainChoice { id, name: railgun_ui::chains::chain_name(id).map_or_else(|| format!("Chain {id}"), str::to_owned) }).collect(), default_chain_id: self.wallet.default_chain_id,
            }
        }).collect();
        // The unlocked wallet's own accounts, so the peer can list them without a prompt.
        let wallet_accounts = self.wallet.view.as_ref().map_or_else(Vec::new, |view| {
            self.wallet
                .public_accounts
                .iter()
                .filter(|account| account.is_active_for_wallet(view.wallet_id()))
                .map(|account| GatewayAccountChoice {
                    uuid: account.public_account_uuid.clone(),
                    label: account.label.clone(),
                    address: account.address.to_string(),
                })
                .collect()
        });
        GatewayServerMessage::UiSnapshot {
            version: 1,
            generation: self.generation,
            locked: self.wallet.view.is_none(),
            accounts: wallet_accounts,
            public_view: self.wallet.public_view.clone(),
            chains: self
                .wallet
                .chain_ids
                .iter()
                .map(|&id| GatewayChainChoice {
                    id,
                    name: railgun_ui::chain_name(id)
                        .map_or_else(|| format!("Chain {id}"), str::to_owned),
                })
                .collect(),
            permissions: self.ui_peers.get(&session).map_or_else(Vec::new, |peer| {
                let peer = alloy::hex::encode(peer.to_bytes());
                self.permissions()
                    .into_iter()
                    .filter(|permission| permission.paired_peer_id == peer)
                    .map(|permission| super::GatewaySitePermission {
                        permission_id: permission.permission_id,
                        origin: permission.url,
                        account_uuid: permission.public_account_uuid,
                        chain_id: permission.chain_id,
                    })
                    .collect()
            }),
            ui_error: self.ui_errors.get(&session).cloned(),
            pending_connects,
            pending_requests: self.pending_request_summaries(session),
        }
    }
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
