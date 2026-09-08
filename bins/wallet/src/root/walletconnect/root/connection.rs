use super::*;

impl WalletRoot {
    pub(in crate::root) fn open_walletconnect_connection_dialog(
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
        if account.status != PublicAccountStatus::Active {
            self.walletconnect.error = Some(Arc::from(
                "Activate this Public account before using WalletConnect",
            ));
            cx.notify();
            return;
        }

        self.walletconnect.selected_account_uuid = Some(public_account_uuid);
        self.walletconnect.connection_dialog_open = true;
        self.walletconnect.status = None;
        self.walletconnect.error = None;
        self.sync_walletconnect_account_select(window, cx);

        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(640.0));
        let dialog_max_height = (window.viewport_size().height * 0.88).min(px(820.0));
        let content_width = secondary_dialog_content_width(dialog_width);
        window.close_all_dialogs(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height)
                .title(walletconnect_title_row("WalletConnect"))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.walletconnect.connection_dialog_open = false;
                        cx.notify();
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_walletconnect_connection_dialog_content(
                            &content_root,
                            content_width,
                        ),
                )
        });
        cx.defer_in(window, |root, window, cx| {
            root.walletconnect
                .uri_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        });
        cx.notify();
    }

    pub(in crate::root) fn open_walletconnect_connection_dialog_for_default_account(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let public_account_uuid = self
            .selected_public_account()
            .filter(|account| account.status == PublicAccountStatus::Active)
            .or_else(|| {
                self.public_accounts
                    .iter()
                    .find(|account| account.status == PublicAccountStatus::Active)
            })
            .map(|account| Arc::from(account.public_account_uuid.as_str()));
        let Some(public_account_uuid) = public_account_uuid else {
            self.walletconnect.error = Some(Arc::from(
                "Add or activate a Public account before connecting a dapp with WalletConnect.",
            ));
            cx.notify();
            return;
        };
        self.open_walletconnect_connection_dialog(public_account_uuid, window, cx);
    }

    pub(in crate::root) fn set_walletconnect_selected_account(
        &mut self,
        public_account_uuid: Arc<str>,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .public_account_for_uuid(Some(public_account_uuid.as_ref()))
            .is_some_and(|account| account.status == PublicAccountStatus::Active)
        {
            self.walletconnect.selected_account_uuid = Some(public_account_uuid);
            self.walletconnect.error = None;
            cx.notify();
        }
    }

    pub(in crate::root) fn sync_walletconnect_account_select(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.active_walletconnect_public_accounts();
        let selected = normalized_walletconnect_account_uuid(
            self.walletconnect.selected_account_uuid.as_ref(),
            &accounts,
        );
        self.walletconnect
            .selected_account_uuid
            .clone_from(&selected);
        sync_walletconnect_account_select_entity(
            &self.walletconnect.account_select,
            &accounts,
            self.public_balance_snapshot.as_deref(),
            self.selected_chain,
            Some(self.public_broadcaster_anchor_cache.as_ref()),
            selected.as_ref(),
            window,
            cx,
        );
    }

    pub(in crate::root::walletconnect) fn active_walletconnect_public_accounts(
        &self,
    ) -> Vec<PublicAccountMetadata> {
        self.public_accounts
            .iter()
            .filter(|account| account.status == PublicAccountStatus::Active)
            .cloned()
            .collect()
    }

    pub(in crate::root::walletconnect) fn selected_walletconnect_public_account(
        &self,
    ) -> Option<&PublicAccountMetadata> {
        self.public_account_for_uuid(
            self.walletconnect
                .selected_account_uuid
                .as_ref()
                .map(AsRef::as_ref),
        )
        .filter(|account| account.status == PublicAccountStatus::Active)
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_connection_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
    ) -> gpui::Div {
        let connect_root = root.clone();
        let uri_input = app_input(&self.walletconnect.uri_input).small();
        let selected_account = self.selected_walletconnect_public_account();
        let can_connect = self.view_session.is_some()
            && self.vault_store.is_some()
            && selected_account.is_some()
            && !self.walletconnect.pairing_in_progress;
        let mut content = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .child(app_muted_text(
                "Choose the Public account to expose, then paste a copied wc: URI from a dapp before approving the session.",
            ).whitespace_normal())
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(app_muted_text("Account"))
                    .child(
                        Select::new(&self.walletconnect.account_select)
                            .small()
                            .w_full()
                            .placeholder("Select active Public account")
                            .menu_width(px(500.0)),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w(px(0.0)).child(uri_input))
                    .child(
                        app_button(
                            "walletconnect-modal-connect",
                            if self.walletconnect.pairing_in_progress {
                                "Connecting..."
                            } else {
                                "Connect"
                            },
                        )
                        .primary()
                        .small()
                        .loading(self.walletconnect.pairing_in_progress)
                        .disabled(!can_connect)
                        .on_click(move |_event, window, cx| {
                            connect_root.update(cx, |root, cx| {
                                root.start_walletconnect_pairing_from_input(window, cx);
                            });
                        }),
                    ),
            );
        if selected_account.is_none() {
            content = content.child(
                Alert::warning(
                    "walletconnect-modal-no-public-account",
                    "Select an active Public account before connecting a dapp.",
                )
                .small(),
            );
        }
        if let Some(status) = self.walletconnect.status.as_ref() {
            content = content
                .child(Alert::info("walletconnect-modal-status", status.to_string()).small());
        }
        if let Some(error) = self.walletconnect.error.as_ref() {
            content =
                content.child(Alert::error("walletconnect-modal-error", error.to_string()).small());
        }
        if let Some(proposal) = self.walletconnect.pending_proposal.as_ref() {
            content = content.child(self.render_walletconnect_proposal(root, proposal));
        }
        content
    }
}
