#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn public_broadcaster_progress_address(
    progress: &PrivateBroadcasterProgressState,
) -> Option<&str> {
    progress.result.as_ref().map_or_else(
        || {
            progress
                .estimate
                .as_ref()
                .map(|estimate| estimate.broadcaster.railgun_address.as_ref())
        },
        |result| Some(result.broadcaster.railgun_address.as_ref()),
    )
}

pub(super) fn render_pending_public_broadcaster_progress_context(
    progress: &PrivateBroadcasterProgressState,
) -> gpui::Div {
    app_panel(theme::SURFACE_ELEVATED, theme::BORDER)
        .child(app_strong_text("Transaction context"))
        .child(private_broadcaster_context_row(
            "Broadcaster",
            "Selecting during preparation".to_string(),
        ))
        .child(private_broadcaster_context_row(
            "Recipient",
            spend_authorization_recipient_display(progress.recipient.as_ref()),
        ))
        .child(cost_estimate_detail_text(
            "Fee and gas cost will appear after the transaction fee is calculated.",
        ))
}

fn private_broadcaster_copy_action(id: String, value: String, tooltip: &'static str) -> AnyElement {
    div()
        .id(SharedString::from(format!("{id}-action")))
        .flex_none()
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .child(clipboard_with_toast(SharedString::from(id), value))
        .into_any_element()
}

pub(super) fn self_broadcast_progress_context_rows(
    progress: &PrivateBroadcasterProgressState,
) -> Vec<ui::private_action::DisplayRow> {
    let mut values = vec![
        (
            if progress.sponsored_funding {
                "Transaction signer"
            } else {
                "Gas payer"
            },
            progress
                .gas_payer
                .as_deref()
                .unwrap_or("Selected Public account")
                .to_owned(),
        ),
        ("Recipient", progress.recipient.to_string()),
    ];
    if let Some(result) = &progress.self_broadcast_result {
        values.extend(self_broadcast_composite_output_rows(progress, result));
        values.extend([
            (
                "Max fee",
                format!("{} gwei", format_gwei(result.max_fee_per_gas)),
            ),
            (
                "Max tip",
                format!("{} gwei", format_gwei(result.max_priority_fee_per_gas)),
            ),
            (
                "Estimated gas cost",
                format_native_token_amount_for_display(
                    result.chain_id,
                    result.estimated_native_gas_cost,
                ),
            ),
            (
                "Receipt",
                match result.tx.receipt() {
                    Some(receipt) if receipt.status => "confirmed",
                    Some(_) => "reverted",
                    None => "inclusion unknown",
                }
                .into(),
            ),
        ]);
        if let Some(receipt) = result.tx.receipt() {
            values.push(("Block", receipt.block_number.to_string()));
        }
        if matches!(
            result.tx,
            wallet_ops::SelfBroadcastTxOutcome::ExecutorReceipt { .. }
        ) {
            // The receipt tells a revert at the time. It is recorded nowhere.
            let execution = match result.tx.execution_status() {
                Some(true) => "confirmed",
                Some(false) => "reverted",
                None => "not confirmed",
            };
            values.push(("Execution", execution.into()));
        }
    } else if let Some(output) = &progress.recipient_output {
        values.push(("Recipient receives", output.to_string()));
    }
    if let Some(SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt)) =
        &progress.sponsored_self_broadcast_outcome
    {
        values.extend([
            (
                "Receipt",
                if receipt.status {
                    "confirmed"
                } else {
                    "reverted"
                }
                .into(),
            ),
            ("Transaction hash", receipt.tx_hash.clone()),
            ("Block", receipt.block_number.to_string()),
        ]);
    }
    values
        .into_iter()
        .map(|(label, value)| ui::private_action::DisplayRow {
            label: label.into(),
            value,
            suffix: None,
        })
        .collect()
}

pub(super) fn render_self_broadcast_progress_context(
    progress: &PrivateBroadcasterProgressState,
) -> gpui::Div {
    let rows = self_broadcast_progress_context_rows(progress)
        .into_iter()
        .map(|mut row| {
            let action = match row.label.as_str() {
                "Recipient" => {
                    let action = private_broadcaster_copy_action(
                        format!(
                            "private-self-broadcast-recipient-copy-{}",
                            progress.generation_id
                        ),
                        row.value.clone(),
                        "Copy recipient",
                    );
                    row.value = spend_authorization_recipient_display(&row.value);
                    Some(action)
                }
                "Transaction hash" => {
                    let action = private_broadcaster_copy_action(
                        format!(
                            "private-self-broadcast-tx-hash-copy-{}",
                            progress.generation_id
                        ),
                        row.value.clone(),
                        "Copy transaction hash",
                    );
                    row.value = short_hash(&row.value);
                    Some(action)
                }
                _ => None,
            };
            (row, action)
        })
        .collect();
    ui::private_submission::transaction_context_with_actions(rows)
        .p(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
}

pub(in crate::root) fn self_broadcast_composite_output_rows(
    progress: &PrivateBroadcasterProgressState,
    result: &DesktopSelfBroadcastResult,
) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if let Some(output) = progress.recipient_output.as_ref() {
        let output = result.native_top_up.as_ref().map_or_else(
            || output.to_string(),
            |top_up| {
                format_recipient_amount_with_native_top_up(
                    output.as_ref(),
                    result.chain_id,
                    top_up.native_amount,
                )
            },
        );
        rows.push(("Recipient receives", output));
    } else if let Some(top_up) = result.native_top_up.as_ref() {
        rows.push((
            "Recipient receives",
            format!(
                "{} (gas top-up)",
                format_native_token_amount_for_display(result.chain_id, top_up.native_amount)
            ),
        ));
    }
    rows
}
