use std::cmp::Ordering;
use std::sync::Arc;
use std::time::SystemTime;

use alloy::primitives::{Address, U256};
use gpui::{
    App, Context, Div, Entity, InteractiveElement, IntoElement, ParentElement, Pixels,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div, img, px, rgb,
};
use gpui_component::{
    Icon, IconNamed, Sizable, Size,
    input::{Input, InputState},
    select::{SearchableVec, Select, SelectItem, SelectState},
    table::{Column, ColumnSort, TableDelegate, TableState},
    tooltip::Tooltip,
};

use broadcaster_monitor::FeeRow;
use railgun_ui::{
    chain_icon_asset_path, chain_name, format_broadcaster_address_label, format_token_amount,
    lookup_token, short_address, token_icon_asset_path,
};
use ui::clipboard::clipboard_with_toast;
use ui::format::{format_compact_duration, format_relative_age};
use ui::icons;
use ui::theme::{self, APP_MONO_FONT_FAMILY};

pub type FeeAnchorLookup = Arc<dyn Fn(u64, Address) -> Option<U256> + Send + Sync>;

type PreferenceStatusFn = dyn Fn(&str) -> BroadcasterPreferenceStatus;
type PreferenceToggleFn = dyn Fn(String, &mut Window, &mut App);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BroadcasterPreferenceStatus {
    Neutral,
    Favorite,
    Banned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BroadcasterPreferenceIcon {
    Favorite,
    Banned,
}

impl IconNamed for BroadcasterPreferenceIcon {
    fn path(self) -> SharedString {
        match self {
            Self::Favorite => icons::star_icon_path(),
            Self::Banned => icons::ban_icon_path(),
        }
        .into()
    }
}

#[derive(Clone)]
pub struct BroadcasterPreferenceHooks {
    status: Arc<PreferenceStatusFn>,
    toggle_favorite: Arc<PreferenceToggleFn>,
    toggle_banned: Arc<PreferenceToggleFn>,
}

impl BroadcasterPreferenceHooks {
    #[must_use]
    pub fn new(
        status: impl Fn(&str) -> BroadcasterPreferenceStatus + 'static,
        toggle_favorite: impl Fn(String, &mut Window, &mut App) + 'static,
        toggle_banned: impl Fn(String, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            status: Arc::new(status),
            toggle_favorite: Arc::new(toggle_favorite),
            toggle_banned: Arc::new(toggle_banned),
        }
    }

    fn status(&self, address: &str) -> BroadcasterPreferenceStatus {
        (self.status)(address)
    }
}

/// A single-select filter: either "All" (no filter) or a specific value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeesFilter<T: Copy + PartialEq> {
    All,
    One(T),
}

#[derive(Clone, Copy)]
pub(crate) struct FeesChainFilterItem {
    value: FeesFilter<u64>,
}

impl FeesChainFilterItem {
    pub(crate) const fn all() -> Self {
        Self {
            value: FeesFilter::All,
        }
    }

    pub(crate) const fn chain(chain_id: u64) -> Self {
        Self {
            value: FeesFilter::One(chain_id),
        }
    }
}

impl SelectItem for FeesChainFilterItem {
    type Value = FeesFilter<u64>;

    fn title(&self) -> SharedString {
        match self.value {
            FeesFilter::All => SharedString::from("All"),
            FeesFilter::One(chain_id) => SharedString::from(chain_label(chain_id)),
        }
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(self.render_title(px(16.0)).into_any_element())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.render_title(px(16.0))
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

impl FeesChainFilterItem {
    fn render_title(&self, icon_size: Pixels) -> gpui::AnyElement {
        match self.value {
            FeesFilter::All => div().child("All").into_any_element(),
            FeesFilter::One(chain_id) => icon_label_row(
                chain_id,
                SharedString::from(chain_label(chain_id)),
                icon_size,
            )
            .into_any_element(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FeesTokenFilterItem {
    value: FeesFilter<(u64, Address)>,
    show_chain: bool,
}

impl FeesTokenFilterItem {
    pub(crate) const fn all(show_chain: bool) -> Self {
        Self {
            value: FeesFilter::All,
            show_chain,
        }
    }

    const fn token(chain_id: u64, address: Address, show_chain: bool) -> Self {
        Self {
            value: FeesFilter::One((chain_id, address)),
            show_chain,
        }
    }
}

impl SelectItem for FeesTokenFilterItem {
    type Value = FeesFilter<(u64, Address)>;

    fn title(&self) -> SharedString {
        match self.value {
            FeesFilter::All => SharedString::from("All"),
            FeesFilter::One((chain_id, address)) => {
                SharedString::from(token_menu_label(chain_id, &address, self.show_chain))
            }
        }
    }

    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(self.render_title(px(16.0)).into_any_element())
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        self.render_title(px(16.0))
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.to_ascii_lowercase();
        match self.value {
            FeesFilter::All => "all".contains(&query),
            FeesFilter::One((chain_id, address)) => {
                chain_label(chain_id).to_ascii_lowercase().contains(&query)
                    || token_label(chain_id, &address)
                        .to_ascii_lowercase()
                        .contains(&query)
                    || address.to_string().to_ascii_lowercase().contains(&query)
            }
        }
    }
}

impl FeesTokenFilterItem {
    fn render_title(&self, icon_size: Pixels) -> gpui::AnyElement {
        match self.value {
            FeesFilter::All => div().child("All").into_any_element(),
            FeesFilter::One((chain_id, address)) => token_trigger_content(
                self.show_chain.then_some(chain_id),
                Some((chain_id, address)),
                SharedString::from(token_menu_label(chain_id, &address, self.show_chain)),
                icon_size,
            )
            .into_any_element(),
        }
    }
}

/// `TableDelegate` backing the fees pane. Owns the full sorted row snapshot
/// (`all_rows`) plus the post-filter visible subset (`rows`) that
/// `rows_count` / `render_td` see. Header cells for the first three columns
/// are overridden in `render_th` to host per-column filter widgets.
pub(crate) struct FeesDelegate {
    all_rows: Arc<[FeeRow]>,
    rows: Arc<[FeeRow]>,
    columns: Vec<Column>,
    fee_anchor_lookup: FeeAnchorLookup,
    preference_hooks: Option<BroadcasterPreferenceHooks>,
    chain_select: Entity<SelectState<Vec<FeesChainFilterItem>>>,
    chain_filter: FeesFilter<u64>,
    /// Lower-cased substring query for the broadcaster filter (empty = no filter).
    broadcaster_query: Arc<str>,
    /// Owned by the delegate so `render_th(col=1)` can render the live `Input`.
    broadcaster_input: Entity<InputState>,
    token_filter: FeesFilter<(u64, Address)>,
    default_fee_tokens: Vec<(u64, Address)>,
    token_select: Entity<SelectState<SearchableVec<FeesTokenFilterItem>>>,
    synced_token_filter_items: Vec<FeesTokenFilterItem>,
    /// Active sort state for the fee column. `Default` preserves the natural
    /// (chain, broadcaster, token) order set by `set_rows`.
    fee_sort: ColumnSort,
    /// Active sort state for the anchor bonus column. Missing anchors sort last.
    bonus_sort: ColumnSort,
}

impl FeesDelegate {
    pub(crate) fn new(
        broadcaster_input: Entity<InputState>,
        chain_select: Entity<SelectState<Vec<FeesChainFilterItem>>>,
        initial_chain_filter: FeesFilter<u64>,
        token_select: Entity<SelectState<SearchableVec<FeesTokenFilterItem>>>,
        default_fee_tokens: Vec<(u64, Address)>,
        fee_anchor_lookup: FeeAnchorLookup,
    ) -> Self {
        let initial_token_filter =
            default_token_filter_for_chain(initial_chain_filter, default_fee_tokens.as_slice());
        Self {
            all_rows: Arc::from(Vec::<FeeRow>::new()),
            rows: Arc::from(Vec::<FeeRow>::new()),
            columns: fee_columns(false),
            fee_anchor_lookup,
            preference_hooks: None,
            chain_select,
            chain_filter: initial_chain_filter,
            broadcaster_query: Arc::from(""),
            broadcaster_input,
            token_filter: initial_token_filter,
            default_fee_tokens,
            token_select,
            synced_token_filter_items: Vec::new(),
            fee_sort: ColumnSort::Default,
            bonus_sort: ColumnSort::Default,
        }
    }

    pub(crate) fn set_rows(&mut self, rows: Vec<FeeRow>) {
        let mut sorted = rows;
        sorted.sort_by(|a, b| {
            a.chain_id
                .cmp(&b.chain_id)
                .then_with(|| a.railgun_address.cmp(&b.railgun_address))
                .then_with(|| a.token_address.cmp(&b.token_address))
        });
        self.all_rows = Arc::from(sorted);
        self.rebuild_visible();
    }

    pub(crate) fn set_chain_filter(&mut self, filter: FeesFilter<u64>) {
        let changed = self.chain_filter != filter;
        self.chain_filter = filter;
        // Chain changes reset to the chain's native broadcaster fee token.
        // The broadcaster query is a substring — it self-corrects when rows
        // stop matching, no reset needed.
        self.token_filter = if changed {
            self.default_token_filter(filter)
        } else {
            cascade_reset_token(filter, self.token_filter)
        };
        self.rebuild_visible();
    }

    pub(crate) fn set_broadcaster_query(&mut self, query: Arc<str>) {
        self.broadcaster_query = query;
        self.rebuild_visible();
    }

    pub(crate) fn set_token_filter(&mut self, filter: FeesFilter<(u64, Address)>) {
        self.token_filter = filter;
        self.rebuild_visible();
    }

    /// Advance fee sort through Default → Descending → Ascending → Default,
    /// matching the cycle `gpui_component::table::DataTable` uses for its built-in
    /// sort icon.
    pub(crate) fn toggle_fee_sort(&mut self) {
        self.fee_sort = match self.fee_sort {
            ColumnSort::Default => ColumnSort::Descending,
            ColumnSort::Descending => ColumnSort::Ascending,
            ColumnSort::Ascending => ColumnSort::Default,
        };
        self.bonus_sort = ColumnSort::Default;
        self.rebuild_visible();
    }

    /// Advance bonus sort through Default → Descending → Ascending → Default.
    pub(crate) fn toggle_bonus_sort(&mut self) {
        self.bonus_sort = match self.bonus_sort {
            ColumnSort::Default => ColumnSort::Descending,
            ColumnSort::Descending => ColumnSort::Ascending,
            ColumnSort::Ascending => ColumnSort::Default,
        };
        self.fee_sort = ColumnSort::Default;
        self.rebuild_visible();
    }

    pub(crate) fn refresh_anchor_values(&mut self) {
        self.rebuild_visible();
    }

    pub(crate) fn set_preference_hooks(&mut self, hooks: Option<BroadcasterPreferenceHooks>) {
        let enabled = hooks.is_some();
        self.preference_hooks = hooks;
        self.columns = fee_columns(enabled);
    }

    fn rebuild_visible(&mut self) {
        let chain_filter = self.chain_filter;
        let token_filter = self.token_filter;
        let query = self.broadcaster_query.clone();

        let mut rows: Vec<FeeRow> = self
            .all_rows
            .iter()
            .filter(|row| matches_chain(row, chain_filter))
            .filter(|row| matches_token(row, token_filter))
            .filter(|row| matches_broadcaster(row, &query))
            .cloned()
            .collect();
        // Sort by raw fee amount when requested. Raw comparison is stable
        // within a single (chain, token) group; cross-token ordering is
        // meaningful only in wei — callers comparing human-scale magnitudes
        // across tokens should filter by token first.
        match self.bonus_sort {
            ColumnSort::Default => match self.fee_sort {
                ColumnSort::Default => {}
                ColumnSort::Ascending => rows.sort_by_key(|row| row.fee),
                ColumnSort::Descending => rows.sort_by_key(|row| std::cmp::Reverse(row.fee)),
            },
            sort => {
                rows.sort_by(|a, b| compare_fee_bonus_rows(a, b, sort, &self.fee_anchor_lookup));
            }
        }
        self.rows = Arc::from(rows);
    }

    /// Unique token options across `all_rows`, scoped by the current chain
    /// filter. Sorted by (`chain_id`, symbol-or-address) for stable menu order.
    fn token_options(&self) -> Vec<(u64, Address)> {
        let mut seen: Vec<(u64, Address)> = Vec::new();
        for row in self.all_rows.iter() {
            if !matches_chain(row, self.chain_filter) {
                continue;
            }
            let key = (row.chain_id, row.token_address);
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        if let FeesFilter::One(chain_id) = self.chain_filter
            && let Some(token) = self.default_fee_token(chain_id)
        {
            let key = (chain_id, token);
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        if let FeesFilter::One(key) = self.token_filter
            && matches_chain_id(key.0, self.chain_filter)
            && !seen.contains(&key)
        {
            seen.push(key);
        }
        seen.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| token_label(a.0, &a.1).cmp(&token_label(b.0, &b.1)))
        });
        seen
    }

    fn default_fee_token(&self, chain_id: u64) -> Option<Address> {
        self.default_fee_tokens
            .iter()
            .find_map(|(default_chain_id, token)| (*default_chain_id == chain_id).then_some(*token))
    }

    fn default_token_filter(&self, filter: FeesFilter<u64>) -> FeesFilter<(u64, Address)> {
        default_token_filter_for_chain(filter, self.default_fee_tokens.as_slice())
    }

    fn token_filter_items(&self) -> Vec<FeesTokenFilterItem> {
        let show_chain = matches!(self.chain_filter, FeesFilter::All);
        std::iter::once(FeesTokenFilterItem::all(show_chain))
            .chain(
                self.token_options()
                    .into_iter()
                    .map(move |(chain_id, address)| {
                        FeesTokenFilterItem::token(chain_id, address, show_chain)
                    }),
            )
            .collect()
    }

    pub(crate) fn sync_filter_selects(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) {
        self.chain_select.update(cx, |select, cx| {
            if select.selected_value().copied() != Some(self.chain_filter) {
                select.set_selected_value(&self.chain_filter, window, cx);
            }
        });

        let token_items = self.token_filter_items();
        let token_items_changed = self.synced_token_filter_items != token_items;
        if token_items_changed {
            self.synced_token_filter_items.clone_from(&token_items);
        }
        self.token_select.update(cx, |select, cx| {
            let selected_value = select.selected_value().copied();
            if token_items_changed {
                select.set_items(SearchableVec::new(token_items), window, cx);
            }
            if token_items_changed || selected_value != Some(self.token_filter) {
                select.set_selected_value(&self.token_filter, window, cx);
            }
        });
    }
}

const fn matches_chain(row: &FeeRow, filter: FeesFilter<u64>) -> bool {
    matches_chain_id(row.chain_id, filter)
}

const fn matches_chain_id(chain_id: u64, filter: FeesFilter<u64>) -> bool {
    match filter {
        FeesFilter::All => true,
        FeesFilter::One(id) => chain_id == id,
    }
}

fn default_token_filter_for_chain(
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

fn matches_token(row: &FeeRow, filter: FeesFilter<(u64, Address)>) -> bool {
    match filter {
        FeesFilter::All => true,
        FeesFilter::One((chain_id, addr)) => row.chain_id == chain_id && row.token_address == addr,
    }
}

fn matches_broadcaster(row: &FeeRow, query: &str) -> bool {
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
const fn cascade_reset_token(
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
fn chain_label(chain_id: u64) -> String {
    chain_name(chain_id).map_or_else(|| chain_id.to_string(), str::to_owned)
}

/// Display label for a token in filter UI — symbol when known, short-hash fallback.
fn token_label(chain_id: u64, addr: &Address) -> String {
    lookup_token(chain_id, addr).map_or_else(|| short_address(addr), |info| info.symbol.to_owned())
}

/// Token label for the filter menu, optionally prefixed with the chain name.
/// The prefix is added when the chain filter is `All`, so items from different
/// chains (`Ethereum: USDC` vs. `BSC: USDC`) can be told apart. When a single
/// chain is pinned the prefix is redundant and omitted.
fn token_menu_label(chain_id: u64, addr: &Address, show_chain: bool) -> String {
    if show_chain {
        format!("{}: {}", chain_label(chain_id), token_label(chain_id, addr))
    } else {
        token_label(chain_id, addr)
    }
}

fn icon_label_row(chain_id: u64, label: SharedString, icon_size: Pixels) -> impl IntoElement {
    let mut row = div().flex().items_center().gap_1();
    if let Some(path) = chain_icon_asset_path(chain_id) {
        row = row.child(img(path).size(icon_size).flex_none());
    }
    row.child(label)
}

fn token_label_row(
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

fn token_trigger_content(
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

impl TableDelegate for FeesDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let table = cx.entity();
        match col_ix {
            0 => render_chain_header(&self.chain_select).into_any_element(),
            1 => div()
                .id("fees-broadcaster-filter-input")
                .size_full()
                .flex()
                .items_center()
                .on_click(|_event, _window, cx| {
                    cx.stop_propagation();
                })
                .child(
                    Input::new(&self.broadcaster_input)
                        .bg(rgb(theme::SURFACE))
                        .with_size(Size::XSmall)
                        .w_full(),
                )
                .into_any_element(),
            2 => render_token_header(&self.token_select).into_any_element(),
            3 => render_fee_header(self.fee_sort, table).into_any_element(),
            4 => render_bonus_header(self.bonus_sort, table).into_any_element(),
            _ => div()
                .size_full()
                .child(self.columns[col_ix].name.clone())
                .into_any_element(),
        }
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from(format!("broadcaster-fee-row-{row_ix}")))
            .group(fee_row_group(row_ix))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        match col_ix {
            0 => {
                if matches!(self.chain_filter, FeesFilter::All) {
                    div()
                        .text_color(rgb(theme::PURPLE))
                        .child(icon_label_row(
                            row.chain_id,
                            SharedString::from(""),
                            px(16.0),
                        ))
                        .into_any_element()
                } else {
                    div().into_any_element()
                }
            }
            1 => {
                let addr = row.railgun_address.as_ref();
                let label = format_broadcaster_address_label(addr, row.identifier.as_deref());
                let addr = addr.to_string();
                let group = SharedString::from(format!("broadcaster-addr-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!(
                        "broadcaster-addr-cell-{row_ix}"
                    )))
                    .flex()
                    .items_center()
                    .gap_1()
                    .font_family(APP_MONO_FONT_FAMILY)
                    .text_color(rgb(theme::PURPLE))
                    .children(
                        self.preference_hooks
                            .as_ref()
                            .map(|hooks| render_preference_cell(row_ix, row, hooks)),
                    )
                    .child(SharedString::from(label))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "broadcaster-addr-copy-action-{row_ix}"
                            )))
                            .group(group.clone())
                            .flex_none()
                            .opacity(0.0)
                            .group_hover(group, |this| this.opacity(1.0))
                            .hover(|this| this.opacity(1.0))
                            .tooltip(|window, cx| {
                                Tooltip::new("Copy broadcaster address").build(window, cx)
                            })
                            .child(clipboard_with_toast(
                                SharedString::from(format!("broadcaster-addr-clipboard-{row_ix}")),
                                addr,
                            )),
                    )
                    .into_any_element()
            }
            2 => {
                let label = lookup_token(row.chain_id, &row.token_address).map_or_else(
                    || short_address(&row.token_address),
                    |info| info.symbol.to_owned(),
                );
                let addr_for_clipboard = row.token_address.to_string();
                let group = SharedString::from(format!("token-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!("token-cell-{row_ix}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(rgb(theme::TEXT))
                    .child(token_label_row(
                        row.chain_id,
                        &row.token_address,
                        SharedString::from(label),
                        px(14.0),
                    ))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "token-address-copy-action-{row_ix}"
                            )))
                            .group(group.clone())
                            .flex_none()
                            .opacity(0.0)
                            .group_hover(group, |this| this.opacity(1.0))
                            .hover(|this| this.opacity(1.0))
                            .tooltip(|window, cx| {
                                Tooltip::new("Copy token address").build(window, cx)
                            })
                            .child(clipboard_with_toast(
                                SharedString::from(format!("token-address-clipboard-{row_ix}")),
                                addr_for_clipboard,
                            )),
                    )
                    .into_any_element()
            }
            3 => {
                let raw_fee = row.fee.to_string();
                let label = raw_fee_label(row);
                let group = SharedString::from(format!("fee-cell-group-{row_ix}"));
                div()
                    .group(group.clone())
                    .id(SharedString::from(format!("fee-cell-{row_ix}")))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(rgb(theme::WARNING))
                    .tooltip({
                        let raw_fee = raw_fee.clone();
                        move |window, cx| Tooltip::new(raw_fee.clone()).build(window, cx)
                    })
                    .child(SharedString::from(label))
                    .child(
                        div()
                            .id(SharedString::from(format!("fee-copy-action-{row_ix}")))
                            .group(group.clone())
                            .flex_none()
                            .opacity(0.0)
                            .group_hover(group, |this| this.opacity(1.0))
                            .hover(|this| this.opacity(1.0))
                            .tooltip(|window, cx| Tooltip::new("Copy raw fee").build(window, cx))
                            .child(clipboard_with_toast(
                                SharedString::from(format!("fee-clipboard-{row_ix}")),
                                raw_fee,
                            )),
                    )
                    .into_any_element()
            }
            4 => render_fee_bonus_cell(row, &self.fee_anchor_lookup).into_any_element(),
            5 => {
                let (label, color) = if row.signature_valid {
                    ("OK", rgb(theme::SUCCESS))
                } else {
                    ("BAD", rgb(theme::DANGER))
                };
                div()
                    .text_color(color)
                    .child(SharedString::from(label))
                    .into_any_element()
            }
            6 => {
                let color = if row.reliability >= 0.9 {
                    rgb(theme::SUCCESS)
                } else {
                    rgb(theme::WARNING_STRONG)
                };
                div()
                    .text_color(color)
                    .child(SharedString::from(format!("{:.2}", row.reliability)))
                    .into_any_element()
            }
            7 => {
                let age = SystemTime::now()
                    .duration_since(row.last_seen)
                    .unwrap_or_default();
                div()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(SharedString::from(format_relative_age(age)))
                    .into_any_element()
            }
            8 => {
                let now = SystemTime::now();
                if let Ok(d) = row.fee_expiration.duration_since(now) {
                    div()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(format_compact_duration(d)))
                        .into_any_element()
                } else {
                    let age = now.duration_since(row.fee_expiration).unwrap_or_default();
                    div()
                        .text_color(rgb(theme::DANGER))
                        .child(SharedString::from(format!(
                            "expired {}",
                            format_relative_age(age)
                        )))
                        .into_any_element()
                }
            }
            _ => div().into_any_element(),
        }
    }
}

fn fee_columns(with_preferences: bool) -> Vec<Column> {
    let broadcaster_width = if with_preferences {
        px(316.0)
    } else {
        px(240.0)
    };
    let columns = vec![
        Column::new("chain", "chain").width(px(60.0)).movable(false),
        Column::new("broadcaster", "broadcaster")
            .width(broadcaster_width)
            .movable(false),
        Column::new("token", "token")
            .width(px(120.0))
            .movable(false),
        // Sorting is driven by our own cell-wide click handler in `render_th`
        // instead of the built-in sort icon, whose hitbox is too small.
        Column::new("fee", "fee").width(px(100.0)).movable(false),
        Column::new("bonus", "bonus %")
            .width(px(78.0))
            .movable(false),
        Column::new("sig", "sig").width(px(40.0)).movable(false),
        Column::new("reliability", "rel")
            .width(px(50.0))
            .movable(false),
        Column::new("last_seen", "last seen")
            .width(px(120.0))
            .movable(false),
        Column::new("expires", "expires in")
            .width(px(120.0))
            .movable(false),
    ];
    columns
}

fn raw_fee_label(row: &FeeRow) -> String {
    lookup_token(row.chain_id, &row.token_address).map_or_else(
        || row.fee.to_string(),
        |info| format_token_amount(row.fee, info.decimals),
    )
}

fn render_fee_bonus_cell(row: &FeeRow, fee_anchor_lookup: &FeeAnchorLookup) -> gpui::Div {
    match fee_bonus_bps(row, fee_anchor_lookup) {
        Some(bonus_bps) => div()
            .text_color(rgb(theme::WARNING))
            .child(SharedString::from(format_bonus_bps(bonus_bps))),
        None => div()
            .text_color(rgb(theme::TEXT_MUTED))
            .child(SharedString::from("n/a")),
    }
}

fn fee_bonus_bps(row: &FeeRow, fee_anchor_lookup: &FeeAnchorLookup) -> Option<i128> {
    let anchor = fee_anchor_lookup(row.chain_id, row.token_address)?;
    fee_bonus_bps_from_anchor(row.fee, anchor)
}

fn fee_bonus_bps_from_anchor(fee: U256, anchor: U256) -> Option<i128> {
    if anchor.is_zero() {
        return None;
    }
    let bps = fee.checked_mul(U256::from(10_000))?.checked_div(anchor)?;
    i128::try_from(bps).ok().map(|bps| bps - 10_000)
}

fn format_bonus_bps(bonus_bps: i128) -> String {
    let sign = if bonus_bps >= 0 { "+" } else { "-" };
    let abs_bps = bonus_bps.checked_abs().unwrap_or(i128::MAX);
    let tenths = (abs_bps + 5) / 10;
    format!("{sign}{}.{:01}%", tenths / 10, tenths % 10)
}

fn compare_fee_bonus_rows(
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

fn compare_optional_bonus(a: Option<i128>, b: Option<i128>, sort: ColumnSort) -> Ordering {
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

fn fee_row_group(row_ix: usize) -> SharedString {
    SharedString::from(format!("broadcaster-fee-row-group-{row_ix}"))
}

fn render_preference_cell(
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

fn render_chain_header(select: &Entity<SelectState<Vec<FeesChainFilterItem>>>) -> impl IntoElement {
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

fn render_token_header(
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

fn render_fee_header(
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

fn render_bonus_header(
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use alloy::uint;
    use std::time::SystemTime;

    fn row(chain_id: u64, broadcaster: &str, token: Address, identifier: Option<&str>) -> FeeRow {
        FeeRow {
            chain_id,
            railgun_address: Arc::from(broadcaster),
            token_address: token,
            fee: uint!(0_U256),
            signature_valid: true,
            fees_id: Arc::from("fid"),
            fee_expiration: SystemTime::now(),
            available_wallets: 1,
            version: Arc::from("8.2.3"),
            relay_adapt: address!("0000000000000000000000000000000000000002"),
            relay_adapt_7702: None,
            required_poi_list_keys: Vec::new(),
            identifier: identifier.map(Arc::from),
            last_seen: SystemTime::now(),
            reliability: 1.0,
        }
    }

    fn row_with_fee(fee: U256) -> FeeRow {
        let mut row = row(
            1,
            "0zkaaa",
            address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            None,
        );
        row.fee = fee;
        row
    }

    fn fixed_anchor_lookup(anchor: Option<U256>) -> FeeAnchorLookup {
        Arc::new(move |_chain_id, _token| anchor)
    }

    #[test]
    fn fee_bonus_bps_calculates_premium_and_discount() {
        assert_eq!(
            fee_bonus_bps_from_anchor(uint!(1_500_U256), uint!(1_000_U256)),
            Some(5_000)
        );
        assert_eq!(
            fee_bonus_bps_from_anchor(uint!(900_U256), uint!(1_000_U256)),
            Some(-1_000)
        );
    }

    #[test]
    fn fee_bonus_label_formats_signed_percent() {
        assert_eq!(format_bonus_bps(5_250), "+52.5%");
        assert_eq!(format_bonus_bps(-1_000), "-10.0%");
    }

    #[test]
    fn fee_bonus_sort_keeps_missing_anchor_last() {
        assert_eq!(
            compare_optional_bonus(Some(100), None, ColumnSort::Ascending),
            Ordering::Less
        );
        assert_eq!(
            compare_optional_bonus(None, Some(100), ColumnSort::Descending),
            Ordering::Greater
        );
        assert_eq!(
            compare_optional_bonus(Some(100), Some(200), ColumnSort::Ascending),
            Ordering::Less
        );
        assert_eq!(
            compare_optional_bonus(Some(100), Some(200), ColumnSort::Descending),
            Ordering::Greater
        );
    }

    #[test]
    fn raw_fee_label_remains_token_scaled() {
        let row = row_with_fee(uint!(1_234_000_U256));

        assert_eq!(raw_fee_label(&row), "1.23");
    }

    #[test]
    fn fee_bonus_uses_anchor_lookup_and_allows_cache_miss() {
        let row = row_with_fee(uint!(1_500_U256));

        assert_eq!(
            fee_bonus_bps(&row, &fixed_anchor_lookup(Some(uint!(1_000_U256)))),
            Some(5_000)
        );
        assert_eq!(fee_bonus_bps(&row, &fixed_anchor_lookup(None)), None);
    }

    #[test]
    fn chain_filter_one_narrows_rows_to_that_chain() {
        let t = address!("0x0000000000000000000000000000000000000001");
        let r1 = row(1, "0zkaaa", t, None);
        let r137 = row(137, "0zkbbb", t, None);
        assert!(matches_chain(&r1, FeesFilter::One(1)));
        assert!(!matches_chain(&r137, FeesFilter::One(1)));
        assert!(matches_chain(&r1, FeesFilter::All));
        assert!(matches_chain(&r137, FeesFilter::All));
    }

    #[test]
    fn broadcaster_query_matches_identifier_and_address_case_insensitive() {
        let t = address!("0x0000000000000000000000000000000000000001");
        let r = row(1, "0zkABCdef", t, Some("Alice"));
        // Empty query always matches.
        assert!(matches_broadcaster(&r, ""));
        // The caller is expected to pre-lowercase the query (that's what the
        // subscription in root.rs does); predicate does per-row lowercasing.
        assert!(matches_broadcaster(&r, "abcdef"));
        assert!(matches_broadcaster(&r, "ali"));
        assert!(!matches_broadcaster(&r, "zzz"));
        // A broadcaster without an identifier still matches by address.
        let r_no_id = row(1, "0zkABCdef", t, None);
        assert!(matches_broadcaster(&r_no_id, "abcdef"));
        assert!(!matches_broadcaster(&r_no_id, "ali"));
    }

    #[test]
    fn token_filter_items_compare_value_and_chain_display_scope() {
        let token = address!("0x0000000000000000000000000000000000000001");

        assert_eq!(
            FeesTokenFilterItem::token(1, token, false),
            FeesTokenFilterItem::token(1, token, false)
        );
        assert_ne!(
            FeesTokenFilterItem::token(1, token, false),
            FeesTokenFilterItem::token(1, token, true)
        );
    }

    #[test]
    fn cascade_reset_drops_token_filter_when_chain_changes() {
        let t = address!("0x0000000000000000000000000000000000000001");
        let token_on_chain_1 = FeesFilter::One((1u64, t));

        // Switching to a different chain drops the token filter.
        assert_eq!(
            cascade_reset_token(FeesFilter::One(137), token_on_chain_1),
            FeesFilter::All
        );
        // Staying on the same chain preserves it.
        assert_eq!(
            cascade_reset_token(FeesFilter::One(1), token_on_chain_1),
            token_on_chain_1
        );
        // Removing the chain filter preserves the token filter.
        assert_eq!(
            cascade_reset_token(FeesFilter::All, token_on_chain_1),
            token_on_chain_1
        );
        // When no token filter is set, any chain change is a no-op.
        assert_eq!(
            cascade_reset_token(FeesFilter::One(137), FeesFilter::All),
            FeesFilter::All
        );
    }
}
