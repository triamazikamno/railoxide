use gpui::{
    AnyElement, Div, InteractiveElement as _, ParentElement as _, SharedString,
    prelude::FluentBuilder as _, px,
};

use super::public_action::{
    PublicActionStepStatus, public_action_step_color, render_public_action_step_marker,
};
use super::{app_step_row, app_stepper_container};

/// Presentation supplied by each submission owner to the shared progress view.
pub(super) struct SubmissionProgressStep {
    pub(super) label: String,
    pub(super) detail: String,
    pub(super) status: PublicActionStepStatus,
    pub(super) error_copy_id: SharedString,
    pub(super) action: Option<AnyElement>,
}

pub(super) fn render_submission_progress_stepper(
    steps: impl IntoIterator<Item = SubmissionProgressStep>,
) -> Div {
    let mut steps = steps.into_iter().peekable();
    let mut stepper = app_stepper_container();
    while let Some(step) = steps.next() {
        let is_last = steps.peek().is_none();
        let color = public_action_step_color(step.status);
        let selector = format!("submission-step-{}", step.error_copy_id);
        let body = ui::private_submission::progress_step_body(
            step.label,
            step.detail,
            (step.status == PublicActionStepStatus::Error).then_some(step.error_copy_id),
            color,
            step.action,
        )
        .when(!is_last, gpui::Styled::pb_3);
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
