#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(super) fn render_hardware_wallet_setup(
        &self,
        root: Entity<Self>,
        device_kind: HardwareDeviceKind,
    ) -> gpui::AnyElement {
        let create_root = root.clone();
        let recover_root = root.clone();
        let password_root = root.clone();
        let back_root = root;
        let device_label = hardware_device_label(device_kind);
        let create_button_id = hardware_create_button_id(device_kind);
        let recover_button_id = hardware_recover_button_id(device_kind);
        let create_active = self.hardware_wallet_creation.in_progress
            && self.hardware_wallet_creation.intent == Some(HardwareWalletSyncIntent::CreateNew);
        let recover_active = self.hardware_wallet_creation.in_progress
            && self.hardware_wallet_creation.intent
                == Some(HardwareWalletSyncIntent::RecoverExisting);
        let restore_index_set = self.hardware_wallet_creation.restore_account_index_set;
        let mut body = vault_dialog_body(format!(
            "Choose whether this {device_label}-derived Railgun wallet is new or already has private history. The choice only changes the first sync baseline."
        ));
        if let Some(error) = self.render_vault_error() {
            body = body.child(error);
        }

        body.child(
            app_input(&self.vault_inputs.wallet_name_input).disabled(self.hardware_wallet_creation.in_progress),
        )
        .when(
            matches!(self.vault_state, VaultState::ViewUnlocked),
            |this| {
                this.child(self.render_add_wallet_password(
                    password_root,
                    self.hardware_wallet_creation.in_progress,
                ))
            },
        )
        .child(labeled_field(
            "Restore Railgun account index (optional)",
            app_input(&self.hardware_wallet_creation.restore_account_index_input)
                .disabled(self.hardware_wallet_creation.in_progress),
        ))
        .child(
            app_muted_text(
                "Leave blank to use the next unused index. Enter an index to restore a deleted hardware-derived wallet; creating a new wallet is disabled while this is set.",
            )
            .whitespace_normal(),
        )
        .child(hardware_setup_notice(device_kind))
        .when(self.hardware_wallet_creation.in_progress, |this| {
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
            .disabled(self.hardware_wallet_creation.in_progress || restore_index_set)
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
            .disabled(self.hardware_wallet_creation.in_progress)
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
                .disabled(self.hardware_wallet_creation.in_progress)
                .on_click(move |_event, window, cx| {
                    back_root.update(cx, |root, cx| {
                        root.back_to_wallet_setup_choice(window, cx);
                    });
                }),
        )
        .into_any_element()
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn render_hardware_profile_unlock_dialog_content(
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
                let device_auth_root = root.clone();
                this.child(masked_input_with_device_auth(
                    &self.hardware_profile.password_input,
                    self.hardware_profile_unlock.in_progress || self.device_auth.in_progress,
                    device_auth_buttons(self.device_auth_prompt_cached().as_ref(), "hardware-profile-unlock-touch-id", self.device_auth.in_progress, self.hardware_profile_unlock.in_progress,
                    move |method, window, cx| {
                        device_auth_root.update(cx, |root, cx| root.unlock_hardware_profile_with_device_auth(method, window, cx));
                    }),
                ))
            })
            .when(device_kind == HardwareDeviceKind::Trezor, |this| {
                let mode = self.hardware_profile_unlock.trezor_passphrase_mode;
                let passphrase_mode_focus = self.hardware_profile.trezor_passphrase_mode_focus.clone();
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
                                    &self.hardware_profile.trezor_app_passphrase_input,
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
    pub(super) fn render_hardware_profile_picker_header(
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
}
