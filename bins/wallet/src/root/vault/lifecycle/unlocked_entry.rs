#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn sync_wallet_select(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let items = wallet_select_items_from_metadata(&self.wallet_metadata);
        let selected_value = self.selected_wallet_id.as_ref().map(|wallet_id| {
            wallet_select_value_for_selected_wallet(wallet_id, &self.wallet_metadata)
        });
        self.wallet_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            if let Some(value) = selected_value.as_ref() {
                select.set_selected_value(value, window, cx);
            } else {
                select.set_selected_index(None, window, cx);
            }
        });
    }

    pub(in crate::root::vault) fn enter_view_unlocked(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.install_view_session(session, metadata, window, cx);
    }

    pub(in crate::root::vault) fn enter_new_wallet_view_unlocked(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.install_view_session_with_dialog_policy(
            session,
            metadata,
            true,
            wallet_ops::CreatedWalletChainInitPolicy::InitialCreate,
            window,
            cx,
        );
    }

    pub(in crate::root::vault) fn enabled_chain_ids_for_created_wallet(&self) -> BTreeSet<u64> {
        self.effective_chain_configs
            .railgun_chains()
            .map(|chain| chain.chain_id)
            .collect()
    }

    pub(super) fn initialize_created_wallet_chain_metadata(
        &mut self,
        init_policy: wallet_ops::CreatedWalletChainInitPolicy,
    ) {
        let Some(view_session) = self.view_session.clone() else {
            return;
        };
        let Some(vault_store) = self.vault_store.as_ref() else {
            return;
        };
        let effective_chains = self.effective_chain_configs.clone();
        let db = vault_store.db();
        let http = self.http.clone();
        let skip_chain_id = Some(self.last_evm_chain);

        let task = self.runtime.spawn(async move {
            wallet_ops::initialize_created_wallet_chain_metadata_for_session(
                view_session,
                effective_chains,
                db,
                http,
                skip_chain_id,
                init_policy,
            )
            .await;
        });
        self.wallet_sync_lifecycle.track_wallet_task(task);
    }

    pub(in crate::root) fn enter_password_metadata_unlocked(
        &mut self,
        metadata: &[WalletMetadataBundle],
        vault_view_unlock: Arc<ViewUnlock>,
        setup_password: Option<Zeroizing<String>>,
        pending_software_profile_open: Option<PendingSoftwareProfileOpen>,
        gateway_unlock: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.gateway_unlock_is_current(gateway_unlock) {
            self.retire_gateway_unlock();
        }
        self.clear_protected_software_seed_session(cx);
        let active = wallet_options_from_metadata(metadata.to_owned());
        if active.is_empty() {
            if let Some(password) = setup_password {
                self.set_default_wallet_name_from_password(password.as_str(), window, cx);
                self.setup_password = Some(password);
            }
            self.install_vault_view_unlock(vault_view_unlock);
            self.vault_error = None;
            self.vault_state = VaultState::SetupWallet;
            self.publish_gateway_desktop_state();
            self.wallet_setup_mode = WalletSetupMode::Choose;
            self.ensure_waku_started(cx);
            cx.notify();
            return;
        }

        if let Some(pending) = pending_software_profile_open {
            self.enter_pending_software_profile_open(pending, gateway_unlock, window, cx);
            return;
        }

        window.close_all_dialogs(cx);
        self.advance_active_wallet_generation_for_gateway_unlock(gateway_unlock);
        self.pending_software_profile_base_profile_uuid = None;
        self.view_session = None;
        self.install_vault_view_unlock(vault_view_unlock);
        self.revealed_passphrase_context_id = None;
        self.wallet_metadata = visible_wallet_metadata(metadata, None);
        self.wallet_options = wallet_options_from_metadata(self.wallet_metadata.clone());
        self.selected_wallet_id = None;
        self.sync_wallet_select(window, cx);
        self.shutdown_wallet_session_store();
        self.reset_wallet_scoped_state(cx);
        self.setup_password = None;
        self.generated_seed = None;
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
        self.vault_state = VaultState::ViewUnlocked;
        self.publish_gateway_desktop_state();
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.ensure_waku_started(cx);
        cx.notify();
    }
}
