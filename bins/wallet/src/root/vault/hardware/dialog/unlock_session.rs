#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    #[cfg(feature = "hardware")]
    pub(in crate::root) const fn hardware_profile_unlock_requires_password(&self) -> bool {
        self.hardware_profile_unlock.vault_view_unlock.is_none()
            && self.vault_view_unlock.is_none()
            && self.view_session.is_none()
            && self.setup_password.is_none()
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) fn hardware_profile_vault_view_unlock(
        &mut self,
        store: &Arc<DesktopVaultStore>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Arc<ViewUnlock>> {
        if let Some(vault_view_unlock) = self.hardware_profile_unlock.vault_view_unlock.clone() {
            return Some(vault_view_unlock);
        }
        if let Some(vault_view_unlock) = self.vault_view_unlock.clone() {
            self.hardware_profile_unlock.vault_view_unlock = Some(Arc::clone(&vault_view_unlock));
            return Some(vault_view_unlock);
        }
        if let Some(session) = self.view_session.as_ref() {
            let vault_view_unlock = Arc::new(session.clone_vault_view_unlock());
            self.install_vault_view_unlock(Arc::clone(&vault_view_unlock));
            self.hardware_profile_unlock.vault_view_unlock = Some(Arc::clone(&vault_view_unlock));
            return Some(vault_view_unlock);
        }
        if let Some(password) = self.setup_password.as_ref() {
            match store.unlock_view(password.as_str()) {
                Ok(view) => {
                    let vault_view_unlock = Arc::new(view);
                    self.install_vault_view_unlock(Arc::clone(&vault_view_unlock));
                    self.hardware_profile_unlock.vault_view_unlock =
                        Some(Arc::clone(&vault_view_unlock));
                    return Some(vault_view_unlock);
                }
                Err(error) => {
                    self.handle_hardware_profile_vault_error(&error);
                    cx.notify();
                    return None;
                }
            }
        }

        let password = match self.hardware_profile.device_auth_password.take() {
            Some(password) => password,
            None => Self::read_and_clear_input(&self.hardware_profile.password_input, window, cx),
        };
        if password.trim().is_empty() {
            self.hardware_profile_unlock.error =
                Some(Arc::from("Enter the vault password to continue"));
            cx.notify();
            return None;
        }
        match store.unlock_view(password.as_str()) {
            Ok(view) => {
                let vault_view_unlock = Arc::new(view);
                self.install_vault_view_unlock(Arc::clone(&vault_view_unlock));
                self.hardware_profile_unlock.vault_view_unlock =
                    Some(Arc::clone(&vault_view_unlock));
                let store = Arc::clone(store);
                let join = self
                    .runtime
                    .spawn_blocking(move || Self::renew_device_auth(&store, password.as_str()));
                cx.spawn(async move |this, cx| {
                    if let Err(error) = join.await {
                        tracing::warn!(target: LOG_TARGET, %error, "device authentication renewal task failed");
                    }
                    let _ = this.update(cx, |root, cx| {
                        root.refresh_device_auth_status();
                        cx.notify();
                    });
                })
                .detach();
                Some(vault_view_unlock)
            }
            Err(error) => {
                self.handle_hardware_profile_vault_error(&error);
                cx.notify();
                None
            }
        }
    }

    #[cfg(feature = "hardware")]
    pub(super) fn dismiss_hardware_profile_unlock_dialog(
        &mut self,
        gateway_unlock: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.gateway.private_hardware_selection_pending {
            self.gateway.private_hardware_selection_pending = false;
            self.gateway.private_selection_message = Some("Wallet selection cancelled.");
        }
        self.abandon_gateway_unlock_dialog(gateway_unlock);
        self.next_hardware_profile_action_generation();
        self.manage_wallets.finish_hardware_delete_unlock_dialog();
        dismiss_hardware_profile_unlock_state(&mut self.hardware_profile_unlock);
        self.clear_hardware_profile_sensitive_inputs(window, cx);
        self.publish_gateway_desktop_state();
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) const fn next_hardware_profile_action_generation(&mut self) -> u64 {
        self.hardware_wallet_creation.generation =
            self.hardware_wallet_creation.generation.wrapping_add(1);
        self.hardware_wallet_creation.generation
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) const fn hardware_profile_action_is_current(
        &self,
        generation: u64,
    ) -> bool {
        hardware_wallet_creation_result_is_current(
            self.hardware_wallet_creation.generation,
            generation,
        )
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) const fn hardware_profile_unlock_auto_starts(&self) -> bool {
        matches!(
            self.hardware_profile_unlock.device_kind,
            Some(HardwareDeviceKind::Ledger)
        ) && !self.hardware_profile_unlock_requires_password()
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) fn begin_hardware_profile_detection_progress(&mut self) {
        self.hardware_profile_unlock.progress_steps = default_hardware_profile_steps();
        let message = self
            .hardware_profile_unlock
            .reconnect_notice
            .take()
            .unwrap_or_else(|| "Plug it in and enter your PIN.".into());
        self.hardware_profile_unlock.set_progress_step(
            HardwareProfileStep::UnlockDevice,
            HardwareProfileStepStatus::Pending,
            Some(message),
        );
    }

    #[cfg(feature = "hardware")]
    fn apply_hardware_profile_progress_update(
        &mut self,
        update: HardwareProfileProgressUpdate,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(always_on_device) = update.trezor_passphrase_always_on_device {
            self.hardware_profile_unlock
                .trezor_passphrase_always_on_device = Some(always_on_device);
            if always_on_device
                && self.hardware_profile_unlock.trezor_passphrase_mode
                    == TrezorPassphraseMode::EnterInApp
            {
                self.hardware_profile_unlock.trezor_passphrase_mode =
                    TrezorPassphraseMode::NoPassphrase;
                self.hardware_profile
                    .trezor_app_passphrase_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
        }
        if let Some(approval_prompt) = update.approval_prompt {
            self.hardware_profile_unlock.approval_prompt = Some(approval_prompt);
        }
        if update.clear_trezor_pin_matrix_request {
            self.hardware_profile_unlock
                .clear_trezor_pin_matrix_prompt();
        }
        if let Some(request) = update.trezor_pin_matrix_request {
            self.hardware_profile_unlock
                .clear_trezor_pin_matrix_prompt();
            self.hardware_profile_unlock.trezor_pin_matrix_prompt =
                Some(TrezorPinMatrixPromptState {
                    kind: request.kind,
                    positions: String::new(),
                    response_tx: Some(request.response_tx),
                });
        }
        if update.apply_step {
            self.hardware_profile_unlock.set_progress_step(
                update.step,
                update.status,
                update.message.map(Arc::<str>::from),
            );
        }
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault::hardware) fn spawn_hardware_profile_progress_listener(
        generation: u64,
        mut progress_rx: mpsc::UnboundedReceiver<HardwareProfileProgressUpdate>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while let Some(update) = progress_rx.recv().await {
                let Ok(current) = this.update_in(cx, |root, window, cx| {
                    if !root.hardware_profile_action_is_current(generation) {
                        return false;
                    }
                    root.apply_hardware_profile_progress_update(update, window, cx);
                    true
                }) else {
                    break;
                };
                if !current {
                    break;
                }
            }
        })
        .detach();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) fn handle_hardware_profile_vault_error(
        &mut self,
        error: &VaultError,
    ) {
        if matches!(error, VaultError::HardwareWalletIdentityMismatch)
            && let Some(session) = self.hardware_profile_unlock.session.as_mut()
        {
            session.discard_trezor_session();
        }
        let message = vault_error_message(error);
        self.hardware_profile_unlock
            .mark_first_pending_progress_step_error(Arc::clone(&message));
        self.hardware_profile_unlock.error = Some(message);
    }
}
