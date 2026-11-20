#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn open_private_pending_status_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.private_pending_status_dialog_open = true;
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            let content = content_root
                .read(cx)
                .current_private_pending_summary()
                .map_or_else(
                    || private_pending_status_empty(content_width),
                    |summary| {
                        Self::render_private_pending_status_dialog_content(
                            &content_root,
                            &summary,
                            content_width,
                        )
                    },
                );
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Private asset status"))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, _cx| {
                        root.private_pending_status_dialog_open = false;
                    });
                })
                .child(content)
        });
    }

    fn current_private_pending_summary(&self) -> Option<PrivatePendingSummary> {
        let snapshot = self.chain_states.get(&self.last_evm_chain)?.snapshot()?;
        let presentation = self.private_asset_presentation(snapshot);
        self.private_pending_summary_from_snapshot(snapshot, &presentation.rows)
    }

    pub(super) fn private_pending_summary_from_snapshot(
        &self,
        snapshot: &ListUtxosOutput,
        assets: &[FormattedTokenTotal],
    ) -> Option<PrivatePendingSummary> {
        let chain_state = self.chain_states.get(&self.last_evm_chain);
        let blocked_shield_rows = blocked_shield_rescue_display_rows(
            snapshot,
            &self.blocked_shield_rescue_rows,
            &self.blocked_shield_refunds_in_flight,
        );
        private_pending_summary_with_workflow(
            assets,
            snapshot,
            blocked_shield_rows,
            chain_state.map_or_else(
                WalletPpoiWorkflowStatus::default,
                ChainUtxoState::ppoi_workflow_status,
            ),
            chain_state.is_some_and(ChainUtxoState::poi_refreshing),
            chain_state.and_then(ChainUtxoState::poi_refresh_session),
        )
    }

    pub(in crate::root) fn private_pending_status_has_shield_timer(&self) -> bool {
        let Some(snapshot) = self
            .chain_states
            .get(&self.last_evm_chain)
            .and_then(ChainUtxoState::snapshot)
        else {
            return false;
        };
        let waits = pending_shield_waits_by_token(snapshot, now_epoch_secs());
        self.private_asset_presentation(snapshot)
            .rows
            .iter()
            .filter(|asset| should_show_pending_poi_amount(asset.pending_poi_total))
            .any(|asset| {
                asset
                    .token
                    .and_then(|token| waits.get(&token).copied())
                    .is_some_and(|wait| {
                        pending_shield_wait_matches_total(wait, asset.pending_poi_total)
                    })
            })
    }

    pub(super) fn render_private_pending_status_card(
        root: &Entity<Self>,
        summary: &PrivatePendingSummary,
    ) -> gpui::Div {
        let details_root = root.clone();
        ui::private_assets::private_pending_status(
            private_pending_summary_title(summary),
            private_pending_summary_detail(summary).map(Into::into),
            app_button("wallet-private-pending-details", "Details")
                .ghost()
                .xsmall()
                .compact()
                .on_click(move |_event, window, cx| {
                    details_root.update(cx, |root, cx| {
                        root.open_private_pending_status_dialog(window, cx);
                    });
                }),
        )
    }

    fn render_private_pending_status_dialog_content(
        root: &Entity<Self>,
        summary: &PrivatePendingSummary,
        content_width: Pixels,
    ) -> gpui::Div {
        let retry_available = summary.poi_refresh_session.is_some();
        let close_root = root.clone();
        let retry_root = root.clone();
        let retrying = summary.poi_refreshing;
        let recoverable = summary.recoverable_poi_outputs;
        let show_recovery = recoverable > 0;
        let workflow_retry_count = usize::try_from(
            summary
                .ppoi_workflow_status
                .needs_attention
                .saturating_add(summary.ppoi_workflow_status.awaiting_recovery),
        )
        .unwrap_or(usize::MAX);
        let retry_count = recoverable.saturating_add(workflow_retry_count);

        div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .when(!summary.blocked_shield_rows.is_empty(), |this| {
                this.child(private_blocked_shield_detail_section(
                    root,
                    &summary.blocked_shield_rows,
                ))
            })
            .when(summary.ppoi_workflow_status.has_outstanding(), |this| {
                this.child(private_ppoi_workflow_section(
                    summary.ppoi_workflow_status,
                    summary.poi_refreshing,
                ))
            })
            .child(ui::private_assets::private_pending_details(
                private_pending_presentation(summary)
                    .categories
                    .into_iter()
                    .map(|category| ui::private_assets::PrivatePendingCategory {
                        title: category.title.into(),
                        count: category.count.into(),
                        detail: category.detail.into(),
                        assets: category
                            .assets
                            .into_iter()
                            .map(|asset| ui::private_assets::PrivatePendingAmount {
                                label: asset.label.into(),
                                amount: asset.amount.into(),
                                shield_wait: asset.shield_wait.map(Into::into),
                            })
                            .collect(),
                    })
                    .collect(),
            ))
            .when(show_recovery, |this| {
                this.child(private_pending_retry_section(
                    recoverable,
                    &summary.recoverable_ppoi_states,
                ))
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("wallet-private-pending-dialog-close", "Close")
                            .outline()
                            .small()
                            .on_click(move |_event, window, cx| {
                                close_root.update(cx, |root, _cx| {
                                    root.private_pending_status_dialog_open = false;
                                });
                                window.close_dialog(cx);
                            }),
                    )
                    .when(retry_count > 0, |actions| {
                        actions.child(
                            app_button(
                                "wallet-private-pending-dialog-retry-poi",
                                retry_poi_label(retry_count, retrying),
                            )
                            .primary()
                            .small()
                            .disabled(!retry_available)
                            .on_click(move |_event, _window, cx| {
                                retry_root.update(cx, |root, cx| {
                                    Self::retry_poi_submissions(root.selected_chain_session(), cx);
                                });
                            }),
                        )
                    }),
            )
    }
}

pub(in crate::root) fn retry_poi_label(count: usize, retrying: bool) -> String {
    if retrying {
        "Queue PPOI retry".to_string()
    } else if count == 1 {
        "Retry".to_string()
    } else {
        format!("Retry ({count})")
    }
}

fn private_pending_status_empty(content_width: Pixels) -> gpui::Div {
    div()
        .w(content_width)
        .text_size(px(13.0))
        .line_height(px(19.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .child("Nothing needs attention right now.")
}

fn private_ppoi_workflow_section(status: WalletPpoiWorkflowStatus, refreshing: bool) -> gpui::Div {
    let count = usize::try_from(status.outstanding_count()).unwrap_or(usize::MAX);
    let color = if status.needs_attention > 0 || status.recovery_needs_attention > 0 {
        theme::DANGER
    } else {
        theme::WARNING
    };
    let title = ppoi_workflow_status_title(status, refreshing).unwrap_or("Outgoing proof recovery");
    let content = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(private_pending_section_header(
            title,
            count_label(count, "PPOI output"),
            color,
            RailgunActionIcon::Clock,
        ))
        .child(
            app_muted_text(ppoi_workflow_status_detail(status))
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal(),
        )
        .child(
            app_muted_text(
                "Outgoing proof recovery runs automatically and does not affect your displayed balance.",
            )
            .text_size(px(12.0))
            .line_height(px(17.0))
            .whitespace_normal(),
        );
    private_pending_section_card(color, content)
}

fn private_pending_section_card(accent_color: u32, content: gpui::Div) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_3()
        .p(px(10.0))
        .child(private_pending_section_accent(accent_color))
        .child(content.flex_1().min_w(px(0.0)))
}

fn private_pending_section_accent(accent_color: u32) -> gpui::Div {
    div()
        .w(px(3.0))
        .min_h(px(48.0))
        .h_full()
        .flex_none()
        .rounded_full()
        .bg(rgb_with_alpha(accent_color, 0.82))
}

fn private_pending_section_header(
    title: &'static str,
    count_label: String,
    accent_color: u32,
    icon: RailgunActionIcon,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).xsmall().text_color(rgb(accent_color)))
                .child(app_strong_text(title)),
        )
        .child(app_muted_text(count_label).text_size(px(12.0)))
}

fn private_blocked_shield_detail_section(
    root: &Entity<WalletRoot>,
    rows: &[UtxoDisplayRow],
) -> gpui::Div {
    let content = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(private_pending_section_header(
            "Blocked Shields",
            pending_output_count_label(rows.len()),
            theme::DANGER,
            RailgunActionIcon::Shield,
        ))
        .child(
            app_muted_text("These can't be spent privately. When the original account is known, you can refund them to it.")
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal(),
        )
        .children(
            rows.iter()
                .enumerate()
                .map(|(ix, row)| private_blocked_shield_row(root, ix, row)),
        );
    private_pending_section_card(theme::DANGER, content)
}

fn private_blocked_shield_row(
    root: &Entity<WalletRoot>,
    ix: usize,
    row: &UtxoDisplayRow,
) -> gpui::Div {
    let status = blocked_shield_status_label(row);
    let action_available = blocked_shield_refund_action_available(row);
    let resolving = blocked_shield_refund_origin_resolving(row);
    let action_label = blocked_shield_action_label(row);
    let action_row = row.clone();
    let action_root = root.clone();
    let mut action = app_button(
        SharedString::from(format!("wallet-private-blocked-shield-action-{ix}")),
        action_label,
    )
    .small();
    if row
        .blocked_shield_rescue
        .as_ref()
        .is_some_and(|rescue| rescue.eligible)
    {
        action = action.danger();
    } else {
        action = action.outline();
    }
    action = action.loading(resolving).disabled(resolving);
    if action_available {
        action = action.on_click(move |_event, window, cx| {
            let row = action_row.clone();
            action_root.update(cx, |root, cx| {
                root.begin_blocked_shield_refund(&row, window, cx);
            });
        });
    } else {
        action = action.disabled(true);
    }

    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .text_size(px(12.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_color(rgb(theme::TEXT))
                        .child(SharedString::from(format!("{} {}", row.amount, row.token))),
                )
                .child(
                    app_muted_text(format!("{} · {}", status, short_hash(&row.source_tx_hash)))
                        .text_size(px(12.0))
                        .whitespace_normal(),
                ),
        )
        .child(action)
}

fn blocked_shield_action_label(row: &UtxoDisplayRow) -> &'static str {
    if blocked_shield_refund_action_available(row) || blocked_shield_refund_origin_resolving(row) {
        "Refund"
    } else {
        "Unavailable"
    }
}

fn blocked_shield_status_label(row: &UtxoDisplayRow) -> String {
    let Some(rescue) = row.blocked_shield_rescue.as_ref() else {
        return "Refund unavailable".to_string();
    };
    if rescue.eligible {
        return "Refund available".to_string();
    }
    if let Some(reason) = &rescue.disabled_reason {
        return reason.clone();
    }
    if blocked_shield_refund_action_available(row) {
        return "Origin check needed".to_string();
    }
    "Refund unavailable".to_string()
}

fn private_pending_shield_wait_label(wait: Option<PrivatePendingShieldWait>) -> Option<String> {
    let wait = wait?;
    let display =
        shield_poi_wait_time_display(wait.latest_source_block_timestamp, now_epoch_secs())?;
    Some(if wait.has_delayed && wait.output_count == 1 {
        "Shield · taking longer than usual".to_string()
    } else if wait.has_delayed {
        format!(
            "{} Shields · some taking longer than usual",
            wait.output_count
        )
    } else if wait.output_count == 1 {
        format!("Shield · ready in {}", display.label)
    } else {
        format!(
            "{} Shields · ready in up to {}",
            wait.output_count, display.label
        )
    })
}

pub(in crate::root) fn private_pending_presentation(
    summary: &PrivatePendingSummary,
) -> wallet_ops::gateway::GatewayPrivatePending {
    use wallet_ops::gateway::{
        GatewayPrivatePending, GatewayPrivatePendingAmount, GatewayPrivatePendingCategory,
    };
    let mut pending = GatewayPrivatePending::default();
    pending.title = private_pending_summary_title(summary);
    pending.detail = private_pending_summary_detail(summary).map(str::to_owned);
    for (title, detail, assets, prefix) in [
        (
            "Pending incoming",
            "Waiting for the network to confirm.",
            &summary.pending_incoming_assets,
            "+",
        ),
        (
            "Pending outgoing",
            "Waiting for the network to confirm.",
            &summary.pending_outgoing_assets,
            "-",
        ),
        (
            "Not yet spendable",
            "These need Private Proof of Innocence (PPOI) verification before they can be spent. It usually completes on its own.",
            &summary.pending_poi_assets,
            "",
        ),
    ] {
        if assets.is_empty() {
            continue;
        }
        let mut category = GatewayPrivatePendingCategory::default();
        category.title = title.into();
        category.count = pending_asset_count_label(assets.len());
        category.detail = detail.into();
        category.assets = assets
            .iter()
            .map(|asset| {
                let mut row = GatewayPrivatePendingAmount::default();
                row.label.clone_from(&asset.label);
                row.amount = format!("{prefix}{}", asset.amount);
                row.shield_wait = private_pending_shield_wait_label(asset.shield_wait);
                row
            })
            .collect();
        pending.categories.push(category);
    }
    pending
}

fn private_pending_retry_section(recoverable: usize, states: &[UtxoPpoiState]) -> gpui::Div {
    let content = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(private_pending_section_header(
            "Retry PPOI",
            pending_output_count_label(recoverable),
            theme::DANGER,
            RailgunActionIcon::Clock,
        ))
        .child(
            app_muted_text("Submission is normally automatic. Retrying re-checks these and any related outputs from this wallet.")
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal(),
        )
        .children(states.iter().map(|state| {
            app_muted_text(crate::root::utxo::ppoi_state_detail(*state))
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal()
        }));
    private_pending_section_card(theme::DANGER, content)
}

fn pending_output_count_label(count: usize) -> String {
    if count == 1 {
        "1 output".to_string()
    } else {
        format!("{count} outputs")
    }
}

pub(super) fn pending_asset_count_label(count: usize) -> String {
    if count == 1 {
        "1 asset".to_string()
    } else {
        format!("{count} assets")
    }
}
