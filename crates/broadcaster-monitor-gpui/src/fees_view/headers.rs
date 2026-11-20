#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn render_chain_header(
    select: &Entity<SelectState<Vec<FeesChainFilterItem>>>,
) -> impl IntoElement {
    div()
        .id("fees-chain-filter")
        .size_full()
        .on_click(|_event, _window, cx| cx.stop_propagation())
        .child(
            Select::new(select)
                .appearance(false)
                .xsmall()
                .w_full()
                .menu_width(px(170.0)),
        )
}

pub(super) fn render_token_header(
    select: &Entity<SelectState<SearchableVec<FeesTokenFilterItem>>>,
) -> impl IntoElement {
    div()
        .id("fees-token-filter")
        .size_full()
        .on_click(|_event, _window, cx| cx.stop_propagation())
        .child(
            Select::new(select)
                .appearance(false)
                .xsmall()
                .w_full()
                .menu_width(px(240.0))
                .search_placeholder("Search tokens"),
        )
}

pub(super) fn render_fee_header(
    sort: ColumnSort,
    table: Entity<TableState<FeesDelegate>>,
) -> impl IntoElement {
    // `⇅` when no sort is active (affordance only); up/down triangles
    // otherwise. Using inline text arrows keeps this independent of the
    // lib's built-in sort icon, which has a cell-right-corner hitbox
    // that's ~14px tall and hard to target.
    let arrow = match sort {
        ColumnSort::Default => "⇅",
        ColumnSort::Ascending => "▲",
        ColumnSort::Descending => "▼",
    };
    div()
        .id("fees-fee-header")
        .size_full()
        .cursor_pointer()
        .child(SharedString::from(format!("fee {arrow}")))
        .on_click(move |_event, _window, cx| {
            cx.stop_propagation();
            table.update(cx, |state, cx| {
                state.delegate_mut().toggle_fee_sort();
                cx.notify();
            });
        })
}

pub(super) fn render_bonus_header(
    sort: ColumnSort,
    table: Entity<TableState<FeesDelegate>>,
) -> impl IntoElement {
    let arrow = match sort {
        ColumnSort::Default => "⇅",
        ColumnSort::Ascending => "▲",
        ColumnSort::Descending => "▼",
    };
    div()
        .id("fees-bonus-header")
        .size_full()
        .cursor_pointer()
        .child(SharedString::from(format!("bonus % {arrow}")))
        .on_click(move |_event, _window, cx| {
            cx.stop_propagation();
            table.update(cx, |state, cx| {
                state.delegate_mut().toggle_bonus_sort();
                cx.notify();
            });
        })
}
