#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(super) fn render_wallet_setup(&self, root: Entity<Self>) -> gpui::AnyElement {
        match self.wallet_setup_mode {
            WalletSetupMode::Choose => self.render_wallet_setup_choice(root),
            WalletSetupMode::GeneratedReview => self.render_generated_wallet_review(root),
            WalletSetupMode::Import => self.render_import_wallet(root),
            WalletSetupMode::Hardware(device_kind) => {
                self.render_hardware_wallet_setup(root, device_kind)
            }
        }
    }

    fn render_wallet_setup_choice(&self, root: Entity<Self>) -> gpui::AnyElement {
        let generate_root = root.clone();
        let import_root = root.clone();
        let ledger_root = root.clone();
        let trezor_root = root;
        let mut body = vault_dialog_body(
            "Generate or import a Railgun recovery phrase, or derive a Railgun wallet from a Ledger or Trezor device.",
        );
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        body.child(
            app_button_base("generate-vault-wallet")
                .primary()
                .w_full()
                .child(setup_button_label(
                    setup_component_icon(IconName::File),
                    "Generate new wallet",
                ))
                .on_click(move |_event, _window, cx| {
                    generate_root.update(cx, |root, cx| {
                        root.choose_generated_wallet(cx);
                    });
                }),
        )
        .child(
            app_button_base("import-vault-wallet")
                .outline()
                .w_full()
                .child(setup_button_label(
                    setup_embedded_icon(IMPORT_ICON_PATH),
                    "Import recovery phrase",
                ))
                .on_click(move |_event, window, cx| {
                    import_root.update(cx, |root, cx| {
                        root.choose_import_wallet(window, cx);
                    });
                }),
        )
        .child(
            app_button_base("ledger-derived-vault-wallet")
                .outline()
                .w_full()
                .child(hardware_wallet_setup_button_label(
                    HardwareDeviceKind::Ledger,
                    "Ledger-derived wallet",
                ))
                .on_click(move |_event, window, cx| {
                    ledger_root.update(cx, |root, cx| {
                        root.choose_hardware_wallet(HardwareDeviceKind::Ledger, window, cx);
                    });
                }),
        )
        .child(
            app_button_base("trezor-derived-vault-wallet")
                .outline()
                .w_full()
                .child(hardware_wallet_setup_button_label(
                    HardwareDeviceKind::Trezor,
                    "Trezor-derived wallet",
                ))
                .on_click(move |_event, window, cx| {
                    trezor_root.update(cx, |root, cx| {
                        root.choose_hardware_wallet(HardwareDeviceKind::Trezor, window, cx);
                    });
                }),
        )
        .into_any_element()
    }

    fn render_generated_wallet_review(&self, root: Entity<Self>) -> gpui::AnyElement {
        let confirm_root = root.clone();
        let password_root = root.clone();
        let reveal_root = root.clone();
        let back_root = root;
        let phrase_for_clipboard = self
            .generated_seed
            .as_ref()
            .map_or_else(String::new, |seed| seed.mnemonic.to_string());
        let revealed = self.generated_seed_revealed;
        let phrase = if revealed {
            phrase_for_clipboard.clone()
        } else {
            phrase_for_clipboard
                .split_whitespace()
                .map(|_| "••••")
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut body = vault_dialog_body(
            "Write this phrase down before continuing. It is shown once and then encrypted into the vault.",
        );
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        body = body.child(app_input(&self.vault_inputs.wallet_name_input));
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            body = body.child(self.render_add_wallet_password(password_root, false));
        }

        body.child(
            div()
                .w_full()
                .p(px(14.0))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER_STRONG))
                .bg(rgb(theme::SURFACE_ELEVATED))
                .text_color(rgb(theme::WARNING))
                .text_size(APP_TEXT_SIZE)
                .line_height(px(21.0))
                .flex()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(SharedString::from(phrase)),
                )
                .when(!phrase_for_clipboard.is_empty(), |this| {
                    let visibility_label = if revealed {
                        "Hide recovery phrase"
                    } else {
                        "Show recovery phrase"
                    };
                    this.child(
                        Button::new("generated-wallet-recovery-phrase-visibility")
                            .icon(if revealed {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            })
                            .ghost()
                            .xsmall()
                            .flex_none()
                            .accessibility_label(visibility_label)
                            .tooltip(visibility_label)
                            .on_click(move |_event, _window, cx| {
                                reveal_root.update(cx, |root, cx| {
                                    root.generated_seed_revealed = !root.generated_seed_revealed;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        div()
                            .id("generated-wallet-recovery-phrase-copy-action")
                            .flex_none()
                            .tooltip(|window, cx| {
                                Tooltip::new("Copy recovery phrase").build(window, cx)
                            })
                            .child(secret_clipboard_with_toast(
                                "generated-wallet-recovery-phrase-copy",
                                phrase_for_clipboard,
                            )),
                    )
                }),
        )
        .child(
            app_button("confirm-generated-wallet", "I saved it, create wallet")
                .primary()
                .w_full()
                .on_click(move |_event, window, cx| {
                    confirm_root.update(cx, |root, cx| {
                        root.store_generated_wallet(window, cx);
                    });
                }),
        )
        .child(
            app_button("back-generated-wallet", "Back")
                .ghost()
                .w_full()
                .on_click(move |_event, window, cx| {
                    back_root.update(cx, |root, cx| {
                        root.back_to_wallet_setup_choice(window, cx);
                    });
                }),
        )
        .into_any_element()
    }

    fn render_import_wallet(&self, root: Entity<Self>) -> gpui::AnyElement {
        let import_root = root.clone();
        let password_root = root.clone();
        let back_root = root;
        let mut body = vault_dialog_body(
            "Paste the recovery phrase. The phrase is validated, converted to canonical entropy, and cleared from the input.",
        );
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        body.child(app_input(&self.vault_inputs.wallet_name_input))
            .when(
                matches!(self.vault_state, VaultState::ViewUnlocked),
                |this| this.child(self.render_add_wallet_password(password_root, false)),
            )
            .child(
                gpui_component::input::Textarea::new(&self.vault_inputs.import_mnemonic_input)
                    .bg(rgb(theme::SURFACE))
                    .w_full()
                    .px(px(8.0)),
            )
            .child(
                app_button("store-imported-wallet", "Import wallet")
                    .primary()
                    .w_full()
                    .on_click(move |_event, window, cx| {
                        import_root.update(cx, |root, cx| {
                            root.store_imported_wallet(window, cx);
                        });
                    }),
            )
            .child(
                app_button("back-import-wallet", "Back")
                    .ghost()
                    .w_full()
                    .on_click(move |_event, window, cx| {
                        back_root.update(cx, |root, cx| {
                            root.back_to_wallet_setup_choice(window, cx);
                        });
                    }),
            )
            .into_any_element()
    }
}
