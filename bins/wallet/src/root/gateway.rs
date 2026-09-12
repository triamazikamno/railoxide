use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::Address;
use gpui::{
    Anchor, App, Context, DismissEvent, Entity, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Rems, SharedString, StatefulInteractiveElement, Styled, Window, div, img,
    prelude::FluentBuilder as _, px, rems, rgb,
};
use gpui_component::{
    ActiveTheme as _, Disableable, Icon, IconName, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    collapsible::Collapsible,
    input::OtpState,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    notification::Notification,
    popover::{Popover, PopoverState},
    scroll::ScrollableElement,
    separator::Separator,
    switch::Switch,
    tooltip::Tooltip,
};
use railgun_ui::{chain_icon_asset_path, chain_name, short_address};
use tokio::sync::watch;
use ui::clipboard::copy_to_clipboard_with_toast;
use ui::controls::{app_button_base, app_button_label, app_muted_text, app_strong_text, app_text};
use ui::icons;
use ui::theme::{self, APP_TEXT_SIZE};
use wallet_ops::gateway::{
    GatewayConfig, GatewayError, GatewayHandle, GatewayPairingOffer, GatewaySnapshot,
    GatewayUnlockState, GatewayWalletState, PeerId,
};

use crate::assets::RailgunSidebarIcon;

use super::sidebar::{SIDEBAR_FOOTER_HORIZONTAL_INSET, SIDEBAR_STATUS_PILL_HEIGHT};
use super::{SIDEBAR_WIDTH, VaultState, WalletRoot, app_status_tag, rgb_with_alpha};
#[path = "gateway_handoff.rs"]
mod handoff;
use handoff::{GatewaySwitchContinuation, GatewaySwitchDialog};
#[path = "gateway_listener.rs"]
mod listener;
use listener::GatewayListenerDialog;
#[path = "gateway_pairing.rs"]
mod pairing;
use pairing::GatewayPairingDialog;

#[derive(Default)]
struct GatewayUnlock {
    state: GatewayUnlockState,
    continuation: Option<u64>,
    installing: bool,
}

impl GatewayUnlock {
    const fn retire(&mut self) {
        self.state = GatewayUnlockState {
            cohort: self.state.cohort.wrapping_add(1),
            completed: false,
        };
        self.continuation = None;
        self.installing = false;
    }

    fn begin(&mut self, has_view: bool) -> Option<u64> {
        if has_view || self.continuation.is_some() || self.state.completed {
            self.retire();
        }
        self.continuation = (!has_view).then_some(self.state.cohort);
        self.continuation
    }

    fn is_current(&self, continuation: Option<u64>) -> bool {
        continuation.is_some_and(|cohort| {
            self.continuation == Some(cohort)
                && self.state
                    == (GatewayUnlockState {
                        cohort,
                        completed: false,
                    })
        })
    }

    fn complete(&mut self, continuation: Option<u64>, has_view: bool) {
        if has_view && self.is_current(continuation) {
            self.state.completed = true;
            self.continuation = None;
            self.installing = false;
        }
    }
}

type GatewayDesktopState = (GatewayWalletState, u64, Vec<(String, String)>);

async fn publish_gateway_desktop_states(
    handle: GatewayHandle,
    mut states: watch::Receiver<GatewayDesktopState>,
) {
    while states.changed().await.is_ok() {
        let (state, generation, summaries) = states.borrow_and_update().clone();
        if let Err(error) = handle.publish_wallet_state(state.clone(), generation).await {
            tracing::warn!(%error, generation, "gateway desktop state publication rejected");
            continue;
        }
        handle.publish_summaries(state, generation, summaries).await;
    }
}

pub(super) struct GatewayUi {
    pub(super) private_selection_message: Option<&'static str>,
    pub(super) private_hardware_selection_pending: bool,
    pub(super) drafts: RefCell<super::gateway_drafts::GatewayDraftBook>,
    pub(super) draft_watch: Option<gpui::Task<()>>,
    unlock: RefCell<GatewayUnlock>,
    wallet_switch: RefCell<Option<GatewaySwitchContinuation>>,
    switch_dialog: Option<GatewaySwitchDialog>,
    listener_dialog: Option<GatewayListenerDialog>,
    request_network: RefCell<Option<GatewayWalletState>>,
    handle: Option<GatewayHandle>,
    runtime: tokio::runtime::Handle,
    snapshot: GatewaySnapshot,
    desktop_state: Option<watch::Sender<GatewayDesktopState>>,
    pairing_dialog: Option<GatewayPairingDialog>,
    pending_offer: Option<(Entity<OtpState>, Instant)>,
    offer_expiry_task: Option<gpui::Task<()>>,
    approval_task: Option<gpui::Task<()>>,
    operation_generation: u64,
    busy: bool,
    popover_open: bool,
    /// A row context menu is open; keeps the popover from treating clicks
    /// on the menu (which extends past the popover bounds) as outside clicks.
    peer_menu_open: bool,
    paired_browsers_open: bool,
    dapp_permissions_open: bool,
    /// Vault lookups for the accounts referenced by `snapshot.permissions`,
    /// keyed by public account uuid, so rendering never touches the vault.
    permission_accounts: HashMap<String, GatewayPermissionAccount>,
    error: Option<GatewayError>,
}

struct GatewayPermissionAccount {
    label: Option<String>,
    address: Address,
}

impl GatewayUi {
    pub(super) fn wallet_switch_in_progress(&self) -> bool {
        self.wallet_switch.borrow().is_some() || self.switch_dialog.is_some()
    }

    fn retire_unlock(&self) {
        self.drafts.borrow_mut().retire();
        self.unlock.borrow_mut().retire();
    }

    pub(super) fn start(
        store: &wallet_ops::vault::DesktopVaultStore,
        runtime: &tokio::runtime::Handle,
        window: &Window,
        cx: &Context<'_, WalletRoot>,
    ) -> Self {
        let handle = {
            let _entered = runtime.enter();
            GatewayHandle::start(store.db(), true, 0)
        };
        let mut switch_requests = handle.wallet_switch_requests();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let requests = switch_requests.borrow_and_update().clone();
                if this
                    .update_in(cx, |root, window, cx| {
                        root.reconcile_gateway_wallet_switches(&requests, window, cx);
                    })
                    .is_err()
                {
                    break;
                }
                tokio::select! {
                    changed = switch_requests.changed() => if changed.is_err() { break; },
                    () = cx.background_executor().timer(Duration::from_millis(250)) => {},
                }
            }
        })
        .detach();
        let mut ui_events = handle.ui_events();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let event = match ui_events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                if this
                    .update_in(cx, |root, window, cx| {
                        let current = root.gateway.desktop_state.as_ref().is_some_and(|state| {
                            let (wallet, generation, _) = &*state.borrow();
                            event.is_current(wallet, *generation)
                        });
                        if !current || root.gateway.handle.is_none() {
                            return;
                        }
                        match event.kind {
                            wallet_ops::gateway::GatewayUiEventKind::PrivateView { command } => {
                                root.apply_gateway_private_command(command, window, cx);
                            }
                            wallet_ops::gateway::GatewayUiEventKind::PublicView {
                                peer_id,
                                command,
                            } => {
                                root.apply_gateway_public_command(&peer_id, command, window, cx);
                            }
                            wallet_ops::gateway::GatewayUiEventKind::SummonDesktop => {
                                window.activate_window();
                            }
                            wallet_ops::gateway::GatewayUiEventKind::UserActivity => {
                                root.handle_wallet_activity(
                                    super::auto_lock::WalletActivitySource::Extension,
                                    window,
                                    cx,
                                );
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let mut snapshots = handle.snapshots();
        let snapshot = snapshots.borrow().clone();
        let (desktop_state, state_rx) =
            watch::channel((GatewayWalletState::default(), 0_u64, Vec::new()));
        runtime.spawn(publish_gateway_desktop_states(handle.clone(), state_rx));
        cx.spawn_in(window, async move |this, cx| {
            let mut previous_remote = false;
            loop {
                let snapshot = snapshots.borrow_and_update().clone();
                if this
                    .update_in(cx, |root, window, cx| {
                        if root.gateway.handle.is_none() {
                            return;
                        }
                        let remote = snapshot
                            .listener_addr
                            .is_some_and(|address| !address.ip().is_loopback());
                        if remote && !previous_remote {
                            window.push_notification(
                                Notification::info("Browser gateway network access enabled."),
                                cx,
                            );
                        }
                        previous_remote = remote;
                        root.reconcile_gateway_pairing_dialog(&snapshot, window, cx);
                        root.gateway.snapshot = snapshot;
                        root.gateway
                            .drafts
                            .borrow_mut()
                            .retain_peers(&root.gateway.snapshot.peers);
                        root.refresh_gateway_permission_accounts();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                if snapshots.changed().await.is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut approvals = handle.approval_requests();
        let approval_handle = handle.clone();
        let approval_task = cx.spawn(async move |this, cx| {
            loop {
                let ready = approvals.borrow_and_update().clone();
                if this
                    .update(cx, |root, cx| {
                        if root.gateway.handle.is_some() {
                            root.reconcile_gateway_approvals(&approval_handle, &ready, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
                if approvals.changed().await.is_err() {
                    break;
                }
            }
        });
        Self {
            private_selection_message: None,
            private_hardware_selection_pending: false,
            drafts: RefCell::default(),
            draft_watch: None,
            unlock: RefCell::new(GatewayUnlock::default()),
            wallet_switch: RefCell::new(None),
            switch_dialog: None,
            listener_dialog: None,
            request_network: RefCell::new(None),
            handle: Some(handle),
            runtime: runtime.clone(),
            snapshot,
            desktop_state: Some(desktop_state),
            pairing_dialog: None,
            pending_offer: None,
            offer_expiry_task: None,
            approval_task: Some(approval_task),
            operation_generation: 0,
            busy: false,
            popover_open: false,
            peer_menu_open: false,
            paired_browsers_open: true,
            dapp_permissions_open: false,
            permission_accounts: HashMap::new(),
            error: None,
        }
    }

    pub(super) fn take_shutdown_handle(&mut self) -> Option<GatewayHandle> {
        self.drafts.borrow_mut().retire();
        self.draft_watch = None;
        self.pairing_dialog = None;
        self.desktop_state = None;
        self.approval_task = None;
        if let Some(handle) = &self.handle {
            handle.set_wallet_authority(GatewayWalletState::default());
        }
        self.pending_offer = None;
        self.offer_expiry_task = None;
        self.operation_generation = self.operation_generation.wrapping_add(1);
        self.busy = false;
        self.handle.take()
    }
}

impl Drop for GatewayUi {
    fn drop(&mut self) {
        if let Some(handle) = self.take_shutdown_handle() {
            self.runtime.spawn(async move {
                let _ = handle.shutdown().await;
            });
        }
    }
}

impl WalletRoot {
    pub(super) fn gateway_has_connected_browser(&self) -> bool {
        self.gateway
            .snapshot
            .peers
            .iter()
            .any(|peer| peer.connected_sessions > 0)
    }

    pub(super) fn publish_gateway_summaries(&self, summaries: Vec<(String, String)>) {
        if let Some(state) = &self.gateway.desktop_state {
            state.send_if_modified(|(_, _, current)| {
                if *current == summaries {
                    return false;
                }
                *current = summaries;
                true
            });
        }
    }

    pub(super) fn retire_gateway_unlock(&self) {
        self.gateway.retire_unlock();
        self.publish_gateway_desktop_state();
    }

    pub(super) fn begin_gateway_unlock(&self) -> Option<u64> {
        let continuation = self
            .gateway
            .unlock
            .borrow_mut()
            .begin(self.view_session.is_some());
        self.publish_gateway_desktop_state();
        continuation
    }

    pub(super) fn gateway_unlock_continuation(&self) -> Option<u64> {
        self.gateway.unlock.borrow().continuation
    }

    pub(super) fn gateway_unlock_is_current(&self, continuation: Option<u64>) -> bool {
        self.gateway.unlock.borrow().is_current(continuation)
    }

    pub(super) fn transfer_gateway_unlock_to_installation(&self, continuation: Option<u64>) {
        let mut unlock = self.gateway.unlock.borrow_mut();
        unlock.installing = unlock.is_current(continuation);
    }

    #[cfg(feature = "hardware")]
    pub(super) fn abandon_gateway_unlock_dialog(&self, continuation: Option<u64>) {
        let should_retire = {
            let unlock = self.gateway.unlock.borrow();
            unlock.is_current(continuation) && !unlock.installing
        };
        if should_retire {
            self.retire_gateway_unlock();
        }
    }

    pub(super) fn complete_gateway_unlock(&self, continuation: Option<u64>) {
        self.gateway
            .unlock
            .borrow_mut()
            .complete(continuation, self.view_session.is_some());
    }

    pub(super) fn current_public_balance_scope(
        &self,
        chain_id: u64,
    ) -> Option<wallet_ops::PublicBalanceScope> {
        if self.public_transaction_cleanup.is_some()
            || self.public_sync_cache_resetting
            || self.merkle_forest_cache_resetting
            || self.manage_wallets.deleting_wallet_id.is_some()
            || !matches!(self.vault_state, VaultState::ViewUnlocked)
        {
            return None;
        }
        let view = self.view_session.as_ref()?;
        let config = self.effective_chain_configs.get(&chain_id)?;
        let route =
            wallet_ops::settings::resolve_effective_chain_rpc_route(chain_id, Some(config)).ok()?;
        Some(wallet_ops::PublicBalanceScope::new(
            view.wallet_id().to_owned(),
            self.active_wallet_generation,
            self.http.rpc_broker(),
            route,
        ))
    }

    pub(super) fn publish_gateway_desktop_state(&self) {
        self.publish_gateway_private_progress();
        if self.view_session.is_none() {
            self.private_asset_presentation_cache.borrow_mut().clear();
        }
        self.gateway
            .drafts
            .borrow_mut()
            .reconcile_wallet(self.view_session.as_ref(), self.active_wallet_generation);
        // Keep effective networking visible to cohort retirement even while the vault is locked.
        let mut network = GatewayWalletState::default();
        network.set_rpc_context(self.http.clone(), &self.effective_chain_configs);
        let mut previous_network = self.gateway.request_network.borrow_mut();
        if previous_network
            .as_ref()
            .is_some_and(|previous| !previous.same_authority(&network))
        {
            self.gateway.retire_unlock();
        }
        *previous_network = Some(network);
        drop(previous_network);
        self.public_balance_cache.reconcile_scopes(
            self.effective_chain_configs
                .keys()
                .filter_map(|&chain_id| self.current_public_balance_scope(chain_id))
                .collect(),
        );
        for scope in self
            .effective_chain_configs
            .keys()
            .filter_map(|&chain_id| self.current_public_balance_scope(chain_id))
        {
            self.public_transaction_tracker.reconcile_scope(
                &scope,
                &self.public_accounts,
                &self.public_balance_cache,
            );
        }
        if let Some(state) = self.gateway.desktop_state.as_ref() {
            // Hardware startup can unlock metadata before installing a usable wallet view.
            let unlocked =
                matches!(self.vault_state, VaultState::ViewUnlocked) && self.view_session.is_some();
            let mut snapshot = GatewayWalletState {
                private_view_supported: true,
                private_actions_supported: unlocked,
                private_view: unlocked.then(|| self.gateway_private_view()),
                wallet_selection_generation: self.wallet_switch_generation,
                wallet_transition: matches!(self.vault_state, VaultState::SwitchingWallet),
                public_view: if unlocked {
                    self.gateway_public_view()
                } else {
                    wallet_ops::gateway::GatewayPublicView::default()
                },
                waiting_unlock: self.gateway.unlock.borrow().state,
                view: if unlocked {
                    self.view_session.clone()
                } else {
                    None
                },
                active_wallet_generation: self.active_wallet_generation,
                public_balance_cache: self.public_balance_cache.clone(),
                public_transaction_tracker: self.public_transaction_tracker.clone(),
                public_accounts: if unlocked {
                    self.public_accounts.clone()
                } else {
                    Vec::new()
                },
                chain_ids: if unlocked {
                    self.effective_chain_configs.keys().copied().collect()
                } else {
                    Vec::new()
                },
                default_chain_id: unlocked.then_some(self.selected_chain),
                ..GatewayWalletState::default()
            };
            if unlocked {
                snapshot.token_registry = Some(Arc::new(self.effective_token_registry.clone()));
                snapshot.set_rpc_context(self.http.clone(), &self.effective_chain_configs);
            }
            snapshot.wallet_switch = self.gateway_wallet_switch_transition(&snapshot);
            if let Some(handle) = &self.gateway.handle {
                handle.set_wallet_authority(snapshot.clone());
            }
            // Advance at publication, so coalescing lock/unlock notifications cannot reuse an epoch.
            let summaries = self.gateway_request_summaries();
            state.send_if_modified(|(previous, generation, previous_summaries)| {
                if previous.same_state(&snapshot) && *previous_summaries == summaries {
                    return false;
                }
                if !previous.same_authority(&snapshot) {
                    *generation = generation.wrapping_add(1);
                }
                *previous = snapshot;
                *previous_summaries = summaries;
                true
            });
        }
    }

    /// Resolve the accounts behind the current permissions once per snapshot,
    /// so permission rows can show labels and addresses without vault lookups
    /// during render. Permissions can reference accounts of another wallet,
    /// which are absent from `public_accounts`.
    fn refresh_gateway_permission_accounts(&mut self) {
        let accounts = self
            .view_session
            .as_ref()
            .zip(self.vault_store.as_ref())
            .filter(|_| !self.gateway.snapshot.permissions.is_empty())
            .and_then(|(view_session, store)| store.list_all_public_accounts(view_session).ok());
        let Some(accounts) = accounts else {
            self.gateway.permission_accounts.clear();
            return;
        };
        self.gateway.permission_accounts = self
            .gateway
            .snapshot
            .permissions
            .iter()
            .filter_map(|permission| {
                let account = accounts.iter().find(|account| {
                    account.public_account_uuid == permission.public_account_uuid
                })?;
                Some((
                    permission.public_account_uuid.clone(),
                    GatewayPermissionAccount {
                        label: account.label.clone(),
                        address: account.address,
                    },
                ))
            })
            .collect();
    }

    pub(super) fn render_gateway_status_pill(
        &self,
        root: &Entity<Self>,
        collapsed: bool,
    ) -> impl IntoElement {
        let popover_root = root.clone();
        let content_root = root.clone();
        let width =
            SIDEBAR_WIDTH - SIDEBAR_FOOTER_HORIZONTAL_INSET - SIDEBAR_FOOTER_HORIZONTAL_INSET;
        let total_pairings = self.gateway.snapshot.peers.len();
        let active_sessions: usize = self
            .gateway
            .snapshot
            .peers
            .iter()
            .map(|peer| peer.connected_sessions)
            .sum();
        let status = div()
            .flex_none()
            .flex()
            .items_center()
            .whitespace_nowrap()
            .text_size(px(11.0))
            .line_height(px(16.0))
            .text_color(rgb(theme::TEXT_MUTED));
        let status = if matches!(
            self.gateway.snapshot.error,
            Some(GatewayError::PortInUse(_))
        ) {
            status.text_color(rgb(theme::DANGER)).child("Port in use")
        } else if total_pairings > 1 {
            status
                .child(
                    div()
                        .text_color(rgb(theme::SUCCESS))
                        .child(active_sessions.to_string()),
                )
                .child(format!("/{total_pairings}"))
        } else if active_sessions > 0 {
            status.text_color(rgb(theme::SUCCESS)).child("active")
        } else {
            status.child("inactive")
        };
        let trigger = Button::new("wallet-gateway-status-pill-trigger")
            .accessibility_label("Browser pairing")
            .text()
            .when(collapsed, |this| this.tooltip("Browser pairing"))
            .when(!collapsed, |this| {
                this.w(width).min_w(px(0.0)).flex_shrink(1.0)
            })
            .child(
                div()
                    .id("wallet-gateway-status-pill")
                    .w_full()
                    .min_w(px(0.0))
                    .h(if collapsed {
                        px(32.0)
                    } else {
                        SIDEBAR_STATUS_PILL_HEIGHT
                    })
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb_with_alpha(theme::TEXT_MUTED, 0.08))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .hover(|this| this.bg(rgb_with_alpha(theme::TEXT_MUTED, 0.14)))
                    .line_height(px(16.0))
                    .when(collapsed, Styled::justify_center)
                    .child(
                        Icon::new(RailgunSidebarIcon::BrowserExtensions)
                            .small()
                            .flex_none(),
                    )
                    .when(!collapsed, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(13.0))
                                .line_height(px(16.0))
                                .child("Browser pairing"),
                        )
                        .child(status)
                    }),
            );
        let popover = Popover::new("wallet-gateway-status-popover")
            .anchor(Anchor::BottomLeft)
            .open(self.gateway.popover_open)
            .overlay_closable(!self.gateway.peer_menu_open)
            .on_open_change(move |open, _window, cx| {
                popover_root.update(cx, |root, cx| {
                    root.gateway.popover_open = *open;
                    root.gateway.peer_menu_open = false;
                    cx.notify();
                });
            })
            .trigger(trigger)
            .content(move |_state, window, cx| {
                let viewport = window.viewport_size();
                let width = (viewport.width - px(56.0)).max(px(0.0)).min(px(300.0));
                let max_height = (viewport.height * 0.75 - px(56.0)).max(px(0.0));
                let popover = cx.entity();
                div()
                    .w(width)
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .text_size(APP_TEXT_SIZE)
                    .text_color(rgb(theme::TEXT))
                    .child(app_strong_text("Browser pairing"))
                    .child(content_root.update(cx, |root, cx| {
                        root.render_gateway_controls(&content_root, &popover, max_height, cx)
                            .into_any_element()
                    }))
            });
        div()
            .when(!collapsed, |this| this.w(width).min_w(px(0.0)))
            .child(popover)
    }

    fn render_gateway_controls(
        &self,
        root: &Entity<Self>,
        popover: &Entity<PopoverState>,
        max_height: gpui::Pixels,
        cx: &App,
    ) -> impl IntoElement {
        let config = self.gateway.snapshot.config;
        let disabled = self.gateway.busy || self.gateway.handle.is_none();
        let enable_root = root.clone();
        let edit_root = root.clone();
        let edit_popover = popover.clone();
        let error = match self.gateway.snapshot.error {
            Some(error @ GatewayError::PortInUse(_)) => Some(error),
            error => self.gateway.error.or(error),
        };
        let mut content = div().min_w_0().flex().flex_col().gap_3()
            .child(app_muted_text("Connect dapps in your browser to this wallet through the extension. Signing and spending always happen on this desktop app.")
                .text_size(px(12.0)).line_height(px(18.0)))
            .when_some(error, |this, error| {
                this.child(app_muted_text(error.to_string())
                    .text_size(px(12.0)).line_height(px(18.0)).text_color(cx.theme().danger))
            })
            .child(Switch::new("gateway-enabled").small().label("Enable browser gateway")
                .checked(config.enabled).disabled(disabled)
                .on_click(move |enabled, window, cx| {
                    enable_root.update(cx, |root, cx| {
                        root.configure_gateway(GatewayConfig { enabled: *enabled, ..root.gateway.snapshot.config }, window, cx);
                    });
                }));
        if !config.enabled {
            return div()
                .max_h(max_height)
                .flex()
                .flex_col()
                .child(div().overflow_y_scrollbar().child(content));
        }
        content = content.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div().flex_1().min_w_0().child(
                        app_muted_text(self.gateway.snapshot.listener_addr.map_or_else(
                            || "Listener stopped".to_string(),
                            |address| format!("Listening on {address}"),
                        ))
                        .text_size(px(12.0))
                        .line_height(px(17.0)),
                    ),
                )
                .child(
                    Button::new("gateway-listener-edit")
                        .ghost()
                        .small()
                        .icon(IconName::Settings)
                        .accessibility_label("Edit listener address and port")
                        .tooltip("Edit listener address and port…")
                        .disabled(disabled)
                        .on_click(move |_, window, cx| {
                            edit_popover.update(cx, |state, cx| state.dismiss(window, cx));
                            edit_root.update(cx, |root, cx| {
                                root.open_gateway_listener_dialog(window, cx);
                            });
                        }),
                ),
        );
        content = content.child(self.render_gateway_paired_browsers(root, popover, disabled));
        if !self.gateway.snapshot.locked {
            content = content.child(self.render_gateway_dapp_permissions(root, disabled));
        }
        div()
            .max_h(max_height)
            .flex()
            .flex_col()
            .child(div().overflow_y_scrollbar().child(content))
    }

    fn render_gateway_paired_browsers(
        &self,
        root: &Entity<Self>,
        popover: &Entity<PopoverState>,
        disabled: bool,
    ) -> impl IntoElement {
        let pair_root = root.clone();
        let pair_popover = popover.clone();
        let mut peers_content = gateway_section_content().gap_0();
        if self.gateway.snapshot.peers.is_empty() {
            peers_content = peers_content.child(
                app_muted_text("No paired browsers")
                    .text_size(px(12.0))
                    .line_height(px(17.0)),
            );
        }
        let now = super::utxo::now_epoch_secs();
        let peer_count = self.gateway.snapshot.peers.len();
        for (index, peer) in self.gateway.snapshot.peers.iter().enumerate() {
            let identity = alloy::hex::encode(peer.id.to_bytes());
            let identity_text = SharedString::from(identity.clone());
            let label = peer.label.clone().map(SharedString::from);
            let is_hex_identity = label.is_none();
            let display_name =
                label.unwrap_or_else(|| SharedString::from(short_identity(&identity)));
            let tooltip = identity_text.clone();
            let (status, color) = if peer.connected_sessions > 0 {
                ("Active", theme::SUCCESS)
            } else {
                ("Inactive", theme::TEXT_MUTED)
            };
            let last_active = gateway_peer_age(peer.last_active_at, now);
            let paired = gateway_peer_age(peer.paired_at, now);
            let menu_target = GatewayPeerMenuTarget {
                root: root.clone(),
                peer_id: peer.id,
                identity: identity_text,
                disabled,
            };
            let revoke_root = root.clone();
            let peer_id = peer.id;
            peers_content = peers_content.child(
                div()
                    .id(SharedString::from(format!("gateway-peer-row-{identity}")))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .min_w_0()
                    .py_1p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                app_text(display_name)
                                    .id(SharedString::from(format!(
                                        "gateway-peer-label-{identity}"
                                    )))
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .line_height(px(17.0))
                                    .when(is_hex_identity, |this| {
                                        this.font_family(theme::APP_MONO_FONT_FAMILY)
                                    })
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(tooltip.clone()).build(window, cx)
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .child(app_status_tag(status, color)),
                            )
                            .child(
                                Button::new(SharedString::from(format!(
                                    "gateway-peer-revoke-{identity}"
                                )))
                                .ghost()
                                .small()
                                .flex_none()
                                .icon(Icon::empty().path(icons::ban_icon_path()))
                                .accessibility_label("Revoke pairing")
                                .tooltip("Revoke pairing")
                                .disabled(disabled)
                                .on_click(
                                    move |_, window, cx| {
                                        revoke_gateway_peer(&revoke_root, peer_id, window, cx);
                                    },
                                ),
                            ),
                    )
                    .child(
                        app_muted_text(format!("Last active {last_active} · paired {paired}"))
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .line_height(px(17.0)),
                    )
                    .context_menu(move |menu, _window, cx| {
                        gateway_peer_menu(menu, &menu_target, cx)
                    }),
            );
            if index + 1 < peer_count {
                peers_content = peers_content.child(Separator::horizontal());
            }
        }
        let peers_toggle_root = root.clone();
        Collapsible::new()
            .open(self.gateway.paired_browsers_open)
            .w_full()
            .min_w_0()
            .gap_3()
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button_base("gateway-paired-browsers-toggle")
                            .text()
                            .small()
                            .compact()
                            .flex_1()
                            .min_w_0()
                            .accessibility_label("Paired browsers")
                            .icon(if self.gateway.paired_browsers_open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        app_button_label("Paired browsers")
                                            .text_color(rgb(theme::TEXT))
                                            .font_weight(FontWeight::SEMIBOLD),
                                    )
                                    .when(peer_count > 0, |this| {
                                        this.child(
                                            app_button_label(format!("({peer_count})"))
                                                .text_color(rgb(theme::TEXT_MUTED)),
                                        )
                                    }),
                            )
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                peers_toggle_root.update(cx, |root, cx| {
                                    root.gateway.paired_browsers_open =
                                        !root.gateway.paired_browsers_open;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("gateway-pair")
                            .ghost()
                            .small()
                            .flex_none()
                            .icon(IconName::Plus)
                            .accessibility_label("Create new pairing")
                            .tooltip("Create new pairing")
                            .disabled(disabled || self.gateway.snapshot.listener_addr.is_none())
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                pair_popover.update(cx, |state, cx| state.dismiss(window, cx));
                                pair_root.update(cx, |root, cx| {
                                    root.open_gateway_pairing_dialog(window, cx);
                                });
                            }),
                    ),
            )
            .content(peers_content)
    }

    fn render_gateway_dapp_permissions(
        &self,
        root: &Entity<Self>,
        disabled: bool,
    ) -> impl IntoElement {
        let mut permissions_content = gateway_section_content().gap_0();
        let permission_count = self.gateway.snapshot.permissions.len();
        for (index, permission) in self.gateway.snapshot.permissions.iter().enumerate() {
            let account = self
                .gateway
                .permission_accounts
                .get(&permission.public_account_uuid);
            let account_address = account.map(|account| account.address);
            let permission_id = permission.permission_id.clone();
            let url = SharedString::from(permission.url.clone());
            let display_url = SharedString::from(
                permission
                    .url
                    .strip_prefix("https://")
                    .or_else(|| permission.url.strip_prefix("http://"))
                    .unwrap_or(permission.url.as_str())
                    .to_owned(),
            );
            let menu_target = GatewayPermissionMenuTarget {
                root: root.clone(),
                permission_id: permission_id.clone(),
                url: url.clone(),
                address: account_address.map(|address| SharedString::from(format!("{address:#x}"))),
                peer_identity: SharedString::from(permission.paired_peer_id.clone()),
                disabled,
            };
            let revoke_root = root.clone();
            let mut account_row = gateway_permission_detail_row().gap_1();
            if let Some(label) = account.and_then(|account| account.label.clone()) {
                account_row =
                    account_row.child(div().min_w_0().truncate().child(SharedString::from(label)));
            }
            account_row = account_row.child(
                div()
                    .flex_none()
                    .font_family(theme::APP_MONO_FONT_FAMILY)
                    .child(SharedString::from(account_address.map_or_else(
                        || short_identity(&permission.public_account_uuid),
                        |address| short_address(&address),
                    ))),
            );
            let chain_label = chain_name(permission.chain_id)
                .map_or_else(|| format!("Chain {}", permission.chain_id), str::to_owned);
            let mut chain_row = gateway_permission_detail_row().gap_1p5();
            if let Some(path) = chain_icon_asset_path(permission.chain_id) {
                chain_row = chain_row.child(img(path).size(px(12.0)).flex_none());
            }
            chain_row = chain_row
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(format!("{chain_label} · browser"))),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(theme::APP_MONO_FONT_FAMILY)
                        .child(SharedString::from(short_identity(
                            &permission.paired_peer_id,
                        ))),
                );
            permissions_content = permissions_content.child(
                div()
                    .id(SharedString::from(format!(
                        "gateway-permission-row-{permission_id}"
                    )))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .min_w_0()
                    .py_1p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                app_text(display_url)
                                    .id(SharedString::from(format!(
                                        "gateway-permission-url-{permission_id}"
                                    )))
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.0))
                                    .line_height(px(17.0))
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(url.clone()).build(window, cx)
                                    }),
                            )
                            .child(
                                Button::new(SharedString::from(format!(
                                    "gateway-permission-revoke-{permission_id}"
                                )))
                                .ghost()
                                .small()
                                .flex_none()
                                .icon(Icon::empty().path(icons::ban_icon_path()))
                                .accessibility_label("Revoke access")
                                .tooltip("Revoke access")
                                .disabled(disabled)
                                .on_click(
                                    move |_, window, cx| {
                                        revoke_gateway_permission(
                                            &revoke_root,
                                            permission_id.clone(),
                                            window,
                                            cx,
                                        );
                                    },
                                ),
                            ),
                    )
                    .child(account_row)
                    .child(chain_row)
                    .context_menu(move |menu, _window, cx| {
                        gateway_permission_menu(menu, &menu_target, cx)
                    }),
            );
            if index + 1 < permission_count {
                permissions_content = permissions_content.child(Separator::horizontal());
            }
        }
        let permissions_toggle_root = root.clone();
        Collapsible::new()
            .open(self.gateway.dapp_permissions_open)
            .w_full()
            .min_w_0()
            .gap_3()
            .child(
                app_button_base("gateway-dapp-permissions-toggle")
                    .text()
                    .small()
                    .compact()
                    .w_full()
                    .min_w_0()
                    .accessibility_label("Dapp permissions")
                    .icon(if self.gateway.dapp_permissions_open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                app_button_label("Dapp permissions")
                                    .text_color(rgb(theme::TEXT))
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .when(permission_count > 0, |this| {
                                this.child(
                                    app_button_label(format!("({permission_count})"))
                                        .text_color(rgb(theme::TEXT_MUTED)),
                                )
                            }),
                    )
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        permissions_toggle_root.update(cx, |root, cx| {
                            root.gateway.dapp_permissions_open =
                                !root.gateway.dapp_permissions_open;
                            cx.notify();
                        });
                    }),
            )
            .content(permissions_content)
    }

    fn configure_gateway(
        &mut self,
        config: GatewayConfig,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(handle) = self.gateway.handle.clone() else {
            return;
        };
        self.gateway.pending_offer = None;
        self.gateway.offer_expiry_task = None;
        self.run_gateway_operation(
            async move { handle.configure(config).await.map(|()| None) },
            window,
            cx,
        );
    }

    fn run_gateway_operation(
        &mut self,
        operation: impl Future<Output = Result<Option<GatewayPairingOffer>, GatewayError>>
        + Send
        + 'static,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.gateway.busy || self.gateway.handle.is_none() {
            return;
        }
        self.gateway.busy = true;
        self.gateway.error = None;
        self.gateway.operation_generation = self.gateway.operation_generation.wrapping_add(1);
        let generation = self.gateway.operation_generation;
        let started_at = Instant::now();
        let pairing_identity = self
            .gateway
            .pairing_dialog
            .as_ref()
            .map(|state| state.lease.clone());
        let task = self.runtime.spawn(operation);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.unwrap_or(Err(GatewayError::Unavailable));
            let _ = this.update_in(cx, |root, window, cx| {
                if root.gateway.handle.is_none() || root.gateway.operation_generation != generation
                {
                    return;
                }
                root.gateway.busy = false;
                if let Some(handle) = root.gateway.handle.clone() {
                    root.gateway.snapshot = handle.snapshots().borrow().clone();
                    root.refresh_gateway_permission_accounts();
                }
                match result {
                    Ok(Some(offer)) => {
                        let deadline =
                            started_at + Duration::from_secs(offer.expires_in_secs.min(120));
                        if let Some(identity) = pairing_identity.as_ref().filter(|identity| {
                            root.gateway.pairing_dialog.as_ref().is_some_and(|current| {
                                current.lease.ptr_eq(identity) && current.lease.upgrade().is_some()
                            }) && root.gateway.snapshot.pairing_active
                                && root.gateway.snapshot.config.enabled
                                && root.gateway.snapshot.listener_addr.is_some()
                                && deadline > Instant::now()
                        }) {
                            root.show_gateway_pairing_offer(offer, deadline, identity, window, cx);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => root.gateway.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

/// Leading inset for collapsible section bodies: the header toggle's small
/// chevron (`size_3p5`) plus its `gap_1`, so body text aligns with the title.
const GATEWAY_SECTION_INDENT: Rems = rems(0.875 + 0.25);

fn gateway_section_content() -> gpui::Div {
    div().min_w_0().pl(GATEWAY_SECTION_INDENT).flex().flex_col()
}

fn gateway_permission_detail_row() -> gpui::Div {
    div()
        .flex()
        .items_center()
        .min_w_0()
        .text_size(px(12.0))
        .line_height(px(17.0))
        .text_color(rgb(theme::TEXT_MUTED))
}

/// Shorten a hex identity or uuid for display: `9574bd…8e93`.
fn short_identity(hex: &str) -> String {
    if hex.len() <= 10 {
        return hex.to_owned();
    }
    format!("{}…{}", &hex[..6], &hex[hex.len() - 4..])
}

#[derive(Clone)]
struct GatewayPeerMenuTarget {
    root: Entity<WalletRoot>,
    peer_id: PeerId,
    identity: SharedString,
    disabled: bool,
}

#[derive(Clone)]
struct GatewayPermissionMenuTarget {
    root: Entity<WalletRoot>,
    permission_id: String,
    url: SharedString,
    address: Option<SharedString>,
    peer_identity: SharedString,
    disabled: bool,
}

fn gateway_peer_age(timestamp: u64, now: u64) -> String {
    ui::format::format_relative_age(Duration::from_secs(now.saturating_sub(timestamp)))
}

/// A row context menu is drawn outside the popover, so the popover must ignore
/// outside clicks while one is up. `ContextMenu` has no open-change hook: the
/// builder runs on every open, and the menu entity emits `DismissEvent` when it
/// closes, whether by an item click, a click outside or Escape.
fn gate_popover_for_menu(root: &Entity<WalletRoot>, cx: &mut Context<'_, PopupMenu>) {
    root.update(cx, |root, cx| {
        root.gateway.peer_menu_open = true;
        cx.notify();
    });
    let root = root.clone();
    cx.subscribe_self(move |_, _: &DismissEvent, cx| {
        root.update(cx, |root, cx| {
            root.gateway.peer_menu_open = false;
            cx.notify();
        });
    })
    .detach();
}

fn revoke_gateway_peer(root: &Entity<WalletRoot>, peer_id: PeerId, window: &Window, cx: &mut App) {
    root.update(cx, |root, cx| {
        let Some(handle) = root.gateway.handle.clone() else {
            return;
        };
        root.run_gateway_operation(
            async move { handle.revoke_peer(peer_id).await.map(|()| None) },
            window,
            cx,
        );
    });
}

fn revoke_gateway_permission(
    root: &Entity<WalletRoot>,
    permission_id: String,
    window: &Window,
    cx: &mut App,
) {
    root.update(cx, |root, cx| {
        let Some(handle) = root.gateway.handle.clone() else {
            return;
        };
        root.run_gateway_operation(
            async move { handle.revoke_permission(permission_id).await.map(|()| None) },
            window,
            cx,
        );
    });
}

fn gateway_peer_menu(
    menu: PopupMenu,
    target: &GatewayPeerMenuTarget,
    cx: &mut Context<'_, PopupMenu>,
) -> PopupMenu {
    gate_popover_for_menu(&target.root, cx);
    let copy_identity = target.identity.clone();
    let revoke_root = target.root.clone();
    let peer_id = target.peer_id;
    menu.min_w(px(180.0))
        .item(
            PopupMenuItem::new("Copy browser ID")
                .icon(IconName::Copy)
                .on_click(move |_event, window, cx| {
                    copy_to_clipboard_with_toast(copy_identity.clone(), window, cx);
                }),
        )
        .item(PopupMenuItem::separator())
        .item(
            PopupMenuItem::new("Revoke pairing")
                .icon(Icon::empty().path(icons::ban_icon_path()))
                .disabled(target.disabled)
                .on_click(move |_event, window, cx| {
                    revoke_gateway_peer(&revoke_root, peer_id, window, cx);
                }),
        )
}

fn gateway_permission_menu(
    menu: PopupMenu,
    target: &GatewayPermissionMenuTarget,
    cx: &mut Context<'_, PopupMenu>,
) -> PopupMenu {
    gate_popover_for_menu(&target.root, cx);
    let copy_url = target.url.clone();
    let copy_address = target.address.clone();
    let copy_identity = target.peer_identity.clone();
    let revoke_root = target.root.clone();
    let permission_id = target.permission_id.clone();
    menu.min_w(px(180.0))
        .item(
            PopupMenuItem::new("Copy URL")
                .icon(IconName::Copy)
                .on_click(move |_event, window, cx| {
                    copy_to_clipboard_with_toast(copy_url.clone(), window, cx);
                }),
        )
        .item(
            PopupMenuItem::new("Copy account address")
                .icon(IconName::Copy)
                .disabled(copy_address.is_none())
                .on_click(move |_event, window, cx| {
                    if let Some(address) = copy_address.clone() {
                        copy_to_clipboard_with_toast(address, window, cx);
                    }
                }),
        )
        .item(
            PopupMenuItem::new("Copy browser ID")
                .icon(IconName::Copy)
                .on_click(move |_event, window, cx| {
                    copy_to_clipboard_with_toast(copy_identity.clone(), window, cx);
                }),
        )
        .item(PopupMenuItem::separator())
        .item(
            PopupMenuItem::new("Revoke access")
                .icon(Icon::empty().path(icons::ban_icon_path()))
                .disabled(target.disabled)
                .on_click(move |_event, window, cx| {
                    revoke_gateway_permission(&revoke_root, permission_id.clone(), window, cx);
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_state_does_not_stop_later_wallet_publication() {
        use wallet_ops::vault::{DesktopVaultStore, KdfParams, WalletSource};

        const PASSWORD: &str = "gateway publisher test password";
        let path = std::env::temp_dir().join(format!(
            "railoxide-gateway-publisher-{:032x}",
            rand::random::<u128>()
        ));
        let store = DesktopVaultStore::open(path.clone()).unwrap();
        store
            .create_vault_with_params(PASSWORD, KdfParams::new(1024, 1, 1))
            .unwrap();
        let metadata = store
            .new_wallet_metadata(PASSWORD, "software", 0, WalletSource::Imported, "Software")
            .unwrap();
        store
            .import_wallet_mnemonic_with_metadata(
                PASSWORD,
                "software",
                0,
                "english",
                "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
                &metadata,
            )
            .unwrap();
        let view = Arc::new(store.load_view_session(PASSWORD, "software").unwrap());
        let handle = GatewayHandle::start(store.db(), true, 0);
        handle.configure(GatewayConfig::default()).await.unwrap();
        let mut snapshots = handle.snapshots();
        snapshots.borrow_and_update();
        let (states, receiver) = watch::channel((GatewayWalletState::default(), 0, Vec::new()));
        let mut publisher = Box::pin(publish_gateway_desktop_states(handle.clone(), receiver));

        // Reusing the actor's epoch for different authority rejects this publication.
        let stale = GatewayWalletState {
            active_wallet_generation: 1,
            ..GatewayWalletState::default()
        };
        handle.set_wallet_authority(stale.clone());
        states.send((stale, 0, Vec::new())).unwrap();
        assert!(futures_util::poll!(publisher.as_mut()).is_pending());
        // This command is queued after the state, so its reply confirms rejection was processed.
        handle.configure(GatewayConfig::default()).await.unwrap();
        let publisher = tokio::spawn(publisher);
        assert!(snapshots.borrow_and_update().locked);

        // A later successful selection must unlock the gateway without restarting it.
        let unlocked = GatewayWalletState {
            view: Some(view.clone()),
            active_wallet_generation: 2,
            ..GatewayWalletState::default()
        };
        handle.set_wallet_authority(unlocked.clone());
        states
            .send((unlocked, 1, Vec::new()))
            .expect("publisher must still accept desktop updates");
        tokio::time::timeout(
            Duration::from_secs(5),
            snapshots.wait_for(|snapshot| !snapshot.locked && snapshot.generation == 1),
        )
        .await
        .expect("later wallet state must reach the gateway")
        .unwrap();

        drop(states);
        publisher.await.unwrap();
        handle.shutdown().await.unwrap();
        drop(handle);
        drop(view);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn unlock_continuation_cannot_complete_after_retirement_or_relock() {
        let mut unlock = GatewayUnlock::default();
        let software = unlock.begin(false);
        assert!(unlock.is_current(software));
        unlock.retire(); // Settings changed while password/profile preparation was in progress.
        let hardware = unlock.begin(false);
        assert!(!unlock.is_current(software));
        unlock.complete(software, true);
        assert!(!unlock.state.completed);
        assert!(unlock.is_current(hardware));
        unlock.complete(hardware, false); // Metadata-only unlock does not complete the continuation.
        assert!(!unlock.state.completed);
        assert!(unlock.is_current(hardware));
        unlock.complete(hardware, true);
        assert!(unlock.state.completed);
        unlock.retire(); // Lock after an unlock, even if the actor never observed the unlocked state.
        unlock.complete(hardware, true);
        assert!(!unlock.state.completed);
        assert!(!unlock.is_current(hardware));
    }
}
