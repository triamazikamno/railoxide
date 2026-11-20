#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::root) fn render_private_broadcaster_progress_context(
    progress: &PrivateBroadcasterProgressState,
    context: &PrivateBroadcasterProgressContext<'_>,
    mut broadcaster_action: Option<AnyElement>,
) -> gpui::Div {
    let rows = private_broadcaster_progress_context_rows(progress, context)
        .into_iter()
        .map(|row| {
            let action = if row.label == "Broadcaster" {
                broadcaster_action.take()
            } else {
                None
            };
            (row, action)
        })
        .collect();
    ui::private_submission::transaction_context_with_actions(rows)
}

pub(in crate::root) fn private_broadcaster_progress_context_rows(
    progress: &PrivateBroadcasterProgressState,
    context: &PrivateBroadcasterProgressContext<'_>,
) -> Vec<ui::private_action::DisplayRow> {
    use ui::private_action::DisplayRow;
    let display = &context.display;
    let mut rows = [
        (
            "Broadcaster",
            broadcaster_candidate_label(display.broadcaster),
        ),
        (
            "Recipient",
            spend_authorization_recipient_display(progress.recipient.as_ref()),
        ),
        (
            "Entered amount",
            display.action_amount(display.entered_amount),
        ),
    ]
    .into_iter()
    .map(|(label, value)| DisplayRow {
        label: label.into(),
        value,
        suffix: None,
    })
    .collect::<Vec<_>>();
    rows.extend(display.outcome_rows(context.anchor_cache));
    rows.push(DisplayRow {
        label: "Network gas".into(),
        value: display.gas_value(),
        suffix: None,
    });
    rows
}

pub(in crate::root) fn private_broadcaster_context_row(
    label: impl Into<SharedString>,
    value: String,
) -> gpui::Div {
    private_broadcaster_context_row_with_action(label, value, None)
}

pub(in crate::root) fn private_broadcaster_context_row_with_action(
    label: impl Into<SharedString>,
    value: String,
    action: Option<AnyElement>,
) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .child(app_muted_text(label).flex_none())
        .child(strong_wrapping_value(value, None).children(action))
}

fn strong_wrapping_value(value: String, suffix: Option<String>) -> gpui::Div {
    let row = div()
        .min_w(px(0.0))
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_1();

    if let Some(suffix) = suffix {
        return row
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
            );
    }

    row.child(
        app_strong_text(value)
            .min_w(px(0.0))
            .text_align(gpui::TextAlign::Right)
            .whitespace_normal(),
    )
}
