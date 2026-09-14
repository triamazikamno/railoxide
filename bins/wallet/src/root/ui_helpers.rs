use gpui::{
    App, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels,
    SharedString, Styled, Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, Sizable,
    button::{Button, ButtonVariant, ButtonVariants},
    dialog::{AlertDialog, Cancel, Confirm, DialogButtonProps, DialogFooter},
    tag::Tag,
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button_base, app_muted_text, app_strong_text, app_text};
use ui::icons;
use ui::theme::{self, APP_FONT_FAMILY, APP_TEXT_SIZE};

use crate::assets::WalletIconSource;

pub(super) fn input_enter_scope(
    enabled: bool,
    submit: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    div().on_action(move |_: &gpui_component::input::Enter, window, cx| {
        cx.stop_propagation();
        if enabled {
            submit(window, cx);
        }
    })
}

const DIALOG_CONTENT_HORIZONTAL_INSET: Pixels = px(56.0);

pub(super) use ui::format::format_binary_bytes;

#[cfg(test)]
mod tests {
    use ui::format::{format_binary_bytes, format_decimal_byte_rate, format_decimal_bytes};

    #[test]
    fn byte_formatters_preserve_units_boundaries_and_max_values() {
        assert_eq!(format_decimal_byte_rate(None), "--");
        assert_eq!(format_decimal_bytes(999), "999 B");
        assert_eq!(format_decimal_byte_rate(Some(1_000)), "1.0 kB/s");
        assert_eq!(format_decimal_bytes(999_950), "1.0 MB");
        assert_eq!(format_decimal_byte_rate(Some(u64::MAX)), "18.4 EB/s");
        assert_eq!(format_binary_bytes(1023), "1023 B");
        assert_eq!(format_binary_bytes(1024), "1 KiB");
        assert_eq!(format_binary_bytes(u64::MAX), "15 EiB");
    }
}

pub(super) fn rgb_with_alpha(hex: u32, alpha: f32) -> gpui::Rgba {
    let mut color = rgb(hex);
    color.a = alpha;
    color
}

pub(super) fn count_label(count: usize, singular: &'static str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

pub(super) fn centered_message(message: impl Into<SharedString>) -> gpui::Div {
    let message = message.into();
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(theme::TEXT_SUBTLE))
        .child(message)
}

pub(super) fn secondary_dialog_content_width(dialog_width: Pixels) -> Pixels {
    (dialog_width - DIALOG_CONTENT_HORIZONTAL_INSET).max(px(0.0))
}

pub(super) fn dialog_max_height(window: &Window) -> Pixels {
    window.viewport_size().height * 0.84
}

pub(super) fn dialog_content_max_height(window: &Window) -> Pixels {
    window.viewport_size().height * 0.74
}

#[derive(Clone, Copy)]
pub(super) struct ConfirmationDialogProps {
    title: &'static str,
    body: &'static str,
    detail: Option<&'static str>,
    confirm_text: &'static str,
    confirm_variant: ButtonVariant,
}

impl ConfirmationDialogProps {
    pub(super) const fn danger(
        title: &'static str,
        body: &'static str,
        detail: Option<&'static str>,
        confirm_text: &'static str,
    ) -> Self {
        Self {
            title,
            body,
            detail,
            confirm_text,
            confirm_variant: ButtonVariant::Danger,
        }
    }
}

pub(super) fn dialog_footer(ok_text: impl Into<SharedString>, show_cancel: bool) -> DialogFooter {
    DialogFooter::new()
        .when(show_cancel, |footer| {
            footer.child(Button::new("cancel").secondary().label("Cancel").on_click(
                |_, window, cx| {
                    window.dispatch_action(Box::new(Cancel), cx);
                },
            ))
        })
        .child(
            Button::new("ok")
                .label(ok_text)
                .primary()
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(Confirm { secondary: false }), cx);
                }),
        )
}

pub(super) fn confirmation_dialog(
    dialog: AlertDialog,
    props: ConfirmationDialogProps,
    dialog_width: Pixels,
    dialog_max_height: Pixels,
) -> AlertDialog {
    let content_width = secondary_dialog_content_width(dialog_width);
    dialog
        .width(dialog_width)
        .max_h(dialog_max_height)
        .title(app_strong_text(props.title))
        .button_props(
            DialogButtonProps::default()
                .cancel_variant(ButtonVariant::Secondary)
                .ok_text(props.confirm_text)
                .ok_variant(props.confirm_variant),
        )
        .confirm()
        .child(confirmation_dialog_content(props, content_width))
}

fn confirmation_dialog_content(props: ConfirmationDialogProps, content_width: Pixels) -> gpui::Div {
    div()
        .w(content_width)
        .flex()
        .flex_col()
        .gap_2()
        .child(
            app_text(props.body)
                .line_height(px(20.0))
                .whitespace_normal(),
        )
        .when_some(props.detail, |this, detail| {
            this.child(
                app_muted_text(detail)
                    .text_size(px(12.0))
                    .line_height(px(17.0))
                    .whitespace_normal(),
            )
        })
}

pub(super) fn app_panel(bg: u32, border: u32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p(px(12.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(border))
        .bg(rgb(bg))
}

pub(super) fn app_refresh_button(
    id: impl Into<ElementId>,
    tooltip: impl Into<SharedString>,
    refreshing: bool,
    enabled: bool,
    on_refresh: impl Fn(&mut Window, &mut App) + 'static,
) -> Button {
    let tooltip: SharedString = tooltip.into();
    let button = app_button_base(id)
        .ghost()
        .xsmall()
        .compact()
        .icon(Icon::empty().path(icons::refresh_ccw_icon_path()))
        .accessibility_label(tooltip.clone())
        .tooltip(tooltip)
        .loading(refreshing)
        .disabled(refreshing || !enabled);

    if enabled && !refreshing {
        button.on_click(move |_event, window, cx| {
            cx.stop_propagation();
            on_refresh(window, cx);
        })
    } else {
        button
    }
}

pub(super) fn app_status_tag(label: impl Into<SharedString>, color: u32) -> impl IntoElement {
    Tag::custom(
        rgb_with_alpha(color, 0.12).into(),
        rgb(color).into(),
        rgb(color).into(),
    )
    .small()
    .rounded_full()
    .child(label.into())
}

pub(super) fn copyable_mono_field(
    label: &'static str,
    value: String,
    button_id: impl Into<ElementId>,
) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_2()
        .child(
            div()
                .w(px(72.0))
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .p(px(8.0))
                .rounded_sm()
                .bg(rgb(theme::BACKGROUND))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .font_family(APP_FONT_FAMILY)
                .font_weight(FontWeight::LIGHT)
                .text_size(APP_TEXT_SIZE)
                .text_color(rgb(theme::TEXT))
                .child(SharedString::from(value.clone())),
        )
        .child(clipboard_with_toast(button_id, value))
}

pub(super) fn app_stepper_container() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_0()
        .p(px(10.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
}

pub(super) fn app_step_row(
    marker: impl IntoElement,
    body: impl IntoElement,
    is_last: bool,
    color: u32,
    connector_min_height: Pixels,
    connector_opacity: Option<f32>,
) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .child(marker)
                .children((!is_last).then(|| {
                    let connector = div()
                        .w(px(2.0))
                        .flex_1()
                        .min_h(connector_min_height)
                        .my(px(3.0))
                        .rounded_full()
                        .bg(rgb(color));
                    if let Some(opacity) = connector_opacity {
                        connector.opacity(opacity)
                    } else {
                        connector
                    }
                })),
        )
        .child(body)
}

pub(super) fn labeled_field(
    label: impl Into<SharedString>,
    content: impl IntoElement,
) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text(label))
        .child(content)
}

pub(super) fn token_label_row(
    label: SharedString,
    icon_path: Option<WalletIconSource>,
    icon_size: Pixels,
) -> gpui::Div {
    let mut row = div().flex().items_center().gap_1();
    if let Some(path) = icon_path {
        row = row.child(img(path).size(icon_size).rounded_full().flex_none());
    }
    row.child(label)
}

#[cfg(test)]
mod input_enter_tests {
    use gpui::{AppContext as _, Context, Entity, Focusable as _, Render, TestAppContext};
    use gpui_component::input::InputState;

    use super::*;

    struct InputEnterProbe {
        inputs: [Entity<InputState>; 2],
        focus: gpui::FocusHandle,
        enabled: bool,
        submissions: [usize; 2],
        dialog_confirmations: usize,
        dialog_closes: usize,
    }

    impl Render for InputEnterProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            let confirm_probe = cx.entity();
            let close_probe = cx.entity();
            gpui_kit::base::Dialog::new(cx)
                .focus_handle(self.focus.clone())
                .on_ok(move |_, _, cx| {
                    confirm_probe.update(cx, |probe, _| probe.dialog_confirmations += 1);
                    true
                })
                .on_close(move |_, _, cx| {
                    close_probe.update(cx, |probe, _| probe.dialog_closes += 1);
                })
                .popup(
                    div()
                        .flex()
                        .flex_col()
                        .children(self.inputs.iter().enumerate().map(|(index, input)| {
                            let submit_probe = cx.entity();
                            input_enter_scope(self.enabled, move |_, cx| {
                                submit_probe.update(cx, |probe, _| probe.submissions[index] += 1);
                            })
                            .child(ui::controls::app_input(input))
                        })),
                )
        }
    }

    #[gpui::test]
    fn input_enter_scopes_submit_only_the_focused_operation_and_consume_when_disabled(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        cx.update(ui::theme::apply_zenburn_component_theme);
        let (probe, cx) = cx.add_window_view(|window, cx| InputEnterProbe {
            inputs: [
                cx.new(|cx| InputState::new(window, cx)),
                cx.new(|cx| InputState::new(window, cx)),
            ],
            focus: cx.focus_handle(),
            enabled: true,
            submissions: [0; 2],
            dialog_confirmations: 0,
            dialog_closes: 0,
        });
        let mut expected = [0; 2];
        for enabled in [true, false, true] {
            for index in 0..2 {
                probe.update(cx, |probe, cx| {
                    probe.enabled = enabled;
                    cx.notify();
                });
                cx.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    probe.read(cx).inputs[index]
                        .read(cx)
                        .focus_handle(cx)
                        .focus(window, cx);
                });
                let keystroke = gpui::Keystroke::parse("enter").expect("Enter key");
                cx.simulate_event(gpui::KeyDownEvent {
                    keystroke: keystroke.clone(),
                    is_held: false,
                    prefer_character_input: false,
                });
                cx.simulate_event(gpui::KeyUpEvent { keystroke });
                cx.run_until_parked();
                expected[index] += usize::from(enabled);
                cx.update(|_, cx| {
                    let probe = probe.read(cx);
                    assert_eq!(probe.submissions, expected);
                    assert_eq!(probe.dialog_confirmations, 0);
                    assert_eq!(probe.dialog_closes, 0);
                });
            }
        }
    }
}

#[cfg(test)]
pub(super) mod select_layout_test {
    use std::sync::Arc;

    use gpui::{AppContext as _, Context, Entity, Render, TestAppContext};
    use gpui_component::{
        IndexPath,
        select::{Select, SelectItem, SelectState},
    };
    use ui::controls::FullWidthSelectItems;

    use super::*;

    struct SelectProbe<T: SelectItem + 'static> {
        select: Entity<SelectState<FullWidthSelectItems<T>>>,
        width: Pixels,
        small: bool,
    }

    impl<T: SelectItem + 'static> Render for SelectProbe<T> {
        fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
            div().p_4().child(
                div()
                    .debug_selector(|| "select-layout-trigger".to_owned())
                    .w(self.width)
                    .child(
                        Select::new(&self.select)
                            .when(self.small, Sizable::small)
                            .w_full()
                            .menu_width(self.width),
                    ),
            )
        }
    }

    pub(in crate::root) fn assert_balances_align_and_rows_select<T>(
        cx: &mut TestAppContext,
        items: Vec<T>,
        widths: [f32; 2],
        small: bool,
        balance_selectors: [&'static str; 2],
        expected_value: &str,
    ) where
        T: SelectItem<Value = Arc<str>> + 'static,
    {
        cx.update(gpui_component::init);
        cx.update(ui::theme::apply_zenburn_component_theme);
        let (probe, cx) = cx.add_window_view(|window, cx| SelectProbe {
            select: cx.new(|cx| {
                SelectState::new(
                    FullWidthSelectItems::new(items),
                    Some(IndexPath::new(0)),
                    window,
                    cx,
                )
                .searchable(true)
            }),
            width: px(widths[0]),
            small,
        });
        for width in widths {
            probe.update(cx, |probe, cx| {
                probe.width = px(width);
                cx.notify();
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let trigger = cx
                .debug_bounds("select-layout-trigger")
                .expect("select trigger");
            cx.simulate_click(trigger.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let first = cx
                .debug_bounds(balance_selectors[0])
                .expect("first balance");
            let second = cx
                .debug_bounds(balance_selectors[1])
                .expect("second balance");
            assert!((first.right() - second.right()).abs() < px(1.0));
            assert!(
                second.right() > trigger.right() - px(50.0),
                "balance must reach the menu's trailing content edge"
            );
            assert!(second.right() < trigger.right());
            cx.simulate_click(second.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            cx.update(|_, cx| {
                assert_eq!(
                    probe
                        .read(cx)
                        .select
                        .read(cx)
                        .selected_value()
                        .map(AsRef::as_ref),
                    Some(expected_value)
                );
            });
        }
    }
}
