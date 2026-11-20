#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn executor_owner_for_public_chain(
        &self,
        chain_id: u64,
    ) -> Option<Arc<wallet_ops::ExecutorOwner>> {
        match self.chain_states.get(&chain_id) {
            Some(
                crate::root::ChainUtxoState::Ready { session, .. }
                | crate::root::ChainUtxoState::Syncing { session, .. },
            ) => session.executor_owner(),
            _ => None,
        }
    }

    pub(in crate::root) fn reload_public_accounts(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.reload_tron_accounts();
        let Some(store) = self.vault_store.as_ref() else {
            self.public_form.error = Some(Arc::from("Wallet vault storage is unavailable"));
            return;
        };
        let Some(view_session) = self.view_session.as_ref() else {
            self.public_accounts.clear();
            self.public_form.selected_account_uuid = None;
            self.publish_gateway_desktop_state();
            self.sync_walletconnect_account_select(window, cx);
            self.sync_self_broadcast_gas_payer_selects(window, cx);
            self.invalidate_blocked_shield_rescue_rows(cx);
            return;
        };
        let next_derived_index = store
            .next_derived_public_account_index_for_session(view_session.as_ref())
            .ok();
        match store.list_public_accounts_for_session(view_session.as_ref(), true) {
            Ok(accounts) => {
                self.public_form.next_account_label_number =
                    next_public_account_label_number(accounts.len());
                let selected = restored_public_account_selection(
                    &accounts,
                    self.public_form.selected_account_uuid.as_deref(),
                    &self.ui_state,
                    view_session.wallet_id(),
                );
                if self.public_accounts != accounts {
                    self.public_balance_cache.retain_accounts(&accounts);
                    self.restore_public_balance_snapshot();
                }
                self.public_accounts = accounts;
                self.public_form.selected_account_uuid = selected;
                if let Some(account) = self.listed_selected_account() {
                    self.public_form.open_section = account.status;
                }
                self.public_form.next_derived_index = next_derived_index;
                self.reconcile_public_account_selection();
                self.remember_public_account_selection();
                self.publish_gateway_desktop_state();
                self.sync_self_broadcast_gas_payer_selects(window, cx);
                self.sync_public_edit_label_input(window, cx);
                self.invalidate_blocked_shield_rescue_rows(cx);
                self.reload_walletconnect_sessions(cx);
                self.sync_walletconnect_account_select(window, cx);
            }
            Err(error) => {
                tracing::warn!(target: LOG_TARGET,
                    error_kind = vault_error_kind(&error),
                    "load public accounts failed"
                );
                self.public_form.error = Some(Arc::from(error.to_string()));
            }
        }
    }

    pub(in crate::root) fn sync_public_edit_label_input(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let account_uuid = self
            .public_form
            .editing_account_uuid
            .as_ref()
            .or_else(|| self.listed_selected_uuid());
        let label = self
            .listed_account_for_uuid(account_uuid.map(AsRef::as_ref))
            .and_then(|account| account.label.clone())
            .unwrap_or_default();
        self.public_form
            .edit_label_input
            .update(cx, |input, cx| input.set_value(&label, window, cx));
    }

    pub(in crate::root) fn selected_public_account(&self) -> Option<&PublicAccountMetadata> {
        self.public_account_for_uuid(
            self.public_form
                .selected_account_uuid
                .as_ref()
                .map(AsRef::as_ref),
        )
    }

    pub(in crate::root) fn public_account_for_uuid(
        &self,
        public_account_uuid: Option<&str>,
    ) -> Option<&PublicAccountMetadata> {
        let selected = public_account_uuid?;
        self.public_accounts.iter().find(|account| {
            account.public_account_uuid == selected
                && account.is_available_on_chain(self.last_evm_chain)
        })
    }

    pub(in crate::root) fn set_public_selected_balance(
        &mut self,
        public_account_uuid: Arc<str>,
        asset: PublicAssetId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.selected_account_uuid = Some(public_account_uuid);
        self.public_form.selected_asset = Some(asset);
        // Selecting a balance starts a fresh shield draft, so restore the configured default.
        self.public_form.mimic_railway_shield = self.mimic_railway_shields_by_default;
        self.public_form.shield_approval_mode = wallet_ops::PublicShieldApprovalMode::Exact;
        self.public_form.native_shield_preparation.invalidate();
        self.public_form.public_send_kind = PublicSendKind::Transfer;
        self.public_form.advanced_send_estimate = None;
        self.public_form.advanced_send_estimate_invalidated = false;
        self.invalidate_advanced_public_send_estimate();
        self.public_form.send_error = None;
        self.public_form.shield_error = None;
        self.remember_public_account_selection();
        self.sync_public_edit_label_input(window, cx);
        self.publish_gateway_desktop_state();
        cx.notify();
    }

    pub(super) fn remember_public_account_selection(&mut self) {
        let Some(view) = self.view_session.as_ref() else {
            return;
        };
        let Some(account) = self
            .selected_public_account()
            .filter(|account| account.is_active_for_wallet(view.wallet_id()))
        else {
            return;
        };
        if self.ui_state.last_public_accounts.get(view.wallet_id())
            != Some(&account.public_account_uuid)
        {
            let wallet_id = view.wallet_id().to_owned();
            let account_uuid = account.public_account_uuid.clone();
            self.ui_state
                .last_public_accounts
                .insert(wallet_id, account_uuid);
            self.save_ui_state();
        }
    }

    pub(in crate::root) fn set_public_account_section_open(
        &mut self,
        status: PublicAccountStatus,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.public_form.open_section = status;
        self.public_form
            .list_scroll
            .set_offset(gpui::Point::default());
        self.reconcile_public_account_selection();
        self.public_form.list_focus.focus(window, cx);
        cx.notify();
    }

    pub(in crate::root) fn has_active_public_accounts(&self) -> bool {
        self.public_accounts.iter().any(|account| {
            account.status == PublicAccountStatus::Active
                && account.is_available_on_chain(self.last_evm_chain)
        })
    }
}
