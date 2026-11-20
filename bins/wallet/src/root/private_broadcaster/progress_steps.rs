#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn render_private_broadcaster_progress_footer(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
) -> gpui::Div {
    let action = progress_footer_action(
        private_broadcaster_progress_stop_available(progress),
        private_broadcaster_progress_is_terminal(progress),
    );
    if matches!(action, ProgressFooterAction::Stop) {
        use ui::private_submission::OperationControl;
        let control = if progress.sponsored_funding {
            OperationControl::StopRetries
        } else if public_broadcaster_waiting_can_stop(progress) && !progress.stop_available {
            OperationControl::StopWaiting
        } else {
            OperationControl::Stop
        };
        return ui::private_submission::operation_controls(
            delivery_element_id(progress.key, progress.kind, "progress-controls"),
            [control],
            move |_, _, cx| root.update(cx, WalletRoot::stop_private_broadcaster_progress),
        )
        .w_full()
        .justify_end();
    }
    let button_root = root;
    let (id_suffix, label) = match action {
        ProgressFooterAction::Stop => (
            "progress-stop",
            if public_broadcaster_waiting_can_stop(progress) && !progress.stop_available {
                "Stop waiting"
            } else if progress.sponsored_funding {
                "Stop retries"
            } else {
                "Stop"
            },
        ),
        ProgressFooterAction::Close => ("progress-close", "Close"),
    };
    let button = app_button(
        delivery_element_id(progress.key, progress.kind, id_suffix),
        label,
    )
    .small()
    .flex_none();
    let button = match action {
        ProgressFooterAction::Stop => button.danger().icon(Icon::new(RailgunActionIcon::Square)),
        ProgressFooterAction::Close => button.outline(),
    };
    div()
        .w_full()
        .flex()
        .justify_end()
        .pt(px(2.0))
        .child(button.on_click(move |_event, window, cx| {
            button_root.update(cx, |root, cx| match action {
                ProgressFooterAction::Stop => root.stop_private_broadcaster_progress(cx),
                ProgressFooterAction::Close => {
                    root.close_private_broadcaster_progress_dialog(window, cx);
                }
            });
        }))
}

/// `refusal_actions` is `Some(offer_another_broadcaster)` when a broadcaster refusal can be retried.
/// `input_pin_override` offers to continue a submission the form's note pin stopped.
pub(in crate::root) fn render_private_broadcaster_progress_stepper(
    root: &Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
    refusal_actions: Option<bool>,
    input_pin_override: bool,
) -> gpui::Div {
    render_submission_progress_stepper(progress.steps.iter().map(|step| {
        let detail = if step.status == PublicActionStepStatus::Error {
            step.message
                .as_deref()
                .unwrap_or("This broadcaster submission step failed.")
                .to_owned()
        } else {
            private_broadcaster_step_detail(progress, step, Instant::now())
        };
        SubmissionProgressStep {
            label: private_broadcaster_stage_label(step.stage, progress.requires_device_approval)
                .into(),
            detail,
            status: step.status,
            error_copy_id: format!(
                "wallet-private-broadcaster-{}-error-copy",
                private_broadcaster_stage_id(step.stage)
            )
            .into(),
            action: match refusal_actions {
                Some(another) if step.status == PublicActionStepStatus::Error => Some(
                    render_refused_broadcaster_step_action(root.clone(), progress, another),
                ),
                _ if input_pin_override && step.status == PublicActionStepStatus::Error => Some(
                    render_input_pin_conflict_step_action(root.clone(), progress),
                ),
                _ => render_self_broadcast_step_action(root.clone(), progress, step),
            },
        }
    }))
}

fn render_refused_broadcaster_step_action(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
    another_broadcaster: bool,
) -> gpui::AnyElement {
    let key = progress.key;
    let kind = progress.kind;
    let generation_id = progress.generation_id;
    let pending = progress.refusal_retry_pending;
    let custom_fee_root = root.clone();
    let buttons = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            app_button(
                delivery_element_id(key, kind, "progress-retry-custom-fee"),
                "Retry with custom fee…",
            )
            .small()
            .outline()
            .disabled(pending)
            .on_click(move |_event, window, cx| {
                custom_fee_root.update(cx, |root, cx| {
                    root.retry_public_broadcaster_with_custom_fee(
                        kind,
                        key,
                        generation_id,
                        window,
                        cx,
                    );
                });
            }),
        )
        .when(another_broadcaster, |this| {
            this.child(
                app_button(
                    delivery_element_id(key, kind, "progress-try-another-broadcaster"),
                    "Try another broadcaster",
                )
                .small()
                .outline()
                .disabled(pending)
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        root.try_another_public_broadcaster(kind, key, generation_id, window, cx);
                    });
                }),
            )
        });
    div()
        .pt(px(4.0))
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(buttons)
        .when(pending, |this| {
            this.child(app_muted_text("Getting a new fee estimate…").whitespace_normal())
        })
        .when_some(progress.refusal_retry_error.as_deref(), |this, error| {
            this.child(
                app_muted_text(format!("Retry failed: {error}"))
                    .text_color(rgb(theme::DANGER))
                    .whitespace_normal(),
            )
        })
        .into_any_element()
}

fn render_input_pin_conflict_step_action(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
) -> gpui::AnyElement {
    let key = progress.key;
    let kind = progress.kind;
    let generation_id = progress.generation_id;
    div()
        .pt(px(4.0))
        .child(
            app_button(
                delivery_element_id(key, kind, "progress-continue-despite-pin"),
                "Continue anyway…",
            )
            .small()
            .outline()
            .on_click(move |_event, window, cx| {
                root.update(cx, |root, cx| {
                    root.continue_despite_input_pin(kind, key, generation_id, window, cx);
                });
            }),
        )
        .into_any_element()
}

fn render_self_broadcast_step_action(
    root: Entity<WalletRoot>,
    progress: &PrivateBroadcasterProgressState,
    step: &PrivateBroadcasterProgressStepState,
) -> Option<gpui::AnyElement> {
    if progress.flow != PrivateSubmissionProgressFlow::SelfBroadcast
        || progress.self_broadcast_command_tx.is_none()
        || progress.stopped
        || progress.error.is_some()
        || progress.self_broadcast_result.is_some()
    {
        return None;
    }
    let retry_kind = self_broadcast_step_retry_kind(progress, step)?;
    let label = match retry_kind {
        SelfBroadcastGasRetryKind::RetryStep => "Retry step",
        SelfBroadcastGasRetryKind::RetryEstimate => "Retry with custom gas",
        SelfBroadcastGasRetryKind::SpeedUp => "Speed up transaction",
    };
    let key = progress.key;
    let kind = progress.kind;
    let generation_id = progress.generation_id;
    let mut action = div()
        .pt(px(4.0))
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(
            app_button(
                delivery_element_id(key, kind, private_broadcaster_retry_button_id(retry_kind)),
                label,
            )
            .small()
            .outline()
            .on_click(move |_event, window, cx| {
                root.update(cx, |root, cx| {
                    if retry_kind == SelfBroadcastGasRetryKind::RetryStep {
                        root.submit_self_broadcast_step_retry(kind, key, generation_id, cx);
                    } else {
                        root.open_self_broadcast_gas_retry_dialog(
                            kind,
                            key,
                            generation_id,
                            retry_kind,
                            window,
                            cx,
                        );
                    }
                });
            }),
        );
    if let Some(error) = progress.self_broadcast_action_error.as_deref() {
        action = action.child(
            app_muted_text(format!("Last retry failed: {error}"))
                .text_color(rgb(theme::DANGER))
                .whitespace_normal(),
        );
    }
    Some(action.into_any_element())
}

/// The route card of the root's private submission progress, which the progress dialog's route
/// chip shows on hover.
pub(super) fn private_broadcaster_root_route_card(
    root: &WalletRoot,
    _cx: &App,
) -> Option<SubmissionRouteCard> {
    root.private_broadcaster_progress
        .as_ref()
        .and_then(private_broadcaster_route_card)
}
