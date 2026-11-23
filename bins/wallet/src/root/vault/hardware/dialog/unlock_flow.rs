#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
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
        tracing::warn!(target: LOG_TARGET,
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
    pub(in crate::root) fn open_hardware_profile_unlock_dialog_for_wallet(
        &mut self,
        wallet_id: Arc<str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_hardware_profile_unlock_dialog_for_wallet_purpose(
            wallet_id,
            HardwareProfileUnlockPurpose::Open,
            None,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn open_hardware_profile_unlock_dialog_for_wallet_continuing(
        &mut self,
        wallet_id: Arc<str>,
        gateway_unlock: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_hardware_profile_unlock_dialog_for_wallet_purpose(
            wallet_id,
            HardwareProfileUnlockPurpose::Open,
            gateway_unlock,
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
            None,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    fn open_hardware_profile_unlock_dialog_for_wallet_purpose(
        &mut self,
        wallet_id: Arc<str>,
        purpose: HardwareProfileUnlockPurpose,
        gateway_unlock: Option<u64>,
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
        self.open_hardware_profile_unlock_dialog(
            Some(wallet_id),
            device_kind,
            purpose,
            gateway_unlock,
            window,
            cx,
        );
    }

    #[cfg(feature = "hardware")]
    fn open_hardware_profile_unlock_dialog(
        &mut self,
        wallet_id: Option<Arc<str>>,
        device_kind: HardwareDeviceKind,
        purpose: HardwareProfileUnlockPurpose,
        gateway_unlock: Option<u64>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if purpose != HardwareProfileUnlockPurpose::Open {
            self.retire_gateway_unlock();
        } else if !self.gateway_unlock_is_current(gateway_unlock) {
            let _ = self.begin_gateway_unlock();
        }
        let gateway_unlock = self.gateway_unlock_continuation();
        window.close_all_dialogs(cx);
        self.wallet_switch_generation = self.wallet_switch_generation.wrapping_add(1);
        self.next_hardware_profile_action_generation();
        self.hardware_profile_unlock
            .reset_for_device(device_kind, wallet_id, purpose);
        let lease = Rc::new(Cell::new(true));
        self.hardware_profile_unlock.dialog_lease = Rc::downgrade(&lease);
        self.clear_hardware_profile_sensitive_inputs(window, cx);
        self.hardware_profile.label_input.update(cx, |input, cx| {
            input.set_value(default_hardware_profile_label(device_kind), window, cx);
        });
        self.hardware_profile
            .recovery_start_input
            .update(cx, |input, cx| input.set_value("0", window, cx));
        self.hardware_profile
            .recovery_count_input
            .update(cx, |input, cx| input.set_value("1", window, cx));
        self.hardware_profile
            .exact_index_input
            .update(cx, |input, cx| input.set_value("0", window, cx));
        let root = cx.entity();
        let device_label = hardware_device_label(device_kind);
        let viewport_size = window.viewport_size();
        let dialog_width = (viewport_size.width * 0.92).min(px(620.0));
        let dialog_max_height = viewport_size.height * 0.84;
        let content_width = secondary_dialog_content_width(dialog_width);
        let content_focus = cx.focus_handle();
        let dialog_content_focus = content_focus.clone();
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let identity = Rc::downgrade(&lease);
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height)
                .title(app_strong_text(format!("{device_label} wallet")))
                .on_close(move |_event, window, cx| {
                    if let Some(open) = identity.upgrade() {
                        open.set(false);
                    }
                    close_root.update(cx, |root, cx| {
                        root.dismiss_hardware_profile_unlock_dialog(gateway_unlock, window, cx);
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_hardware_profile_unlock_dialog_body(&content_root, content_width)
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
                    root.hardware_profile
                        .password_input
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
                    root.hardware_profile
                        .trezor_passphrase_mode_focus
                        .focus(window, cx);
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
        self.open_hardware_profile_unlock_dialog(None, device_kind, purpose, None, window, cx);
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
            self.hardware_profile
                .trezor_app_passphrase_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.hardware_profile
                .trezor_passphrase_mode_focus
                .focus(window, cx);
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
                    root.hardware_profile
                        .trezor_app_passphrase_input
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
