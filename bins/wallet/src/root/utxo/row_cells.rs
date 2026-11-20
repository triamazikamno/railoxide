#[allow(clippy::wildcard_imports)]
use super::*;

/// A UTXO table cell's content, centred in the row at the wallet's body text size. The table
/// sizes its own cell text for `Size::Large`, so the cell has to set it again.
pub(super) fn utxo_cell(content: impl IntoElement) -> gpui::Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .text_size(APP_TEXT_SIZE)
        .line_height(relative(APP_TEXT_LINE_HEIGHT))
        .child(content)
}

pub(in crate::root) fn local_pending_spent_summary(count: usize) -> String {
    let noun = if count == 1 { "UTXO" } else { "UTXOs" };
    format!("Locally locked pending submission: {count} {noun}")
}

/// Local pending-submission locks with their two-step clear, shared by the UTXOs tab
/// and the locked notes dialog. `id` prefixes the button IDs.
pub(in crate::root) fn local_pending_spent_card(
    id: &str,
    summary: impl Into<SharedString>,
    confirming: bool,
    begin: impl Fn(&mut Window, &mut App) + 'static,
    cancel: impl Fn(&mut Window, &mut App) + 'static,
    confirm: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(if confirming {
            theme::DANGER
        } else {
            theme::BORDER
        }))
        .bg(if confirming {
            rgb_with_alpha(theme::DANGER, 0.08)
        } else {
            rgb(theme::SURFACE)
        })
        .p(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(app_muted_text(summary).line_height(px(18.0)))
                .child(div().flex_1())
                .when(!confirming, |this| {
                    this.child(
                        app_button(
                            SharedString::from(format!("{id}-clear-local-pending-spent")),
                            "Clear local locks",
                        )
                        .outline()
                        .small()
                        .danger()
                        .on_click(move |_event, window, cx| begin(window, cx)),
                    )
                }),
        )
        .when(confirming, |this| {
            this.child(
                div()
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .text_color(rgb(theme::DANGER))
                    .child("This only clears local submitted-transaction locks. If the original transaction later confirms, these UTXOs may fail simulation or become spent again."),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button(
                            SharedString::from(format!("{id}-cancel-clear-local-pending-spent")),
                            "Cancel",
                        )
                        .outline()
                        .small()
                        .on_click(move |_event, window, cx| cancel(window, cx)),
                    )
                    .child(
                        app_button(
                            SharedString::from(format!("{id}-confirm-clear-local-pending-spent")),
                            "Clear local locks",
                        )
                        .small()
                        .danger()
                        .on_click(move |_event, window, cx| confirm(window, cx)),
                    ),
            )
        })
}

pub(super) fn poi_status_indicator(row: &UtxoDisplayRow, row_ix: usize) -> gpui::AnyElement {
    if is_shield_blocked_poi_status(&row.poi_status) {
        return div()
            .id(SharedString::from(format!(
                "wallet-poi-shield-blocked-{row_ix}"
            )))
            .flex_none()
            .tooltip(|window, cx| Tooltip::new("ShieldBlocked").build(window, cx))
            .child(
                Icon::empty()
                    .path(icons::ban_icon_path())
                    .small()
                    .text_color(rgb(theme::DANGER)),
            )
            .into_any_element();
    }
    let tag = if row.poi_spendable {
        Tag::success()
    } else {
        Tag::warning()
    };
    let (label, detail, show_clock) = match shield_poi_wait_display(row, now_epoch_secs()) {
        Some(display) => (display.label, display.detail, true),
        None => (
            row.poi_status.clone(),
            ppoi_row_state_detail_with_submission(
                row.ppoi_state,
                row.is_spent,
                row.ppoi_last_submission_at,
            ),
            false,
        ),
    };
    div()
        .id(SharedString::from(format!("wallet-poi-status-{row_ix}")))
        .tooltip(move |window, cx| Tooltip::new(detail).build(window, cx))
        .child(
            tag.small().outline().child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(show_clock, |this| {
                        this.child(Icon::new(RailgunActionIcon::Clock).xsmall())
                    })
                    .child(SharedString::from(label)),
            ),
        )
        .into_any_element()
}

pub(in crate::root) struct ShieldPoiWaitDisplay {
    pub(in crate::root) label: String,
    pub(in crate::root) detail: &'static str,
    pub(in crate::root) delayed: bool,
}

pub(in crate::root) fn shield_poi_wait_display(
    row: &UtxoDisplayRow,
    now_epoch_secs: u64,
) -> Option<ShieldPoiWaitDisplay> {
    if row.activity_classification != "Shield"
        || row.poi_spendable
        || row.is_spent
        || row.pending_new
        || row.pending_spent
        || row.local_pending_spent
        || row.source_block_timestamp == 0
        || !matches!(
            row.ppoi_state,
            UtxoPpoiState::Missing | UtxoPpoiState::Unknown | UtxoPpoiState::ProofSubmitted
        )
    {
        return None;
    }

    shield_poi_wait_time_display(row.source_block_timestamp, now_epoch_secs)
}

pub(in crate::root) fn shield_poi_wait_time_display(
    source_block_timestamp: u64,
    now_epoch_secs: u64,
) -> Option<ShieldPoiWaitDisplay> {
    let deadline = source_block_timestamp.checked_add(SECONDS_PER_HOUR)?;
    let remaining_secs = deadline
        .saturating_sub(now_epoch_secs)
        .min(SECONDS_PER_HOUR);
    if remaining_secs == 0 {
        return Some(ShieldPoiWaitDisplay {
            label: "Taking longer than usual".to_string(),
            detail: "Shield PPOI verification is taking longer than usual. Timing may vary.",
            delayed: true,
        });
    }

    let remaining_minutes = remaining_secs.div_ceil(SECONDS_PER_MINUTE);
    let label = if remaining_minutes >= 60 {
        "~1h".to_string()
    } else {
        format!("~{remaining_minutes}m")
    };
    Some(ShieldPoiWaitDisplay {
        label,
        detail: "Estimated time remaining for Shield PPOI verification. This typically takes about one hour; timing may vary.",
        delayed: false,
    })
}

pub(in crate::root) fn pending_finality_display(
    row: &UtxoDisplayRow,
    context: UtxoFinalityContext,
) -> Option<(String, String)> {
    let (block_number, progress_subject, pending_object, indexing_detail) = if row.pending_spent {
        (
            row.spent_block_number?,
            "spend",
            "spend",
            "The spend transaction reached the chain safe head. Waiting for the wallet snapshot to mark this output spent.",
        )
    } else if row.pending_new {
        (
            row.source_block_number,
            "receive",
            "output",
            "The source transaction reached the chain safe head. Waiting for the wallet snapshot to include this output.",
        )
    } else {
        return None;
    };
    let (elapsed, finality_depth) = match context.progress(block_number)? {
        BlockFinalityProgress::Safe => {
            return Some(("Indexing".to_string(), indexing_detail.to_string()));
        }
        BlockFinalityProgress::Confirming { elapsed, depth } => (elapsed, depth),
    };
    Some((
        format!("{elapsed}/{finality_depth} blocks"),
        format!(
            "Pending {progress_subject}: {elapsed} of {finality_depth} finality blocks elapsed. This {pending_object} remains pending until it reaches the chain safe head."
        ),
    ))
}

pub(super) fn ppoi_retry_action(
    row: &UtxoDisplayRow,
    row_ix: usize,
    root: WeakEntity<WalletRoot>,
    refreshing: bool,
    session_available: bool,
) -> gpui::AnyElement {
    let tooltip = ppoi_retry_tooltip(row.ppoi_state);
    div()
        .child(
            app_button_base(SharedString::from(format!("wallet-retry-poi-{row_ix}")))
                .xsmall()
                .disabled(!session_available)
                .tooltip(tooltip)
                .child(ppoi_row_retry_label(refreshing))
                .on_click(move |_event, _window, cx| {
                    cx.stop_propagation();
                    let _ = root.update(cx, |root, cx| {
                        WalletRoot::retry_poi_submissions(root.selected_chain_session(), cx);
                    });
                }),
        )
        .into_any_element()
}

pub(super) fn source_tx_cell(
    row: &UtxoDisplayRow,
    row_ix: usize,
    tx_hash: &str,
    tx_search_input: Entity<InputState>,
) -> gpui::AnyElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(activity_classification_icon(row, row_ix))
        .child(tx_hash_cell(
            row,
            row_ix,
            "source",
            tx_hash,
            rgb(theme::TEAL),
            tx_search_input,
        ))
        .into_any_element()
}

fn activity_classification_icon(row: &UtxoDisplayRow, row_ix: usize) -> gpui::AnyElement {
    let (path, color, label) = activity_classification_icon_style(&row.activity_classification);
    div()
        .id(SharedString::from(format!(
            "wallet-source-tx-classification-{row_ix}"
        )))
        .flex_none()
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .child(Icon::empty().path(path).small().text_color(rgb(color)))
        .into_any_element()
}

pub(in crate::root) fn activity_classification_icon_style(
    classification: &str,
) -> (&'static str, u32, &'static str) {
    match classification {
        "Shield" => (icons::shield_plus_icon_path(), theme::SUCCESS, "Shield"),
        "BlockedShield" | "Blocked Shield" => (
            icons::shield_alert_icon_path(),
            theme::DANGER,
            "Blocked Shield",
        ),
        _ => (
            icons::shield_check_icon_path(),
            theme::TEXT,
            "Private Output",
        ),
    }
}

pub(super) fn tx_hash_cell(
    row: &UtxoDisplayRow,
    row_ix: usize,
    kind: &'static str,
    tx_hash: &str,
    color: gpui::Rgba,
    tx_search_input: Entity<InputState>,
) -> gpui::AnyElement {
    let display_hash = short_hash(tx_hash);
    let search_hash = tx_hash.to_string();
    let group = SharedString::from(format!("wallet-{kind}-tx-group-{row_ix}"));

    div()
        .group(group.clone())
        .id(SharedString::from(format!("wallet-{kind}-tx-{row_ix}")))
        .flex()
        .items_center()
        .gap_1()
        .child(
            div()
                .id(SharedString::from(format!(
                    "wallet-{kind}-tx-copy-{row_ix}"
                )))
                .flex_none()
                .font_family(APP_MONO_FONT_FAMILY)
                .text_size(px(12.0))
                .text_color(utxo_cell_text_color(row, color))
                .child(SharedString::from(display_hash)),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "wallet-{kind}-tx-actions-{row_ix}"
                )))
                .group(group.clone())
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .opacity(0.0)
                .group_hover(group, |this| this.opacity(1.0))
                .hover(|this| this.opacity(1.0))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "wallet-{kind}-tx-copy-action-{row_ix}"
                        )))
                        .tooltip(|window, cx| {
                            Tooltip::new("Copy transaction hash").build(window, cx)
                        })
                        .child(clipboard_with_toast(
                            SharedString::from(format!("wallet-{kind}-tx-clipboard-{row_ix}")),
                            tx_hash.to_string(),
                        )),
                )
                .child(
                    app_button_base(SharedString::from(format!(
                        "wallet-{kind}-tx-search-{row_ix}"
                    )))
                    .ghost()
                    .xsmall()
                    .accessibility_label("Filter by this transaction")
                    .tooltip("Filter by this transaction")
                    .icon(IconName::Search)
                    .on_click(move |_event, window, cx| {
                        tx_search_input.update(cx, |input, cx| {
                            input.replace_all(search_hash.clone(), window, cx);
                        });
                    }),
                ),
        )
        .into_any_element()
}

pub(super) fn blocked_shield_refund_action(
    row: &UtxoDisplayRow,
    row_ix: usize,
    root: WeakEntity<WalletRoot>,
) -> gpui::AnyElement {
    let Some(rescue) = row.blocked_shield_rescue.as_ref() else {
        return div().into_any_element();
    };
    let mut button = app_button_base(SharedString::from(format!(
        "wallet-blocked-shield-refund-{row_ix}"
    )))
    .xsmall()
    .danger()
    .loading(blocked_shield_refund_origin_resolving(row))
    .child("Refund");
    if rescue.eligible || can_start_blocked_shield_origin_resolution(row, rescue) {
        let row = row.clone();
        button = button.on_click(move |_event, window, cx| {
            cx.stop_propagation();
            let row = row.clone();
            let _ = root.update(cx, |root, cx| {
                root.begin_blocked_shield_refund(&row, window, cx);
            });
        });
        if !rescue.eligible {
            button = button.tooltip(
                rescue
                    .disabled_reason
                    .clone()
                    .unwrap_or_else(|| "Check source transaction origin before refund".to_owned()),
            );
        }
    } else {
        let reason = rescue
            .disabled_reason
            .clone()
            .unwrap_or_else(|| "Blocked Shield refund is unavailable.".to_string());
        button = button.disabled(true).tooltip(reason);
    }
    div().child(button).into_any_element()
}

pub(super) fn blocked_shield_refund_authorization_summary(
    row: &UtxoDisplayRow,
    rescue: &BlockedShieldRescueInfo,
    origin_address: &str,
    device_only: bool,
) -> SpendAuthorizationSummary {
    let gas_payer = rescue
        .public_account_label
        .as_ref()
        .map_or_else(|| origin_address.to_string(), std::clone::Clone::clone);
    SpendAuthorizationSummary::new(
        "Blocked Shield refund",
        if device_only {
            "Approve this refund on your hardware wallet."
        } else {
            "Enter your vault password to authorize this refund."
        },
        vec![
            SpendAuthorizationSummaryRow::new("Amount", format!("{} {}", row.amount, row.token))
                .with_icon(row.token_icon_path.clone()),
            SpendAuthorizationSummaryRow::new("Recipient", origin_address.to_string())
                .with_shortened_copyable(),
            SpendAuthorizationSummaryRow::new("Delivery", "Self-broadcast"),
            SpendAuthorizationSummaryRow::new("Source transaction", row.source_tx_hash.clone()),
            SpendAuthorizationSummaryRow::new("Origin gas payer", gas_payer),
        ],
    )
}

pub(in crate::root) fn should_show_blocked_shield_refund_action(row: &UtxoDisplayRow) -> bool {
    is_shield_blocked_poi_status(&row.poi_status) && row.blocked_shield_rescue.is_some()
}

pub(in crate::root) fn blocked_shield_refund_action_available(row: &UtxoDisplayRow) -> bool {
    let Some(rescue) = row.blocked_shield_rescue.as_ref() else {
        return false;
    };
    rescue.eligible || can_start_blocked_shield_origin_resolution(row, rescue)
}

pub(in crate::root) fn blocked_shield_refund_origin_resolving(row: &UtxoDisplayRow) -> bool {
    row.blocked_shield_rescue.as_ref().is_some_and(|rescue| {
        rescue.disabled_reason.as_deref() == Some(BLOCKED_SHIELD_RESCUE_RESOLVING_REASON)
    })
}
