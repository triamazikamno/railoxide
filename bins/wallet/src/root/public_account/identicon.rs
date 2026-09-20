use alloy::primitives::Address;
use gpui::{ParentElement, Pixels, Styled, div, px, rgb};

const PUBLIC_ACCOUNT_IDENTICON_SIZE: Pixels = px(40.0);
const PUBLIC_ACCOUNT_IDENTICON_CELL_SIZE: Pixels = px(8.0);
pub(in crate::root) use ui::public_address::{
    PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT, PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE,
};
pub(in crate::root) fn render_public_account_identicon(address: &Address) -> gpui::Div {
    let pattern = public_account_identicon_pattern(address);
    let foreground = public_account_identicon_color(address);
    let mut icon = div()
        .size(PUBLIC_ACCOUNT_IDENTICON_SIZE)
        .flex_none()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_0();
    for row in pattern.as_chunks::<PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE>().0 {
        let mut row_div = div().flex().gap_0();
        for active in row {
            let cell = div().size(PUBLIC_ACCOUNT_IDENTICON_CELL_SIZE);
            row_div = row_div.child(if *active {
                cell.bg(rgb(foreground))
            } else {
                cell
            });
        }
        icon = icon.child(row_div);
    }
    icon
}

pub(in crate::root) fn public_account_identicon_pattern(
    address: &Address,
) -> [bool; PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT] {
    ui::public_address::public_account_identicon_pattern(address.as_ref())
}
pub(in crate::root) fn public_account_identicon_color(address: &Address) -> u32 {
    ui::public_address::public_account_identicon_color(address.as_ref())
}
