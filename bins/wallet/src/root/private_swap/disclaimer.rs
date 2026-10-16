//! How swaps work: the third-party protocols that run swaps, when funds leave the user's
//! control, and where to follow an order that gets stuck. It opens above the swap dialog each
//! time that dialog opens until "I understand" is pressed, and from the question-mark button next
//! to My orders. Its links are copied, never opened.

use gpui::{
    App, Context, FontWeight, InteractiveElement as _, ParentElement as _, Pixels, SharedString,
    Styled as _, StyledImage as _, WeakEntity, Window, div, img, px, rgb,
};
use gpui_component::{
    Icon, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
};
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text, app_text};
use ui::theme;
use wallet_ops::vault::BridgeProvider;

use super::PrivateSwapsView;
use super::dialog::provider_mark_path;
use super::model::provider_name;
use crate::assets::{CIRCLE_QUESTION_MARK_ICON_PATH, COW_DAO_LIGHT_PATH};
use crate::root::{WalletRoot, dialog_max_height, secondary_dialog_content_width};

const DISCLAIMER_WIDTH: Pixels = px(600.0);
const TITLE: &str = "How swaps work";

/// A protocol that holds or moves funds during a swap.
struct Protocol {
    id: &'static str,
    mark: &'static str,
    name: &'static str,
    role: &'static str,
    website: &'static str,
    explorer: &'static str,
}

const PROTOCOLS: [Protocol; 3] = [
    Protocol {
        id: "cow",
        mark: COW_DAO_LIGHT_PATH,
        name: "CoW Protocol",
        role: "Trades every swap",
        website: "https://cow.fi",
        explorer: "https://explorer.cow.fi",
    },
    Protocol {
        id: "across",
        mark: provider_mark_path(BridgeProvider::Across),
        name: provider_name(BridgeProvider::Across),
        role: "Bridges to other networks",
        website: "https://across.to",
        explorer: "https://across.to/transfers",
    },
    Protocol {
        id: "near-intents",
        mark: provider_mark_path(BridgeProvider::NearIntents),
        name: provider_name(BridgeProvider::NearIntents),
        role: "Bridges routes Across doesn't serve",
        website: "https://near-intents.org",
        explorer: "https://explorer.near-intents.org",
    },
];

impl PrivateSwapsView {
    /// Whether "I understand" was pressed. Without a wallet there is no swap to explain.
    pub(super) fn swap_disclaimer_acknowledged(&self, cx: &App) -> bool {
        self.root
            .upgrade()
            .is_none_or(|root| root.read(cx).ui_state.swap_disclaimer_acknowledged)
    }

    /// Open the disclaimer above the swap dialog. The dialog layer gives the focus back to the
    /// swap dialog when it closes.
    pub(super) fn open_swap_disclaimer(&self, window: &mut Window, cx: &mut App) {
        let root = self.root.clone();
        window.open_dialog(cx, move |dialog, window, _| {
            let width = (window.viewport_size().width * 0.92).min(DISCLAIMER_WIDTH);
            let content_width = secondary_dialog_content_width(width);
            dialog
                .w(width)
                .max_h(dialog_max_height(window))
                .on_ok(|_, _, _| false)
                .title(app_strong_text(TITLE))
                // The footer stays in view while the body scrolls.
                .child(render_body().w(content_width).min_w_0())
                .footer(
                    render_footer(root.clone())
                        .w(content_width)
                        .min_w_0()
                        .pt_2(),
                )
        });
    }

    /// The swap form's question-mark button, which opens the disclaimer.
    pub(super) fn render_disclaimer_button(cx: &Context<'_, Self>) -> Button {
        app_button_base("swap-disclaimer-open")
            .primary()
            .small()
            .flex_none()
            .icon(Icon::empty().path(CIRCLE_QUESTION_MARK_ICON_PATH))
            .accessibility_label(TITLE)
            .tooltip(TITLE)
            .debug_selector(|| "swap-disclaimer-open".into())
            .on_click(cx.listener(|this, _, window, cx| {
                this.open_swap_disclaimer(window, cx);
            }))
    }
}

fn render_body() -> gpui::Div {
    div()
        .debug_selector(|| "swap-disclaimer".into())
        .flex()
        .flex_col()
        .gap_4()
        .child(app_text(
            "Swaps run on third-party protocols. RailOxide places and tracks your orders, but it \
             doesn't control these protocols and can't move or refund funds they hold.",
        ))
        .child(section(
            "Protocols",
            PROTOCOLS.iter().map(protocol_row).collect::<Vec<_>>(),
        ))
        .child(section(
            "When funds leave your control",
            vec![
                app_text(
                    "A same-network swap settles in one atomic transaction. Your tokens stay \
                     put until it settles, and an expired order moves nothing.",
                ),
                app_text(
                    "A cross-network swap hands the proceeds to a bridge once the trade \
                     settles. The bridge holds them until it delivers.",
                ),
                list_item(
                    "•",
                    "Across usually delivers within minutes. If no one fills the deposit, it's \
                     refunded once it expires, typically within a few hours. The refund goes to \
                     your stealth account on the source network, which only you control.",
                ),
                list_item(
                    "•",
                    "NEAR Intents holds funds with a bridge operator until delivery. Refunds \
                     depend on 1Click and aren't guaranteed.",
                ),
            ],
        ))
        .child(section(
            "If an order gets stuck",
            vec![
                list_item(
                    "1.",
                    "Open the swap in My orders and copy its order ID, transaction or deposit \
                     address.",
                ),
                list_item("2.", "Look it up in the protocol's explorer."),
                list_item(
                    "3.",
                    "Still unclear? Contact that protocol's support. Only the protocol holding \
                     your funds can release or refund them.",
                ),
            ],
        ))
}

fn section(heading: &'static str, children: Vec<gpui::Div>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            app_muted_text(heading.to_uppercase())
                .text_xs()
                .font_weight(FontWeight::BOLD),
        )
        .children(children)
}

/// A bullet or numbered step, its text indented past the marker.
fn list_item(marker: &'static str, text: &'static str) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap_1()
        .child(app_muted_text(marker).w(px(16.0)).flex_none())
        .child(app_text(text).flex_1().min_w_0())
}

/// The protocol's mark, name and role, and its copyable links.
fn protocol_row(protocol: &Protocol) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap(px(10.0))
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE))
        .child(
            img(protocol.mark)
                .size(px(20.0))
                .flex_none()
                .object_fit(gpui::ObjectFit::Contain),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_x_2()
                        .child(app_strong_text(protocol.name))
                        .child(app_muted_text(protocol.role)),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1p5()
                        .child(link_chip(protocol, "Website", protocol.website))
                        .child(link_chip(protocol, "Explorer", protocol.explorer)),
                ),
        )
}

/// A link shown without its scheme, with a button that copies the full URL.
fn link_chip(protocol: &Protocol, kind: &'static str, url: &'static str) -> gpui::Div {
    let noun = kind.to_lowercase();
    let copy = format!("Copy {} {noun}", protocol.name);
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1p5()
        .pl_2()
        .rounded_sm()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SETTINGS_INPUT_SURFACE))
        .child(app_muted_text(kind).flex_none().text_xs())
        .child(
            app_text(url.trim_start_matches("https://"))
                .min_w_0()
                .truncate()
                .text_xs(),
        )
        .child(
            clipboard_with_toast(
                SharedString::from(format!("swap-disclaimer-{}-{noun}", protocol.id)),
                url,
            )
            .tooltip(copy.clone())
            .accessibility_label(copy),
        )
}

/// How to reopen the disclaimer, and "I understand", which closes it for good.
fn render_footer(root: WeakEntity<WalletRoot>) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(reopen_hint().min_w_0())
        .child(
            app_button(
                "swap-disclaimer-acknowledge",
                "I understand, don't show again",
            )
            .primary()
            .ml_auto()
            .flex_none()
            .debug_selector(|| "swap-disclaimer-acknowledge".into())
            .on_click(move |_, window, cx| {
                let _ = root.update(cx, |root, _| {
                    if !root.ui_state.swap_disclaimer_acknowledged {
                        root.ui_state.swap_disclaimer_acknowledged = true;
                        root.save_ui_state();
                    }
                });
                window.close_dialog(cx);
            }),
        )
}

/// Where the dialog reopens from, with the question-mark button in miniature.
fn reopen_hint() -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .child(app_muted_text("Reopen it any time from").text_xs())
        .child(
            div()
                .flex_none()
                .size(px(16.0))
                .rounded_sm()
                .bg(rgb(theme::PRIMARY))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::empty()
                        .path(CIRCLE_QUESTION_MARK_ICON_PATH)
                        .with_size(px(12.0))
                        .text_color(rgb(theme::PRIMARY_FOREGROUND)),
                ),
        )
        .child(app_muted_text("next to My orders.").text_xs())
}
