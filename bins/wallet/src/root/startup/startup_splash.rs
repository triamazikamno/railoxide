#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletStartupRoot {
    fn open_startup_settings_dialog(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.close_all_dialogs(cx);
        let root = cx.entity();
        let (editor, summary) = match self.vault_store.clone() {
            Some(store) => {
                let db = store.db();
                match load_wallet_settings(db.as_ref()) {
                    Ok(settings) => {
                        let runtime = self.runtime.clone();
                        let startup_root = root.downgrade();
                        let maintenance_controller = self.maintenance_controller.clone();
                        (
                            Some(cx.new(|cx| {
                                WalletSettingsEditor::new(
                                    store,
                                    &self.options.db_path,
                                    runtime,
                                    settings,
                                    maintenance_controller,
                                    Some(startup_root),
                                    None,
                                    window,
                                    cx,
                                )
                            })),
                            None,
                        )
                    }
                    Err(error) => (
                        None,
                        Some(StartupSettingsSummary::error(format!(
                            "Failed to load wallet settings: {error}"
                        ))),
                    ),
                }
            }
            None => (
                None,
                Some(StartupSettingsSummary::error(
                    self.error.as_ref().map_or_else(
                        || "Wallet database is unavailable".to_string(),
                        ToString::to_string,
                    ),
                )),
            ),
        };
        let (dialog_width, content_height, dialog_max_height) = settings_dialog_dimensions(window);
        let maintenance_controller = self.maintenance_controller.clone();
        let data_folder = data_folder_path(&self.options.db_path);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let reset_root = root.clone();
            let retry_root = root.clone();
            let open_root = root.clone();
            let content = if let Some(editor) = editor.clone() {
                div()
                    .h(content_height)
                    .min_h(px(0.0))
                    .child(editor)
                    .into_any_element()
            } else {
                let summary = summary.clone().unwrap_or_else(|| {
                    StartupSettingsSummary::error("Settings are unavailable".to_string())
                });
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(app_muted_text(
                        "Settings are stored in the selected wallet database and are readable before vault unlock.",
                    ))
                    .child(summary.render())
                    .child(data_folder_row(
                        &data_folder,
                        None,
                        true,
                        root.read(cx).data_folder_open_error.clone(),
                        move |_window, cx| {
                            open_root.update(cx, Self::open_settings_data_folder);
                        },
                    ))
                    .child(
                        div()
                            .mt(px(8.0))
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                app_button("startup-settings-reset", "Reset settings")
                                    .disabled(!maintenance_controller.read(&*cx).is_idle())
                                    .on_click(move |_event, window, cx| {
                                        window.close_all_dialogs(cx);
                                        reset_root.update(cx, |root, cx| {
                                            root.reset_settings_and_retry(window, cx);
                                        });
                                    }),
                            )
                            .child(
                                app_button("startup-settings-retry", "Retry startup")
                                    .primary()
                                    .disabled(!maintenance_controller.read(&*cx).is_idle())
                                    .on_click(move |_event, window, cx| {
                                        window.close_all_dialogs(cx);
                                        retry_root.update(cx, |root, cx| {
                                            root.retry_startup(window, cx);
                                        });
                                    }),
                            ),
                    )
                    .into_any_element()
            };
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .margin_top(px(16.0))
                .title(app_strong_text("Startup Settings"))
                .on_ok(|_, _, _| false)
                .child(content)
        });
    }

    pub(super) fn render_splash(
        &self,
        window: &Window,
        cx: &Context<'_, Self>,
    ) -> gpui::AnyElement {
        let root = cx.entity();
        let has_error = self.error.is_some();
        let accent = if has_error {
            theme::DANGER
        } else {
            theme::INFO
        };
        let percent = self.progress.percent.unwrap_or(0);
        let maintenance_idle = self.maintenance_controller.read(cx).is_idle();
        let maintenance_status = self.maintenance_controller.read(cx).status();
        let action_state = startup_settings_action_state(has_error, maintenance_idle);
        let tor_reset_available = self.tor_bootstrap_recovery_available;
        let tor_reset_error = self.tor_reset_error.clone();
        let stage = if has_error {
            "Network startup failed"
        } else {
            self.progress.stage.label()
        };
        let detail = self
            .error
            .as_ref()
            .map_or_else(|| self.progress.detail.to_string(), ToString::to_string);
        let card = div()
            .w_full()
            .p(px(24.0))
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER_STRONG))
            .bg(rgb_with_alpha(theme::SURFACE_ELEVATED, 0.86))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .size(px(34.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(rgb(theme::SURFACE))
                            .border_1()
                            .border_color(rgb(accent))
                            .when(!has_error, |this| {
                                this.child(
                                    Spinner::new()
                                        .icon(IconName::LoaderCircle)
                                        .color(rgb(accent).into())
                                        .with_size(px(18.0)),
                                )
                            })
                            .when(has_error, |this| {
                                this.child(img(icons::globe_icon_path()).size(px(17.0)))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(accent))
                                    .child(stage),
                            )
                            .child(
                                div()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(SharedString::from(detail)),
                            ),
                    ),
            )
            .when_some(maintenance_status, |this, status| {
                this.child(
                    div()
                        .mt(px(14.0))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .bg(rgb(theme::SURFACE))
                        .p(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(status.to_string())),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        UiProgress::new("wallet-startup-progress")
                            .flex_1()
                            .h(px(7.0))
                            .value(f32::from(percent)),
                    )
                    .child(
                        div()
                            .w(px(42.0))
                            .text_color(rgb(accent))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(format!("{percent}%"))),
                    ),
            )
            .when(tor_reset_available, |this| {
                let reset_root = root.clone();
                this.child(
                    div()
                        .mt(px(14.0))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .when_some(tor_reset_error, |this, error| {
                            this.child(
                                div()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(theme::DANGER))
                                    .bg(rgb(theme::SURFACE))
                                    .p(px(10.0))
                                    .text_color(rgb(theme::DANGER))
                                    .child(SharedString::from(error.to_string())),
                            )
                        })
                        .child(
                            div().flex().justify_end().child(
                                app_button("wallet-startup-reset-tor-state", "Reset Tor state")
                                    .outline()
                                    .danger()
                                    .disabled(!maintenance_idle)
                                    .tooltip(TOR_RESET_TOOLTIP)
                                    .on_click(move |_event, _window, cx| {
                                        reset_root.update(cx, |root, cx| {
                                            root.quit_and_reset_tor_state(cx);
                                        });
                                    }),
                            ),
                        ),
                )
            })
            .when(action_state.reset || action_state.retry, |this| {
                let retry_root = root.clone();
                let reset_root = root.clone();
                this.child(
                    div()
                        .mt(px(14.0))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(theme::DANGER))
                        .bg(rgb(theme::SURFACE))
                        .p(px(12.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(
                            "Wallet networking failed closed. No direct network fallback was started.",
                        ),
                )
                .child(
                    div()
                        .mt(px(14.0))
                        .flex()
                        .gap_2()
                        .justify_end()
                        .when(action_state.reset, |this| {
                            this.child(
                                app_button("wallet-startup-reset-settings", "Reset settings")
                                    .disabled(!action_state.maintenance_actions_enabled)
                                    .on_click(move |_event, window, cx| {
                                        reset_root.update(cx, |root, cx| {
                                            root.reset_settings_and_retry(window, cx);
                                        });
                                    }),
                            )
                        })
                        .when(action_state.retry, |this| {
                            this.child(
                                app_button("wallet-startup-retry", "Retry startup")
                                    .primary()
                                    .disabled(!action_state.maintenance_actions_enabled)
                                    .on_click(move |_event, window, cx| {
                                        retry_root.update(cx, |root, cx| {
                                            root.retry_startup(window, cx);
                                        });
                                    }),
                            )
                        }),
                )
            })
            .into_any_element();

        render_wallet_hero_screen(window, card, None)
            .when(action_state.settings, |this| {
                this.child(Self::render_startup_settings_gear(root))
            })
            .into_any_element()
    }

    fn render_startup_settings_gear(root: Entity<Self>) -> gpui::Div {
        div().absolute().right(px(24.0)).bottom(px(24.0)).child(
            app_button_base("wallet-startup-settings")
                .outline()
                .h(px(40.0))
                .w(px(40.0))
                .accessibility_label("Settings")
                .tooltip("Settings")
                .icon(IconName::Settings)
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.open_startup_settings_dialog(window, cx);
                    });
                }),
        )
    }

    pub(in crate::root) fn open_settings_from_shortcut(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(root) = self.wallet_root.clone() {
            root.update(cx, |root, cx| {
                root.open_settings_from_shortcut(window, cx);
            });
        } else if self.setup.is_some() {
            self.open_first_start_settings(None, window, cx);
        } else {
            self.open_startup_settings_dialog(window, cx);
        }
    }

    pub(in crate::root) fn lock_vault_from_shortcut(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(root) = self.wallet_root.clone() {
            root.update(cx, |root, cx| {
                root.lock_vault_from_shortcut(window, cx);
            });
        }
    }

    pub(in crate::root) fn toggle_network_status_from_shortcut(&self, cx: &mut Context<'_, Self>) {
        if let Some(root) = self.wallet_root.clone() {
            root.update(cx, |root, cx| {
                root.toggle_network_status_from_shortcut(cx);
            });
        }
    }
}
