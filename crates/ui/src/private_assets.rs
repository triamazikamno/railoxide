//! Private wallet presentation. Callers own formatting, authority and actions.
use gpui::{
    AnyElement, ImageSource, IntoElement, ParentElement, SharedString, Styled, div, img,
    prelude::FluentBuilder as _, px, relative, rems, rgb,
};
use gpui_component::{Icon, Sizable as _};

use crate::{
    controls::{app_muted_text, app_strong_text},
    theme,
};

#[must_use]
pub fn private_balance(
    total: impl Into<SharedString>,
    compact: bool,
    actions: impl IntoElement,
) -> gpui::Div {
    if compact {
        return crate::wallet_balance::wallet_balance_summary(
            total,
            "Private balance",
            rgb(theme::WARNING).into(),
            actions,
        );
    }
    div()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_4()
        .px_4()
        .py_6()
        .child(app_strong_text("Total private balance").text_color(rgb(theme::TEXT_MUTED)))
        .child(
            div()
                .text_color(rgb(theme::WARNING))
                .text_size(px(44.0))
                .line_height(relative(theme::APP_TEXT_LINE_HEIGHT))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(total.into()),
        )
        .child(actions)
}

pub struct PrivateAssetRow {
    pub compact: bool,
    pub label: SharedString,
    pub icon: Option<ImageSource>,
    pub primary: SharedString,
    pub secondary: Option<SharedString>,
    pub pending_verification: Option<SharedString>,
    pub pending_incoming: Option<SharedString>,
    pub pending_outgoing: Option<SharedString>,
    pub actions: Option<AnyElement>,
}

impl PrivateAssetRow {
    #[must_use]
    pub fn into_div(self) -> gpui::Div {
        let compact = self.compact;
        let pending = [
            self.pending_verification.map(|amount| {
                if compact {
                    format!("{amount} not yet spendable")
                } else {
                    format!("{amount} {} not yet spendable", self.label)
                }
            }),
            self.pending_incoming.map(|amount| {
                if compact {
                    format!("+{amount} arriving")
                } else {
                    format!("+{amount} {} arriving", self.label)
                }
            }),
            self.pending_outgoing.map(|amount| {
                if compact {
                    format!("-{amount} leaving")
                } else {
                    format!("-{amount} {} leaving", self.label)
                }
            }),
        ];
        let pending_labels = pending
            .into_iter()
            .flatten()
            .map(|label| {
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_1()
                    .child(
                        Icon::empty()
                            .path("railgun/icons/clock.svg")
                            .xsmall()
                            .text_color(rgb(theme::TEXT_MUTED)),
                    )
                    .child(
                        app_muted_text(label)
                            .text_size(px(12.0))
                            .text_align(gpui::TextAlign::Right),
                    )
            })
            .collect::<Vec<_>>();
        let (inline_pending, below_pending) = if compact {
            (Vec::new(), pending_labels)
        } else {
            (pending_labels, Vec::new())
        };
        div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_4()
            .p_4()
            .rounded_lg()
            .bg(rgb(theme::SURFACE))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .when(compact, |row| {
                row.p_0()
                    .py_1()
                    .gap_x_2()
                    .gap_y_0()
                    .border_0()
                    .rounded_none()
                    .bg(gpui::transparent_black())
            })
            .child(
                div()
                    .flex_1()
                    .min_w(rems(8.0))
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(self.icon.map(|path| {
                        img(path)
                            .size(px(if compact { 24.0 } else { 32.0 }))
                            .rounded_full()
                            .flex_none()
                    }))
                    .child(
                        app_strong_text(self.label)
                            .flex_1()
                            .min_w_0()
                            .text_size(if compact {
                                theme::APP_TEXT_SIZE
                            } else {
                                theme::ASSET_SYMBOL_TEXT_SIZE
                            }),
                    ),
            )
            .children(self.actions)
            .child(
                div()
                    .min_w_0()
                    .max_w_full()
                    .ml_auto()
                    .flex()
                    .flex_col()
                    .items_end()
                    .child(
                        app_strong_text(self.primary)
                            .text_size(if compact {
                                theme::APP_TEXT_SIZE
                            } else {
                                theme::BALANCE_TEXT_SIZE
                            })
                            .text_color(rgb(theme::WARNING)),
                    )
                    .children(self.secondary.map(|value| {
                        app_muted_text(value)
                            .text_align(gpui::TextAlign::Right)
                            .when(compact, |text| text.text_size(px(12.0)))
                    }))
                    .children(inline_pending),
            )
            .when(!below_pending.is_empty(), |row| {
                row.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .justify_end()
                        .gap_x_3()
                        .children(below_pending),
                )
            })
    }
}

#[must_use]
pub fn private_message(message: impl Into<SharedString>, action: Option<AnyElement>) -> gpui::Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .child(app_muted_text(message))
                .children(action),
        )
}

#[must_use]
pub fn private_pending_status(
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    action: impl IntoElement,
) -> gpui::Div {
    let mut background = rgb(theme::WARNING);
    background.a = 0.08;
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(background)
        .p_3()
        .child(
            Icon::empty()
                .path("railgun/icons/clock.svg")
                .small()
                .text_color(rgb(theme::WARNING)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text(title))
                .children(detail.map(app_muted_text)),
        )
        .child(action)
}

pub struct PrivatePendingAmount {
    pub label: SharedString,
    pub amount: SharedString,
    pub shield_wait: Option<SharedString>,
}

pub struct PrivatePendingCategory {
    pub title: SharedString,
    pub count: SharedString,
    pub detail: SharedString,
    pub assets: Vec<PrivatePendingAmount>,
}

#[must_use]
pub fn private_pending_details(categories: Vec<PrivatePendingCategory>) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .children(categories.into_iter().map(|category| {
            div()
                .w_full()
                .flex()
                .items_start()
                .gap_3()
                .p_3()
                .child(
                    div()
                        .w(px(3.0))
                        .min_h(px(48.0))
                        .h_full()
                        .flex_none()
                        .rounded_full()
                        .bg(rgb(theme::WARNING)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            Icon::empty()
                                                .path("railgun/icons/clock.svg")
                                                .xsmall()
                                                .text_color(rgb(theme::WARNING)),
                                        )
                                        .child(app_strong_text(category.title)),
                                )
                                .child(app_muted_text(category.count).text_size(px(12.0))),
                        )
                        .child(app_muted_text(category.detail).text_size(px(12.0)))
                        .children(category.assets.into_iter().map(|asset| {
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(app_strong_text(asset.label).text_size(px(12.0)))
                                        .when_some(asset.shield_wait, |this, wait| {
                                            this.child(
                                                div()
                                                    .flex()
                                                    .items_start()
                                                    .gap_1()
                                                    .child(
                                                        Icon::empty()
                                                            .path("railgun/icons/clock.svg")
                                                            .xsmall()
                                                            .text_color(rgb(theme::WARNING)),
                                                    )
                                                    .child(
                                                        app_muted_text(wait)
                                                            .text_size(px(11.0))
                                                            .text_color(rgb(theme::WARNING)),
                                                    ),
                                            )
                                        }),
                                )
                                .child(app_muted_text(asset.amount).text_size(px(12.0)))
                        })),
                )
        }))
}
