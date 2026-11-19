#[path = "gateway_chains.rs"]
mod chains;
use super::network::{query_exit_ip_through_tor, reset_tor_state_and_quit};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wallet_ops::gateway::{
    GatewayNetworkError, GatewayNetworkOperation, GatewayNetworkOutcome, GatewayNetworkRequest,
};

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
    GatewayConfig, GatewayError, GatewayHandle, GatewayInstallBundle, GatewayPairingOffer,
    GatewaySnapshot, GatewayUnlockState, GatewayWalletState, PeerId, extension_update_available,
};

use crate::assets::RailgunSidebarIcon;

use super::{VaultState, WalletRoot, app_status_tag};
#[path = "gateway_handoff.rs"]
mod handoff;
use handoff::{GatewaySwitchContinuation, GatewaySwitchDialog};
#[path = "gateway_install.rs"]
mod install;
use install::GatewayInstallDialog;
#[path = "gateway_listener.rs"]
mod listener;
use listener::GatewayListenerDialog;
mod desktop_state;
mod paired_browsers;
#[path = "gateway_pairing.rs"]
mod pairing;
#[path = "gateway_unlock.rs"]
mod remote_unlock;
mod status_controls;
use pairing::GatewayPairingDialog;

/// Extension packaged into this build by `bins/wallet/build.rs`, empty when it was not built.
const fn install_bundle() -> GatewayInstallBundle {
    GatewayInstallBundle {
        zip: include_bytes!(concat!(env!("OUT_DIR"), "/browser-extension.zip")),
        version: env!("RAILOXIDE_EXTENSION_VERSION"),
    }
}

#[derive(Default)]
struct GatewayUnlock {
    remote: Option<Arc<wallet_ops::gateway::GatewayUnlockAttempt>>,
    state: GatewayUnlockState,
    continuation: Option<u64>,
    installing: bool,
}

impl GatewayUnlock {
    fn retire(&mut self) {
        if let Some(remote) = self.remote.take() {
            remote.cancel();
        }
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
        if has_view
            && self.is_current(continuation)
            && self
                .remote
                .as_ref()
                .is_none_or(|remote| remote.is_current())
        {
            if let Some(remote) = self.remote.take() {
                remote.finish(wallet_ops::gateway::GatewayUnlockPhase::Complete);
            }
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
    install_dialog: Option<GatewayInstallDialog>,
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
    pub(super) popover_open: bool,
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
            GatewayHandle::start(store.db(), true, 0, install_bundle())
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
                let received = tokio::select! {
                    event = ui_events.recv() => Some(event),
                    () = cx.background_executor().timer(Duration::from_millis(100)) => None,
                };
                if this
                    .update_in(cx, |root, window, cx| {
                        root.reconcile_remote_unlock(window, cx);
                    })
                    .is_err()
                {
                    break;
                }
                let Some(received) = received else {
                    continue;
                };
                let event = match received {
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
                            wallet_ops::gateway::GatewayUiEventKind::Unlock { request } => {
                                root.apply_gateway_unlock(&request, window, cx);
                            }
                            wallet_ops::gateway::GatewayUiEventKind::ChainEditor { request } => {
                                root.apply_gateway_chain_editor(request, window, cx);
                            }
                            wallet_ops::gateway::GatewayUiEventKind::Network { request } => {
                                root.apply_gateway_network_command(request, cx);
                            }
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
            install_dialog: None,
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
    allow_unlock: bool,
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
    let unlock_root = target.root.clone();
    let allow_unlock = target.allow_unlock;
    let unlock_disabled = target.disabled || target.root.read(cx).vault_view_unlock.is_none();
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
        .item(
            PopupMenuItem::element(|_, _| {
                div()
                    .id("gateway-peer-allow-unlock")
                    .flex_1()
                    .child("Allow unlocking")
                    .tooltip(|window, cx| {
                        remote_unlock::unlock_trust_tooltip(window).build(window, cx)
                    })
            })
            .checked(allow_unlock)
            .disabled(unlock_disabled)
            .on_click(move |_, window, cx| {
                unlock_root.update(cx, |root, cx| {
                    let Some(handle) = root.gateway.handle.clone() else {
                        return;
                    };
                    root.run_gateway_operation(
                        async move {
                            handle
                                .set_peer_allow_unlock(peer_id, !allow_unlock)
                                .await
                                .map(|()| None)
                        },
                        window,
                        cx,
                    );
                });
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

impl WalletRoot {
    pub(super) fn publish_gateway_network_state(&self) {
        let Some(state) = &self.gateway.desktop_state else {
            return;
        };
        let network = self.gateway_network_view();
        state.send_if_modified(|(wallet, _, _)| {
            let next = wallet.view.as_ref().map(|_| network);
            if wallet.network_view == next {
                return false;
            }
            wallet.network_view = next;
            if let Some(handle) = &self.gateway.handle {
                handle.set_wallet_authority(wallet.clone());
            }
            true
        });
    }

    /// Completes rotation requests whose view and network context are still current. A committed
    /// session changes the context, so its outcome reaches views through the snapshot instead.
    pub(super) fn complete_gateway_network_requests(
        &self,
        requests: Vec<GatewayNetworkRequest>,
        outcome: &GatewayNetworkOutcome,
    ) {
        let Some(handle) = &self.gateway.handle else {
            return;
        };
        let revision = self.network_context_revision();
        for request in requests
            .into_iter()
            .filter(|request| request.is_current(&revision))
        {
            let handle = handle.clone();
            let outcome = outcome.clone();
            self.runtime.spawn(async move {
                handle.complete_network_request(request, outcome).await;
            });
        }
    }

    pub(super) fn apply_gateway_network_command(
        &mut self,
        request: GatewayNetworkRequest,
        cx: &mut Context<'_, Self>,
    ) {
        if !request.is_current(&self.network_context_revision())
            || self.http.network_mode() != wallet_ops::WalletNetworkMode::Tor
            || self.view_session.is_none()
            || *self.root_shutdown.borrow()
        {
            return;
        }
        let Some(handle) = self.gateway.handle.clone() else {
            return;
        };
        if request.operation() == GatewayNetworkOperation::QueryExitIp {
            let proxy = self.http.proxy_url.clone();
            let query = self.runtime.spawn(async move {
                let Some(proxy) = proxy else {
                    return GatewayNetworkOutcome::Failed {
                        error: GatewayNetworkError::Unavailable,
                    };
                };
                match query_exit_ip_through_tor(proxy).await {
                    Ok(ip) => GatewayNetworkOutcome::ExitIp { ip },
                    Err(_) => GatewayNetworkOutcome::Failed {
                        error: GatewayNetworkError::ExitIpFailed,
                    },
                }
            });
            let runtime = self.runtime.clone();
            cx.spawn(async move |this, cx| {
                let outcome = query.await.unwrap_or(GatewayNetworkOutcome::Failed {
                    error: GatewayNetworkError::ExitIpFailed,
                });
                let _ = this.update(cx, |root, _| {
                    if request.is_current(&root.network_context_revision()) {
                        runtime.spawn(async move {
                            handle.complete_network_request(request, outcome).await;
                        });
                    }
                });
            })
            .detach();
            return;
        }
        let outcome = match request.operation() {
            GatewayNetworkOperation::NewTorSession => {
                // Admitted: the root owns the rotation, and closing this view doesn't cancel it.
                self.request_tor_rotation(Some(request), cx);
                return;
            }
            GatewayNetworkOperation::QuitAndReset => {
                reset_tor_state_and_quit(&self.options.db_path, || cx.quit()).map_or(
                    GatewayNetworkOutcome::Failed {
                        error: GatewayNetworkError::ResetFailed,
                    },
                    |()| GatewayNetworkOutcome::Done,
                )
            }
            GatewayNetworkOperation::QueryExitIp => unreachable!("query handled above"),
        };
        self.runtime.spawn(async move {
            handle.complete_network_request(request, outcome).await;
        });
        cx.notify();
    }
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
        let handle = GatewayHandle::start(store.db(), true, 0, GatewayInstallBundle::default());
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
            .send((unlocked.clone(), 1, Vec::new()))
            .expect("publisher must still accept desktop updates");
        tokio::time::timeout(
            Duration::from_secs(5),
            snapshots.wait_for(|snapshot| !snapshot.locked && snapshot.generation == 1),
        )
        .await
        .expect("later wallet state must reach the gateway")
        .unwrap();

        // An explicit add completion may publish alongside the watch publisher.
        // Both use the same epoch; an older queued route snapshot must be rejected.
        let mut added = unlocked.clone();
        added.routes.insert(
            9_007_199_254_740_993,
            wallet_ops::RpcChainRoute::new(
                9_007_199_254_740_993,
                vec![reqwest::Url::parse("https://custom.invalid").unwrap()],
            ),
        );
        handle.set_wallet_authority(added.clone());
        assert!(handle.publish_wallet_state(unlocked, 1).await.is_err());
        handle.publish_wallet_state(added, 1).await.unwrap();

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
