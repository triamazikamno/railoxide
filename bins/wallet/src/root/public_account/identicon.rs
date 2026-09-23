use alloy::primitives::Address;
use gpui::{ParentElement, Styled, div, rgb};

pub(in crate::root) use ui::public_address::{
    PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT, PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE,
};
pub(in crate::root) fn public_account_identicon_pattern(
    address: &Address,
) -> [bool; PUBLIC_ACCOUNT_IDENTICON_CELL_COUNT] {
    ui::public_address::public_account_identicon_pattern(address.as_ref())
}
pub(in crate::root) fn public_account_identicon_color(address: &Address) -> u32 {
    ui::public_address::public_account_identicon_color(address.as_ref())
}

pub(super) fn render_public_account_row_identicon(address: &Address) -> gpui::Div {
    let pattern = public_account_identicon_pattern(address);
    let color = public_account_identicon_color(address);
    let size = super::list::dimension(super::list::IDENTICON_WIDTH);
    let cell = size / 5.0;
    div().size(size).flex_none().flex().flex_col().children(
        pattern
            .as_chunks::<PUBLIC_ACCOUNT_IDENTICON_GRID_SIZE>()
            .0
            .iter()
            .map(|row| {
                div().flex().children(row.iter().map(|active| {
                    // Identicon color is address-derived data, shared with the other account views.
                    let cell = div().size(cell);
                    if *active { cell.bg(rgb(color)) } else { cell }
                }))
            }),
    )
}
