#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn update_selected_public_account_label(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        let Some(account_uuid) = self
            .public_form
            .editing_account_uuid
            .clone()
            .or_else(|| self.listed_selected_uuid().cloned())
        else {
            self.public_form.error = Some(Arc::from("Select a public account first"));
            cx.notify();
            return;
        };
        let label = self
            .public_form
            .edit_label_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if label.is_empty() {
            self.public_form.error = Some(Arc::from("Enter an account label"));
            cx.notify();
            return;
        }
        let result = if self.tron_selected() {
            store
                .update_tron_account_label(
                    view_session.as_ref(),
                    account_uuid.as_ref(),
                    Some(&label),
                )
                .map(drop)
        } else {
            store
                .update_public_account_label(
                    view_session.as_ref(),
                    account_uuid.as_ref(),
                    Some(&label),
                )
                .map(drop)
        };
        match result {
            Ok(()) => {
                self.public_form.editing_account_uuid = None;
                self.reload_public_accounts(window, cx);
                window.close_all_dialogs(cx);
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    pub(in crate::root) fn deactivate_public_account(
        &mut self,
        public_account_uuid: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        if self.tron_selected() {
            self.set_tron_account_active(public_account_uuid, false, window, cx);
            return;
        }
        let neighbour = self.public_account_neighbour(public_account_uuid);
        match store
            .deactivate_derived_public_account(view_session.as_ref(), public_account_uuid.as_ref())
        {
            Ok(_) => {
                if self.public_form.selected_account_uuid.as_deref() == Some(public_account_uuid) {
                    self.public_form.selected_account_uuid = neighbour;
                }
                self.reload_public_accounts(window, cx);
                self.reset_public_asset_focus();
                self.scroll_selected_public_row_into_view(window);
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    pub(in crate::root) fn activate_public_account(
        &mut self,
        public_account_uuid: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        if self.tron_selected() {
            self.set_tron_account_active(public_account_uuid, true, window, cx);
            return;
        }
        match store
            .activate_derived_public_account(view_session.as_ref(), public_account_uuid.as_ref())
        {
            Ok(account) => {
                self.public_form.selected_account_uuid =
                    Some(Arc::from(account.public_account_uuid.as_str()));
                self.reload_public_accounts(window, cx);
                self.schedule_account_public_balance_refresh(
                    self.last_evm_chain,
                    [account.public_account_uuid],
                    None,
                    cx,
                );
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    pub(in crate::root) fn delete_public_account(
        &mut self,
        public_account_uuid: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self
            .listed_account_for_uuid(Some(public_account_uuid))
            .cloned()
        else {
            return;
        };
        if account.is_global() {
            let root = cx.entity();
            let account_uuid = account.public_account_uuid.clone();
            let label = public_account_display_label(&account)
                .unwrap_or_else(|| self.listed_short_address(&account));
            let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
            let dialog_max_height = dialog_max_height(window);
            window.open_wallet_alert_dialog(cx, move |dialog, _window, _cx| {
                let confirm_root = root.clone();
                let account_uuid = account_uuid.clone();
                confirmation_dialog(
                    dialog,
                    ConfirmationDialogProps::danger(
                        "Delete global account?",
                        "Deleting this global account removes it from every Private wallet.",
                        None,
                        "Delete account",
                    ),
                    dialog_width,
                    dialog_max_height,
                )
                .child(app_strong_text(label.clone()).whitespace_normal())
                .on_ok(move |_event, window, cx| {
                    confirm_root.update(cx, |root, cx| {
                        root.delete_public_account_confirmed(&account_uuid, window, cx);
                    });
                    true
                })
            });
        } else {
            self.delete_public_account_confirmed(public_account_uuid, window, cx);
        }
    }

    fn delete_public_account_confirmed(
        &mut self,
        public_account_uuid: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self
            .listed_account_for_uuid(Some(public_account_uuid))
            .cloned()
        else {
            return;
        };
        let Some(store) = self.vault_store.clone() else {
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        let neighbour = self.public_account_neighbour(public_account_uuid);
        let account_is_global = account.is_global();
        let owning_wallet_uuid = view_session.wallet_id().to_owned();
        let result = if self.tron_selected() {
            store
                .delete_imported_tron_account(view_session.as_ref(), &account.public_account_uuid)
                .map(drop)
        } else {
            store
                .delete_imported_public_account(view_session.as_ref(), &account.public_account_uuid)
                .map(drop)
        };
        match result {
            Ok(()) => {
                let participation_changed = if account_is_global {
                    remove_global_participant(
                        &mut self.ui_state.governance_participants,
                        &account.public_account_uuid,
                    )
                } else {
                    remove_scoped_participant(
                        &mut self.ui_state.governance_participants,
                        &owning_wallet_uuid,
                        &account.public_account_uuid,
                    )
                };
                let exclusion_changed = self
                    .ui_state
                    .public_refresh_excluded
                    .remove(&account.public_account_uuid);
                if participation_changed || exclusion_changed {
                    self.save_ui_state();
                }
                if participation_changed {
                    self.invalidate_governance_context();
                }
                if self.listed_selected_uuid().map(AsRef::as_ref) == Some(public_account_uuid) {
                    self.set_listed_selected_uuid(neighbour);
                }
                self.reload_public_accounts(window, cx);
                self.reset_public_asset_focus();
                self.scroll_selected_public_row_into_view(window);
            }
            Err(error) => self.public_form.error = Some(Arc::from(error.to_string())),
        }
        cx.notify();
    }

    pub(in crate::root) fn public_account_visible_balances(
        &self,
        public_account_uuid: &str,
        status: PublicAccountStatus,
    ) -> Vec<PublicBalanceEntry> {
        public_account_visible_balances_for_chain(
            self.listed_public_snapshot(),
            self.listed_public_chain(),
            public_account_uuid,
            status,
        )
    }
}
