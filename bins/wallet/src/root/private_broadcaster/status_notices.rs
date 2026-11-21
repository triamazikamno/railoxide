#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::root) fn render_private_broadcaster_status_notice(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    result: &PublicBroadcasterResultKind,
) -> gpui::Div {
    let (title, detail, border) = match result {
        PublicBroadcasterResultKind::Submitted { .. } => (
            "Submitted via public broadcaster",
            "Open the broadcaster status dialog for the transaction details.",
            theme::SUCCESS,
        ),
        PublicBroadcasterResultKind::Failed { .. } => (
            "Public broadcaster failed",
            "Open the broadcaster status dialog for the returned error.",
            theme::DANGER,
        ),
        PublicBroadcasterResultKind::TimedOut => (
            "Public broadcaster timed out",
            "Check history before submitting again. A new submission is sent from a new identity.",
            theme::WARNING,
        ),
    };
    render_private_broadcaster_status_notice_box(root, key, kind, title, detail, border, None)
}

pub(in crate::root) fn render_private_self_broadcast_status_notice(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    result: &DesktopSelfBroadcastResult,
) -> gpui::Div {
    let (title, detail, border) = match result.tx.execution_status() {
        Some(true) => (
            "Self-broadcast confirmed",
            "Open the self-broadcast status dialog for transaction details.",
            theme::SUCCESS,
        ),
        Some(_) => (
            "Self-broadcast reverted",
            "Open the self-broadcast status dialog for receipt details.",
            theme::DANGER,
        ),
        None if result.tx.receipt().is_some() => (
            "Execution not confirmed",
            "The receipt alone does not confirm the execution. Stealth accounts keeps this operation, where Check balances updates its result.",
            theme::WARNING,
        ),
        None => (
            "Transaction inclusion unknown",
            progress::SELF_BROADCAST_INCLUSION_UNOBSERVED_MESSAGE,
            theme::WARNING,
        ),
    };
    render_private_broadcaster_status_notice_box(root, key, kind, title, detail, border, None)
}

pub(in crate::root) fn render_private_submission_active_status_notice(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    active: &PrivateBroadcasterClosedActiveProgress,
) -> gpui::Div {
    let title = match (active.flow, active.step.status) {
        (PrivateSubmissionProgressFlow::PublicBroadcaster, PublicActionStepStatus::Error) => {
            "Public broadcaster needs attention"
        }
        (PrivateSubmissionProgressFlow::SelfBroadcast, PublicActionStepStatus::Error) => {
            "Self-broadcast needs attention"
        }
        (PrivateSubmissionProgressFlow::PublicBroadcaster, _) => "Public broadcaster in progress",
        (PrivateSubmissionProgressFlow::SelfBroadcast, _) => "Self-broadcast in progress",
    };
    let border = if active.step.status == PublicActionStepStatus::Error {
        theme::DANGER
    } else {
        theme::INFO
    };
    let discard_generation_id =
        private_submission_discard_attempt_available(active).then_some(active.generation_id);
    render_private_broadcaster_status_notice_box(
        root,
        key,
        kind,
        title,
        private_submission_active_status_detail(active),
        border,
        discard_generation_id,
    )
}

fn private_submission_active_status_detail(
    active: &PrivateBroadcasterClosedActiveProgress,
) -> String {
    let stage = active.step.stage;
    let detail = match active.step.status {
        PublicActionStepStatus::Error => active.step.message.as_deref().map_or_else(
            || "This private submission step failed.".to_string(),
            private_submission_error_summary,
        ),
        _ => private_broadcaster_stage_detail(
            stage,
            active.step.status,
            active.requires_device_approval,
        )
        .to_string(),
    };
    format!(
        "{}: {detail}",
        private_broadcaster_stage_label(stage, active.requires_device_approval)
    )
}

fn private_submission_error_summary(message: &str) -> String {
    let normalized = message.to_ascii_lowercase();
    let signing_cancelled = normalized.contains("cancel")
        || normalized.contains("rejected")
        || normalized.contains("denied");
    if signing_cancelled {
        return "Signing was cancelled.".to_string();
    }
    message.to_string()
}

fn render_private_broadcaster_status_notice_box(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    border: u32,
    discard_generation_id: Option<u64>,
) -> gpui::Div {
    let view_root = root.clone();
    let discard_root = root;
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .p(px(10.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(border))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text(title))
                .child(app_muted_text(detail).whitespace_normal()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .justify_end()
                .gap_2()
                .child(
                    app_button(
                        delivery_element_id(key, kind, "view-broadcaster-progress"),
                        "View status",
                    )
                    .outline()
                    .small()
                    .on_click(move |_event, window, cx| {
                        view_root.update(cx, |root, cx| {
                            root.show_private_broadcaster_progress_dialog(window, cx);
                        });
                    }),
                )
                .children(discard_generation_id.map(|generation_id| {
                    app_button(
                        delivery_element_id(key, kind, "discard-attempt"),
                        "Discard attempt",
                    )
                    .danger()
                    .small()
                    .on_click(move |_event, _window, cx| {
                        discard_root.update(cx, |root, cx| {
                            root.discard_private_submission_attempt(kind, key, generation_id, cx);
                        });
                    })
                })),
        )
}
