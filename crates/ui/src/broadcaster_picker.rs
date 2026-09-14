//! Shared broadcaster grouping and display records. Native owners supply eligibility and estimates.
use crate::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_LINE_HEIGHT, APP_TEXT_SIZE};
use gpui::{
    App, Entity, Focusable, InteractiveElement, InteractiveText, IntoElement, ParentElement,
    Pixels, Rems, SharedString, StatefulInteractiveElement, Styled, StyledText, Window, div,
    prelude::FluentBuilder as _, px, relative, rems, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable,
    button::{Button, ButtonVariants},
    input::InputState,
    popover::Popover,
    separator::Separator,
    tooltip::Tooltip,
};
use ruint::aliases::U256;
use std::collections::{BTreeMap, BTreeSet};

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd,
)]
pub enum BroadcasterPickerFeeStatus {
    InRange,
    NoPremium,
    LowIncentive,
    VeryLowIncentive,
    HighFee,
    NotAssessed,
}

impl BroadcasterPickerFeeStatus {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::InRange => "in-range",
            Self::NoPremium => "no-premium",
            Self::LowIncentive => "low-incentive",
            Self::VeryLowIncentive => "very-low-incentive",
            Self::HighFee => "high-fee",
            Self::NotAssessed => "not-assessed",
        }
    }

    #[must_use]
    pub const fn tier(self) -> BroadcasterPickerTier {
        match self {
            Self::InRange => BroadcasterPickerTier::Incentivised,
            Self::NoPremium | Self::LowIncentive => BroadcasterPickerTier::Uncompensated,
            Self::VeryLowIncentive | Self::HighFee => BroadcasterPickerTier::OutsideRange,
            Self::NotAssessed => BroadcasterPickerTier::NotAssessed,
        }
    }
}

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd,
)]
pub enum BroadcasterPickerTier {
    Incentivised,
    Uncompensated,
    OutsideRange,
    NotAssessed,
}

impl BroadcasterPickerTier {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Incentivised => "Incentivised",
            Self::Uncompensated => "Uncompensated",
            Self::OutsideRange => "Outside range",
            Self::NotAssessed => "Not assessed",
        }
    }

    #[must_use]
    pub const fn badge_label(self, show_uncompensated_badge: bool) -> Option<&'static str> {
        match self {
            Self::Uncompensated if !show_uncompensated_badge => None,
            Self::Uncompensated => Some("No fee"),
            _ => Some(self.label()),
        }
    }

    #[must_use]
    pub const fn is_muted(self) -> bool {
        !matches!(self, Self::Incentivised)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BroadcasterPickerViewMode {
    #[default]
    Grouped,
    List,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BroadcasterPickerLayout {
    Standard,
    Compact,
}

impl BroadcasterPickerLayout {
    const fn primary_min_width(self) -> Rems {
        match self {
            Self::Standard => BROADCASTER_PICKER_PRIMARY_MIN_WIDTH,
            Self::Compact => rems(0.0),
        }
    }

    const fn fee_width(self) -> Rems {
        match self {
            Self::Standard => BROADCASTER_PICKER_FEE_WIDTH,
            Self::Compact => rems(5.0),
        }
    }

    const fn status_width(self) -> Rems {
        match self {
            Self::Standard => BROADCASTER_PICKER_STATUS_WIDTH,
            Self::Compact => rems(5.0),
        }
    }
}

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd,
)]
pub enum BroadcasterPickerGroupKey {
    Tier(BroadcasterPickerTier),
    Status(BroadcasterPickerFeeStatus),
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct BroadcasterPickerRow {
    pub railgun_address: String,
    pub label: String,
    pub advertised_fee: U256,
    pub premium_bps: Option<i128>,
    pub sort_order: usize,
    pub estimated_fee_amount: Option<U256>,
    pub estimated_fee_label: String,
    pub estimated_fee_usd_micro: Option<U256>,
    pub estimated_fee_usd_label: Option<String>,
    pub fee_status: BroadcasterPickerFeeStatus,
    pub fee_tier: BroadcasterPickerTier,
    pub show_uncompensated_badge: bool,
    pub fee_status_detail: String,
    pub fee_warning: Option<String>,
    pub favorite: bool,
    pub selected: bool,
    pub child_of: Option<BroadcasterPickerGroupKey>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
struct BroadcasterPickerGroupChildRevision {
    railgun_address: String,
    label: String,
    advertised_fee: U256,
    premium_bps: Option<i128>,
    estimated_fee_amount: Option<U256>,
    estimated_fee_label: String,
    estimated_fee_usd_micro: Option<U256>,
    estimated_fee_usd_label: Option<String>,
    fee_status: BroadcasterPickerFeeStatus,
    fee_tier: BroadcasterPickerTier,
    show_uncompensated_badge: bool,
    fee_status_detail: String,
    fee_warning: Option<String>,
    favorite: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct BroadcasterPickerGroupRevision(Vec<BroadcasterPickerGroupChildRevision>);

impl BroadcasterPickerGroupRevision {
    fn from_rows(rows: &[BroadcasterPickerRow]) -> Self {
        Self(
            rows.iter()
                .map(|row| BroadcasterPickerGroupChildRevision {
                    railgun_address: row.railgun_address.clone(),
                    label: row.label.clone(),
                    advertised_fee: row.advertised_fee,
                    premium_bps: row.premium_bps,
                    estimated_fee_amount: row.estimated_fee_amount,
                    estimated_fee_label: row.estimated_fee_label.clone(),
                    estimated_fee_usd_micro: row.estimated_fee_usd_micro,
                    estimated_fee_usd_label: row.estimated_fee_usd_label.clone(),
                    fee_status: row.fee_status,
                    fee_tier: row.fee_tier,
                    show_uncompensated_badge: row.show_uncompensated_badge,
                    fee_status_detail: row.fee_status_detail.clone(),
                    fee_warning: row.fee_warning.clone(),
                    favorite: row.favorite,
                })
                .collect(),
        )
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct BroadcasterPickerSelectedCollapse {
    selected_address: String,
    group_revision: BroadcasterPickerGroupRevision,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct BroadcasterPickerGroup {
    pub key: BroadcasterPickerGroupKey,
    pub label: String,
    pub count: usize,
    pub estimated_fee_label: String,
    pub estimated_fee_usd_label: Option<String>,
    pub fee_tier: BroadcasterPickerTier,
    pub detail: String,
    pub expanded: bool,
    pub selected_child_address: Option<String>,
    pub revision: BroadcasterPickerGroupRevision,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
pub enum BroadcasterPickerEntry {
    Group(BroadcasterPickerGroup),
    Broadcaster(BroadcasterPickerRow),
}

impl BroadcasterPickerEntry {
    const fn tier(&self) -> BroadcasterPickerTier {
        match self {
            Self::Group(group) => group.fee_tier,
            Self::Broadcaster(row) => row.fee_tier,
        }
    }
}

pub fn broadcaster_picker_section_divider_before(
    entries: &[BroadcasterPickerEntry],
    view_mode: BroadcasterPickerViewMode,
    row: usize,
) -> bool {
    if view_mode != BroadcasterPickerViewMode::Grouped || row == 0 {
        return false;
    }
    matches!(
        entries.get(row),
        Some(BroadcasterPickerEntry::Group(group))
            if group.key == BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated)
    ) && entries.get(row - 1).map(BroadcasterPickerEntry::tier)
        == Some(BroadcasterPickerTier::Incentivised)
}

enum BroadcasterPickerSectionItem {
    Direct(Box<BroadcasterPickerRow>),
    Group {
        key: BroadcasterPickerGroupKey,
        rows: Vec<BroadcasterPickerRow>,
    },
}

impl BroadcasterPickerSectionItem {
    fn sort_fee(&self) -> U256 {
        match self {
            Self::Direct(row) => row.advertised_fee,
            Self::Group { rows, .. } => rows
                .iter()
                .map(|row| row.advertised_fee)
                .min()
                .unwrap_or_default(),
        }
    }

    fn tie_order(&self) -> usize {
        match self {
            Self::Direct(row) => row.sort_order,
            Self::Group { rows, .. } => rows
                .iter()
                .map(|row| row.sort_order)
                .min()
                .unwrap_or_default(),
        }
    }
}

#[must_use]
pub fn project_broadcaster_picker_entries(
    rows: &[BroadcasterPickerRow],
    view_mode: BroadcasterPickerViewMode,
    query_active: bool,
    expanded_groups: &BTreeSet<BroadcasterPickerGroupKey>,
    collapsed_selected_children: &BTreeMap<
        BroadcasterPickerGroupKey,
        BroadcasterPickerSelectedCollapse,
    >,
) -> Vec<BroadcasterPickerEntry> {
    let mut sorted_rows = rows.to_vec();
    sorted_rows.sort_by(|left, right| {
        left.advertised_fee
            .cmp(&right.advertised_fee)
            .then_with(|| left.fee_tier.cmp(&right.fee_tier))
            .then_with(|| left.sort_order.cmp(&right.sort_order))
    });
    if view_mode == BroadcasterPickerViewMode::List {
        return sorted_rows
            .into_iter()
            .map(|mut row| {
                row.show_uncompensated_badge = row.fee_tier == BroadcasterPickerTier::Uncompensated;
                BroadcasterPickerEntry::Broadcaster(row)
            })
            .collect();
    }

    let mut entries = Vec::new();
    for tier in [
        BroadcasterPickerTier::Incentivised,
        BroadcasterPickerTier::Uncompensated,
        BroadcasterPickerTier::OutsideRange,
        BroadcasterPickerTier::NotAssessed,
    ] {
        append_broadcaster_picker_tier(
            &mut entries,
            tier,
            sorted_rows
                .iter()
                .filter(|row| row.fee_tier == tier)
                .cloned()
                .collect(),
            query_active,
            expanded_groups,
            collapsed_selected_children,
        );
    }
    entries
}

fn append_broadcaster_picker_tier(
    entries: &mut Vec<BroadcasterPickerEntry>,
    tier: BroadcasterPickerTier,
    rows: Vec<BroadcasterPickerRow>,
    query_active: bool,
    expanded_groups: &BTreeSet<BroadcasterPickerGroupKey>,
    collapsed_selected_children: &BTreeMap<
        BroadcasterPickerGroupKey,
        BroadcasterPickerSelectedCollapse,
    >,
) {
    if rows.is_empty() {
        return;
    }
    if query_active {
        entries.extend(rows.into_iter().map(BroadcasterPickerEntry::Broadcaster));
        return;
    }

    let mut items = match tier {
        BroadcasterPickerTier::Incentivised | BroadcasterPickerTier::NotAssessed => rows
            .into_iter()
            .map(|row| BroadcasterPickerSectionItem::Direct(Box::new(row)))
            .collect(),
        BroadcasterPickerTier::Uncompensated => {
            vec![BroadcasterPickerSectionItem::Group {
                key: BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated),
                rows,
            }]
        }
        BroadcasterPickerTier::OutsideRange => grouped_outside_range_items(rows),
    };
    items.sort_by(|left, right| {
        left.sort_fee()
            .cmp(&right.sort_fee())
            .then_with(|| left.tie_order().cmp(&right.tie_order()))
    });
    for item in items {
        match item {
            BroadcasterPickerSectionItem::Direct(row) => {
                entries.push(BroadcasterPickerEntry::Broadcaster(*row));
            }
            BroadcasterPickerSectionItem::Group { key, rows } => append_broadcaster_picker_group(
                entries,
                key,
                rows,
                expanded_groups,
                collapsed_selected_children,
            ),
        }
    }
}

fn grouped_outside_range_items(
    rows: Vec<BroadcasterPickerRow>,
) -> Vec<BroadcasterPickerSectionItem> {
    let mut grouped = BTreeMap::<BroadcasterPickerFeeStatus, Vec<BroadcasterPickerRow>>::new();
    for row in rows {
        grouped.entry(row.fee_status).or_default().push(row);
    }
    let mut items = Vec::new();
    for (status, rows) in grouped {
        if rows.len() >= 2 {
            items.push(BroadcasterPickerSectionItem::Group {
                key: BroadcasterPickerGroupKey::Status(status),
                rows,
            });
        } else {
            items.extend(
                rows.into_iter()
                    .map(|row| BroadcasterPickerSectionItem::Direct(Box::new(row))),
            );
        }
    }
    items
}

fn append_broadcaster_picker_group(
    entries: &mut Vec<BroadcasterPickerEntry>,
    key: BroadcasterPickerGroupKey,
    rows: Vec<BroadcasterPickerRow>,
    expanded_groups: &BTreeSet<BroadcasterPickerGroupKey>,
    collapsed_selected_children: &BTreeMap<
        BroadcasterPickerGroupKey,
        BroadcasterPickerSelectedCollapse,
    >,
) {
    let revision = BroadcasterPickerGroupRevision::from_rows(&rows);
    let selected_child_address = rows
        .iter()
        .find(|row| row.selected)
        .map(|row| row.railgun_address.clone());
    let selected_collapse_matches = selected_child_address.as_deref().is_some_and(|selected| {
        collapsed_selected_children
            .get(&key)
            .is_some_and(|collapse| {
                collapse.selected_address == selected && collapse.group_revision == revision
            })
    });
    let expanded = expanded_groups.contains(&key)
        || (selected_child_address.is_some() && !selected_collapse_matches);
    let tier = rows[0].fee_tier;
    let (estimated_fee_label, estimated_fee_usd_label) = group_minimum_estimated_fee_labels(&rows);
    let label = match key {
        BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated)
            if rows.len() == 1 =>
        {
            "1 broadcaster earning no fee".to_string()
        }
        BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated) => {
            format!("{} broadcasters earning no fee", rows.len())
        }
        BroadcasterPickerGroupKey::Status(_) if tier == BroadcasterPickerTier::OutsideRange => {
            format!("{} broadcasters outside the allowed range", rows.len())
        }
        BroadcasterPickerGroupKey::Tier(_) | BroadcasterPickerGroupKey::Status(_) => {
            format!("{} broadcasters", rows.len())
        }
    };
    let detail = broadcaster_picker_group_detail(key, &rows);
    entries.push(BroadcasterPickerEntry::Group(BroadcasterPickerGroup {
        key,
        label,
        count: rows.len(),
        estimated_fee_label,
        estimated_fee_usd_label,
        fee_tier: tier,
        detail,
        expanded,
        selected_child_address,
        revision,
    }));
    if expanded {
        entries.extend(rows.into_iter().map(|mut row| {
            row.child_of = Some(key);
            BroadcasterPickerEntry::Broadcaster(row)
        }));
    }
}

fn broadcaster_picker_group_detail(
    key: BroadcasterPickerGroupKey,
    rows: &[BroadcasterPickerRow],
) -> String {
    let Some(first) = rows.first() else {
        return "Fee comparison unavailable.".to_string();
    };
    match key {
        BroadcasterPickerGroupKey::Tier(BroadcasterPickerTier::Uncompensated) => {
            "These broadcasters charge gas cost or less. They earn nothing on your transaction and have no reason to prioritise it."
                .to_string()
        }
        BroadcasterPickerGroupKey::Status(BroadcasterPickerFeeStatus::VeryLowIncentive) => {
            "These fees are below the allowed range.".to_string()
        }
        BroadcasterPickerGroupKey::Status(BroadcasterPickerFeeStatus::HighFee) => {
            let unavailable = rows
                .iter()
                .filter(|row| {
                    row.premium_bps
                        .and_then(|premium_bps| premium_bps.checked_add(10_000))
                        .is_none()
                })
                .count();
            if unavailable == 0 {
                "These fees are above the allowed range.".to_string()
            } else if unavailable == rows.len() {
                "These fees are outside the allowed range, but gas-cost comparisons are unavailable."
                    .to_string()
            } else {
                "These fees are above the allowed range where gas-cost comparisons are available; some comparisons are unavailable."
                    .to_string()
            }
        }
        BroadcasterPickerGroupKey::Tier(_) | BroadcasterPickerGroupKey::Status(_) => {
            first.fee_status_detail.clone()
        }
    }
}

#[must_use]
pub fn group_minimum_estimated_fee_labels(
    rows: &[BroadcasterPickerRow],
) -> (String, Option<String>) {
    let Some(first) = rows.first() else {
        return ("Estimate unavailable".to_string(), None);
    };
    if let Some(unavailable) = rows.iter().find(|row| row.estimated_fee_amount.is_none()) {
        return (unavailable.estimated_fee_label.clone(), None);
    }
    let token_label = rows
        .iter()
        .min_by_key(|row| row.estimated_fee_amount.unwrap_or_default())
        .map_or_else(
            || first.estimated_fee_label.clone(),
            |row| format!("from {}", row.estimated_fee_label),
        );
    let usd_label = rows
        .iter()
        .map(|row| {
            Some((
                row.estimated_fee_usd_micro?,
                row.estimated_fee_usd_label.as_deref()?,
            ))
        })
        .collect::<Option<Vec<_>>>()
        .and_then(|values| values.into_iter().min_by_key(|(value, _)| *value))
        .map(|(_, label)| format!("from {label}"));
    (token_label, usd_label)
}

pub fn update_broadcaster_picker_group_expansion(
    expanded_groups: &mut BTreeSet<BroadcasterPickerGroupKey>,
    collapsed_selected_children: &mut BTreeMap<
        BroadcasterPickerGroupKey,
        BroadcasterPickerSelectedCollapse,
    >,
    key: BroadcasterPickerGroupKey,
    currently_expanded: bool,
    selected_child_address: Option<String>,
    group_revision: BroadcasterPickerGroupRevision,
) {
    if currently_expanded {
        expanded_groups.remove(&key);
        if let Some(selected_child_address) = selected_child_address {
            collapsed_selected_children.insert(
                key,
                BroadcasterPickerSelectedCollapse {
                    selected_address: selected_child_address,
                    group_revision,
                },
            );
        } else {
            collapsed_selected_children.remove(&key);
        }
    } else {
        collapsed_selected_children.remove(&key);
        expanded_groups.insert(key);
    }
}

#[must_use]
pub fn render_broadcaster_picker_row(
    row: &BroadcasterPickerRow,
    layout: BroadcasterPickerLayout,
) -> gpui::Div {
    let label: SharedString = row.label.clone().into();
    let text = StyledText::new(label.clone());
    let text_layout = text.layout().clone();
    let label = InteractiveText::new(format!("broadcaster-label-{}", row.railgun_address), text)
        .tooltip(move |_, window, cx| {
            (text_layout.text() != label.as_ref())
                .then(|| broadcaster_picker_detail_tooltip(label.clone(), window, cx))
        });
    div()
        .w_full()
        .flex()
        .when(layout == BroadcasterPickerLayout::Standard, |this| {
            this.flex_wrap()
        })
        .items_center()
        .gap_2()
        .text_size(APP_TEXT_SIZE)
        .when(layout == BroadcasterPickerLayout::Compact, |this| {
            this.text_size(rems(0.875))
                .line_height(relative(APP_TEXT_LINE_HEIGHT))
        })
        .child(
            div()
                .flex_1()
                .min_w(layout.primary_min_width())
                .when(row.child_of.is_some(), |this| {
                    this.pl(
                        BROADCASTER_PICKER_GROUP_TOGGLE_SIZE + BROADCASTER_PICKER_GROUP_PRIMARY_GAP
                    )
                })
                .overflow_hidden()
                .whitespace_nowrap()
                .flex()
                .items_center()
                .gap(rems(0.3125))
                .text_color(rgb(if row.fee_tier.is_muted() {
                    theme::TEXT_SUBTLE
                } else {
                    theme::TEXT
                }))
                .font_family(APP_MONO_FONT_FAMILY)
                .font_weight(if row.fee_tier.is_muted() {
                    gpui::FontWeight::LIGHT
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .child(div().flex_1().min_w_0().truncate().child(label))
                .children(row.favorite.then(|| {
                    div()
                        .flex_shrink(1.0)
                        .when(layout == BroadcasterPickerLayout::Compact, |this| {
                            this.flex_none()
                        })
                        .flex()
                        .items_center()
                        .text_color(rgb(theme::WARNING))
                        .child(Icon::new(IconName::Star).size(rems(0.8125)))
                })),
        )
        .child(
            div()
                .flex_shrink(1.0)
                .when(layout == BroadcasterPickerLayout::Compact, |this| {
                    this.flex_none()
                })
                .min_w(rems(0.0))
                .flex()
                .items_center()
                .gap_1()
                .child(render_broadcaster_picker_fee_cell(row, layout))
                .child(render_broadcaster_picker_status_cell(row, layout)),
        )
}

fn render_broadcaster_picker_fee_cell(
    row: &BroadcasterPickerRow,
    layout: BroadcasterPickerLayout,
) -> impl IntoElement {
    render_broadcaster_picker_estimated_fee_cell(
        format!("broadcaster-fee-{}", row.railgun_address),
        &row.estimated_fee_label,
        row.estimated_fee_usd_label.as_deref(),
        row.fee_tier.is_muted(),
        layout,
    )
}

fn render_broadcaster_picker_estimated_fee_cell(
    id: String,
    token_label: &str,
    usd_label: Option<&str>,
    muted: bool,
    layout: BroadcasterPickerLayout,
) -> impl IntoElement {
    let token_label = token_label.to_string();
    let usd_label = usd_label.map(str::to_string);
    let (primary_color, secondary_color) = broadcaster_picker_fee_text_colors(muted);
    div()
        .id(SharedString::from(id))
        .w(layout.fee_width())
        .flex_shrink(1.0)
        .min_w(rems(0.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap(rems(0.0625))
        .when(layout == BroadcasterPickerLayout::Compact, |this| {
            let detail = token_label.clone();
            this.tooltip(move |window, cx| {
                broadcaster_picker_detail_tooltip(detail.clone().into(), window, cx)
            })
        })
        .child(
            div()
                .w_full()
                .truncate()
                .text_color(rgb(primary_color))
                .font_weight(if muted {
                    gpui::FontWeight::LIGHT
                } else {
                    gpui::FontWeight::MEDIUM
                })
                .child(usd_label.clone().unwrap_or_else(|| token_label.clone())),
        )
        .children(
            usd_label
                .as_ref()
                .filter(|_| layout == BroadcasterPickerLayout::Standard)
                .map(|_| {
                    div()
                        .w_full()
                        .truncate()
                        .text_color(rgb(secondary_color))
                        .text_size(rems(0.6875))
                        .child(token_label)
                }),
        )
}

fn render_broadcaster_picker_status_cell(
    row: &BroadcasterPickerRow,
    layout: BroadcasterPickerLayout,
) -> gpui::Div {
    let id = format!("broadcaster-picker-status-{}", row.railgun_address);
    render_broadcaster_picker_tier_cell(
        id,
        row.fee_tier,
        row.show_uncompensated_badge,
        SharedString::from(row.fee_status_detail.clone()),
        layout,
    )
}

fn render_broadcaster_picker_tier_cell(
    id: impl AsRef<str>,
    tier: BroadcasterPickerTier,
    show_uncompensated_badge: bool,
    detail: SharedString,
    layout: BroadcasterPickerLayout,
) -> gpui::Div {
    div()
        .w(layout.status_width())
        .flex_shrink(1.0)
        .when(layout == BroadcasterPickerLayout::Compact, |this| {
            this.flex()
        })
        .children(
            tier.badge_label(show_uncompensated_badge).map(|label| {
                render_broadcaster_picker_status_badge(id, tier, label, detail, layout)
            }),
        )
}

fn render_broadcaster_picker_status_badge(
    id: impl AsRef<str>,
    tier: BroadcasterPickerTier,
    label: &'static str,
    detail: SharedString,
    layout: BroadcasterPickerLayout,
) -> impl IntoElement {
    let color = status_tier_color(tier);
    let tooltip_revision = broadcaster_picker_status_tooltip_revision(tier, &detail);
    let tooltip_detail = detail;
    let compact_label = match tier {
        BroadcasterPickerTier::Incentivised => "Incentive",
        BroadcasterPickerTier::Uncompensated => "No fee",
        BroadcasterPickerTier::OutsideRange => "Outside",
        BroadcasterPickerTier::NotAssessed => "Unknown",
    };
    div()
        .id(SharedString::from(format!(
            "{}-badge-{tooltip_revision:016x}",
            id.as_ref()
        )))
        .w(layout.status_width())
        .flex_shrink(1.0)
        .flex()
        .overflow_hidden()
        .items_center()
        .px(rems(0.375))
        .py(rems(0.25))
        .rounded_sm()
        .border_1()
        .border_color(rgb(color))
        .text_color(rgb(color))
        .text_size(rems(0.625))
        .font_weight(gpui::FontWeight::MEDIUM)
        .when(layout == BroadcasterPickerLayout::Compact, |this| {
            this.w_auto()
                .max_w_full()
                .px_1()
                .py_0()
                .text_size(rems(0.6875))
                .line_height(relative(APP_TEXT_LINE_HEIGHT))
        })
        .tooltip(move |window, cx| {
            let Some(tooltip_width) = broadcaster_picker_status_tooltip_width(
                window.viewport_size().width,
                window.rem_size(),
            ) else {
                return Tooltip::element(|_window, _cx| div())
                    .m(rems(0.0))
                    .p(rems(0.0))
                    .border_0()
                    .build(window, cx);
            };
            let tooltip_detail = tooltip_detail.clone();
            Tooltip::element(move |_window, _cx| {
                render_broadcaster_picker_status_tooltip(
                    tier,
                    label,
                    tooltip_detail.clone(),
                    tooltip_width,
                )
            })
            .build(window, cx)
        })
        .child(div().flex_1().min_w(rems(0.0)).truncate().child(
            if layout == BroadcasterPickerLayout::Compact {
                compact_label
            } else {
                label
            },
        ))
}

fn render_broadcaster_picker_status_tooltip(
    tier: BroadcasterPickerTier,
    label: &'static str,
    detail: SharedString,
    width: Pixels,
) -> gpui::Div {
    let color = status_tier_color(tier);
    div()
        .w(width)
        .py(rems(0.125))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().size(rems(0.4375)).rounded_full().bg(rgb(color)))
                .child(
                    div()
                        .text_size(rems(0.75))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(color))
                        .child(label),
                ),
        )
        .child(
            Separator::horizontal()
                .color(rgb(theme::BORDER_SUBTLE))
                .my(rems(0.0625)),
        )
        .child(
            div()
                .w_full()
                .min_w(rems(0.0))
                .whitespace_normal()
                .text_size(rems(0.75))
                .line_height(rems(1.125))
                .text_color(rgb(theme::TEXT))
                .child(detail),
        )
}

fn broadcaster_picker_detail_tooltip(
    detail: SharedString,
    window: &mut Window,
    cx: &mut App,
) -> gpui::AnyView {
    let Some(width) =
        broadcaster_picker_status_tooltip_width(window.viewport_size().width, window.rem_size())
    else {
        return Tooltip::element(|_window, _cx| div())
            .m(rems(0.0))
            .p(rems(0.0))
            .border_0()
            .build(window, cx);
    };
    Tooltip::element(move |_window, _cx| {
        div()
            .max_w(width)
            .whitespace_normal()
            .text_size(rems(0.75))
            .line_height(relative(APP_TEXT_LINE_HEIGHT))
            .text_color(rgb(theme::TEXT))
            .child(detail.clone())
    })
    .build(window, cx)
}

#[must_use]
pub fn broadcaster_picker_status_tooltip_width(
    viewport_width: Pixels,
    rem_size: Pixels,
) -> Option<Pixels> {
    let tooltip_chrome = rem_size * 2.5 + px(2.0);
    let available_width = viewport_width - tooltip_chrome;
    (available_width > px(0.0)).then(|| available_width.min(rem_size * 20.0))
}

#[must_use]
pub fn broadcaster_picker_status_tooltip_revision(
    tier: BroadcasterPickerTier,
    detail: &str,
) -> u64 {
    let mut revision = 0xcbf29ce484222325_u64;
    for byte in tier.label().bytes().chain(detail.bytes()) {
        revision ^= u64::from(byte);
        revision = revision.wrapping_mul(0x100000001b3);
    }
    revision
}

const fn status_tier_color(tier: BroadcasterPickerTier) -> u32 {
    match tier {
        BroadcasterPickerTier::Incentivised => theme::SUCCESS,
        BroadcasterPickerTier::Uncompensated | BroadcasterPickerTier::NotAssessed => {
            theme::TEXT_MUTED
        }
        BroadcasterPickerTier::OutsideRange => theme::DANGER,
    }
}

#[must_use]
pub const fn broadcaster_picker_fee_text_colors(muted: bool) -> (u32, u32) {
    if muted {
        (theme::TEXT_MUTED, theme::TEXT_SUBTLE)
    } else {
        (theme::TEXT, theme::TEXT_MUTED)
    }
}

#[must_use]
pub fn broadcaster_picker_group_element_id(key: BroadcasterPickerGroupKey) -> String {
    match key {
        BroadcasterPickerGroupKey::Tier(tier) => {
            format!(
                "broadcaster-picker-group-tier-{}",
                tier.label().to_ascii_lowercase().replace(' ', "-")
            )
        }
        BroadcasterPickerGroupKey::Status(status) => {
            format!("broadcaster-picker-group-status-{}", status.key())
        }
    }
}

pub const BROADCASTER_PICKER_PRIMARY_MIN_WIDTH: Rems = rems(9.0);
pub const BROADCASTER_PICKER_FEE_WIDTH: Rems = rems(10.5);
pub const BROADCASTER_PICKER_STATUS_WIDTH: Rems = rems(6.875);
pub const BROADCASTER_PICKER_GROUP_TOGGLE_SIZE: Rems = rems(1.125);
pub const BROADCASTER_PICKER_GROUP_PRIMARY_GAP: Rems = rems(0.5);
pub fn render_broadcaster_picker_group(
    group: BroadcasterPickerGroup,
    layout: BroadcasterPickerLayout,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    disabled: bool,
) -> impl IntoElement {
    let uncompensated_detail = SharedString::from(group.detail.clone());
    crate::controls::app_button_base(SharedString::from(format!(
        "{}-toggle",
        broadcaster_picker_group_element_id(group.key)
    )))
    .ghost()
    .p_0()
    .flex_none()
    .disabled(disabled)
    .w_full()
    .h_auto()
    .on_click(move |_, window, cx| {
        cx.stop_propagation();
        on_toggle(window, cx);
    })
    .child(
        div()
            .id(SharedString::from(format!(
                "{}-card",
                broadcaster_picker_group_element_id(group.key)
            )))
            .w_full()
            .min_h(rems(5.0))
            .px(rems(0.75))
            .flex()
            .when(layout == BroadcasterPickerLayout::Standard, |this| {
                this.flex_wrap()
            })
            .items_center()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::SURFACE))
            .text_size(APP_TEXT_SIZE)
            .when(layout == BroadcasterPickerLayout::Compact, |this| {
                this.min_h_8()
                    .px_2()
                    .text_size(rems(0.875))
                    .line_height(relative(APP_TEXT_LINE_HEIGHT))
            })
            .when(
                group.fee_tier == BroadcasterPickerTier::Uncompensated,
                move |this| {
                    let detail = uncompensated_detail;
                    this.tooltip(move |window, cx| {
                        broadcaster_picker_detail_tooltip(detail.clone(), window, cx)
                    })
                },
            )
            .child(
                div()
                    .flex_1()
                    .min_w(layout.primary_min_width())
                    .flex()
                    .items_center()
                    .gap(BROADCASTER_PICKER_GROUP_PRIMARY_GAP)
                    .child(
                        div()
                            .size(BROADCASTER_PICKER_GROUP_TOGGLE_SIZE)
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(
                                Icon::new(if group.expanded {
                                    IconName::Minus
                                } else {
                                    IconName::Plus
                                })
                                .size(rems(0.75)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(rems(0.0))
                            .truncate()
                            .font_weight(if group.fee_tier.is_muted() {
                                gpui::FontWeight::LIGHT
                            } else {
                                gpui::FontWeight::MEDIUM
                            })
                            .text_color(rgb(if group.fee_tier.is_muted() {
                                theme::TEXT_SUBTLE
                            } else {
                                theme::TEXT
                            }))
                            .child(if layout == BroadcasterPickerLayout::Compact {
                                format!(
                                    "{} · {}",
                                    group.count,
                                    match group.key {
                                        BroadcasterPickerGroupKey::Tier(
                                            BroadcasterPickerTier::Uncompensated,
                                        ) => "No incentive",
                                        BroadcasterPickerGroupKey::Status(
                                            BroadcasterPickerFeeStatus::HighFee,
                                        ) => "High fee",
                                        BroadcasterPickerGroupKey::Status(
                                            BroadcasterPickerFeeStatus::VeryLowIncentive,
                                        ) => "Low fee",
                                        _ => group.fee_tier.label(),
                                    }
                                )
                            } else {
                                group.label
                            }),
                    ),
            )
            .child(
                div()
                    .flex_shrink(1.0)
                    .when(layout == BroadcasterPickerLayout::Compact, |this| {
                        this.flex_none()
                    })
                    .min_w(rems(0.0))
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(render_broadcaster_picker_estimated_fee_cell(
                        format!("{}-fee", broadcaster_picker_group_element_id(group.key)),
                        &group.estimated_fee_label,
                        group.estimated_fee_usd_label.as_deref(),
                        group.fee_tier.is_muted(),
                        layout,
                    ))
                    .child(render_broadcaster_picker_tier_cell(
                        broadcaster_picker_group_element_id(group.key),
                        group.fee_tier,
                        false,
                        SharedString::from(group.detail.clone()),
                        layout,
                    )),
            ),
    )
}

#[cfg(test)]
mod tests;

pub fn render_broadcaster_picker_header(
    layout: BroadcasterPickerLayout,
    query_input: &Entity<InputState>,
    filtered_count: usize,
    total_count: usize,
    fee_status_popover_open: bool,
    on_open_change: impl Fn(bool, &mut App) + 'static,
) -> gpui::Div {
    let broadcaster_header = if filtered_count == total_count {
        format!("Broadcaster ({total_count})")
    } else {
        format!("Broadcaster ({filtered_count} of {total_count})")
    };
    div()
        .flex()
        .when(layout == BroadcasterPickerLayout::Standard, |this| {
            this.flex_wrap()
        })
        .items_center()
        .gap_2()
        .px(rems(1.3125))
        .when(layout == BroadcasterPickerLayout::Compact, |this| {
            this.px_2()
        })
        .pt(rems(0.25))
        .text_size(rems(0.6875))
        .text_color(rgb(theme::TEXT_MUTED))
        .child(
            div()
                .flex_1()
                .min_w(layout.primary_min_width())
                .truncate()
                .child(broadcaster_header),
        )
        .child(
            div()
                .flex_shrink(1.0)
                .when(layout == BroadcasterPickerLayout::Compact, |this| {
                    this.flex_none()
                })
                .min_w(rems(0.0))
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(layout.fee_width())
                        .flex_shrink(1.0)
                        .when(layout == BroadcasterPickerLayout::Compact, |this| {
                            this.flex_none()
                        })
                        .min_w(rems(0.0))
                        .truncate()
                        .child(if layout == BroadcasterPickerLayout::Compact {
                            "Est. fee"
                        } else {
                            "Est. tx fee"
                        }),
                )
                .child(
                    div()
                        .w(layout.status_width())
                        .flex_shrink(1.0)
                        .when(layout == BroadcasterPickerLayout::Compact, |this| {
                            this.flex_none()
                        })
                        .min_w(rems(0.0))
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(div().min_w(rems(0.0)).truncate().child(
                            if layout == BroadcasterPickerLayout::Compact {
                                "Status"
                            } else {
                                "Fee status"
                            },
                        ))
                        .child({
                            let focus_query_input = query_input.clone();
                            let tooltip_enabled = !fee_status_popover_open;
                            Popover::new("broadcaster-picker-fee-status-popover")
                                .open(fee_status_popover_open)
                                .on_open_change(move |open, window, cx| {
                                    on_open_change(*open, cx);
                                    if !*open {
                                        focus_query_input
                                            .read(cx)
                                            .focus_handle(cx)
                                            .focus(window, cx);
                                    }
                                })
                                .trigger(
                                    Button::new("broadcaster-picker-fee-status-trigger")
                                        .text()
                                        .xsmall()
                                        .compact()
                                        .child(render_fee_status_info_icon(tooltip_enabled)),
                                )
                                .content(|_state, window, _cx| render_fee_status_popover(window))
                        }),
                ),
        )
}

fn render_fee_status_info_icon(tooltip_enabled: bool) -> impl IntoElement {
    div()
        .id("broadcaster-picker-fee-status-info")
        .size(rems(0.875))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::WARNING))
        .text_color(rgb(theme::WARNING))
        .text_size(rems(0.5625))
        .font_weight(gpui::FontWeight::MEDIUM)
        .hover(|this| this.bg(rgb(theme::SURFACE_HOVER)))
        .child("i")
        .when(tooltip_enabled, |this| {
            this.tooltip(|window, cx| {
                Tooltip::element(|window, _cx| render_fee_status_popover(window)).build(window, cx)
            })
        })
}

fn render_fee_status_popover(window: &Window) -> gpui::Div {
    div()
        .w(rems(22.5))
        .when_some(
            broadcaster_picker_status_tooltip_width(window.viewport_size().width, window.rem_size()),
            Styled::max_w,
        )
        .p(rems(0.75))
        .flex()
        .flex_col()
        .gap_2()
        .text_size(rems(0.75))
        .text_color(rgb(theme::TEXT))
        .child(
            div()
                .text_color(rgb(theme::WARNING))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child("Fee status"),
        )
        .child(div().child(
            "Est. tx fee includes gas cost and the broadcaster's fee.",
        ))
        .child(div().child(
            "Incentivised broadcasters charge more than gas cost, so submitting earns them something.",
        ))
}
