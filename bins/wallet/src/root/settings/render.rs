use super::*;
use wallet_ops::device_auth::DeviceAuthMethod;

mod broadcasters;
mod data_sources;

impl Render for WalletSettingsEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let editor = cx.entity();
        let chain_editing = self.chain_editor.read(cx).is_editing();
        let open_page = self.open_page.take();
        let shell = div()
            .size_full()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_status_indicator(cx))
            .when_some(self.validation_error.clone(), |this, error| {
                this.child(settings_danger_banner(error.to_string()))
            })
            .when_some(self.render_status_message(cx), |this, status| {
                this.child(status)
            });
        if chain_editing {
            // An open chain owns the body and its own footer until it returns to the list.
            return shell.child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(self.chain_editor.clone()),
            );
        }
        shell
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(
                        ComponentSettings::new("wallet-settings-editor")
                            .when_some(open_page, |settings, page_ix| {
                                settings.default_selected_index(SelectIndex {
                                    page_ix,
                                    group_ix: None,
                                })
                            })
                            .sidebar_width(px(190.0))
                            .with_group_variant(GroupBoxVariant::Normal)
                            .page(Self::general_page(&editor))
                            .page(self.security_page(&editor, cx))
                            .page(self.network_page(&editor))
                            .page(self.chains_page(&editor))
                            .page(self.tokens_page(&editor))
                            .page(self.broadcasters_page(&editor))
                            .page(self.data_sources_page(&editor))
                            .page(Self::storage_page(&editor)),
                    ),
            )
            .child(self.render_footer(&editor, cx))
    }
}

impl WalletSettingsEditor {
    fn general_page(editor: &Entity<Self>) -> SettingPage {
        SettingPage::new("General").group(
            settings_group()
                .item(
                    Self::settings_switch_item(
                        "wallet-settings-unwrap-by-default",
                        "Unwrap by default",
                        editor.clone(),
                        None,
                        |settings| settings.privacy.unwrap_unshields_by_default,
                        |settings, value| {
                            settings.privacy.unwrap_unshields_by_default = value;
                        },
                    )
                    .description(
                        "Preselects native token output when unshielding wrapped native tokens. You can change the output in each unshield form.",
                    )
                    .keywords(["unshield", "wrapped", "native token", "transaction defaults"]),
                )
                .item(
                    Self::settings_switch_item(
                        "wallet-settings-mimic-railway-by-default",
                        "Mimic Railway by default",
                        editor.clone(),
                        None,
                        |settings| settings.privacy.mimic_railway_shields_by_default,
                        |settings, value| {
                            settings.privacy.mimic_railway_shields_by_default = value;
                        },
                    )
                    .description(
                        "Preselects Mimic Railway for new shields. ERC-20 shields may grant an unlimited token allowance.",
                    )
                    .keywords(["shield", "allowance", "approval", "transaction defaults"]),
                ),
        )
    }

    fn security_page(&self, editor: &Entity<Self>, cx: &App) -> SettingPage {
        let auto_lock_timeout = Self::dropdown_field(
            editor.clone(),
            auto_lock_timeout_options(),
            |settings| auto_lock_timeout_value(settings.runtime.auto_lock_timeout_secs),
            |settings, value| {
                settings.runtime.auto_lock_timeout_secs =
                    auto_lock_timeout_from_value(value.as_ref());
            },
        );
        let mut security_group = settings_group().item(
            SettingItem::new("Auto-lock vault", auto_lock_timeout)
                .description("Lock the vault after this long without wallet activity.")
                .keywords([
                    "timeout",
                    "idle",
                    "lock",
                    "autolock",
                    "inactivity",
                    "disabled",
                    "minutes",
                    "security",
                ]),
        );
        for method in DeviceAuthMethod::ALL {
            if let Some(setting) = self.device_auth_setting(method, cx) {
                security_group = security_group.item(setting);
            }
        }
        SettingPage::new("Security").group(security_group)
    }

    fn network_page(&self, editor: &Entity<Self>) -> SettingPage {
        let network_mode = Self::dropdown_field(
            editor.clone(),
            vec![
                (
                    SharedString::from("tor"),
                    SharedString::from("Built-in Tor"),
                ),
                (SharedString::from("proxy"), SharedString::from("Proxy")),
                (SharedString::from("direct"), SharedString::from("Direct")),
            ],
            |settings| SharedString::from(network_mode_value(settings.network.mode)),
            |settings, value| {
                settings.network.mode = network_mode_from_value(value.as_ref());
                if !should_show_proxy_url_setting(settings.network.mode) {
                    settings.network.proxy_url = None;
                }
            },
        );
        let proxy_url = Self::shared_string_field(
            "network-proxy-url",
            editor.clone(),
            |settings| settings.network.proxy_url.clone().unwrap_or_default(),
            |settings, value| {
                settings.network.proxy_url = non_empty_setting(&value);
            },
        );
        let walletconnect_project_id = Self::shared_string_field(
            "walletconnect-project-id-override",
            editor.clone(),
            |settings| settings.walletconnect.effective_project_id().to_owned(),
            |settings, value| {
                settings.walletconnect.project_id_override = non_empty_setting(&value)
                    .filter(|project_id| project_id.as_str() != WALLETCONNECT_DEFAULT_PROJECT_ID);
            },
        );
        let mut network_group =
            settings_group().item(SettingItem::new("Network mode", network_mode).keywords([
                "tor",
                "built-in tor",
                "proxy",
                "direct",
                "proxy url",
                "socks",
                "privacy",
                "anonymity",
                "connection",
            ]));
        if should_show_proxy_url_setting(self.draft.network.mode) {
            network_group = network_group.item(
                SettingItem::new("Proxy URL", proxy_url)
                    .keywords(["socks", "socks5", "network mode", "privacy"])
                    .layout(Axis::Vertical),
            );
        }
        if should_show_proxy_waku_disclaimer(self.draft.network.mode) {
            let dns_warning = proxy_url_resolves_dns_locally(&self.draft);
            network_group = network_group.footer(move |_window, _cx| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(settings_warning_banner(
                        "wallet-settings-proxy-waku-warning",
                        PROXY_WAKU_DISCLAIMER,
                    ))
                    .when(dns_warning, |footer| {
                        footer.child(settings_warning_banner(
                            "wallet-settings-proxy-dns-warning",
                            PROXY_SOCKS5_DNS_WARNING,
                        ))
                    })
            });
        }
        let walletconnect_group = settings_group().title("WalletConnect").item(
            SettingItem::new("WalletConnect project ID", walletconnect_project_id)
                .keywords(["wallet connect", "reown", "dapp", "pairing"])
                .layout(Axis::Vertical),
        );
        SettingPage::new("Network")
            .group(network_group)
            .group(walletconnect_group)
    }

    fn chains_page(&self, editor: &Entity<Self>) -> SettingPage {
        let chain_editor = self.chain_editor.clone();
        SettingPage::new("Chains")
            .group(
                settings_group().item(
                    SettingItem::render(move |_, _, _| div().w_full().child(chain_editor.clone()))
                        .keywords(chains_page_keywords(&self.draft)),
                ),
            )
            .groups(self.tron_settings_groups(editor))
    }

    fn tokens_page(&self, editor: &Entity<Self>) -> SettingPage {
        let mut token_page = SettingPage::new("Tokens");
        let token_entries = display_token_entries(&self.draft);
        let token_editor = editor.clone();
        token_page = token_page.group(
            settings_group().item(
                SettingItem::new(
                    "Tokens",
                    SettingField::<SharedString>::render(move |_options, _window, _cx| {
                        Self::render_token_list(&token_editor, token_entries.clone())
                    }),
                )
                .description("Known token metadata, built-in token overrides, and custom tokens.")
                .keywords([
                    "erc-20",
                    "erc20",
                    "custom token",
                    "decimals",
                    "symbol",
                    "contract address",
                ])
                .layout(Axis::Vertical),
            ),
        );

        let price_anchor_entries = display_price_anchor_entries(&self.draft);
        let price_anchor_editor = editor.clone();
        token_page = token_page.group(
            settings_group().item(
                SettingItem::new(
                    "Price oracles",
                    SettingField::<SharedString>::render(move |_options, _window, _cx| {
                        Self::render_price_anchor_list(
                            &price_anchor_editor,
                            price_anchor_entries.clone(),
                        )
                    }),
                )
                .description("Token price anchors used to evaluate transaction fees.")
                .keywords(["price feed", "usd", "chainlink", "fees", "tokens"])
                .layout(Axis::Vertical),
            ),
        );
        token_page
    }

    fn storage_page(editor: &Entity<Self>) -> SettingPage {
        let poi_cache_reset_editor = editor.clone();
        let poi_data_reset_editor = editor.clone();
        let merkle_forest_cache_reset_editor = editor.clone();
        let prover_cache_editor = editor.clone();
        let data_folder_editor = editor.clone();
        SettingPage::new("Storage")
            .group(
                settings_group().title("Data folder").item(
                    SettingItem::new(
                        "Location",
                        SettingField::<SharedString>::render(move |_options, window, cx| {
                            Self::render_data_folder_action(&data_folder_editor, window, cx)
                        }),
                    )
                    .keywords([
                        "data folder",
                        "database",
                        "db",
                        "path",
                        "disk space",
                        "size",
                        "file manager",
                    ])
                    .layout(Axis::Vertical),
                ),
            )
            .group(
                settings_group().title("Prover cache").item(
                    SettingItem::new(
                        "Build prover cache",
                        SettingField::<SharedString>::render(move |_options, _window, cx| {
                            Self::render_build_prover_cache_action(&prover_cache_editor, cx)
                        }),
                    )
                    .description("Prepares prover artifacts locally so transaction proof generation can start faster.")
                    .keywords(["proving", "circuits", "zkey", "download", "speed"])
                    .layout(Axis::Vertical),
                ),
            )
            .group(
                settings_group()
                    .title("Local caches")
                    .item(
                        SettingItem::new(
                            "Reset public sync caches",
                            SettingField::<SharedString>::render(move |_options, _window, cx| {
                                Self::render_local_poi_cache_reset_action(
                                    &poi_cache_reset_editor,
                                    cx,
                                )
                            }),
                        )
                        .description("Clears downloaded public sync data while preserving verified PPOI data used for wallet checks.")
                        .keywords(["clear", "txid", "artifact chunks", "resync", "local caches"])
                        .layout(Axis::Vertical),
                    )
                    .item(
                        SettingItem::new(
                            "Reset PPOI data",
                            SettingField::<SharedString>::render(move |_options, _window, cx| {
                                Self::render_poi_data_reset_action(&poi_data_reset_editor, cx)
                            }),
                        )
                        .description("Deletes verified PPOI data and downloaded PPOI chunks for all chains, then rebuilds from the current source.")
                        .keywords(["poi", "proof of innocence", "clear", "rebuild", "local caches"])
                        .layout(Axis::Vertical),
                    )
                    .item(
                        SettingItem::new(
                            "Reset local Merkle forest cache",
                            SettingField::<SharedString>::render(move |_options, _window, cx| {
                                Self::render_local_merkle_forest_cache_reset_action(
                                    &merkle_forest_cache_reset_editor,
                                    cx,
                                )
                            }),
                        )
                        .description("Clears local Merkle forest snapshots for all chains, then resyncs tree data before private actions are available again.")
                        .keywords(["merkle tree", "snapshots", "clear", "resync", "local caches"])
                        .layout(Axis::Vertical),
                    ),
            )
    }

    fn render_footer(&self, editor: &Entity<Self>, cx: &App) -> gpui::Div {
        let root_replacement_allowed = self.root_replacement_is_allowed(cx);
        // Before network startup nothing needs restarting, so Save takes every valid change.
        let save_enabled = if self.pre_start {
            self.validation_error.is_none() && self.draft != self.saved
        } else {
            settings_save_action_enabled(&self.saved, &self.draft, self.validation_error.is_some())
        };
        let save_editor = editor.clone();
        let discard_editor = editor.clone();
        let reset_editor = editor.clone();
        let apply_editor = editor.clone();
        div()
            .flex_none()
            .flex()
            .flex_wrap()
            .justify_end()
            .gap_2()
            .child(
                app_button("wallet-settings-discard", "Discard")
                    .disabled(!self.is_dirty())
                    .on_click(move |_event, _window, cx| {
                        discard_editor.update(cx, |editor, cx| {
                            editor.discard_changes(cx);
                        });
                    }),
            )
            .child(
                app_button("wallet-settings-reset", "Reset to defaults").on_click(
                    move |_event, _window, cx| {
                        reset_editor.update(cx, |editor, cx| {
                            editor.reset_defaults(cx);
                        });
                    },
                ),
            )
            .child(
                app_button("wallet-settings-save", "Save")
                    .when(self.pre_start, ButtonVariants::primary)
                    .disabled(!self.maintenance_controller.read(cx).is_idle() || !save_enabled)
                    .on_click(move |_event, window, cx| {
                        save_editor.update(cx, |editor, cx| {
                            // Setup shows the saved values once Settings closes.
                            if editor.save_draft(cx) && editor.pre_start {
                                window.close_all_dialogs(cx);
                            }
                        });
                    }),
            )
            .when(!self.pre_start, |footer| {
                footer.child(
                    app_button("wallet-settings-apply-restart", "Apply")
                        .primary()
                        .disabled(
                            !self.maintenance_controller.read(cx).is_idle()
                                || !settings_restart_action_enabled(
                                    &self.saved,
                                    &self.draft,
                                    self.validation_error.is_some(),
                                    root_replacement_allowed,
                                ),
                        )
                        .on_click(move |_event, window, cx| {
                            apply_editor.update(cx, |editor, cx| {
                                editor.apply_and_restart(window, cx);
                            });
                        }),
                )
            })
    }
}

impl WalletSettingsEditor {
    fn device_auth_setting(&self, method: DeviceAuthMethod, cx: &App) -> Option<SettingItem> {
        let root = self.active_root.as_ref()?.upgrade()?;
        let checked = root.read(cx).device_auth_setting_state(method)?;
        let available = root.read(cx).device_auth_available(method);
        let description = if !available {
            match method {
                DeviceAuthMethod::TouchId => "Touch ID is unavailable right now. Check Touch ID in macOS System Settings.",
                DeviceAuthMethod::AppleWatch => "Apple Watch is unavailable right now. Make sure your watch is unlocked, on your wrist, and nearby.",
            }.to_owned()
        } else if root.read(cx).device_auth_status(method)
            == wallet_ops::vault::DeviceAuthStatus::NeedsReenrollment
        {
            format!(
                "Enter your vault password when prompted to restore {}. Changes apply immediately.",
                method.label()
            )
        } else {
            match method {
                DeviceAuthMethod::TouchId => "Use Touch ID instead of your vault password on this Mac. Changes apply immediately.",
                DeviceAuthMethod::AppleWatch => "Double-press your watch's side button for each approval. Never unlocks automatically. Changes apply immediately.",
            }.to_owned()
        };
        Some(
            SettingItem::new(
                format!("Use {}", method.label()),
                SettingField::<SharedString>::render(move |options, _window, _cx| {
                    let root = root.clone();
                    Switch::new(match method {
                        DeviceAuthMethod::TouchId => "wallet-settings-touch-id",
                        DeviceAuthMethod::AppleWatch => "wallet-settings-apple-watch",
                    })
                    .accessibility_label(format!("Use {}", method.label()))
                    .checked(checked)
                    // Revocation must remain available even while the device is not.
                    .disabled(!checked && !available)
                    .with_size(options.size())
                    .on_change(move |enabled, window, cx| {
                        root.update(cx, |root, cx| {
                            if *enabled {
                                root.open_enable_device_auth_dialog(method, window, cx);
                            } else {
                                root.disable_device_auth(method, window, cx);
                            }
                        });
                    })
                }),
            )
            .description(description)
            .keywords([
                "touch id",
                "apple watch",
                "biometric",
                "fingerprint",
                "device unlock",
                "password",
                "security",
            ]),
        )
    }
}
