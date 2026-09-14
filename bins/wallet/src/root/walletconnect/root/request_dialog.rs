use super::*;
use gpui::StatefulInteractiveElement as _;
use std::time::Duration;

impl WalletRoot {
    pub(in crate::root) fn open_next_walletconnect_request_dialog_if_idle(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .walletconnect
            .request_dialog_key
            .as_deref()
            .is_some_and(|key| {
                key.starts_with("gateway:")
                    && !self.walletconnect.pending_requests.contains_key(key)
                    && !self
                        .walletconnect
                        .completed_request_dialogs
                        .contains_key(key)
            })
        {
            self.clear_walletconnect_request_dialog_state(window, cx);
            window.close_all_dialogs(cx);
        }
        let active_dialog = window.has_active_dialog(cx);
        if self.walletconnect.request_dialog_open && !active_dialog {
            let stale_request_key = self
                .walletconnect
                .request_dialog_key
                .as_deref()
                .map(walletconnect_request_key_log_label);
            tracing::debug!(
                target: "wallet::root::walletconnect",
                request_key = stale_request_key.as_deref().unwrap_or("<none>"),
                "clearing stale walletconnect request dialog state"
            );
            self.clear_walletconnect_request_dialog_state(window, cx);
            self.walletconnect.request_dialog_deferred_logged = false;
        }
        if self.walletconnect.pending_requests.is_empty() {
            self.walletconnect.request_dialog_deferred_logged = false;
            return;
        }
        let Some(request_key) = next_walletconnect_auto_open_request_key(
            &self.walletconnect.pending_requests,
            &self.walletconnect.dismissed_request_dialog_keys,
        ) else {
            self.walletconnect.request_dialog_deferred_logged = false;
            return;
        };
        if self.walletconnect.request_dialog_open || active_dialog {
            if !self.walletconnect.request_dialog_deferred_logged {
                tracing::debug!(
                    target: "wallet::root::walletconnect",
                    pending_count = self.walletconnect.pending_requests.len(),
                    walletconnect_dialog_open = self.walletconnect.request_dialog_open,
                    active_dialog,
                    request_key = %walletconnect_request_key_log_label(&request_key),
                    "walletconnect request dialog deferred"
                );
                self.walletconnect.request_dialog_deferred_logged = true;
            }
            return;
        }
        self.walletconnect.request_dialog_deferred_logged = false;
        tracing::info!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(&request_key),
            "opening walletconnect request dialog"
        );
        self.open_walletconnect_request_dialog(request_key, window, cx);
    }

    pub(in crate::root) fn open_walletconnect_pending_request_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(request_key) =
            first_walletconnect_pending_request_key(&self.walletconnect.pending_requests)
        else {
            return;
        };
        self.open_walletconnect_request_dialog(request_key, window, cx);
    }

    pub(in crate::root::walletconnect) fn clear_walletconnect_request_dialog_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let current_key = self.walletconnect.request_dialog_key.clone();
        if let Some(current_key) = current_key.as_deref() {
            self.walletconnect.dismiss_request_dialog(current_key);
            self.walletconnect
                .completed_request_dialogs
                .remove(current_key);
        }
        self.discard_walletconnect_fee_state(window, cx);
        self.clear_trezor_app_passphrase_input(window, cx);
        self.stop_walletconnect_request_dialog_refresh();
        self.walletconnect.request_dialog_open = false;
        self.walletconnect.request_dialog_key = None;
    }

    pub(in crate::root::walletconnect) fn close_walletconnect_request_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.clear_walletconnect_request_dialog_state(window, cx);
        window.close_dialog(cx);
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn open_walletconnect_request_dialog(
        &mut self,
        request_key: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .walletconnect
            .pending_requests
            .contains_key(&request_key)
            || window.has_active_dialog(cx)
        {
            return;
        }
        let request_key = Arc::<str>::from(request_key);
        self.walletconnect.request_dialog_open = true;
        self.walletconnect.request_dialog_deferred_logged = false;
        self.walletconnect.request_dialog_key = Some(Arc::clone(&request_key));
        self.attach_walletconnect_fee_state(request_key.as_ref(), window, cx);
        self.start_walletconnect_request_dialog_refresh(cx);
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(620.0));
        let dialog_max_height = (window.viewport_size().height * 0.88).min(px(820.0));
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            let footer_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Dapp request"))
                // A footer otherwise gives Enter a default confirm-and-close path.
                .on_ok(|_, _, _| false)
                .footer(
                    gpui_component::dialog::DialogFooter::new().children(
                        footer_root
                            .read(cx)
                            .render_walletconnect_request_footer(&footer_root),
                    ),
                )
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.clear_walletconnect_request_dialog_state(window, cx);
                        cx.notify();
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_walletconnect_request_dialog_content(&content_root, content_width),
                )
        });
        cx.defer_in(window, |root, window, cx| {
            root.walletconnect.request_dialog_focus.focus(window, cx);
        });
        cx.notify();
    }

    fn start_walletconnect_request_dialog_refresh(&mut self, cx: &Context<'_, Self>) {
        if self.walletconnect.request_dialog_refresh_active {
            return;
        }
        self.walletconnect.request_dialog_refresh_active = true;
        self.walletconnect.request_dialog_refresh_generation = self
            .walletconnect
            .request_dialog_refresh_generation
            .wrapping_add(1);
        let generation = self.walletconnect.request_dialog_refresh_generation;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let keep_running = this
                    .update(cx, |root, cx| {
                        if root.walletconnect.request_dialog_refresh_generation != generation
                            || !root.walletconnect.request_dialog_open
                            || root
                                .walletconnect
                                .request_dialog_key
                                .as_deref()
                                .is_none_or(|key| {
                                    !root.walletconnect.pending_requests.contains_key(key)
                                        && !root
                                            .walletconnect
                                            .completed_request_dialogs
                                            .contains_key(key)
                                })
                        {
                            root.walletconnect.request_dialog_refresh_active = false;
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_running {
                    return;
                }
            }
        })
        .detach();
    }

    pub(in crate::root::walletconnect::root) const fn stop_walletconnect_request_dialog_refresh(
        &mut self,
    ) {
        self.walletconnect.request_dialog_refresh_active = false;
        self.walletconnect.request_dialog_refresh_generation = self
            .walletconnect
            .request_dialog_refresh_generation
            .wrapping_add(1);
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_request_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
    ) -> gpui::Stateful<gpui::Div> {
        let keyboard_root = root.clone();
        let focus_handle = self.walletconnect.request_dialog_focus.clone();
        let mut content = div()
            .id("walletconnect-request-content")
            .role(gpui::accesskit::Role::Group)
            .aria_label("WalletConnect request")
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .track_focus(&focus_handle.tab_stop(true))
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                let target_key = keyboard_root
                    .read(cx)
                    .walletconnect
                    .request_dialog_key
                    .as_ref()
                    .and_then(|request_key| {
                        walletconnect_request_dialog_nav(
                            &keyboard_root.read(cx).walletconnect.pending_requests,
                            request_key,
                        )
                    })
                    .and_then(|nav| match event.keystroke.key.as_str() {
                        "left" => nav.previous_key,
                        "right" => nav.next_key,
                        _ => None,
                    });
                let Some(target_key) = target_key else {
                    return;
                };
                keyboard_root.update(cx, |root, cx| {
                    root.navigate_walletconnect_request_dialog(&target_key, window, cx);
                });
                cx.stop_propagation();
            });
        let Some(request_key) = self.walletconnect.request_dialog_key.as_deref() else {
            return content.child(app_muted_text(
                "This dapp request was already resolved or is no longer available.",
            ));
        };
        if let Some(completed) = self
            .walletconnect
            .completed_request_dialogs
            .get(request_key)
        {
            return content.child(self.render_walletconnect_completed_request(root, completed));
        }
        if let Some(nav) =
            walletconnect_request_dialog_nav(&self.walletconnect.pending_requests, request_key)
            && nav.total > 1
        {
            content = content.child(Self::render_walletconnect_request_dialog_nav(root, &nav));
        }
        match self.walletconnect.pending_requests.get(request_key) {
            Some(request) => {
                content.child(self.render_walletconnect_request(root, request, content_width))
            }
            None => content.child(app_muted_text(
                "This dapp request was already resolved or is no longer available.",
            )),
        }
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_request_dialog_nav(
        root: &Entity<Self>,
        nav: &WalletConnectRequestDialogNav,
    ) -> gpui::Div {
        let previous_key = nav.previous_key.clone();
        let next_key = nav.next_key.clone();
        let previous_root = root.clone();
        let next_root = root.clone();
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                app_button_base("walletconnect-request-previous")
                    .accessibility_label("Previous WalletConnect request")
                    .icon(IconName::ArrowLeft)
                    .outline()
                    .small()
                    .disabled(previous_key.is_none())
                    .on_click(move |_event, window, cx| {
                        let Some(key) = previous_key.clone() else {
                            return;
                        };
                        previous_root.update(cx, |root, cx| {
                            root.navigate_walletconnect_request_dialog(&key, window, cx);
                        });
                    }),
            )
            .child(
                app_strong_text(format!("{} out of {}", nav.index, nav.total))
                    .text_size(px(13.0))
                    .text_color(rgb(theme::TEXT)),
            )
            .child(
                app_button_base("walletconnect-request-next")
                    .accessibility_label("Next WalletConnect request")
                    .icon(IconName::ArrowRight)
                    .outline()
                    .small()
                    .disabled(next_key.is_none())
                    .on_click(move |_event, window, cx| {
                        let Some(key) = next_key.clone() else {
                            return;
                        };
                        next_root.update(cx, |root, cx| {
                            root.navigate_walletconnect_request_dialog(&key, window, cx);
                        });
                    }),
            )
    }

    pub(in crate::root::walletconnect) fn navigate_walletconnect_request_dialog(
        &mut self,
        request_key: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .walletconnect
            .pending_requests
            .contains_key(request_key)
        {
            return;
        }
        let current_key = self.walletconnect.request_dialog_key.clone();
        if let Some(current_key) = current_key.as_deref()
            && current_key != request_key
        {
            self.walletconnect.dismiss_request_dialog(current_key);
        }
        self.walletconnect.request_dialog_key = Some(Arc::from(request_key));
        self.walletconnect.request_dialog_open = true;
        self.walletconnect.request_dialog_deferred_logged = false;
        self.attach_walletconnect_fee_state(request_key, window, cx);
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_completed_request(
        &self,
        root: &Entity<Self>,
        completed: &WalletConnectCompletedRequestUi,
    ) -> gpui::Div {
        let close_root = root.clone();
        let next_root = root.clone();
        let current_key = Arc::<str>::from(completed.request.key.as_str());
        let next_key =
            first_walletconnect_pending_request_key(&self.walletconnect.pending_requests);
        let status_alert = match completed.status {
            WalletConnectCompletedRequestStatus::Approved
            | WalletConnectCompletedRequestStatus::TransactionSubmitted => Alert::success(
                SharedString::from(format!(
                    "walletconnect-request-completed-{}",
                    completed.request.key
                )),
                completed.message.to_string(),
            ),
            WalletConnectCompletedRequestStatus::AuthorizationFailed
            | WalletConnectCompletedRequestStatus::RequestFailed
            | WalletConnectCompletedRequestStatus::Expired => Alert::error(
                SharedString::from(format!(
                    "walletconnect-request-completed-{}",
                    completed.request.key
                )),
                completed.message.to_string(),
            ),
            WalletConnectCompletedRequestStatus::RelayResponseFailed
            | WalletConnectCompletedRequestStatus::TransactionSubmittedRelayResponseFailed => {
                Alert::warning(
                    SharedString::from(format!(
                        "walletconnect-request-completed-{}",
                        completed.request.key
                    )),
                    completed.message.to_string(),
                )
            }
        }
        .small();
        let mut card = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .bg(rgb(theme::SURFACE_ELEVATED))
            .p(px(10.0))
            .child(status_alert)
            .child(walletconnect_kv_element_row(
                "Dapp",
                app_strong_text(completed.request.item.dapp_name.clone()),
            ))
            .child(walletconnect_kv_row(
                "Method",
                completed.request.item.method.as_str().to_owned(),
            ))
            .child(walletconnect_kv_element_row(
                "Chain",
                walletconnect_approved_chain_chip(&approved_chain_display_item(
                    &completed.request.item.chain_id,
                )),
            ))
            .child(walletconnect_kv_row(
                "Public account",
                short_address(&completed.request.item.account),
            ));
        if let Some(tx_hash) = completed.submitted_tx_hash.as_ref() {
            card = card.child(walletconnect_completed_tx_hash_row(
                &completed.request.key,
                tx_hash,
            ));
        }
        if let Some(error) = completed.error.as_ref() {
            card = card.child(
                Alert::error("walletconnect-request-result-error", error.to_string()).small(),
            );
        }
        card.child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .when_some(next_key, |this, next_key| {
                    this.child(
                        app_button("walletconnect-request-review-next", "Review next")
                            .outline()
                            .small()
                            .on_click(move |_event, window, cx| {
                                let next_key = next_key.clone();
                                let current_key = Arc::clone(&current_key);
                                next_root.update(cx, |root, cx| {
                                    root.walletconnect
                                        .completed_request_dialogs
                                        .remove(current_key.as_ref());
                                    root.walletconnect.request_dialog_key =
                                        Some(Arc::from(next_key.as_str()));
                                    root.walletconnect.request_dialog_open = true;
                                    root.walletconnect.request_dialog_deferred_logged = false;
                                    root.walletconnect.error = None;
                                    root.attach_walletconnect_fee_state(&next_key, window, cx);
                                    cx.notify();
                                });
                            }),
                    )
                })
                .child(
                    app_button("walletconnect-request-result-close", "Close")
                        .primary()
                        .small()
                        .on_click(move |_event, window, cx| {
                            close_root.update(cx, |root, cx| {
                                root.close_walletconnect_request_dialog(window, cx);
                            });
                        }),
                ),
        )
    }
}
