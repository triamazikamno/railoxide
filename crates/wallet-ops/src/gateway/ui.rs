//! Ephemeral native UI requests. Only authenticated transport frames create these events.
use super::GatewayWalletState;
use crate::dapp_request::DappRequestControl;
use serde::Serialize;

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GatewayPendingRequest {
    pub request_id: String,
    pub url: String,
    pub needs_unlock: bool,
    pub summary: Option<String>,
}

#[derive(Clone, Copy)]
pub enum GatewayUiEventKind {
    SummonDesktop,
    UserActivity,
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
        self.generation == generation
            && self.wallet.same_authority(wallet)
            && self.control.ensure_current().is_ok()
    }
}
