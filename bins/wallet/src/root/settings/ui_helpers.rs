use super::*;

pub(in crate::root) const SETTINGS_GROUP_CONTENT_INDENT: Pixels = px(16.0);
pub(in crate::root) const SETTINGS_GROUP_HEADER_OFFSET: Pixels = px(-16.0);

pub(in crate::root) fn settings_group() -> SettingGroup {
    SettingGroup::new().pl(SETTINGS_GROUP_CONTENT_INDENT)
}

pub(in crate::root) fn settings_section_header(title: impl Into<String>) -> SettingItem {
    let title = title.into();
    SettingItem::render(move |_options, _window, _cx| {
        settings_section_header_element(&title, None, None)
    })
}

pub(in crate::root) fn settings_chain_section_header(
    chain_id: u64,
    title: impl Into<String>,
) -> SettingItem {
    let title = title.into();
    SettingItem::render(move |_options, _window, _cx| {
        settings_section_header_element(&title, None, Some(chain_id))
    })
}

pub(in crate::root) fn settings_section_header_element(
    title: &str,
    description: Option<&str>,
    chain_id: Option<u64>,
) -> gpui::Div {
    let mut title_row = div().flex().items_center().gap_2();
    if let Some(path) = chain_id.and_then(chain_icon_asset_path) {
        title_row = title_row.child(img(path).size(px(16.0)).flex_none());
    }
    title_row = title_row.child(
        div()
            .font_family(APP_MONO_FONT_FAMILY)
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(12.0))
            .line_height(px(16.0))
            .text_color(rgb(theme::TEXT_MUTED))
            .child(SharedString::from(title.to_ascii_uppercase())),
    );

    div()
        .w_full()
        .ml(SETTINGS_GROUP_HEADER_OFFSET)
        .flex()
        .flex_col()
        .gap_1()
        .child(title_row)
        .when_some(description, |this, description| {
            this.child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(16.0))
                    .text_color(rgb(theme::TEXT_SUBTLE))
                    .child(SharedString::from(description.to_string())),
            )
        })
}

pub(in crate::root) fn settings_danger_banner(message: impl Into<SharedString>) -> gpui::Div {
    div()
        .w_full()
        .child(Alert::error("wallet-settings-danger-banner", message.into().to_string()).small())
}

pub(in crate::root) fn settings_info_banner(message: impl Into<SharedString>) -> gpui::Div {
    div()
        .w_full()
        .child(Alert::info("wallet-settings-info-banner", message.into().to_string()).small())
}

pub(in crate::root) fn settings_warning_banner(message: impl Into<SharedString>) -> gpui::Div {
    div()
        .w_full()
        .child(Alert::warning("wallet-settings-warning-banner", message.into().to_string()).small())
}

pub(in crate::root) fn settings_text_input(input: &Entity<InputState>) -> Input {
    Input::new(input)
        .w_full()
        .rounded_md()
        .bg(rgb(theme::SETTINGS_INPUT_SURFACE))
        .border_color(rgb_with_alpha(theme::BORDER_SUBTLE, 0.55))
        .px(px(12.0))
        .py(px(9.0))
        .font_family(APP_MONO_FONT_FAMILY)
        .text_size(px(13.0))
        .line_height(px(18.0))
        .text_color(rgb(theme::TEXT))
}

pub(in crate::root) fn render_token_dialog_content(
    inputs: &TokenDialogInputs,
    content_width: Pixels,
    readonly_identity: bool,
) -> gpui::Div {
    div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(settings_dialog_field(
            "Chain ID",
            &inputs.chain_id,
            readonly_identity,
        ))
        .child(settings_dialog_field(
            "Token address",
            &inputs.token_address,
            readonly_identity,
        ))
        .child(settings_dialog_field("Symbol", &inputs.symbol, false))
        .child(settings_dialog_field("Decimals", &inputs.decimals, false))
        .child(settings_dialog_field("Icon path", &inputs.icon_path, false))
}

pub(in crate::root) fn render_price_anchor_dialog_content(
    inputs: &PriceAnchorDialogInputs,
    content_width: Pixels,
    cx: &App,
) -> gpui::Div {
    let anchor_type = inputs.selected_anchor_type.read(cx).value().to_string();
    let mut content = div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(app_muted_text(
            "Configure the price anchor fields before adding the override.",
        ))
        .child(settings_dialog_select_field(
            "Token chain",
            &inputs.chain_id,
            content_width,
        ))
        .child(settings_dialog_field(
            "Token address",
            &inputs.token_address,
            false,
        ))
        .child(settings_dialog_select_field(
            "Anchor type",
            &inputs.anchor_type,
            content_width,
        ));

    match anchor_type.as_str() {
        "oracle" => {
            content = content
                .child(settings_dialog_select_field(
                    "Oracle chain",
                    &inputs.oracle_chain_id,
                    content_width,
                ))
                .child(settings_dialog_field(
                    "Oracle address",
                    &inputs.oracle_address,
                    false,
                ))
                .child(settings_dialog_field(
                    "Token decimals",
                    &inputs.oracle_token_decimals,
                    false,
                ))
                .child(settings_dialog_field(
                    "Oracle decimals",
                    &inputs.oracle_decimals,
                    false,
                ))
                .child(settings_dialog_select_field(
                    "Inverse oracle",
                    &inputs.oracle_is_inversed,
                    content_width,
                ));
        }
        "uniswap-v3-twap" => {
            content = content
                .child(settings_dialog_field(
                    "Pool address",
                    &inputs.twap_pool_address,
                    false,
                ))
                .child(settings_dialog_field(
                    "Base-token address",
                    &inputs.twap_base_token_address,
                    false,
                ))
                .child(settings_dialog_field(
                    "Quote-token address",
                    &inputs.twap_quote_token_address,
                    false,
                ))
                .child(settings_dialog_field(
                    "Base-token decimals",
                    &inputs.twap_base_token_decimals,
                    false,
                ))
                .child(settings_dialog_field(
                    "Window seconds",
                    &inputs.twap_window_seconds,
                    false,
                ));
        }
        "product" => {
            content = content.child(settings_dialog_field(
                "Scale decimals",
                &inputs.product_scale_decimals,
                false,
            ));
            for (index, component) in inputs.product_components.iter().enumerate() {
                content = content.child(render_price_anchor_product_component_dialog_content(
                    index,
                    component,
                    content_width,
                    cx,
                ));
            }
        }
        _ => {
            content = content.child(settings_dialog_field(
                "Fixed rate",
                &inputs.fixed_rate,
                false,
            ));
        }
    }

    content
}

pub(in crate::root) fn render_price_anchor_product_component_dialog_content(
    index: usize,
    component: &ProductAnchorComponentDialogInputs,
    content_width: Pixels,
    cx: &App,
) -> gpui::Div {
    let anchor_type = component.selected_anchor_type.read(cx).value().to_string();
    let mut content = div()
        .w_full()
        .flex()
        .flex_col()
        .gap_3()
        .pt(px(4.0))
        .child(settings_dialog_subsection_label(format!(
            "Component {}",
            index + 1
        )))
        .child(settings_dialog_select_field(
            "Component type",
            &component.anchor_type,
            content_width,
        ));

    match anchor_type.as_str() {
        "oracle" => {
            content = content
                .child(settings_dialog_select_field(
                    "Oracle chain",
                    &component.oracle_chain_id,
                    content_width,
                ))
                .child(settings_dialog_field(
                    "Oracle address",
                    &component.oracle_address,
                    false,
                ))
                .child(settings_dialog_field(
                    "Token decimals",
                    &component.oracle_token_decimals,
                    false,
                ))
                .child(settings_dialog_field(
                    "Oracle decimals",
                    &component.oracle_decimals,
                    false,
                ))
                .child(settings_dialog_select_field(
                    "Inverse oracle",
                    &component.oracle_is_inversed,
                    content_width,
                ));
        }
        "uniswap-v3-twap" => {
            content = content
                .child(settings_dialog_field(
                    "Pool address",
                    &component.twap_pool_address,
                    false,
                ))
                .child(settings_dialog_field(
                    "Base-token address",
                    &component.twap_base_token_address,
                    false,
                ))
                .child(settings_dialog_field(
                    "Quote-token address",
                    &component.twap_quote_token_address,
                    false,
                ))
                .child(settings_dialog_field(
                    "Base-token decimals",
                    &component.twap_base_token_decimals,
                    false,
                ))
                .child(settings_dialog_field(
                    "Window seconds",
                    &component.twap_window_seconds,
                    false,
                ));
        }
        _ => {
            content = content.child(settings_dialog_field(
                "Fixed rate",
                &component.fixed_rate,
                false,
            ));
        }
    }

    content
}

pub(in crate::root) fn settings_dialog_field(
    label: impl Into<SharedString>,
    input: &Entity<InputState>,
    readonly: bool,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(Label::new(label).text_sm())
        .child(settings_text_input(input).disabled(readonly))
}

pub(in crate::root) fn settings_dialog_subsection_label(
    label: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .font_family(APP_MONO_FONT_FAMILY)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(12.0))
        .line_height(px(16.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .child(label.into())
}

pub(in crate::root) fn settings_dialog_select_field<D>(
    label: impl Into<SharedString>,
    select: &Entity<SelectState<D>>,
    menu_width: Pixels,
) -> gpui::Div
where
    D: SelectDelegate + 'static,
{
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(Label::new(label).text_sm())
        .child(
            div().w_full().h(px(36.0)).child(
                Select::new(select)
                    .small()
                    .w_full()
                    .h(px(36.0))
                    .menu_width(menu_width),
            ),
        )
}

pub(in crate::root) fn render_token_entry_summary(entry: &DisplayTokenEntry) -> gpui::Div {
    let badge = if entry.built_in { "Built-in" } else { "Custom" };
    div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .font_family(APP_MONO_FONT_FAMILY)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(13.0))
                        .line_height(px(18.0))
                        .text_color(rgb(theme::TEXT))
                        .child(SharedString::from(entry.symbol.clone())),
                )
                .child(
                    div()
                        .rounded_sm()
                        .bg(rgb_with_alpha(theme::SURFACE_HOVER_SUBTLE, 0.75))
                        .px(px(6.0))
                        .py(px(2.0))
                        .text_size(px(11.0))
                        .line_height(px(14.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(badge),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .line_height(px(16.0))
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(format!("{} decimals", entry.decimals)),
                ),
        )
        .child(
            div()
                .truncate()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_size(px(12.0))
                .line_height(px(16.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(entry.token_address.clone())),
        )
}

pub(in crate::root) fn render_price_anchor_entry_summary(
    entry: &DisplayPriceAnchorEntry,
) -> gpui::Div {
    let source = if entry.built_in_default {
        "Built-in default"
    } else {
        "Override"
    };
    let primary_label = price_anchor_token_primary_label(entry);
    let token_address = entry.key.token_address.clone();
    let mut body = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .font_family(APP_MONO_FONT_FAMILY)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(13.0))
                        .line_height(px(18.0))
                        .text_color(rgb(theme::TEXT))
                        .child(SharedString::from(primary_label)),
                )
                .child(settings_badge(price_anchor_type_display(
                    &entry.price_anchor,
                )))
                .child(settings_badge(source)),
        )
        .child(
            div()
                .truncate()
                .text_size(px(12.0))
                .line_height(px(16.0))
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(price_anchor_summary(
                    &entry.price_anchor,
                    entry.key.chain_id,
                ))),
        );
    if entry.token_symbol.is_some() {
        body = body.child(
            div()
                .truncate()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_size(px(12.0))
                .line_height(px(16.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(token_address)),
        );
    }
    body
}

pub(in crate::root) fn settings_badge(label: impl Into<SharedString>) -> gpui::Div {
    div()
        .rounded_sm()
        .bg(rgb_with_alpha(theme::SURFACE_HOVER_SUBTLE, 0.75))
        .px(px(6.0))
        .py(px(2.0))
        .text_size(px(11.0))
        .line_height(px(14.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .child(label.into())
}

pub(in crate::root) fn price_anchor_token_primary_label(entry: &DisplayPriceAnchorEntry) -> String {
    entry
        .token_symbol
        .clone()
        .unwrap_or_else(|| short_token_address(&entry.key.token_address))
}

pub(in crate::root) fn short_token_address(token_address: &str) -> String {
    token_address.parse::<Address>().map_or_else(
        |_| token_address.to_string(),
        |address| short_address(&address),
    )
}

pub(in crate::root) const fn price_anchor_type_display(
    anchor: &PriceAnchorSettings,
) -> &'static str {
    match anchor {
        PriceAnchorSettings::Fixed { .. } => "Fixed",
        PriceAnchorSettings::Oracle { .. } => "Oracle",
        PriceAnchorSettings::UniswapV3Twap { .. } => "Uniswap V3 TWAP",
        PriceAnchorSettings::Product { .. } => "Product",
    }
}

pub(in crate::root) fn price_anchor_summary(
    anchor: &PriceAnchorSettings,
    owner_chain_id: u64,
) -> String {
    match anchor {
        PriceAnchorSettings::Fixed { rate } => format!("Fixed rate {rate}"),
        PriceAnchorSettings::Oracle {
            chain_id,
            oracle_address,
            token_decimals,
            oracle_decimals,
            is_inversed,
        } => {
            let chain =
                chain_name(*chain_id).map_or_else(|| chain_id.to_string(), ToString::to_string);
            let inverse = if *is_inversed { ", inverse" } else { "" };
            format!(
                "Oracle {} on {chain}, {token_decimals}/{oracle_decimals} decimals{inverse}",
                short_token_address(oracle_address)
            )
        }
        PriceAnchorSettings::Product {
            components,
            scale_decimals,
        } => format!(
            "Product of {} components, scale {scale_decimals} decimals",
            components.len()
        ),
        PriceAnchorSettings::UniswapV3Twap {
            pool_address,
            base_token_address,
            quote_token_address,
            base_token_decimals,
            window_seconds,
        } => format!(
            "TWAP on {}: {} / {}, pool {}, {} decimals, {} seconds",
            chain_name(owner_chain_id)
                .map_or_else(|| owner_chain_id.to_string(), ToString::to_string),
            short_token_address(base_token_address),
            short_token_address(quote_token_address),
            short_token_address(pool_address),
            base_token_decimals,
            window_seconds,
        ),
    }
}

pub(in crate::root) fn settings_token_chain_header(chain_id: u64) -> gpui::Div {
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .pt(px(10.0))
        .pb(px(4.0))
        .font_family(APP_MONO_FONT_FAMILY)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(11.0))
        .line_height(px(14.0))
        .text_color(rgb(theme::TEXT_SUBTLE));
    if let Some(path) = chain_icon_asset_path(chain_id) {
        row = row.child(img(path).size(px(16.0)).flex_none());
    }
    row.child(SharedString::from(
        chain_name(chain_id)
            .map_or_else(|| chain_id.to_string(), ToString::to_string)
            .to_ascii_uppercase(),
    ))
}

pub(in crate::root) fn settings_icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    app_button_base(id)
        .icon(icon)
        .ghost()
        .xsmall()
        .compact()
        .tooltip(tooltip)
}

pub(in crate::root) fn settings_danger_icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    app_button_base(id)
        .icon(icon)
        .ghost()
        .xsmall()
        .compact()
        .tooltip(tooltip)
        .text_color(rgb(theme::DANGER))
}

pub(in crate::root) fn render_settings_url_dialog_content(
    input: &Entity<InputState>,
    content_width: Pixels,
    help: &'static str,
) -> gpui::Div {
    div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(app_muted_text(help))
        .child(settings_text_input(input))
}

pub(in crate::root) fn render_waku_direct_peer_dialog_content(
    inputs: &WakuDirectPeerDialogInputs,
    content_width: Pixels,
) -> gpui::Div {
    div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_3()
        .child(app_muted_text(
            "Enter one libp2p peer ID and one multiaddr. Add another row for additional addresses.",
        ))
        .child(dialog_text_field("Peer ID", &inputs.peer_id))
        .child(dialog_text_field("Multiaddr", &inputs.addr))
}

pub(in crate::root) fn dialog_text_field(
    label: &'static str,
    input: &Entity<InputState>,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(label))
        .child(settings_text_input(input))
}

pub(in crate::root) fn price_anchor_type_select_items() -> Vec<PriceAnchorTypeSelectItem> {
    vec![
        PriceAnchorTypeSelectItem {
            value: "fixed",
            label: "Fixed",
        },
        PriceAnchorTypeSelectItem {
            value: "oracle",
            label: "Oracle",
        },
        PriceAnchorTypeSelectItem {
            value: "uniswap-v3-twap",
            label: "Uniswap V3 TWAP",
        },
        PriceAnchorTypeSelectItem {
            value: "product",
            label: "Product",
        },
    ]
}

pub(in crate::root) fn product_component_type_select_items() -> Vec<PriceAnchorTypeSelectItem> {
    vec![
        PriceAnchorTypeSelectItem {
            value: "fixed",
            label: "Fixed",
        },
        PriceAnchorTypeSelectItem {
            value: "oracle",
            label: "Oracle",
        },
        PriceAnchorTypeSelectItem {
            value: "uniswap-v3-twap",
            label: "Uniswap V3 TWAP",
        },
    ]
}

pub(in crate::root) fn price_anchor_chain_select_items() -> Vec<ChainSelectItem> {
    railgun_ui::DEFAULT_CHAINS
        .iter()
        .map(|chain_id| ChainSelectItem {
            chain_id: *chain_id,
        })
        .collect()
}

pub(in crate::root) fn bool_select_items() -> Vec<BoolSelectItem> {
    vec![
        BoolSelectItem {
            value: false,
            label: "No",
        },
        BoolSelectItem {
            value: true,
            label: "Yes",
        },
    ]
}

pub(in crate::root) fn chain_select_index(
    items: &[ChainSelectItem],
    chain_id: u64,
) -> Option<IndexPath> {
    (!items.is_empty()).then(|| {
        IndexPath::default().row(
            items
                .iter()
                .position(|item| item.chain_id == chain_id)
                .unwrap_or_default(),
        )
    })
}

pub(in crate::root) fn price_anchor_type_select_index(
    items: &[PriceAnchorTypeSelectItem],
    value: &str,
) -> Option<IndexPath> {
    (!items.is_empty()).then(|| {
        IndexPath::default().row(
            items
                .iter()
                .position(|item| item.value.eq_ignore_ascii_case(value))
                .unwrap_or_default(),
        )
    })
}

pub(in crate::root) fn bool_select_index(value: bool) -> IndexPath {
    IndexPath::default().row(usize::from(value))
}
