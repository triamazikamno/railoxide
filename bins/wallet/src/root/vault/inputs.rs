use super::{
    Context, Entity, Focusable, InputState, PRIMARY_WALLET_LABEL, VaultState, WalletRoot,
    WalletSetupMode, Window, Zeroizing,
};

pub(in crate::root) const fn should_focus_vault_input(
    vault_state: &VaultState,
    wallet_setup_mode: WalletSetupMode,
) -> bool {
    matches!(
        vault_state,
        VaultState::CreateVault | VaultState::UnlockVault
    ) || matches!(
        (vault_state, wallet_setup_mode),
        (VaultState::SetupWallet, WalletSetupMode::Import)
    )
}

impl WalletRoot {
    pub(in crate::root) fn set_wallet_name_input(
        &self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let value = value.to_owned();
        self.wallet_name_input
            .update(cx, |input, cx| input.set_value(&value, window, cx));
    }

    pub(super) fn set_default_wallet_name_from_password(
        &self,
        password: &str,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let label = self
            .vault_store
            .as_ref()
            .and_then(|store| store.default_wallet_label(password).ok())
            .unwrap_or_else(|| PRIMARY_WALLET_LABEL.to_owned());
        Self::defer_wallet_name_input(label, window, cx);
    }

    pub(super) fn defer_wallet_name_input(
        value: String,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.defer_in(window, move |root, window, cx| {
            root.set_wallet_name_input(&value, window, cx);
        });
    }

    pub(super) fn clear_hardware_wallet_restore_account_index(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.hardware_wallet_restore_account_index_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.hardware_wallet_restore_account_index_set = false;
    }

    #[cfg(feature = "hardware")]
    pub(super) fn clear_hardware_profile_sensitive_inputs(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.hardware_profile_password_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.clear_trezor_app_passphrase_input(window, cx);
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn clear_trezor_app_passphrase_input(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.trezor_app_passphrase_input
            .update(cx, |input, cx| input.set_value("", window, cx));
    }

    #[cfg(not(feature = "hardware"))]
    #[allow(clippy::unused_self)]
    pub(in crate::root) const fn clear_trezor_app_passphrase_input(
        &self,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) {
    }

    pub(in crate::root) fn focus_vault_input_if_requested(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.focus_vault_input_on_render
            || !should_focus_vault_input(&self.vault_state, self.wallet_setup_mode)
        {
            self.focus_vault_input_on_render = false;
            return;
        }

        match self.vault_state {
            VaultState::CreateVault => self
                .new_password_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            VaultState::UnlockVault => self
                .unlock_password_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            VaultState::SetupWallet if self.wallet_setup_mode == WalletSetupMode::Import => self
                .import_mnemonic_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            VaultState::SetupWallet
            | VaultState::SwitchingWallet
            | VaultState::PendingSoftwareProfileOpen
            | VaultState::ViewUnlocked
            | VaultState::Error(_) => {}
        }
        self.focus_vault_input_on_render = false;
    }

    pub(in crate::root) fn read_and_clear_input(
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Zeroizing<String> {
        let value = Zeroizing::new(input.read(cx).value().to_string());
        input.update(cx, |input, cx| input.set_value("", window, cx));
        value
    }
}
