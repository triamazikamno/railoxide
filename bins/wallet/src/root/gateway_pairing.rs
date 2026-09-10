//! Transient pairing-code display owned by the active dialog.
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable as _, IntoElement, ParentElement,
    Styled, Window, div, prelude::FluentBuilder as _,
};
use gpui_component::{
    ActiveTheme as _, Disableable, Icon, Sizable as _, WindowExt,
    alert::Alert,
    button::ButtonVariants as _,
    clipboard::Clipboard,
    dialog::{Cancel, Confirm, DialogFooter},
    input::OtpState,
};
use gpui_kit::base::OtpInput;
use ui::controls::{app_button, app_muted_text, app_strong_text, app_text};
use ui::theme::APP_MONO_FONT_FAMILY;
use wallet_ops::gateway::{GatewayPairingOffer, GatewaySnapshot, PeerId};

use crate::root::WalletRoot;

pub(super) struct GatewayPairingDialog {
    pub(super) lease: Weak<()>,
    focus: FocusHandle,
    // Keep this after attempt exhaustion: an already admitted attempt can still commit.
    peers_before_offer: Option<Vec<PeerId>>,
}

impl WalletRoot {
    pub(super) fn open_gateway_pairing_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.gateway.busy
            || !self.gateway.snapshot.config.enabled
            || self.gateway.snapshot.listener_addr.is_none()
            || window.has_active_dialog(cx)
        {
            return;
        }
        if self.gateway.handle.is_none() {
            return;
        }
        self.gateway.popover_open = false;
        self.gateway.pending_offer = None;
        self.gateway.offer_expiry_task = None;
        let lease = Rc::new(());
        let identity = Rc::downgrade(&lease);
        let root = cx.entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            // Programmatic closes bypass on_close. Only this builder retains the lease.
            let identity = Rc::downgrade(&lease);
            let current = root.read(cx);
            let gateway = &current.gateway;
            let mut body = div().flex().flex_col().gap_3().child(app_muted_text(
                "Enter this code in the browser extension to pair it with this desktop app.",
            ));
            let offer = gateway.pending_offer.as_ref().filter(|(_, deadline)| {
                gateway
                    .pairing_dialog
                    .as_ref()
                    .is_some_and(|current| current.lease.ptr_eq(&identity))
                    && gateway.snapshot.pairing_active
                    && gateway.snapshot.config.enabled
                    && gateway.snapshot.listener_addr.is_some()
                    && *deadline > Instant::now()
            });
            let show_generate = offer.is_none();
            let can_generate = !gateway.busy
                && gateway.handle.is_some()
                && gateway.snapshot.config.enabled
                && gateway.snapshot.listener_addr.is_some();
            if let Some((state, deadline)) = offer {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(render_pairing_code(state, cx))
                        .child(app_muted_text(format!(
                            "Expires in {}:{:02} · invalid after 3 failed attempts",
                            seconds / 60,
                            seconds % 60
                        ))),
                );
            } else if gateway.busy {
                body = body.child(app_muted_text("Generating pairing code…"));
            } else if let Some(error) = gateway.error {
                body = body.child(Alert::error("gateway-pairing-error", error.to_string()).small());
            } else {
                body = body.child(
                    Alert::error("gateway-pairing-invalid", "This code is no longer valid.")
                        .small(),
                );
            }
            body = body.child(if let Some(address) = gateway.snapshot.listener_addr {
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(app_muted_text("Endpoint"))
                    .child(
                        app_text(address.to_string())
                            .min_w_0()
                            .font_family(APP_MONO_FONT_FAMILY),
                    )
                    .child(
                        Clipboard::new("gateway-pairing-copy-address")
                            .value(address.to_string())
                            .tooltip("Copy endpoint"),
                    )
            } else {
                app_muted_text("Listener stopped")
            });
            let generate_root = root.clone();
            let generate_identity = identity.clone();
            let close_root = root.clone();
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
                        .child(app_strong_text("Browser extension pairing")),
                )
                .on_ok(move |_, window, cx| {
                    generate_root.update(cx, |root, cx| {
                        root.generate_gateway_pairing_code(&generate_identity, window, cx);
                    });
                    false
                })
                .on_close(move |_, _, cx| {
                    close_root.update(cx, |root, cx| {
                        if root
                            .gateway
                            .pairing_dialog
                            .as_ref()
                            .is_some_and(|current| current.lease.ptr_eq(&identity))
                        {
                            root.gateway.pairing_dialog = None;
                            root.gateway.pending_offer = None;
                            root.gateway.offer_expiry_task = None;
                            cx.notify();
                        }
                    });
                })
                .child(body)
                .footer(
                    DialogFooter::new()
                        .child(
                            app_button("gateway-pairing-close", "Close").on_click(
                                |_, window, cx| window.dispatch_action(Box::new(Cancel), cx),
                            ),
                        )
                        .when(show_generate, |footer| {
                            footer.child(
                                app_button("gateway-pairing-generate", "Generate new code")
                                    .primary()
                                    .disabled(!can_generate)
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(
                                            Box::new(Confirm { secondary: false }),
                                            cx,
                                        );
                                    }),
                            )
                        }),
                )
        });
        let Some(focus) = window.focused(cx) else {
            return;
        };
        self.gateway.pairing_dialog = Some(GatewayPairingDialog {
            lease: identity.clone(),
            focus,
            peers_before_offer: None,
        });
        self.generate_gateway_pairing_code(&identity, window, cx);
    }

    fn generate_gateway_pairing_code(
        &mut self,
        identity: &Weak<()>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.gateway.busy
            || !self.gateway.snapshot.config.enabled
            || self.gateway.snapshot.listener_addr.is_none()
            || !self.gateway.pairing_dialog.as_ref().is_some_and(|current| {
                current.lease.ptr_eq(identity) && current.lease.upgrade().is_some()
            })
            || self
                .gateway
                .pending_offer
                .as_ref()
                .is_some_and(|(_, deadline)| {
                    self.gateway.snapshot.pairing_active && *deadline > Instant::now()
                })
        {
            return;
        }
        let Some(handle) = self.gateway.handle.clone() else {
            return;
        };
        self.retire_gateway_pairing_offer(window, cx);
        self.run_gateway_operation(
            async move { handle.issue_pairing_code().await.map(Some) },
            window,
            cx,
        );
    }

    pub(super) fn reconcile_gateway_pairing_dialog(
        &mut self,
        snapshot: &GatewaySnapshot,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // A newly persisted peer proves pairing completed. Session counts also change on reconnect.
        let paired = !snapshot.pairing_active
            && self.gateway.pairing_dialog.as_ref().is_some_and(|dialog| {
                dialog.peers_before_offer.as_ref().is_some_and(|peers| {
                    snapshot.peers.iter().any(|peer| !peers.contains(&peer.id))
                })
            });
        if !snapshot.pairing_active || !snapshot.config.enabled || snapshot.listener_addr.is_none()
        {
            let own_dialog = self.retire_gateway_pairing_offer(window, cx);
            if paired && own_dialog {
                self.gateway.pairing_dialog = None;
                window.close_dialog(cx);
            }
        }
    }

    fn retire_gateway_pairing_offer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let own_dialog = self.gateway.pairing_dialog.as_ref().is_some_and(|dialog| {
            dialog.lease.upgrade().is_some()
                && (dialog.focus.contains_focused(window, cx)
                    || self
                        .gateway
                        .pending_offer
                        .as_ref()
                        .is_some_and(|(state, _)| {
                            state.read(cx).focus_handle(cx).is_focused(window)
                        }))
        });
        if own_dialog && self.gateway.pending_offer.is_some() {
            // The deadline filter may already have unmounted the focused OTP. Its handle still
            // identifies our focus, so restore the dialog's existing root before dropping it.
            self.gateway
                .pairing_dialog
                .as_ref()
                .expect("current pairing dialog")
                .focus
                .focus(window, cx);
        }
        self.gateway.pending_offer = None;
        self.gateway.offer_expiry_task = None;
        own_dialog
    }

    pub(super) fn show_gateway_pairing_offer(
        &mut self,
        offer: GatewayPairingOffer,
        deadline: Instant,
        identity: &Weak<()>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let code = String::from_utf8_lossy(offer.code.expose_for_display()).into_owned();
        drop(offer);
        let state = cx.new(|cx| OtpState::new(6, window, cx).default_value(code));
        self.gateway.pending_offer = Some((state, deadline));
        if let Some(dialog) = self.gateway.pairing_dialog.as_mut() {
            dialog.peers_before_offer = Some(
                self.gateway
                    .snapshot
                    .peers
                    .iter()
                    .map(|peer| peer.id)
                    .collect(),
            );
        }
        let identity = identity.clone();
        self.gateway.offer_expiry_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(
                        Duration::from_secs(1)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    )
                    .await;
                let keep_ticking = this.update_in(cx, |root, window, cx| {
                    let current = root
                        .gateway
                        .pairing_dialog
                        .as_ref()
                        .is_some_and(|current| current.lease.ptr_eq(&identity));
                    let same_offer = root
                        .gateway
                        .pending_offer
                        .as_ref()
                        .is_some_and(|(_, expires)| *expires == deadline);
                    if !current || !same_offer {
                        return false;
                    }
                    if identity.upgrade().is_none() || deadline <= Instant::now() {
                        root.retire_gateway_pairing_offer(window, cx);
                        if identity.upgrade().is_none() {
                            root.gateway.pairing_dialog = None;
                        }
                        cx.notify();
                        return false;
                    }
                    cx.notify();
                    true
                });
                if !matches!(keep_ticking, Ok(true)) {
                    break;
                }
            }
        }));
    }
}

fn render_pairing_code(state: &Entity<OtpState>, cx: &App) -> impl IntoElement {
    // The styled OTP only offers muted disabled cells, with no primary or read-only variant.
    // Base keeps editing disabled while the application supplies readable primary cells.
    let code = state.read(cx).value();
    let groups = (0..2).map(|group| {
        div()
            .flex()
            .items_center()
            .gap_1()
            .children(code.chars().skip(group * 3).take(3).map(|digit| {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_11()
                    .h_11()
                    .text_lg()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().primary)
                    .bg(cx.theme().background)
                    .text_color(cx.theme().primary)
                    .child(digit.to_string())
            }))
    });
    OtpInput::new(state).disabled(true).child(
        div()
            .flex()
            .items_center()
            .justify_center()
            .gap_5()
            .children(groups),
    )
}
