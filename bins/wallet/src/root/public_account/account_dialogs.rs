#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn open_public_account_dialog(
        &mut self,
        kind: PublicAccountDialogKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Weak<Cell<bool>> {
        window.close_all_dialogs(cx);
        self.public_form.error = None;
        // Programmatic closure skips on_close, so a replaced Derive dialog's preview ends here.
        self.public_form.derive_preview.reset();
        self.clear_public_account_dialog_inputs(kind, window, cx);
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(PUBLIC_ACCOUNT_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let lease = Rc::new(Cell::new(true));
        let identity = Rc::downgrade(&lease);
        self.public_form.derive_preview.lease = identity.clone();
        // Closing the dialog by any path drops its builder and this entity with it, so the
        // preview's key goes with the dialog even when on_close doesn't run.
        let builder_alive = cx.new(|_| ());
        let released_lease = identity.clone();
        cx.observe_release(&builder_alive, move |root, (), _| {
            let state = &mut root.public_form.derive_preview;
            if Weak::ptr_eq(&state.lease, &released_lease) {
                state.reset();
            }
        })
        .detach();
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let _ = &builder_alive;
            // Only the active builder owns the lease. Programmatic closure skips on_close.
            let identity = Rc::downgrade(&lease);
            let close_identity = identity.clone();
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height)
                .title(app_strong_text(kind.title()))
                .on_close(move |_event, window, cx| {
                    if let Some(lease) = close_identity.upgrade() {
                        lease.set(false);
                    }
                    close_root.update(cx, |root, cx| {
                        root.public_form.error = None;
                        root.clear_public_account_dialog_inputs(kind, window, cx);
                    });
                })
                .child(content_root.read(cx).render_public_account_dialog_content(
                    content_root.clone(),
                    kind,
                    content_width,
                    identity,
                ))
        });
        cx.defer_in(window, move |root, window, cx| {
            root.focus_public_account_dialog_input(kind, window, cx);
        });
        identity
    }

    pub(in crate::root) fn open_public_account_edit_dialog(
        &mut self,
        public_account_uuid: Arc<str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.public_form.error = None;
        self.public_form.editing_account_uuid = Some(public_account_uuid);
        self.sync_public_edit_label_input(window, cx);
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(PUBLIC_ACCOUNT_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(dialog_max_height)
                .title(app_strong_text(PublicAccountDialogKind::EditLabel.title()))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.public_form.error = None;
                        root.clear_public_account_dialog_inputs(
                            PublicAccountDialogKind::EditLabel,
                            window,
                            cx,
                        );
                    });
                })
                .child(content_root.read(cx).render_public_account_dialog_content(
                    content_root.clone(),
                    PublicAccountDialogKind::EditLabel,
                    content_width,
                    Weak::new(),
                ))
        });
        cx.defer_in(window, |root, window, cx| {
            root.focus_public_account_dialog_input(PublicAccountDialogKind::EditLabel, window, cx);
        });
    }

    /// Receive's label, full address (also the QR payload) and the note naming its network.
    pub(in crate::root) fn public_receive_text(
        &self,
        account: &PublicAccountMetadata,
    ) -> (Option<String>, String, String) {
        let warning = if self.tron_selected() {
            tron::TRON_RECEIVE_WARNING.to_owned()
        } else {
            let chain_label = chain_name(self.last_evm_chain)
                .map_or_else(|| format!("chain {}", self.last_evm_chain), str::to_owned);
            format!("Send only public {chain_label} assets to this address.")
        };
        (
            public_account_display_label(account),
            self.listed_address(account),
            warning,
        )
    }

    /// Receive: the account's label, full address, its QR code, a copy action and the network
    /// the address is for.
    pub(in crate::root) fn open_public_address_qr_dialog(
        &self,
        account: &PublicAccountMetadata,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        let dialog_width =
            (window.viewport_size().width * 0.92).min(PUBLIC_ADDRESS_QR_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let (label, address, warning) = self.public_receive_text(account);
        let address_text = SharedString::from(address);
        let account_label = label.map(SharedString::from);
        let copy_id = SharedString::from(format!(
            "wallet-public-address-qr-copy-{}",
            account.public_account_uuid
        ));
        let receive_warning = SharedString::from(warning);
        let root = cx.entity().downgrade();
        let generation = self.active_wallet_generation;
        window.open_wallet_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Public account address"))
                .child(render_public_address_qr_dialog_content(
                    account_label.clone(),
                    address_text.clone(),
                    Some(receive_warning.clone()),
                    copy_id.clone(),
                    content_width,
                    {
                        let root = root.clone();
                        let address = address_text.clone();
                        move |window, cx| {
                            if root.upgrade().is_some_and(|root| {
                                let root = root.read(cx);
                                root.active_wallet_generation == generation
                                    && root.view_session.is_some()
                            }) {
                                ui::clipboard::copy_to_clipboard_with_toast(
                                    address.clone(),
                                    window,
                                    cx,
                                );
                            }
                        }
                    },
                ))
        });
    }

    fn focus_public_account_dialog_input(
        &self,
        kind: PublicAccountDialogKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            // Tron's hardware add has no input to focus.
            PublicAccountDialogKind::Derive
                if self.tron_selected()
                    && self.selected_hardware_public_device_kind().is_some() => {}
            // A hardware wallet's authorize state has no password, so Enter reaches Show addresses.
            PublicAccountDialogKind::Derive
                if self.selected_hardware_public_device_kind().is_some() =>
            {
                self.public_form.derive_preview.focus.focus(window, cx);
            }
            PublicAccountDialogKind::Derive => self
                .public_form
                .add_password_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            PublicAccountDialogKind::Import => self
                .public_form
                .import_private_key_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            PublicAccountDialogKind::EditLabel => self
                .public_form
                .edit_label_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
        }
    }

    pub(in crate::root) fn clear_public_account_dialog_inputs(
        &mut self,
        kind: PublicAccountDialogKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let default_label = public_account_default_label(if self.tron_selected() {
            next_public_account_label_number(self.tron_public.accounts.len())
        } else {
            self.public_form.next_account_label_number
        });
        match kind {
            PublicAccountDialogKind::Derive => {
                self.public_form.derive_preview.reset();
                self.public_form
                    .derive_preview
                    .go_to_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.public_form
                    .add_password_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.clear_trezor_app_passphrase_input(window, cx);
                self.clear_trezor_pin_matrix_prompt(cx);
            }
            PublicAccountDialogKind::Import => {
                self.public_form
                    .import_label_input
                    .update(cx, |input, cx| input.set_value(&default_label, window, cx));
                self.public_form
                    .import_private_key_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.public_form
                    .import_password_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.public_form.import_global = false;
                self.public_form.import_tron = self.tron_selected();
            }
            PublicAccountDialogKind::EditLabel => {
                self.public_form.editing_account_uuid = None;
                self.public_form
                    .edit_label_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
        }
    }
}
