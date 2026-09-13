//! Addresses to open in a browser to install the extension from the gateway.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::rc::{Rc, Weak};

use gpui::{
    App, Context, FocusHandle, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, rems,
};
use gpui_component::{
    Icon, Sizable as _, WindowExt,
    button::ButtonVariants as _,
    clipboard::Clipboard,
    dialog::{Cancel, Confirm, DialogFooter},
};
use ui::controls::{app_button, app_muted_text, app_strong_text, app_text};
use ui::theme::APP_MONO_FONT_FAMILY;

use super::listener::local_ipv4_addresses;
use crate::root::WalletRoot;

pub(super) struct GatewayInstallDialog {
    focus: FocusHandle,
    lease: Weak<()>,
    listener: SocketAddr,
    interfaces: Option<Result<Vec<(IpAddr, String)>, ()>>,
}

impl GatewayInstallDialog {
    /// The enumerated interfaces, or `None` while they are pending or failed.
    const fn addresses(&self) -> Option<&[(IpAddr, String)]> {
        match &self.interfaces {
            Some(Ok(addresses)) => Some(addresses.as_slice()),
            Some(Err(())) | None => None,
        }
    }
}

/// Install page addresses as `(label, url)`. A listener bound to one address
/// has exactly one; a listener bound to every interface leads with loopback
/// and lists the interfaces after it.
pub(super) fn install_addresses(
    listener: SocketAddr,
    interfaces: Option<&[(IpAddr, String)]>,
) -> Vec<(String, String)> {
    if !listener.ip().is_unspecified() {
        return vec![("Address".to_owned(), format!("http://{listener}/install"))];
    }
    let port = listener.port();
    let loopback = if listener.is_ipv6() {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    let url = |address| format!("http://{}/install", SocketAddr::new(address, port));
    let mut addresses = vec![("This computer".to_owned(), url(loopback))];
    if let Some(interfaces) = interfaces {
        addresses.extend(
            interfaces
                .iter()
                .map(|(address, name)| (name.clone(), url(*address))),
        );
    }
    addresses
}

impl WalletRoot {
    pub(super) fn open_gateway_install_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.gateway.busy || self.gateway.handle.is_none() || window.has_active_dialog(cx) {
            return;
        }
        let Some(listener) = self.gateway.snapshot.listener_addr else {
            return;
        };
        self.gateway.popover_open = false;
        let lease = Rc::new(());
        let identity = Rc::downgrade(&lease);
        let root = cx.entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            // Only the active dialog builder owns a strong lease. Programmatic closes
            // can bypass on_close, so opening a browser also checks the lease and focus.
            let identity = Rc::downgrade(&lease);
            let current = root.read(cx);
            let Some(state) = current
                .gateway
                .install_dialog
                .as_ref()
                .filter(|state| state.lease.ptr_eq(&identity))
            else {
                return dialog;
            };
            let loading = state.listener.ip().is_unspecified() && state.interfaces.is_none();
            let enumeration_failed = matches!(state.interfaces, Some(Err(())));
            let addresses = install_addresses(state.listener, state.addresses());
            let mut address_rows = div().flex().flex_col().gap_2();
            for (index, (label, url)) in addresses.into_iter().enumerate() {
                let copy = SharedString::from(format!("gateway-install-copy-{index}"));
                address_rows = address_rows.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(app_muted_text(label).flex_none().w(rems(6.0)))
                        .child(
                            app_text(url.clone())
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(APP_MONO_FONT_FAMILY),
                        )
                        .child(Clipboard::new(copy).value(url).tooltip("Copy address")),
                );
            }
            let body = div()
                .flex()
                .flex_col()
                .gap_3()
                .child(app_muted_text(
                    "Open this address in the browser you want to pair.",
                ))
                .child(address_rows)
                .when(loading, |this| {
                    this.child(app_muted_text("Finding local addresses…"))
                })
                .when(enumeration_failed, |this| {
                    this.child(app_muted_text("Could not list local addresses."))
                });
            let open_root = root.clone();
            let open_identity = identity.clone();
            let close_root = root.clone();
            let close_identity = identity;
            dialog
                .w((window.viewport_size().width * 0.92).min(window.rem_size() * 32.0))
                .max_h(window.viewport_size().height * 0.8)
                .title(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(crate::assets::RailgunSidebarIcon::BrowserExtensions).small(),
                        )
                        .child(app_strong_text("Install browser extension")),
                )
                .on_ok(move |_, window, cx| {
                    if let Some(url) =
                        open_root
                            .read(cx)
                            .gateway_install_url(&open_identity, window, cx)
                    {
                        cx.open_url(&url);
                    }
                    false
                })
                .on_close(move |_, _, cx| {
                    close_root.update(cx, |root, cx| {
                        if root
                            .gateway
                            .install_dialog
                            .as_ref()
                            .is_some_and(|state| state.lease.ptr_eq(&close_identity))
                        {
                            root.gateway.install_dialog = None;
                            cx.notify();
                        }
                    });
                })
                .child(body)
                .footer(
                    DialogFooter::new().children([
                        app_button("gateway-install-close", "Close")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx))
                            .into_any_element(),
                        app_button("gateway-install-open", "Open in browser")
                            .primary()
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(Confirm { secondary: false }), cx);
                            })
                            .into_any_element(),
                    ]),
                )
        });
        let Some(focus) = window.focused(cx) else {
            return;
        };
        self.gateway.install_dialog = Some(GatewayInstallDialog {
            focus,
            lease: identity.clone(),
            listener,
            interfaces: None,
        });
        if listener.ip().is_unspecified() {
            let interfaces = cx
                .background_executor()
                .spawn(async { local_ipv4_addresses() });
            cx.spawn(async move |this, cx| {
                let interfaces = interfaces.await;
                let _ = this.update(cx, |root, cx| {
                    if let Some(state) = root.gateway.install_dialog.as_mut().filter(|state| {
                        state.lease.ptr_eq(&identity) && state.lease.upgrade().is_some()
                    }) {
                        state.interfaces = Some(interfaces);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// The address `Open in browser` uses, or `None` once this dialog is gone.
    fn gateway_install_url(
        &self,
        identity: &Weak<()>,
        window: &Window,
        cx: &App,
    ) -> Option<String> {
        let state = self.gateway.install_dialog.as_ref().filter(|state| {
            state.lease.ptr_eq(identity)
                && state.lease.upgrade().is_some()
                && state.focus.contains_focused(window, cx)
        })?;
        install_addresses(state.listener, state.addresses())
            .into_iter()
            .next()
            .map(|(_, url)| url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_addresses_follow_the_listener() {
        let interfaces = [
            (
                IpAddr::V4(Ipv4Addr::new(192, 168, 64, 3)),
                "eth0".to_owned(),
            ),
            (IpAddr::V4(Ipv4Addr::new(10, 0, 0, 12)), "wlan0".to_owned()),
        ];
        assert_eq!(
            install_addresses("127.0.0.1:43110".parse().unwrap(), Some(&interfaces)),
            vec![(
                "Address".to_owned(),
                "http://127.0.0.1:43110/install".to_owned()
            )]
        );
        assert_eq!(
            install_addresses("192.168.64.3:43110".parse().unwrap(), None),
            vec![(
                "Address".to_owned(),
                "http://192.168.64.3:43110/install".to_owned()
            )]
        );
        assert_eq!(
            install_addresses("0.0.0.0:43110".parse().unwrap(), Some(&interfaces)),
            vec![
                (
                    "This computer".to_owned(),
                    "http://127.0.0.1:43110/install".to_owned()
                ),
                (
                    "eth0".to_owned(),
                    "http://192.168.64.3:43110/install".to_owned()
                ),
                (
                    "wlan0".to_owned(),
                    "http://10.0.0.12:43110/install".to_owned()
                ),
            ]
        );
        assert_eq!(
            install_addresses("[::]:43110".parse().unwrap(), None),
            vec![(
                "This computer".to_owned(),
                "http://[::1]:43110/install".to_owned()
            )]
        );
        assert_eq!(
            install_addresses("0.0.0.0:43110".parse().unwrap(), None),
            vec![(
                "This computer".to_owned(),
                "http://127.0.0.1:43110/install".to_owned()
            )]
        );
    }
}
