use gpui::{
    InteractiveElement, IntoElement, ObjectFit, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, StyledImage as _, Window, div, img,
    prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{Icon, IconName, scroll::ScrollableElement, tooltip::Tooltip};
use ui::clipboard::{clipboard_with_toast, copy_to_clipboard_with_custom_toast};
use ui::theme::{self, APP_FONT_FAMILY, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::release_check::RAILOXIDE_REPOSITORY_URL;

use crate::assets::{HEMATITE_HERO_PATH, HERO_WORDMARK_PATH, RailgunSocialIcon, WARM_GLOW_PATH};

use super::super::update_check::{UpdateCheckTrigger, update_check_button};
use super::super::{
    HERO_CARD_MAX_WIDTH, HERO_MEDIUM_BREAKPOINT, HERO_STAGE_MAX_WIDTH, HERO_WIDE_BREAKPOINT,
    rgb_with_alpha,
};

pub(in crate::root) const COPY_URL_TOOLTIP: &str = "Click to copy URL to clipboard";
pub(in crate::root) const LINK_COPIED_MESSAGE: &str = "Link copied to clipboard!";
pub(in crate::root) const TELEGRAM_URL: &str = "https://t.me/railoxide";

#[derive(Clone, Copy, Eq, PartialEq)]
enum WalletHeroLayout {
    Wide,
    Medium,
    Narrow,
}

fn wallet_hero_layout(window: &Window) -> WalletHeroLayout {
    let viewport = window.viewport_size();
    if viewport.width >= HERO_WIDE_BREAKPOINT && viewport.width >= viewport.height * 1.4 {
        WalletHeroLayout::Wide
    } else if viewport.width >= HERO_MEDIUM_BREAKPOINT {
        WalletHeroLayout::Medium
    } else {
        WalletHeroLayout::Narrow
    }
}

pub(in crate::root) fn render_wallet_hero_screen(
    window: &Window,
    card: gpui::AnyElement,
    update_check: Option<UpdateCheckTrigger>,
) -> gpui::Div {
    let viewport = window.viewport_size();
    let layout = wallet_hero_layout(window);
    let stage_width = (viewport.width - px(96.0))
        .max(px(0.0))
        .min(HERO_STAGE_MAX_WIDTH);
    let card_width = (viewport.width - px(48.0))
        .max(px(0.0))
        .min(HERO_CARD_MAX_WIDTH);
    let vertical_padding = match layout {
        WalletHeroLayout::Wide => px(32.0),
        WalletHeroLayout::Medium => px(40.0),
        WalletHeroLayout::Narrow => px(24.0),
    };
    let scroll_content_min_height = (viewport.height - vertical_padding * 2.0).max(px(0.0));

    let stage = if layout == WalletHeroLayout::Wide {
        div()
            .w(stage_width)
            .flex()
            .items_center()
            .gap_6()
            .child(
                render_wallet_brand_block(window, layout, update_check)
                    .w(px(560.0))
                    .flex_none(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .justify_end()
                    .child(div().w(card_width).child(card)),
            )
    } else {
        div()
            .w(card_width)
            .flex()
            .flex_col()
            .items_center()
            .gap_6()
            .child(render_wallet_brand_block(window, layout, update_check).w_full())
            .child(div().w_full().child(card))
    };

    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .bg(rgb(theme::BACKGROUND))
        .text_color(rgb(theme::TEXT))
        .font_family(APP_FONT_FAMILY)
        .text_size(APP_TEXT_SIZE)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .overflow_y_scrollbar()
                .child(
                    div()
                        .w_full()
                        .min_h(scroll_content_min_height)
                        .flex()
                        .items_center()
                        .justify_center()
                        .px(px(24.0))
                        .py(vertical_padding)
                        .child(stage),
                ),
        )
}

fn render_wallet_brand_block(
    window: &Window,
    layout: WalletHeroLayout,
    update_check: Option<UpdateCheckTrigger>,
) -> gpui::Div {
    let viewport = window.viewport_size();
    let show_mineral = layout != WalletHeroLayout::Narrow;
    let mineral_size = match layout {
        WalletHeroLayout::Wide => (viewport.height * 0.42).min(px(500.0)).max(px(360.0)),
        WalletHeroLayout::Medium => (viewport.width * 0.24).min(px(320.0)).max(px(210.0)),
        WalletHeroLayout::Narrow => px(0.0),
    };
    let wordmark_width = match layout {
        WalletHeroLayout::Wide => px(400.0),
        WalletHeroLayout::Medium => (viewport.width * 0.44).min(px(360.0)).max(px(260.0)),
        WalletHeroLayout::Narrow => (viewport.width * 0.66).min(px(360.0)).max(px(220.0)),
    };
    let wordmark_height = wordmark_width * (23.0 / 166.0);
    let art_size = mineral_size * 1.5;
    let horizontal_mineral_offset = (art_size - mineral_size) / 2.0;
    let vertical_glow_offset = (mineral_size - art_size) / 2.0;

    div()
        .flex()
        .flex_col()
        .items_center()
        .gap_6()
        .when(show_mineral, |this| {
            this.child(
                div()
                    .relative()
                    .w(art_size)
                    .h(mineral_size)
                    .child(
                        img(WARM_GLOW_PATH)
                            .absolute()
                            .top(vertical_glow_offset)
                            .left_0()
                            .size(art_size)
                            .object_fit(ObjectFit::Fill),
                    )
                    .child(
                        img(HEMATITE_HERO_PATH)
                            .absolute()
                            .top_0()
                            .left(horizontal_mineral_offset)
                            .size(mineral_size)
                            .object_fit(ObjectFit::Contain),
                    ),
            )
        })
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_2()
                .child(
                    img(HERO_WORDMARK_PATH)
                        .w(wordmark_width)
                        .h(wordmark_height)
                        .object_fit(ObjectFit::Contain),
                )
                .child(render_wallet_build_metadata(update_check)),
        )
}

fn render_wallet_build_metadata(update_check: Option<UpdateCheckTrigger>) -> gpui::Div {
    let build_label = wallet_build_label();

    div()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .gap_1()
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .gap_1()
                .child(
                    div()
                        .font_family(APP_MONO_FONT_FAMILY)
                        .text_size(px(12.0))
                        .line_height(px(16.0))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(build_label.clone()),
                )
                .child(clipboard_with_toast(
                    "wallet-hero-build-info-copy",
                    build_label,
                ))
                .children(
                    update_check
                        .map(|trigger| update_check_button("wallet-hero-update-check", trigger)),
                ),
        )
        .child(
            div()
                .w_full()
                .flex()
                .justify_center()
                .gap_1()
                .child(render_wallet_social_copy_button(
                    "wallet-hero-repository-url-copy",
                    Icon::new(IconName::Github).size_4(),
                    RAILOXIDE_REPOSITORY_URL,
                ))
                .child(render_wallet_social_copy_button(
                    "wallet-hero-telegram-url-copy",
                    Icon::new(RailgunSocialIcon::Telegram).size_4(),
                    TELEGRAM_URL,
                )),
        )
}

fn render_wallet_social_copy_button(
    id: &'static str,
    icon: impl IntoElement,
    url: &'static str,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_color(rgb(theme::TEXT_MUTED))
        .cursor_pointer()
        .hover(|this| {
            this.bg(rgb_with_alpha(theme::SURFACE_HOVER, 0.24))
                .text_color(rgb(theme::TEXT))
        })
        .tooltip(|window, cx| Tooltip::new(COPY_URL_TOOLTIP).build(window, cx))
        .on_click(move |_event, window, cx| {
            copy_to_clipboard_with_custom_toast(url, LINK_COPIED_MESSAGE, window, cx);
        })
        .child(icon)
}

pub(in crate::root) fn wallet_build_label() -> SharedString {
    SharedString::from(format!(
        "v{} {}",
        env!("CARGO_PKG_VERSION"),
        option_env!("RAILOXIDE_GIT_SHORT_HASH").unwrap_or("unknown")
    ))
}
