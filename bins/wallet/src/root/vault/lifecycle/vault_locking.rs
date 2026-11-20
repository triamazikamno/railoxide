#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn lock_vault(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !vault_lock_is_allowed(
            self.maintenance_controller.read(cx).is_idle(),
            self.manage_wallets.deleting_wallet_id.is_some(),
        ) {
            return;
        }
        self.vault_state = VaultState::UnlockVault;
        self.retire_gateway_unlock();
        self.shutdown_wallet_session_store();
        self.invalidate_governance_context();
        self.governance.proposal_participation.clear_session();
        self.invalidate_proposals_chain(self.last_evm_chain);
        self.clear_private_broadcaster_progress_state();
        crate::root::renew_submission_scope(&mut self.submission_scope);
        self.cancel_tor_rotation();
        self.stop_waku();
        window.close_all_dialogs(cx);
        self.set_network_status_popover_open(false, cx);
        self.clear_protected_software_seed_session(cx);
        ui::clipboard::clear_copied_secret(cx);
        self.view_session = None;
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.clear_address_books();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preferences.error = None;
        self.logs_panel
            .logs_search
            .update(cx, crate::root::search_field::SearchField::end_session);
        self.address_book.search_query = Arc::from("");
        self.address_book
            .search_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.address_book.clear_dialog_state(window, cx);
        self.broadcaster_preferences
            .favorite_broadcaster_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.broadcaster_preferences
            .banned_broadcaster_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.broadcaster_preferences.active_tab = BroadcasterActivityTab::default();
        self.address_book_save_error = None;
        self.advance_active_wallet_generation();
        self.sync_wallet_select(window, cx);
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.reset_public_wallet_state(window, cx);
        self.private_action_form = None;
        self.broadcaster_picker = None;
        self.active_wallet_tab = self.default_wallet_tab();
        self.setup_password = None;
        self.clear_add_wallet_password(window, cx);
        self.vault_view_unlock = None;
        self.auto_lock.disarm();
        self.generated_seed = None;
        self.vault_inputs
            .import_mnemonic_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.clear_key_export_dialog_state(window, cx);
        #[cfg(feature = "hardware")]
        {
            self.hardware_profile.active = None;
            self.hardware_profile_unlock = HardwareProfileUnlockState::default();
            self.clear_hardware_profile_sensitive_inputs(window, cx);
        }
        self.hardware_wallet_creation.in_progress = false;
        self.hardware_wallet_creation.generation =
            self.hardware_wallet_creation.generation.wrapping_add(1);
        self.hardware_wallet_creation.intent = None;
        self.clear_hardware_wallet_restore_account_index(window, cx);
        self.vault_error = None;
        self.repair_cache_error = None;
        self.publish_gateway_desktop_state();
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.focus_vault_input_on_render = true;
        self.refresh_device_auth_status();
        for state in self.chain_states.values_mut() {
            *state = ChainUtxoState::Idle;
        }
        self.sync_utxo_table(cx);
        cx.notify();
    }

    pub(in crate::root) fn handle_vault_error(
        &mut self,
        error: &VaultError,
        cx: &mut Context<'_, Self>,
    ) {
        tracing::warn!(target: LOG_TARGET,
            error_kind = vault_error_kind(error),
            "desktop wallet vault operation failed"
        );
        self.set_vault_error(vault_error_message(error), cx);
    }

    pub(in crate::root) fn set_vault_error(
        &mut self,
        message: impl Into<Arc<str>>,
        cx: &mut Context<'_, Self>,
    ) {
        self.vault_error = Some(message.into());
        cx.notify();
    }

    pub(super) fn set_wallet_replacement_error(
        &mut self,
        message: &'static str,
        cx: &mut Context<'_, Self>,
    ) {
        self.retire_gateway_unlock();
        self.vault_error = None;
        self.vault_state = VaultState::Error(Arc::from(message));
        self.publish_gateway_desktop_state();
        self.focus_vault_input_on_render = false;
        cx.notify();
    }
}
