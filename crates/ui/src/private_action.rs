//! Portable private-action presentation. Callers own amounts, eligibility and commands.
pub mod self_broadcast;
use gpui::{
    App, Div, ElementId, IntoElement, ParentElement, SharedString, Styled, Window, div, relative,
    rems, rgb,
};
use gpui_component::{
    Disableable, Sizable,
    button::{ButtonGroup, ButtonVariants},
    switch::Switch,
};

use crate::controls::{app_muted_text, app_segment_button, app_strong_text};
use crate::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_LINE_HEIGHT, APP_TEXT_SIZE};

#[must_use]
pub fn asset_row(label: impl Into<SharedString>, icon: Option<gpui::ImageSource>) -> Div {
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .children(icon.map(|icon| gpui::img(icon).size_4().rounded_full().flex_none()))
        .child(div().min_w_0().truncate().child(label.into()))
}

#[must_use]
pub fn broadcaster_count_label(count: usize) -> String {
    match count {
        0 => "no broadcasters".into(),
        1 => "1 broadcaster".into(),
        count => format!("{count} broadcasters"),
    }
}

#[must_use]
pub fn fee_token_row(label: &str, icon: Option<gpui::ImageSource>, count: usize) -> Div {
    asset_row(
        format!("{label} · {}", broadcaster_count_label(count)),
        icon,
    )
}

#[must_use]
pub fn fee_token_control(control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(app_muted_text("Fee token").flex_none())
        .child(
            div()
                .flex()
                .justify_end()
                .flex_1()
                .min_w_0()
                .max_w_56()
                .child(control),
        )
}

#[must_use]
pub fn asset_select<D>(
    state: &gpui::Entity<gpui_component::select::SelectState<D>>,
    disabled: bool,
) -> Div
where
    D: gpui_component::select::SelectDelegate + 'static,
    <D::Item as gpui_component::select::SelectItem>::Value: PartialEq + Clone,
{
    div().w_full().child(
        gpui_component::select::Select::new(state)
            .w_full()
            .placeholder("Select asset")
            .disabled(disabled),
    )
}

#[must_use]
pub fn amount_input(
    state: &gpui::Entity<gpui_component::input::InputState>,
    disabled: bool,
    submit_enabled: bool,
    submit: impl Fn(&mut Window, &mut App) + 'static,
) -> Div {
    use gpui::InteractiveElement as _;
    div()
        .on_action(move |_: &gpui_component::input::Enter, window, cx| {
            if submit_enabled {
                submit(window, cx);
            }
        })
        .child(crate::controls::app_input(state).disabled(disabled))
}

#[must_use]
pub fn amount_metric(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    value: String,
    disabled: bool,
    on_select: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui_component::button::Button {
    crate::controls::app_button_base(id)
        .disabled(disabled)
        .child(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_2()
                .child(app_muted_text(label))
                .child(app_strong_text(value)),
        )
        .on_click(move |_, window, cx| on_select(window, cx))
}

pub struct BroadcasterSettings {
    pub allow_out_of_range: bool,
    pub favorites_only: bool,
    pub random_selected: bool,
    pub specific_label: String,
    pub candidate_count: usize,
    pub disabled: bool,
}

#[derive(Clone, Copy)]
pub enum BroadcasterSettingsEvent {
    Random,
    ChooseSpecific,
    AllowOutOfRange(bool),
    FavoritesOnly(bool),
}

#[must_use]
pub fn broadcaster_settings(
    id: impl Into<ElementId>,
    settings: BroadcasterSettings,
    fee_token: impl IntoElement,
    fee_mode: Option<gpui::AnyElement>,
    on_change: impl Fn(BroadcasterSettingsEvent, &mut Window, &mut App) + 'static,
) -> Div {
    use BroadcasterSettingsEvent as Event;
    use gpui::{InteractiveElement, StatefulInteractiveElement as _, prelude::FluentBuilder as _};
    use gpui_component::tooltip::Tooltip;
    let on_change = std::rc::Rc::new(on_change);
    let policy = on_change.clone();
    let favorites = on_change.clone();
    let random = on_change.clone();
    let disabled = settings.disabled;
    let selector_disabled = disabled || settings.candidate_count == 0;
    div().child(
        div()
            .id(id)
            .min_w_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_2p5()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(
                // The pinned Switch stores its tooltip but does not render it.
                div()
                    .id("out-of-range-help")
                    .tooltip(|window, cx| {
                        Tooltip::new("Allow broadcaster fees outside the configured anchor range.")
                            .build(window, cx)
                    })
                    .child(
                        Switch::new("out-of-range")
                            .small()
                            .label("Allow out-of-range fees")
                            .checked(settings.allow_out_of_range)
                            .disabled(disabled)
                            .on_click(move |checked, window, cx| {
                                policy(Event::AllowOutOfRange(*checked), window, cx);
                            }),
                    ),
            )
            .child(
                div()
                    .id("favorites-only-help")
                    .tooltip(|window, cx| {
                        Tooltip::new("Only use broadcasters saved in your favorites list.")
                            .build(window, cx)
                    })
                    .child(
                        Switch::new("favorites-only")
                            .small()
                            .label("Favorites only")
                            .checked(settings.favorites_only)
                            .disabled(disabled)
                            .on_click(move |checked, window, cx| {
                                favorites(Event::FavoritesOnly(*checked), window, cx);
                            }),
                    ),
            )
            .child(fee_token)
            .child(
                ButtonGroup::new("choice")
                    .outline()
                    .compact()
                    .w_full()
                    .disabled(selector_disabled)
                    .child(
                        app_segment_button(
                            "random",
                            "Random",
                            settings.random_selected,
                            selector_disabled,
                            None,
                        )
                        .flex_1()
                        .min_w_0()
                        .on_click(move |_, window, cx| random(Event::Random, window, cx)),
                    )
                    .child(
                        app_segment_button(
                            "specific",
                            settings.specific_label,
                            !settings.random_selected,
                            selector_disabled,
                            None,
                        )
                        .flex_1()
                        .min_w_0()
                        .on_click(move |_, window, cx| {
                            on_change(Event::ChooseSpecific, window, cx);
                        }),
                    ),
            )
            .children(fee_mode)
            .when(settings.candidate_count == 0, |this| {
                this.child(app_muted_text(
                    "No eligible broadcaster currently advertises this token.",
                ))
            }),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayRow {
    pub label: String,
    pub value: String,
    pub suffix: Option<String>,
}

#[must_use]
pub fn display_row(row: DisplayRow) -> Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .child(app_muted_text(row.label).flex_none())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_wrap()
                .justify_end()
                .gap_1()
                .text_align(gpui::TextAlign::Right)
                .child(app_strong_text(row.value).min_w_0().whitespace_normal())
                .children(
                    row.suffix
                        .map(|suffix| app_strong_text(suffix).min_w_0().whitespace_normal()),
                ),
        )
}

#[must_use]
pub fn fee_mode_toggle(
    id: impl Into<ElementId>,
    unshield: bool,
    broadcaster: bool,
    add_on_top: bool,
    disabled: bool,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> Div {
    let (deduct_help, add_help) = match (unshield, broadcaster) {
        (false, _) => (
            "Use the entered amount as the token spend. Recipient receives less after the broadcaster fee.",
            "Recipient receives the entered amount. The wallet adds the broadcaster fee on top.",
        ),
        (true, true) => (
            "Use the entered amount as the token spend. Recipient receives less after the RAILGUN fee, and after broadcaster fee if paid in this token.",
            "Recipient receives the entered amount. The wallet adds the RAILGUN fee, and broadcaster fee if paid in this token.",
        ),
        (true, false) => (
            "Use the entered amount as the token spend. Recipient receives less after the RAILGUN fee.",
            "Recipient receives the entered amount. The wallet adds the RAILGUN fee on top.",
        ),
    };
    let on_change = std::rc::Rc::new(on_change);
    let deduct = on_change.clone();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_3()
        .child(app_muted_text("Fees"))
        .child(
            ButtonGroup::new(id)
                .outline()
                .compact()
                .disabled(disabled)
                .child(
                    app_segment_button("deduct", "Deduct", !add_on_top, disabled, None)
                        .tooltip(deduct_help)
                        .on_click(move |_, window, cx| deduct(false, window, cx)),
                )
                .child(
                    app_segment_button("add", "Add on top", add_on_top, disabled, None)
                        .tooltip(add_help)
                        .on_click(move |_, window, cx| on_change(true, window, cx)),
                ),
        )
}

#[must_use]
pub fn native_top_up_control(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    funding_detail: String,
    enabled: bool,
    disabled: bool,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> Div {
    use gpui::{InteractiveElement as _, StatefulInteractiveElement as _};
    use gpui_component::tooltip::Tooltip;

    div().child(
        div()
            .id(id)
            .tooltip(move |window, cx| {
                let detail = funding_detail.clone();
                let width = (window.viewport_size().width - window.rem_size() * 3.0)
                    .min(window.rem_size() * 20.0);
                Tooltip::element(move |_, _| {
                    div()
                        .max_w(width)
                        .whitespace_normal()
                        .line_height(relative(APP_TEXT_LINE_HEIGHT))
                        .child(detail.clone())
                })
                .build(window, cx)
            })
            .child(
                Switch::new("native-top-up")
                    .label(label.into())
                    .checked(enabled)
                    .disabled(disabled)
                    .on_click(on_change),
            ),
    )
}

#[must_use]
pub fn unshield_output_toggle(
    id: impl Into<ElementId>,
    native_label: impl Into<SharedString>,
    wrapped_label: impl Into<SharedString>,
    unwrap: bool,
    disabled: bool,
    on_change: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> Div {
    let on_change = std::rc::Rc::new(on_change);
    let native = on_change.clone();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(app_muted_text("Output"))
        .child(
            ButtonGroup::new(id)
                .outline()
                .disabled(disabled)
                .child(
                    app_segment_button("native", native_label, unwrap, disabled, None)
                        .on_click(move |_, window, cx| native(true, window, cx)),
                )
                .child(
                    app_segment_button("wrapped", wrapped_label, !unwrap, disabled, None)
                        .on_click(move |_, window, cx| on_change(false, window, cx)),
                ),
        )
}

#[must_use]
pub fn outcome(rows: impl IntoIterator<Item = DisplayRow>) -> impl IntoElement {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .children(rows.into_iter().map(display_row))
}

#[must_use]
pub fn estimated_outcome(
    broadcaster: String,
    rows: Vec<DisplayRow>,
    fee_breakdown: impl IntoElement,
    shape: String,
    refresh_id: impl Into<ElementId>,
    refreshing: bool,
    on_refresh: impl Fn(&mut Window, &mut App) + 'static,
) -> Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_STRONG))
        .bg(rgb(theme::SURFACE_ELEVATED))
        .child(
            div()
                .flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(app_strong_text("Estimated outcome"))
                        .child(estimate_detail_text(
                            "Proof is not generated yet; the final fee may move slightly before publish.",
                        )),
                )
                .child(
                    crate::controls::app_button_base(refresh_id)
                        .ghost()
                        .xsmall()
                        .compact()
                        .icon(gpui_component::Icon::empty().path(crate::icons::refresh_ccw_icon_path()))
                        .accessibility_label("Refresh estimate")
                        .tooltip("Refresh estimate")
                        .loading(refreshing)
                        .disabled(refreshing)
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            on_refresh(window, cx);
                        }),
                ),
        )
        .child(
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap_3()
                .child(app_muted_text("Broadcaster").flex_none())
                .child(
                    app_strong_text(broadcaster)
                        .flex_1()
                        .min_w_0()
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_align(gpui::TextAlign::Right)
                        .whitespace_normal(),
                ),
        )
        .child(outcome(rows))
        .child(fee_breakdown)
        .child(estimate_detail_text(shape))
}

fn estimate_detail_text(text: impl Into<SharedString>) -> Div {
    div()
        .min_w_0()
        .text_size(rems(0.875))
        .line_height(relative(APP_TEXT_LINE_HEIGHT))
        .text_color(rgb(theme::TEXT_SUBTLE))
        .whitespace_normal()
        .child(text.into())
}

#[must_use]
pub fn transaction_fee_breakdown(
    id: impl Into<ElementId>,
    total: String,
    rows: Vec<DisplayRow>,
    network_gas: String,
    open: bool,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    gpui_component::collapsible::Collapsible::new()
        .open(open)
        .w_full()
        .min_w_0()
        .child(
            crate::controls::app_button_base(id)
                .ghost()
                .w_full()
                .min_w_0()
                .h_auto()
                .min_h_8()
                .px_0()
                .py_1()
                .text_size(APP_TEXT_SIZE)
                .line_height(relative(APP_TEXT_LINE_HEIGHT))
                .accessibility_label("Transaction fee")
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(div().flex_none().child("Transaction fee"))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .justify_end()
                                .gap_2()
                                .child(
                                    div()
                                        .min_w_0()
                                        .text_align(gpui::TextAlign::Right)
                                        .whitespace_normal()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .child(total),
                                )
                                .child(
                                    gpui_component::Icon::new(if open {
                                        gpui_component::IconName::ChevronUp
                                    } else {
                                        gpui_component::IconName::ChevronDown
                                    })
                                    .xsmall()
                                    .flex_none()
                                    .text_color(rgb(theme::TEXT_MUTED)),
                                ),
                        ),
                )
                .on_click(move |_, window, cx| on_toggle(!open, window, cx)),
        )
        .content(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_2()
                .border_t_1()
                .border_color(rgb(theme::BORDER))
                .children(
                    rows.into_iter()
                        .map(|row| transaction_fee_row(row.label, row.value, false)),
                )
                .child(transaction_fee_row("Network gas".into(), network_gas, true)),
        )
}

fn transaction_fee_row(label: String, value: String, muted: bool) -> Div {
    div()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .text_size(rems(0.875))
        .line_height(relative(APP_TEXT_LINE_HEIGHT))
        .text_color(rgb(if muted {
            theme::TEXT_SUBTLE
        } else {
            theme::TEXT
        }))
        .child(div().flex_none().child(label))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_align(gpui::TextAlign::Right)
                .whitespace_normal()
                .child(value),
        )
}

#[must_use]
pub fn warning(
    id: impl Into<ElementId>,
    message: impl Into<SharedString>,
) -> gpui_component::alert::Alert {
    gpui_component::alert::Alert::warning(id, message.into()).small()
}

#[must_use]
pub const fn spend_capacity_warning(unshield: bool) -> &'static str {
    if unshield {
        "Spend capacity is limited by private note fragmentation and POI verification status."
    } else {
        "Spend capacity is limited by private note fragmentation and POI verification status. One send can spend up to 8 proof chunks."
    }
}

pub const NATIVE_TOP_UP_LINKAGE_WARNING: &str = "The token unshield and native gas top-up are public outputs to the same recipient and can be linked.";
