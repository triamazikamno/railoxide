#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::root) struct UtxoDelegate {
    root: WeakEntity<WalletRoot>,
    rows: Arc<[UtxoDisplayRow]>,
    columns: [Column; 7],
    tx_search_input: Entity<InputState>,
    poi_refreshing: bool,
    poi_retry_session_available: bool,
    finality_context: UtxoFinalityContext,
}

/// The UTXO table, empty until a chain's notes load.
pub(in crate::root) fn new_utxo_table(
    root_weak: &WeakEntity<WalletRoot>,
    tx_search_input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TableState<UtxoDelegate>> {
    cx.new(|cx| {
        TableState::new(
            UtxoDelegate::new(root_weak.clone(), tx_search_input.clone()),
            window,
            cx,
        )
    })
}

impl UtxoDelegate {
    pub(in crate::root) fn new(
        root: WeakEntity<WalletRoot>,
        tx_search_input: Entity<InputState>,
    ) -> Self {
        Self {
            root,
            rows: Arc::from(Vec::<UtxoDisplayRow>::new()),
            columns: [
                Column::new("tree_position", "tree/position")
                    .width(px(120.0))
                    .movable(false),
                Column::new("generated", "generated")
                    .width(px(130.0))
                    .movable(false),
                Column::new("token", "token")
                    .width(px(150.0))
                    .movable(false),
                Column::new("amount", "amount")
                    .width(px(160.0))
                    .movable(false),
                Column::new("poi", "PPOI")
                    .width(px(POI_COLUMN_WIDTH))
                    .movable(false),
                Column::new("source_tx", "source tx")
                    .width(px(201.0))
                    .movable(false),
                Column::new("spent_tx", "spent tx")
                    .width(px(201.0))
                    .movable(false),
            ],
            tx_search_input,
            poi_refreshing: false,
            poi_retry_session_available: false,
            finality_context: UtxoFinalityContext::default(),
        }
    }

    pub(in crate::root) fn set_rows(
        &mut self,
        rows: Vec<UtxoDisplayRow>,
        poi_refreshing: bool,
        poi_retry_session_available: bool,
        finality_context: UtxoFinalityContext,
    ) {
        self.rows = Arc::from(rows);
        self.poi_refreshing = poi_refreshing;
        self.poi_retry_session_available = poi_retry_session_available;
        self.finality_context = finality_context;
    }

    pub(in crate::root) fn set_finality_context(&mut self, context: UtxoFinalityContext) -> bool {
        if self.finality_context == context {
            return false;
        }
        self.finality_context = context;
        true
    }

    pub(in crate::root) const fn set_poi_refreshing(&mut self, poi_refreshing: bool) -> bool {
        if self.poi_refreshing == poi_refreshing {
            return false;
        }
        self.poi_refreshing = poi_refreshing;
        true
    }

    pub(in crate::root) fn set_column_widths(&mut self, widths: &[Pixels]) {
        for (column, width) in self.columns.iter_mut().zip(widths.iter().copied()) {
            column.width = width;
        }
    }
}

impl TableDelegate for UtxoDelegate {
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
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        utxo_cell(self.columns[col_ix].name.clone())
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        let row = div().id(("row", row_ix));
        if self
            .rows
            .get(row_ix)
            .is_some_and(|row| row.pending_new || row.pending_spent || row.local_pending_spent)
        {
            return row.bg(rgb(theme::WARNING_BG));
        }
        if self.rows.get(row_ix).is_some_and(|row| row.is_spent) {
            return row.bg(rgb(theme::SPENT_ROW_BG));
        }
        row
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        utxo_cell(match col_ix {
            0 => div()
                .text_color(utxo_cell_text_color(row, rgb(theme::TEXT)))
                .child(SharedString::from(row.tree_position.clone()))
                .into_any_element(),
            1 => {
                let finality = pending_finality_display(row, self.finality_context);
                let tooltip = SharedString::from(finality.as_ref().map_or_else(
                    || local_datetime_label(row.source_block_timestamp),
                    |(_, detail)| {
                        format!(
                            "Generated {}. {detail}",
                            local_datetime_label(row.source_block_timestamp)
                        )
                    },
                ));
                div()
                    .id(SharedString::from(format!("wallet-generated-{row_ix}")))
                    .h_full()
                    .flex()
                    .flex_col()
                    .items_start()
                    .justify_center()
                    .gap(px(2.0))
                    .text_color(utxo_cell_text_color(row, rgb(theme::TEXT_MUTED)))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(div().text_size(px(13.0)).line_height(px(16.0)).child(
                        SharedString::from(generated_age_label(row.source_block_timestamp)),
                    ))
                    .when_some(finality, |this, (label, _)| {
                        this.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(rgb(theme::WARNING))
                                .child(Icon::new(RailgunActionIcon::Clock).xsmall())
                                .child(SharedString::from(label)),
                        )
                    })
                    .into_any_element()
            }
            2 => {
                let address = row.token_address.clone();
                let group = SharedString::from(format!("wallet-token-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!("wallet-token-cell-{row_ix}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .font_bold()
                    .text_color(utxo_cell_text_color(row, rgb(theme::TEXT)))
                    .child(token_label_row(
                        SharedString::from(row.token.clone()),
                        row.token_icon_path.clone(),
                        px(14.0),
                    ))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "wallet-token-address-copy-action-{row_ix}"
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
                                SharedString::from(format!(
                                    "wallet-token-address-clipboard-{row_ix}"
                                )),
                                address,
                            )),
                    )
                    .into_any_element()
            }
            3 => div()
                .text_color(utxo_cell_text_color(row, rgb(theme::WARNING)))
                .child(SharedString::from(row.amount.clone()))
                .into_any_element(),
            4 => div()
                .h_full()
                .flex()
                .flex_col()
                .items_start()
                .justify_center()
                .gap(px(2.0))
                .opacity(if row.is_spent { 0.6 } else { 1.0 })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(poi_status_indicator(row, row_ix))
                        .when(should_show_blocked_shield_refund_action(row), |this| {
                            this.child(blocked_shield_refund_action(row, row_ix, self.root.clone()))
                        })
                        .when(should_show_ppoi_retry_action(row), |this| {
                            this.child(ppoi_retry_action(
                                row,
                                row_ix,
                                self.root.clone(),
                                self.poi_refreshing,
                                self.poi_retry_session_available,
                            ))
                        }),
                )
                .when_some(
                    row.ppoi_last_submission_at
                        .filter(|_| should_show_ppoi_submission_age(row)),
                    |this, timestamp| {
                        this.child(
                            div()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(SharedString::from(format_ppoi_submission_age(
                                    timestamp,
                                    now_epoch_secs(),
                                ))),
                        )
                    },
                )
                .into_any_element(),
            5 => source_tx_cell(
                row,
                row_ix,
                &row.source_tx_hash,
                self.tx_search_input.clone(),
            ),
            _ => match row.spent_tx_hash.as_deref() {
                Some(tx_hash) => tx_hash_cell(
                    row,
                    row_ix,
                    "spent",
                    tx_hash,
                    rgb(theme::DANGER),
                    self.tx_search_input.clone(),
                ),
                None => div()
                    .text_color(rgb(theme::TEXT_SUBTLE))
                    .child("-")
                    .into_any_element(),
            },
        })
    }
}
