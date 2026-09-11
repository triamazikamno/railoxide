use alloy::primitives::Address;
use gpui::{
    InteractiveElement, ParentElement, Pixels, SharedString, StatefulInteractiveElement, Styled,
    div, px, rgb,
};
use gpui_component::tooltip::Tooltip;
use ui::clipboard::{clipboard_with_toast, copy_to_clipboard_with_toast};
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};

use crate::root::ui_helpers::rgb_with_alpha;

const PUBLIC_ADDRESS_QR_MODULE_SIZE: Pixels = px(6.0);

pub(in crate::root) fn render_public_address_qr_dialog_content(
    label: Option<SharedString>,
    address: SharedString,
    warning: Option<SharedString>,
    copy_id: SharedString,
    content_width: Pixels,
) -> gpui::Div {
    let address_copy_value = address.clone();
    let copy_row_id = SharedString::from(format!("{}-row", copy_id.as_ref()));
    div()
        .w(content_width)
        .flex()
        .flex_col()
        .items_center()
        .gap_4()
        .children(warning.map(|warning| {
            div()
                .w_full()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .bg(rgb_with_alpha(theme::PRIMARY, 0.08))
                .p(px(10.0))
                .text_color(rgb(theme::TEXT_MUTED))
                .text_size(APP_TEXT_SIZE)
                .line_height(px(18.0))
                .child(warning)
        }))
        .children(label.map(|label| {
            div()
                .text_color(rgb(theme::TEXT))
                .text_size(theme::ACCOUNT_LABEL_TEXT_SIZE)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(label)
        }))
        .child(render_public_address_qr_code(address.as_ref()))
        .child(
            div()
                .id(copy_row_id)
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .bg(rgb(theme::SURFACE_ELEVATED))
                .px(px(10.0))
                .py(px(8.0))
                .cursor_pointer()
                .hover(|this| {
                    this.bg(rgb(theme::SURFACE_HOVER_SUBTLE))
                        .border_color(rgb(theme::BORDER_STRONG))
                })
                .tooltip(|window, cx| Tooltip::new("Copy address").build(window, cx))
                .on_click(move |_event, window, cx| {
                    copy_to_clipboard_with_toast(address_copy_value.clone(), window, cx);
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(rgb(theme::TEXT))
                        .text_size(px(12.0))
                        .font_family(APP_MONO_FONT_FAMILY)
                        .line_height(px(17.0))
                        .child(address.clone()),
                )
                .child(clipboard_with_toast(copy_id, address)),
        )
}

fn render_public_address_qr_code(payload: &str) -> gpui::Div {
    ui::public_address::render_public_address_qr_code(payload, PUBLIC_ADDRESS_QR_MODULE_SIZE)
}

pub(in crate::root) fn public_address_qr_payload(address: Address) -> String {
    format!("{address:#x}")
}
