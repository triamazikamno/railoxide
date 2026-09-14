//! Compact balance header shared by browser Home views.
use gpui::{IntoElement, ParentElement as _, SharedString, Styled as _, div, px, relative, rems};

use crate::{
    controls::{app_muted_text, app_strong_text},
    theme::APP_TEXT_LINE_HEIGHT,
};

const VALUE_TEXT_SIZE: gpui::Rems = gpui::Rems(1.5);
pub const CAPTION_TEXT_SIZE: gpui::Pixels = px(12.0);

/// Align a network control with the balance amount's text row.
#[must_use]
pub fn wallet_balance_network(network: impl IntoElement) -> gpui::Div {
    div()
        .w_full()
        .h(VALUE_TEXT_SIZE * APP_TEXT_LINE_HEIGHT)
        .flex_none()
        .flex()
        .items_center()
        .child(network)
}

#[must_use]
pub fn wallet_balance_summary(
    total: impl Into<SharedString>,
    label: impl Into<SharedString>,
    total_color: gpui::Hsla,
    network: impl IntoElement,
) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .w_full()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex_auto()
                .flex_shrink_0()
                .min_w(rems(9.0))
                .flex()
                .flex_col()
                .child(
                    app_strong_text(total)
                        .text_size(VALUE_TEXT_SIZE)
                        .whitespace_nowrap()
                        .text_color(total_color),
                )
                .child(app_muted_text(label).text_size(CAPTION_TEXT_SIZE)),
        )
        .child(
            div()
                .w(relative(0.5))
                .min_w(rems(11.0))
                .flex_none()
                .ml_auto()
                .child(network),
        )
}
