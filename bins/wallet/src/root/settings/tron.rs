//! Tron's Settings groups. Tron is not an EVM chain, so its groups follow the EVM chain editor on
//! the Chains page rather than living inside it.
use super::*;
use crate::root::ui_helpers::dialog_footer;
use ui::chain_editor::{EndpointFlag, confirm_direct_endpoints, endpoint_flag_button};
use ui::dialog::WalletDialogExt as _;
use wallet_ops::settings::{NativeUsdPricing, TronEndpointSetting, TronSettings};
use wallet_ops::tron::address::{encode_tron_address, parse_tron_address};

const fn flag_on(endpoint: &TronEndpointSetting, flag: EndpointFlag) -> bool {
    match flag {
        EndpointFlag::Direct => endpoint.direct,
        EndpointFlag::General => endpoint.general,
        EndpointFlag::Sensitive => endpoint.sensitive,
        EndpointFlag::Submit => endpoint.submit,
    }
}

const fn flag_mut(endpoint: &mut TronEndpointSetting, flag: EndpointFlag) -> &mut bool {
    match flag {
        EndpointFlag::Direct => &mut endpoint.direct,
        EndpointFlag::General => &mut endpoint.general,
        EndpointFlag::Sensitive => &mut endpoint.sensitive,
        EndpointFlag::Submit => &mut endpoint.submit,
    }
}

const fn native_usd_value(pricing: NativeUsdPricing) -> &'static str {
    match pricing {
        NativeUsdPricing::Default => "default",
        NativeUsdPricing::Disabled => "disabled",
        NativeUsdPricing::Oracle { .. } => "oracle",
    }
}

fn native_usd_from_value(value: &str, current: NativeUsdPricing) -> NativeUsdPricing {
    match value {
        "disabled" => NativeUsdPricing::Disabled,
        "oracle" if matches!(current, NativeUsdPricing::Oracle { .. }) => current,
        // Starts from the preset feed, so prices keep working until another oracle is entered.
        "oracle" => NativeUsdPricing::Oracle {
            contract_address: TronSettings::default()
                .native_usd_oracle()
                .unwrap_or_default(),
        },
        _ => NativeUsdPricing::Default,
    }
}

impl WalletSettingsEditor {
    pub(in crate::root) fn tron_settings_groups(&self, editor: &Entity<Self>) -> Vec<SettingGroup> {
        let endpoints = self.draft.tron.effective_endpoints();
        let customized = self.draft.tron.endpoints.is_some();
        let native_usd = Self::dropdown_field(
            editor.clone(),
            vec![
                (SharedString::from("default"), SharedString::from("Default")),
                (
                    SharedString::from("disabled"),
                    SharedString::from("Disabled"),
                ),
                (SharedString::from("oracle"), SharedString::from("Oracle")),
            ],
            |settings| SharedString::from(native_usd_value(settings.tron.native_usd)),
            |settings, value| {
                settings.tron.native_usd =
                    native_usd_from_value(value.as_ref(), settings.tron.native_usd);
            },
        );
        let mut native_usd_group = settings_group().title("Tron USD price").item(
            SettingItem::new("TRX price source", native_usd)
                .description(
                    "Default reads Chainlink's TRX/USD feed on Tron through General endpoints.",
                )
                .keywords([
                    "tron",
                    "trx",
                    "default",
                    "disabled",
                    "oracle",
                    "chainlink",
                    "oracle contract",
                    "native usd",
                    "price",
                ]),
        );
        if let NativeUsdPricing::Oracle { contract_address } = self.draft.tron.native_usd {
            let oracle = encode_tron_address(contract_address);
            let oracle_editor = editor.clone();
            native_usd_group = native_usd_group.item(
                SettingItem::new(
                    "Oracle contract",
                    SettingField::<SharedString>::render(move |_options, _window, _cx| {
                        Self::render_tron_oracle(&oracle_editor, oracle.clone())
                    }),
                )
                .description("A Chainlink-compatible TRX/USD feed on Tron, in base58.")
                .keywords([
                    "tron",
                    "trx",
                    "oracle",
                    "chainlink",
                    "price feed",
                    "contract address",
                    "native usd",
                ])
                .layout(Axis::Vertical),
            );
        }
        let endpoints_editor = editor.clone();
        let tron_group = settings_group()
            .title("Tron")
            .item(
                Self::settings_switch_item(
                    "wallet-settings-tron-enabled",
                    "Enable Tron",
                    editor.clone(),
                    None,
                    |settings| settings.tron.enabled,
                    |settings, value| settings.tron.enabled = value,
                )
                .description(
                    "Shows Tron in the network selector. Tron accounts and settings stay \
                     stored while it is off.",
                )
                .keywords([
                    "tron",
                    "trx",
                    "enable",
                    "disable",
                    "non-evm",
                    "network list",
                ]),
            )
            .item(
                SettingItem::new(
                    "Endpoints",
                    SettingField::<SharedString>::render(move |_options, _window, _cx| {
                        Self::render_tron_endpoints(
                            &endpoints_editor,
                            endpoints.clone(),
                            customized,
                        )
                    }),
                )
                .description(
                    "Tried in this order. Each is checked against Tron mainnet before use.",
                )
                .keywords([
                    "tron",
                    "trx",
                    "rpc",
                    "json-rpc",
                    "http api",
                    "node",
                    "submit",
                    "broadcast",
                    "direct",
                    "sensitive",
                ])
                .layout(Axis::Vertical),
            );
        vec![tron_group, native_usd_group]
    }

    fn render_tron_endpoints(
        editor: &Entity<Self>,
        endpoints: Vec<TronEndpointSetting>,
        customized: bool,
    ) -> gpui::Div {
        let reset_editor = editor.clone();
        let add_editor = editor.clone();
        let actions = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                app_button("wallet-settings-tron-endpoints-reset", "Reset to presets")
                    .small()
                    .disabled(!customized)
                    .on_click(move |_event, _window, cx| {
                        reset_editor.update(cx, |editor, cx| {
                            editor.draft.tron.endpoints = None;
                            editor.programmatic_draft_changed(cx);
                        });
                    }),
            )
            .child(
                app_button("wallet-settings-tron-endpoint-add", "Add endpoint")
                    .icon(IconName::Plus)
                    .small()
                    .on_click(move |_event, window, cx| {
                        add_editor.update(cx, |editor, cx| {
                            editor.open_tron_endpoint_dialog(None, window, cx);
                        });
                    }),
            );
        let count = endpoints.len();
        let mut list = div().w_full().flex().flex_col();
        if endpoints.is_empty() {
            list = list.child(app_muted_text("No endpoints configured.").py(px(8.0)));
        }
        for (index, endpoint) in endpoints.into_iter().enumerate() {
            let flags = EndpointFlag::ALL.iter().fold(
                div().flex().flex_none().items_center().gap_1(),
                |group, &flag| {
                    let flag_editor = editor.clone();
                    let on = flag_on(&endpoint, flag);
                    group.child(endpoint_flag_button(
                        format!("tron-endpoint-{index}-{}", flag.key()),
                        flag,
                        on,
                        // Broadcasting needs the HTTP API; the row below says so.
                        !on && flag == EndpointFlag::Submit && endpoint.http_api_url.is_none(),
                        move |_event, window, cx| {
                            flag_editor.update(cx, |editor, cx| {
                                editor.toggle_tron_endpoint_flag(index, flag, window, cx);
                            });
                        },
                    ))
                },
            );
            let up_editor = editor.clone();
            let down_editor = editor.clone();
            let edit_editor = editor.clone();
            let remove_editor = editor.clone();
            let no_http_api = endpoint.http_api_url.is_none();
            let http_api = endpoint.http_api_url.map_or_else(
                || "JSON-RPC only. Broadcasting needs the HTTP API.".to_owned(),
                |url| format!("HTTP API {url}"),
            );
            let row_id = SharedString::from(format!("wallet-settings-tron-endpoint-row-{index}"));
            list = list.child(
                settings_list_row(row_id, index + 1 == count)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .truncate()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(13.0))
                                    .line_height(px(18.0))
                                    .text_color(rgb(theme::TEXT))
                                    .child(SharedString::from(endpoint.json_rpc_url)),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(px(12.0))
                                    .line_height(px(16.0))
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .when(no_http_api, |this| {
                                        this.debug_selector(move || {
                                            format!("tron-endpoint-{index}-no-http-api")
                                        })
                                    })
                                    .child(SharedString::from(http_api)),
                            ),
                    )
                    .child(flags)
                    .child(
                        settings_row_actions()
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!("tron-endpoint-{index}-up")),
                                    IconName::ArrowUp,
                                    "Move up",
                                )
                                .disabled(index == 0)
                                .on_click(
                                    move |_event, _window, cx| {
                                        up_editor.update(cx, |editor, cx| {
                                            editor.move_tron_endpoint(index, true, cx);
                                        });
                                    },
                                ),
                            )
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!("tron-endpoint-{index}-down")),
                                    IconName::ArrowDown,
                                    "Move down",
                                )
                                .disabled(index + 1 == count)
                                .on_click(
                                    move |_event, _window, cx| {
                                        down_editor.update(cx, |editor, cx| {
                                            editor.move_tron_endpoint(index, false, cx);
                                        });
                                    },
                                ),
                            )
                            .child(
                                settings_icon_button(
                                    SharedString::from(format!("tron-endpoint-{index}-edit")),
                                    Icon::new(RailgunActionIcon::Pencil),
                                    "Edit",
                                )
                                .on_click(
                                    move |_event, window, cx| {
                                        edit_editor.update(cx, |editor, cx| {
                                            editor.open_tron_endpoint_dialog(
                                                Some(index),
                                                window,
                                                cx,
                                            );
                                        });
                                    },
                                ),
                            )
                            .child(
                                settings_danger_icon_button(
                                    SharedString::from(format!("tron-endpoint-{index}-remove")),
                                    Icon::new(RailgunActionIcon::Trash2),
                                    "Remove",
                                )
                                .on_click(
                                    move |_event, _window, cx| {
                                        remove_editor.update(cx, |editor, cx| {
                                            editor.remove_tron_endpoint(index, cx);
                                        });
                                    },
                                ),
                            ),
                    ),
            );
        }
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(actions)
            .child(list)
    }

    fn render_tron_oracle(editor: &Entity<Self>, oracle: String) -> gpui::Div {
        let edit_editor = editor.clone();
        div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_size(px(13.0))
                    .line_height(px(18.0))
                    .text_color(rgb(theme::TEXT))
                    .child(SharedString::from(oracle)),
            )
            .child(
                settings_icon_button(
                    "wallet-settings-tron-oracle-edit",
                    Icon::new(RailgunActionIcon::Pencil),
                    "Edit",
                )
                .on_click(move |_event, window, cx| {
                    edit_editor.update(cx, |editor, cx| {
                        editor.open_tron_oracle_dialog(window, cx);
                    });
                }),
            )
    }

    /// Refuses Submit without an HTTP API URL, and confirms Direct and direct Submit first.
    pub(in crate::root) fn toggle_tron_endpoint_flag(
        &mut self,
        index: usize,
        flag: EndpointFlag,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(endpoint) = self.draft.tron.effective_endpoints().into_iter().nth(index) else {
            return;
        };
        let on = !flag_on(&endpoint, flag);
        if on && flag == EndpointFlag::Submit && endpoint.http_api_url.is_none() {
            return;
        }
        let direct = on && flag == EndpointFlag::Direct;
        let submitting = on && flag == EndpointFlag::Submit && endpoint.direct;
        if !direct && !submitting {
            self.set_tron_endpoint_flag(index, flag, on, cx);
            return;
        }
        let editor = cx.entity();
        let ok_text = if direct {
            "Use Direct"
        } else {
            "Turn on Submit"
        };
        confirm_direct_endpoints(
            window,
            cx,
            usize::from(direct),
            submitting,
            ok_text,
            move |cx| {
                editor.update(cx, |editor, cx| {
                    editor.set_tron_endpoint_flag(index, flag, true, cx);
                });
            },
        );
    }

    fn set_tron_endpoint_flag(
        &mut self,
        index: usize,
        flag: EndpointFlag,
        on: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(endpoint) = self.tron_endpoints_mut().get_mut(index) else {
            return;
        };
        *flag_mut(endpoint, flag) = on;
        // A node announces transactions from its own address, so Direct starts without Submit.
        if flag == EndpointFlag::Direct && on {
            endpoint.submit = false;
        }
        self.commit_tron_endpoints(cx);
    }

    fn move_tron_endpoint(&mut self, index: usize, up: bool, cx: &mut Context<'_, Self>) {
        ui::chain_editor::move_row(self.tron_endpoints_mut(), index, up);
        self.commit_tron_endpoints(cx);
    }

    fn remove_tron_endpoint(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        let endpoints = self.tron_endpoints_mut();
        if index < endpoints.len() {
            endpoints.remove(index);
        }
        self.commit_tron_endpoints(cx);
    }

    fn save_tron_endpoint(
        &mut self,
        index: Option<usize>,
        json_rpc_url: String,
        http_api_url: Option<String>,
        cx: &mut Context<'_, Self>,
    ) {
        let endpoints = self.tron_endpoints_mut();
        if let Some(index) = index.filter(|index| *index < endpoints.len()) {
            let endpoint = &mut endpoints[index];
            // Direct was confirmed for the old host, not this one.
            if endpoint.json_rpc_url != json_rpc_url || endpoint.http_api_url != http_api_url {
                endpoint.direct = false;
            }
            endpoint.submit &= http_api_url.is_some();
            endpoint.json_rpc_url = json_rpc_url;
            endpoint.http_api_url = http_api_url;
        } else {
            endpoints.push(TronEndpointSetting {
                submit: http_api_url.is_some(),
                json_rpc_url,
                http_api_url,
                ..TronEndpointSetting::default()
            });
        }
        self.commit_tron_endpoints(cx);
    }

    /// The draft's endpoint list, copied from the presets on the first change.
    fn tron_endpoints_mut(&mut self) -> &mut Vec<TronEndpointSetting> {
        let presets = self.draft.tron.effective_endpoints();
        self.draft.tron.endpoints.get_or_insert(presets)
    }

    /// A list edited back to the presets is stored as the presets, so it follows their updates.
    fn commit_tron_endpoints(&mut self, cx: &mut Context<'_, Self>) {
        if self.draft.tron.endpoints == Some(TronSettings::default().effective_endpoints()) {
            self.draft.tron.endpoints = None;
        }
        self.programmatic_draft_changed(cx);
    }

    fn open_tron_endpoint_dialog(
        &self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let initial = index
            .and_then(|index| self.draft.tron.effective_endpoints().into_iter().nth(index))
            .unwrap_or_default();
        let json_rpc =
            cx.new(|cx| InputState::new(window, cx).default_value(initial.json_rpc_url.clone()));
        let http_api = cx.new(|cx| {
            InputState::new(window, cx).default_value(initial.http_api_url.unwrap_or_default())
        });
        let title = if index.is_some() {
            "Edit Tron endpoint"
        } else {
            "Add Tron endpoint"
        };
        let action_label = SharedString::from(if index.is_some() { "Save" } else { "Add" });
        let dialog_width = (window.viewport_size().width * 0.92).min(px(560.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let dialog_json_rpc = json_rpc.clone();
        window.open_wallet_dialog(cx, move |dialog, _window, _cx| {
            let save_editor = editor.clone();
            let save_json_rpc = dialog_json_rpc.clone();
            let save_http_api = http_api.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(title))
                .footer(dialog_footer(action_label.clone(), true))
                .on_ok(move |_event, _window, cx| {
                    let json_rpc_url = save_json_rpc.read(cx).value().trim().to_owned();
                    let http_api_url = non_empty_setting(&save_http_api.read(cx).value());
                    save_editor.update(cx, |editor, cx| {
                        editor.save_tron_endpoint(index, json_rpc_url, http_api_url, cx);
                    });
                    true
                })
                .child(
                    div()
                        .w(content_width)
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(app_muted_text(
                            "Submit needs the HTTP API. Without it, this endpoint serves \
                             JSON-RPC reads only.",
                        ))
                        .child(dialog_text_field("JSON-RPC URL", &dialog_json_rpc))
                        .child(dialog_text_field("HTTP API URL (optional)", &http_api)),
                )
        });
        cx.defer_in(window, move |_editor, window, cx| {
            json_rpc.read(cx).focus_handle(cx).focus(window, cx);
        });
    }

    fn open_tron_oracle_dialog(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let initial = match self.draft.tron.native_usd {
            NativeUsdPricing::Oracle { contract_address } => encode_tron_address(contract_address),
            NativeUsdPricing::Default | NativeUsdPricing::Disabled => String::new(),
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let dialog_width = (window.viewport_size().width * 0.92).min(px(560.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let editor = cx.entity();
        let dialog_input = input.clone();
        let error = cx.new(|_| None::<SharedString>);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let save_editor = editor.clone();
            let save_input = dialog_input.clone();
            let save_error = error.clone();
            let shown_error = error.read(cx).clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Tron price oracle"))
                .footer(dialog_footer("Save", true))
                .on_ok(move |_event, _window, cx| {
                    let Ok(contract_address) =
                        parse_tron_address(save_input.read(cx).value().trim())
                    else {
                        set_dialog_error(&save_error, TRON_ADDRESS_NEEDED, cx);
                        return false;
                    };
                    save_editor.update(cx, |editor, cx| {
                        editor.draft.tron.native_usd =
                            NativeUsdPricing::Oracle { contract_address };
                        editor.programmatic_draft_changed(cx);
                    });
                    true
                })
                .child(
                    render_settings_url_dialog_content(
                        &dialog_input,
                        content_width,
                        "Enter the oracle contract's base58 Tron address.",
                    )
                    .when_some(shown_error, |this, error| {
                        this.child(settings_dialog_error(error))
                    }),
                )
        });
        cx.defer_in(window, move |_editor, window, cx| {
            input.read(cx).focus_handle(cx).focus(window, cx);
        });
    }
}
