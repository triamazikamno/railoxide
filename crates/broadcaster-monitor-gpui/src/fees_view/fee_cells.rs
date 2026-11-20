#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn raw_fee_label(row: &FeeRow) -> String {
    lookup_token(row.chain_id, &row.token_address).map_or_else(
        || row.fee.to_string(),
        |info| format_token_amount(row.fee, info.decimals),
    )
}

pub(super) fn render_fee_bonus_cell(
    row: &FeeRow,
    fee_anchor_lookup: &FeeAnchorLookup,
) -> gpui::Div {
    match fee_bonus_bps(row, fee_anchor_lookup) {
        Some(bonus_bps) => div()
            .text_color(rgb(theme::WARNING))
            .child(SharedString::from(format_bonus_bps(bonus_bps))),
        None => div()
            .text_color(rgb(theme::TEXT_MUTED))
            .child(SharedString::from("n/a")),
    }
}

pub(super) fn fee_bonus_bps(row: &FeeRow, fee_anchor_lookup: &FeeAnchorLookup) -> Option<i128> {
    let anchor = fee_anchor_lookup(row.chain_id, row.token_address)?;
    fee_bonus_bps_from_anchor(row.fee, anchor)
}

pub(super) fn fee_bonus_bps_from_anchor(fee: U256, anchor: U256) -> Option<i128> {
    if anchor.is_zero() {
        return None;
    }
    let bps = fee.checked_mul(U256::from(10_000))?.checked_div(anchor)?;
    i128::try_from(bps).ok().map(|bps| bps - 10_000)
}

pub(super) fn format_bonus_bps(bonus_bps: i128) -> String {
    let sign = if bonus_bps >= 0 { "+" } else { "-" };
    let abs_bps = bonus_bps.checked_abs().unwrap_or(i128::MAX);
    let tenths = (abs_bps + 5) / 10;
    format!("{sign}{}.{:01}%", tenths / 10, tenths % 10)
}

pub(super) fn compare_fee_bonus_rows(
    a: &FeeRow,
    b: &FeeRow,
    sort: ColumnSort,
    fee_anchor_lookup: &FeeAnchorLookup,
) -> Ordering {
    let a_bonus = fee_bonus_bps(a, fee_anchor_lookup);
    let b_bonus = fee_bonus_bps(b, fee_anchor_lookup);
    compare_optional_bonus(a_bonus, b_bonus, sort)
        .then_with(|| a.chain_id.cmp(&b.chain_id))
        .then_with(|| a.railgun_address.cmp(&b.railgun_address))
        .then_with(|| a.token_address.cmp(&b.token_address))
}

pub(super) fn compare_optional_bonus(
    a: Option<i128>,
    b: Option<i128>,
    sort: ColumnSort,
) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => match sort {
            ColumnSort::Ascending => a.cmp(&b),
            ColumnSort::Descending => b.cmp(&a),
            ColumnSort::Default => Ordering::Equal,
        },
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

pub(super) fn fee_row_group(row_ix: usize) -> SharedString {
    SharedString::from(format!("broadcaster-fee-row-group-{row_ix}"))
}

pub(super) fn render_preference_cell(
    row_ix: usize,
    row: &FeeRow,
    hooks: &BroadcasterPreferenceHooks,
) -> gpui::Div {
    let status = hooks.status(row.railgun_address.as_ref());
    let row_group = fee_row_group(row_ix);
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap_1()
        .child(render_preference_toggle(
            row_ix,
            "favorite",
            BroadcasterPreferenceIcon::Favorite,
            "Favorite broadcaster",
            matches!(status, BroadcasterPreferenceStatus::Favorite),
            theme::WARNING,
            row.railgun_address.to_string(),
            Arc::clone(&hooks.toggle_favorite),
            row_group.clone(),
        ))
        .child(render_preference_toggle(
            row_ix,
            "banned",
            BroadcasterPreferenceIcon::Banned,
            "Ban broadcaster",
            matches!(status, BroadcasterPreferenceStatus::Banned),
            theme::DANGER,
            row.railgun_address.to_string(),
            Arc::clone(&hooks.toggle_banned),
            row_group,
        ))
}

fn render_preference_toggle(
    row_ix: usize,
    action: &'static str,
    icon: BroadcasterPreferenceIcon,
    tooltip: &'static str,
    active: bool,
    color: u32,
    address: String,
    toggle: Arc<PreferenceToggleFn>,
    row_group: SharedString,
) -> impl IntoElement {
    let bg = if active {
        rgb_with_alpha(color, 0.16)
    } else {
        rgb_with_alpha(theme::SURFACE, 0.0)
    };
    let border = if active { color } else { theme::BORDER_SUBTLE };
    let text = if active { color } else { theme::TEXT_MUTED };
    div()
        .id(SharedString::from(format!(
            "broadcaster-preference-{action}-{row_ix}"
        )))
        .w(px(28.0))
        .h(px(26.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .border_1()
        .border_color(rgb_with_alpha(border, 0.0))
        .bg(bg)
        .text_color(rgb(text))
        .cursor_pointer()
        .group_hover(row_group, move |this| this.border_color(rgb(border)))
        .hover(move |this| this.bg(rgb_with_alpha(color, 0.12)))
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .on_click(move |_event, window, cx| {
            cx.stop_propagation();
            toggle(address.clone(), window, cx);
        })
        .child(Icon::new(icon).size_3().text_color(rgb(text)))
}

fn rgb_with_alpha(hex: u32, alpha: f32) -> gpui::Rgba {
    let mut color = rgb(hex);
    color.a = alpha;
    color
}
