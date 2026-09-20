//! Identity rows used by the desktop and browser wallet selectors.
use gpui::{IntoElement, ParentElement, SharedString, Styled, div, img, px};

use crate::{controls::app_text, icons};

#[must_use]
pub fn chain_label_row(label: impl Into<SharedString>, icon: Option<&'static str>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .size(px(16.0))
                .flex_none()
                .children(icon.map(|path| img(path).size_full())),
        )
        .child(app_text(label))
}

#[must_use]
pub fn wallet_label_row(label: impl Into<SharedString>, device: Option<&str>) -> gpui::Div {
    let icon = match device {
        Some("ledger") => img("railgun/icons/ledger-logo-short-white.svg")
            .h(px(19.0))
            .flex_none()
            .into_any_element(),
        Some("trezor") => img("railgun/icons/trezor-symbol-white-rgb.svg")
            .h(px(22.0))
            .flex_none()
            .into_any_element(),
        _ => img(icons::wallet_icon_path())
            .size(px(22.0))
            .flex_none()
            .into_any_element(),
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .child(
            div()
                .size(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(icon),
        )
        .child(app_text(label).min_w_0().truncate())
}
