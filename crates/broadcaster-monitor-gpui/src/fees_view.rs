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

mod delegate_state;
mod fee_cells;
mod filter_helpers;
mod filter_items;
mod headers;
mod table_delegate;

#[allow(clippy::wildcard_imports)]
use fee_cells::*;
#[allow(clippy::wildcard_imports)]
use filter_helpers::*;
#[allow(clippy::wildcard_imports)]
use headers::*;
#[allow(clippy::wildcard_imports)]
use table_delegate::*;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FeesTokenFilterItem {
    value: FeesFilter<(u64, Address)>,
    show_chain: bool,
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

#[cfg(test)]
mod tests;
