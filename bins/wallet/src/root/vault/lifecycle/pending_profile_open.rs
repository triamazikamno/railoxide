#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root::vault) fn enter_pending_software_profile_open(
        &mut self,
        mut pending: PendingSoftwareProfileOpen,
        gateway_unlock: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.pending_software_profile_open_lifetime_generation = self
            .pending_software_profile_open_lifetime_generation
            .wrapping_add(1);
        self.pending_software_profile_open_operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        pending.set_operation_generation(self.pending_software_profile_open_operation_generation);
        self.pending_software_profile_base_profile_uuid =
            Some(Arc::clone(&pending.base_profile_uuid));
        self.pending_software_profile_open = Some(pending);
        self.shutdown_wallet_session_store();
        self.advance_active_wallet_generation_for_gateway_unlock(gateway_unlock);
        self.clear_protected_software_seed_session(cx);
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.vault_view_unlock = None;
        self.clear_address_books();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preferences.error = None;
        self.reset_wallet_scoped_state(cx);
        self.sync_wallet_select(window, cx);
        self.setup_password = None;
        self.generated_seed = None;
        self.vault_error = None;
        self.vault_state = VaultState::PendingSoftwareProfileOpen;
        self.publish_gateway_desktop_state();
        self.wallet_setup_mode = WalletSetupMode::Choose;
        cx.notify();
        self.schedule_pending_software_profile_open_timeout(window, cx);
        let passphrase_open_ui = self.passphrase_open_ui.clone();
        cx.defer_in(window, move |_root, window, cx| {
            passphrase_open_ui.update(cx, |ui, cx| ui.focus_passphrase(window, cx));
        });
    }

    fn schedule_pending_software_profile_open_timeout(
        &self,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        let captured_lifetime_generation = self.pending_software_profile_open_lifetime_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(PENDING_SOFTWARE_PROFILE_OPEN_TIMEOUT)
                .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if pending_software_profile_open_timeout_is_current(
                    captured_lifetime_generation,
                    root.pending_software_profile_open_lifetime_generation,
                    matches!(root.vault_state, VaultState::PendingSoftwareProfileOpen),
                ) {
                    root.abandon_pending_software_profile_open(window, cx);
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn pending_software_profile_open_stage(
        &self,
    ) -> Option<crate::root::vault::PendingSoftwareProfileOpenStage> {
        self.pending_software_profile_open
            .as_ref()
            .map(PendingSoftwareProfileOpen::stage)
    }

    pub(in crate::root) fn pending_software_profile_open_base_label(&self) -> Option<&str> {
        self.pending_software_profile_open
            .as_ref()
            .map(|pending| pending.base_metadata.label.as_str())
    }

    pub(in crate::root::vault) fn pending_software_profile_open_is_current(
        &self,
        operation_generation: u64,
        active_wallet_generation: u64,
        selected_chain: u64,
        base_profile_uuid: &str,
    ) -> bool {
        pending_profile_result_is_current(
            operation_generation,
            self.pending_software_profile_open_operation_generation,
            active_wallet_generation,
            self.active_wallet_generation,
            base_profile_uuid,
            self.pending_software_profile_base_profile_uuid.as_deref(),
            self.selected_wallet_id.as_deref(),
            selected_chain,
            self.last_evm_chain,
            matches!(self.vault_state, VaultState::PendingSoftwareProfileOpen),
        )
    }

    pub(in crate::root) fn abandon_pending_software_profile_open(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.retire_gateway_unlock();
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        self.shutdown_wallet_session_store();
        self.clear_protected_software_seed_session(cx);
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.vault_view_unlock = None;
        self.clear_address_books();
        self.set_broadcaster_preferences(wallet_ops::vault::BroadcasterPreferences::default(), cx);
        self.broadcaster_preferences.error = None;
        self.reset_wallet_scoped_state(cx);
        self.sync_wallet_select(window, cx);
        self.vault_state = VaultState::UnlockVault;
        self.publish_gateway_desktop_state();
        self.focus_vault_input_on_render = true;
        cx.notify();
    }

    pub(in crate::root) const fn invalidate_pending_profile_open_tokens(&mut self) {
        self.pending_software_profile_open_operation_generation = self
            .pending_software_profile_open_operation_generation
            .wrapping_add(1);
        self.pending_software_profile_open_lifetime_generation = self
            .pending_software_profile_open_lifetime_generation
            .wrapping_add(1);
    }
}
