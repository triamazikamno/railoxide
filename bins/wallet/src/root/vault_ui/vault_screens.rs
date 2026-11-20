#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) const fn titlebar_color(&self) -> u32 {
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            theme::SURFACE
        } else {
            theme::BACKGROUND
        }
    }

    pub(in crate::root) fn render_locked_vault_screen(
        &self,
        root: Entity<Self>,
        window: &Window,
    ) -> gpui::Div {
        let card = self.render_vault_card(root.clone());
        let update_check = Self::update_check_trigger(root.clone());
        render_wallet_hero_screen(window, card, Some(update_check)).when(
            should_show_pre_unlock_settings_action(&self.vault_state),
            |this| this.child(Self::render_pre_unlock_settings_gear(root)),
        )
    }

    pub(in crate::root) fn render_add_wallet_dialog_content(
        &self,
        root: Entity<Self>,
        content_width: gpui::Pixels,
    ) -> gpui::AnyElement {
        div()
            .w(content_width)
            .child(self.render_wallet_setup(root))
            .into_any_element()
    }

    fn vault_dialog_title(&self) -> &'static str {
        match &self.vault_state {
            VaultState::CreateVault => "Create wallet vault",
            VaultState::UnlockVault => "Unlock wallet",
            VaultState::SwitchingWallet => "Opening wallet",
            VaultState::SetupWallet => match self.wallet_setup_mode {
                WalletSetupMode::Choose => "Add your first wallet",
                WalletSetupMode::GeneratedReview => "Save recovery phrase",
                WalletSetupMode::Import => "Import wallet",
                WalletSetupMode::Hardware(HardwareDeviceKind::Ledger) => "Ledger-derived wallet",
                WalletSetupMode::Hardware(HardwareDeviceKind::Trezor) => "Trezor-derived wallet",
            },
            VaultState::PendingSoftwareProfileOpen => {
                pending_software_profile_open_title(self.pending_software_profile_open_stage())
            }
            VaultState::ViewUnlocked => "Wallet",
            VaultState::Error(_) => "Wallet vault unavailable",
        }
    }

    fn render_vault_dialog_content(&self, root: Entity<Self>) -> gpui::AnyElement {
        match &self.vault_state {
            VaultState::CreateVault => self.render_create_vault(root).into_any_element(),
            VaultState::UnlockVault => self.render_unlock_vault(root).into_any_element(),
            VaultState::SwitchingWallet => {
                Self::render_switching_wallet(self.wallet_switch_delayed).into_any_element()
            }
            VaultState::SetupWallet => self.render_wallet_setup(root).into_any_element(),
            VaultState::PendingSoftwareProfileOpen => {
                self.passphrase_open_ui.clone().into_any_element()
            }
            VaultState::ViewUnlocked => div().into_any_element(),
            VaultState::Error(message) => self.render_vault_fatal(message).into_any_element(),
        }
    }

    fn render_vault_card(&self, root: Entity<Self>) -> gpui::AnyElement {
        div()
            .w_full()
            .p(px(28.0))
            .flex()
            .flex_col()
            .gap_5()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER_STRONG))
            .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.86))
            .child(
                app_strong_text(self.vault_dialog_title())
                    .text_size(px(22.0))
                    .line_height(px(28.0)),
            )
            .child(self.render_vault_dialog_content(root))
            .into_any_element()
    }

    fn render_switching_wallet(delayed: bool) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(Spinner::new().small())
            .child(app_muted_shimmer_text(if delayed {
                "Opening your wallet is taking longer than expected…"
            } else {
                "Closing the previous wallet session…"
            }))
    }

    fn render_pre_unlock_settings_gear(root: Entity<Self>) -> gpui::Div {
        div().absolute().right(px(24.0)).bottom(px(24.0)).child(
            app_button_base("pre-unlock-wallet-settings")
                .outline()
                .h(px(40.0))
                .w(px(40.0))
                .accessibility_label("Settings")
                .tooltip("Settings")
                .icon(IconName::Settings)
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.open_pre_unlock_settings_dialog(window, cx);
                    });
                }),
        )
    }

    fn open_pre_unlock_settings_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        self.refresh_device_auth_status();
        self.invalidate_settings_data_folder_size(cx);
        cx.notify();
        window.close_all_dialogs(cx);
        let (dialog_width, content_height, dialog_max_height) = settings_dialog_dimensions(window);
        let editor = self.settings_editor.clone();
        let settings_error = self.settings_error.clone();
        let root = cx.entity();
        let data_folder = data_folder_path(&self.options.db_path);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let content = if let Some(editor) = editor.clone() {
                div()
                    .h(content_height)
                    .min_h(px(0.0))
                    .child(editor)
                    .into_any_element()
            } else {
                let open_root = root.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(app_muted_text(
                        "Settings are stored in the selected wallet database and are readable before vault unlock.",
                    ))
                    .child(app_muted_text(settings_error.as_ref().map_or_else(
                        || "Settings are unavailable".to_string(),
                        ToString::to_string,
                    )))
                    .child(data_folder_row(
                        &data_folder,
                        None,
                        true,
                        root.read(cx).data_folder_open_error.clone(),
                        move |_window, cx| {
                            open_root.update(cx, Self::open_settings_data_folder);
                        },
                    ))
                    .into_any_element()
            };
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .margin_top(px(16.0))
                .title(app_strong_text("Settings"))
                .on_ok(|_, _, _| false)
                .child(content)
        });
    }

    pub(in crate::root) fn open_settings_from_shortcut(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            window.close_all_dialogs(cx);
            self.clear_settings_transient_status(cx);
            self.invalidate_settings_data_folder_size(cx);
            self.refresh_device_auth_status();
            self.active_activity = Activity::Settings;
            cx.notify();
        } else if should_show_pre_unlock_settings_action(&self.vault_state) {
            self.open_pre_unlock_settings_dialog(window, cx);
        }
    }

    pub(in crate::root) fn lock_vault_from_shortcut(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            self.lock_vault(window, cx);
        }
    }

    fn render_create_vault(&self, root: Entity<Self>) -> gpui::Div {
        let submit_root = root.clone();
        let mut body = vault_dialog_body(
            "Choose one password for this desktop wallet vault. It will be required every time the app starts.",
        );
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        let device_auth_root = root.clone();
        body.child(app_masked_input(
            &self.vault_inputs.new_password_input,
            false,
        ))
        .child(app_masked_input(
            &self.vault_inputs.confirm_password_input,
            false,
        ))
        .when_some(self.vault_creation_device_auth_method(), |this, method| {
            this.child(
                Checkbox::new("create-wallet-vault-device-auth")
                    .label(format!("Also unlock with {} on this Mac", method.label()))
                    .checked(self.device_auth.enable_device_auth_on_create)
                    .small()
                    .on_click(move |checked, _window, cx| {
                        let checked = *checked;
                        device_auth_root.update(cx, |root, cx| {
                            root.set_enable_device_auth_on_create(checked, cx);
                        });
                    }),
            )
        })
        .child(
            app_button("create-wallet-vault", "Create vault")
                .primary()
                .w_full()
                .on_click(move |_event, window, cx| {
                    submit_root.update(cx, |root, cx| {
                        root.create_vault_from_inputs(window, cx);
                    });
                }),
        )
        .child(self.render_create_vault_cache_offer(root))
    }

    fn render_create_vault_cache_offer(&self, root: Entity<Self>) -> gpui::Div {
        let start_root = root;
        let progress = self.prover_cache_build.progress.clone();
        let completed = self.prover_cache_build.completed;
        let percent = progress
            .as_ref()
            .map_or(0, wallet_ops::ProverCacheBuildProgress::percent);
        let progress_text = progress.as_ref().map(|progress| {
            if progress.total_variants == 0 {
                "Preparing prover cache...".to_string()
            } else {
                format!(
                    "Building prover cache: {} of {} variants complete ({percent}%)",
                    progress.completed_variants, progress.total_variants
                )
            }
        });

        div()
            .w_full()
            .mt(px(4.0))
            .p(px(14.0))
            .flex()
            .flex_col()
            .gap_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb_with_alpha(theme::SURFACE, 0.72))
            .child(app_strong_text("Prepare prover cache in the background"))
            .child(app_muted_text(
                "Private transactions and POI proofs need prover artifacts. Building the cache now parses zkeys and compiles circuit WASM ahead of time. It can take a long time and uses about 2.5 GB of disk space in this wallet database.",
            ))
            .child(app_muted_text(
                "If you skip this, nothing breaks. The wallet will build missing cache entries on demand, so the first private action for each circuit may take much longer.",
            ))
            .when_some(progress_text, |this, text| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            app_muted_text(SharedString::from(text))
                                .text_color(rgb(theme::INFO)),
                        )
                        .child(
                            UiProgress::new("create-vault-prover-cache-progress")
                                .h(px(7.0))
                                .value(f32::from(percent)),
                        ),
                )
            })
            .when(progress.is_none() && !completed, |this| {
                this.child(
                    app_button("create-vault-build-prover-cache", "Build cache in background")
                        .outline()
                        .w_full()
                        .on_click(move |_event, _window, cx| {
                            start_root.update(cx, |root, cx| {
                                root.start_background_prover_cache_build(cx);
                            });
                        }),
                )
            })
            .when(progress.is_some(), |this| {
                this.child(
                    app_button("create-vault-build-prover-cache-running", "Building prover cache")
                        .outline()
                        .w_full()
                        .loading(true)
                        .disabled(true),
                )
            })
            .when(completed && progress.is_none(), |this| {
                this.child(
                    app_muted_text("Prover cache is ready for this wallet database.")
                        .text_color(rgb(theme::SUCCESS)),
                )
            })
            .child(app_muted_text(
                "You can also start this later from Settings.",
            ))
    }

    pub(super) fn render_unlock_vault(&self, root: Entity<Self>) -> gpui::Div {
        let submit_root = root.clone();
        let device_auth_root = root;
        let device_auth = self.device_auth_prompt_cached().is_some();
        let busy = self.unlock_in_progress || self.device_auth.in_progress;
        let mut body = vault_dialog_body(if device_auth {
            "Use an enabled device or enter the vault password to view wallet balances and history."
        } else {
            "Enter the vault password to view wallet balances and history."
        });
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        let password = masked_input_with_device_auth(
            &self.vault_inputs.unlock_password_input,
            busy,
            device_auth_buttons(
                self.device_auth_prompt_cached().as_ref(),
                "unlock-wallet-vault-touch-id",
                self.device_auth.in_progress,
                self.unlock_in_progress,
                move |method, window, cx| {
                    device_auth_root.update(cx, |root, cx| {
                        root.unlock_vault_with_device_auth(method, window, cx);
                    });
                },
            ),
        );

        body.child(password).child(
            app_button("unlock-wallet-vault", "Unlock vault")
                .primary()
                .w_full()
                .loading(self.unlock_in_progress)
                .disabled(busy)
                .on_click(move |_event, window, cx| {
                    submit_root.update(cx, |root, cx| {
                        root.unlock_vault_from_input(window, cx);
                    });
                }),
        )
    }

    fn render_vault_fatal(&self, message: &str) -> gpui::Div {
        let mut body = vault_dialog_body(SharedString::from(message.to_owned()));
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }
        body
    }

    pub(super) fn render_vault_error(&self) -> Option<gpui::AnyElement> {
        self.vault_error.as_ref().map(|message| {
            Alert::error("wallet-vault-error", message.to_string())
                .small()
                .into_any_element()
        })
    }
}
