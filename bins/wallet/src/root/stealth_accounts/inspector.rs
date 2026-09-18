use super::view::{account_caption, has_local_history};
use super::{
    Context, Disableable, ExecutorAsset, ExecutorRecord, IntoElement, ParentElement, SharedString,
    Sizable, StealthAccountsView, Styled, app_button, app_strong_text, asset_label, div,
};
use gpui::{InteractiveElement as _, prelude::FluentBuilder as _};
use gpui_component::button::ButtonVariants as _;
use gpui_component::{
    ActiveTheme as _, ChildElement as _,
    description_list::{DescriptionItem, DescriptionList},
    separator::Separator,
    table::{Table, TableBody, TableCell, TableHead, TableHeader, TableRow},
    tag::Tag,
};
use wallet_ops::vault::{ExecutorPayloadStatus, IssuedExecutorPayload};

mod history;
use history::{block_label, payload_purpose, payload_rows, recorded_outcomes, status_as_of};

impl StealthAccountsView {
    pub(super) fn render_inspector(
        &self,
        record: &ExecutorRecord,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let operation = record.operation();
        let local_history = has_local_history(record);
        let recovery_disabled = self.recovery_disabled_reason(record, cx);
        let mut content = div()
            .debug_selector(|| "stealth-outcome-summary".into())
            .flex()
            .flex_col()
            .gap_2()
            .child(app_strong_text(if local_history {
                "What happened"
            } else {
                "Account"
            }))
            .children(record.purpose_summary().map(|purpose| {
                Self::purpose(
                    format!("stealth-inspector-recipient-{}", operation.opaque_id()).into(),
                    purpose,
                    cx,
                )
            }));
        let outcomes = recorded_outcomes(record);
        if local_history && outcomes.is_empty() {
            content = content.child(inspector_text("No confirmed result is recorded."));
        }
        content = content.children(
            outcomes
                .into_iter()
                .map(|outcome| inspector_text(outcome).whitespace_normal()),
        );
        if !record.issued().is_empty() || !record.recovery_transactions().is_empty() {
            content = content.child(
                inspector_text(status_as_of(record, self.status(record).rechecked()))
                    .whitespace_normal(),
            );
        }
        let body = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_4()
            .child(inspector_columns(
                content,
                Some(self.render_balances(record, cx)),
            ))
            .when_some(
                self.render_account_details(record, cx),
                |content, details| content.child(Separator::horizontal()).child(details),
            )
            .when(self.holding(operation), |inspector| {
                inspector.child(
                    div().flex().child(
                        app_button("stealth-inspector-recover", "Recover…")
                            .debug_selector(|| "stealth-inspector-recover".into())
                            .primary()
                            .disabled(recovery_disabled.is_some())
                            .when_some(recovery_disabled, gpui_component::button::Button::tooltip)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_recovery(operation, None, window, cx);
                            })),
                    ),
                )
            });
        div()
            .w_full()
            .min_w_0()
            .p_3()
            .bg(cx.theme().muted)
            .text_color(cx.theme().foreground)
            .child(body)
    }

    fn render_balances(&self, record: &ExecutorRecord, cx: &Context<'_, Self>) -> gpui::Div {
        let operation = record.operation();
        let busy = self.job.is_some();
        let mut rows = TableBody::new();
        for asset in self.assets_for(record) {
            let address = match asset {
                ExecutorAsset::Native => None,
                ExecutorAsset::Erc20(address)
                | ExecutorAsset::Erc721 {
                    collection: address,
                    ..
                } => Some(address),
            };
            let icon = address.map_or_else(
                || {
                    railgun_ui::chain_icon_asset_path(self.session.chain_id)
                        .map(crate::assets::WalletIconSource::embedded)
                },
                |address| {
                    self.root.upgrade().and_then(|root| {
                        crate::root::tokens::token_display_metadata(
                            Some(&root.read(cx).effective_token_registry),
                            self.session.chain_id,
                            &address,
                        )
                        .and_then(|metadata| metadata.icon_path)
                    })
                },
            );
            let (value, date) = self.balance_value_lines(operation, asset, cx);
            let unavailable = self
                .observations
                .get(&operation)
                .and_then(|observations| observations.assets.get(&asset))
                .is_some_and(|balance| {
                    balance.attempt == super::observations::CheckAttempt::Unavailable
                });
            rows = rows.child(
                TableRow::new()
                    .child(
                        TableCell::new().flex_1().min_w_0().child(
                            ui::private_action::asset_row(
                                self.asset_name(asset, cx),
                                icon.map(Into::into),
                            )
                            .children(address.map(|address| {
                                Self::copy_identifier_button(
                                    format!(
                                        "stealth-asset-address-{}-{}",
                                        operation.opaque_id(),
                                        asset_label(asset)
                                    )
                                    .into(),
                                    address.to_checksum(None),
                                    "Copy token address",
                                    cx,
                                )
                            })),
                        ),
                    )
                    .child(
                        TableCell::new().flex_1().min_w_0().child(
                            div()
                                .child(ui::controls::app_text(value).whitespace_normal())
                                .children(date.map(|date| {
                                    account_caption(date)
                                        .whitespace_normal()
                                        .when(unavailable, |text| {
                                            text.text_color(cx.theme().warning)
                                        })
                                })),
                        ),
                    ),
            );
        }
        rows = rows.child(
            TableRow::new().child(
                TableCell::new().col_span(2).w_full().child(
                    div()
                        .track_focus(&self.add_token_focus)
                        .tab_group()
                        .tab_stop(false)
                        .child(
                            app_button("stealth-add-token", "Add token…")
                                .debug_selector(|| "stealth-add-token".into())
                                .ghost()
                                .small()
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_add_token(operation, window, cx);
                                })),
                        ),
                ),
            ),
        );
        div()
            .debug_selector(|| "stealth-balances".into())
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(app_strong_text("Balances"))
                    .child(
                        app_button("stealth-check-balances", "Check balances")
                            .debug_selector(|| "stealth-check-balances".into())
                            .outline()
                            .small()
                            .tooltip("Each check includes native balance, nonces and account code")
                            .disabled(busy || record.address().is_none())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(record) = this
                                    .records
                                    .iter()
                                    .find(|record| record.operation() == operation)
                                {
                                    this.check_record(operation, this.assets_for(record), cx);
                                }
                            })),
                    ),
            )
            .child(
                inspector_table("Account balances", cx)
                    .child(
                        TableHeader::new().child(
                            TableRow::new()
                                .child(
                                    TableHead::new()
                                        .flex_1()
                                        .min_w_0()
                                        .child(account_caption("Asset")),
                                )
                                .child(
                                    TableHead::new()
                                        .flex_1()
                                        .min_w_0()
                                        .child(account_caption("Balance")),
                                ),
                        ),
                    )
                    .child(rows),
            )
    }

    fn render_account_details(
        &self,
        record: &ExecutorRecord,
        cx: &Context<'_, Self>,
    ) -> Option<gpui::Div> {
        let operation = record.operation();
        let local_history = has_local_history(record);
        let inspection = self
            .observations
            .get(&operation)
            .and_then(|observations| observations.inspection.as_ref());
        if !local_history && inspection.is_none() {
            return None;
        }
        let mut metadata = metadata_list().child(description_item(
            "Index",
            inspector_text(format!("#{}", record.index())),
        ));
        if local_history {
            metadata = metadata.child(description_item(
                "Delegate",
                Self::address(
                    format!("stealth-delegate-{}", operation.opaque_id()).into(),
                    record.delegate(),
                    cx,
                )
                .text_color(cx.theme().foreground),
            ));
        }
        if let Some(inspection) = inspection {
            metadata = metadata
                .child(description_item(
                    "Observed",
                    inspector_text(format!("#{}", block_label(inspection.block().number))),
                ))
                .child(description_item(
                    "Account nonce",
                    inspector_text(
                        inspection
                            .account_nonce()
                            .map_or_else(|| "Unknown".into(), |nonce| nonce.to_string()),
                    ),
                ))
                .child(description_item(
                    "Execution nonce",
                    inspector_text(
                        inspection
                            .execution_nonce()
                            .map_or_else(|| "Unknown".into(), |nonce| nonce.to_string()),
                    ),
                ));
        }
        let mut history = div()
            .debug_selector(|| "stealth-payloads".into())
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3();
        if !record.issued().is_empty() {
            let rows = payload_rows(record);
            history = history.child(app_strong_text("Signed payloads")).child(
                inspector_table("Signed payloads", cx)
                    .child(payload_header("Nonce", true))
                    .child(
                        TableBody::new().children(
                            rows.iter()
                                .map(|payload| Self::payload_row(record, payload, cx)),
                        ),
                    ),
            );
        }
        if !record.recovery_transactions().is_empty() {
            let rows = record.recovery_transactions().iter().map(|transaction| {
                TableRow::new()
                    .child(
                        TableCell::new()
                            .w(gpui::rems(4.))
                            .min_w_0()
                            .flex_none()
                            .child(inspector_text((transaction.step() + 1).to_string())),
                    )
                    .child(
                        TableCell::new().flex_1().min_w_0().child(
                            Self::copyable_identifier(
                                format!("stealth-recovery-{}", transaction.hash()).into(),
                                transaction.hash().to_string(),
                                crate::root::utxo::short_hash(&transaction.hash().to_string()),
                                "Copy transaction hash",
                                cx,
                            )
                            .text_color(cx.theme().foreground),
                        ),
                    )
                    .child(
                        TableCell::new()
                            .w(gpui::rems(7.))
                            .min_w_0()
                            .flex_none()
                            .child(inspector_text(transaction.inclusion().map_or_else(
                                || "Not seen".into(),
                                |inclusion| format!("#{}", block_label(inclusion.block().number)),
                            ))),
                    )
                    .child(
                        TableCell::new()
                            .w(gpui::rems(9.))
                            .min_w_0()
                            .flex_none()
                            .child(payload_result(
                                record.recorded_recovery_transaction_status(transaction.hash()),
                                cx,
                            )),
                    )
            });
            history = history
                .child(app_strong_text("Recovery transactions"))
                .child(
                    inspector_table("Recovery transactions", cx)
                        .with_ix(1)
                        .child(payload_header("Step", false))
                        .child(TableBody::new().children(rows)),
                );
        }
        Some(inspector_columns(
            div()
                .debug_selector(|| "stealth-account-details".into())
                .w_full()
                .flex()
                .min_w_0()
                .flex_col()
                .gap_2()
                .child(app_strong_text("Account details"))
                .child(metadata),
            (!record.issued().is_empty() || !record.recovery_transactions().is_empty())
                .then_some(history),
        ))
    }
    fn payload_row(
        record: &ExecutorRecord,
        payload: &IssuedExecutorPayload,
        cx: &Context<'_, Self>,
    ) -> TableRow {
        let transaction = payload
            .inclusion()
            .map(wallet_ops::vault::ExecutorPayloadInclusion::transaction_hash)
            .or_else(|| payload.transaction_hashes().last().copied());
        let mut identifier = div().flex().flex_wrap().items_center().gap_1();
        if let Some(transaction) = transaction {
            identifier = identifier.child(
                Self::copyable_identifier(
                    format!("stealth-transaction-{}-{transaction}", payload.hash()).into(),
                    transaction.to_string(),
                    crate::root::utxo::short_hash(&transaction.to_string()),
                    "Copy transaction hash",
                    cx,
                )
                .text_color(cx.theme().foreground),
            );
        } else {
            identifier = identifier.child(inspector_text("Not submitted"));
        }
        TableRow::new()
            .child(
                TableCell::new()
                    .w(gpui::rems(4.))
                    .min_w_0()
                    .flex_none()
                    .child(inspector_text(payload.nonce().to_string())),
            )
            .child(
                TableCell::new()
                    .w(gpui::rems(10.))
                    .min_w_0()
                    .flex_none()
                    .child(inspector_text(payload_purpose(payload)).whitespace_normal()),
            )
            .child(TableCell::new().flex_1().min_w_0().child(identifier))
            .child(
                TableCell::new()
                    .w(gpui::rems(7.))
                    .min_w_0()
                    .flex_none()
                    .child(inspector_text(payload.inclusion().map_or_else(
                        || "Not seen".into(),
                        |inclusion| format!("#{}", block_label(inclusion.block().number)),
                    ))),
            )
            .child(
                TableCell::new()
                    .w(gpui::rems(9.))
                    .min_w_0()
                    .flex_none()
                    .child(payload_result(
                        record.recorded_payload_status(payload.hash()),
                        cx,
                    )),
            )
    }
}

fn inspector_columns(summary: gpui::Div, table: Option<gpui::Div>) -> gpui::Div {
    // Both rows share column widths so their tables align and wrap together.
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_start()
        .gap_6()
        .child(summary.flex_grow(1.).flex_basis(gpui::rems(26.)).min_w_0())
        .children(table.map(|table| table.w(gpui::rems(48.)).max_w_full().min_w_0().ml_auto()))
}

fn inspector_text(label: impl Into<SharedString>) -> gpui::Div {
    ui::controls::app_text(label).text_xs()
}

fn metadata_list() -> DescriptionList {
    DescriptionList::horizontal()
        .small()
        .bordered(false)
        .columns(1)
        .label_width(gpui::rems(8.))
}

fn description_item(label: &'static str, value: impl IntoElement) -> DescriptionItem {
    DescriptionItem::new(account_caption(label).into_any_element()).value(value.into_any_element())
}

fn inspector_table(label: &'static str, cx: &gpui::App) -> Table {
    Table::new()
        .accessibility_label(label)
        .small()
        .line_height(gpui::relative(ui::theme::APP_TEXT_LINE_HEIGHT))
        .border_1()
        .border_color(cx.theme().border)
        .rounded_md()
}

fn payload_header(first: &'static str, purpose: bool) -> TableHeader {
    TableHeader::new().child(
        TableRow::new()
            .child(
                TableHead::new()
                    .w(gpui::rems(4.))
                    .min_w_0()
                    .flex_none()
                    .child(account_caption(first)),
            )
            .when(purpose, |row| {
                row.child(
                    TableHead::new()
                        .w(gpui::rems(10.))
                        .min_w_0()
                        .flex_none()
                        .child(account_caption("Purpose")),
                )
            })
            .child(
                TableHead::new()
                    .flex_1()
                    .min_w_0()
                    .child(account_caption("Transaction")),
            )
            .child(
                TableHead::new()
                    .w(gpui::rems(7.))
                    .min_w_0()
                    .flex_none()
                    .child(account_caption("Included")),
            )
            .child(
                TableHead::new()
                    .w(gpui::rems(9.))
                    .min_w_0()
                    .flex_none()
                    .child(account_caption("Result")),
            ),
    )
}

fn payload_result(status: Option<ExecutorPayloadStatus>, cx: &gpui::App) -> gpui::Div {
    let (tag, label) = match status {
        Some(ExecutorPayloadStatus::Executed) => (Tag::success(), "Executed"),
        Some(ExecutorPayloadStatus::Reverted) => (Tag::danger(), "Reverted"),
        Some(ExecutorPayloadStatus::MissingEffects) => (Tag::warning(), "Effects missing"),
        Some(ExecutorPayloadStatus::Invalidated { .. }) => (Tag::secondary(), "Superseded"),
        Some(ExecutorPayloadStatus::Uncertain) | None => (Tag::secondary(), "Unconfirmed"),
    };
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(
            tag.outline()
                .small()
                .rounded_full()
                .line_height(gpui::relative(ui::theme::APP_TEXT_LINE_HEIGHT))
                .when(
                    matches!(
                        status,
                        Some(
                            ExecutorPayloadStatus::Uncertain
                                | ExecutorPayloadStatus::Invalidated { .. }
                        ) | None
                    ),
                    |tag| tag.text_color(cx.theme().foreground),
                )
                .child(label),
        )
        .when(
            matches!(status, Some(ExecutorPayloadStatus::Uncertain) | None),
            |cell| cell.child(inspector_text("No result recorded")),
        )
}
