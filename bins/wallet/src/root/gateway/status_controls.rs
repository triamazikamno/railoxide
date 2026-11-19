use super::*;

impl WalletRoot {
    /// Whether any paired browser reports an extension older than the bundled one.
    fn gateway_update_available(&self) -> bool {
        self.gateway.snapshot.peers.iter().any(|peer| {
            peer.extension_version
                .as_deref()
                .is_some_and(|reported| extension_update_available(reported, install_bundle()))
        })
    }

    /// Status bar icon. While a dialog is open it only shows its tooltip.
    pub(in crate::root) fn render_gateway_status_icon(
        &self,
        root: &Entity<Self>,
        dialog_open: bool,
    ) -> gpui::AnyElement {
        let popover_root = root.clone();
        let content_root = root.clone();
        let update_available = self.gateway_update_available();
        let total_pairings = self.gateway.snapshot.peers.len();
        let active_sessions: usize = self
            .gateway
            .snapshot
            .peers
            .iter()
            .map(|peer| peer.connected_sessions)
            .sum();
        let (color, status) = if matches!(
            self.gateway.snapshot.error,
            Some(GatewayError::PortInUse(_))
        ) {
            (theme::DANGER, "Port in use".to_owned())
        } else if total_pairings > 1 {
            let color = if active_sessions > 0 {
                theme::SUCCESS
            } else {
                theme::TEXT_MUTED
            };
            (color, format!("{active_sessions}/{total_pairings} active"))
        } else if active_sessions > 0 {
            (theme::SUCCESS, "active".to_owned())
        } else {
            (theme::TEXT_MUTED, "inactive".to_owned())
        };
        let update_note = if update_available {
            " (extension update available)"
        } else {
            ""
        };
        let description = format!("Browser pairing: {status}{update_note}");
        let trigger = Button::new("wallet-gateway-status-pill-trigger")
            .ghost()
            .small()
            .px_0p5()
            .accessibility_label(description.clone())
            .tooltip(description)
            .debug_selector(|| "wallet-gateway-status-icon".into())
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(
                        Icon::new(RailgunSidebarIcon::BrowserExtensions)
                            .size(ui::theme::STATUS_BAR_ICON_SIZE)
                            .text_color(rgb(color)),
                    )
                    .when(update_available, |this| {
                        this.child(
                            // Inside the icon's box: the button clips anything outside it.
                            div()
                                .absolute()
                                .top_0()
                                .right_0()
                                .size(px(6.0))
                                .rounded_full()
                                .bg(rgb(theme::WARNING)),
                        )
                    }),
            );
        if dialog_open {
            // Inert above a dialog: a press must not take focus from the dialog either.
            return trigger
                .capture_any_mouse_down(|_, window, _| window.prevent_default())
                .into_any_element();
        }
        Popover::new("wallet-gateway-status-popover")
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
                let install = content_root
                    .read(cx)
                    .render_gateway_install_button(&content_root, &popover);
                div()
                    .w(width)
                    .min_w(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .text_size(APP_TEXT_SIZE)
                    .text_color(rgb(theme::TEXT))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(app_strong_text("Browser pairing").flex_1().min_w_0())
                            .child(install),
                    )
                    .child(content_root.update(cx, |root, cx| {
                        root.render_gateway_controls(&content_root, &popover, max_height, cx)
                            .into_any_element()
                    }))
            })
            .into_any_element()
    }

    /// Title-row action: opens the install dialog once a listener address exists.
    fn render_gateway_install_button(
        &self,
        root: &Entity<Self>,
        popover: &Entity<PopoverState>,
    ) -> Button {
        let disabled = self.gateway.busy
            || self.gateway.handle.is_none()
            || self.gateway.snapshot.listener_addr.is_none();
        let install_root = root.clone();
        let install_popover = popover.clone();
        Button::new("gateway-install-extension")
            .small()
            .outline()
            .secondary()
            .icon(Icon::new(RailgunSidebarIcon::BrowserExtensions).small())
            .label("Install extension…")
            .disabled(disabled)
            .on_click(move |_, window, cx| {
                install_popover.update(cx, |state, cx| state.dismiss(window, cx));
                install_root.update(cx, |root, cx| {
                    root.open_gateway_install_dialog(window, cx);
                });
            })
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

    pub(super) fn run_gateway_operation(
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
