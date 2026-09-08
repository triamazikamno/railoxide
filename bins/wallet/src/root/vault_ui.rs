#[cfg(feature = "hardware")]
use std::sync::Arc;

use gpui::{
    Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::alert::Alert;
use gpui_component::progress::Progress as UiProgress;
use gpui_component::spinner::Spinner;
use gpui_component::{Disableable, IconName, WindowExt, button::ButtonVariants, tooltip::Tooltip};
use gpui_component::{Icon, Sizable};
#[cfg(feature = "hardware")]
use gpui_component::{
    Selectable,
    button::{Button, ButtonGroup},
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{
    app_button, app_button_base, app_button_label, app_input, app_masked_input, app_muted_text,
    app_strong_text,
};
#[cfg(feature = "hardware")]
use ui::theme::APP_MONO_FONT_FAMILY;
use ui::theme::{self, APP_TEXT_SIZE};
use wallet_ops::hardware::{HardwareDeviceKind, HardwareWalletSyncIntent};
#[cfg(feature = "hardware")]
use wallet_ops::vault::TrezorPassphraseMode;

#[cfg(feature = "hardware")]
use super::actions::{CycleTrezorPassphraseMode, TREZOR_PASSPHRASE_MODE_KEY_CONTEXT};
use super::settings::settings_dialog_dimensions;
use super::shell::render_wallet_hero_screen;
#[cfg(feature = "hardware")]
use super::vault::{
    HardwareProfileApprovalPrompt, HardwareProfilePickerView, HardwareProfileStep,
    HardwareProfileStepState, HardwareProfileStepStatus,
};
use super::vault::{PendingSoftwareProfileOpenStage, hardware_device_label};
use super::{Activity, VaultState, WalletRoot, WalletSetupMode, labeled_field, rgb_with_alpha};
#[cfg(feature = "hardware")]
use crate::assets::RailgunActionIcon;
use crate::assets::{
    IMPORT_ICON_PATH, LEDGER_LOGO_SHORT_WHITE_ICON_PATH, TREZOR_SYMBOL_WHITE_ICON_PATH,
};

#[cfg(feature = "hardware")]
mod trezor;
#[cfg(feature = "hardware")]
use trezor::trezor_pin_matrix_title;

impl WalletRoot {
    pub(super) const fn titlebar_color(&self) -> u32 {
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            theme::SURFACE
        } else {
            theme::BACKGROUND
        }
    }

    pub(super) fn render_locked_vault_screen(
        &self,
        root: Entity<Self>,
        window: &Window,
    ) -> gpui::Div {
        let card = self.render_vault_card(root.clone());
        render_wallet_hero_screen(window, card).when(
            should_show_pre_unlock_settings_action(&self.vault_state),
            |this| this.child(Self::render_pre_unlock_settings_gear(root)),
        )
    }

    pub(super) fn render_add_wallet_dialog_content(
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
            .child(app_muted_text(if delayed {
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
        &self,
        window: &mut Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        let (dialog_width, content_height, dialog_max_height) = settings_dialog_dimensions(window);
        let editor = self.settings_editor.clone();
        let settings_error = self.settings_error.clone();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let content = if let Some(editor) = editor.clone() {
                div()
                    .h(content_height)
                    .min_h(px(0.0))
                    .child(editor)
                    .into_any_element()
            } else {
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

    pub(super) fn open_settings_from_shortcut(
        &mut self,
        window: &mut Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            window.close_all_dialogs(cx);
            self.clear_settings_transient_status(cx);
            self.active_activity = Activity::Settings;
            cx.notify();
        } else if should_show_pre_unlock_settings_action(&self.vault_state) {
            self.open_pre_unlock_settings_dialog(window, cx);
        }
    }

    pub(super) fn lock_vault_from_shortcut(
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

        body.child(app_masked_input(&self.new_password_input, false))
            .child(app_masked_input(&self.confirm_password_input, false))
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
        let progress = self.prover_cache_build_progress.clone();
        let completed = self.prover_cache_build_completed;
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

    fn render_unlock_vault(&self, root: Entity<Self>) -> gpui::Div {
        let submit_root = root;
        let mut body =
            vault_dialog_body("Enter the vault password to view wallet balances and history.");
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        body.child(app_masked_input(
            &self.unlock_password_input,
            self.unlock_in_progress,
        ))
        .child(
            app_button("unlock-wallet-vault", "Unlock vault")
                .primary()
                .w_full()
                .loading(self.unlock_in_progress)
                .disabled(self.unlock_in_progress)
                .on_click(move |_event, window, cx| {
                    submit_root.update(cx, |root, cx| {
                        root.unlock_vault_from_input(window, cx);
                    });
                }),
        )
    }

    fn render_wallet_setup(&self, root: Entity<Self>) -> gpui::AnyElement {
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
        let back_root = root;
        let phrase = self
            .generated_seed
            .as_ref()
            .map_or_else(String::new, |seed| seed.mnemonic.to_string());
        let phrase_for_clipboard = phrase.clone();
        let mut body = vault_dialog_body(
            "Write this phrase down before continuing. It is shown once and then encrypted into the vault.",
        );
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        body = body.child(app_input(&self.wallet_name_input));
        if matches!(self.vault_state, VaultState::ViewUnlocked) {
            body = body.child(app_masked_input(&self.add_wallet_password_input, false));
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
                    this.child(
                        div()
                            .id("generated-wallet-recovery-phrase-copy-action")
                            .flex_none()
                            .tooltip(|window, cx| {
                                Tooltip::new("Copy recovery phrase").build(window, cx)
                            })
                            .child(clipboard_with_toast(
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
        let back_root = root;
        let mut body = vault_dialog_body(
            "Paste the recovery phrase. The phrase is validated, converted to canonical entropy, and cleared from the input.",
        );
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        body.child(app_input(&self.wallet_name_input))
            .when(
                matches!(self.vault_state, VaultState::ViewUnlocked),
                |this| this.child(app_masked_input(&self.add_wallet_password_input, false)),
            )
            .child(
                gpui_component::input::Textarea::new(&self.import_mnemonic_input)
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

    fn render_hardware_wallet_setup(
        &self,
        root: Entity<Self>,
        device_kind: HardwareDeviceKind,
    ) -> gpui::AnyElement {
        let create_root = root.clone();
        let recover_root = root.clone();
        let back_root = root;
        let device_label = hardware_device_label(device_kind);
        let create_button_id = hardware_create_button_id(device_kind);
        let recover_button_id = hardware_recover_button_id(device_kind);
        let create_active = self.hardware_wallet_creation_in_progress
            && self.hardware_wallet_creation_intent == Some(HardwareWalletSyncIntent::CreateNew);
        let recover_active = self.hardware_wallet_creation_in_progress
            && self.hardware_wallet_creation_intent
                == Some(HardwareWalletSyncIntent::RecoverExisting);
        let restore_index_set = self.hardware_wallet_restore_account_index_set;
        let mut body = vault_dialog_body(format!(
            "Choose whether this {device_label}-derived Railgun wallet is new or already has private history. The choice only changes the first sync baseline."
        ));
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        body.child(
            app_input(&self.wallet_name_input).disabled(self.hardware_wallet_creation_in_progress),
        )
        .when(
            matches!(self.vault_state, VaultState::ViewUnlocked),
            |this| {
                this.child(
                    app_masked_input(
                        &self.add_wallet_password_input,
                        self.hardware_wallet_creation_in_progress,
                    ),
                )
            },
        )
        .child(labeled_field(
            "Restore Railgun account index (optional)",
            app_input(&self.hardware_wallet_restore_account_index_input)
                .disabled(self.hardware_wallet_creation_in_progress),
        ))
        .child(
            app_muted_text(
                "Leave blank to use the next unused index. Enter an index to restore a deleted hardware-derived wallet; creating a new wallet is disabled while this is set.",
            )
            .whitespace_normal(),
        )
        .child(hardware_setup_notice(device_kind))
        .when(self.hardware_wallet_creation_in_progress, |this| {
            this.child(hardware_setup_progress(device_kind))
        })
        .child(
            app_button(
                create_button_id,
                format!("Create new {device_label}-derived wallet"),
            )
            .primary()
            .w_full()
            .loading(create_active)
            .disabled(self.hardware_wallet_creation_in_progress || restore_index_set)
            .on_click(move |_event, window, cx| {
                create_root.update(cx, |root, cx| {
                    root.store_hardware_derived_wallet(
                        device_kind,
                        HardwareWalletSyncIntent::CreateNew,
                        window,
                        cx,
                    );
                });
            }),
        )
        .child(
            app_button(
                recover_button_id,
                format!("Recover existing {device_label}-derived wallet"),
            )
            .outline()
            .w_full()
            .loading(recover_active)
            .disabled(self.hardware_wallet_creation_in_progress)
            .on_click(move |_event, window, cx| {
                recover_root.update(cx, |root, cx| {
                    root.store_hardware_derived_wallet(
                        device_kind,
                        HardwareWalletSyncIntent::RecoverExisting,
                        window,
                        cx,
                    );
                });
            }),
        )
        .child(
            app_button("back-hardware-wallet", "Back")
                .ghost()
                .w_full()
                .disabled(self.hardware_wallet_creation_in_progress)
                .on_click(move |_event, window, cx| {
                    back_root.update(cx, |root, cx| {
                        root.back_to_wallet_setup_choice(window, cx);
                    });
                }),
        )
        .into_any_element()
    }

    #[cfg(feature = "hardware")]
    pub(super) fn render_hardware_profile_unlock_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: gpui::Pixels,
    ) -> gpui::Div {
        let Some(device_kind) = self.hardware_profile_unlock.device_kind else {
            return div()
                .w(content_width)
                .child(app_muted_text("Choose a hardware wallet first."));
        };
        let device_label = hardware_device_label(device_kind);
        let unlock_root = root.clone();
        let trezor_mode_root = root.clone();
        let requires_password = self.hardware_profile_unlock_requires_password();
        let auto_waits = self.hardware_profile_unlock_auto_starts();
        let can_retry_auto_wait = auto_waits
            && !self.hardware_profile_unlock.in_progress
            && self.hardware_profile_unlock.error.is_some();
        let readiness_copy =
            "We'll detect the active wallet and show accounts you can open, create, or recover.";

        if self.hardware_profile_unlock.session.is_some() {
            return div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .children(
                    self.hardware_profile_unlock
                        .error
                        .as_ref()
                        .map(|message| hardware_profile_error(message.as_ref()).into_any_element()),
                )
                .child(self.render_hardware_profile_picker(root, content_width));
        }

        let mut content = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .child(app_muted_text(readiness_copy).whitespace_normal())
            .when(device_kind == HardwareDeviceKind::Ledger, |this| {
                this.child(
                    app_muted_text(
                        "If you use a Ledger passphrase wallet, activate it on the Ledger first. This app never asks for it.",
                    )
                    .whitespace_normal(),
                )
            })
            .children(
                self.hardware_profile_unlock
                    .error
                    .as_ref()
                    .map(|message| hardware_profile_error(message.as_ref()).into_any_element()),
            )
            .child(render_hardware_profile_stepper(
                device_kind,
                &self.hardware_profile_unlock.progress_steps,
            ))
            .children(
                self.hardware_profile_unlock
                    .trezor_pin_matrix_prompt
                    .as_ref()
                    .map(|prompt| render_trezor_pin_matrix_prompt(root, prompt).into_any_element()),
            );

        content = content
            .when(requires_password, |this| {
                this.child(
                    app_masked_input(
                        &self.hardware_profile_password_input,
                        self.hardware_profile_unlock.in_progress,
                    ),
                )
            })
            .when(device_kind == HardwareDeviceKind::Trezor, |this| {
                let mode = self.hardware_profile_unlock.trezor_passphrase_mode;
                let passphrase_mode_focus = self.trezor_passphrase_mode_focus.clone();
                let passphrase_mode_keyboard_root = trezor_mode_root.clone();
                let enter_in_app_disabled = self.hardware_profile_unlock.in_progress
                    || self
                        .hardware_profile_unlock
                        .trezor_passphrase_always_on_device
                        .unwrap_or(false);
                let passphrase_copy = if self
                    .hardware_profile_unlock
                    .trezor_passphrase_always_on_device
                    .unwrap_or(false)
                {
                    "Your Trezor always asks for the passphrase on-device, so in-app entry is unavailable."
                } else {
                    crate::root::vault::trezor_passphrase_mode_copy(mode)
                };
                this.child(
                    div()
                        .id("trezor-passphrase-mode")
                        .role(gpui::accesskit::Role::Group)
                        .aria_label("Trezor passphrase entry mode")
                        .w_full()
                        .p(px(12.0))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .bg(rgb_with_alpha(theme::SURFACE, 0.72))
                        .key_context(TREZOR_PASSPHRASE_MODE_KEY_CONTEXT)
                        .track_focus(
                            &passphrase_mode_focus
                                .tab_stop(!self.hardware_profile_unlock.in_progress),
                        )
                        .on_action(move |_: &CycleTrezorPassphraseMode, window, cx| {
                            passphrase_mode_keyboard_root.update(cx, |root, cx| {
                                root.cycle_trezor_profile_passphrase_mode(window, cx);
                            });
                        })
                        .child(app_strong_text("Passphrase"))
                        .child(app_muted_text(passphrase_copy).whitespace_normal())
                        .child(
                            ButtonGroup::new("trezor-passphrase-mode-toggle")
                                .w_full()
                                .disabled(self.hardware_profile_unlock.in_progress)
                                .child(trezor_passphrase_mode_segment_button(
                                    "trezor-passphrase-none".into(),
                                    "No passphrase",
                                    mode == TrezorPassphraseMode::NoPassphrase,
                                    self.hardware_profile_unlock.in_progress,
                                ))
                                .child(trezor_passphrase_mode_segment_button(
                                    "trezor-passphrase-on-device".into(),
                                    "Enter on Trezor",
                                    mode == TrezorPassphraseMode::EnterOnTrezor,
                                    self.hardware_profile_unlock.in_progress,
                                ))
                                .child(trezor_passphrase_mode_segment_button(
                                    "trezor-passphrase-in-app".into(),
                                    "Enter in app",
                                    mode == TrezorPassphraseMode::EnterInApp,
                                    enter_in_app_disabled,
                                ))
                                .on_click(move |selected, window, cx| {
                                    let Some(index) = selected.first() else {
                                        return;
                                    };
                                    let mode = match *index {
                                        0 => TrezorPassphraseMode::NoPassphrase,
                                        1 => TrezorPassphraseMode::EnterOnTrezor,
                                        2 => TrezorPassphraseMode::EnterInApp,
                                        _ => return,
                                    };
                                    trezor_mode_root.update(cx, |root, cx| {
                                        root.set_trezor_profile_passphrase_mode(mode, window, cx);
                                    });
                                }),
                        )
                        .when(mode == TrezorPassphraseMode::EnterInApp, |this| {
                            this.child(
                                app_masked_input(
                                    &self.trezor_app_passphrase_input,
                                    self.hardware_profile_unlock.in_progress,
                                ),
                            )
                        }),
                )
            })
            .when(
                requires_password || !auto_waits || can_retry_auto_wait,
                |this| {
                    this.child(
                        app_button(
                            "unlock-hardware-profile",
                            if can_retry_auto_wait {
                                format!("Try {device_label} again")
                            } else {
                                format!("Continue with {device_label}")
                            },
                        )
                        .primary()
                        .w_full()
                        .loading(self.hardware_profile_unlock.in_progress)
                        .disabled(self.hardware_profile_unlock.in_progress)
                        .on_click(move |_event, window, cx| {
                            unlock_root.update(cx, |root, cx| {
                                root.unlock_hardware_profile_from_dialog(window, cx);
                            });
                        }),
                    )
                },
            );

        content
    }

    #[cfg(feature = "hardware")]
    fn render_hardware_profile_picker_header(
        &self,
        root: &Entity<Self>,
        device_kind: HardwareDeviceKind,
    ) -> gpui::Div {
        let edit_label_root = root.clone();
        let profile = self.hardware_profile_unlock.profile.as_ref();
        let profile_label =
            profile.map_or("New hardware profile", |profile| profile.label.as_str());
        let device_label = hardware_device_label(device_kind);
        div().w_full().flex().flex_col().gap_2().child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .child(hardware_device_symbol(device_kind))
                .child(
                    app_strong_text(format!("Connected {device_label}: {profile_label}"))
                        .text_color(rgb(theme::SUCCESS))
                        .line_height(gpui::relative(1.18))
                        .truncate(),
                )
                .child(
                    app_button_base("hardware-profile-edit-label")
                        .ghost()
                        .small()
                        .icon(Icon::new(RailgunActionIcon::Pencil))
                        .accessibility_label("Edit label")
                        .tooltip("Edit label")
                        .disabled(self.hardware_profile_unlock.in_progress)
                        .on_click(move |_event, window, cx| {
                            edit_label_root.update(cx, |root, cx| {
                                root.begin_hardware_profile_label_edit(window, cx);
                            });
                        }),
                ),
        )
    }

    #[cfg(feature = "hardware")]
    fn render_hardware_profile_picker(
        &self,
        root: &Entity<Self>,
        content_width: gpui::Pixels,
    ) -> gpui::Div {
        let default_continue_root = root.clone();
        let default_new_root = root.clone();
        let default_recover_root = root.clone();
        let default_choice_back_root = root.clone();
        let advanced_root = root.clone();
        let add_root = root.clone();
        let recover_exact_root = root.clone();
        let recover_range_root = root.clone();
        let device_kind = self
            .hardware_profile_unlock
            .device_kind
            .unwrap_or(HardwareDeviceKind::Ledger);
        let device_label = hardware_device_label(device_kind);
        let account_count = self.hardware_profile_unlock.accounts.len();
        let supported_account_count = self
            .hardware_profile_unlock
            .accounts
            .iter()
            .filter(|row| row.supported)
            .count();
        let show_account_chooser =
            self.hardware_profile_unlock.target_wallet_id.is_none() && supported_account_count > 1;
        let show_trezor_app_passphrase = self
            .hardware_profile_unlock
            .session
            .as_ref()
            .is_some_and(|session| session.device_kind == HardwareDeviceKind::Trezor)
            && self.hardware_profile_unlock.trezor_passphrase_mode
                == TrezorPassphraseMode::EnterInApp;
        let advanced_open = self.hardware_profile_unlock.advanced_open;
        let advanced_toggle_label = if advanced_open {
            "Hide advanced"
        } else {
            "Advanced"
        };
        let awaiting_approval = self.hardware_profile_unlock.in_progress
            && hardware_profile_awaiting_approval(&self.hardware_profile_unlock.progress_steps);

        let mut content = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_hardware_profile_picker_header(root, device_kind));

        if awaiting_approval {
            let approval_prompt = self
                .hardware_profile_unlock
                .approval_prompt
                .clone()
                .or_else(|| {
                    if device_kind == HardwareDeviceKind::Ledger {
                        crate::root::vault::hardware_profile_evm_address_for_session(
                            self.hardware_profile_unlock.session.as_ref(),
                        )
                        .map(|address| {
                            HardwareProfileApprovalPrompt::EvmAddress(Arc::from(address))
                        })
                    } else {
                        None
                    }
                });
            return content
                .child(render_hardware_profile_approval_wait(
                    device_kind,
                    approval_prompt.as_ref(),
                ))
                .max_w(content_width);
        }

        content = if account_count > 0 {
            let title = if show_account_chooser {
                "Choose Railgun wallet"
            } else {
                "Railgun accounts"
            };
            content.child(
                div()
                    .w_full()
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb_with_alpha(theme::SURFACE, 0.72))
                    .child(app_strong_text(title))
                    .children(
                        self.hardware_profile_unlock
                            .accounts
                            .iter()
                            .map(|row| render_hardware_profile_account_row(root, row, false)),
                    )
                    .child(
                        app_button(
                            crate::root::vault::HARDWARE_PROFILE_ADD_SUBACCOUNT_BUTTON_ID,
                            "Create subaccount",
                        )
                        .outline()
                        .w_full()
                        .icon(IconName::Plus)
                        .disabled(self.hardware_profile_unlock.in_progress)
                        .on_click(move |_event, window, cx| {
                            add_root.update(cx, |root, cx| {
                                root.add_hardware_subaccount_from_profile_picker(window, cx);
                            });
                        }),
                    ),
            )
        } else if self.hardware_profile_unlock.picker_view
            == HardwareProfilePickerView::ChooseDefaultSyncIntent
        {
            content.child(
                div()
                    .w_full()
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb_with_alpha(theme::SURFACE, 0.72))
                    .child(app_strong_text(format!(
                        "Is this {device_label} new to Railgun?"
                    )))
                    .child(
                        app_button(
                            "hardware-profile-default-new",
                            "Set up as new",
                        )
                            .primary()
                            .w_full()
                            .loading(self.hardware_profile_unlock.in_progress)
                            .disabled(self.hardware_profile_unlock.in_progress)
                            .on_click(move |_event, window, cx| {
                                default_new_root.update(cx, |root, cx| {
                                    root.setup_default_hardware_account_from_profile_picker(
                                        HardwareWalletSyncIntent::CreateNew,
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                app_muted_text(format!(
                                    "Use this only if this {device_label}/passphrase has never held Railgun private funds."
                                ))
                                .whitespace_normal(),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(
                                        app_strong_text("Faster:")
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .flex_none(),
                                    )
                                    .child(
                                        app_muted_text(
                                            "starts syncing from the current safe block.",
                                        )
                                        .min_w(px(0.0))
                                        .whitespace_normal(),
                                    ),
                            ),
                    )
                    .child(
                        app_button(
                            "hardware-profile-default-recover",
                            "Recover existing wallet",
                        )
                            .outline()
                            .w_full()
                            .loading(self.hardware_profile_unlock.in_progress)
                            .disabled(self.hardware_profile_unlock.in_progress)
                            .on_click(move |_event, window, cx| {
                                default_recover_root.update(cx, |root, cx| {
                                    root.setup_default_hardware_account_from_profile_picker(
                                        HardwareWalletSyncIntent::RecoverExisting,
                                        window,
                                        cx,
                                    );
                                });
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                app_muted_text(format!(
                                    "Use this if this {device_label}/passphrase may have been used before, or if you are unsure."
                                ))
                                .whitespace_normal(),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(
                                        app_strong_text("Slower:")
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .flex_none(),
                                    )
                                    .child(
                                        app_muted_text(
                                            "scans prior history so old private balances can appear.",
                                        )
                                        .min_w(px(0.0))
                                        .whitespace_normal(),
                                    ),
                            ),
                    )
                    .child(
                        app_button(
                            "hardware-profile-default-choice-back",
                            "Back",
                        )
                            .ghost()
                            .w_full()
                            .disabled(self.hardware_profile_unlock.in_progress)
                            .on_click(move |_event, _window, cx| {
                                default_choice_back_root.update(cx, |root, cx| {
                                    root.show_hardware_profile_summary(cx);
                                });
                            }),
                    ),
            )
        } else {
            content.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        app_muted_text(format!(
                            "Set up the default Railgun wallet for this {device_label}."
                        ))
                        .whitespace_normal(),
                    )
                    .child(
                        app_button("hardware-profile-default-continue", "Continue")
                            .primary()
                            .w_full()
                            .disabled(self.hardware_profile_unlock.in_progress)
                            .on_click(move |_event, _window, cx| {
                                default_continue_root.update(cx, |root, cx| {
                                    root.show_hardware_profile_default_sync_choice(cx);
                                });
                            }),
                    ),
            )
        };

        content = content.child(
            app_button_base("hardware-profile-advanced-toggle")
                .ghost()
                .small()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(advanced_toggle_label)
                        .child(
                            Icon::new(if advanced_open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall(),
                        ),
                )
                .on_click(move |_event, _window, cx| {
                    advanced_root.update(cx, |root, cx| {
                        root.toggle_hardware_profile_advanced(cx);
                    });
                }),
        );

        content = content.when(advanced_open, |this| {
            this.child(
                div()
                    .w_full()
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb_with_alpha(theme::SURFACE, 0.72))
                    .when(show_trezor_app_passphrase, |this| {
                        this.child(
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(app_strong_text("Trezor app passphrase"))
                                .child(app_muted_text(
                                    "If the Trezor session expires, re-enter the app passphrase here before opening or creating accounts.",
                                ).whitespace_normal())
                                .child(
                                    app_masked_input(
                                        &self.trezor_app_passphrase_input,
                                        self.hardware_profile_unlock.in_progress,
                                    ),
                                ),
                        )
                    })
                    .child(app_strong_text("Advanced recovery"))
                    .child(app_muted_text(
                        "Recover a known account index or a bounded range. The app never scans beyond the range you enter.",
                    ).whitespace_normal())
                    .child(labeled_field(
                        "Recover exact account index",
                        super::ui_helpers::input_enter_scope(
                            !self.hardware_profile_unlock.in_progress,
                            {
                                let root = root.clone();
                                move |window, cx| {
                                    root.update(cx, |root, cx| {
                                        if !root.hardware_profile_unlock.in_progress {
                                            root.recover_hardware_exact_account_from_profile_picker(window, cx);
                                        }
                                    });
                                }
                            },
                        ).child(app_input(&self.hardware_profile_exact_index_input)
                            .disabled(self.hardware_profile_unlock.in_progress)),
                    ))
                    .child(
                        app_button(
                            crate::root::vault::HARDWARE_PROFILE_RECOVER_EXACT_BUTTON_ID,
                            "Recover exact index",
                        )
                            .outline()
                            .w_full()
                            .disabled(self.hardware_profile_unlock.in_progress)
                            .on_click(move |_event, window, cx| {
                                recover_exact_root.update(cx, |root, cx| {
                                    root.recover_hardware_exact_account_from_profile_picker(
                                        window, cx,
                                    );
                                });
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(labeled_field(
                                        "Range start",
                                        super::ui_helpers::input_enter_scope(
                            !self.hardware_profile_unlock.in_progress,
                            {
                                let root = root.clone();
                                move |window, cx| {
                                    root.update(cx, |root, cx| {
                                        if !root.hardware_profile_unlock.in_progress {
                                            root.recover_hardware_range_from_profile_picker(window, cx);
                                        }
                                    });
                                }
                            },
                        ).child(app_input(&self.hardware_profile_recovery_start_input)
                                            .disabled(self.hardware_profile_unlock.in_progress)),
                                    )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .child(labeled_field(
                                        "Count",
                                        super::ui_helpers::input_enter_scope(
                            !self.hardware_profile_unlock.in_progress,
                            {
                                let root = root.clone();
                                move |window, cx| {
                                    root.update(cx, |root, cx| {
                                        if !root.hardware_profile_unlock.in_progress {
                                            root.recover_hardware_range_from_profile_picker(window, cx);
                                        }
                                    });
                                }
                            },
                        ).child(app_input(&self.hardware_profile_recovery_count_input)
                                            .disabled(self.hardware_profile_unlock.in_progress)),
                                    )),
                            ),
                    )
                    .child(
                        app_button(
                            crate::root::vault::HARDWARE_PROFILE_RECOVER_RANGE_BUTTON_ID,
                            "Recover bounded range",
                        )
                            .outline()
                            .w_full()
                            .disabled(self.hardware_profile_unlock.in_progress)
                            .on_click(move |_event, window, cx| {
                                recover_range_root.update(cx, |root, cx| {
                                    root.recover_hardware_range_from_profile_picker(window, cx);
                                });
                            }),
                    )
                    .when(!self.hardware_profile_unlock.locked_accounts.is_empty(), |this| {
                        this.child(app_strong_text("Other hardware profiles"))
                            .children(
                                self.hardware_profile_unlock
                                    .locked_accounts
                                    .iter()
                                    .map(|row| render_hardware_profile_account_row(root, row, true)),
                            )
                    }),
            )
        });

        content.max_w(content_width)
    }

    fn render_vault_fatal(&self, message: &str) -> gpui::Div {
        let mut body = vault_dialog_body(SharedString::from(message.to_owned()));
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }
        body
    }

    fn render_vault_error(&self) -> Option<gpui::AnyElement> {
        self.vault_error.as_ref().map(|message| {
            Alert::error("wallet-vault-error", message.to_string())
                .small()
                .into_any_element()
        })
    }
}

pub(super) const fn should_show_pre_unlock_settings_action(vault_state: &VaultState) -> bool {
    matches!(
        vault_state,
        VaultState::CreateVault
            | VaultState::UnlockVault
            | VaultState::SetupWallet
            | VaultState::Error(_)
    )
}

const fn pending_software_profile_open_title(
    stage: Option<PendingSoftwareProfileOpenStage>,
) -> &'static str {
    match stage {
        None | Some(PendingSoftwareProfileOpenStage::Choosing) => "Open wallet",
        Some(PendingSoftwareProfileOpenStage::UnknownDecision) => "Passphrase wallet not found",
        Some(PendingSoftwareProfileOpenStage::CreationHandoff) => "Add passphrase wallet",
    }
}

pub(in crate::root) fn vault_dialog_body(subtitle: impl Into<SharedString>) -> gpui::Div {
    let subtitle = subtitle.into();
    div().w_full().flex().flex_col().gap_3().child(
        app_muted_text(subtitle)
            .whitespace_normal()
            .line_height(px(18.0)),
    )
}

pub(in crate::root) const fn hardware_create_button_id(
    device_kind: HardwareDeviceKind,
) -> &'static str {
    match device_kind {
        HardwareDeviceKind::Ledger => "create-ledger-derived-wallet",
        HardwareDeviceKind::Trezor => "create-trezor-derived-wallet",
    }
}

pub(in crate::root) const fn hardware_recover_button_id(
    device_kind: HardwareDeviceKind,
) -> &'static str {
    match device_kind {
        HardwareDeviceKind::Ledger => "recover-ledger-derived-wallet",
        HardwareDeviceKind::Trezor => "recover-trezor-derived-wallet",
    }
}

fn hardware_setup_notice(device_kind: HardwareDeviceKind) -> gpui::Div {
    let [connect, passphrase, sync_baseline, custody] = hardware_setup_notice_lines(device_kind);
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb_with_alpha(theme::SURFACE, 0.72))
        .child(app_strong_text("Before approving on the device"))
        .child(app_muted_text(connect))
        .child(app_muted_text(passphrase))
        .child(app_muted_text(sync_baseline))
        .child(app_muted_text(custody).text_color(rgb(theme::WARNING)))
}

fn hardware_setup_progress(device_kind: HardwareDeviceKind) -> gpui::Div {
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::INFO))
        .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.74))
        .child(
            app_strong_text(hardware_setup_progress_title(device_kind))
                .text_color(rgb(theme::INFO)),
        )
        .child(app_muted_text(hardware_setup_progress_detail(device_kind)).whitespace_normal())
}

pub(in crate::root) fn hardware_setup_progress_title(device_kind: HardwareDeviceKind) -> String {
    let device_label = hardware_device_label(device_kind);
    format!("Waiting for {device_label} approval")
}

pub(in crate::root) fn hardware_setup_progress_detail(device_kind: HardwareDeviceKind) -> String {
    let device_label = hardware_device_label(device_kind);
    match device_kind {
        HardwareDeviceKind::Ledger => format!(
            "Check your {device_label} and approve the Railgun derivation request. The device may describe this as providing a public key or shared secret."
        ),
        HardwareDeviceKind::Trezor => {
            format!("Check your {device_label} and approve the Railgun derivation request.")
        }
    }
}

pub(in crate::root) fn hardware_setup_notice_lines(device_kind: HardwareDeviceKind) -> [String; 4] {
    let device_label = hardware_device_label(device_kind);
    [
        format!(
            "Connect your {device_label}, open the Ethereum app, and approve the Railgun derivation request."
        ),
        "If you use a hardware passphrase wallet, activate the intended passphrase context on the device. Do not enter that passphrase into this app.".to_owned(),
        "Create new starts from the current safe head. Recover existing backfills from deployment and is safer if unsure.".to_owned(),
        "This is hardware-derived software custody, not true hardware signing: the desktop app signs Railgun spends with temporary in-memory keys.".to_owned(),
    ]
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_stepper(
    device_kind: HardwareDeviceKind,
    steps: &[HardwareProfileStepState],
) -> gpui::Div {
    let mut stepper = super::app_stepper_container().w_full();
    let last_index = steps.len().saturating_sub(1);
    for (index, step) in steps.iter().enumerate() {
        stepper = stepper.child(render_hardware_profile_step(
            device_kind,
            step,
            index == last_index,
        ));
    }
    stepper
}

#[cfg(feature = "hardware")]
fn hardware_profile_awaiting_approval(steps: &[HardwareProfileStepState]) -> bool {
    steps.iter().any(|step| {
        step.step == HardwareProfileStep::ApproveRailgunRequest
            && step.status == HardwareProfileStepStatus::Pending
    })
}

fn hardware_wallet_setup_button_label(
    device_kind: HardwareDeviceKind,
    label: &'static str,
) -> gpui::Div {
    setup_button_label(hardware_device_symbol(device_kind), label)
}

fn setup_button_label(icon: gpui::AnyElement, label: &'static str) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_center()
        .gap_2()
        .child(setup_icon_slot(icon))
        .child(app_button_label(label))
}

fn setup_icon_slot(icon: gpui::AnyElement) -> gpui::Div {
    div()
        .size(px(22.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(icon)
}

fn setup_component_icon(icon: IconName) -> gpui::AnyElement {
    Icon::new(icon).size(px(22.0)).into_any_element()
}

fn setup_embedded_icon(path: &'static str) -> gpui::AnyElement {
    Icon::empty().path(path).size(px(22.0)).into_any_element()
}

fn hardware_device_symbol(device_kind: HardwareDeviceKind) -> gpui::AnyElement {
    match device_kind {
        HardwareDeviceKind::Ledger => img(LEDGER_LOGO_SHORT_WHITE_ICON_PATH)
            .h(px(19.0))
            .flex_none()
            .into_any_element(),
        HardwareDeviceKind::Trezor => img(TREZOR_SYMBOL_WHITE_ICON_PATH)
            .h(px(22.0))
            .flex_none()
            .into_any_element(),
    }
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_approval_wait(
    device_kind: HardwareDeviceKind,
    approval_prompt: Option<&HardwareProfileApprovalPrompt>,
) -> gpui::Div {
    let device_label = hardware_device_label(device_kind);
    let copy = hardware_profile_approval_copy(device_kind, approval_prompt);
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb_with_alpha(theme::SURFACE, 0.72))
        .child(app_strong_text(format!("Confirm on {device_label}")))
        .child(app_muted_text(copy.intro).whitespace_normal())
        .children(copy.value.map(render_hardware_profile_approval_value))
        .child(app_muted_text(copy.warning).whitespace_normal())
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Spinner::new()
                        .icon(IconName::LoaderCircle)
                        .color(rgb(theme::SUCCESS).into())
                        .with_size(px(14.0)),
                )
                .child(
                    app_muted_text(format!("Waiting for {device_label} approval..."))
                        .text_color(rgb(theme::SUCCESS)),
                ),
        )
}

#[cfg(feature = "hardware")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct HardwareProfileApprovalCopy {
    pub(super) intro: String,
    pub(super) value: Option<String>,
    pub(super) warning: String,
}

#[cfg(feature = "hardware")]
pub(super) fn hardware_profile_approval_copy(
    device_kind: HardwareDeviceKind,
    approval_prompt: Option<&HardwareProfileApprovalPrompt>,
) -> HardwareProfileApprovalCopy {
    let device_label = hardware_device_label(device_kind);
    match approval_prompt {
        Some(HardwareProfileApprovalPrompt::EvmAddress(address)) => HardwareProfileApprovalCopy {
            intro: format!(
                "Compare this {device_label} address with the one shown on your device:"
            ),
            value: Some(address.to_string()),
            warning: "Only approve if they match. Your device is asking to provide the Railgun public secret key."
                .to_owned(),
        },
        Some(HardwareProfileApprovalPrompt::TrezorCipherKeyValue(key_label)) => {
            HardwareProfileApprovalCopy {
                intro: "Your Trezor should show ENCRYPT VALUE for:".to_owned(),
                value: Some(key_label.to_string()),
                warning: "Only approve if this value matches. Your device is asking to provide the Railgun public secret key."
                    .to_owned(),
            }
        }
        None => HardwareProfileApprovalCopy {
            intro: format!(
                "Your {device_label} is asking to provide the Railgun public secret key."
            ),
            value: None,
            warning: "Review the request on the device, then approve it to continue.".to_owned(),
        },
    }
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_approval_value(value: String) -> gpui::Div {
    div()
        .w_full()
        .p(px(10.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::SURFACE))
        .child(
            app_muted_text(value)
                .font_family(APP_MONO_FONT_FAMILY)
                .text_size(APP_TEXT_SIZE)
                .whitespace_normal(),
        )
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_step(
    device_kind: HardwareDeviceKind,
    step: &HardwareProfileStepState,
    is_last: bool,
) -> gpui::Div {
    let color = hardware_profile_step_color(step.status);
    let body = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .pb(if is_last { px(0.0) } else { px(12.0) })
        .child(
            app_strong_text(hardware_profile_step_label(device_kind, step.step))
                .text_color(rgb(color))
                .line_height(gpui::relative(1.0)),
        )
        .children(
            hardware_profile_step_detail(device_kind, step).map(|detail| {
                app_muted_text(detail)
                    .text_color(rgb(color))
                    .line_height(gpui::relative(1.0))
                    .whitespace_normal()
                    .into_any_element()
            }),
        );

    super::app_step_row(
        render_hardware_profile_step_marker(step.status, color),
        body,
        is_last,
        color,
        px(30.0),
        Some(0.34),
    )
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_step_marker(status: HardwareProfileStepStatus, color: u32) -> gpui::Div {
    div()
        .size(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(rgb(color))
        .bg(rgb(theme::SURFACE))
        .text_color(rgb(color))
        .child(match status {
            HardwareProfileStepStatus::NotStarted => div()
                .size(px(7.0))
                .rounded_full()
                .bg(rgb(color))
                .into_any_element(),
            HardwareProfileStepStatus::Pending => Spinner::new()
                .icon(IconName::LoaderCircle)
                .color(rgb(color).into())
                .with_size(px(14.0))
                .into_any_element(),
            HardwareProfileStepStatus::Done => {
                Icon::new(IconName::CircleCheck).small().into_any_element()
            }
            HardwareProfileStepStatus::Error => Icon::new(IconName::TriangleAlert)
                .small()
                .into_any_element(),
        })
}

#[cfg(feature = "hardware")]
const fn hardware_profile_step_color(status: HardwareProfileStepStatus) -> u32 {
    match status {
        HardwareProfileStepStatus::NotStarted => theme::TEXT_MUTED,
        HardwareProfileStepStatus::Pending => theme::WARNING,
        HardwareProfileStepStatus::Done => theme::SUCCESS,
        HardwareProfileStepStatus::Error => theme::DANGER,
    }
}

#[cfg(feature = "hardware")]
fn hardware_profile_step_label(
    device_kind: HardwareDeviceKind,
    step: HardwareProfileStep,
) -> String {
    let device_label = hardware_device_label(device_kind);
    match step {
        HardwareProfileStep::UnlockDevice => format!("Unlock {device_label}"),
        HardwareProfileStep::OpenEthereumApp => match device_kind {
            HardwareDeviceKind::Ledger => "Open Ethereum app".to_owned(),
            HardwareDeviceKind::Trezor => "Confirm active wallet".to_owned(),
        },
        HardwareProfileStep::ApproveRailgunRequest => "Approve Railgun request".to_owned(),
    }
}

#[cfg(feature = "hardware")]
fn hardware_profile_step_detail(
    device_kind: HardwareDeviceKind,
    step: &HardwareProfileStepState,
) -> Option<String> {
    if matches!(
        step.status,
        HardwareProfileStepStatus::NotStarted | HardwareProfileStepStatus::Done
    ) {
        return None;
    }
    if let Some(message) = step.message.as_ref() {
        return Some(message.to_string());
    }
    if step.status == HardwareProfileStepStatus::Error {
        return Some("Needs attention.".to_owned());
    }
    let device_label = hardware_device_label(device_kind);
    Some(match (step.step, device_kind) {
        (HardwareProfileStep::UnlockDevice, _) => "Plug it in and enter your PIN.".to_owned(),
        (HardwareProfileStep::OpenEthereumApp, HardwareDeviceKind::Ledger) => {
            "Open the Ethereum app on your Ledger.".to_owned()
        }
        (HardwareProfileStep::OpenEthereumApp, HardwareDeviceKind::Trezor) => {
            "Confirm the wallet on your Trezor.".to_owned()
        }
        (HardwareProfileStep::ApproveRailgunRequest, _) => {
            format!("Approve the Railgun request on your {device_label}.")
        }
    })
}

#[cfg(feature = "hardware")]
fn hardware_profile_error(message: &str) -> gpui::Div {
    div()
        .w_full()
        .child(Alert::error("wallet-hardware-profile-error", message.to_owned()).small())
}

#[cfg(feature = "hardware")]
fn render_hardware_profile_account_row(
    root: &Entity<WalletRoot>,
    row: &crate::root::vault::HardwareAccountPickerRow,
    locked: bool,
) -> gpui::AnyElement {
    let wallet_id = Arc::clone(&row.wallet_id);
    let wallet_id_for_element = Arc::clone(&row.wallet_id);
    let supported = row.supported;
    let active = row.active;
    let label = row.label.to_string();
    let action_root = root.clone();
    let row_element = div()
        .w_full()
        .p(px(12.0))
        .flex()
        .items_center()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(if active && !locked {
            theme::PRIMARY
        } else if locked {
            theme::BORDER
        } else {
            theme::BORDER_STRONG
        }))
        .bg(rgb(if locked {
            theme::SURFACE
        } else {
            theme::SURFACE_ELEVATED
        }))
        .child(app_strong_text(label).truncate())
        .when(locked, |this| {
            this.child(
                app_muted_text("Locked")
                    .text_size(px(11.0))
                    .ml_auto()
                    .flex_none(),
            )
        })
        .when(!locked && !supported, |this| {
            this.child(
                app_muted_text("Unsupported")
                    .text_size(px(11.0))
                    .ml_auto()
                    .flex_none(),
            )
        });

    if locked || !supported {
        return row_element.into_any_element();
    }

    row_element
        .id(SharedString::from(format!(
            "hardware-profile-open-{wallet_id_for_element}"
        )))
        .cursor_pointer()
        .hover(|this| {
            this.bg(rgb(theme::SURFACE_HOVER))
                .border_color(rgb(theme::PRIMARY))
        })
        .on_click(move |_event, window, cx| {
            let wallet_id = Arc::clone(&wallet_id);
            action_root.update(cx, |root, cx| {
                root.open_hardware_account_from_profile_picker(wallet_id.as_ref(), window, cx);
            });
        })
        .into_any_element()
}

#[cfg(feature = "hardware")]
pub(in crate::root) fn render_trezor_pin_matrix_prompt(
    root: &Entity<WalletRoot>,
    prompt: &crate::root::vault::TrezorPinMatrixPromptState,
) -> gpui::Div {
    let submit_root = root.clone();
    let clear_root = root.clone();
    let backspace_root = root.clone();
    let positions_len = prompt.positions.len();
    div()
        .w_full()
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb_with_alpha(theme::SURFACE, 0.72))
        .child(app_strong_text(trezor_pin_matrix_title(prompt.kind)))
        .child(
            app_muted_text(
                "Look at the randomized PIN digits on your Trezor and click the matching positions below. The app sends positions, not the digits shown on your device.",
            )
            .whitespace_normal(),
        )
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_2()
                .child(render_trezor_pin_matrix_row(root, [7, 8, 9]))
                .child(render_trezor_pin_matrix_row(root, [4, 5, 6]))
                .child(render_trezor_pin_matrix_row(root, [1, 2, 3])),
        )
        .child(app_muted_text(format!(
            "Positions entered: {}",
            "•".repeat(positions_len)
        )))
        .child(
            div()
                .w_full()
                .flex()
                .gap_2()
                .child(
                    app_button("trezor-pin-clear", "Clear")
                        .outline()
                        .flex_1()
                        .disabled(positions_len == 0)
                        .on_click(move |_event, _window, cx| {
                            clear_root.update(cx, |root, cx| {
                                root.clear_trezor_pin_matrix_positions(cx);
                            });
                        }),
                )
                .child(
                    app_button("trezor-pin-backspace", "Backspace")
                        .outline()
                        .flex_1()
                        .disabled(positions_len == 0)
                        .on_click(move |_event, _window, cx| {
                            backspace_root.update(cx, |root, cx| {
                                root.backspace_trezor_pin_matrix_position(cx);
                            });
                        }),
                )
                .child(
                    app_button("trezor-pin-submit", "Submit PIN")
                        .primary()
                        .flex_1()
                        .disabled(positions_len == 0)
                        .on_click(move |_event, _window, cx| {
                            submit_root.update(cx, |root, cx| {
                                root.submit_trezor_pin_matrix_positions(cx);
                            });
                        }),
                ),
        )
}

#[cfg(feature = "hardware")]
fn render_trezor_pin_matrix_row(root: &Entity<WalletRoot>, positions: [u8; 3]) -> gpui::Div {
    positions
        .into_iter()
        .fold(div().w_full().flex().gap_2(), |row, position| {
            row.child(render_trezor_pin_matrix_position_button(root, position))
        })
}

#[cfg(feature = "hardware")]
fn render_trezor_pin_matrix_position_button(root: &Entity<WalletRoot>, position: u8) -> Button {
    let position_root = root.clone();
    app_button_base(("trezor-pin-position", usize::from(position)))
        .outline()
        .flex_1()
        .min_w(px(0.0))
        .h(px(44.0))
        .child(app_button_label(" "))
        .on_click(move |_event, _window, cx| {
            let position = char::from(b'0' + position);
            position_root.update(cx, |root, cx| {
                root.push_trezor_pin_matrix_position(position, cx);
            });
        })
}

#[cfg(feature = "hardware")]
fn trezor_passphrase_mode_segment_button(
    id: SharedString,
    label: &'static str,
    selected: bool,
    disabled: bool,
) -> Button {
    let button = app_button_base(id)
        .tab_stop(false)
        .flex_1()
        .min_w(px(0.0))
        .selected(selected)
        .disabled(disabled)
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .child(app_button_label(label)),
        );
    if selected { button.primary() } else { button }
}
