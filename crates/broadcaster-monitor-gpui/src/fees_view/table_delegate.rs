#[allow(clippy::wildcard_imports)]
use super::*;

impl TableDelegate for FeesDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let table = cx.entity();
        match col_ix {
            0 => render_chain_header(&self.chain_select).into_any_element(),
            1 => div()
                .id("fees-broadcaster-filter-input")
                .size_full()
                .flex()
                .items_center()
                .on_click(|_event, _window, cx| {
                    cx.stop_propagation();
                })
                .child(
                    Input::new(&self.broadcaster_input)
                        .bg(rgb(theme::SURFACE))
                        .with_size(Size::XSmall)
                        .w_full(),
                )
                .into_any_element(),
            2 => render_token_header(&self.token_select).into_any_element(),
            3 => render_fee_header(self.fee_sort, table).into_any_element(),
            4 => render_bonus_header(self.bonus_sort, table).into_any_element(),
            _ => div()
                .size_full()
                .child(self.columns[col_ix].name.clone())
                .into_any_element(),
        }
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from(format!("broadcaster-fee-row-{row_ix}")))
            .group(fee_row_group(row_ix))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        match col_ix {
            0 => {
                if matches!(self.chain_filter, FeesFilter::All) {
                    div()
                        .text_color(rgb(theme::PURPLE))
                        .child(icon_label_row(
                            row.chain_id,
                            SharedString::from(""),
                            px(16.0),
                        ))
                        .into_any_element()
                } else {
                    div().into_any_element()
                }
            }
            1 => {
                let addr = row.railgun_address.as_ref();
                let label = format_broadcaster_address_label(addr, row.identifier.as_deref());
                let addr = addr.to_string();
                let group = SharedString::from(format!("broadcaster-addr-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!(
                        "broadcaster-addr-cell-{row_ix}"
                    )))
                    .flex()
                    .items_center()
                    .gap_1()
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_color(rgb(theme::PURPLE))
                    .children(
                        self.preference_hooks
                            .as_ref()
                            .map(|hooks| render_preference_cell(row_ix, row, hooks)),
                    )
                    .child(SharedString::from(label))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "broadcaster-addr-copy-action-{row_ix}"
                            )))
                            .group(group.clone())
                            .flex_none()
                            .opacity(0.0)
                            .group_hover(group, |this| this.opacity(1.0))
                            .hover(|this| this.opacity(1.0))
                            .tooltip(|window, cx| {
                                Tooltip::new("Copy broadcaster address").build(window, cx)
                            })
                            .child(clipboard_with_toast(
                                SharedString::from(format!("broadcaster-addr-clipboard-{row_ix}")),
                                addr,
                            )),
                    )
                    .into_any_element()
            }
            2 => {
                let label = lookup_token(row.chain_id, &row.token_address).map_or_else(
                    || short_address(&row.token_address),
                    |info| info.symbol.to_owned(),
                );
                let addr_for_clipboard = row.token_address.to_string();
                let group = SharedString::from(format!("token-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!("token-cell-{row_ix}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(rgb(theme::TEXT))
                    .child(token_label_row(
                        row.chain_id,
                        &row.token_address,
                        SharedString::from(label),
                        px(14.0),
                    ))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "token-address-copy-action-{row_ix}"
                            )))
                            .group(group.clone())
                            .flex_none()
                            .opacity(0.0)
                            .group_hover(group, |this| this.opacity(1.0))
                            .hover(|this| this.opacity(1.0))
                            .tooltip(|window, cx| {
                                Tooltip::new("Copy token address").build(window, cx)
                            })
                            .child(clipboard_with_toast(
                                SharedString::from(format!("token-address-clipboard-{row_ix}")),
                                addr_for_clipboard,
                            )),
                    )
                    .into_any_element()
            }
            3 => {
                let raw_fee = row.fee.to_string();
                let label = raw_fee_label(row);
                let group = SharedString::from(format!("fee-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!("fee-cell-{row_ix}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(rgb(theme::WARNING))
                    .tooltip({
                        let raw_fee = raw_fee.clone();
                        move |window, cx| Tooltip::new(raw_fee.clone()).build(window, cx)
                    })
                    .child(SharedString::from(label))
                    .child(
                        div()
                            .id(SharedString::from(format!("fee-copy-action-{row_ix}")))
                            .group(group.clone())
                            .flex_none()
                            .opacity(0.0)
                            .group_hover(group, |this| this.opacity(1.0))
                            .hover(|this| this.opacity(1.0))
                            .tooltip(|window, cx| Tooltip::new("Copy raw fee").build(window, cx))
                            .child(clipboard_with_toast(
                                SharedString::from(format!("fee-clipboard-{row_ix}")),
                                raw_fee,
                            )),
                    )
                    .into_any_element()
            }
            4 => render_fee_bonus_cell(row, &self.fee_anchor_lookup).into_any_element(),
            5 => {
                let color = if row.reliability >= 0.9 {
                    rgb(theme::SUCCESS)
                } else {
                    rgb(theme::WARNING_STRONG)
                };
                div()
                    .text_color(color)
                    .child(SharedString::from(format!("{:.2}", row.reliability)))
                    .into_any_element()
            }
            6 => {
                let age = SystemTime::now()
                    .duration_since(row.last_seen)
                    .unwrap_or_default();
                div()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(SharedString::from(format_relative_age(age)))
                    .into_any_element()
            }
            7 => {
                let now = SystemTime::now();
                if let Ok(d) = row.fee_expiration.duration_since(now) {
                    div()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(format_compact_duration(d)))
                        .into_any_element()
                } else {
                    let age = now.duration_since(row.fee_expiration).unwrap_or_default();
                    div()
                        .text_color(rgb(theme::DANGER))
                        .child(SharedString::from(format!(
                            "expired {}",
                            format_relative_age(age)
                        )))
                        .into_any_element()
                }
            }
            _ => div().into_any_element(),
        }
    }
}

pub(super) fn fee_columns(with_preferences: bool) -> Vec<Column> {
    let broadcaster_width = if with_preferences {
        px(316.0)
    } else {
        px(240.0)
    };
    let columns = vec![
        Column::new("chain", "chain").width(px(60.0)).movable(false),
        Column::new("broadcaster", "broadcaster")
            .width(broadcaster_width)
            .movable(false),
        Column::new("token", "token")
            .width(px(120.0))
            .movable(false),
        // Sorting is driven by our own cell-wide click handler in `render_th`
        // instead of the built-in sort icon, whose hitbox is too small.
        Column::new("fee", "fee").width(px(100.0)).movable(false),
        Column::new("bonus", "bonus %")
            .width(px(78.0))
            .movable(false),
        Column::new("reliability", "rel")
            .width(px(50.0))
            .movable(false),
        Column::new("last_seen", "last seen")
            .width(px(120.0))
            .movable(false),
        Column::new("expires", "expires in")
            .width(px(120.0))
            .movable(false),
    ];
    columns
}
