use gpui::{Div, ParentElement, SharedString, Styled, div, prelude::FluentBuilder as _, px, rgb};

use crate::{
    controls::{app_muted_text, app_strong_text},
    theme,
};

/// Shared fee presentation; callers own estimation, currency formatting, and row visibility.
#[must_use]
pub fn estimated_fees(
    gas_limit: Option<String>,
    expected_gas_cost: String,
    maximum_gas_cost: Option<String>,
    protocol_fee: Option<(String, String)>,
    mono_font: impl Into<SharedString>,
) -> Div {
    let mono_font = mono_font.into();
    div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_2()
        .p(px(10.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(app_strong_text("Estimated fees"))
        .when_some(gas_limit, |this, limit| {
            this.child(fee_row("Gas limit", limit, false, &mono_font))
        })
        .child(fee_row(
            "Expected gas cost",
            expected_gas_cost,
            false,
            &mono_font,
        ))
        .when_some(maximum_gas_cost, |this, maximum| {
            this.child(fee_row("Maximum gas cost", maximum, true, &mono_font))
        })
        .when_some(protocol_fee, |this, (label, value)| {
            this.child(fee_row(label, value, false, &mono_font))
        })
}

fn fee_row(
    label: impl Into<SharedString>,
    value: String,
    muted: bool,
    mono_font: &SharedString,
) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_2()
        .child(app_muted_text(label).flex_none())
        .child(
            (if muted {
                app_muted_text(value)
            } else {
                app_strong_text(value)
            })
            .min_w(px(0.0))
            .text_size(px(13.0))
            .font_family(mono_font.clone())
            .whitespace_normal(),
        )
}
