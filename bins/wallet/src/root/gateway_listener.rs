//! Draft listener settings and lifecycle identity for the browser gateway dialog.
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU16;
use std::rc::{Rc, Weak};

use gpui::{
    AppContext as _, Context, Entity, FocusHandle, IntoElement, ParentElement, SharedString,
    Styled, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Disableable, Icon, Sizable as _, WindowExt,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    dialog::{Cancel, Confirm, DialogFooter},
    input::{InputEvent, InputState},
};
use ui::controls::{app_button, app_input, app_muted_text, app_strong_text};
use wallet_ops::gateway::{GatewayConfig, GatewayError};

use crate::root::WalletRoot;

/// Addresses of the local IPv4 interfaces with their interface names, sorted
/// and deduplicated. Loopback and unspecified addresses are left out: callers
/// present those themselves when they apply.
pub(super) fn local_ipv4_addresses() -> Result<Vec<(IpAddr, String)>, ()> {
    let interfaces = if_addrs::get_if_addrs().map_err(|_| ())?;
    let mut seen = HashSet::new();
    let mut addresses = Vec::new();
    for interface in interfaces {
        let address = interface.ip();
        if address.is_ipv6()
            || address.is_loopback()
            || address.is_unspecified()
            || !seen.insert(address)
        {
            continue;
        }
        addresses.push((address, interface.name));
    }
    addresses.sort_by_key(|(address, _)| *address);
    Ok(addresses)
}

pub(super) struct GatewayListenerDialog {
    address: Entity<InputState>,
    _address_subscription: gpui::Subscription,
    port: Entity<InputState>,
    focus: FocusHandle,
    lease: Weak<()>,
    interfaces: Option<Result<Vec<(IpAddr, String)>, ()>>,
    error: Option<String>,
}

impl WalletRoot {
    pub(super) fn open_gateway_listener_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.gateway.busy || self.gateway.handle.is_none() || window.has_active_dialog(cx) {
            return;
        }
        self.gateway.popover_open = false;
        let config = self.gateway.snapshot.config;
        let address =
            cx.new(|cx| InputState::new(window, cx).default_value(config.bind_address.to_string()));
        let address_subscription = cx.subscribe(&address, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let port = cx.new(|cx| InputState::new(window, cx).default_value(config.port.to_string()));
        let lease = Rc::new(());
        let identity = Rc::downgrade(&lease);
        let root = cx.entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            // Only the active dialog builder owns a strong lease. Programmatic closes
            // can bypass on_close, so completion also checks the lease and focus.
            let identity = Rc::downgrade(&lease);
            let current = root.read(cx);
            let Some(state) = current.gateway.listener_dialog.as_ref()
                .filter(|state| state.lease.ptr_eq(&identity)) else {
                    return dialog;
                };
            let address = state.address.clone();
            let non_loopback = address.read(cx).value().trim().parse::<IpAddr>()
                .is_ok_and(|address| !address.is_loopback());
            let port = state.port.clone();
            let error = state.error.clone();
            let busy = current.gateway.busy || current.gateway.handle.is_none();
            let mut suggestions = vec![
                (IpAddr::V4(Ipv4Addr::LOCALHOST), "127.0.0.1 · This computer".to_owned()),
                (IpAddr::V4(Ipv4Addr::UNSPECIFIED), "0.0.0.0 · All IPv4 interfaces".to_owned()),
            ];
            if let Some(Ok(interfaces)) = &state.interfaces {
                suggestions.extend(
                    interfaces
                        .iter()
                        .map(|(address, name)| (*address, format!("{address} · {name}"))),
                );
            }
            let loading = state.interfaces.is_none();
            let enumeration_failed = matches!(state.interfaces, Some(Err(())));
            let mut body = div().flex().flex_col().gap_3()
                .child(div().flex().flex_col().gap_2()
                    .child(app_strong_text("IP address"))
                    .child(app_input(&address).aria_label("IP address").disabled(busy)))
                .child(div().flex().flex_col().gap_2()
                    .child(app_strong_text("Port"))
                    .child(app_input(&port).aria_label("Port").disabled(busy)))
                .when_some(error, |this, error| {
                    this.child(app_muted_text(error).text_color(cx.theme().danger))
                })
                .when(non_loopback, |this| {
                    this.child(Alert::warning(
                        "gateway-listener-exposure",
                        "This address may expose the gateway to other devices on your network.",
                    ).small())
                });
            let mut address_buttons = div().flex().flex_wrap().items_center().gap_2();
            for (address, label) in suggestions {
                let suggestion_root = root.clone();
                let suggestion_identity = identity.clone();
                let button = Button::new(SharedString::from(format!("gateway-address-{address}")))
                    .small().compact().outline().min_w_0().max_w_full()
                    .label(label)
                    .map(|button| {
                        if matches!(address, IpAddr::V4(address) if address == Ipv4Addr::LOCALHOST || address == Ipv4Addr::UNSPECIFIED) {
                            button.primary()
                        } else {
                            button.secondary()
                        }
                    })
                    .disabled(busy).on_click(move |_, window, cx| {
                    suggestion_root.update(cx, |root, cx| {
                        if root.gateway.busy {
                            return;
                        }
                        if let Some(state) = root.gateway.listener_dialog.as_ref()
                            .filter(|state| state.lease.ptr_eq(&suggestion_identity)
                                && state.lease.upgrade().is_some()) {
                            state.address.update(cx, |input, cx| {
                                input.set_value(address.to_string(), window, cx);
                            });
                            cx.notify();
                        }
                    });
                });
                address_buttons = address_buttons.child(button);
            }
            body = body
                .child(address_buttons)
                .when(loading, |this| this.child(app_muted_text("Finding local addresses…")))
                .when(enumeration_failed, |this| this.child(app_muted_text(
                    "Could not list local addresses. Choose a preset or enter an IP address.",
                )));
            let save_root = root.clone();
            let save_identity = identity.clone();
            let close_root = root.clone();
            let close_identity = identity;
            dialog
                .w((window.viewport_size().width * 0.92).min(window.rem_size() * 32.0))
                .max_h(window.viewport_size().height * 0.8)
                .title(div().flex().items_center().gap_2()
                    .child(Icon::new(crate::assets::RailgunSidebarIcon::BrowserExtensions).small())
                    .child(app_strong_text("Listener settings")))
                .on_ok(move |_, window, cx| {
                    save_root.update(cx, |root, cx| {
                        root.save_gateway_listener(&save_identity, window, cx);
                    });
                    false
                })
                .on_close(move |_, _, cx| {
                    close_root.update(cx, |root, cx| {
                        if root.gateway.listener_dialog.as_ref()
                            .is_some_and(|state| state.lease.ptr_eq(&close_identity)) {
                            root.gateway.listener_dialog = None;
                            cx.notify();
                        }
                    });
                })
                .child(body)
                .footer(DialogFooter::new().children([
                    app_button("gateway-listener-cancel", "Cancel")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx))
                        .into_any_element(),
                    app_button("gateway-listener-save", if busy { "Saving…" } else { "Save" })
                        .primary().disabled(busy)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(Confirm { secondary: false }), cx);
                        })
                        .into_any_element(),
                ]))
        });
        let Some(focus) = window.focused(cx) else {
            return;
        };
        self.gateway.listener_dialog = Some(GatewayListenerDialog {
            address,
            _address_subscription: address_subscription,
            port,
            focus,
            lease: identity.clone(),
            interfaces: None,
            error: None,
        });
        let interfaces = cx
            .background_executor()
            .spawn(async { local_ipv4_addresses() });
        cx.spawn(async move |this, cx| {
            let interfaces = interfaces.await;
            let _ = this.update(cx, |root, cx| {
                if let Some(state) = root.gateway.listener_dialog.as_mut().filter(|state| {
                    state.lease.ptr_eq(&identity) && state.lease.upgrade().is_some()
                }) {
                    state.interfaces = Some(interfaces);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn save_gateway_listener(
        &mut self,
        identity: &Weak<()>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.gateway.busy {
            return;
        }
        let Some(handle) = self.gateway.handle.clone() else {
            return;
        };
        let Some(state) = self.gateway.listener_dialog.as_mut().filter(|state| {
            state.lease.ptr_eq(identity)
                && state.lease.upgrade().is_some()
                && state.focus.contains_focused(window, cx)
        }) else {
            return;
        };
        let Ok(bind_address) = state.address.read(cx).value().trim().parse::<IpAddr>() else {
            state.error = Some("Enter a valid IP address.".to_owned());
            cx.notify();
            return;
        };
        let Ok(port) = state.port.read(cx).value().trim().parse::<NonZeroU16>() else {
            state.error = Some("Enter a port from 1 to 65535.".to_owned());
            cx.notify();
            return;
        };
        state.error = None;
        self.gateway.pending_offer = None;
        self.gateway.offer_expiry_task = None;
        self.gateway.busy = true;
        self.gateway.error = None;
        self.gateway.operation_generation = self.gateway.operation_generation.wrapping_add(1);
        let generation = self.gateway.operation_generation;
        let config = GatewayConfig {
            bind_address,
            port,
            ..self.gateway.snapshot.config
        };
        let task = self
            .runtime
            .spawn(async move { handle.configure(config).await });
        let identity = identity.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.unwrap_or(Err(GatewayError::Unavailable));
            let _ = this.update_in(cx, |root, window, cx| {
                if root.gateway.handle.is_none() || root.gateway.operation_generation != generation
                {
                    return;
                }
                root.gateway.busy = false;
                if let Some(handle) = root.gateway.handle.as_ref() {
                    root.gateway.snapshot = handle.snapshots().borrow().clone();
                }
                root.gateway.error = result.err();
                let own_dialog = root.gateway.listener_dialog.as_ref().is_some_and(|state| {
                    state.lease.ptr_eq(&identity)
                        && state.lease.upgrade().is_some()
                        && state.focus.contains_focused(window, cx)
                });
                if own_dialog {
                    if let Some(error) = root.gateway.error {
                        root.gateway
                            .listener_dialog
                            .as_mut()
                            .expect("current dialog")
                            .error = Some(error.to_string());
                    } else {
                        root.gateway.listener_dialog = None;
                        window.close_dialog(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
