#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
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
            self.hardware_profile
                .trezor_app_passphrase_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            return None;
        }
        let passphrase = Self::read_and_clear_input(
            &self.hardware_profile.trezor_app_passphrase_input,
            window,
            cx,
        );
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
        // A discarded Trezor session ends a hardware address preview like a cleared one.
        self.public_form
            .derive_preview
            .end_hardware_preview_unless(None);
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
            self.hardware_profile
                .trezor_app_passphrase_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            return None;
        }
        let passphrase = Self::read_and_clear_input(
            &self.hardware_profile.trezor_app_passphrase_input,
            window,
            cx,
        );
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
}
