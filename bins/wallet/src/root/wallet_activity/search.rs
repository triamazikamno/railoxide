//! Activity's search field, modelled on Discord's search: free text plus filter chips, with
//! suggestions under the field drawn from the network's rows. Recent searches stay in memory
//! for the wallet session, since UI state on disk is plaintext and they hold addresses and
//! hashes.

use alloy::primitives::U256;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Focusable as _, FontWeight,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Window, anchored, canvas, deferred, div, img,
    point, prelude::FluentBuilder as _, px, relative, rems, rgb,
};
use gpui_component::{
    Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Backspace, Enter, Escape, InputEvent, InputState, MoveDown, MoveUp},
    scroll::ScrollableElement as _,
    separator::Separator,
};
use ui::controls::{app_input, app_muted_text, app_text};
use ui::theme::{self, APP_TEXT_LINE_HEIGHT};
use wallet_ops::ReceivedKind;
use wallet_ops::vault::{ActivityAsset, ActivityNetwork};

use super::{
    ACTIVITY_KINDS, ActivityClearKind, ActivityRow, ActivityRowKind, clear_kind_label,
    contract_text, row_kind_label, shorten, status_badge,
};
use crate::assets::WalletIconSource;
use crate::root::ui_helpers::{count_label, token_icon_slot};
use crate::root::{WalletRoot, new_text_input};

/// The tokens the network's rows move, with their symbols and icons.
type Assets = [(ActivityAsset, String, Option<WalletIconSource>)];

const RECENT: &str = "Recent";
const RECENT_LIMIT: usize = 5;
/// Values listed for a pending filter.
const VALUES_LIMIT: usize = 8;
/// Matches listed per filter for free text.
const MATCHES_LIMIT: usize = 5;
const SUGGESTIONS_MAX_HEIGHT: Pixels = px(320.0);

/// A search filter, which chips of one kind belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum ActivityFacet {
    Kind,
    Token,
    With,
    Hash,
    Status,
    Value,
}

impl ActivityFacet {
    /// In the order the empty field lists them.
    const ALL: [Self; 6] = [
        Self::Kind,
        Self::Token,
        Self::With,
        Self::Hash,
        Self::Status,
        Self::Value,
    ];

    pub(in crate::root) const fn keyword(self) -> &'static str {
        match self {
            Self::Kind => "type:",
            Self::Token => "token:",
            Self::With => "with:",
            Self::Hash => "hash:",
            Self::Status => "status:",
            Self::Value => "value:",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Kind => "Operation",
            Self::Token => "Token",
            Self::With => "Counterparty",
            Self::Hash => "Transaction",
            Self::Status => "Status",
            Self::Value => "Value",
        }
    }

    const fn hint(self) -> &'static str {
        match self {
            Self::Kind => "send, receive, swap, shield…",
            Self::Token => "symbol or contract",
            Self::With => "contact or address",
            Self::Hash => "transaction hash",
            Self::Status => "in progress, confirmed, failed",
            Self::Value => ">$100, <$10",
        }
    }

    /// The filter whose keyword, in any case, starts `text`, and the text after it.
    pub(in crate::root) fn split_keyword(text: &str) -> Option<(Self, &str)> {
        let text = text.trim_start();
        Self::ALL.into_iter().find_map(|facet| {
            let keyword = facet.keyword();
            text.get(..keyword.len())
                .filter(|head| head.eq_ignore_ascii_case(keyword))
                .map(|_| (facet, &text[keyword.len()..]))
        })
    }
}

/// A `status:` value: a row tag's label, or still in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum ActivityStatusChoice {
    InProgress,
    Submitted,
    Unconfirmed,
    Confirmed,
    Failed,
    Blocked,
    Incomplete,
}

impl ActivityStatusChoice {
    const ALL: [Self; 7] = [
        Self::InProgress,
        Self::Submitted,
        Self::Unconfirmed,
        Self::Confirmed,
        Self::Failed,
        Self::Blocked,
        Self::Incomplete,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::InProgress => "In progress",
            Self::Submitted => "Submitted",
            Self::Unconfirmed => "Unconfirmed",
            Self::Confirmed => "Confirmed",
            Self::Failed => "Failed",
            Self::Blocked => "Blocked",
            Self::Incomplete => "Incomplete",
        }
    }

    /// Matches the tag the row shows; untagged received rows count as confirmed.
    fn matches(self, row: &ActivityRow) -> bool {
        if self == Self::InProgress {
            return row.in_flight;
        }
        let blocked = ReceivedKind::ShieldReceived { blocked: true };
        let tag = if row.kind == ActivityRowKind::Received(blocked) {
            "Blocked"
        } else {
            row.status
                .map_or("Confirmed", |status| status_badge(status).0)
        };
        tag == self.label()
    }
}

/// A filter chip in the search field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) enum ActivityChip {
    Kind(ActivityClearKind),
    Token(ActivityAsset),
    /// A counterparty's full address, lowercased.
    With(String),
    /// A transaction hash as Activity shows it, lowercased.
    Hash(String),
    Status(ActivityStatusChoice),
    /// Rows whose largest priced amount is worth more, or less, than `usd_micro` today.
    Value {
        above: bool,
        usd_micro: U256,
    },
}

impl ActivityChip {
    const fn facet(&self) -> ActivityFacet {
        match self {
            Self::Kind(_) => ActivityFacet::Kind,
            Self::Token(_) => ActivityFacet::Token,
            Self::With(_) => ActivityFacet::With,
            Self::Hash(_) => ActivityFacet::Hash,
            Self::Status(_) => ActivityFacet::Status,
            Self::Value { .. } => ActivityFacet::Value,
        }
    }

    fn matches(&self, row: &ActivityRow) -> bool {
        match self {
            Self::Kind(kind) => row.clear_kind() == *kind,
            Self::Token(token) => row.amounts.iter().any(|amount| amount.asset == *token),
            Self::With(address) => row
                .with
                .as_ref()
                .is_some_and(|with| with.address == *address),
            Self::Hash(hash) => row.hashes.contains(hash),
            Self::Status(status) => status.matches(row),
            Self::Value { above, usd_micro } => row.usd_micro().is_some_and(|usd| {
                if *above {
                    usd > *usd_micro
                } else {
                    usd < *usd_micro
                }
            }),
        }
    }
}

/// The search field's free text and chips. An empty one matches every row.
#[derive(Clone, Debug, Default)]
pub(in crate::root) struct ActivityFilter {
    /// Trimmed and lowercased.
    query: String,
    pub(in crate::root) chips: Vec<ActivityChip>,
}

impl ActivityFilter {
    pub(in crate::root) fn set_query(&mut self, query: &str) {
        self.query = query.trim().to_lowercase();
    }
}

/// Whether `row`'s search text holds the query and, for every filter that has chips, `row`
/// matches at least one of its chips.
pub(in crate::root) fn row_matches(row: &ActivityRow, filter: &ActivityFilter) -> bool {
    row.search.contains(&filter.query)
        && ActivityFacet::ALL.into_iter().all(|facet| {
            let mut chips = filter
                .chips
                .iter()
                .filter(|chip| chip.facet() == facet)
                .peekable();
            chips.peek().is_none() || chips.any(|chip| chip.matches(row))
        })
}

/// A recent search: the chips and free text in effect when it was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct ActivitySearch {
    pub(in crate::root) chips: Vec<ActivityChip>,
    pub(in crate::root) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) enum ActivitySuggestionAction {
    AddChip(ActivityChip),
    /// Picks a filter, whose values the suggestions then list.
    Pending(ActivityFacet),
    /// "Search for …": runs the field's text.
    SearchText,
    /// Replaces the chips and text with recent search `n`.
    Recent(usize),
}

#[derive(Clone, Debug)]
pub(in crate::root) struct ActivitySuggestion {
    pub(in crate::root) title: String,
    /// A keyword, shown bold, and the value it filters by.
    pub(in crate::root) secondary: Option<(&'static str, String)>,
    /// Shown at the end, such as how many operations move a token.
    pub(in crate::root) note: Option<String>,
    pub(in crate::root) icon: Option<WalletIconSource>,
    pub(in crate::root) action: ActivitySuggestionAction,
}

#[derive(Clone, Debug)]
pub(in crate::root) struct ActivitySuggestionGroup {
    /// `None` for the leading "Search for …".
    pub(in crate::root) title: Option<&'static str>,
    pub(in crate::root) items: Vec<ActivitySuggestion>,
}

/// The suggestions under the field, in groups, none of them empty.
///
/// - With a pending filter: its values in `rows`, narrowed by `text`.
/// - With no text: the filters, then the recent searches.
/// - With text: "Search for" it, then the filters and the values of each filter that match it.
pub(in crate::root) fn activity_suggestions(
    rows: &[ActivityRow],
    assets: &Assets,
    network: ActivityNetwork,
    text: &str,
    pending: Option<ActivityFacet>,
    recent: &[ActivitySearch],
) -> Vec<ActivitySuggestionGroup> {
    let text = text.trim();
    let query = text.to_lowercase();
    let group = |title, items| ActivitySuggestionGroup { title, items };
    let mut groups = Vec::new();
    if let Some(facet) = pending {
        groups.push(group(
            Some(facet.title()),
            facet_values(facet, rows, assets, network, &query, VALUES_LIMIT),
        ));
    } else if query.is_empty() {
        groups.push(group(
            Some("Filters"),
            ActivityFacet::ALL
                .into_iter()
                .map(filter_suggestion)
                .collect(),
        ));
        groups.push(group(
            Some(RECENT),
            (0..recent.len())
                .map(|index| {
                    suggestion(String::new(), None, ActivitySuggestionAction::Recent(index))
                })
                .collect(),
        ));
    } else {
        groups.push(group(
            None,
            vec![suggestion(
                format!("Search for {text}"),
                None,
                ActivitySuggestionAction::SearchText,
            )],
        ));
        groups.push(group(
            Some("Filters"),
            ActivityFacet::ALL
                .into_iter()
                .filter(|facet| {
                    facet.title().to_lowercase().starts_with(&query)
                        || facet.keyword().starts_with(&query)
                })
                .map(filter_suggestion)
                .collect(),
        ));
        for facet in [
            ActivityFacet::With,
            ActivityFacet::Token,
            ActivityFacet::Kind,
            ActivityFacet::Status,
            ActivityFacet::Hash,
            ActivityFacet::Value,
        ] {
            if facet == ActivityFacet::Hash && !is_hash_fragment(&query) {
                continue;
            }
            groups.push(group(
                Some(facet.title()),
                facet_values(facet, rows, assets, network, &query, MATCHES_LIMIT),
            ));
        }
    }
    groups.retain(|group| !group.items.is_empty());
    groups
}

const fn suggestion(
    title: String,
    secondary: Option<(&'static str, String)>,
    action: ActivitySuggestionAction,
) -> ActivitySuggestion {
    ActivitySuggestion {
        title,
        secondary,
        note: None,
        icon: None,
        action,
    }
}

fn filter_suggestion(facet: ActivityFacet) -> ActivitySuggestion {
    suggestion(
        facet.title().to_owned(),
        Some((facet.keyword(), facet.hint().to_owned())),
        ActivitySuggestionAction::Pending(facet),
    )
}

/// Up to `limit` values of `facet` present in `rows` that hold `query`, lowercased, those
/// starting with it first: "conf" lists Confirmed before Unconfirmed.
fn facet_values(
    facet: ActivityFacet,
    rows: &[ActivityRow],
    assets: &Assets,
    network: ActivityNetwork,
    query: &str,
    limit: usize,
) -> Vec<ActivitySuggestion> {
    let mut values = matching_facet_values(facet, rows, assets, network, query, usize::MAX);
    values.sort_by_key(|value| !value.title.to_lowercase().starts_with(query));
    values.truncate(limit);
    values
}

fn matching_facet_values(
    facet: ActivityFacet,
    rows: &[ActivityRow],
    assets: &Assets,
    network: ActivityNetwork,
    query: &str,
    limit: usize,
) -> Vec<ActivitySuggestion> {
    let add_chip = |chip, title, value| {
        suggestion(
            title,
            Some((facet.keyword(), value)),
            ActivitySuggestionAction::AddChip(chip),
        )
    };
    match facet {
        ActivityFacet::Kind => ACTIVITY_KINDS
            .into_iter()
            .map(ActivityClearKind::Entry)
            .chain([ActivityClearKind::Received])
            .filter(|kind| {
                rows.iter().any(|row| row.clear_kind() == *kind)
                    && clear_kind_label(*kind).to_lowercase().contains(query)
            })
            .take(limit)
            .map(|kind| {
                let label = clear_kind_label(kind);
                add_chip(
                    ActivityChip::Kind(kind),
                    label.to_owned(),
                    label.to_lowercase(),
                )
            })
            .collect(),
        ActivityFacet::Token => assets
            .iter()
            .filter_map(|(asset, symbol, icon)| {
                let contract = contract_text(*asset, network);
                let matched = symbol.to_lowercase().contains(query)
                    || contract
                        .as_ref()
                        .is_some_and(|contract| contract.to_lowercase().contains(query));
                matched.then(|| {
                    let count = rows
                        .iter()
                        .filter(|row| row.amounts.iter().any(|amount| amount.asset == *asset))
                        .count();
                    ActivitySuggestion {
                        icon: icon.clone(),
                        note: Some(count_label(count, "operation")),
                        ..add_chip(
                            ActivityChip::Token(*asset),
                            symbol.clone(),
                            contract
                                .as_deref()
                                .map_or_else(|| "native".to_owned(), shorten),
                        )
                    }
                })
            })
            .take(limit)
            .collect(),
        ActivityFacet::With => {
            let mut seen: Vec<&str> = Vec::new();
            let mut items = Vec::new();
            for with in rows.iter().filter_map(|row| row.with.as_ref()) {
                if items.len() == limit {
                    break;
                }
                let matched = with.address.contains(query)
                    || with
                        .label
                        .as_ref()
                        .is_some_and(|label| label.to_lowercase().contains(query));
                if !matched || seen.contains(&with.address.as_str()) {
                    continue;
                }
                seen.push(&with.address);
                items.push(add_chip(
                    ActivityChip::With(with.address.clone()),
                    with.label.clone().unwrap_or_else(|| with.short.clone()),
                    with.short.clone(),
                ));
            }
            items
        }
        ActivityFacet::Hash => {
            let fragment = query.strip_prefix("0x").unwrap_or(query);
            let mut seen: Vec<&str> = Vec::new();
            let mut items = Vec::new();
            for row in rows {
                for hash in &row.hashes {
                    if items.len() == limit {
                        return items;
                    }
                    if !hash.contains(fragment) || seen.contains(&hash.as_str()) {
                        continue;
                    }
                    seen.push(hash);
                    items.push(add_chip(
                        ActivityChip::Hash(hash.clone()),
                        row_kind_label(row.kind).to_owned(),
                        shorten(hash),
                    ));
                }
            }
            items
        }
        ActivityFacet::Status => ActivityStatusChoice::ALL
            .into_iter()
            .filter(|status| {
                rows.iter().any(|row| status.matches(row))
                    && status.label().to_lowercase().contains(query)
            })
            .take(limit)
            .map(|status| {
                add_chip(
                    ActivityChip::Status(status),
                    status.label().to_owned(),
                    status.label().to_lowercase(),
                )
            })
            .collect(),
        ActivityFacet::Value => {
            let values = match (parse_value(query), parse_usd(query)) {
                (Some(value), _) => vec![value],
                // A bare amount offers both directions.
                (None, Some(usd)) => vec![(true, usd), (false, usd)],
                (None, None) if query.is_empty() => vec![
                    (true, U256::from(100_000_000_u64)),
                    (false, U256::from(10_000_000_u64)),
                ],
                (None, None) => Vec::new(),
            };
            values
                .into_iter()
                .map(|(above, usd_micro)| {
                    let usd = usd_text(usd_micro);
                    let (title, value) = if above {
                        (format!("Above {usd}"), format!(">{usd}"))
                    } else {
                        (format!("Below {usd}"), format!("<{usd}"))
                    };
                    add_chip(ActivityChip::Value { above, usd_micro }, title, value)
                })
                .collect()
        }
    }
}

/// Whether free text looks like part of a hash: six or more hex digits, after an optional `0x`.
fn is_hash_fragment(query: &str) -> bool {
    let hex = query.strip_prefix("0x").unwrap_or(query);
    hex.len() >= 6 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// `>N` or `<N` US dollars, with an optional `$` and decimals: whether it is above, and the
/// amount in micro-USD.
fn parse_value(text: &str) -> Option<(bool, U256)> {
    let text = text.trim();
    let (above, amount) = match text.strip_prefix('>') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('<')?),
    };
    Some((above, parse_usd(amount)?))
}

/// `100`, `$12.5` or `0.25` as micro-USD.
fn parse_usd(amount: &str) -> Option<U256> {
    let amount = amount.trim();
    let amount = amount.strip_prefix('$').unwrap_or(amount);
    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    if (whole.is_empty() && fraction.is_empty())
        || !whole
            .chars()
            .chain(fraction.chars())
            .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    // Micro-USD has six decimals; finer digits are dropped.
    U256::from_str_radix(&format!("{whole}{fraction:0<6.6}"), 10).ok()
}

/// `$100`, or `$12.50` with cents.
fn usd_text(usd_micro: U256) -> String {
    let usd = railgun_ui::format_usd_micro_value(usd_micro);
    match usd.strip_suffix(".00") {
        Some(whole) => whole.to_owned(),
        None => usd,
    }
}

/// The value a chip shows, and its token's icon.
fn chip_value(
    chip: &ActivityChip,
    rows: &[ActivityRow],
    assets: &Assets,
) -> (String, Option<WalletIconSource>) {
    match chip {
        ActivityChip::Kind(kind) => (clear_kind_label(*kind).to_owned(), None),
        ActivityChip::Token(token) => assets
            .iter()
            .find(|(asset, ..)| asset == token)
            .map_or_else(
                || ("Token".to_owned(), None),
                |(_, symbol, icon)| (symbol.clone(), icon.clone()),
            ),
        ActivityChip::With(address) => (
            rows.iter()
                .filter_map(|row| row.with.as_ref())
                .find(|with| with.address == *address)
                .map_or_else(
                    || shorten(address),
                    |with| with.label.clone().unwrap_or_else(|| with.short.clone()),
                ),
            None,
        ),
        ActivityChip::Hash(hash) => (shorten(hash), None),
        ActivityChip::Status(status) => (status.label().to_owned(), None),
        ActivityChip::Value { above, usd_micro } => (
            format!("{}{}", if *above { '>' } else { '<' }, usd_text(*usd_micro)),
            None,
        ),
    }
}

/// The list child that shows suggestion `index`, counting group dividers and headers.
fn suggestion_child_index(groups: &[ActivitySuggestionGroup], index: usize) -> usize {
    let mut child = 0;
    let mut remaining = index;
    for (group_index, group) in groups.iter().enumerate() {
        child += usize::from(group_index > 0) + usize::from(group.title.is_some());
        if remaining < group.items.len() {
            return child + remaining;
        }
        child += group.items.len();
        remaining -= group.items.len();
    }
    child
}

/// A chip: its keyword, muted, then its value with a token's icon.
fn render_chip(
    keyword: &'static str,
    value: Option<(String, Option<WalletIconSource>)>,
) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap_1()
        .px(px(6.0))
        .py(px(1.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_ELEVATED))
        .child(
            app_text(keyword)
                .flex_none()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD),
        )
        .when_some(value, |this, (value, icon)| {
            this.when_some(icon, |this, icon| {
                this.child(img(icon).size_3p5().flex_none().rounded_full())
            })
            .child(
                app_text(value)
                    .flex_none()
                    .text_xs()
                    .font_weight(FontWeight::NORMAL),
            )
        })
}

/// The search field's state: its input, filter, pending filter, suggestions and recent
/// searches.
pub(super) struct ActivitySearchState {
    input: Entity<InputState>,
    pub(super) filter: ActivityFilter,
    /// A filter picked or typed with no value yet, whose values the suggestions list.
    pending: Option<ActivityFacet>,
    /// The field's text. With a pending filter it narrows the filter's values; else it is the
    /// filter's query.
    text: String,
    open: bool,
    /// Whether the press under way began in the already focused field, which toggles the
    /// suggestions.
    press_toggles: bool,
    /// The highlighted suggestion, counted across groups.
    highlighted: usize,
    groups: Vec<ActivitySuggestionGroup>,
    /// Newest first, in memory only.
    recent: Vec<ActivitySearch>,
    /// The field's window bounds, measured each frame, which anchor the suggestions.
    bounds: Entity<Option<Bounds<Pixels>>>,
    scroll: ScrollHandle,
}

impl ActivitySearchState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<'_, WalletRoot>) -> Self {
        let input = new_text_input(window, cx, "Search address, hash or token");
        cx.subscribe(&input, |root, input, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                let text = input.read(cx).value().to_string();
                root.set_wallet_activity_query(&text, cx);
            }
            InputEvent::PressEnter { .. } => root.search_wallet_activity_text(cx),
            InputEvent::Focus => root.show_wallet_activity_suggestions(true, cx),
            InputEvent::Blur => root.show_wallet_activity_suggestions(false, cx),
        })
        .detach();
        Self {
            input,
            filter: ActivityFilter::default(),
            pending: None,
            text: String::new(),
            open: false,
            press_toggles: false,
            highlighted: 0,
            groups: Vec::new(),
            recent: Vec::new(),
            bounds: cx.new(|_| None),
            scroll: ScrollHandle::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn recent(&self) -> &[ActivitySearch] {
        &self.recent
    }

    #[cfg(test)]
    pub(super) const fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    /// Forgets the wallet session's search: its text, filters and recent searches.
    pub(super) fn end_session(&mut self, window: AnyWindowHandle, cx: &mut App) {
        self.recent.clear();
        self.groups.clear();
        self.highlighted = 0;
        self.filter = ActivityFilter::default();
        self.pending = None;
        self.text.clear();
        self.open = false;
        self.set_field(String::new(), window, cx);
    }

    /// Sets the field's text, which needs the window a root update doesn't hold. Setting it
    /// emits no change.
    fn set_field(&self, text: String, window: AnyWindowHandle, cx: &mut App) {
        let input = self.input.clone();
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                input.update(cx, |input, cx| input.set_value(text, window, cx));
            });
        });
    }

    /// Drops the chips that name the previous network's tokens, counterparties or hashes.
    pub(super) fn change_network(&mut self) {
        self.filter.chips.retain(|chip| {
            !matches!(
                chip,
                ActivityChip::Token(_) | ActivityChip::With(_) | ActivityChip::Hash(_)
            )
        });
    }

    /// Follows the network's `rows` and `assets`: drops token chips no longer among `assets`,
    /// which couldn't be suggested again, and recomputes the query and suggestions.
    pub(super) fn refresh(
        &mut self,
        rows: &[ActivityRow],
        assets: &Assets,
        network: ActivityNetwork,
    ) {
        self.filter.chips.retain(|chip| match chip {
            ActivityChip::Token(token) => assets.iter().any(|(asset, ..)| asset == token),
            _ => true,
        });
        let query = if self.pending.is_some() {
            ""
        } else {
            self.text.as_str()
        };
        self.filter.set_query(query);
        self.groups = activity_suggestions(
            rows,
            assets,
            network,
            &self.text,
            self.pending,
            &self.recent,
        );
        if self.suggestion(self.highlighted).is_none() {
            self.highlighted = 0;
        }
    }

    const fn shown(&self) -> bool {
        self.open && !self.groups.is_empty()
    }

    fn suggestion(&self, index: usize) -> Option<&ActivitySuggestion> {
        self.groups.iter().flat_map(|group| &group.items).nth(index)
    }

    /// Records the chips and text in effect as the newest recent search.
    fn remember(&mut self) {
        let search = ActivitySearch {
            chips: self.filter.chips.clone(),
            text: self.text.trim().to_owned(),
        };
        if search.chips.is_empty() && search.text.is_empty() {
            return;
        }
        self.recent.retain(|recent| *recent != search);
        self.recent.insert(0, search);
        self.recent.truncate(RECENT_LIMIT);
    }
}

impl WalletRoot {
    /// Recomputes the search's query and suggestions, then shows the rows it leaves.
    fn apply_wallet_activity_search(&mut self, cx: &mut Context<'_, Self>) {
        let network = self.selected_activity_network();
        let state = &mut self.wallet_activity;
        state.search.refresh(&state.rows, &state.assets, network);
        self.sync_wallet_activity_list(cx);
        cx.notify();
    }

    fn set_wallet_activity_field(&self, text: String, cx: &mut Context<'_, Self>) {
        self.wallet_activity
            .search
            .set_field(text, self.window_handle, cx);
    }

    /// Follows the field's text. A filter keyword at its start becomes the pending filter.
    pub(in crate::root) fn set_wallet_activity_query(
        &mut self,
        text: &str,
        cx: &mut Context<'_, Self>,
    ) {
        let search = &mut self.wallet_activity.search;
        let typed = search
            .pending
            .is_none()
            .then(|| ActivityFacet::split_keyword(text))
            .flatten();
        if let Some((facet, rest)) = typed {
            search.pending = Some(facet);
            rest.trim_start().clone_into(&mut search.text);
        } else {
            text.clone_into(&mut search.text);
        }
        search.open = true;
        search.highlighted = 0;
        if typed.is_some() {
            let text = search.text.clone();
            self.set_wallet_activity_field(text, cx);
        }
        self.apply_wallet_activity_search(cx);
    }

    fn show_wallet_activity_suggestions(&mut self, open: bool, cx: &mut Context<'_, Self>) {
        let search = &mut self.wallet_activity.search;
        if search.open != open {
            search.open = open;
            search.highlighted = 0;
            cx.notify();
        }
    }

    /// Runs the field's free text, as "Search for …" or Enter: records it as a recent search
    /// and closes the suggestions.
    pub(in crate::root) fn search_wallet_activity_text(&mut self, cx: &mut Context<'_, Self>) {
        let search = &mut self.wallet_activity.search;
        if search.pending.is_none() && !search.text.trim().is_empty() {
            search.remember();
        }
        search.open = false;
        self.apply_wallet_activity_search(cx);
    }

    fn add_wallet_activity_chip(&mut self, chip: ActivityChip, cx: &mut Context<'_, Self>) {
        let search = &mut self.wallet_activity.search;
        if !search.filter.chips.contains(&chip) {
            search.filter.chips.push(chip);
        }
        search.pending = None;
        search.text.clear();
        search.open = false;
        search.remember();
        self.set_wallet_activity_field(String::new(), cx);
        self.apply_wallet_activity_search(cx);
    }

    fn remove_wallet_activity_chip(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        let chips = &mut self.wallet_activity.search.filter.chips;
        if index < chips.len() {
            chips.remove(index);
            self.apply_wallet_activity_search(cx);
        }
    }

    /// The field's clear button: drops the chips, the pending filter and the text.
    fn clear_wallet_activity_search(&mut self, cx: &mut Context<'_, Self>) {
        let search = &mut self.wallet_activity.search;
        search.filter.chips.clear();
        search.pending = None;
        search.text.clear();
        search.highlighted = 0;
        search.open = true;
        self.set_wallet_activity_field(String::new(), cx);
        self.apply_wallet_activity_search(cx);
    }

    fn apply_wallet_activity_suggestion(
        &mut self,
        action: ActivitySuggestionAction,
        cx: &mut Context<'_, Self>,
    ) {
        match action {
            ActivitySuggestionAction::AddChip(chip) => self.add_wallet_activity_chip(chip, cx),
            ActivitySuggestionAction::Pending(facet) => {
                let search = &mut self.wallet_activity.search;
                search.pending = Some(facet);
                search.text.clear();
                search.open = true;
                search.highlighted = 0;
                self.set_wallet_activity_field(String::new(), cx);
                self.apply_wallet_activity_search(cx);
            }
            ActivitySuggestionAction::SearchText => self.search_wallet_activity_text(cx),
            ActivitySuggestionAction::Recent(index) => {
                let search = &mut self.wallet_activity.search;
                let Some(recent) = search.recent.get(index).cloned() else {
                    return;
                };
                search.filter.chips = recent.chips;
                search.pending = None;
                search.text.clone_from(&recent.text);
                search.open = false;
                self.set_wallet_activity_field(recent.text, cx);
                self.apply_wallet_activity_search(cx);
            }
        }
    }

    fn clear_wallet_activity_recent(&mut self, cx: &mut Context<'_, Self>) {
        self.wallet_activity.search.recent.clear();
        self.apply_wallet_activity_search(cx);
    }

    fn highlight_wallet_activity_suggestion(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        let search = &mut self.wallet_activity.search;
        if search.highlighted != index {
            search.highlighted = index;
            cx.notify();
        }
    }

    /// Up or Down while suggestions are shown; `false` when none are.
    fn move_wallet_activity_highlight(&mut self, down: bool, cx: &mut Context<'_, Self>) -> bool {
        let search = &mut self.wallet_activity.search;
        let count = search
            .groups
            .iter()
            .map(|group| group.items.len())
            .sum::<usize>();
        if !search.shown() || count == 0 {
            return false;
        }
        search.highlighted = if down {
            (search.highlighted + 1) % count
        } else {
            (search.highlighted + count - 1) % count
        };
        search
            .scroll
            .scroll_to_item(suggestion_child_index(&search.groups, search.highlighted));
        cx.notify();
        true
    }

    /// Enter while suggestions are shown applies the highlighted one; `false` when none are.
    fn apply_highlighted_wallet_activity_suggestion(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let search = &self.wallet_activity.search;
        let Some(action) = search
            .shown()
            .then(|| search.suggestion(search.highlighted))
            .flatten()
            .map(|suggestion| suggestion.action.clone())
        else {
            return false;
        };
        self.apply_wallet_activity_suggestion(action, cx);
        true
    }

    /// Escape while suggestions are shown closes them; `false` when none are.
    fn dismiss_wallet_activity_suggestions(&mut self, cx: &mut Context<'_, Self>) -> bool {
        if !self.wallet_activity.search.shown() {
            return false;
        }
        self.show_wallet_activity_suggestions(false, cx);
        true
    }

    /// Backspace in the empty field clears the pending filter, else removes the last chip;
    /// `false` when the field has text or there is nothing to remove.
    fn wallet_activity_search_backspace(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let search = &mut self.wallet_activity.search;
        if !search.text.is_empty() {
            return false;
        }
        if search.pending.is_some() {
            search.pending = None;
        } else if search.filter.chips.pop().is_none() {
            return false;
        }
        self.apply_wallet_activity_search(cx);
        true
    }

    /// The search field, with its chips in the input's prefix, and the suggestions anchored
    /// under it while it is focused. Up, Down, Enter, Escape and Backspace are taken in the
    /// capture phase, before the input's own bindings, only when they act on the suggestions
    /// or chips.
    pub(super) fn render_wallet_activity_search(
        &self,
        root: &Entity<Self>,
        cx: &App,
    ) -> impl IntoElement {
        let state = &self.wallet_activity;
        let search = &state.search;
        let chips = div()
            .flex()
            .items_center()
            .gap_1()
            .children(search.filter.chips.iter().enumerate().map(|(index, chip)| {
                let root = root.clone();
                render_chip(
                    chip.facet().keyword(),
                    Some(chip_value(chip, &state.rows, &state.assets)),
                )
                .child(
                    div()
                        .id(("wallet-activity-chip-remove", index))
                        .flex_none()
                        .cursor_pointer()
                        .child(
                            Icon::new(IconName::Close)
                                .xsmall()
                                .text_color(rgb(theme::TEXT_MUTED)),
                        )
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            cx.stop_propagation();
                            root.update(cx, |root, cx| root.remove_wallet_activity_chip(index, cx));
                        }),
                )
            }))
            .children(
                search
                    .pending
                    .map(|facet| render_chip(facet.keyword(), None)),
            );
        let has_chips = !search.filter.chips.is_empty() || search.pending.is_some();
        let clear_root = root.clone();
        let clear = div()
            // Keeps the press from toggling the suggestions.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new("wallet-activity-search-clear")
                    .icon(Icon::new(IconName::Close))
                    .ghost()
                    .xsmall()
                    .tab_stop(false)
                    .on_click(move |_, _, cx| {
                        clear_root.update(cx, Self::clear_wallet_activity_search);
                    }),
            );
        let field = app_input(&search.input)
            .when(has_chips, |this| this.prefix(chips))
            .when(has_chips || !search.text.is_empty(), |this| {
                this.suffix(clear)
            });
        let measured = search.bounds.clone();
        let bounds = *search.bounds.read(cx);
        // Takes a key from the input only when `handle` acted on it.
        let key = |handle: fn(&mut Self, &mut Context<'_, Self>) -> bool| {
            let root = root.clone();
            move |cx: &mut App| {
                if root.update(cx, handle) {
                    cx.stop_propagation();
                }
            }
        };
        let (up, down, enter, escape, backspace) = (
            key(|root, cx| root.move_wallet_activity_highlight(false, cx)),
            key(|root, cx| root.move_wallet_activity_highlight(true, cx)),
            key(Self::apply_highlighted_wallet_activity_suggestion),
            key(Self::dismiss_wallet_activity_suggestions),
            key(Self::wallet_activity_search_backspace),
        );
        div()
            .relative()
            .flex_1()
            .min_w(px(0.0))
            .capture_action(move |_: &MoveUp, _, cx| up(cx))
            .capture_action(move |_: &MoveDown, _, cx| down(cx))
            .capture_action(move |_: &Enter, _, cx| enter(cx))
            .capture_action(move |_: &Escape, _, cx| escape(cx))
            .capture_action(move |_: &Backspace, _, cx| backspace(cx))
            .capture_any_mouse_down({
                let root = root.clone();
                let input = search.input.clone();
                move |_, window, cx| {
                    let focused = input.read(cx).focus_handle(cx).is_focused(window);
                    root.update(cx, |root, _| {
                        root.wallet_activity.search.press_toggles = focused;
                    });
                }
            })
            .on_mouse_down(MouseButton::Left, {
                let root = root.clone();
                move |_, _, cx| {
                    root.update(cx, |root, cx| {
                        let search = &mut root.wallet_activity.search;
                        if std::mem::take(&mut search.press_toggles) {
                            let open = !search.open;
                            root.show_wallet_activity_suggestions(open, cx);
                        }
                    });
                }
            })
            .child(field)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        if *measured.read(cx) != Some(bounds) {
                            measured.update(cx, |measured, _| *measured = Some(bounds));
                            window.request_animation_frame();
                        }
                    },
                    |_, (), _, _| {},
                )
                // Measures the field's frame, not the static position after it.
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .when_some(bounds.filter(|_| search.shown()), |this, bounds| {
                this.child(
                    deferred(
                        anchored()
                            .position(point(bounds.left(), bounds.bottom() + px(4.0)))
                            .snap_to_window_with_margin(px(8.0))
                            .child(
                                self.render_wallet_activity_suggestions(root, bounds.size.width),
                            ),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                )
            })
    }

    /// The suggestion list, as wide as the field: groups under section headers, with dividers
    /// between them.
    fn render_wallet_activity_suggestions(&self, root: &Entity<Self>, width: Pixels) -> gpui::Div {
        let search = &self.wallet_activity.search;
        let mut list = div()
            .id("wallet-activity-suggestions")
            .max_h(SUGGESTIONS_MAX_HEIGHT)
            .overflow_y_scroll()
            .track_scroll(&search.scroll)
            .flex()
            .flex_col();
        let mut index = 0;
        for (group_index, group) in search.groups.iter().enumerate() {
            if group_index > 0 {
                list = list.child(
                    Separator::horizontal()
                        .h(px(1.0))
                        .my(px(4.0))
                        .color(rgb(theme::BORDER_SUBTLE)),
                );
            }
            if let Some(title) = group.title {
                let clear_root = root.clone();
                list = list.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(8.0))
                        .pt(px(6.0))
                        .pb(px(4.0))
                        .child(
                            app_text(title.to_uppercase())
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD),
                        )
                        .when(title == RECENT, |this| {
                            this.child(
                                div()
                                    // Keeps the press from toggling the suggestions.
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(
                                        Button::new("wallet-activity-recent-clear")
                                            .text()
                                            .xsmall()
                                            .compact()
                                            .tab_stop(false)
                                            .accessibility_label("Clear recent searches")
                                            // The extra-small button sets the 12px size.
                                            .child(
                                                div()
                                                    .line_height(relative(APP_TEXT_LINE_HEIGHT))
                                                    .text_color(rgb(theme::PRIMARY))
                                                    .child("Clear"),
                                            )
                                            .on_click(move |_, _, cx| {
                                                clear_root.update(cx, |root, cx| {
                                                    root.clear_wallet_activity_recent(cx);
                                                });
                                            }),
                                    ),
                            )
                        }),
                );
            }
            for item in &group.items {
                list = list.child(self.render_wallet_activity_suggestion(root, item, index));
                index += 1;
            }
        }
        div()
            .w(width)
            .max_w(rems(28.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb(theme::POPOVER_BG))
            .overflow_hidden()
            .occlude()
            .child(list)
            .vertical_scrollbar(&search.scroll)
    }

    /// One suggestion: a title over its keyword and value, or a recent search's chips.
    fn render_wallet_activity_suggestion(
        &self,
        root: &Entity<Self>,
        item: &ActivitySuggestion,
        index: usize,
    ) -> impl IntoElement {
        let state = &self.wallet_activity;
        let content = match &item.action {
            ActivitySuggestionAction::Recent(recent) => {
                let recent = state.search.recent.get(*recent);
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .items_center()
                    .gap_1()
                    .overflow_hidden()
                    .children(
                        recent
                            .into_iter()
                            .flat_map(|recent| &recent.chips)
                            .map(|chip| {
                                render_chip(
                                    chip.facet().keyword(),
                                    Some(chip_value(chip, &state.rows, &state.assets)),
                                )
                            }),
                    )
                    .children(
                        recent
                            .filter(|recent| !recent.text.is_empty())
                            .map(|recent| app_text(recent.text.clone()).truncate()),
                    )
            }
            _ => div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .child(app_text(item.title.clone()).truncate())
                .when_some(item.secondary.clone(), |this, (keyword, value)| {
                    this.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w(px(0.0))
                            .child(
                                app_text(keyword)
                                    .flex_none()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD),
                            )
                            .child(app_muted_text(value).min_w(px(0.0)).text_xs().truncate()),
                    )
                }),
        };
        let hover_root = root.clone();
        let click_root = root.clone();
        let action = item.action.clone();
        div()
            .id(("wallet-activity-suggestion", index))
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px(px(8.0))
            .py(px(4.0))
            .rounded_sm()
            .cursor_pointer()
            .when(index == state.search.highlighted, |this| {
                this.bg(rgb(theme::SURFACE_HOVER))
            })
            .on_hover(move |hovered, _, cx| {
                if *hovered {
                    hover_root.update(cx, |root, cx| {
                        root.highlight_wallet_activity_suggestion(index, cx);
                    });
                }
            })
            // A press, not a click, so the field keeps its focus and the suggestions stay open.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                cx.stop_propagation();
                click_root.update(cx, |root, cx| {
                    root.apply_wallet_activity_suggestion(action.clone(), cx);
                });
            })
            .when_some(item.icon.clone(), |this, icon| {
                this.child(token_icon_slot(Some(icon)))
            })
            .child(content)
            .when_some(item.note.clone(), |this, note| {
                this.child(app_muted_text(note).flex_none().text_xs())
            })
    }
}
