#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) const fn matches_chain(row: &FeeRow, filter: FeesFilter<u64>) -> bool {
    matches_chain_id(row.chain_id, filter)
}

pub(super) const fn matches_chain_id(chain_id: u64, filter: FeesFilter<u64>) -> bool {
    match filter {
        FeesFilter::All => true,
        FeesFilter::One(id) => chain_id == id,
    }
}

pub(super) fn default_token_filter_for_chain(
    chain: FeesFilter<u64>,
    default_fee_tokens: &[(u64, Address)],
) -> FeesFilter<(u64, Address)> {
    let FeesFilter::One(chain_id) = chain else {
        return FeesFilter::All;
    };
    default_fee_tokens
        .iter()
        .find_map(|(default_chain_id, token)| {
            (*default_chain_id == chain_id).then_some(FeesFilter::One((chain_id, *token)))
        })
        .unwrap_or(FeesFilter::All)
}

pub(super) fn matches_token(row: &FeeRow, filter: FeesFilter<(u64, Address)>) -> bool {
    match filter {
        FeesFilter::All => true,
        FeesFilter::One((chain_id, addr)) => row.chain_id == chain_id && row.token_address == addr,
    }
}

pub(super) fn matches_broadcaster(row: &FeeRow, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let addr_hit = row.railgun_address.to_ascii_lowercase().contains(query);
    let id_hit = row
        .identifier
        .as_deref()
        .is_some_and(|i| i.to_ascii_lowercase().contains(query));
    addr_hit || id_hit
}

/// Downgrade an existing token filter to `All` when a new chain filter
/// rules out its chain. Returns the original filter otherwise.
pub(super) const fn cascade_reset_token(
    chain: FeesFilter<u64>,
    token: FeesFilter<(u64, Address)>,
) -> FeesFilter<(u64, Address)> {
    match (chain, token) {
        (FeesFilter::One(c), FeesFilter::One((tc, _))) if tc != c => FeesFilter::All,
        _ => token,
    }
}

/// Display label for a chain id in filter UI — falls back to the numeric id
/// for CLI-set chains outside the default broadcaster set.
pub(super) fn chain_label(chain_id: u64) -> String {
    chain_name(chain_id).map_or_else(|| chain_id.to_string(), str::to_owned)
}

/// Display label for a token in filter UI — symbol when known, short-hash fallback.
pub(super) fn token_label(chain_id: u64, addr: &Address) -> String {
    lookup_token(chain_id, addr).map_or_else(|| short_address(addr), |info| info.symbol.to_owned())
}

/// Token label for the filter menu, optionally prefixed with the chain name.
/// The prefix is added when the chain filter is `All`, so items from different
/// chains (`Ethereum: USDC` vs. `BSC: USDC`) can be told apart. When a single
/// chain is pinned the prefix is redundant and omitted.
pub(super) fn token_menu_label(chain_id: u64, addr: &Address, show_chain: bool) -> String {
    if show_chain {
        format!("{}: {}", chain_label(chain_id), token_label(chain_id, addr))
    } else {
        token_label(chain_id, addr)
    }
}

pub(super) fn icon_label_row(
    chain_id: u64,
    label: SharedString,
    icon_size: Pixels,
) -> impl IntoElement {
    let mut row = div().flex().items_center().gap_1();
    if let Some(path) = chain_icon_asset_path(chain_id) {
        row = row.child(img(path).size(icon_size).flex_none());
    }
    row.child(label)
}

pub(super) fn token_label_row(
    chain_id: u64,
    addr: &Address,
    label: SharedString,
    icon_size: Pixels,
) -> gpui::Div {
    let mut row = div().flex().items_center().gap_1();
    if let Some(path) = token_icon_asset_path(chain_id, addr) {
        row = row.child(img(path).size(icon_size).rounded_full().flex_none());
    }
    row.child(label)
}

pub(super) fn token_trigger_content(
    chain_id: Option<u64>,
    token: Option<(u64, Address)>,
    label: SharedString,
    icon_size: Pixels,
) -> impl IntoElement {
    let mut row = div().w_full().flex().items_center().gap_1().text_left();
    if let Some(chain_id) = chain_id
        && let Some(path) = chain_icon_asset_path(chain_id)
    {
        row = row.child(img(path).size(icon_size).flex_none());
    }
    if let Some((chain_id, addr)) = token
        && let Some(path) = token_icon_asset_path(chain_id, &addr)
    {
        row = row.child(img(path).size(icon_size).rounded_full().flex_none());
    }
    row.child(label)
}
