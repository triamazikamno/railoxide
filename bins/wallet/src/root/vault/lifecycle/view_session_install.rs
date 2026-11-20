#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root::vault) fn install_view_session(
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
            wallet_ops::CreatedWalletChainInitPolicy::Resumed,
            window,
            cx,
        );
    }

    pub(in crate::root::vault) fn install_view_session_for_gateway_unlock(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        gateway_unlock: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.begin_view_session_installation(
            session,
            metadata,
            true,
            wallet_ops::CreatedWalletChainInitPolicy::Resumed,
            None,
            gateway_unlock,
            window,
            cx,
        );
    }

    pub(in crate::root) fn install_view_session_after_management(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.install_view_session_with_dialog_policy(
            session,
            metadata,
            false,
            wallet_ops::CreatedWalletChainInitPolicy::Resumed,
            window,
            cx,
        );
    }

    pub(in crate::root::vault) fn install_verified_software_context(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        protected_seed_session: Option<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        gateway_unlock: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.begin_view_session_installation(
            session,
            metadata,
            true,
            wallet_ops::CreatedWalletChainInitPolicy::Resumed,
            protected_seed_session,
            gateway_unlock,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    fn active_hardware_profile_for_wallet(
        &self,
        wallet_id: &str,
        metadata: &[WalletMetadataBundle],
    ) -> Option<HardwareProfileMetadata> {
        let account = metadata
            .iter()
            .find(|wallet| wallet.wallet_uuid == wallet_id)
            .and_then(|wallet| wallet.hardware_account.as_ref())?;

        self.hardware_profile_unlock
            .profile
            .as_ref()
            .filter(|profile| profile.profile_id == account.profile_id)
            .cloned()
            .or_else(|| {
                self.hardware_profile
                    .active
                    .as_ref()
                    .filter(|profile| profile.profile_id == account.profile_id)
                    .cloned()
            })
    }

    pub(super) fn install_view_session_with_dialog_policy(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        close_dialogs: bool,
        created_wallet_init_policy: wallet_ops::CreatedWalletChainInitPolicy,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.begin_view_session_installation(
            session,
            metadata,
            close_dialogs,
            created_wallet_init_policy,
            None,
            None,
            window,
            cx,
        );
    }

    fn begin_view_session_installation(
        &mut self,
        session: DesktopViewSession,
        metadata: &[WalletMetadataBundle],
        close_dialogs: bool,
        created_wallet_init_policy: wallet_ops::CreatedWalletChainInitPolicy,
        protected_seed_session: Option<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        gateway_unlock: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.manage_wallets.deleting_wallet_id.is_some() {
            self.sync_wallet_select(window, cx);
            return;
        }
        // Keep the originating attempt even if later lifecycle cleanup retires it.
        let remote_unlock = self.remote_unlock_attempt();
        if remote_unlock
            .as_ref()
            .is_some_and(|attempt| !attempt.is_current())
        {
            self.reconcile_remote_unlock(window, cx);
            return;
        }
        self.begin_gateway_wallet_installation(session.wallet_id());
        self.gateway.private_selection_message = None;
        self.gateway.private_hardware_selection_pending = false;
        if !self.gateway_unlock_is_current(gateway_unlock) {
            self.retire_gateway_unlock();
        }
        self.transfer_gateway_unlock_to_installation(gateway_unlock);
        #[cfg(feature = "hardware")]
        let active_hardware_profile =
            self.active_hardware_profile_for_wallet(session.wallet_id(), metadata);
        self.clear_protected_software_seed_session(cx);
        self.supersede_wallet_sessions();
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
        let profile_open_operation_generation =
            self.pending_software_profile_open_operation_generation;
        let profile_open_lifetime_generation =
            self.pending_software_profile_open_lifetime_generation;
        if close_dialogs {
            window.close_all_dialogs(cx);
        }
        self.advance_active_wallet_generation_for_gateway_unlock(gateway_unlock);
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        let wallet_switch_generation = self.wallet_switch_generation;
        clear_wallet_context_visibility(
            &mut self.view_session,
            &mut self.selected_wallet_id,
            &mut self.revealed_passphrase_context_id,
            &mut self.wallet_metadata,
            &mut self.wallet_options,
            &mut self.public_accounts,
        );
        self.vault_view_unlock = None;
        self.vault_state = VaultState::SwitchingWallet;
        self.publish_gateway_desktop_state();
        self.wallet_switch_delayed = false;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.focus_vault_input_on_render = false;
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
        self.reset_wallet_scoped_state(cx);
        self.clear_address_books();
        self.sync_wallet_select(window, cx);
        cx.notify();

        let installation = WalletContextInstallation {
            remote_unlock,
            gateway_unlock,
            session: Arc::new(session),
            metadata: metadata.to_vec(),
            created_wallet_init_policy,
            protected_seed_session,
            active_wallet_generation: self.active_wallet_generation,
            wallet_switch_generation,
            profile_open_operation_generation,
            profile_open_lifetime_generation,
            selected_chain: self.last_evm_chain,
            #[cfg(feature = "hardware")]
            active_hardware_profile,
            cleanup: self.wallet_sync_cleanup_wait_group(),
        };
        let cleanup = installation.cleanup.clone();
        let captured_wallet_generation = installation.active_wallet_generation;
        let captured_switch_generation = installation.wallet_switch_generation;
        let captured_operation_generation = installation.profile_open_operation_generation;
        let captured_lifetime_generation = installation.profile_open_lifetime_generation;
        let captured_chain = installation.selected_chain;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(WALLET_REPLACEMENT_DELAYED_THRESHOLD)
                .await;
            let _ = this.update_in(cx, |root, _window, cx| {
                if wallet_replacement_update_is_current(
                    captured_wallet_generation,
                    root.active_wallet_generation,
                    captured_switch_generation,
                    root.wallet_switch_generation,
                    captured_operation_generation,
                    root.pending_software_profile_open_operation_generation,
                    captured_lifetime_generation,
                    root.pending_software_profile_open_lifetime_generation,
                    captured_chain,
                    root.last_evm_chain,
                    root.manage_wallets.deleting_wallet_id.is_some(),
                    root.view_session.is_none()
                        && root.vault_view_unlock.is_none()
                        && matches!(root.vault_state, VaultState::SwitchingWallet),
                ) {
                    root.wallet_switch_delayed = true;
                    cx.notify();
                }
            });
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            match wait_for_wallet_replacement_cleanup(
                cleanup,
                cx.background_executor().timer(WALLET_REPLACEMENT_PRESENTATION_CUTOFF),
            )
            .await
            {
                WalletReplacementCleanupWaitOutcome::Completed(cleanup_result) => {
                    let _ = this.update_in(cx, |root, window, cx| {
                        let installation = installation;
                        if !wallet_replacement_update_is_current(
                            captured_wallet_generation,
                            root.active_wallet_generation,
                            captured_switch_generation,
                            root.wallet_switch_generation,
                            captured_operation_generation,
                            root.pending_software_profile_open_operation_generation,
                            captured_lifetime_generation,
                            root.pending_software_profile_open_lifetime_generation,
                            captured_chain,
                            root.last_evm_chain,
                            root.manage_wallets.deleting_wallet_id.is_some(),
                            root.view_session.is_none()
                                && root.vault_view_unlock.is_none()
                                && matches!(root.vault_state, VaultState::SwitchingWallet),
                        ) {
                            return;
                        }
                        let report = match cleanup_result {
                            Ok(report) => report,
                            Err(error) => {
                                tracing::warn!(target: LOG_TARGET, %error, "wallet replacement sync cleanup failed");
                                root.set_wallet_replacement_error(
                                    "Previous wallet sync cleanup could not be completed. Retry the wallet replacement before continuing.",
                                    cx,
                                );
                                return;
                            }
                        };
                        if report.failed_startup_tasks != 0 {
                            root.set_wallet_replacement_error(
                                "Previous wallet sync cleanup failed. Retry the wallet replacement.",
                                cx,
                            );
                            return;
                        }
                        root.finish_view_session_installation(installation, window, cx);
                    });
                }
                WalletReplacementCleanupWaitOutcome::PresentationTimedOut => {
                    let _ = this.update_in(cx, |root, window, cx| {
                        if wallet_replacement_update_is_current(
                            captured_wallet_generation,
                            root.active_wallet_generation,
                            captured_switch_generation,
                            root.wallet_switch_generation,
                            captured_operation_generation,
                            root.pending_software_profile_open_operation_generation,
                            captured_lifetime_generation,
                            root.pending_software_profile_open_lifetime_generation,
                            captured_chain,
                            root.last_evm_chain,
                            root.manage_wallets.deleting_wallet_id.is_some(),
                            root.view_session.is_none()
                                && root.vault_view_unlock.is_none()
                                && matches!(root.vault_state, VaultState::SwitchingWallet),
                        ) {
                            root.abandon_wallet_replacement_installation(window, cx);
                        }
                    });
                    drop(installation);
                }
            }
        })
        .detach();
    }

    fn finish_view_session_installation(
        &mut self,
        installation: WalletContextInstallation,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(attempt) = &installation.remote_unlock {
            if !attempt.finish_if_current(wallet_ops::gateway::GatewayUnlockPhase::Complete) {
                self.abandon_wallet_replacement_installation(window, cx);
                return;
            }
            self.set_remote_unlock_attempt(None);
        }
        let WalletContextInstallation {
            remote_unlock: _,
            gateway_unlock,
            session,
            metadata,
            created_wallet_init_policy,
            protected_seed_session,
            active_wallet_generation: _,
            wallet_switch_generation: _,
            profile_open_operation_generation: _,
            profile_open_lifetime_generation: _,
            selected_chain: _,
            #[cfg(feature = "hardware")]
            active_hardware_profile,
            cleanup: _,
        } = installation;
        let wallet_id: Arc<str> = Arc::from(session.wallet_id().to_owned());
        let vault_view_unlock = Arc::new(session.clone_vault_view_unlock());
        self.invalidate_pending_profile_open_tokens();
        let visible_metadata = visible_wallet_metadata(&metadata, Some(wallet_id.as_ref()));
        self.revealed_passphrase_context_id = visible_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_id.as_ref())
            .and_then(|metadata| {
                metadata
                    .software_context
                    .as_ref()
                    .filter(|context| {
                        context.kind == wallet_ops::vault::WalletSoftwareContextKind::Passphrase
                    })
                    .map(|_| Arc::clone(&wallet_id))
            });
        self.view_session = Some(Arc::clone(&session));
        self.install_vault_view_unlock(vault_view_unlock);
        self.wallet_metadata = visible_metadata;
        self.wallet_options = wallet_options_from_metadata(self.wallet_metadata.clone());
        self.selected_wallet_id = Some(Arc::clone(&wallet_id));
        let selected_metadata = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_id.as_ref());
        if selected_metadata.is_some_and(|metadata| metadata.source.is_hardware_derived())
            || session.hardware_profile_session().is_some()
        {
            self.ui_state.last_wallet_id = Some(wallet_id.as_ref().to_owned());
            self.ui_state.last_wallet_kind = RememberedWalletKind::HardwareWallet;
        } else {
            self.ui_state.last_wallet_id = Some(
                selected_metadata
                    .and_then(|metadata| metadata.software_context.as_ref())
                    .map_or_else(
                        || wallet_id.as_ref().to_owned(),
                        |context| context.base_profile_uuid.clone(),
                    ),
            );
            self.ui_state.last_wallet_kind = RememberedWalletKind::SoftwareProfile;
        }
        self.save_ui_state();
        #[cfg(feature = "hardware")]
        {
            self.hardware_profile.active = if selected_metadata
                .is_some_and(|metadata| metadata.source.is_hardware_derived())
                || session.hardware_profile_session().is_some()
            {
                active_hardware_profile
            } else {
                None
            };
        }
        self.sync_wallet_select(window, cx);
        self.reset_wallet_scoped_state(cx);
        self.protected_software_seed_session = protected_seed_session.map(Arc::new);
        self.reload_address_books(cx);
        self.reload_broadcaster_preferences(cx);
        self.reload_public_accounts(window, cx);
        if self.active_activity == crate::root::sidebar::Activity::Proposals {
            match self.governance.tab {
                crate::root::governance::GovernanceTab::Proposals => {
                    self.start_proposals_refresh(false, cx);
                }
                crate::root::governance::GovernanceTab::Staking => {
                    self.start_staking_refresh(cx);
                }
            }
        }
        self.setup_password = None;
        self.generated_seed = None;
        #[cfg(feature = "hardware")]
        {
            self.hardware_profile_unlock = HardwareProfileUnlockState::default();
            self.clear_hardware_profile_sensitive_inputs(window, cx);
        }
        self.hardware_wallet_creation.in_progress = false;
        self.hardware_wallet_creation.generation =
            self.hardware_wallet_creation.generation.wrapping_add(1);
        self.hardware_wallet_creation.intent = None;
        self.clear_hardware_wallet_restore_account_index(window, cx);
        self.clear_add_wallet_password(window, cx);
        self.vault_inputs
            .import_mnemonic_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.clear_key_export_dialog_state(window, cx);
        self.vault_error = None;
        self.complete_gateway_unlock(gateway_unlock);
        self.vault_state = VaultState::ViewUnlocked;
        self.finish_gateway_wallet_installation();
        self.publish_gateway_desktop_state();
        self.wallet_switch_delayed = false;
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.ensure_waku_started(cx);
        self.ensure_chain_load_with_start_policy(
            self.last_evm_chain,
            Some(created_wallet_init_policy.sync_start_policy()),
            cx,
        );
        if created_wallet_init_policy != wallet_ops::CreatedWalletChainInitPolicy::Resumed {
            self.initialize_created_wallet_chain_metadata(created_wallet_init_policy);
        }
        cx.notify();
    }

    pub(in crate::root) fn abandon_wallet_replacement_installation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.retire_gateway_unlock();
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        self.pending_software_profile_open = None;
        self.pending_software_profile_base_profile_uuid = None;
        self.invalidate_pending_profile_open_tokens();
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
        self.wallet_switch_delayed = false;
        self.vault_error = Some(Arc::from(WALLET_REPLACEMENT_TIMEOUT_MESSAGE));
        self.vault_state = VaultState::UnlockVault;
        self.publish_gateway_desktop_state();
        self.wallet_setup_mode = WalletSetupMode::Choose;
        self.focus_vault_input_on_render = true;
        self.sync_wallet_select(window, cx);
        cx.notify();
    }
}
