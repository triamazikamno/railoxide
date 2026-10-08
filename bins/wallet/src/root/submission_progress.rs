use gpui::{
    AnyElement, Div, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    Styled as _, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{Icon, IconName, Sizable as _, spinner::Spinner};
use ui::controls::{app_muted_text, app_text};

use super::public_action::{
    PublicActionStepStatus, public_action_step_color, render_public_action_step_marker,
};
use super::{app_step_row, app_stepper_container};
use crate::assets::RailgunActionIcon;

/// Presentation supplied by each submission owner to the shared progress view.
pub(super) struct SubmissionProgressStep {
    pub(super) label: String,
    pub(super) detail: String,
    pub(super) status: PublicActionStepStatus,
    pub(super) error_copy_id: SharedString,
    pub(super) action: Option<AnyElement>,
}

/// One part of a step whose parts run side by side, such as one setup per network.
pub(super) struct SubmissionProgressSubstep {
    /// The sub-step's identity within its stepper.
    pub(super) id: SharedString,
    pub(super) label: String,
    pub(super) status: PublicActionStepStatus,
    /// Content after the label, such as an account and the control that copies its address.
    pub(super) content: Option<AnyElement>,
    /// What the sub-step waits for, or its result, at the trailing edge.
    pub(super) outcome: String,
}

/// A step and the sub-steps shown indented under it.
pub(super) struct SubmissionProgressGroup {
    pub(super) step: SubmissionProgressStep,
    /// The step's result at its trailing edge, such as the block it was included in.
    pub(super) outcome: String,
    pub(super) substeps: Vec<SubmissionProgressSubstep>,
}

pub(super) fn render_submission_progress_stepper(
    steps: impl IntoIterator<Item = SubmissionProgressStep>,
) -> Div {
    render_submission_progress_groups(steps.into_iter().map(|step| SubmissionProgressGroup {
        step,
        outcome: String::new(),
        substeps: Vec::new(),
    }))
}

/// The stepper for steps that may have sub-steps.
pub(super) fn render_submission_progress_groups(
    groups: impl IntoIterator<Item = SubmissionProgressGroup>,
) -> Div {
    let mut groups = groups.into_iter().peekable();
    let mut stepper = app_stepper_container();
    while let Some(SubmissionProgressGroup {
        step,
        outcome,
        substeps,
    }) = groups.next()
    {
        let is_last = groups.peek().is_none();
        let color = public_action_step_color(step.status);
        let selector = format!("submission-step-{}", step.error_copy_id);
        let body = ui::private_submission::progress_step_body(
            step.label,
            step.detail,
            (step.status == PublicActionStepStatus::Error).then_some(step.error_copy_id),
            color,
            step.action,
        );
        let body = if outcome.is_empty() {
            body
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_start()
                .gap_2()
                .child(body)
                .child(app_muted_text(outcome).flex_none())
        };
        let body = if substeps.is_empty() {
            body
        } else {
            substeps_body(body, substeps)
        };
        let body = body.when(!is_last, gpui::Styled::pb_3);
        stepper = stepper.child(
            app_step_row(
                render_public_action_step_marker(step.status, color),
                body,
                is_last,
                color,
                px(32.0),
                None,
            )
            .debug_selector(move || selector),
        );
    }
    stepper
}

/// A step's body with its sub-steps under it. Until every sub-step is done, the trailing edge
/// counts the ones that are.
fn substeps_body(body: Div, substeps: Vec<SubmissionProgressSubstep>) -> Div {
    let done = substeps
        .iter()
        .filter(|substep| substep.status == PublicActionStepStatus::Done)
        .count();
    let count = (done < substeps.len())
        .then(|| app_muted_text(format!("{done} of {}", substeps.len())).flex_none());
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .w_full()
                .min_w_0()
                .flex()
                .items_start()
                .gap_2()
                .child(body)
                .children(count),
        )
        .children(substeps.into_iter().map(render_substep))
}

fn render_substep(substep: SubmissionProgressSubstep) -> Div {
    let color = public_action_step_color(substep.status);
    let failed = substep.status == PublicActionStepStatus::Error;
    let selector = format!("submission-substep-{}", substep.id);
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .debug_selector(move || selector)
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap_2()
                .child(render_substep_marker(substep.status, color))
                .child(app_text(substep.label).flex_none())
                .children(substep.content),
        )
        .when(!substep.outcome.is_empty(), |row| {
            row.child(
                app_muted_text(substep.outcome)
                    .flex_none()
                    .when(failed, |outcome| outcome.text_color(rgb(color))),
            )
        })
}

/// A sub-step's status in a slot one text line tall, without the ring of a step's marker.
fn render_substep_marker(status: PublicActionStepStatus, color: u32) -> Div {
    div()
        .size_4()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(color))
        .child(match status {
            PublicActionStepStatus::NotStarted => div()
                .size_1p5()
                .rounded_full()
                .bg(rgb(color))
                .into_any_element(),
            PublicActionStepStatus::Pending => Spinner::new()
                .icon(IconName::LoaderCircle)
                .color(rgb(color).into())
                .xsmall()
                .into_any_element(),
            PublicActionStepStatus::Done => {
                Icon::new(IconName::CircleCheck).xsmall().into_any_element()
            }
            PublicActionStepStatus::Error | PublicActionStepStatus::Warning => {
                Icon::new(IconName::TriangleAlert)
                    .xsmall()
                    .into_any_element()
            }
            PublicActionStepStatus::Stopped => Icon::new(RailgunActionIcon::Square)
                .xsmall()
                .into_any_element(),
        })
}
