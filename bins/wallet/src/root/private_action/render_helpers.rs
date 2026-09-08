use super::*;

pub(in crate::root) fn private_action_asset_title_select(
    asset_select: &Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>,
    disabled: bool,
) -> gpui::Div {
    div().w(px(170.0)).h(px(32.0)).child(
        Select::new(asset_select)
            .w_full()
            .h(px(32.0))
            .placeholder("Select asset")
            .menu_width(px(220.0))
            .disabled(disabled),
    )
}

pub(in crate::root) fn private_action_asset_select_row(
    label: &str,
    icon_path: Option<WalletIconSource>,
) -> gpui::Div {
    div().flex().items_center().gap_2().child(token_label_row(
        SharedString::from(label.to_owned()),
        icon_path,
        px(16.0),
    ))
}

pub(in crate::root) fn render_fee_token_selector(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    options: &[PublicBroadcasterFeeTokenOption],
    selected_fee_token: Address,
    generating: bool,
) -> gpui::Div {
    let selected_option = options
        .iter()
        .find(|option| option.token == selected_fee_token)
        .cloned();
    let options = options.to_vec();
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .min_w(px(0.0))
                .child(app_muted_text("Transaction fee token")),
        )
        .child(
            Popover::new(delivery_element_id(key, kind, "fee-token-selector"))
                .trigger(
                    Button::new(delivery_element_id(key, kind, "fee-token-selector-trigger"))
                        .outline()
                        .child(fee_token_selector_trigger_row(
                            selected_option.as_ref(),
                            selected_fee_token,
                        ))
                        .dropdown_caret(true)
                        .disabled(generating || options.is_empty()),
                )
                .content(move |_state, window, cx| {
                    let popover = cx.entity();
                    render_fee_token_selector_menu(
                        &root,
                        &popover,
                        key,
                        kind,
                        &options,
                        selected_fee_token,
                        window,
                    )
                }),
        )
}

pub(in crate::root) fn render_fee_token_selector_menu(
    root: &Entity<WalletRoot>,
    popover: &Entity<gpui_component::popover::PopoverState>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    options: &[PublicBroadcasterFeeTokenOption],
    selected_fee_token: Address,
    _window: &mut Window,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w(px(260.0))
        .children(options.iter().map(|option| {
            let selector_root = root.clone();
            let popover = popover.clone();
            let token = option.token;
            let selected = token == selected_fee_token;
            let disabled = option.eligible_broadcaster_count == 0;
            div()
                .id(fee_token_element_id(key, kind, token))
                .w_full()
                .p(px(8.0))
                .rounded_sm()
                .text_color(rgb(if selected {
                    theme::PRIMARY_FOREGROUND
                } else {
                    theme::TEXT
                }))
                .opacity(if disabled { 0.5 } else { 1.0 })
                .when(selected, |this| this.bg(rgb(theme::PRIMARY)))
                .when(!disabled && !selected, |this| {
                    this.cursor_pointer()
                        .hover(|this| this.bg(rgb(theme::SURFACE_HOVER)))
                })
                .when(!disabled, |this| {
                    this.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        cx.stop_propagation();
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                        selector_root.update(cx, |root, cx| match kind {
                            DeliveryFormKind::Send => root.set_send_fee_token(key, token, cx),
                            DeliveryFormKind::Unshield => {
                                root.set_unshield_fee_token(key, token, cx);
                            }
                        });
                    })
                })
                .child(fee_token_option_label_row(option, px(18.0)))
        }))
}

pub(in crate::root) fn fee_token_element_id(
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    token: Address,
) -> SharedString {
    let action = format!("fee-token-{}", token.to_checksum(None));
    delivery_element_id(key, kind, &action)
}

pub(in crate::root) fn fee_token_option_button_label(
    option: &PublicBroadcasterFeeTokenOption,
) -> String {
    format!(
        "{} · {}",
        option.label,
        broadcaster_count_label(option.eligible_broadcaster_count)
    )
}

pub(in crate::root) fn fee_token_selector_trigger_row(
    option: Option<&PublicBroadcasterFeeTokenOption>,
    selected_fee_token: Address,
) -> gpui::Div {
    option.map_or_else(
        || {
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(SharedString::from(short_address(&selected_fee_token)))
        },
        |option| fee_token_option_label_row(option, px(16.0)),
    )
}

pub(in crate::root) fn fee_token_option_label_row(
    option: &PublicBroadcasterFeeTokenOption,
    icon_size: Pixels,
) -> gpui::Div {
    token_label_row(
        SharedString::from(fee_token_option_button_label(option)),
        option.icon_path.clone(),
        icon_size,
    )
}

pub(in crate::root) fn broadcaster_count_label(count: usize) -> String {
    match count {
        0 => "no broadcasters".to_string(),
        1 => "1 broadcaster".to_string(),
        count => format!("{count} broadcasters"),
    }
}

pub(in crate::root) fn render_fee_mode_toggle(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    delivery_mode: DeliveryMode,
    mode: FeeHandlingMode,
    generating: bool,
) -> gpui::Div {
    let selector_root = root;
    let (deduct_tooltip, add_tooltip) = match (kind, delivery_mode) {
        (DeliveryFormKind::Send, _) => (
            "Use the entered amount as the token spend. Recipient receives less after the broadcaster fee.",
            "Recipient receives the entered amount. The wallet adds the broadcaster fee on top.",
        ),
        (DeliveryFormKind::Unshield, DeliveryMode::PublicBroadcaster) => (
            "Use the entered amount as the token spend. Recipient receives less after the RAILGUN fee, and after broadcaster fee if paid in this token.",
            "Recipient receives the entered amount. The wallet adds the RAILGUN fee, and broadcaster fee if paid in this token.",
        ),
        (DeliveryFormKind::Unshield, _) => (
            "Use the entered amount as the token spend. Recipient receives less after the RAILGUN fee.",
            "Recipient receives the entered amount. The wallet adds the RAILGUN fee on top.",
        ),
    };
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(div().min_w(px(0.0)).child(app_muted_text("Fees")))
        .child(
            div().flex_none().child(
                ButtonGroup::new(delivery_element_id(key, kind, "fee-mode-toggle"))
                    .outline()
                    .compact()
                    .disabled(generating)
                    .child(fee_mode_segment_button(
                        delivery_element_id(key, kind, "fee-mode-deduct"),
                        delivery_element_id(key, kind, "fee-mode-deduct-info"),
                        "Deduct",
                        deduct_tooltip,
                        mode == FeeHandlingMode::DeductFromAmount,
                        generating,
                    ))
                    .child(fee_mode_segment_button(
                        delivery_element_id(key, kind, "fee-mode-add"),
                        delivery_element_id(key, kind, "fee-mode-add-info"),
                        "Add on top",
                        add_tooltip,
                        mode == FeeHandlingMode::AddToAmount,
                        generating,
                    ))
                    .on_click(move |selected, window, cx| {
                        let Some(index) = selected.first() else {
                            return;
                        };
                        let mode = if *index == 0 {
                            FeeHandlingMode::DeductFromAmount
                        } else {
                            FeeHandlingMode::AddToAmount
                        };
                        selector_root.update(cx, |root, cx| match kind {
                            DeliveryFormKind::Send => {
                                root.set_send_fee_mode(key, mode, window, cx);
                            }
                            DeliveryFormKind::Unshield => {
                                root.set_unshield_fee_mode(key, mode, window, cx);
                            }
                        });
                    }),
            ),
        )
}

pub(in crate::root) fn fee_mode_segment_button(
    id: SharedString,
    info_id: SharedString,
    label: &'static str,
    tooltip: &'static str,
    selected: bool,
    disabled: bool,
) -> Button {
    app_segment_button(
        id,
        label,
        selected,
        disabled,
        Some(render_private_action_info_icon(info_id, label, tooltip).into_any_element()),
    )
}

const PRIVATE_ACTION_INFO_TOOLTIP_MAX_WIDTH: Pixels = px(360.0);

pub(in crate::root) fn private_action_info_tooltip_width(
    viewport_width: Pixels,
    rem_size: Pixels,
) -> Option<Pixels> {
    let tooltip_chrome = rem_size * 2.5 + px(2.0);
    let available_width = viewport_width - tooltip_chrome;
    (available_width > px(0.0)).then(|| available_width.min(PRIVATE_ACTION_INFO_TOOLTIP_MAX_WIDTH))
}

pub(in crate::root) fn render_private_action_info_icon(
    id: SharedString,
    title: &'static str,
    detail: &'static str,
) -> impl IntoElement {
    render_private_action_info_icon_with_body(id, title, move || {
        div().child(detail).into_any_element()
    })
}

pub(in crate::root) fn render_private_action_info_icon_with_body(
    id: SharedString,
    title: &'static str,
    body: impl Fn() -> gpui::AnyElement + Clone + 'static,
) -> impl IntoElement {
    render_private_action_tooltip_icon(
        id,
        IconName::Info,
        theme::TEXT_MUTED,
        theme::TEXT,
        title,
        body,
    )
}

fn render_private_action_tooltip_icon(
    id: SharedString,
    icon: IconName,
    icon_color: u32,
    title_color: u32,
    title: &'static str,
    body: impl Fn() -> gpui::AnyElement + Clone + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(18.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(rgb(icon_color))
        .child(Icon::new(icon).with_size(px(14.0)))
        .tooltip(move |window: &mut Window, cx: &mut App| {
            let Some(width) =
                private_action_info_tooltip_width(window.viewport_size().width, window.rem_size())
            else {
                return Tooltip::element(|_window, _cx| div())
                    .m(px(0.0))
                    .p(px(0.0))
                    .border_0()
                    .build(window, cx);
            };
            let body = body.clone();
            Tooltip::element(move |_window, _cx| {
                div()
                    .w(width)
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .whitespace_normal()
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .text_color(rgb(theme::TEXT))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(title_color))
                            .child(title),
                    )
                    .child(body())
            })
            .build(window, cx)
        })
}

pub(in crate::root) fn private_action_segment_button(
    id: SharedString,
    label: &'static str,
    selected: bool,
) -> Button {
    private_action_segment_button_with_accessory(id, label, selected, None)
}

pub(in crate::root) fn private_action_segment_button_with_accessory(
    id: SharedString,
    label: &'static str,
    selected: bool,
    accessory: Option<gpui::AnyElement>,
) -> Button {
    let button = app_button_base(id)
        .flex_1()
        .min_w(px(0.0))
        .selected(selected)
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .gap_1()
                .child(app_button_label(label))
                .children(accessory),
        );
    if selected { button.primary() } else { button }
}

pub(in crate::root) fn render_self_broadcast_privacy_icon(id: SharedString) -> gpui::AnyElement {
    render_private_action_tooltip_icon(
        id,
        IconName::TriangleAlert,
        theme::WARNING,
        theme::WARNING,
        "Privacy warning",
        move || {
            div()
                .child(SELF_BROADCAST_PRIVACY_WARNING)
                .into_any_element()
        },
    )
    .into_any_element()
}

pub(in crate::root) fn render_self_broadcast_gas_payer_warning_icon(
    id: SharedString,
) -> gpui::AnyElement {
    Button::new(id)
        .text()
        .xsmall()
        .compact()
        .icon(IconName::TriangleAlert)
        .text_color(rgb(theme::DANGER))
        .accessibility_label(SELF_BROADCAST_ZERO_GAS_PAYER_WARNING)
        .tooltip(SELF_BROADCAST_ZERO_GAS_PAYER_WARNING)
        .into_any_element()
}

pub(in crate::root) fn render_send_result(
    key: UnshieldAssetKey,
    result: &PreparedSendCall,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::SUCCESS))
        .child(app_strong_text("Prepared send calldata"))
        .child(app_muted_text(
            "Submit this transaction externally. The wallet has not broadcast it.",
        ))
        .child(render_unshield_copy_field(
            "To",
            result.to.to_checksum(None),
            send_element_id(key, "copy-to"),
        ))
        .child(render_unshield_copy_field(
            "Calldata",
            result.data.clone(),
            send_element_id(key, "copy-data"),
        ))
}

pub(in crate::root) fn render_unshield_result(
    key: UnshieldAssetKey,
    asset: &UnshieldAsset,
    result: &PreparedUnshieldCall,
) -> gpui::Div {
    let title = if result.native_top_up.is_some() {
        "Prepared composite unshield calldata"
    } else {
        "Prepared calldata"
    };
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::SUCCESS))
        .child(app_strong_text(title))
        .child(app_muted_text(
            "Submit this transaction externally. The wallet has not broadcast it.",
        ))
        .children(result.native_top_up.as_ref().map(|top_up| {
            let recipient_amount = native_top_up_primary_recipient_amount_for_fee_mode(
                result.token,
                top_up.wrapped_native_token,
                result.amount,
                result.fee_mode,
                top_up.native_amount,
            );
            render_unshield_output_row_with_optional_suffix(
                "Recipient receives",
                format_exact_asset_amount_for_display(recipient_amount, asset),
                Some(format_native_top_up_recipient_suffix(
                    result.chain_id,
                    top_up.native_amount,
                )),
            )
        }))
        .child(render_unshield_copy_field(
            "To",
            result.to.to_checksum(None),
            unshield_element_id(key, "copy-to"),
        ))
        .child(render_unshield_copy_field(
            "Calldata",
            result.data.clone(),
            unshield_element_id(key, "copy-data"),
        ))
}

pub(in crate::root) fn render_unshield_copy_field(
    label: &'static str,
    value: String,
    button_id: SharedString,
) -> gpui::Div {
    copyable_mono_field(label, value, button_id)
}

pub(in crate::root) fn render_unshield_native_top_up_control(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    plan: Option<&DesktopNativeTopUpPlan>,
    enabled: bool,
    generating: bool,
    registry: Option<&EffectiveTokenRegistry>,
) -> gpui::Div {
    if let Some(plan) = plan {
        let toggle_root = root.clone();
        let label = format!(
            "Also send {} for gas",
            native_token_display_label(key.chain_id)
        );
        let native_amount =
            format_native_token_amount_for_display(key.chain_id, plan.native_amount);
        let source_amount = format_token_amount_for_display(
            key.chain_id,
            plan.wrapped_native_token,
            plan.wrapped_native_amount,
            registry,
        );
        return div()
            .flex()
            .flex_col()
            .gap_2()
            .p(px(10.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(if enabled {
                theme::WARNING
            } else {
                theme::BORDER
            }))
            .bg(rgb(theme::SURFACE_ELEVATED))
            .when(!generating, |this| {
                this.cursor_pointer().on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                    cx.stop_propagation();
                    toggle_root.update(cx, |root, cx| {
                        root.set_unshield_native_top_up_enabled(key, !enabled, cx);
                    });
                })
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(app_strong_text(label))
                            .child(cost_estimate_detail_text(format!(
                                "Recipient receives {native_amount}; funded from private {source_amount}.",
                            ))),
                    )
                    .child(render_switch(
                        unshield_element_id(key, "native-top-up-toggle"),
                        enabled,
                        generating,
                        theme::WARNING,
                        move |checked, _window, cx| {
                            root.update(cx, |root, cx| {
                                root.set_unshield_native_top_up_enabled(key, checked, cx);
                            });
                        },
                    )),
            );
    }

    div()
}

fn render_unshield_output_row_with_optional_suffix(
    label: &'static str,
    value: String,
    suffix: Option<String>,
) -> gpui::Div {
    let value_row = div()
        .min_w(px(0.0))
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_1();
    let value_row = if let Some(suffix) = suffix {
        value_row
            .child(
                app_strong_text(value)
                    .text_align(gpui::TextAlign::Right)
                    .whitespace_nowrap()
                    .flex_none(),
            )
            .child(
                app_strong_text(suffix)
                    .text_align(gpui::TextAlign::Right)
                    .whitespace_nowrap()
                    .flex_none(),
            )
    } else {
        value_row.child(
            app_strong_text(value)
                .min_w(px(0.0))
                .text_align(gpui::TextAlign::Right)
                .whitespace_normal(),
        )
    };

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(app_muted_text(label).flex_none())
        .child(value_row)
}
