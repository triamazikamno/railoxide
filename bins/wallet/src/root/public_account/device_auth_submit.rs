#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// Runs the add or import form with the vault password from device authentication.
    fn submit_public_account_with_device_auth(
        &mut self,
        method: DeviceAuthMethod,
        kind: PublicAccountDialogKind,
        lease: Weak<Cell<bool>>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.device_auth.in_progress
            || self.public_form.importing_account
            || self.public_form.derive_preview.authorizing
            || !lease.upgrade().is_some_and(|open| open.get())
        {
            return;
        }
        // Check the other fields before asking for device approval on a form that
        // cannot be submitted.
        let error = match kind {
            // Device authentication reaches Derive only for software wallets, where it
            // authorizes the address preview, which has no label.
            PublicAccountDialogKind::Derive => None,
            PublicAccountDialogKind::Import => {
                let form = &self.public_form;
                if form.import_label_input.read(cx).value().trim().is_empty() {
                    Some("Enter an account label")
                } else if form
                    .import_private_key_input
                    .read(cx)
                    .value()
                    .trim()
                    .is_empty()
                {
                    Some("Enter a private key to import an account")
                } else {
                    None
                }
            }
            PublicAccountDialogKind::EditLabel => return,
        };
        if let Some(error) = error {
            self.public_form.error = Some(Arc::from(error));
            cx.notify();
            return;
        }
        let Some(prompt) = self.device_auth_prompt() else {
            cx.notify();
            return;
        };
        self.device_auth.in_progress = true;
        self.public_form.error = None;
        let generation = self.active_wallet_generation;
        cx.notify();
        prompt.run(
            method,
            DEVICE_AUTH_REASON_PUBLIC_ACCOUNT,
            window,
            cx,
            move |root, outcome, window, cx| {
                root.finish_public_account_device_auth(
                    kind, &lease, generation, outcome, window, cx,
                );
            },
        );
    }

    pub(super) fn finish_public_account_device_auth(
        &mut self,
        kind: PublicAccountDialogKind,
        lease: &Weak<Cell<bool>>,
        generation: u64,
        outcome: DeviceAuthPassword,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.device_auth.in_progress = false;
        cx.notify();
        if !lease.upgrade().is_some_and(|open| open.get())
            || self.active_wallet_generation != generation
        {
            return;
        }
        match outcome {
            DeviceAuthPassword::Password(password) => match kind {
                PublicAccountDialogKind::Derive => {
                    self.add_public_derived_account(Some(password), window, cx);
                }
                PublicAccountDialogKind::Import => {
                    self.import_public_account(Some(password), window, cx);
                }
                PublicAccountDialogKind::EditLabel => {}
            },
            DeviceAuthPassword::Cancelled => {}
            DeviceAuthPassword::Failed(message) => {
                self.refresh_device_auth_status();
                self.public_form.error = Some(message);
            }
        }
    }

    pub(super) fn public_account_device_auth_buttons(
        &self,
        root: Entity<Self>,
        kind: PublicAccountDialogKind,
        id: &'static str,
        busy: bool,
        lease: Weak<Cell<bool>>,
    ) -> Vec<InputGroupButton> {
        device_auth_buttons(
            self.device_auth_prompt_cached().as_ref(),
            id,
            self.device_auth.in_progress,
            busy,
            move |method, window, cx| {
                root.update(cx, |root, cx| {
                    root.submit_public_account_with_device_auth(
                        method,
                        kind,
                        lease.clone(),
                        window,
                        cx,
                    );
                });
            },
        )
    }
}
