//! Private submission content shared by the native progress dialog and browser handoff.
use crate::{
    controls::{app_muted_text, app_strong_text},
    private_action::{DisplayRow, display_row},
};
use gpui::{AnyElement, Div, IntoElement, ParentElement, SharedString, Styled, div, rgb};

#[must_use]
pub fn transaction_context(rows: Vec<DisplayRow>, action: Option<AnyElement>) -> Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(app_strong_text("Transaction context"))
        .children(rows.into_iter().map(display_row))
        .children(action)
}

#[must_use]
pub fn progress_step_body(
    label: String,
    detail: String,
    error_copy_id: Option<SharedString>,
    color: u32,
    action: Option<AnyElement>,
) -> Div {
    let copy = error_copy_id
        .map(|id| crate::clipboard::clipboard_with_toast(id, detail.clone()).into_any_element());
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_strong_text(label).text_color(rgb(color)))
        .child(
            div()
                .flex()
                .items_start()
                .gap_1()
                .child(
                    app_muted_text(detail)
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .text_color(rgb(color)),
                )
                .children(copy),
        )
        .children(action)
}

#[derive(Clone, Copy)]
pub enum OperationControl {
    Stop,
    StopWaiting,
    Ban,
    Favorite,
}

/// The owner supplies admitted controls; this composite only renders their actions.
#[must_use]
pub fn operation_controls(
    id: impl Into<SharedString>,
    controls: impl IntoIterator<Item = OperationControl>,
    on_control: impl Fn(OperationControl, &mut gpui::Window, &mut gpui::App) + 'static,
) -> Div {
    use gpui_component::{Sizable as _, button::ButtonVariants as _};
    let id = id.into();
    let on_control = std::rc::Rc::new(on_control);
    div().flex().flex_wrap().gap_2().children(controls.into_iter().map(|control| {
        let (suffix, label, help) = match control {
            OperationControl::Stop => ("stop", "Stop", "Stop local transaction preparation."),
            OperationControl::StopWaiting => ("stop-waiting", "Stop waiting", "Stop waiting for the broadcaster. A published transaction may still be submitted."),
            OperationControl::Ban => ("ban", "Ban this broadcaster", "Exclude this broadcaster from future selections. This does not stop the current wait."),
            OperationControl::Favorite => ("favorite", "Add to favorites", "Save this broadcaster to your favorites so future transactions can prefer it."),
        };
        let callback = on_control.clone();
        let button = crate::controls::app_button(SharedString::from(format!("{id}-{suffix}")), label)
            .small().outline().tooltip(help);
        let button = match control {
            OperationControl::Stop | OperationControl::StopWaiting | OperationControl::Ban => button.danger(),
            OperationControl::Favorite => button,
        };
        button.on_click(move |_, window, cx| callback(control, window, cx))
    }))
}
