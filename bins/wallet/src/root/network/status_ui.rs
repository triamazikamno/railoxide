use std::sync::Arc;

use gpui::{App, FocusHandle};
use wallet_ops::gateway::GatewayNetworkError;
use wallet_ops::{TorBridgeActivitySnapshot, WalletNetworkHealth};

use super::{TorExitIpQueryState, TorRotation};

pub(in crate::root) struct NetworkStatusUi {
    pub(in crate::root) network_health: WalletNetworkHealth,
    pub(in crate::root) tor_bridge_activity: Option<TorBridgeActivitySnapshot>,
    pub(in crate::root) tor_download_rate: Option<u64>,
    pub(in crate::root) popover_open: bool,
    pub(in crate::root) focus: FocusHandle,
    pub(in crate::root) popover_focus: FocusHandle,
    pub(in crate::root) error: Option<Arc<str>>,
    pub(in crate::root) tor_exit_ip_query: TorExitIpQueryState,
    pub(in crate::root) tor_exit_ip_query_generation: u64,
    pub(in crate::root) tor_state_reset_confirming: bool,
    /// The one pending explicit Tor rotation, which later activations join.
    pub(in crate::root) tor_rotation: Option<TorRotation>,
    pub(in crate::root) tor_rotation_id: u64,
    /// The last rotation's failure, shown until the next rotation, lock or root retirement.
    pub(in crate::root) tor_rotation_failure: Option<GatewayNetworkError>,
}

impl NetworkStatusUi {
    pub(in crate::root) fn new(
        network_health: WalletNetworkHealth,
        tor_bridge_activity: Option<TorBridgeActivitySnapshot>,
        cx: &App,
    ) -> Self {
        Self {
            network_health,
            tor_bridge_activity,
            tor_download_rate: None,
            popover_open: false,
            focus: cx.focus_handle(),
            popover_focus: cx.focus_handle(),
            error: None,
            tor_exit_ip_query: TorExitIpQueryState::Idle,
            tor_exit_ip_query_generation: 0,
            tor_state_reset_confirming: false,
            tor_rotation: None,
            tor_rotation_id: 0,
            tor_rotation_failure: None,
        }
    }
}
