//! Public address visuals shared by the desktop and browser frontend.
use crate::theme;
use gpui::{ParentElement, Pixels, Styled, div, px, rgb};
use qrcodegen::{QrCode, QrCodeEcc};
use std::ops::Range;

pub const PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE: usize = 5;
pub const PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT: usize = 25;
const PUBLIC_ACCOUNT_IDENTICON_SOURCE_COLUMNS: usize = 3;
pub const PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES: i32 = 4;
// QR colors are encoded image content with a fixed contrast contract.
const PUBLIC_ADDRESS_QR_FOREGROUND: u32 = 0x1e3c67;
const PUBLIC_ADDRESS_QR_BACKGROUND: u32 = 0xffffff;
const PUBLIC_ACCOUNT_IDENTICON_COLORS: [u32; 8] = [
    theme::PRIMARY,
    theme::SUCCESS,
    theme::WARNING_STRONG,
    theme::WARNING,
    theme::DANGER,
    theme::PURPLE,
    theme::BLUE,
    theme::OLIVE,
];

#[must_use]
pub fn public_account_identicon_pattern(
    address: &[u8; 20],
) -> [bool; PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT] {
    let mut pattern = [false; PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT];
    let mut has_foreground = false;
    for (row_index, row) in pattern
        .as_chunks_mut::<PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE>()
        .0
        .iter_mut()
        .enumerate()
    {
        for column in 0..PUBLIC_ACCOUNT_IDENTICON_SOURCE_COLUMNS {
            let bit_index = row_index * PUBLIC_ACCOUNT_IDENTICON_SOURCE_COLUMNS + column;
            let active = public_account_identicon_bit(address, bit_index);
            has_foreground |= active;
            row[column] = active;
            row[PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE - column - 1] = active;
        }
    }
    if !has_foreground {
        pattern[PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT / 2] = true;
    }
    pattern
}

const fn public_account_identicon_bit(address: &[u8; 20], bit_index: usize) -> bool {
    let bytes = address.as_slice();
    let byte = bytes[(bit_index * 7) % bytes.len()];
    let shift = (bit_index * 5) % u8::BITS as usize;
    ((byte >> shift) & 1) == 1
}

#[must_use]
pub fn public_account_identicon_color(address: &[u8; 20]) -> u32 {
    let bytes = address.as_slice();
    let color_index = usize::from(bytes[3] ^ bytes[7] ^ bytes[11] ^ bytes[15] ^ bytes[19])
        % PUBLIC_ACCOUNT_IDENTICON_COLORS.len();
    PUBLIC_ACCOUNT_IDENTICON_COLORS[color_index]
}
#[must_use]
pub fn render_public_address_qr_code(payload: &str, module_size: Pixels) -> gpui::Div {
    let Ok(qr) = QrCode::encode_text(payload, QrCodeEcc::Medium) else {
        return div()
            .p(px(14.0))
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::DANGER))
            .bg(rgb(theme::SURFACE_ELEVATED))
            .text_color(rgb(theme::DANGER))
            .child("QR code unavailable");
    };
    let mut grid = div()
        .flex()
        .flex_col()
        .flex_none()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_STRONG))
        .bg(rgb(PUBLIC_ADDRESS_QR_BACKGROUND))
        .p(px(6.0));
    let module_range = public_address_qr_module_range(qr.size());
    for y in module_range.clone() {
        let mut row = div().flex().flex_none();
        for x in module_range.clone() {
            let active = x >= 0 && y >= 0 && x < qr.size() && y < qr.size() && qr.get_module(x, y);
            row = row.child(div().size(module_size).flex_none().bg(rgb(if active {
                PUBLIC_ADDRESS_QR_FOREGROUND
            } else {
                PUBLIC_ADDRESS_QR_BACKGROUND
            })));
        }
        grid = grid.child(row);
    }
    grid
}

#[must_use]
pub const fn public_address_qr_module_range(qr_size: i32) -> Range<i32> {
    -PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES..qr_size + PUBLIC_ADDRESS_QR_QUIET_ZONE_MODULES
}

/// Complete Receive content. The owner validates the live identity immediately before copying.
#[must_use]
pub fn receive_address(
    label: Option<gpui::SharedString>,
    address: gpui::SharedString,
    warning: Option<gpui::SharedString>,
    copy_id: gpui::SharedString,
    module_size: Pixels,
    on_copy: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Div {
    use gpui::{InteractiveElement as _, StatefulInteractiveElement as _};
    use gpui_component::{IconName, button::ButtonVariants as _};
    let on_copy = std::rc::Rc::new(on_copy);
    let row_copy = on_copy.clone();
    div()
        .w_full()
        .flex()
        .flex_col()
        .items_center()
        .gap_4()
        .children(warning.map(|text| crate::controls::app_muted_text(text).w_full()))
        .children(label.map(crate::controls::app_strong_text))
        .child(render_public_address_qr_code(&address, module_size))
        .child(
            div()
                .id(gpui::SharedString::from(format!("{copy_id}-row")))
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .bg(rgb(theme::SURFACE_ELEVATED))
                .px_3()
                .py_2()
                .cursor_pointer()
                .on_click(move |_, window, cx| row_copy(window, cx))
                .child(
                    crate::controls::app_text(address)
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.0))
                        .font_family(theme::APP_MONO_FONT_FAMILY)
                        .text_color(rgb(theme::TEAL)),
                )
                .child(
                    crate::controls::app_button(copy_id, "Copy address")
                        .ghost()
                        .icon(IconName::Copy)
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            on_copy(window, cx);
                        }),
                ),
        )
}
