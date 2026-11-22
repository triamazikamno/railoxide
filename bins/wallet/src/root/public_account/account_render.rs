#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn render_public_wallet_body(
        &self,
        root: &Entity<Self>,
        window: &Window,
        cx: &gpui::App,
    ) -> gpui::AnyElement {
        // With Tron selected the same list shows Tron accounts; stealth accounts are EVM only.
        if !self.tron_selected()
            && let Some(view) = self.stealth_accounts_body()
        {
            return view.into_any_element();
        }
        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .w(list::dimension(list::CONTENT_WIDTH))
                    .max_w_full()
                    .h_full()
                    .min_h(px(0.0))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(self.render_public_list_controls(root))
                            .child(div().flex_1().min_w(px(0.0)))
                            .child(
                                div()
                                    .debug_selector(|| "public-walletconnect".into())
                                    .child(self.render_walletconnect_toolbar_button(root)),
                            )
                            .child(self.render_public_add_account_dropdown(root)),
                    )
                    .child(self.render_public_accounts_summary(root, cx))
                    .children(self.public_form.error.as_ref().map(|message| {
                        div()
                            .flex_none()
                            .child(Alert::error("wallet-public-error", message.to_string()).small())
                    }))
                    .child(self.render_public_account_list(root, window, cx)),
            )
            .into_any_element()
    }

    pub(in crate::root) fn render_public_add_account_dropdown(
        &self,
        root: &Entity<Self>,
    ) -> impl IntoElement {
        #[cfg(debug_assertions)]
        if Self::public_shield_error_ui_fixture_active() {
            let root = root.clone();
            let label = Self::public_shield_ui_fixture_label();
            return app_button("wallet-public-preview-shield-error", label)
                .small()
                .on_click(move |_, window, cx| {
                    root.update(cx, |root, cx| {
                        let account = root
                            .public_form
                            .selected_account_uuid
                            .clone()
                            .unwrap_or_else(|| Arc::from("ui-fixture"));
                        let asset = root.public_shield_ui_fixture_asset();
                        root.open_public_action_dialog(account, asset, window, cx);
                    });
                })
                .into_any_element();
        }
        let derive_root = root.clone();
        let import_root = root.clone();
        app_button("wallet-public-add-account-trigger", "Add account")
            .primary()
            .small()
            .dropdown_caret(true)
            .disabled(
                self.vault_store.is_none()
                    || self.view_session.is_none()
                    || self.public_form.importing_account,
            )
            .dropdown_menu(move |menu, _window, _cx| {
                let derive_root = derive_root.clone();
                let import_root = import_root.clone();
                menu.min_w(px(190.0))
                    .item(PopupMenuItem::new("Derive from private").on_click(
                        move |_event, window, cx| {
                            derive_root.update(cx, |root, cx| {
                                root.open_public_account_dialog(
                                    PublicAccountDialogKind::Derive,
                                    window,
                                    cx,
                                );
                            });
                        },
                    ))
                    .item(PopupMenuItem::new("Import private key").on_click(
                        move |_event, window, cx| {
                            import_root.update(cx, |root, cx| {
                                root.open_public_account_dialog(
                                    PublicAccountDialogKind::Import,
                                    window,
                                    cx,
                                );
                            });
                        },
                    ))
            })
            .into_any_element()
    }

    /// The label of the debug button that opens the active Shield UI fixture.
    #[cfg(debug_assertions)]
    fn public_shield_ui_fixture_label() -> &'static str {
        match crate::root::public_action::public_shield_ui_fixture() {
            Some(
                crate::root::public_action::PublicShieldUiFixture::EstimateError
                | crate::root::public_action::PublicShieldUiFixture::FundingError,
            ) => "Preview Shield failure…",
            Some(crate::root::public_action::PublicShieldUiFixture::Hints) => {
                "Preview Shield hints…"
            }
            _ => "Preview native shield…",
        }
    }

    pub(in crate::root) fn render_public_account_dialog_content(
        &self,
        root: Entity<Self>,
        kind: PublicAccountDialogKind,
        content_width: Pixels,
        device_auth_lease: Weak<Cell<bool>>,
    ) -> gpui::Div {
        match kind {
            PublicAccountDialogKind::Derive if self.tron_selected() => {
                self.render_tron_derive_content(root, content_width, device_auth_lease)
            }
            PublicAccountDialogKind::Derive => {
                self.render_derive_preview_content(root, content_width, device_auth_lease)
            }
            PublicAccountDialogKind::Import => {
                let import_root = root.clone();
                let global_root = root.clone();
                let network_root = root;
                let busy = self.public_form.importing_account || self.device_auth.in_progress;
                let tron = self.public_form.import_tron;
                div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    // The import names its network; Tron is offered while it is enabled.
                    .when(self.tron_enabled(), |content| {
                        content.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(app_muted_text("Network"))
                                .child(
                                    ButtonGroup::new("wallet-public-import-network")
                                        .outline()
                                        .compact()
                                        .disabled(busy)
                                        .child(app_segment_button(
                                            "wallet-public-import-network-evm",
                                            "EVM",
                                            !tron,
                                            busy,
                                            None,
                                        ))
                                        .child(app_segment_button(
                                            "wallet-public-import-network-tron",
                                            "Tron",
                                            tron,
                                            busy,
                                            None,
                                        ))
                                        .on_click(move |selected, _window, cx| {
                                            let tron = selected.first() == Some(&1);
                                            network_root.update(cx, |root, cx| {
                                                root.public_form.import_tron = tron;
                                                root.public_form.error = None;
                                                cx.notify();
                                            });
                                        }),
                                ),
                        )
                    })
                    .child(app_muted_text(if tron {
                        "Import a private key as a vaulted Tron account. No EVM account is added."
                    } else {
                        "Import an EVM private key as a vaulted Public account."
                    }))
                    .child(
                        app_input(&self.public_form.import_label_input)
                            .disabled(self.device_auth.in_progress),
                    )
                    .child(app_masked_input(
                        &self.public_form.import_private_key_input,
                        self.device_auth.in_progress,
                    ))
                    .child(masked_input_with_device_auth(
                        &self.public_form.import_password_input,
                        self.device_auth.in_progress,
                        self.public_account_device_auth_buttons(
                            import_root.clone(),
                            PublicAccountDialogKind::Import,
                            "wallet-public-import-touch-id",
                            self.public_form.importing_account,
                            device_auth_lease,
                        ),
                    ))
                    .child(
                        Checkbox::new("wallet-public-import-global")
                            .label("Global account")
                            .checked(self.public_form.import_global)
                            .small()
                            .disabled(self.device_auth.in_progress)
                            .on_click(move |checked, _window, cx| {
                                let checked = *checked;
                                global_root.update(cx, |root, cx| {
                                    root.public_form.import_global = checked;
                                    cx.notify();
                                });
                            }),
                    )
                    .children(self.public_form.error.as_ref().map(|message| {
                        Alert::error("wallet-public-import-error", message.to_string()).small()
                    }))
                    .child(
                        app_button(
                            "wallet-public-import-submit",
                            if self.public_form.importing_account {
                                "Importing..."
                            } else {
                                "Import account"
                            },
                        )
                        .primary()
                        .small()
                        .loading(self.public_form.importing_account)
                        .disabled(
                            self.public_form.importing_account || self.device_auth.in_progress,
                        )
                        .on_click(move |_event, window, cx| {
                            import_root.update(cx, |root, cx| {
                                root.import_public_account_from_input(window, cx);
                            });
                        }),
                    )
            }
            PublicAccountDialogKind::EditLabel => {
                let save_root = root;
                div()
                    .w(content_width)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(app_input(&self.public_form.edit_label_input))
                    .children(self.public_form.error.as_ref().map(|message| {
                        Alert::error("wallet-public-edit-label-error", message.to_string()).small()
                    }))
                    .child(
                        app_button("wallet-public-save-label", "Save")
                            .primary()
                            .small()
                            .on_click(move |_event, window, cx| {
                                save_root.update(cx, |root, cx| {
                                    root.update_selected_public_account_label(window, cx);
                                });
                            }),
                    )
            }
        }
    }
}
