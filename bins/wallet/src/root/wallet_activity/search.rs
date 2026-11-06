//! Activity's search: its filters, chips and the values it suggests from the network's rows,
//! shown by the shared [`SearchField`].

use std::rc::Rc;

use alloy::primitives::U256;
use gpui::{App, Entity};
use wallet_ops::ReceivedKind;
use wallet_ops::vault::{ActivityAsset, ActivityNetwork};

use super::{
    ACTIVITY_KINDS, ActivityClearKind, ActivityRow, ActivityRowKind, clear_kind_label,
    contract_text, row_kind_label, shorten, status_badge,
};
use crate::assets::WalletIconSource;
use crate::root::search_field::{
    ChipCombine, SearchField, SearchFilter, SearchSource, Suggestion, SuggestionAction, suggestion,
};
#[cfg(test)]
use crate::root::search_field::{RecentSearch, SuggestionGroup, split_keyword, suggestions};
use crate::root::ui_helpers::count_label;

/// The tokens the network's rows move, with their symbols and icons.
type Assets = [(ActivityAsset, String, Option<WalletIconSource>)];

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
    #[cfg(test)]
    pub(in crate::root) fn split_keyword(text: &str) -> Option<(Self, &str)> {
        split_keyword::<ActivitySearchSource>(text)
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
pub(in crate::root) type ActivityFilter = SearchFilter<ActivityChip>;
/// A recent search: the chips and free text in effect when it was recorded.
#[cfg(test)]
pub(in crate::root) type ActivitySearch = RecentSearch<ActivityChip>;
pub(in crate::root) type ActivitySuggestionAction = SuggestionAction<ActivityFacet, ActivityChip>;
type ActivitySuggestion = Suggestion<ActivityFacet, ActivityChip>;
pub(super) type ActivitySearchField = SearchField<ActivitySearchSource>;

/// Whether `row`'s search text holds the query and, for every filter that has chips, `row`
/// matches at least one of its chips.
pub(in crate::root) fn row_matches(row: &ActivityRow, filter: &ActivityFilter) -> bool {
    row.search.contains(filter.query())
        && filter.chips_match::<ActivitySearchSource>(|chip| chip.matches(row))
}

/// The network's rows and the tokens they move, which the search suggests values from.
pub(super) struct ActivitySearchSource {
    rows: Rc<[ActivityRow]>,
    assets: Rc<Assets>,
    network: ActivityNetwork,
}

impl ActivitySearchSource {
    /// Before any rows are built; the network only names tokens, of which there are none.
    pub(super) fn empty() -> Self {
        Self {
            rows: Rc::default(),
            assets: Rc::default(),
            network: ActivityNetwork::Tron,
        }
    }
}

impl SearchSource for ActivitySearchSource {
    type Facet = ActivityFacet;
    type Chip = ActivityChip;
    const ID_PREFIX: &'static str = "wallet-activity";
    const PLACEHOLDER: &'static str = "Search address, hash or token";
    const FACETS: &'static [ActivityFacet] = &ActivityFacet::ALL;

    fn keyword(facet: ActivityFacet) -> &'static str {
        facet.keyword()
    }

    fn title(facet: ActivityFacet) -> &'static str {
        facet.title()
    }

    fn hint(facet: ActivityFacet) -> &'static str {
        facet.hint()
    }

    fn combine(_: ActivityFacet) -> ChipCombine {
        ChipCombine::Any
    }

    fn chip_facet(chip: &ActivityChip) -> ActivityFacet {
        chip.facet()
    }

    fn chip_value(&self, chip: &ActivityChip) -> (String, Option<WalletIconSource>) {
        chip_value(chip, &self.rows, &self.assets)
    }

    fn facet_values(
        &self,
        facet: ActivityFacet,
        query: &str,
        limit: usize,
    ) -> Vec<ActivitySuggestion> {
        facet_values(facet, &self.rows, &self.assets, self.network, query, limit)
    }

    /// Counterparties, tokens, operations, statuses, hashes only for part of one, and values.
    fn text_facets(&self, query: &str) -> Vec<ActivityFacet> {
        [
            ActivityFacet::With,
            ActivityFacet::Token,
            ActivityFacet::Kind,
            ActivityFacet::Status,
            ActivityFacet::Hash,
            ActivityFacet::Value,
        ]
        .into_iter()
        .filter(|&facet| facet != ActivityFacet::Hash || is_hash_fragment(query))
        .collect()
    }
}

/// The suggestions under the field for `rows`, as the field shows them.
#[cfg(test)]
pub(in crate::root) fn activity_suggestions(
    rows: &[ActivityRow],
    assets: &Assets,
    network: ActivityNetwork,
    text: &str,
    pending: Option<ActivityFacet>,
    recent: &[ActivitySearch],
) -> Vec<SuggestionGroup<ActivityFacet, ActivityChip>> {
    let source = ActivitySearchSource {
        rows: rows.into(),
        assets: assets.into(),
        network,
    };
    suggestions(&source, text, pending, recent)
}

/// Drops the chips that name the previous network's tokens, counterparties or hashes.
pub(super) fn change_network(search: &Entity<ActivitySearchField>, cx: &mut App) {
    search.update(cx, |search, cx| {
        search.retain_chips(
            |chip| {
                !matches!(
                    chip,
                    ActivityChip::Token(_) | ActivityChip::With(_) | ActivityChip::Hash(_)
                )
            },
            cx,
        );
    });
}

/// Follows the network's `rows` and `assets`: drops token chips no longer among `assets`,
/// which couldn't be suggested again, and recomputes the suggestions.
pub(super) fn refresh(
    search: &Entity<ActivitySearchField>,
    rows: &Rc<[ActivityRow]>,
    assets: &Rc<Assets>,
    network: ActivityNetwork,
    cx: &mut App,
) {
    let source = ActivitySearchSource {
        rows: Rc::clone(rows),
        assets: Rc::clone(assets),
        network,
    };
    search.update(cx, |search, cx| {
        search.set_source(source, cx);
        search.retain_chips(
            |chip| match chip {
                ActivityChip::Token(token) => assets.iter().any(|(asset, ..)| asset == token),
                _ => true,
            },
            cx,
        );
    });
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

#[cfg(test)]
impl crate::root::WalletRoot {
    /// Follows the field's text, as typing it does.
    pub(in crate::root) fn set_wallet_activity_query(
        &self,
        text: &str,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        self.wallet_activity
            .search
            .update(cx, |search, cx| search.set_query(text, cx));
    }

    /// Runs the field's free text, as Enter does.
    pub(in crate::root) fn search_wallet_activity_text(
        &mut self,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        self.wallet_activity
            .search
            .update(cx, ActivitySearchField::search_text);
    }
}
