use alloy::primitives::Address;
use gpui::{Pixels, SharedString, Styled, px};

pub(in crate::root) fn render_public_address_qr_dialog_content(
    label: Option<SharedString>,
    address: SharedString,
    warning: Option<SharedString>,
    copy_id: SharedString,
    content_width: Pixels,
    on_copy: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Div {
    ui::public_address::receive_address(label, address, warning, copy_id, px(6.0), on_copy)
        .w(content_width)
}

pub(in crate::root) fn public_address_qr_payload(address: Address) -> String {
    format!("{address:#x}")
}
