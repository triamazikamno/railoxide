//! Ephemeral native UI requests. Only authenticated transport frames create these events.
use super::GatewayWalletState;
use crate::dapp_request::DappRequestControl;
use serde::{Deserialize, Serialize};

/// Desktop-formatted public wallet presentation, never a signing capability.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayPublicView {
    pub selected_account: Option<String>,
    pub selected_chain: Option<u64>,
    pub balances: Vec<GatewayAccountBalances>,
    pub refreshing: bool,
    pub balance_error: bool,
    pub drafts: Vec<super::GatewayDraftView>,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayAccountBalances {
    pub account_uuid: String,
    pub total: Option<String>,
    pub assets: Vec<GatewayAssetBalance>,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct GatewayAssetBalance {
    pub asset: String,
    pub symbol: String,
    pub amount: String,
    /// Exact token balance for Max, before recipient or fee validation.
    pub max_amount: Option<String>,
    pub usd: Option<String>,
    pub icon: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GatewaySitePermission {
    pub permission_id: String,
    pub origin: String,
    pub account_uuid: String,
    pub chain_id: u64,
}

/// Only authenticated extension UI can submit these application commands.
#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayPublicCommand {
    Draft {
        command: Box<super::GatewayDraftCommand>,
    },
    SelectAccount {
        public_account_uuid: String,
    },
    SelectChain {
        chain_id: u64,
    },
    RefreshBalances,
    RevokePermission {
        permission_id: String,
    },
    ReissuePermission {
        permission_id: String,
        public_account_uuid: String,
    },
    ConnectTab {
        document: String,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GatewayPendingRequest {
    pub request_id: String,
    pub url: String,
    pub needs_unlock: bool,
    pub summary: Option<String>,
}

#[derive(Clone)]
pub enum GatewayUiEventKind {
    Network {
        request: super::GatewayNetworkRequest,
    },
    SummonDesktop,
    UserActivity,
    PrivateView {
        command: super::GatewayPrivateCommand,
    },
    PublicView {
        peer_id: String,
        command: GatewayPublicCommand,
    },
}

#[derive(Clone)]
pub struct GatewayUiEvent {
    pub generation: u64,
    pub kind: GatewayUiEventKind,
    pub(super) wallet: GatewayWalletState,
    pub(super) control: DappRequestControl,
}
impl GatewayUiEvent {
    /// GPUI uses its immediate publication, never the asynchronously observed actor snapshot.
    #[must_use]
    pub fn is_current(&self, wallet: &GatewayWalletState, generation: u64) -> bool {
        (!matches!(&self.kind, GatewayUiEventKind::Network { request } if !wallet.network_view.as_ref().is_some_and(|view| request.is_current(&view.context_revision))))
            && self.generation == generation
            && self.wallet.same_authority(wallet)
            && (!matches!(self.kind, GatewayUiEventKind::PrivateView { .. })
                || self.wallet.wallet_selection_generation == wallet.wallet_selection_generation)
            && self.control.ensure_current().is_ok()
    }
}
