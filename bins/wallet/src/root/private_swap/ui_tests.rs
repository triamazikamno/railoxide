use super::super::*;
use super::*;
use crate::root::chain_load::{ChainSyncProgress, ChainUtxoState, WalletSyncLifecycle};
use alloy::primitives::B256;
use broadcaster_core::contracts::swap_math::{SWAP_MATH_ADDRESS, SWAP_MATH_CREATION_CODE};
use gpui::{IntoElement, ParentElement, Render, Styled, TestAppContext, div};
use gpui_component::{Root, WindowExt};
use std::cell::Cell;
use std::rc::Rc;
use ui::dialog::WalletDialogExt as _;
use wallet_ops::vault::ExecutorStore;

/// Mount the root as an entity, as startup does. Calling its render helper through
/// `read` would miss attempts to read the root while GPUI holds its update lease.
struct WalletWindow(Entity<WalletRoot>);

impl Render for WalletWindow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

// The address of `setup_broadcaster`.
use crate::root::private_swap::setup_progress::STAND_IN_SETUP_BROADCASTER as SETUP_BROADCASTER;

#[path = "ui_tests/form_drivers.rs"]
mod form_drivers;
#[path = "ui_tests/placed_swaps.rs"]
mod placed_swaps;
#[path = "ui_tests/session_helpers.rs"]
mod session_helpers;
#[path = "ui_tests/swap_stubs.rs"]
mod swap_stubs;
#[path = "ui_tests/swap_views.rs"]
mod swap_views;

#[allow(clippy::wildcard_imports)]
use form_drivers::*;
#[allow(clippy::wildcard_imports)]
use placed_swaps::*;
#[allow(clippy::wildcard_imports)]
use session_helpers::*;
#[allow(clippy::wildcard_imports)]
use swap_stubs::*;
#[allow(clippy::wildcard_imports)]
use swap_views::*;

#[path = "ui_tests/bridge_restarts.rs"]
mod bridge_restarts;
#[path = "ui_tests/bridge_reviews.rs"]
mod bridge_reviews;
#[path = "ui_tests/bridge_routes.rs"]
mod bridge_routes;
#[path = "ui_tests/bridge_setup_progress.rs"]
mod bridge_setup_progress;
#[path = "ui_tests/costs.rs"]
mod costs;
#[path = "ui_tests/fills.rs"]
mod fills;
#[path = "ui_tests/manual_checks.rs"]
mod manual_checks;
#[path = "ui_tests/observation_routes.rs"]
mod observation_routes;
#[path = "ui_tests/orders.rs"]
mod orders;
#[path = "ui_tests/pickers.rs"]
mod pickers;
#[path = "ui_tests/private_bridge_accounts.rs"]
mod private_bridge_accounts;
#[path = "ui_tests/public_orders.rs"]
mod public_orders;
#[path = "ui_tests/public_preparation.rs"]
mod public_preparation;
#[path = "ui_tests/public_review.rs"]
mod public_review;
#[path = "ui_tests/public_sources.rs"]
mod public_sources;
#[path = "ui_tests/quotes.rs"]
mod quotes;
#[path = "ui_tests/receivers.rs"]
mod receivers;
#[path = "ui_tests/refunds_and_deliveries.rs"]
mod refunds_and_deliveries;
#[path = "ui_tests/setup_inclusion.rs"]
mod setup_inclusion;
#[path = "ui_tests/setup_retries.rs"]
mod setup_retries;
#[path = "ui_tests/setup_retry_reviews.rs"]
mod setup_retry_reviews;
#[path = "ui_tests/speed_up.rs"]
mod speed_up;
#[path = "ui_tests/speed_up_form.rs"]
mod speed_up_form;
#[path = "ui_tests/swap_detail.rs"]
mod swap_detail;
