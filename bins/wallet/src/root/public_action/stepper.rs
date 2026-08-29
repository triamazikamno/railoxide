use super::*;

pub(in crate::root) fn render_public_action_stepper(
    root: &Entity<WalletRoot>,
    steps: &[PublicActionStepState],
    expanded_error_steps: &BTreeSet<PublicActionProgressStep>,
    asset_label: &str,
    requires_device_approval: bool,
    command_available: bool,
    current_gas_fee: Option<(u128, u128)>,
    fees_authorized: bool,
    action_error: Option<&str>,
    fee_authorization_review: Option<&PublicActionFeeAuthorizationReview>,
    generation: u64,
) -> gpui::Div {
    let mut stepper = app_stepper_container();
    let last_index = steps.len().saturating_sub(1);
    for (index, step) in steps.iter().enumerate() {
        stepper = stepper.child(render_public_action_step(
            root,
            step,
            index == last_index,
            expanded_error_steps.contains(&step.step),
            asset_label,
            requires_device_approval,
            command_available,
            current_gas_fee,
            fees_authorized,
            action_error,
            fee_authorization_review,
            generation,
        ));
    }
    stepper
}

pub(in crate::root) fn render_public_action_step(
    root: &Entity<WalletRoot>,
    step: &PublicActionStepState,
    is_last: bool,
    error_details_open: bool,
    asset_label: &str,
    requires_device_approval: bool,
    command_available: bool,
    current_gas_fee: Option<(u128, u128)>,
    fees_authorized: bool,
    action_error: Option<&str>,
    fee_authorization_review: Option<&PublicActionFeeAuthorizationReview>,
    generation: u64,
) -> gpui::Div {
    let color = public_action_step_color(step.status);
    let title = public_action_step_label(step.step);
    let detail = public_action_step_detail_for_context(
        step.step,
        step.status,
        requires_device_approval,
        step.tx_hash.is_some(),
    );
    let mut body = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .pb(if is_last { px(0.0) } else { px(12.0) })
        .child(
            app_strong_text(title)
                .text_color(rgb(color))
                .line_height(gpui::relative(1.0)),
        );
    if step.status == PublicActionStepStatus::Error {
        body = body.child(render_public_action_step_error(
            root.clone(),
            step,
            asset_label,
            error_details_open,
        ));
    } else {
        let detail = fee_authorization_review
            .filter(|review| review.step == step.step)
            .map_or_else(|| detail.to_string(), |review| review.message.to_string());
        body = body.child(
            app_muted_text(detail)
                .text_color(rgb(color))
                .line_height(gpui::relative(1.0)),
        );
    }
    body = body.children(
        step.interval
            .as_ref()
            .map(render_public_action_step_interval),
    );
    body = body.children(
        step.tx_hash
            .as_ref()
            .map(|tx_hash| render_public_action_step_hash(step.step, tx_hash.as_ref())),
    );
    if let Some(action) = render_public_action_step_action(
        root.clone(),
        step,
        command_available,
        current_gas_fee,
        fees_authorized,
        action_error,
        fee_authorization_review,
        generation,
    ) {
        body = body.child(action);
    }

    app_step_row(
        render_public_action_step_marker(step.status, color),
        body,
        is_last,
        color,
        px(32.0),
        None,
    )
}

fn render_public_action_step_interval(interval: &PublicActionStepInterval) -> gpui::Div {
    div().flex().items_center().gap_1().child(
        app_muted_text(format!("Interval {}–{}", interval.start, interval.end))
            .font_family(APP_MONO_FONT_FAMILY)
            .line_height(gpui::relative(1.0)),
    )
}

pub(in crate::root) fn render_public_action_step_action(
    root: Entity<WalletRoot>,
    step: &PublicActionStepState,
    command_available: bool,
    current_gas_fee: Option<(u128, u128)>,
    fees_authorized: bool,
    action_error: Option<&str>,
    fee_authorization_review: Option<&PublicActionFeeAuthorizationReview>,
    generation: u64,
) -> Option<gpui::AnyElement> {
    if !command_available {
        return None;
    }
    let retry_kind = if fee_authorization_review.is_some_and(|review| review.step == step.step) {
        PublicActionGasRetryKind::FeeAuthorization
    } else {
        match step.status {
            PublicActionStepStatus::Error => public_action_error_retry_kind(step),
            PublicActionStepStatus::Pending
                if step.tx_hash.is_some() && current_gas_fee.is_some() =>
            {
                PublicActionGasRetryKind::SpeedUp
            }
            _ => return None,
        }
    };
    if fees_authorized
        && retry_kind != PublicActionGasRetryKind::RetryStep
        && retry_kind != PublicActionGasRetryKind::FeeAuthorization
    {
        return None;
    }
    let label = match retry_kind {
        PublicActionGasRetryKind::RetryStep => "Retry step",
        PublicActionGasRetryKind::RetryEstimate => "Retry with custom gas",
        PublicActionGasRetryKind::SpeedUp => "Speed up transaction",
        PublicActionGasRetryKind::FeeAuthorization => "Review updated gas fee",
    };
    let mut action = div()
        .pt(px(4.0))
        .flex()
        .flex_col()
        .items_start()
        .gap_1()
        .child(
            app_button(public_action_retry_button_id(step.step, retry_kind), label)
                .small()
                .outline()
                .on_click(move |_event, window, cx| {
                    root.update(cx, |root, cx| {
                        if retry_kind == PublicActionGasRetryKind::RetryStep {
                            root.submit_public_action_step_retry(generation, cx);
                        } else {
                            root.open_public_action_gas_retry_dialog(
                                generation, retry_kind, window, cx,
                            );
                        }
                    });
                }),
        );
    if let Some(error) = action_error {
        action = action.child(
            app_muted_text(format!("Last retry failed: {error}"))
                .text_color(rgb(theme::DANGER))
                .whitespace_normal(),
        );
    }
    Some(action.into_any_element())
}

pub(in crate::root) fn render_public_action_step_error(
    root: Entity<WalletRoot>,
    step: &PublicActionStepState,
    asset_label: &str,
    details_open: bool,
) -> gpui::Div {
    let summary = public_action_error_summary(step.step, step.message.as_deref(), asset_label);
    let details = public_action_error_details(&summary, step.message.as_deref());
    let copy_value =
        public_action_error_copy_value(step.step, asset_label, &summary, details.as_deref());
    let copy_id = SharedString::from(format!(
        "wallet-public-action-{}-error-copy",
        public_action_step_id(step.step),
    ));
    let mut error = div().flex().flex_col().gap_1().child(
        div()
            .flex()
            .items_start()
            .gap_1()
            .min_w(px(0.0))
            .child(
                app_muted_text(summary)
                    .flex_1()
                    .min_w(px(0.0))
                    .whitespace_normal()
                    .text_color(rgb(theme::DANGER))
                    .line_height(gpui::relative(1.0)),
            )
            .child(clipboard_with_toast(copy_id, copy_value)),
    );

    if let Some(details) = details {
        let step_kind = step.step;
        let toggle_root = root;
        let toggle_id = SharedString::from(format!(
            "wallet-public-action-{}-error-details-toggle",
            public_action_step_id(step_kind),
        ));
        error = error.child(
            Collapsible::new()
                .open(details_open)
                .child(
                    div()
                        .id(toggle_id)
                        .flex()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .on_click(move |_event, _window, cx| {
                            toggle_root.update(cx, |root, cx| {
                                root.set_public_action_error_details_open(
                                    step_kind,
                                    !details_open,
                                    cx,
                                );
                            });
                        })
                        .child(app_muted_text(if details_open {
                            "Hide details"
                        } else {
                            "Details"
                        }))
                        .child(
                            Icon::new(if details_open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .xsmall()
                            .text_color(rgb(theme::TEXT_MUTED)),
                        ),
                )
                .content(
                    div().pt(px(2.0)).min_w(px(0.0)).child(
                        app_muted_text(details)
                            .font_family(APP_MONO_FONT_FAMILY)
                            .text_size(px(12.0))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .whitespace_normal(),
                    ),
                ),
        );
    }
    error
}

pub(in crate::root) fn render_public_action_step_marker(
    status: PublicActionStepStatus,
    color: u32,
) -> gpui::Div {
    div()
        .size(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(rgb(color))
        .bg(rgb(theme::SURFACE))
        .text_color(rgb(color))
        .child(match status {
            PublicActionStepStatus::NotStarted => div()
                .size(px(7.0))
                .rounded_full()
                .bg(rgb(color))
                .into_any_element(),
            PublicActionStepStatus::Pending => Spinner::new()
                .icon(IconName::LoaderCircle)
                .color(rgb(color).into())
                .with_size(px(14.0))
                .into_any_element(),
            PublicActionStepStatus::Done => {
                Icon::new(IconName::CircleCheck).small().into_any_element()
            }
            PublicActionStepStatus::Error => Icon::new(IconName::TriangleAlert)
                .small()
                .into_any_element(),
            PublicActionStepStatus::Stopped => Icon::new(RailgunActionIcon::Square)
                .small()
                .into_any_element(),
        })
}

#[cfg(test)]
pub(in crate::root) const fn public_action_step_uses_stop_marker(
    status: PublicActionStepStatus,
) -> bool {
    matches!(status, PublicActionStepStatus::Stopped)
}

pub(in crate::root) fn render_public_action_step_hash(
    step: PublicActionProgressStep,
    tx_hash: &str,
) -> gpui::Div {
    let button_id = SharedString::from(format!(
        "wallet-public-action-{}-tx-copy",
        public_action_step_id(step)
    ));
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(
            app_muted_text(short_hash(tx_hash))
                .font_family(APP_MONO_FONT_FAMILY)
                .line_height(gpui::relative(1.0)),
        )
        .child(clipboard_with_toast(button_id, tx_hash.to_string()))
}

pub(in crate::root) const fn public_action_step_color(status: PublicActionStepStatus) -> u32 {
    match status {
        PublicActionStepStatus::NotStarted => theme::TEXT,
        PublicActionStepStatus::Pending => theme::WARNING,
        PublicActionStepStatus::Done => theme::SUCCESS,
        PublicActionStepStatus::Error | PublicActionStepStatus::Stopped => theme::DANGER,
    }
}
