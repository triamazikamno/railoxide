#[cfg(feature = "hardware")]
use gpui::{InteractiveElement, StatefulInteractiveElement};

#[cfg(feature = "hardware")]
use super::{
    Arc, Context, DesktopVaultStore, Focusable, HardwareDerivationError, HardwareDeviceKind,
    HardwareProfilePickerView, HardwareProfileProgressUpdate, HardwareProfileSession,
    HardwareProfileStep, HardwareProfileStepStatus, HardwareProfileUnlockPurpose, ParentElement,
    Styled, TrezorPassphraseMode, TrezorPinMatrixPromptState, TrezorPinMatrixProvider, VaultError,
    ViewUnlock, WalletRoot, Window, WindowExt, Zeroize, Zeroizing, app_strong_text,
    default_hardware_profile_label, default_hardware_profile_steps,
    dismiss_hardware_profile_unlock_state, hardware_device_kind_from_source, hardware_device_label,
    hardware_profile_hardware_error_message, hardware_profile_should_reconnect_after_error,
    hardware_session_needs_trezor_app_passphrase, hardware_wallet_creation_result_is_current, mpsc,
    next_trezor_passphrase_mode, px, secondary_dialog_content_width, trezor_pin_matrix_provider,
    trezor_session_stale_error_message, vault_error_message,
};
#[cfg(not(feature = "hardware"))]
use super::{Context, WalletRoot};

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
        store: &DesktopVaultStore,
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

        let password =
            Self::read_and_clear_input(&self.hardware_profile_password_input, window, cx);
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
    fn dismiss_hardware_profile_unlock_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.next_hardware_profile_action_generation();
        self.manage_wallets.finish_hardware_delete_unlock_dialog();
        dismiss_hardware_profile_unlock_state(&mut self.hardware_profile_unlock);
        self.clear_hardware_profile_sensitive_inputs(window, cx);
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) const fn next_hardware_profile_action_generation(&mut self) -> u64 {
        self.hardware_wallet_creation_generation =
            self.hardware_wallet_creation_generation.wrapping_add(1);
        self.hardware_wallet_creation_generation
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) const fn hardware_profile_action_is_current(
        &self,
        generation: u64,
    ) -> bool {
        hardware_wallet_creation_result_is_current(
            self.hardware_wallet_creation_generation,
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
                self.trezor_app_passphrase_input
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
    pub(super) fn spawn_hardware_profile_progress_listener(
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
    pub(in crate::root) fn trezor_pin_matrix_provider_for_operation(
        &mut self,
        window: &Window,
        cx: &Context<'_, Self>,
    ) -> TrezorPinMatrixProvider {
        self.hardware_profile_unlock
            .clear_trezor_pin_matrix_prompt();
        let generation = self.next_hardware_profile_action_generation();
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        Self::spawn_hardware_profile_progress_listener(generation, progress_rx, window, cx);
        trezor_pin_matrix_provider(progress_tx)
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn drop_trezor_pin_matrix_prompt(&mut self) {
        self.hardware_profile_unlock
            .clear_trezor_pin_matrix_prompt();
    }

    #[cfg(not(feature = "hardware"))]
    #[allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) const fn drop_trezor_pin_matrix_prompt(&mut self) {}

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn clear_trezor_pin_matrix_prompt(&mut self, cx: &mut Context<'_, Self>) {
        self.drop_trezor_pin_matrix_prompt();
        cx.notify();
    }

    #[cfg(not(feature = "hardware"))]
    #[allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) const fn clear_trezor_pin_matrix_prompt(
        &mut self,
        _cx: &mut Context<'_, Self>,
    ) {
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) fn read_trezor_app_passphrase_for_profile_operation(
        &self,
        device_kind: HardwareDeviceKind,
        trezor_mode: TrezorPassphraseMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Zeroizing<String>> {
        if device_kind != HardwareDeviceKind::Trezor
            || trezor_mode != TrezorPassphraseMode::EnterInApp
            || self
                .hardware_profile_unlock
                .trezor_passphrase_always_on_device
                .unwrap_or(false)
        {
            self.trezor_app_passphrase_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            return None;
        }
        let passphrase = Self::read_and_clear_input(&self.trezor_app_passphrase_input, window, cx);
        (!passphrase.is_empty()).then_some(passphrase)
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn current_session_needs_trezor_app_passphrase(&self) -> bool {
        self.view_session
            .as_ref()
            .and_then(|session| session.hardware_profile_session())
            .is_some_and(hardware_session_needs_trezor_app_passphrase)
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn discard_active_trezor_session_if_stale(
        &mut self,
        message: &str,
        cx: &mut Context<'_, Self>,
    ) {
        if !trezor_session_stale_error_message(message) {
            return;
        }
        let Some(view_session) = self.view_session.as_ref() else {
            return;
        };
        let Some(mut hardware_session) = view_session.hardware_profile_session().cloned() else {
            return;
        };
        if !hardware_session.uses_trezor_app_passphrase()
            || hardware_session.trezor_session_id.is_none()
        {
            return;
        }
        hardware_session.discard_trezor_session();
        self.view_session = Some(Arc::new(
            view_session.clone_with_hardware_profile_session(hardware_session),
        ));
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn push_trezor_pin_matrix_position(
        &mut self,
        position: char,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(prompt) = self
            .hardware_profile_unlock
            .trezor_pin_matrix_prompt
            .as_mut()
        else {
            return;
        };
        if ('1'..='9').contains(&position) {
            prompt.positions.push(position);
            cx.notify();
        }
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn backspace_trezor_pin_matrix_position(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(prompt) = self
            .hardware_profile_unlock
            .trezor_pin_matrix_prompt
            .as_mut()
        else {
            return;
        };
        prompt.positions.pop();
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn clear_trezor_pin_matrix_positions(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(prompt) = self
            .hardware_profile_unlock
            .trezor_pin_matrix_prompt
            .as_mut()
        else {
            return;
        };
        prompt.positions.zeroize();
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn submit_trezor_pin_matrix_positions(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(mut prompt) = self.hardware_profile_unlock.trezor_pin_matrix_prompt.take() else {
            return;
        };
        if prompt.positions.is_empty() {
            self.hardware_profile_unlock.trezor_pin_matrix_prompt = Some(prompt);
            return;
        }
        let positions = std::mem::take(&mut prompt.positions);
        if let Some(response_tx) = prompt.response_tx.take()
            && response_tx.send(Zeroizing::new(positions)).is_err()
        {
            self.hardware_profile_unlock.error = Some(Arc::from(
                "Trezor PIN request expired. Enter your PIN, then try again.",
            ));
        }
        prompt.clear_sensitive();
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn read_trezor_app_passphrase_for_hardware_session(
        &self,
        session: &HardwareProfileSession,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Zeroizing<String>> {
        if !session.uses_trezor_app_passphrase() {
            self.trezor_app_passphrase_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            return None;
        }
        let passphrase = Self::read_and_clear_input(&self.trezor_app_passphrase_input, window, cx);
        (!passphrase.is_empty()).then_some(passphrase)
    }

    #[cfg(not(feature = "hardware"))]
    #[allow(clippy::unused_self)]
    pub(in crate::root) const fn current_session_needs_trezor_app_passphrase(&self) -> bool {
        false
    }

    #[cfg(not(feature = "hardware"))]
    #[allow(clippy::unused_self, clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) const fn discard_active_trezor_session_if_stale(
        &mut self,
        _message: &str,
        _cx: &mut Context<'_, Self>,
    ) {
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) fn handle_hardware_profile_hardware_error(
        &mut self,
        operation: &str,
        error: &HardwareDerivationError,
        awaiting_approval: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let awaiting_approval =
            awaiting_approval || self.hardware_profile_unlock.awaiting_approval();
        tracing::warn!(
            %error,
            operation,
            awaiting_approval,
            "hardware profile operation failed"
        );
        if self.hardware_profile_unlock.session.is_some()
            && hardware_profile_should_reconnect_after_error(error, awaiting_approval)
        {
            self.reconnect_hardware_profile_after_interruption(window, cx);
            return;
        }
        if matches!(error, HardwareDerivationError::MissingTrezorAppPassphrase) {
            if let Some(session) = self.hardware_profile_unlock.session.as_mut() {
                session.discard_trezor_session();
            }
            let message = Arc::from(
                "Trezor session expired or requires the app-entered passphrase again. Re-enter the passphrase in the profile picker, then retry.",
            );
            self.hardware_profile_unlock
                .mark_first_pending_progress_step_error(Arc::clone(&message));
            self.hardware_profile_unlock.error = Some(message);
            return;
        }
        let message = hardware_profile_hardware_error_message(operation, error, awaiting_approval);
        self.hardware_profile_unlock
            .mark_first_pending_progress_step_error(Arc::clone(&message));
        self.hardware_profile_unlock.error = Some(message);
    }

    #[cfg(feature = "hardware")]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reconnect_hardware_profile_after_interruption(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(device_kind) = self.hardware_profile_unlock.device_kind else {
            self.hardware_profile_unlock.error = Some(Arc::from(
                "Hardware wallet connection was interrupted. Reconnect your device, then try again.",
            ));
            return;
        };
        self.hardware_profile_unlock.session = None;
        self.hardware_profile_unlock.profile = None;
        self.hardware_profile_unlock.accounts.clear();
        self.hardware_profile_unlock.locked_accounts.clear();
        self.hardware_profile_unlock.approval_prompt = None;
        self.hardware_profile_unlock.picker_view = HardwareProfilePickerView::Summary;
        self.hardware_profile_unlock.advanced_open = false;
        self.hardware_profile_unlock.error = None;
        self.hardware_profile_unlock.progress_steps = default_hardware_profile_steps();
        self.hardware_profile_unlock.reconnect_notice = Some(Arc::from(
            "Ledger connection was interrupted. Plug it in and enter your PIN, then open the Ethereum app to reconnect.",
        ));

        cx.defer_in(window, move |root, window, cx| {
            if root.hardware_profile_unlock.device_kind == Some(device_kind)
                && root.hardware_profile_unlock.session.is_none()
                && !root.hardware_profile_unlock.in_progress
            {
                root.unlock_hardware_profile_from_dialog(window, cx);
            }
        });
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

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn open_hardware_profile_unlock_dialog_for_wallet(
        &mut self,
        wallet_id: Arc<str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_hardware_profile_unlock_dialog_for_wallet_purpose(
            wallet_id,
            HardwareProfileUnlockPurpose::Open,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn open_hardware_profile_unlock_dialog_for_wallet_deletion(
        &mut self,
        wallet_id: Arc<str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_hardware_profile_unlock_dialog_for_wallet_purpose(
            wallet_id,
            HardwareProfileUnlockPurpose::Delete,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    fn open_hardware_profile_unlock_dialog_for_wallet_purpose(
        &mut self,
        wallet_id: Arc<str>,
        purpose: HardwareProfileUnlockPurpose,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(wallet) = self
            .wallet_metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_id.as_ref())
        else {
            self.set_vault_error("Wallet metadata is unavailable", cx);
            return;
        };
        let Some(device_kind) = hardware_device_kind_from_source(wallet.source) else {
            self.set_vault_error("Selected wallet is not hardware-derived", cx);
            return;
        };
        self.open_hardware_profile_unlock_dialog(Some(wallet_id), device_kind, purpose, window, cx);
    }

    #[cfg(feature = "hardware")]
    fn open_hardware_profile_unlock_dialog(
        &mut self,
        wallet_id: Option<Arc<str>>,
        device_kind: HardwareDeviceKind,
        purpose: HardwareProfileUnlockPurpose,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        self.next_hardware_profile_action_generation();
        self.hardware_profile_unlock
            .reset_for_device(device_kind, wallet_id, purpose);
        self.clear_hardware_profile_sensitive_inputs(window, cx);
        self.hardware_profile_label_input.update(cx, |input, cx| {
            input.set_value(default_hardware_profile_label(device_kind), window, cx);
        });
        self.hardware_profile_recovery_start_input
            .update(cx, |input, cx| input.set_value("0", window, cx));
        self.hardware_profile_recovery_count_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
        self.hardware_profile_exact_index_input
            .update(cx, |input, cx| input.set_value("0", window, cx));
        let root = cx.entity();
        let device_label = hardware_device_label(device_kind);
        let viewport_size = window.viewport_size();
        let dialog_width = (viewport_size.width * 0.92).min(px(620.0));
        let dialog_max_height = viewport_size.height * 0.84;
        let content_width = secondary_dialog_content_width(dialog_width);
        let content_focus = cx.focus_handle();
        let dialog_content_focus = content_focus.clone();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height)
                .title(app_strong_text(format!("{device_label} wallet")))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.dismiss_hardware_profile_unlock_dialog(window, cx);
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_hardware_profile_unlock_dialog_content(&content_root, content_width)
                        .id("hardware-profile-unlock-content")
                        .role(gpui::accesskit::Role::Group)
                        .aria_label(format!("{device_label} wallet"))
                        .track_focus(&dialog_content_focus),
                )
        });
        content_focus.focus(window, cx);
        if self.hardware_profile_unlock_requires_password() {
            cx.defer_in(window, move |root, window, cx| {
                if root.hardware_profile_unlock.device_kind == Some(device_kind)
                    && root.hardware_profile_unlock.session.is_none()
                    && !root.hardware_profile_unlock.in_progress
                    && root.hardware_profile_unlock_requires_password()
                {
                    root.hardware_profile_password_input
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window, cx);
                }
            });
        } else if self.hardware_profile_unlock_auto_starts() {
            cx.defer_in(window, move |root, window, cx| {
                if root.hardware_profile_unlock_auto_starts()
                    && root.hardware_profile_unlock.device_kind == Some(device_kind)
                    && root.hardware_profile_unlock.session.is_none()
                    && !root.hardware_profile_unlock.in_progress
                {
                    root.unlock_hardware_profile_from_dialog(window, cx);
                }
            });
        } else if device_kind == HardwareDeviceKind::Trezor {
            cx.defer_in(window, move |root, window, cx| {
                if root.hardware_profile_unlock.device_kind == Some(device_kind)
                    && root.hardware_profile_unlock.session.is_none()
                    && !root.hardware_profile_unlock.in_progress
                {
                    root.trezor_passphrase_mode_focus.focus(window, cx);
                }
            });
        }
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root::vault) fn open_hardware_profile_unlock_dialog_for_device(
        &mut self,
        device_kind: HardwareDeviceKind,
        purpose: HardwareProfileUnlockPurpose,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_hardware_profile_unlock_dialog(None, device_kind, purpose, window, cx);
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn set_trezor_profile_passphrase_mode(
        &mut self,
        mode: TrezorPassphraseMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mode = if mode == TrezorPassphraseMode::EnterInApp
            && self
                .hardware_profile_unlock
                .trezor_passphrase_always_on_device
                .unwrap_or(false)
        {
            TrezorPassphraseMode::NoPassphrase
        } else {
            mode
        };
        self.hardware_profile_unlock.trezor_passphrase_mode = mode;
        self.hardware_profile_unlock.error = None;
        if mode != TrezorPassphraseMode::EnterInApp {
            self.trezor_app_passphrase_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.trezor_passphrase_mode_focus.focus(window, cx);
        }
        cx.notify();
        if mode == TrezorPassphraseMode::EnterInApp {
            cx.defer_in(window, |root, window, cx| {
                if root.hardware_profile_unlock.device_kind == Some(HardwareDeviceKind::Trezor)
                    && root.hardware_profile_unlock.trezor_passphrase_mode
                        == TrezorPassphraseMode::EnterInApp
                    && root.hardware_profile_unlock.session.is_none()
                    && !root.hardware_profile_unlock.in_progress
                {
                    root.trezor_app_passphrase_input
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window, cx);
                }
            });
        }
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn cycle_trezor_profile_passphrase_mode(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.hardware_profile_unlock.device_kind != Some(HardwareDeviceKind::Trezor)
            || self.hardware_profile_unlock.session.is_some()
            || self.hardware_profile_unlock.in_progress
        {
            return;
        }
        let next_mode = next_trezor_passphrase_mode(
            self.hardware_profile_unlock.trezor_passphrase_mode,
            self.hardware_profile_unlock
                .trezor_passphrase_always_on_device
                .unwrap_or(false),
        );
        self.set_trezor_profile_passphrase_mode(next_mode, window, cx);
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn submit_trezor_profile_passphrase_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.hardware_profile_unlock.device_kind != Some(HardwareDeviceKind::Trezor)
            || self.hardware_profile_unlock.trezor_passphrase_mode
                != TrezorPassphraseMode::EnterInApp
            || self.hardware_profile_unlock.session.is_some()
            || self.hardware_profile_unlock.in_progress
        {
            return;
        }

        self.unlock_hardware_profile_from_dialog(window, cx);
    }
}
