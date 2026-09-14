use super::*;

impl WalletRoot {
    pub(in crate::root) fn open_walletconnect_sessions_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.walletconnect.status = None;
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(640.0));
        let dialog_max_height = (window.viewport_size().height * 0.88).min(px(820.0));
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.close_all_dialogs(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(walletconnect_title_row("WalletConnect sessions"))
                .child(
                    content_root
                        .read(cx)
                        .render_walletconnect_sessions_dialog_content(
                            &content_root,
                            content_width,
                            content_max_height,
                        ),
                )
        });
        cx.notify();
    }

    pub(in crate::root) fn open_walletconnect_account_sessions_dialog(
        &mut self,
        public_account_uuid: Arc<str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self.public_account_for_uuid(Some(public_account_uuid.as_ref())) else {
            self.walletconnect.error = Some(Arc::from("Public account is no longer available"));
            cx.notify();
            return;
        };
        let account_label = public_account_walletconnect_label(account);
        self.walletconnect.status = None;

        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(640.0));
        let dialog_max_height = (window.viewport_size().height * 0.88).min(px(820.0));
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.close_all_dialogs(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let content_root = root.clone();
            let account_uuid = Arc::clone(&public_account_uuid);
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(walletconnect_title_row("WalletConnect sessions"))
                .child(
                    content_root
                        .read(cx)
                        .render_walletconnect_account_sessions_dialog_content(
                            &content_root,
                            account_uuid.as_ref(),
                            account_label.clone(),
                            content_width,
                            content_max_height,
                        ),
                )
        });
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_sessions_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
        content_height: Pixels,
    ) -> gpui::Div {
        let new_session_root = root.clone();
        let refresh_root = root.clone();
        let can_create_session = self.view_session.is_some()
            && self.vault_store.is_some()
            && self.has_active_public_accounts()
            && !self.walletconnect.pairing_in_progress;
        let can_refresh = self.view_session.is_some() && self.vault_store.is_some();
        let sessions = self
            .walletconnect
            .sessions
            .iter()
            .filter(|session| walletconnect_session_visible_in_management(session))
            .collect::<Vec<_>>();
        let session_count = sessions.len();
        let mut header = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(app_muted_text(format!(
                "{} dapp session{}.",
                session_count,
                if session_count == 1 { "" } else { "s" }
            )));
        if let Some(error) = self.walletconnect.error.as_ref() {
            header = header
                .child(Alert::error("walletconnect-sessions-error", error.to_string()).small());
        }
        if self.walletconnect.status.as_deref() == Some(WALLETCONNECT_REFRESHED_STATUS) {
            header = header.child(
                Alert::info(
                    "walletconnect-sessions-status",
                    WALLETCONNECT_REFRESHED_STATUS,
                )
                .small(),
            );
        }

        let body_height = content_height.min(px(620.0));
        let mut session_items = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .pr(px(6.0))
            .pb(px(6.0));
        if sessions.is_empty() {
            session_items = session_items.child(
                Alert::info(
                    "walletconnect-sessions-empty",
                    "No WalletConnect sessions are connected.",
                )
                .small(),
            );
        } else {
            for session in sessions {
                session_items =
                    session_items.child(self.render_walletconnect_session(root, session, true));
            }
        }

        let session_list = div()
            .w_full()
            .min_h(px(0.0))
            .flex()
            .flex_1()
            .overflow_y_scrollbar()
            .child(session_items);

        div()
            .w(content_width)
            .h(body_height)
            .max_h(body_height)
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap_3()
            .overflow_hidden()
            .child(header.flex_none())
            .child(
                app_strong_text("Sessions")
                    .text_size(px(13.0))
                    .text_color(rgb(theme::TEXT))
                    .flex_none(),
            )
            .child(session_list)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("walletconnect-global-refresh", "Refresh")
                            .outline()
                            .small()
                            .disabled(!can_refresh)
                            .on_click(move |_event, _window, cx| {
                                refresh_root.update(cx, |root, cx| {
                                    root.refresh_walletconnect_sessions_from_ui(cx);
                                });
                            }),
                    )
                    .child(
                        app_button(
                            "walletconnect-global-new-session",
                            if self.walletconnect.pairing_in_progress {
                                "Connecting..."
                            } else {
                                "New session"
                            },
                        )
                        .primary()
                        .small()
                        .loading(self.walletconnect.pairing_in_progress)
                        .disabled(!can_create_session)
                        .on_click(move |_event, window, cx| {
                            new_session_root.update(cx, |root, cx| {
                                root.open_walletconnect_connection_dialog_for_default_account(
                                    window, cx,
                                );
                            });
                        }),
                    ),
            )
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_account_sessions_dialog_content(
        &self,
        root: &Entity<Self>,
        public_account_uuid: &str,
        account_label: String,
        content_width: Pixels,
        content_height: Pixels,
    ) -> gpui::Div {
        let new_session_root = root.clone();
        let refresh_root = root.clone();
        let new_session_uuid = Arc::<str>::from(public_account_uuid);
        let account_is_active = self
            .public_account_for_uuid(Some(public_account_uuid))
            .is_some_and(|account| account.status == PublicAccountStatus::Active);
        let can_create_session = self.view_session.is_some()
            && self.vault_store.is_some()
            && account_is_active
            && !self.walletconnect.pairing_in_progress;
        let can_refresh = self.view_session.is_some() && self.vault_store.is_some();
        let sessions = self
            .walletconnect
            .sessions
            .iter()
            .filter(|session| {
                session.selected_public_account_uuid == public_account_uuid
                    && walletconnect_session_visible_in_management(session)
            })
            .collect::<Vec<_>>();
        let session_count = sessions.len();
        let mut header = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(app_muted_text(format!(
                "{} dapp session{} for this Public account.",
                session_count,
                if session_count == 1 { "" } else { "s" }
            )))
            .child(walletconnect_kv_row("Public account", account_label));
        if let Some(error) = self.walletconnect.error.as_ref() {
            header = header.child(
                Alert::error("walletconnect-account-sessions-error", error.to_string()).small(),
            );
        }
        if self.walletconnect.status.as_deref() == Some(WALLETCONNECT_REFRESHED_STATUS) {
            header = header.child(
                Alert::info(
                    "walletconnect-account-sessions-status",
                    WALLETCONNECT_REFRESHED_STATUS,
                )
                .small(),
            );
        }

        let body_height = content_height.min(px(620.0));
        let mut session_items = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .pr(px(6.0))
            .pb(px(6.0));
        if sessions.is_empty() {
            session_items = session_items.child(
                Alert::info(
                    "walletconnect-account-sessions-empty",
                    "No WalletConnect sessions are connected to this account.",
                )
                .small(),
            );
        } else {
            for session in sessions {
                session_items =
                    session_items.child(self.render_walletconnect_session(root, session, false));
            }
        }

        let session_list = div()
            .w_full()
            .min_h(px(0.0))
            .flex()
            .flex_1()
            .overflow_y_scrollbar()
            .child(session_items);

        div()
            .w(content_width)
            .h(body_height)
            .max_h(body_height)
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap_3()
            .overflow_hidden()
            .child(header.flex_none())
            .child(
                app_strong_text("Sessions")
                    .text_size(px(13.0))
                    .text_color(rgb(theme::TEXT))
                    .flex_none(),
            )
            .child(session_list)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("walletconnect-account-refresh", "Refresh")
                            .outline()
                            .small()
                            .disabled(!can_refresh)
                            .on_click(move |_event, _window, cx| {
                                refresh_root.update(cx, |root, cx| {
                                    root.refresh_walletconnect_sessions_from_ui(cx);
                                });
                            }),
                    )
                    .child(
                        app_button(
                            "walletconnect-account-new-session",
                            if self.walletconnect.pairing_in_progress {
                                "Connecting..."
                            } else {
                                "New session"
                            },
                        )
                        .primary()
                        .small()
                        .loading(self.walletconnect.pairing_in_progress)
                        .disabled(!can_create_session)
                        .on_click(move |_event, window, cx| {
                            let account_uuid = Arc::clone(&new_session_uuid);
                            new_session_root.update(cx, |root, cx| {
                                root.open_walletconnect_connection_dialog(account_uuid, window, cx);
                            });
                        }),
                    ),
            )
    }

    pub(in crate::root::walletconnect) fn refresh_walletconnect_sessions_from_ui(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        tracing::info!(
            target: "wallet::root::walletconnect",
            "manual walletconnect refresh requested"
        );
        self.walletconnect.error = None;
        self.walletconnect.relay_reconnecting = false;
        self.reload_walletconnect_sessions(cx);
        if self.walletconnect.error.is_none() {
            self.walletconnect.status = Some(Arc::from(WALLETCONNECT_REFRESHED_STATUS));
            cx.notify();
        }
    }

    pub(in crate::root) fn reload_walletconnect_sessions(&mut self, cx: &mut Context<'_, Self>) {
        let (Some(store), Some(view_session)) =
            (self.vault_store.as_ref(), self.view_session.as_ref())
        else {
            tracing::debug!(
                target: "wallet::root::walletconnect",
                "walletconnect sessions unavailable; wallet is locked or vault store is missing"
            );
            self.walletconnect.sessions.clear();
            self.walletconnect.approval_handoff_sessions.clear();
            self.walletconnect.invalidate_gateway_pending_requests();
            self.walletconnect.pending_requests.clear();
            self.walletconnect.request_routes.clear();
            self.walletconnect.request_disclosure_states.clear();
            self.walletconnect.dismissed_request_dialog_keys.clear();
            self.walletconnect.request_dialog_refresh_active = false;
            self.walletconnect.request_dialog_refresh_generation = self
                .walletconnect
                .request_dialog_refresh_generation
                .wrapping_add(1);
            self.walletconnect.subscriptions.clear();
            self.walletconnect.request_expiry_timer_active = false;
            self.walletconnect.request_expiry_generation =
                self.walletconnect.request_expiry_generation.wrapping_add(1);
            self.walletconnect.session_expiry_timer_active = false;
            self.walletconnect.session_expiry_generation =
                self.walletconnect.session_expiry_generation.wrapping_add(1);
            self.walletconnect.session_expiry_deadline = None;
            self.walletconnect.relay_reconnecting = false;
            self.stop_walletconnect_relay_workers_except(&BTreeSet::new());
            self.sync_walletconnect_attention();
            return;
        };
        match store.list_walletconnect_sessions(view_session.as_ref()) {
            Ok(sessions) => {
                let now = current_unix_seconds();
                self.walletconnect.sessions = sessions
                    .into_iter()
                    .map(|session| {
                        let mut session = store
                            .reconcile_walletconnect_session_account_state(
                                view_session.as_ref(),
                                &session.session_uuid,
                            )
                            .unwrap_or(session);
                        if session.lifecycle_state == WalletConnectSessionLifecycleState::Active
                            && walletconnect_session_expired(&session, now)
                        {
                            session.lifecycle_state = WalletConnectSessionLifecycleState::Expired;
                            if let Err(error) = store
                                .update_walletconnect_session(view_session.as_ref(), &session)
                            {
                                tracing::warn!(
                                    target: "wallet::root::walletconnect",
                                    session_uuid = %walletconnect_request_key_log_label(&session.session_uuid),
                                    error = %error,
                                    "could not persist expired walletconnect session state"
                                );
                            }
                        }
                        session
                    })
                    .collect();
                tracing::debug!(
                    target: "wallet::root::walletconnect",
                    session_count = self.walletconnect.sessions.len(),
                    active_session_count = self
                        .walletconnect
                        .sessions
                        .iter()
                        .filter(|session| session.lifecycle_state
                            == WalletConnectSessionLifecycleState::Active)
                        .count(),
                    "loaded walletconnect sessions"
                );
                self.log_walletconnect_restored_session_topics();
                self.ensure_walletconnect_relay_processing(cx);
            }
            Err(error) => {
                tracing::warn!(
                    target: "wallet::root::walletconnect",
                    %error,
                    "could not load walletconnect sessions"
                );
                self.walletconnect.error = Some(Arc::from(format!(
                    "Could not load WalletConnect sessions: {error}"
                )));
            }
        }
        self.sync_walletconnect_attention();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn log_walletconnect_restored_session_topics(&self) {
        for session in &self.walletconnect.sessions {
            tracing::debug!(
                target: "wallet::root::walletconnect",
                session_uuid = %walletconnect_request_key_log_label(&session.session_uuid),
                pairing_topic = %walletconnect_topic_log_label(&session.pairing_topic),
                session_topic = %walletconnect_topic_log_label(&session.session_topic),
                lifecycle_state = ?session.lifecycle_state,
                dapp = session.peer_metadata.name.as_str(),
                "restored walletconnect session topics"
            );
        }
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_session(
        &self,
        root: &Entity<Self>,
        session: &WalletConnectSessionRecord,
        include_public_account: bool,
    ) -> gpui::Div {
        let disconnect_root = root.clone();
        let session_uuid = Arc::<str>::from(session.session_uuid.as_str());
        let disconnecting = self
            .walletconnect
            .disconnecting_sessions
            .contains(session.session_uuid.as_str());
        let selected_account = self
            .public_account_for_uuid(Some(&session.selected_public_account_uuid))
            .map_or_else(
                || walletconnect_unresolved_public_account_label(session),
                public_account_walletconnect_label,
            );
        let card = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER_SUBTLE))
            .bg(rgb(theme::SURFACE_ELEVATED))
            .p(px(10.0))
            .child(walletconnect_kv_element_row(
                "Dapp",
                app_strong_text(session.peer_metadata.name.clone()),
            ))
            .child(walletconnect_kv_row(
                "URL",
                session.peer_metadata.url.clone(),
            ))
            .when(include_public_account, |this| {
                this.child(walletconnect_kv_row("Public account", selected_account))
            });
        card.child(walletconnect_approved_chains_row(session))
            .child(walletconnect_kv_row(
                "Expires",
                format_unix_seconds(session.expiry_timestamp),
            ))
            .when(
                session.lifecycle_state != WalletConnectSessionLifecycleState::Active,
                |this| {
                    this.child(walletconnect_kv_row(
                        "State",
                        walletconnect_lifecycle_label(session.lifecycle_state),
                    ))
                },
            )
            .child(
                div().flex().justify_end().child(
                    app_button(
                        SharedString::from(format!(
                            "walletconnect-disconnect-{}",
                            session.session_uuid
                        )),
                        if disconnecting {
                            "Disconnecting..."
                        } else {
                            "Disconnect"
                        },
                    )
                    .outline()
                    .small()
                    .loading(disconnecting)
                    .disabled(disconnecting)
                    .on_click(move |_event, _window, cx| {
                        let session_uuid = Arc::clone(&session_uuid);
                        disconnect_root.update(cx, |root, cx| {
                            root.disconnect_walletconnect_session(session_uuid.as_ref(), cx);
                        });
                    }),
                ),
            )
    }

    pub(in crate::root::walletconnect) fn disconnect_walletconnect_session(
        &mut self,
        session_uuid: &str,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .walletconnect
            .disconnecting_sessions
            .contains(session_uuid)
        {
            return;
        }
        let Some(session) = self
            .walletconnect
            .sessions
            .iter()
            .find(|session| session.session_uuid == session_uuid)
            .cloned()
        else {
            return;
        };
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let context = match self.walletconnect_client_context_for_session(&session, cx) {
            Ok(context) => context,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let subscription_id = self
            .walletconnect
            .subscriptions
            .get(&session.session_topic)
            .map(String::as_str);
        let plan = match build_walletconnect_disconnect_plan(
            &session,
            walletconnect_request_id_seed(),
            subscription_id,
        ) {
            Ok(plan) => plan,
            Err(error) => {
                self.walletconnect.error = Some(Arc::from(walletconnect_error_message(&error)));
                cx.notify();
                return;
            }
        };
        self.walletconnect
            .disconnecting_sessions
            .insert(session_uuid.to_owned());
        let topic = session.session_topic.clone();
        tracing::info!(
            target: "wallet::root::walletconnect",
            session_uuid = %walletconnect_request_key_log_label(session_uuid),
            session_topic = %walletconnect_topic_log_label(&topic),
            "disconnecting walletconnect session"
        );
        let session_uuid = session_uuid.to_owned();
        let join = self.runtime.spawn(async move {
            execute_walletconnect_relay_steps(&context.worker, plan.relay_steps).await
        });
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |root, cx| {
                root.walletconnect
                    .disconnecting_sessions
                    .remove(&session_uuid);
                root.walletconnect.subscriptions.remove(&topic);
                root.walletconnect.retain_pending_requests(|_, request| {
                    request.session_identity.walletconnect_session_id()
                        != Some(session_uuid.as_str())
                });
                if let Err(error) = store.delete_walletconnect_session(&session_uuid) {
                    root.walletconnect.error = Some(Arc::from(format!(
                        "Could not remove WalletConnect session: {error}"
                    )));
                } else {
                    root.walletconnect.status =
                        Some(Arc::from("WalletConnect session disconnected."));
                }
                if let Ok(Err(error)) = result {
                    tracing::warn!(
                        target: "wallet::root::walletconnect",
                        session_uuid = %walletconnect_request_key_log_label(&session_uuid),
                        error = %error,
                        "walletconnect relay disconnect failed after local removal"
                    );
                    root.walletconnect.error = Some(Arc::from(format!(
                        "Session was removed locally, but relay disconnect failed: {error}"
                    )));
                }
                root.reload_walletconnect_sessions(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
