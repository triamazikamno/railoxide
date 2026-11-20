#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    #[cfg(feature = "hardware")]
    pub(super) fn render_hardware_profile_picker(
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
                                        &self.hardware_profile.trezor_app_passphrase_input,
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
                        crate::root::ui_helpers::input_enter_scope(
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
                        ).child(app_input(&self.hardware_profile.exact_index_input)
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
                                        crate::root::ui_helpers::input_enter_scope(
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
                        ).child(app_input(&self.hardware_profile.recovery_start_input)
                                            .disabled(self.hardware_profile_unlock.in_progress)),
                                    )),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .child(labeled_field(
                                        "Count",
                                        crate::root::ui_helpers::input_enter_scope(
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
                        ).child(app_input(&self.hardware_profile.recovery_count_input)
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
}
