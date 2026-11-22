//! The common amounts popover on the Shield amount-crowd factor, or at the end of the Unshield
//! hints: the nearby amounts many deposits or recent exits used, each with a bar, any of which
//! fills in the form's amount unless the list is browse-only, as Unshield's is with fees added
//! on top.

use gpui::{
    Anchor, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, accesskit::Role, div, prelude::FluentBuilder as _, px,
    relative, rems, rgb,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    popover::{Popover, PopoverState},
};
use gpui_kit::TestSupportExt as _;
use ui::controls::{app_button, app_button_base};
use ui::hint::hint_card;
use ui::theme;
use wallet_ops::PrivacyLevel;

use crate::assets::RailgunActionIcon;

use super::presentation::{
    COMMON_AMOUNTS_FOOTER, COMMON_AMOUNTS_LABEL, COMMON_EXIT_AMOUNTS_BROWSE_FOOTER,
    CommonAmountRow, CommonAmounts,
};
use super::{UseAmount, first_line_box, level_style};

/// What opens the common amounts: a labelled button under the amount-crowd factor of an
/// uncommon amount or at the end of the Unshield hints, or an icon on the factor of a common one,
/// like the trees info button.
#[derive(Clone, Copy)]
pub(super) enum CommonAmountsTrigger {
    Button,
    Icon,
}

/// A button whose popover lists the common amounts with a bar each. When they are choosable,
/// choosing one closes the popover and fills it in with `on_use`; otherwise the list is
/// browse-only and its footer says how to make them choosable. The entered amount shows in the
/// colour of `level`, the amount-crowd factor's, or in the primary colour without one, and can't
/// be chosen.
pub(super) fn render_common_amounts(
    id: &str,
    common: CommonAmounts,
    level: Option<PrivacyLevel>,
    on_use: UseAmount,
    trigger: CommonAmountsTrigger,
) -> impl IntoElement {
    let id = format!("{id}-common-amounts");
    let max_count = common.rows.iter().map(|row| row.count).max().unwrap_or(0);
    let button_id = SharedString::from(format!("{id}-button"));
    let (wrapper, button) = trigger_button(button_id, common.button_label, trigger);
    wrapper
        .id(SharedString::from(id.clone()))
        .child(
            Popover::new(SharedString::from(format!("{id}-popover")))
                // Without an amount-crowd factor the button ends the card, low in a long form,
                // and the popover doesn't flip when it can't fit below, so it opens upwards.
                .when(level.is_none(), |popover| {
                    popover.anchor(Anchor::BottomLeft)
                })
                .trigger(button)
                .content(move |_, window, cx| {
                    let popover = cx.entity();
                    let entered_color: Hsla = level.map_or_else(
                        || rgb(theme::PRIMARY).into(),
                        |level| level_style(level, cx).1,
                    );
                    hint_card(common.title.clone(), theme::INFO, window)
                        .when_some(common.explainer, |card, explainer| {
                            card.child(
                                div()
                                    .whitespace_normal()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(explainer),
                            )
                        })
                        .child(div().flex().flex_col().children(
                            common.rows.iter().enumerate().map(|(index, row)| {
                                let id = format!("{id}-{index}");
                                common_amount_row(
                                    id,
                                    row,
                                    max_count,
                                    entered_color,
                                    &popover,
                                    common.choosable.then_some(&on_use),
                                )
                            }),
                        ))
                        .child(
                            div()
                                .whitespace_normal()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(if common.choosable {
                                    COMMON_AMOUNTS_FOOTER
                                } else {
                                    COMMON_EXIT_AMOUNTS_BROWSE_FOOTER
                                }),
                        )
                }),
        )
        .test_support()
}

/// The trigger's wrapper and its button: labelled `label` as a button, or with Shield's label for
/// assistive technology as an icon.
fn trigger_button(
    button_id: SharedString,
    label: &'static str,
    trigger: CommonAmountsTrigger,
) -> (gpui::Div, Button) {
    let icon = Icon::new(RailgunActionIcon::ChartNoAxesColumn);
    match trigger {
        CommonAmountsTrigger::Button => (
            div(),
            app_button(button_id, label).outline().small().icon(icon),
        ),
        CommonAmountsTrigger::Icon => (
            first_line_box(),
            app_button_base(button_id)
                .ghost()
                .small()
                // As the trees info button.
                .size(px(18.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .icon(icon)
                .accessibility_label(COMMON_AMOUNTS_LABEL),
        ),
    }
}

/// A common amount as a button that closes `popover` and fills it in with `on_use`, or as plain
/// text without `on_use`, for the entered amount, or for one above the balance or the form's
/// maximum.
fn common_amount_row(
    id: String,
    row: &CommonAmountRow,
    max_count: u64,
    entered_color: Hsla,
    popover: &Entity<PopoverState>,
    on_use: Option<&UseAmount>,
) -> gpui::AnyElement {
    let line = common_amount_line(row, max_count, entered_color);
    let Some(on_use) = on_use.filter(|_| row.within_balance && !row.entered) else {
        return line.into_any_element();
    };
    let (amount, popover, on_use) = (row.amount, popover.clone(), on_use.clone());
    line.id(SharedString::from(id))
        .role(Role::Button)
        .aria_label(row.accessibility_label.clone())
        .cursor_pointer()
        .hover(|line| line.bg(rgb(theme::SURFACE_HOVER_SUBTLE)))
        .on_click(move |_event, window, cx| {
            popover.update(cx, |state, cx| state.dismiss(window, cx));
            on_use(amount, window, cx);
        })
        .into_any_element()
}

/// The amount right-aligned in a column that fits "2,350,000 · yours", a bar as long as its
/// share of `max_count`, then the count. The entered amount is in `entered_color` and bold, one
/// above the balance or the form's maximum is dimmed, and a count of zero has no bar.
fn common_amount_line(row: &CommonAmountRow, max_count: u64, entered_color: Hsla) -> gpui::Div {
    let (text_color, bar_color) = if row.entered {
        (entered_color, entered_color)
    } else if row.within_balance {
        (rgb(theme::TEXT).into(), rgb(theme::TEXT_SUBTLE).into())
    } else {
        (rgb(theme::TEXT_SUBTLE).into(), rgb(theme::BORDER).into())
    };
    // Bars are too short for the precision lost to matter.
    #[allow(clippy::cast_precision_loss)]
    let share = row.count as f32 / max_count.max(1) as f32;
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_1()
        .py(px(2.0))
        .rounded_sm()
        .text_color(text_color)
        .when(row.entered, |line| line.font_weight(FontWeight::SEMIBOLD))
        .child(
            div()
                .flex_none()
                .w(rems(6.5))
                .text_right()
                .child(row.amount_text.clone()),
        )
        .child(
            div().flex_1().min_w_0().child(
                div()
                    .h(px(8.0))
                    .when(row.count > 0, |bar| bar.min_w(px(2.0)))
                    .w(relative(share))
                    .rounded_sm()
                    .bg(bar_color),
            ),
        )
        .child(
            div()
                .flex_none()
                .w(rems(3.0))
                .text_right()
                .child(row.count_text.clone()),
        )
}
