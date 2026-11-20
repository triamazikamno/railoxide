#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn add_public_derived_account_from_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.device_auth.in_progress {
            return;
        }
        self.add_public_derived_account(None, window, cx);
    }

    pub(super) fn add_public_derived_account(
        &mut self,
        device_auth_password: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.tron_selected() {
            self.add_derived_tron_account(device_auth_password, window, cx);
            return;
        }
        match self.selected_hardware_public_device_kind() {
            Some(device_kind) => self.authorize_hardware_derive_preview(device_kind, window, cx),
            None => self.authorize_derive_preview(device_auth_password, window, cx),
        }
    }

    pub(super) fn selected_hardware_public_device_kind(&self) -> Option<HardwareDeviceKind> {
        if crate::root::private_swap::ui_fixture_hardware_derive() {
            return Some(HardwareDeviceKind::Ledger);
        }
        match self.selected_wallet_source() {
            WalletSource::LedgerDerived => Some(HardwareDeviceKind::Ledger),
            WalletSource::TrezorDerived => Some(HardwareDeviceKind::Trezor),
            WalletSource::Generated | WalletSource::Imported => None,
        }
    }

    pub(in crate::root) fn import_public_account_from_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.device_auth.in_progress {
            return;
        }
        self.import_public_account(None, window, cx);
    }

    pub(super) fn import_public_account(
        &mut self,
        device_auth_password: Option<Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.importing_account {
            return;
        }
        let Some(store) = self.vault_store.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        let label = self
            .public_form
            .import_label_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if label.is_empty() {
            self.public_form.error = Some(Arc::from("Enter an account label"));
            cx.notify();
            return;
        }
        let private_key =
            Self::read_and_clear_input(&self.public_form.import_private_key_input, window, cx);
        let password = match device_auth_password {
            Some(password) => password,
            None => Self::read_and_clear_input(&self.public_form.import_password_input, window, cx),
        };
        if private_key.trim().is_empty() || password.trim().is_empty() {
            self.public_form.error = Some(Arc::from(
                "Enter a private key and vault password to import an account",
            ));
            cx.notify();
            return;
        }
        let global = self.public_form.import_global;
        if self.public_form.import_tron {
            self.public_form.error = None;
            self.import_tron_account(
                password.as_str(),
                private_key.as_str(),
                &label,
                global,
                window,
                cx,
            );
            cx.notify();
            return;
        }
        self.public_form.importing_account = true;
        self.public_form.error = None;
        let result = store.import_public_account(
            password.as_str(),
            view_session.as_ref(),
            private_key.as_str(),
            Some(&label),
            global,
        );
        self.public_form.importing_account = false;
        match result {
            Ok(account) => {
                self.public_form.selected_account_uuid =
                    Some(Arc::from(account.public_account_uuid.as_str()));
                self.public_form
                    .import_label_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.public_form.import_global = false;
                self.reload_public_accounts(window, cx);
                self.schedule_account_public_balance_refresh(
                    self.last_evm_chain,
                    [account.public_account_uuid],
                    None,
                    cx,
                );
                window.close_all_dialogs(cx);
            }
            Err(error) => {
                self.public_form.error = Some(Arc::from(error.to_string()));
            }
        }
        cx.notify();
    }
}
