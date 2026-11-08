use super::*;

pub(in crate::root) const SETTINGS_GROUP_CONTENT_INDENT: Pixels = px(16.0);

pub(in crate::root) fn settings_group() -> SettingGroup {
    SettingGroup::new().pl(SETTINGS_GROUP_CONTENT_INDENT)
}

pub(in crate::root) fn settings_danger_banner(message: impl Into<SharedString>) -> gpui::Div {
    div()
        .w_full()
        .child(Alert::error("wallet-settings-danger-banner", message.into().to_string()).small())
}

/// A dialog's validation error. The page status banner sits behind the modal, so a dialog that
/// stays open shows its error itself.
pub(in crate::root) fn settings_dialog_error(error: SharedString) -> gpui::Div {
    app_muted_text(error)
        .text_color(rgb(theme::DANGER))
        .whitespace_normal()
}

/// Shows `message` through [`settings_dialog_error`] on the dialog's next render.
pub(in crate::root) fn set_dialog_error(
    error: &Entity<Option<SharedString>>,
    message: impl Into<SharedString>,
    cx: &mut App,
) {
    error.update(cx, |error, cx| {
        *error = Some(message.into());
        cx.notify();
    });
}

/// A row of a Settings list. Every row but the `last` has a divider below it.
pub(in crate::root) fn settings_list_row(
    id: impl Into<ElementId>,
    last: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .px(px(2.0))
        .py(px(9.0))
        .hover(|this| this.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
        .when(!last, |this| {
            this.border_b_1()
                .border_color(rgb_with_alpha(theme::BORDER_SUBTLE, 0.45))
        })
}

/// The trailing group of a Settings list row's action buttons.
pub(in crate::root) fn settings_row_actions() -> gpui::Div {
    div().flex().flex_none().items_center().gap_1()
}

pub(in crate::root) fn settings_info_banner(message: impl Into<SharedString>) -> gpui::Div {
    div()
        .w_full()
        .child(Alert::info("wallet-settings-info-banner", message.into().to_string()).small())
}

pub(in crate::root) fn settings_warning_banner(
    id: impl Into<ElementId>,
    message: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .w_full()
        .child(Alert::warning(id, message.into().to_string()).small())
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
        .when(!readonly_identity, |this| {
            this.child(settings_warning_banner(
                "wallet-settings-token-risk-warning",
                "Tokens that rebase, charge transfer fees, or restrict transfers may fail in Railgun \
                 or cause loss of funds. Adding a token makes it available for swaps on supported \
                 chains and means you accept these risks.",
            ))
        })
        .child(settings_dialog_field(
            "Chain ID",
            &inputs.chain_id,
            readonly_identity,
        ))
        .when(!readonly_identity, |this| {
            this.child(app_muted_text(format!(
                "For a Tron token, use chain ID {} and its base58 address.",
                railgun_ui::TRON_CHAIN_ID
            )))
        })
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
        .font_weight(FontWeight::MEDIUM)
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
                        .font_weight(FontWeight::MEDIUM)
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
                .child(SharedString::from(display_token_address(
                    entry.chain_id,
                    &entry.token_address,
                ))),
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
    let token_address = display_token_address(entry.key.chain_id, &entry.key.token_address);
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
                        .font_weight(FontWeight::MEDIUM)
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
        |_| {
            if wallet_ops::tron::address::parse_tron_address(token_address).is_ok() {
                railgun_ui::short_non_evm(token_address)
            } else {
                token_address.to_string()
            }
        },
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
            let chain = token_chain_name(*chain_id);
            let inverse = if *is_inversed { ", inverse" } else { "" };
            format!(
                "Oracle {} on {chain}, {token_decimals}/{oracle_decimals} decimals{inverse}",
                short_token_address(&display_token_address(*chain_id, oracle_address))
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

/// Token metadata names Tron by its chain id, which the EVM chain lookups leave unnamed.
fn token_chain_name(chain_id: u64) -> String {
    if chain_id == railgun_ui::TRON_CHAIN_ID {
        return railgun_ui::TRON_NAME.to_owned();
    }
    chain_name(chain_id).map_or_else(|| chain_id.to_string(), ToString::to_string)
}

pub(in crate::root) fn settings_token_chain_header(chain_id: u64) -> gpui::Div {
    let icon = if chain_id == railgun_ui::TRON_CHAIN_ID {
        Some(railgun_ui::TRON_ICON_ASSET_PATH)
    } else {
        chain_icon_asset_path(chain_id)
    };
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .pt(px(10.0))
        .pb(px(4.0))
        .font_family(APP_MONO_FONT_FAMILY)
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(11.0))
        .line_height(px(14.0))
        .text_color(rgb(theme::TEXT_SUBTLE));
    if let Some(path) = icon {
        row = row.child(img(path).size(px(16.0)).flex_none());
    }
    row.child(SharedString::from(
        token_chain_name(chain_id).to_ascii_uppercase(),
    ))
}

pub(in crate::root) fn settings_icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    let tooltip: SharedString = tooltip.into();
    app_button_base(id)
        .icon(icon)
        .ghost()
        .xsmall()
        .compact()
        .accessibility_label(tooltip.clone())
        .tooltip(tooltip)
}

pub(in crate::root) fn settings_danger_icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    tooltip: impl Into<SharedString>,
) -> Button {
    let tooltip: SharedString = tooltip.into();
    app_button_base(id)
        .icon(icon)
        .ghost()
        .xsmall()
        .compact()
        .accessibility_label(tooltip.clone())
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

pub(in crate::root) fn price_anchor_chain_select_items(
    settings: &WalletSettings,
) -> Vec<NetworkSelectItem> {
    let mut items: Vec<_> = railgun_ui::built_in_chain_ids()
        .map(|chain_id| NetworkSelectItem {
            network: Network::Evm(chain_id),
            label: chain_name(chain_id).unwrap_or("Chain").into(),
        })
        .chain([NetworkSelectItem {
            network: Network::Tron,
            label: railgun_ui::TRON_NAME.into(),
        }])
        .collect();
    items.extend(
        settings
            .chains
            .custom
            .iter()
            .map(|(&chain_id, chain)| NetworkSelectItem {
                network: Network::Evm(chain_id),
                label: chain.name.clone().into(),
            }),
    );
    items.sort_by_key(|item| price_anchor_chain_id(item.network));
    items
}

/// Token metadata keys Tron by [`railgun_ui::TRON_CHAIN_ID`]; the select item keeps Tron
/// distinct so it shows Tron's icon.
pub(in crate::root) const fn price_anchor_chain_id(network: Network) -> u64 {
    match network {
        Network::Evm(chain_id) => chain_id,
        Network::Tron => railgun_ui::TRON_CHAIN_ID,
    }
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
    items: &[NetworkSelectItem],
    chain_id: u64,
) -> Option<IndexPath> {
    (!items.is_empty()).then(|| {
        IndexPath::default().row(
            items
                .iter()
                .position(|item| price_anchor_chain_id(item.network) == chain_id)
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
