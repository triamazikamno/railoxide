use super::view::{account_caption, has_local_history};
use super::{
    Context, Disableable, ExecutorAsset, ExecutorRecord, IntoElement, ParentElement, SharedString,
    Sizable, StealthAccountsView, Styled, app_button, app_strong_text, asset_label, div,
};
use gpui::{InteractiveElement as _, prelude::FluentBuilder as _};
use gpui_component::button::ButtonVariants as _;
use gpui_component::{
    ActiveTheme as _,
    description_list::{DescriptionItem, DescriptionList},
    separator::Separator,
    table::{Table, TableBody, TableCell, TableHead, TableHeader, TableRow},
    tag::Tag,
};
use wallet_ops::{ExecutorAttributionEvidence, ExecutorPayloadOutcome, ExecutorSignedAction};

mod history;
use history::{action_payload, block_label, payload_purpose, recorded_outcomes, status_as_of};

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
        let attribution = self.owner.attribution(record);
        let evidence = match &attribution {
            Some(attribution) => attribution.evidence(),
            None => ExecutorAttributionEvidence::record_only(),
        };
        let actions = wallet_ops::signed_actions(record, &evidence);
        content = content.children(
            self.outcome_lines(record, &actions, &evidence)
                .into_iter()
                .map(|line| inspector_text(line).whitespace_normal()),
        );
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
                self.render_account_details(record, &actions, cx),
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
                                this.open_recovery(operation, None, None, window, cx);
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

    /// The recorded result of each signed action, the block those results stand at, and for
    /// unconfirmed work whether an account read tells that it is late.
    fn outcome_lines(
        &self,
        record: &ExecutorRecord,
        actions: &[ExecutorSignedAction],
        evidence: &ExecutorAttributionEvidence<'_>,
    ) -> Vec<String> {
        let mut lines = recorded_outcomes(record, actions, evidence);
        if lines.is_empty() && has_local_history(record) {
            lines.push("No result is recorded.".into());
        }
        if record.issued().is_empty() {
            return lines;
        }
        let status = self.status(record);
        lines.push(status_as_of(record, status.read_this_session()));
        let pending = actions
            .iter()
            .any(|action| action.outcome() == ExecutorPayloadOutcome::Pending);
        // Lateness is asserted only from an account read past the submission.
        if pending {
            lines.push(match status.read_this_session() {
                Some(read) if status.overdue() => format!(
                    "An account read at #{}, after the submission, still shows it unconfirmed.",
                    block_label(read.number)
                ),
                _ => "Not read since the submission, so nothing tells whether it is late. Check balances reads the account.".into(),
            });
        }
        lines
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
                    railgun_ui::native_currency_icon_asset_path(self.session.chain_id)
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
                            .tooltip("Each check reads the native balance, account code and nonces, and updates the recorded results")
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
        actions: &[ExecutorSignedAction],
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
            history = history.child(app_strong_text("Signed payloads")).child(
                inspector_table("Signed payloads", cx)
                    .child(payload_header("Nonce", true))
                    .child(
                        TableBody::new().children(
                            actions
                                .iter()
                                .filter_map(|action| Self::payload_row(record, action, cx)),
                        ),
                    ),
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
            (!record.issued().is_empty()).then_some(history),
        ))
    }
    /// One signed action: its fee rounds share a row, a result and a submitted transaction.
    fn payload_row(
        record: &ExecutorRecord,
        action: &ExecutorSignedAction,
        cx: &Context<'_, Self>,
    ) -> Option<TableRow> {
        let payload = action_payload(record, action)?;
        let mut identifier = div().flex().flex_wrap().items_center().gap_1();
        if let Some(transaction) = payload.transaction_hashes().last().copied() {
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
        Some(
            TableRow::new()
                .child(
                    TableCell::new()
                        .w(gpui::rems(4.))
                        .min_w_0()
                        .flex_none()
                        .child(inspector_text(action.nonce().to_string())),
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
                        .children(
                            action
                                .spend_block()
                                .map(|block| inspector_text(format!("#{}", block_label(block)))),
                        ),
                )
                .child(
                    TableCell::new()
                        .w(gpui::rems(9.))
                        .min_w_0()
                        .flex_none()
                        .child(payload_result(action.outcome(), cx)),
                ),
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
                    .child(account_caption("Spent in")),
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

fn payload_result(outcome: ExecutorPayloadOutcome, cx: &gpui::App) -> gpui::Div {
    let (tag, label) = match outcome {
        ExecutorPayloadOutcome::Executed => (Tag::success(), "Executed"),
        ExecutorPayloadOutcome::Superseded => (Tag::secondary(), "Superseded"),
        ExecutorPayloadOutcome::Resolved => (Tag::secondary(), "Nonce used"),
        ExecutorPayloadOutcome::Pending => (Tag::secondary(), "Unconfirmed"),
    };
    let pending = outcome == ExecutorPayloadOutcome::Pending;
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
                .when(outcome != ExecutorPayloadOutcome::Executed, |tag| {
                    tag.text_color(cx.theme().foreground)
                })
                .child(label),
        )
        .when(pending, |cell| {
            cell.child(inspector_text("No result recorded"))
        })
}
