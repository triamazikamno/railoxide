//! Desktop-owned authenticated transport and public-account disclosure permissions.
mod actor;
mod admission;
mod balance_reads;
mod errors;
pub use errors::{GatewayApprovalFailure, LocalProviderFailure, ProviderRpcError};
mod provider;
mod reads;
pub use provider::{
    GatewayAccountChoice, GatewayApprovalRequest, GatewayChainChoice, GatewayConnectPrompt,
    GatewayPermissionSummary, GatewayUnlockState, GatewayWalletState, GatewayWalletSwitchRequest,
    GatewayWalletSwitchTransition,
};
mod storage;
mod ui;
pub use ui::{
    GatewayAccountBalances, GatewayAssetBalance, GatewayPendingRequest, GatewayPublicCommand,
    GatewayPublicView, GatewaySitePermission, GatewayUiEvent, GatewayUiEventKind,
};

pub use dapp_gateway_protocol::{PairingCode, PeerId};
pub mod policy;

use local_db::DbStore;
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU16,
    sync::Arc,
};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub enabled: bool,
    pub bind_address: IpAddr,
    pub port: NonZeroU16,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: Ipv4Addr::LOCALHOST.into(),
            port: NonZeroU16::new(43110).expect("default gateway port is nonzero"),
        }
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum GatewayError {
    #[error("Gateway storage is unavailable")]
    Storage,
    #[error("Gateway storage is invalid or unsupported")]
    InvalidStorage,
    #[error("Gateway listener is unavailable")]
    Listener,
    #[error("Browser gateway port {0} is already in use; choose another browser gateway port")]
    PortInUse(u16),
    #[error("Gateway is disabled")]
    Disabled,
    #[error("Gateway operation is unavailable")]
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GatewayPeerSummary {
    pub id: PeerId,
    pub label: Option<String>,
    pub connected_sessions: usize,
    pub paired_at: u64,
    pub last_active_at: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct GatewaySnapshot {
    pub config: GatewayConfig,
    pub listener_addr: Option<SocketAddr>,
    pub peers: Vec<GatewayPeerSummary>,
    pub pairing_active: bool,
    pub locked: bool,
    pub generation: u64,
    pub error: Option<GatewayError>,
    pub permissions: Vec<GatewayPermissionSummary>,
}

#[derive(Debug)]
pub struct GatewayPairingOffer {
    pub code: PairingCode,
    pub expires_in_secs: u64,
}

/// Version is checked separately so unsupported versions never execute a command.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayClientMessage {
    PublicView {
        version: u16,
        generation: u64,
        command: GatewayPublicCommand,
    },
    GetState {
        version: u16,
    },
    Heartbeat {
        version: u16,
    },
    RegisterDocument {
        version: u16,
        document: String,
        url: String,
    },
    UnregisterDocument {
        version: u16,
        document: String,
    },
    ProviderRequest {
        version: u16,
        document: String,
        request_id: String,
        method: String,
        params: serde_json::Value,
    },
    SummonDesktop {
        version: u16,
        generation: u64,
    },
    UserActivity {
        version: u16,
        generation: u64,
    },
    RequestWalletSwitch {
        version: u16,
        generation: u64,
        request_id: String,
    },
    ResolveConnect {
        version: u16,
        request_id: String,
        public_account_uuid: Option<String>,
        chain_id: u64,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayServerMessage {
    State {
        version: u16,
        locked: bool,
        generation: u64,
    },
    Heartbeat {
        version: u16,
    },
    Unsupported {
        version: u16,
    },
    ProviderState {
        version: u16,
        document: String,
        generation: u64,
        document_generation: u64,
        accounts: Vec<String>,
        chain_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        invalidation_code: Option<i32>,
    },
    ProviderResponse {
        version: u16,
        document: String,
        generation: u64,
        document_generation: u64,
        request_id: String,
        #[serde(flatten)]
        outcome: GatewayProviderOutcome,
    },
    UiSnapshot {
        version: u16,
        generation: u64,
        locked: bool,
        accounts: Vec<GatewayAccountChoice>,
        public_view: GatewayPublicView,
        chains: Vec<GatewayChainChoice>,
        permissions: Vec<GatewaySitePermission>,
        ui_error: Option<String>,
        pending_connects: Vec<GatewayConnectPrompt>,
        pending_requests: Vec<GatewayPendingRequest>,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum GatewayProviderOutcome {
    Success { result: serde_json::Value },
    Failure { error: ProviderRpcError },
}

enum Command {
    Summaries(Box<GatewayWalletState>, u64, Vec<(String, String)>),
    BeginWalletSwitch(
        String,
        oneshot::Sender<Result<Arc<GatewayWalletSwitchRequest>, GatewayError>>,
    ),
    RejectWalletSwitch(String),
    Configure(GatewayConfig, oneshot::Sender<Result<(), GatewayError>>),
    Pair(oneshot::Sender<Result<GatewayPairingOffer, GatewayError>>),
    Revoke(PeerId, oneshot::Sender<Result<(), GatewayError>>),
    State(bool, u64, oneshot::Sender<Result<(), GatewayError>>),
    WalletState(
        Box<GatewayWalletState>,
        u64,
        oneshot::Sender<Result<(), GatewayError>>,
    ),
    RevokePermission(String, oneshot::Sender<Result<(), GatewayError>>),
    ReturnApprovalToReview(String, oneshot::Sender<Result<(), LocalProviderFailure>>),
    BeginApproval(
        String,
        oneshot::Sender<Result<Arc<GatewayApprovalRequest>, LocalProviderFailure>>,
    ),
    CompleteApproval(
        String,
        Result<serde_json::Value, GatewayApprovalFailure>,
        oneshot::Sender<Result<(), LocalProviderFailure>>,
    ),
    ApprovalRead(
        String,
        crate::SensitiveUrl,
        crate::RpcRead,
        tokio::time::Instant,
        oneshot::Sender<Result<serde_json::Value, crate::RpcBrokerError>>,
    ),
    Shutdown(oneshot::Sender<Result<(), GatewayError>>),
}

#[derive(Clone)]
pub struct GatewayHandle {
    commands: mpsc::Sender<Command>,
    ui_events: tokio::sync::broadcast::Sender<GatewayUiEvent>,
    snapshots: watch::Receiver<GatewaySnapshot>,
    approvals: watch::Receiver<Vec<Arc<GatewayApprovalRequest>>>,
    wallet_switches: watch::Receiver<Vec<Arc<GatewayWalletSwitchRequest>>>,
    authority: watch::Sender<GatewayWalletState>,
}

impl GatewayHandle {
    /// Call inside the desktop-owned Tokio runtime. Storage errors fail closed in snapshots.
    #[must_use]
    pub fn start(db: Arc<DbStore>, locked: bool, generation: u64) -> Self {
        let (ui_events, _) = tokio::sync::broadcast::channel(16);
        let (commands, receiver) = mpsc::channel(32);
        let (updates, snapshots) = watch::channel(GatewaySnapshot {
            config: GatewayConfig::default(),
            listener_addr: None,
            peers: Vec::new(),
            pairing_active: false,
            locked,
            generation,
            error: None,
            permissions: Vec::new(),
        });
        let (approval_updates, approvals) = watch::channel(Vec::new());
        let (switch_updates, wallet_switches) = watch::channel(Vec::new());
        let (authority, authority_receiver) = watch::channel(GatewayWalletState::default());
        let _actor = tokio::spawn(actor::run(
            db,
            receiver,
            updates,
            locked,
            generation,
            approval_updates,
            authority_receiver,
            ui_events.clone(),
            switch_updates,
        ));
        Self {
            commands,
            ui_events,
            snapshots,
            approvals,
            wallet_switches,
            authority,
        }
    }

    #[must_use]
    pub fn wallet_switch_requests(&self) -> watch::Receiver<Vec<Arc<GatewayWalletSwitchRequest>>> {
        self.wallet_switches.clone()
    }

    pub async fn begin_wallet_switch(
        &self,
        id: String,
    ) -> Result<Arc<GatewayWalletSwitchRequest>, GatewayError> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::BeginWalletSwitch(id, reply))
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        result.await.map_err(|_| GatewayError::Unavailable)?
    }

    pub async fn reject_wallet_switch(&self, id: String) {
        let _ = self.commands.send(Command::RejectWalletSwitch(id)).await;
    }

    #[must_use]
    pub fn ui_events(&self) -> tokio::sync::broadcast::Receiver<GatewayUiEvent> {
        self.ui_events.subscribe()
    }

    /// Informational text computed by the desktop's shared intent presentation.
    pub async fn publish_summaries(
        &self,
        wallet: GatewayWalletState,
        generation: u64,
        summaries: Vec<(String, String)>,
    ) {
        let _ = self
            .commands
            .send(Command::Summaries(Box::new(wallet), generation, summaries))
            .await;
    }

    #[must_use]
    pub fn snapshots(&self) -> watch::Receiver<GatewaySnapshot> {
        self.snapshots.clone()
    }

    pub async fn configure(&self, config: GatewayConfig) -> Result<(), GatewayError> {
        if !config.enabled {
            self.invalidate_approvals(&crate::RpcBrokerError::Shutdown);
        }
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Configure(config, tx))
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        rx.await.map_err(|_| GatewayError::Unavailable)?
    }

    pub async fn issue_pairing_code(&self) -> Result<GatewayPairingOffer, GatewayError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Pair(tx))
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        rx.await.map_err(|_| GatewayError::Unavailable)?
    }

    pub async fn revoke_peer(&self, peer: PeerId) -> Result<(), GatewayError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Revoke(peer, tx))
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        rx.await.map_err(|_| GatewayError::Unavailable)?
    }

    pub async fn update_desktop_state(
        &self,
        locked: bool,
        generation: u64,
    ) -> Result<(), GatewayError> {
        self.set_wallet_authority(GatewayWalletState::default());
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::State(locked, generation, tx))
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        rx.await.map_err(|_| GatewayError::Unavailable)?
    }

    pub async fn update_wallet_state(
        &self,
        state: GatewayWalletState,
        generation: u64,
    ) -> Result<(), GatewayError> {
        self.set_wallet_authority(state.clone());
        self.publish_wallet_state(state, generation).await
    }

    /// Queue a state whose immediate authority was already published synchronously.
    pub async fn publish_wallet_state(
        &self,
        state: GatewayWalletState,
        generation: u64,
    ) -> Result<(), GatewayError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::WalletState(Box::new(state), generation, tx))
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        rx.await.map_err(|_| GatewayError::Unavailable)?
    }

    pub async fn revoke_permission(&self, permission_id: String) -> Result<(), GatewayError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::RevokePermission(permission_id, tx))
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        rx.await.map_err(|_| GatewayError::Unavailable)?
    }

    /// Stops accepting, aborts and awaits upgrades, and drops all sessions before returning.
    pub async fn shutdown(&self) -> Result<(), GatewayError> {
        self.invalidate_approvals(&crate::RpcBrokerError::Shutdown);
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Shutdown(tx))
            .await
            .map_err(|_| GatewayError::Unavailable)?;
        rx.await.map_err(|_| GatewayError::Unavailable)?
    }
}

impl GatewayHandle {
    #[must_use]
    pub fn approval_requests(&self) -> watch::Receiver<Vec<Arc<GatewayApprovalRequest>>> {
        self.approvals.clone()
    }
    pub fn set_wallet_authority(&self, state: GatewayWalletState) {
        self.authority.send_replace(state);
        for request in self.wallet_switches.borrow().iter() {
            if let Err(error) = request.control.ensure_current() {
                request.control.invalidate(&error);
            }
        }
        for request in self.approvals.borrow().iter() {
            if let Err(error) = request.control.ensure_current() {
                request.control.invalidate(&error);
            }
        }
    }
    fn invalidate_approvals(&self, error: &crate::RpcBrokerError) {
        for request in self.wallet_switches.borrow().iter() {
            request.control.invalidate(error);
        }
        for request in self.approvals.borrow().iter() {
            request.control.invalidate(error);
        }
    }
    pub async fn begin_approval(
        &self,
        id: String,
    ) -> Result<Arc<GatewayApprovalRequest>, LocalProviderFailure> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .try_send(Command::BeginApproval(id, tx))
            .map_err(|_| LocalProviderFailure::LimitExceeded)?;
        rx.await.map_err(|_| LocalProviderFailure::Disconnected)?
    }
    pub async fn return_approval_to_review(&self, id: String) -> Result<(), LocalProviderFailure> {
        let control = self.approval_control(&id)?;
        let (tx, rx) = oneshot::channel();
        self.deliver_approval_command(control, Command::ReturnApprovalToReview(id, tx), rx)
            .await
    }
    pub async fn complete_approval(
        &self,
        id: String,
        result: Result<serde_json::Value, GatewayApprovalFailure>,
    ) -> Result<(), LocalProviderFailure> {
        let control = self.approval_control(&id)?;
        let (tx, rx) = oneshot::channel();
        self.deliver_approval_command(control, Command::CompleteApproval(id, result, tx), rx)
            .await
    }
    fn approval_control(
        &self,
        id: &str,
    ) -> Result<crate::dapp_request::DappRequestControl, LocalProviderFailure> {
        self.approvals
            .borrow()
            .iter()
            .find(|request| request.id == id)
            .map(|request| request.control.clone())
            .ok_or(LocalProviderFailure::Unavailable)
    }
    async fn deliver_approval_command(
        &self,
        control: crate::dapp_request::DappRequestControl,
        command: Command,
        reply: oneshot::Receiver<Result<(), LocalProviderFailure>>,
    ) -> Result<(), LocalProviderFailure> {
        tokio::select! {
            biased;
            () = control.cancelled() => Err(LocalProviderFailure::Unavailable),
            result = async {
                self.commands.send(command).await.map_err(|_| LocalProviderFailure::Disconnected)?;
                reply.await.map_err(|_| LocalProviderFailure::Disconnected)?
            } => result,
        }
    }
    #[must_use]
    pub fn approval_reads(&self, id: String) -> crate::public_wallet::DappRpcReadClient {
        let commands = self.commands.clone();
        let approvals = self.approvals.clone();
        crate::public_wallet::DappRpcReadClient::new(move |endpoint, rpc| {
            let entered = tokio::time::Instant::now();
            let control = approvals
                .borrow()
                .iter()
                .find(|request| request.id == id)
                .map(|request| request.control.clone());
            let commands = commands.clone();
            let (reply, response) = oneshot::channel();
            let command = Command::ApprovalRead(id.clone(), endpoint, rpc, entered, reply);
            Box::pin(async move {
                let control = control.ok_or(crate::RpcBrokerError::OriginRejected)?;
                let deadline = entered + admission::READ_TIMEOUT;
                tokio::select! {
                    biased;
                    () = control.cancelled() => {
                        return Err(control.ensure_current().err().unwrap_or(crate::RpcBrokerError::Timeout));
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        return Err(crate::RpcBrokerError::TimeoutBeforeDispatch);
                    }
                    sent = commands.send(command) => {
                        sent.map_err(|_| crate::RpcBrokerError::Shutdown)?;
                    }
                }
                tokio::select! {
                    biased;
                    () = control.cancelled() => {
                        Err(control.ensure_current().err().unwrap_or(crate::RpcBrokerError::Timeout))
                    }
                    () = tokio::time::sleep_until(deadline) => Err(crate::RpcBrokerError::Timeout),
                    result = response => result.map_err(|_| crate::RpcBrokerError::Shutdown)?,
                }
            })
        })
    }
}
