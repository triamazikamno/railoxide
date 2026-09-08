use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::assets::CHEVRONS_DOWN_ICON_PATH;
use alloy::primitives::{Address, U256};
use gpui::{
    App, AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, SharedString, Size, StatefulInteractiveElement, Styled, WeakEntity, Window,
    div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Icon, IconName, IndexPath, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
    list::{ListDelegate, ListItem, ListState},
    popover::Popover,
    separator::Separator,
    tooltip::Tooltip,
    window_paddings,
};
use railgun_ui::{
    chain_name, format_broadcaster_address_label, format_token_amount, format_usd_micro_value,
};
use ui::controls::{app_muted_text, app_strong_text};
use ui::theme::{self, APP_MONO_FONT_FAMILY, APP_TEXT_SIZE};
use wallet_ops::{
    BroadcasterFeePolicy, BroadcasterFeePolicyStatus, DesktopSendPublicBroadcasterEstimateRequest,
    DesktopUnshieldPublicBroadcasterEstimateRequest, PublicBroadcasterCandidate,
    PublicBroadcasterCostEstimate, PublicBroadcasterSelection, broadcaster_fee_amount,
    buffered_public_broadcaster_fee, estimate_desktop_send_public_broadcaster_cost,
    estimate_desktop_unshield_public_broadcaster_cost, fee_policy_eligible_public_broadcasters,
    parse_send_amount, parse_unshield_amount, public_broadcaster_service_gas_price,
    select_public_broadcaster_with_policy_and_trust, settings::EffectiveTokenRegistry,
    sort_specific_public_broadcasters,
};

use super::retry::retry_backoff_delay;
use super::{
    ChainUtxoState, DeliveryFormKind, DeliveryMode, PRIVATE_ASSET_LIST_WIDTH, UnshieldAssetKey,
    WalletRoot, dialogs::render_broadcaster_picker_dialog_content, effective_fee_handling_mode,
    private_action::native_top_up_request_from_plan, token_display_label, token_display_metadata,
};

const BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const BROADCASTER_PICKER_DIALOG_FIXED_CHROME_HEIGHT: Pixels = px(100.0);
pub(super) const BROADCASTER_PICKER_ENTRY_HEIGHT: Pixels = px(84.0);
pub(super) const BROADCASTER_PICKER_MIN_LIST_HEIGHT: Pixels = px(120.0);
pub(super) const BROADCASTER_PICKER_LIST_HORIZONTAL_PADDING: Pixels = px(8.0);
pub(super) const BROADCASTER_PICKER_LIST_TOP_PADDING: Pixels = px(2.0);
pub(super) const BROADCASTER_PICKER_LIST_BOTTOM_PADDING: Pixels = px(8.0);
const BROADCASTER_PICKER_PRIMARY_MIN_WIDTH: Pixels = px(144.0);
const BROADCASTER_PICKER_FEE_WIDTH: Pixels = px(168.0);
const BROADCASTER_PICKER_STATUS_WIDTH: Pixels = px(110.0);
const BROADCASTER_PICKER_STATUS_TOOLTIP_WIDTH: Pixels = px(320.0);
const BROADCASTER_PICKER_HEADER_HORIZONTAL_PADDING: Pixels = px(21.0);
const BROADCASTER_PICKER_HEADER_TOP_PADDING: Pixels = px(4.0);
const BROADCASTER_PICKER_ROW_HORIZONTAL_PADDING: Pixels = px(12.0);
const BROADCASTER_PICKER_GROUP_TOGGLE_SIZE: Pixels = px(18.0);
const BROADCASTER_PICKER_GROUP_PRIMARY_GAP: Pixels = px(8.0);
const BROADCASTER_PICKER_SECTION_DIVIDER_INSET: Pixels = px(13.0);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) enum BroadcasterChoice {
    #[default]
    Random,
    Specific {
        railgun_address: String,
    },
}

pub(super) struct BroadcasterPickerState {
    pub(super) kind: DeliveryFormKind,
    pub(super) key: UnshieldAssetKey,
    pub(super) query_input: Entity<InputState>,
    pub(super) list: Entity<ListState<BroadcasterPickerDelegate>>,
    pub(super) scroll_indicator: Entity<BroadcasterPickerScrollIndicator>,
    pub(super) fee_status_popover_open: bool,
    view_mode: BroadcasterPickerViewMode,
    expanded_groups: BTreeSet<BroadcasterPickerGroupKey>,
    collapsed_selected_children:
        BTreeMap<BroadcasterPickerGroupKey, BroadcasterPickerSelectedCollapse>,
    fee_estimate_context: Option<BroadcasterPickerFeeEstimateContext>,
    fee_estimate_refresh_pending: bool,
    estimating_fee_context: bool,
    fee_estimate_id: u64,
    fee_estimate_retry: BroadcasterPickerFeeEstimateRetryState,
}

pub(super) struct BroadcasterPickerScrollIndicator {
    list: Entity<ListState<BroadcasterPickerDelegate>>,
    post_layout_refresh_pending: bool,
    last_viewport_size: Option<Size<Pixels>>,
}

impl BroadcasterPickerScrollIndicator {
    fn new(list: Entity<ListState<BroadcasterPickerDelegate>>, cx: &mut Context<'_, Self>) -> Self {
        cx.observe(&list, |indicator, _list, cx| {
            indicator.post_layout_refresh_pending = true;
            cx.notify();
        })
        .detach();
        Self {
            list,
            post_layout_refresh_pending: false,
            last_viewport_size: None,
        }
    }
}

impl Render for BroadcasterPickerScrollIndicator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let viewport_size = window.viewport_size();
        let viewport_changed = self.last_viewport_size != Some(viewport_size);
        self.last_viewport_size = Some(viewport_size);
        if self.post_layout_refresh_pending || viewport_changed {
            self.post_layout_refresh_pending = false;
            window.request_animation_frame();
        }
        let handle = self.list.read(cx).scroll_handle().base_handle().clone();
        let visible =
            broadcaster_picker_scroll_hint_visible(handle.offset().y, handle.max_offset().y);
        div()
            .absolute()
            .left(px(12.0))
            .bottom(px(10.0))
            .when(visible, |this| {
                this.flex()
                    .items_center()
                    .gap_1()
                    .px(px(7.0))
                    .py(px(4.0))
                    .rounded_md()
                    .bg(rgb(theme::SURFACE_ELEVATED))
                    .text_size(px(11.0))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(Icon::empty().path(CHEVRONS_DOWN_ICON_PATH).size(px(15.0)))
                    .child("Scroll for more")
            })
    }
}

pub(super) fn broadcaster_picker_scroll_hint_visible(
    offset_y: Pixels,
    max_offset_height: Pixels,
) -> bool {
    const TOLERANCE: Pixels = px(1.0);
    max_offset_height > TOLERANCE && max_offset_height + offset_y > TOLERANCE
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct BroadcasterPickerFeeEstimateRetryState {
    attempt: u8,
    generation: u64,
    scheduled: bool,
}

impl BroadcasterPickerFeeEstimateRetryState {
    pub(super) const fn should_schedule(
        self,
        estimating: bool,
        has_context: bool,
        refresh_pending: bool,
    ) -> bool {
        !estimating && (!has_context || refresh_pending) && !self.scheduled
    }

    pub(super) const fn mark_scheduled(&mut self, generation: u64) -> Duration {
        let delay = retry_backoff_delay(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        self.generation = generation;
        self.scheduled = true;
        delay
    }

    pub(super) const fn clear_if_current(&mut self, generation: u64) -> bool {
        if !self.scheduled || self.generation != generation {
            return false;
        }
        self.scheduled = false;
        true
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn finish_attempt(&mut self, succeeded: bool) {
        if succeeded {
            self.reset();
        } else {
            self.scheduled = false;
        }
    }

    pub(super) const fn is_scheduled(self) -> bool {
        self.scheduled
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum BroadcasterPickerFeeStatus {
    InRange,
    NoPremium,
    LowIncentive,
    VeryLowIncentive,
    HighFee,
    NotAssessed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BroadcasterPickerSuspiciousFeeDirection {
    BelowRange,
    AboveRange,
    Unrepresentable,
}

impl BroadcasterPickerFeeStatus {
    const fn key(self) -> &'static str {
        match self {
            Self::InRange => "in-range",
            Self::NoPremium => "no-premium",
            Self::LowIncentive => "low-incentive",
            Self::VeryLowIncentive => "very-low-incentive",
            Self::HighFee => "high-fee",
            Self::NotAssessed => "not-assessed",
        }
    }

    pub(super) const fn tier(self) -> BroadcasterPickerTier {
        match self {
            Self::InRange => BroadcasterPickerTier::Incentivised,
            Self::NoPremium | Self::LowIncentive => BroadcasterPickerTier::Uncompensated,
            Self::VeryLowIncentive | Self::HighFee => BroadcasterPickerTier::OutsideRange,
            Self::NotAssessed => BroadcasterPickerTier::NotAssessed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum BroadcasterPickerTier {
    Incentivised,
    Uncompensated,
    OutsideRange,
    NotAssessed,
}

impl BroadcasterPickerTier {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Incentivised => "Incentivised",
            Self::Uncompensated => "Uncompensated",
            Self::OutsideRange => "Outside range",
            Self::NotAssessed => "Not assessed",
        }
    }

    pub(super) const fn badge_label(self, show_uncompensated_badge: bool) -> Option<&'static str> {
        match self {
            Self::Uncompensated if !show_uncompensated_badge => None,
            Self::Uncompensated => Some("No fee"),
            _ => Some(self.label()),
        }
    }

    pub(super) const fn is_muted(self) -> bool {
        !matches!(self, Self::Incentivised)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum BroadcasterPickerViewMode {
    #[default]
    Grouped,
    List,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum BroadcasterPickerGroupKey {
    Tier(BroadcasterPickerTier),
    Status(BroadcasterPickerFeeStatus),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct BroadcasterPickerRow {
    pub(super) railgun_address: String,
    pub(super) label: String,
    pub(super) advertised_fee: U256,
    pub(super) premium_bps: Option<i128>,
    pub(super) sort_order: usize,
    pub(super) estimated_fee_amount: Option<U256>,
    pub(super) estimated_fee_label: String,
    pub(super) estimated_fee_usd_micro: Option<U256>,
    pub(super) estimated_fee_usd_label: Option<String>,
    pub(super) fee_status: BroadcasterPickerFeeStatus,
    pub(super) fee_tier: BroadcasterPickerTier,
    pub(super) show_uncompensated_badge: bool,
    pub(super) fee_status_detail: String,
    pub(super) fee_warning: Option<String>,
    pub(super) favorite: bool,
    pub(super) selected: bool,
    pub(super) child_of: Option<BroadcasterPickerGroupKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct BroadcasterPickerGroupRevision(Vec<BroadcasterPickerGroupChildRevision>);

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct BroadcasterPickerSelectedCollapse {
    selected_address: String,
    group_revision: BroadcasterPickerGroupRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct BroadcasterPickerGroup {
    pub(super) key: BroadcasterPickerGroupKey,
    pub(super) label: String,
    pub(super) count: usize,
    pub(super) estimated_fee_label: String,
    pub(super) estimated_fee_usd_label: Option<String>,
    pub(super) fee_tier: BroadcasterPickerTier,
    pub(super) detail: String,
    pub(super) expanded: bool,
    pub(super) selected_child_address: Option<String>,
    pub(super) revision: BroadcasterPickerGroupRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum BroadcasterPickerEntry {
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

    #[cfg(test)]
    pub(super) const fn height() -> Pixels {
        BROADCASTER_PICKER_ENTRY_HEIGHT
    }
}

pub(super) fn broadcaster_picker_section_divider_before(
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

#[derive(Clone)]
pub(super) struct BroadcasterPickerFeeEstimateContext {
    railgun_address: String,
    fee_amount: U256,
    gas_limit: u64,
    service_gas_price: u128,
}

enum BroadcasterPickerFeeEstimateRequest {
    Send(DesktopSendPublicBroadcasterEstimateRequest),
    Unshield(DesktopUnshieldPublicBroadcasterEstimateRequest),
}

impl BroadcasterPickerFeeEstimateContext {
    pub(super) fn from_estimate(estimate: &PublicBroadcasterCostEstimate) -> Self {
        Self {
            railgun_address: estimate.broadcaster.railgun_address.clone(),
            fee_amount: estimate.fee_amount,
            gas_limit: estimate.gas_limit,
            service_gas_price: public_broadcaster_service_gas_price(estimate.min_gas_price),
        }
    }
}

fn broadcaster_picker_suspicious_fee_direction(
    premium_bps: Option<i128>,
    policy: BroadcasterFeePolicy,
) -> BroadcasterPickerSuspiciousFeeDirection {
    let Some(fee_bps) = premium_bps.and_then(|premium_bps| premium_bps.checked_add(10_000)) else {
        return BroadcasterPickerSuspiciousFeeDirection::Unrepresentable;
    };
    if fee_bps < i128::from(policy.min_anchor_bps) {
        BroadcasterPickerSuspiciousFeeDirection::BelowRange
    } else {
        BroadcasterPickerSuspiciousFeeDirection::AboveRange
    }
}

pub(super) fn broadcaster_picker_fee_status(
    candidate: &PublicBroadcasterCandidate,
    policy: BroadcasterFeePolicy,
) -> BroadcasterPickerFeeStatus {
    match candidate.fee_policy_status {
        BroadcasterFeePolicyStatus::Normal { anchor_rate, .. } if candidate.fee == anchor_rate => {
            BroadcasterPickerFeeStatus::NoPremium
        }
        BroadcasterFeePolicyStatus::Normal { anchor_rate, .. } if candidate.fee < anchor_rate => {
            BroadcasterPickerFeeStatus::LowIncentive
        }
        BroadcasterFeePolicyStatus::Normal { .. } => BroadcasterPickerFeeStatus::InRange,
        BroadcasterFeePolicyStatus::Suspicious { premium_bps, .. } => {
            match broadcaster_picker_suspicious_fee_direction(premium_bps, policy) {
                BroadcasterPickerSuspiciousFeeDirection::BelowRange => {
                    BroadcasterPickerFeeStatus::VeryLowIncentive
                }
                BroadcasterPickerSuspiciousFeeDirection::AboveRange
                | BroadcasterPickerSuspiciousFeeDirection::Unrepresentable => {
                    BroadcasterPickerFeeStatus::HighFee
                }
            }
        }
        BroadcasterFeePolicyStatus::UnknownAnchor => BroadcasterPickerFeeStatus::NotAssessed,
    }
}

pub(super) fn broadcaster_picker_fee_status_detail(
    candidate: &PublicBroadcasterCandidate,
    policy: BroadcasterFeePolicy,
) -> String {
    match broadcaster_picker_fee_status(candidate, policy) {
        BroadcasterPickerFeeStatus::InRange => {
            "Charges more than the gas it spends, so submitting your transaction earns them something."
                .to_string()
        }
        BroadcasterPickerFeeStatus::NoPremium | BroadcasterPickerFeeStatus::LowIncentive => {
            "Charges gas cost or less, so this broadcaster earns nothing on your transaction and has no reason to prioritise it."
                .to_string()
        }
        BroadcasterPickerFeeStatus::VeryLowIncentive => {
            "This fee is below the allowed range.".to_string()
        }
        BroadcasterPickerFeeStatus::HighFee => {
            match broadcaster_picker_suspicious_fee_direction(
                candidate.fee_policy_status.premium_bps(),
                policy,
            ) {
                BroadcasterPickerSuspiciousFeeDirection::AboveRange => {
                    "This fee is above the allowed range.".to_string()
                }
                BroadcasterPickerSuspiciousFeeDirection::BelowRange
                | BroadcasterPickerSuspiciousFeeDirection::Unrepresentable => {
                    "This fee is outside the allowed range, but a gas-cost comparison is unavailable."
                        .to_string()
                }
            }
        }
        BroadcasterPickerFeeStatus::NotAssessed => format!(
            "A gas-cost comparison is unavailable. Advertised fee: {} raw token units.",
            candidate.fee
        ),
    }
}

#[derive(Clone)]
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

pub(super) fn project_broadcaster_picker_entries(
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

pub(super) fn group_minimum_estimated_fee_labels(
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

#[derive(Clone, PartialEq)]
pub(super) struct BroadcasterPickerContent {
    pub(super) entries: Vec<BroadcasterPickerEntry>,
    pub(super) empty_message: SharedString,
    pub(super) generating: bool,
    pub(super) show_all_broadcasters: bool,
    pub(super) query: String,
    pub(super) selected_address: Option<String>,
    pub(super) view_mode: BroadcasterPickerViewMode,
    pub(super) expanded_groups: BTreeSet<BroadcasterPickerGroupKey>,
    pub(super) collapsed_selected_children:
        BTreeMap<BroadcasterPickerGroupKey, BroadcasterPickerSelectedCollapse>,
}

pub(super) struct BroadcasterPickerDialogSnapshot {
    pub(super) query_input: Entity<InputState>,
    pub(super) list: Entity<ListState<BroadcasterPickerDelegate>>,
    pub(super) scroll_indicator: Entity<BroadcasterPickerScrollIndicator>,
    pub(super) entries: Vec<BroadcasterPickerEntry>,
    pub(super) empty_message: SharedString,
    pub(super) generating: bool,
    pub(super) query: String,
    pub(super) filtered_count: usize,
    pub(super) total_count: usize,
    pub(super) show_all_broadcasters: bool,
    pub(super) fee_status_popover_open: bool,
    pub(super) view_mode: BroadcasterPickerViewMode,
    pub(super) selected_address: Option<String>,
    pub(super) expanded_groups: BTreeSet<BroadcasterPickerGroupKey>,
    pub(super) collapsed_selected_children:
        BTreeMap<BroadcasterPickerGroupKey, BroadcasterPickerSelectedCollapse>,
    pub(super) kind: DeliveryFormKind,
    pub(super) key: UnshieldAssetKey,
}

pub(super) struct BroadcasterPickerDelegate {
    root: WeakEntity<WalletRoot>,
    kind: DeliveryFormKind,
    key: UnshieldAssetKey,
    generating: bool,
    entries: Vec<BroadcasterPickerEntry>,
    empty_message: SharedString,
    query: String,
    show_all_broadcasters: bool,
    selected_address: Option<String>,
    view_mode: BroadcasterPickerViewMode,
    expanded_groups: BTreeSet<BroadcasterPickerGroupKey>,
    collapsed_selected_children:
        BTreeMap<BroadcasterPickerGroupKey, BroadcasterPickerSelectedCollapse>,
    pending_content: Option<BroadcasterPickerContent>,
    last_live_update: Option<Instant>,
    live_update_scheduled: bool,
    live_update_epoch: u64,
}

impl BroadcasterPickerDelegate {
    pub(super) fn new(
        root: WeakEntity<WalletRoot>,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> Self {
        Self {
            root,
            kind,
            key,
            generating: false,
            entries: Vec::new(),
            empty_message: SharedString::from("No broadcasters match this search."),
            query: String::new(),
            show_all_broadcasters: false,
            selected_address: None,
            view_mode: BroadcasterPickerViewMode::Grouped,
            expanded_groups: BTreeSet::new(),
            collapsed_selected_children: BTreeMap::new(),
            pending_content: None,
            last_live_update: None,
            live_update_scheduled: false,
            live_update_epoch: 0,
        }
    }

    pub(super) fn set_content(
        &mut self,
        content: BroadcasterPickerContent,
        cx: &Context<'_, ListState<Self>>,
    ) -> bool {
        let current_content_matches = self.current_content_matches(&content);
        if clear_pending_content_if_current(current_content_matches, &mut self.pending_content) {
            return false;
        }

        if self.should_apply_immediately(&content) {
            self.apply_content_synchronously(content);
            return true;
        }

        if self.last_live_update.is_some_and(|last_update| {
            last_update.elapsed() >= BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL
        }) {
            self.apply_content_synchronously(content);
            return true;
        }

        if self.pending_content.as_ref() == Some(&content) {
            return false;
        }

        self.pending_content = Some(content);
        if !self.live_update_scheduled {
            self.live_update_scheduled = true;
            let scheduled_epoch = self.live_update_epoch;
            let remaining = self.last_live_update.map_or(
                BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL,
                |last_update| {
                    BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL.saturating_sub(last_update.elapsed())
                },
            );
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(remaining).await;
                let _ = this.update(cx, |list, cx| {
                    let delegate = list.delegate_mut();
                    let current_epoch = delegate.live_update_epoch;
                    let Some(content) = take_pending_broadcaster_picker_live_update(
                        scheduled_epoch,
                        current_epoch,
                        &mut delegate.live_update_scheduled,
                        &mut delegate.pending_content,
                    ) else {
                        return;
                    };
                    if !delegate.current_content_matches(&content) {
                        delegate.apply_content(content);
                        delegate.last_live_update = Some(Instant::now());
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        false
    }

    fn current_content_matches(&self, content: &BroadcasterPickerContent) -> bool {
        self.entries == content.entries
            && self.empty_message == content.empty_message
            && self.generating == content.generating
            && self.show_all_broadcasters == content.show_all_broadcasters
            && self.query == content.query
            && self.selected_address == content.selected_address
            && self.view_mode == content.view_mode
            && self.expanded_groups == content.expanded_groups
            && self.collapsed_selected_children == content.collapsed_selected_children
    }

    fn should_apply_immediately(&self, content: &BroadcasterPickerContent) -> bool {
        self.last_live_update.is_none()
            || self.query != content.query
            || self.generating != content.generating
            || self.show_all_broadcasters != content.show_all_broadcasters
            || self.selected_address != content.selected_address
            || self.view_mode != content.view_mode
            || self.expanded_groups != content.expanded_groups
            || self.collapsed_selected_children != content.collapsed_selected_children
    }

    fn apply_content(&mut self, content: BroadcasterPickerContent) {
        self.entries = content.entries;
        self.empty_message = content.empty_message;
        self.generating = content.generating;
        self.show_all_broadcasters = content.show_all_broadcasters;
        self.query = content.query;
        self.selected_address = content.selected_address;
        self.view_mode = content.view_mode;
        self.expanded_groups = content.expanded_groups;
        self.collapsed_selected_children = content.collapsed_selected_children;
    }

    fn apply_content_synchronously(&mut self, content: BroadcasterPickerContent) {
        self.pending_content = None;
        invalidate_broadcaster_picker_live_update(
            &mut self.live_update_epoch,
            &mut self.live_update_scheduled,
        );
        self.apply_content(content);
        self.last_live_update = Some(Instant::now());
    }
}

pub(super) fn clear_pending_content_if_current(
    current_content_matches: bool,
    pending_content: &mut Option<BroadcasterPickerContent>,
) -> bool {
    if !current_content_matches {
        return false;
    }
    pending_content.take();
    true
}

pub(super) const fn invalidate_broadcaster_picker_live_update(
    live_update_epoch: &mut u64,
    live_update_scheduled: &mut bool,
) {
    *live_update_epoch = live_update_epoch.wrapping_add(1);
    *live_update_scheduled = false;
}

pub(super) const fn take_pending_broadcaster_picker_live_update<T>(
    scheduled_epoch: u64,
    current_epoch: u64,
    live_update_scheduled: &mut bool,
    pending_content: &mut Option<T>,
) -> Option<T> {
    if scheduled_epoch != current_epoch {
        return None;
    }
    *live_update_scheduled = false;
    pending_content.take()
}

impl WalletRoot {
    pub(super) fn public_broadcaster_selection(
        choice: &BroadcasterChoice,
    ) -> PublicBroadcasterSelection {
        match choice {
            BroadcasterChoice::Random => PublicBroadcasterSelection::Random,
            BroadcasterChoice::Specific { railgun_address } => {
                PublicBroadcasterSelection::Specific {
                    railgun_address: railgun_address.clone(),
                }
            }
        }
    }

    pub(super) fn public_broadcaster_submission_selection(
        choice: &BroadcasterChoice,
        cost_estimate: Option<&PublicBroadcasterCostEstimate>,
    ) -> PublicBroadcasterSelection {
        match choice {
            BroadcasterChoice::Random => {
                cost_estimate.map_or(PublicBroadcasterSelection::Random, |estimate| {
                    PublicBroadcasterSelection::Specific {
                        railgun_address: estimate.broadcaster.railgun_address.clone(),
                    }
                })
            }
            BroadcasterChoice::Specific { .. } => Self::public_broadcaster_selection(choice),
        }
    }

    pub(super) fn set_broadcaster_picker_fee_status_popover_open(
        &mut self,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(picker) = self.broadcaster_picker.as_mut() else {
            return;
        };
        if picker.fee_status_popover_open == open {
            return;
        }
        picker.fee_status_popover_open = open;
        cx.notify();
    }

    pub(super) fn set_broadcaster_picker_view_mode(
        &mut self,
        view_mode: BroadcasterPickerViewMode,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(picker) = self.broadcaster_picker.as_mut() else {
            return;
        };
        if picker.view_mode == view_mode {
            return;
        }
        picker.view_mode = view_mode;
        cx.notify();
    }

    pub(super) fn toggle_broadcaster_picker_group(
        &mut self,
        key: BroadcasterPickerGroupKey,
        currently_expanded: bool,
        selected_child_address: Option<String>,
        group_revision: BroadcasterPickerGroupRevision,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(picker) = self.broadcaster_picker.as_mut() else {
            return;
        };
        update_broadcaster_picker_group_expansion(
            &mut picker.expanded_groups,
            &mut picker.collapsed_selected_children,
            key,
            currently_expanded,
            selected_child_address,
            group_revision,
        );
        cx.notify();
    }

    pub(super) fn open_broadcaster_picker(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.broadcaster_picker.is_some() {
            return;
        }
        let Some((asset_label, chain_id, fee_token)) = (match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).map(|form| {
                (
                    form.asset.label.clone(),
                    form.asset.chain_id,
                    form.selected_fee_token,
                )
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).map(|form| {
                (
                    form.asset.label.clone(),
                    form.asset.chain_id,
                    form.selected_fee_token,
                )
            }),
        }) else {
            return;
        };

        let query_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("search broadcasters"));
        let focus_query_input = query_input.clone();
        cx.subscribe(&query_input, |_this, _input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        let root = cx.weak_entity();
        let list = cx.new(|cx| {
            ListState::new(BroadcasterPickerDelegate::new(root, kind, key), window, cx)
                .selectable(false)
        });
        let scroll_indicator = cx.new(|cx| BroadcasterPickerScrollIndicator::new(list.clone(), cx));
        self.broadcaster_picker = Some(BroadcasterPickerState {
            kind,
            key,
            query_input,
            list,
            scroll_indicator,
            fee_status_popover_open: false,
            view_mode: BroadcasterPickerViewMode::Grouped,
            expanded_groups: BTreeSet::new(),
            collapsed_selected_children: BTreeMap::new(),
            fee_estimate_context: None,
            fee_estimate_refresh_pending: false,
            estimating_fee_context: false,
            fee_estimate_id: 0,
            fee_estimate_retry: BroadcasterPickerFeeEstimateRetryState::default(),
        });
        self.refresh_public_broadcaster_anchor(kind, key, cx);
        self.schedule_broadcaster_picker_fee_estimate(kind, key, cx);
        Self::open_broadcaster_picker_dialog(
            format!(
                "{asset_label} · fee token {}",
                token_display_label(chain_id, fee_token, Some(&self.effective_token_registry))
            ),
            chain_name(chain_id).map_or_else(|| chain_id.to_string(), str::to_owned),
            window,
            cx,
        );
        cx.defer_in(window, move |_this, window, cx| {
            focus_query_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
        });
        cx.notify();
    }

    fn open_broadcaster_picker_dialog(
        asset_label: String,
        chain_label: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            let viewport_size = window.viewport_size();
            let paddings = window_paddings(window);
            let available_width = viewport_size.width - paddings.left - paddings.right;
            let available_height = viewport_size.height - paddings.top - paddings.bottom;
            let dialog_width = (available_width * 0.92).min(PRIVATE_ASSET_LIST_WIDTH);
            let (margin_top, dialog_height) =
                broadcaster_picker_dialog_vertical_geometry(available_height);
            let content_height =
                (dialog_height - BROADCASTER_PICKER_DIALOG_FIXED_CHROME_HEIGHT).max(px(220.0));
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .on_ok(|_, _, _| false)
                .w(dialog_width)
                .h(dialog_height)
                .margin_top(margin_top)
                .title(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(app_strong_text("Choose public broadcaster"))
                        .child(app_muted_text(format!("{asset_label} on {chain_label}"))),
                )
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.close_broadcaster_picker(cx);
                    });
                })
                .child(render_broadcaster_picker_dialog_content(
                    &content_root,
                    content_height,
                    cx,
                ))
        });
    }

    pub(super) fn close_broadcaster_picker(&mut self, cx: &mut Context<'_, Self>) {
        self.broadcaster_picker = None;
        cx.notify();
    }

    pub(in crate::root) fn invalidate_broadcaster_picker_fee_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(picker) = self.broadcaster_picker.as_mut() else {
            return;
        };
        if picker.kind != kind || picker.key != key {
            return;
        }
        picker.fee_estimate_refresh_pending = picker.fee_estimate_context.is_some();
        picker.estimating_fee_context = false;
        picker.fee_estimate_id = 0;
        picker.fee_estimate_retry.reset();
        cx.notify();
    }

    pub(in crate::root) fn adopt_broadcaster_picker_fee_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        context: BroadcasterPickerFeeEstimateContext,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(picker) = self.broadcaster_picker.as_mut()
            && picker.kind == kind
            && picker.key == key
        {
            picker.fee_estimate_context = Some(context);
            picker.fee_estimate_refresh_pending = false;
            picker.estimating_fee_context = false;
            picker.fee_estimate_id = 0;
            picker.fee_estimate_retry.reset();
        }
        cx.notify();
    }

    pub(in crate::root) fn schedule_broadcaster_picker_fee_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        if self.broadcaster_picker.as_ref().is_none_or(|picker| {
            picker.kind != kind
                || picker.key != key
                || picker.estimating_fee_context
                || (picker.fee_estimate_context.is_some() && !picker.fee_estimate_refresh_pending)
                || picker.fee_estimate_retry.is_scheduled()
        }) {
            return;
        }

        if let Some(context) = self.form_public_broadcaster_fee_estimate_context(kind, key) {
            let Some(picker) = self.broadcaster_picker.as_mut() else {
                return;
            };
            picker.fee_estimate_context = Some(context);
            picker.fee_estimate_refresh_pending = false;
            picker.fee_estimate_retry.finish_attempt(true);
            cx.notify();
            return;
        }

        if self.form_has_public_broadcaster_cost_estimate_in_flight(kind, key) {
            self.schedule_broadcaster_picker_fee_estimate_retry(kind, key, cx);
            return;
        }

        let Some(request) = self.broadcaster_picker_fee_estimate_request(kind, key, cx) else {
            self.schedule_broadcaster_picker_fee_estimate_retry(kind, key, cx);
            return;
        };

        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        let Some(picker) = self.broadcaster_picker.as_mut() else {
            return;
        };
        picker.estimating_fee_context = true;
        picker.fee_estimate_id = estimate_id;
        cx.notify();

        let http = self.http.clone();
        let join = match request {
            BroadcasterPickerFeeEstimateRequest::Send(request) => self.runtime.spawn(async move {
                estimate_desktop_send_public_broadcaster_cost(request, &http).await
            }),
            BroadcasterPickerFeeEstimateRequest::Unshield(request) => {
                self.runtime.spawn(async move {
                    estimate_desktop_unshield_public_broadcaster_cost(request, &http).await
                })
            }
        };
        cx.spawn(async move |this, cx| {
            let context = match join.await {
                Ok(Ok(estimate)) => Some(BroadcasterPickerFeeEstimateContext::from_estimate(
                    &estimate,
                )),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "broadcaster picker fee estimate failed");
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "broadcaster picker fee estimate task failed");
                    None
                }
            };
            let retry = context.is_none();
            let _ = this.update(cx, |root, cx| {
                let Some(picker) = root.broadcaster_picker.as_mut() else {
                    return;
                };
                if picker.kind != kind || picker.key != key || picker.fee_estimate_id != estimate_id
                {
                    return;
                }
                picker.estimating_fee_context = false;
                picker.fee_estimate_id = 0;
                if let Some(context) = context {
                    picker.fee_estimate_context = Some(context);
                    picker.fee_estimate_refresh_pending = false;
                }
                picker.fee_estimate_retry.finish_attempt(!retry);
                cx.notify();
                if retry {
                    root.schedule_broadcaster_picker_fee_estimate_retry(kind, key, cx);
                }
            });
        })
        .detach();
    }

    fn schedule_broadcaster_picker_fee_estimate_retry(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let should_schedule = self.broadcaster_picker.as_ref().is_some_and(|picker| {
            picker.kind == kind
                && picker.key == key
                && picker.fee_estimate_retry.should_schedule(
                    picker.estimating_fee_context,
                    picker.fee_estimate_context.is_some(),
                    picker.fee_estimate_refresh_pending,
                )
        });
        if !should_schedule {
            return;
        }

        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let generation = self.cost_estimate_seq;
        let Some(picker) = self.broadcaster_picker.as_mut() else {
            return;
        };
        let delay = picker.fee_estimate_retry.mark_scheduled(generation);
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |root, cx| {
                let current = root.broadcaster_picker.as_mut().is_some_and(|picker| {
                    picker.kind == kind
                        && picker.key == key
                        && picker.fee_estimate_retry.clear_if_current(generation)
                });
                if current {
                    root.schedule_broadcaster_picker_fee_estimate(kind, key, cx);
                }
            });
        })
        .detach();
    }

    fn form_public_broadcaster_fee_estimate_context(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> Option<BroadcasterPickerFeeEstimateContext> {
        match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)?
                .cost_estimate
                .as_ref()
                .map(BroadcasterPickerFeeEstimateContext::from_estimate),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)?
                .cost_estimate
                .as_ref()
                .map(BroadcasterPickerFeeEstimateContext::from_estimate),
        }
    }

    fn form_has_public_broadcaster_cost_estimate_in_flight(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .is_some_and(|form| form.cost_estimate_pending || form.estimating_cost),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .is_some_and(|form| form.cost_estimate_pending || form.estimating_cost),
        }
    }

    fn broadcaster_picker_fee_estimate_request(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &Context<'_, Self>,
    ) -> Option<BroadcasterPickerFeeEstimateRequest> {
        match kind {
            DeliveryFormKind::Send => self
                .broadcaster_picker_send_fee_estimate_request(key, cx)
                .map(BroadcasterPickerFeeEstimateRequest::Send),
            DeliveryFormKind::Unshield => self
                .broadcaster_picker_unshield_fee_estimate_request(key, cx)
                .map(BroadcasterPickerFeeEstimateRequest::Unshield),
        }
    }

    fn broadcaster_picker_send_fee_estimate_request(
        &self,
        key: UnshieldAssetKey,
        cx: &Context<'_, Self>,
    ) -> Option<DesktopSendPublicBroadcasterEstimateRequest> {
        let form = self.send_forms.get(&key)?;
        if form.generating || form.delivery_mode != DeliveryMode::PublicBroadcaster {
            return None;
        }
        let asset = form.asset.clone();
        let amount_raw = form.amount_input.read(cx).value().to_string();
        let amount = parse_send_amount(amount_raw.as_str(), asset.decimals).ok()?;
        let ChainUtxoState::Ready { session, .. } = self.chain_states.get(&asset.chain_id)? else {
            return None;
        };
        let fee_token = form.selected_fee_token;
        let fee_mode = effective_fee_handling_mode(
            DeliveryFormKind::Send,
            asset.token,
            fee_token,
            form.fee_mode,
        );
        let policy = self.public_broadcaster_fee_policy(form.allow_suspicious_broadcasters);
        let candidates = self.current_public_broadcaster_candidates(
            asset.chain_id,
            fee_token,
            false,
            false,
            form.favorites_only_broadcasters,
            policy,
        );
        let selection = Self::public_broadcaster_selection(&form.broadcaster_choice);
        let trust_filter = self.public_broadcaster_trust_filter(form.favorites_only_broadcasters);
        if select_public_broadcaster_with_policy_and_trust(
            &candidates,
            &selection,
            policy,
            &trust_filter,
        )
        .is_err()
        {
            return None;
        }
        let recipient = self
            .view_session
            .as_ref()
            .and_then(|view_session| view_session.receive_address().ok())?;

        Some(DesktopSendPublicBroadcasterEstimateRequest {
            chain_id: asset.chain_id,
            effective_chain: self.effective_chain_configs.get(&asset.chain_id).cloned(),
            session: Arc::clone(session),
            token: asset.token,
            fee_token,
            amount,
            recipient,
            fee_rows: self.monitor_fee_rows(),
            selection,
            fee_mode,
            fee_policy: policy,
            trust_filter,
            anchor_cache: Some(Arc::clone(&self.public_broadcaster_anchor_cache)),
        })
    }

    fn broadcaster_picker_unshield_fee_estimate_request(
        &self,
        key: UnshieldAssetKey,
        cx: &Context<'_, Self>,
    ) -> Option<DesktopUnshieldPublicBroadcasterEstimateRequest> {
        let form = self.unshield_forms.get(&key)?;
        if form.generating || form.delivery_mode != DeliveryMode::PublicBroadcaster {
            return None;
        }
        let asset = form.asset.clone();
        let amount_raw = form.amount_input.read(cx).value().to_string();
        let amount = parse_unshield_amount(amount_raw.as_str(), asset.decimals).ok()?;
        let ChainUtxoState::Ready { session, .. } = self.chain_states.get(&asset.chain_id)? else {
            return None;
        };
        let fee_token = form.selected_fee_token;
        let native_top_up_plan = form
            .native_top_up_enabled
            .then(|| form.native_top_up.clone())
            .flatten();
        let native_top_up = native_top_up_request_from_plan(native_top_up_plan.as_ref());
        let fee_mode = effective_fee_handling_mode(
            DeliveryFormKind::Unshield,
            asset.token,
            fee_token,
            form.fee_mode,
        );
        let policy = self.public_broadcaster_fee_policy(form.allow_suspicious_broadcasters);
        let candidates = self.current_public_broadcaster_candidates(
            asset.chain_id,
            fee_token,
            form.unwrap,
            native_top_up.is_some(),
            form.favorites_only_broadcasters,
            policy,
        );
        let selection = Self::public_broadcaster_selection(&form.broadcaster_choice);
        let trust_filter = self.public_broadcaster_trust_filter(form.favorites_only_broadcasters);
        if select_public_broadcaster_with_policy_and_trust(
            &candidates,
            &selection,
            policy,
            &trust_filter,
        )
        .is_err()
        {
            return None;
        }

        Some(DesktopUnshieldPublicBroadcasterEstimateRequest {
            chain_id: asset.chain_id,
            effective_chain: self.effective_chain_configs.get(&asset.chain_id).cloned(),
            session: Arc::clone(session),
            token: asset.token,
            fee_token,
            amount,
            recipient: Address::ZERO,
            unwrap: form.unwrap,
            native_top_up,
            fee_rows: self.monitor_fee_rows(),
            selection,
            fee_mode,
            fee_policy: policy,
            trust_filter,
            anchor_cache: Some(Arc::clone(&self.public_broadcaster_anchor_cache)),
        })
    }

    pub(super) fn choose_broadcaster_from_picker(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        railgun_address: String,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let choice = BroadcasterChoice::Specific { railgun_address };
        let Some((chain_id, fee_token, unwrap, native_top_up, favorites_only, allow_suspicious)) =
            (match kind {
                DeliveryFormKind::Send => self.send_forms.get(&key).map(|form| {
                    (
                        form.asset.chain_id,
                        form.selected_fee_token,
                        false,
                        false,
                        form.favorites_only_broadcasters,
                        form.allow_suspicious_broadcasters,
                    )
                }),
                DeliveryFormKind::Unshield => self.unshield_forms.get(&key).map(|form| {
                    (
                        form.asset.chain_id,
                        form.selected_fee_token,
                        form.unwrap,
                        form.native_top_up_enabled && form.native_top_up.is_some(),
                        form.favorites_only_broadcasters,
                        form.allow_suspicious_broadcasters,
                    )
                }),
            })
        else {
            return;
        };
        let policy = self.public_broadcaster_fee_policy(allow_suspicious);
        let candidates = self.current_public_broadcaster_candidates(
            chain_id,
            fee_token,
            unwrap,
            native_top_up,
            favorites_only,
            policy,
        );
        if !broadcaster_choice_supported_by_candidates(&choice, &candidates, policy) {
            return;
        }
        match kind {
            DeliveryFormKind::Send => self.set_send_broadcaster_choice(key, choice, cx),
            DeliveryFormKind::Unshield => self.set_unshield_broadcaster_choice(key, choice, cx),
        }
        self.broadcaster_picker = None;
        cx.notify();
        window.close_dialog(cx);
    }

    pub(super) fn broadcaster_picker_dialog_snapshot(
        &self,
        cx: &App,
    ) -> Option<BroadcasterPickerDialogSnapshot> {
        let picker = self.broadcaster_picker.as_ref()?;
        let (
            chain_id,
            token,
            unwrap,
            current_choice,
            generating,
            show_all_broadcasters,
            favorites_only,
            native_top_up,
            cost_estimate,
            cost_estimate_pending,
            estimating_cost,
        ) = (match picker.kind {
            DeliveryFormKind::Send => self.send_forms.get(&picker.key).map(|form| {
                (
                    form.asset.chain_id,
                    form.selected_fee_token,
                    false,
                    form.broadcaster_choice.clone(),
                    form.generating,
                    form.allow_suspicious_broadcasters,
                    form.favorites_only_broadcasters,
                    false,
                    form.cost_estimate.as_ref(),
                    form.cost_estimate_pending,
                    form.estimating_cost,
                )
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&picker.key).map(|form| {
                (
                    form.asset.chain_id,
                    form.selected_fee_token,
                    form.unwrap,
                    form.broadcaster_choice.clone(),
                    form.generating,
                    form.allow_suspicious_broadcasters,
                    form.favorites_only_broadcasters,
                    form.native_top_up_enabled && form.native_top_up.is_some(),
                    form.cost_estimate.as_ref(),
                    form.cost_estimate_pending,
                    form.estimating_cost,
                )
            }),
        })?;
        let query = picker
            .query_input
            .read(cx)
            .value()
            .trim()
            .to_ascii_lowercase();
        let policy = self.public_broadcaster_fee_policy(show_all_broadcasters);
        let candidates = self.current_public_broadcaster_candidates(
            chain_id,
            token,
            unwrap,
            native_top_up,
            favorites_only,
            policy,
        );
        let candidates = if show_all_broadcasters {
            candidates
        } else {
            fee_policy_eligible_public_broadcasters(&candidates, policy)
        };
        let candidates =
            sort_specific_public_broadcasters(candidates, &self.public_broadcaster_sort_seed);
        let total_count = candidates.len();
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter(|candidate| broadcaster_candidate_matches_query(candidate, &query))
            .collect();
        let filtered_count = candidates.len();
        let empty_message = SharedString::from(if total_count == 0 {
            "No eligible broadcaster currently advertises this token."
        } else {
            "No broadcasters match this search."
        });
        let fee_estimate_context = cost_estimate
            .map(BroadcasterPickerFeeEstimateContext::from_estimate)
            .or_else(|| picker.fee_estimate_context.clone());
        let estimated_fee_placeholder =
            if cost_estimate_pending || estimating_cost || picker.estimating_fee_context {
                "Estimating..."
            } else if picker.fee_estimate_retry.is_scheduled() {
                "Retrying..."
            } else {
                "Estimate unavailable"
            };
        let selected_address = match &current_choice {
            BroadcasterChoice::Specific { railgun_address } => Some(railgun_address.clone()),
            BroadcasterChoice::Random => None,
        };
        let rows = candidates
            .iter()
            .enumerate()
            .map(|(sort_order, candidate)| {
                let estimated_fee_amount = broadcaster_candidate_estimated_fee_amount(
                    candidate,
                    fee_estimate_context.as_ref(),
                );
                let estimated_fee_label = estimated_fee_amount.map_or_else(
                    || estimated_fee_placeholder.to_string(),
                    |amount| {
                        format_estimated_fee_amount(
                            candidate,
                            amount,
                            Some(&self.effective_token_registry),
                        )
                    },
                );
                let estimated_fee_usd_micro = estimated_fee_amount.and_then(|amount| {
                    self.public_broadcaster_anchor_cache
                        .cached_token_usd_micro_value(candidate.chain_id, candidate.token, amount)
                });
                let fee_status = broadcaster_picker_fee_status(candidate, policy);
                let fee_tier = fee_status.tier();
                BroadcasterPickerRow {
                    railgun_address: candidate.railgun_address.clone(),
                    label: broadcaster_candidate_label(candidate),
                    advertised_fee: candidate.fee,
                    premium_bps: candidate.fee_policy_status.premium_bps(),
                    sort_order,
                    estimated_fee_amount,
                    estimated_fee_label,
                    estimated_fee_usd_micro,
                    estimated_fee_usd_label: estimated_fee_usd_micro.map(format_usd_micro_value),
                    fee_status,
                    fee_tier,
                    show_uncompensated_badge: false,
                    fee_status_detail: broadcaster_picker_fee_status_detail(candidate, policy),
                    fee_warning: broadcaster_candidate_fee_warning(candidate),
                    favorite: self.is_favorite_broadcaster(&candidate.railgun_address),
                    selected: selected_address.as_deref()
                        == Some(candidate.railgun_address.as_str()),
                    child_of: None,
                }
            })
            .collect::<Vec<_>>();
        let entries = project_broadcaster_picker_entries(
            &rows,
            picker.view_mode,
            !query.is_empty(),
            &picker.expanded_groups,
            &picker.collapsed_selected_children,
        );
        Some(BroadcasterPickerDialogSnapshot {
            query_input: picker.query_input.clone(),
            list: picker.list.clone(),
            scroll_indicator: picker.scroll_indicator.clone(),
            entries,
            empty_message,
            generating,
            query,
            filtered_count,
            total_count,
            show_all_broadcasters,
            fee_status_popover_open: picker.fee_status_popover_open,
            view_mode: picker.view_mode,
            selected_address,
            expanded_groups: picker.expanded_groups.clone(),
            collapsed_selected_children: picker.collapsed_selected_children.clone(),
            kind: picker.kind,
            key: picker.key,
        })
    }
}

pub(super) fn update_broadcaster_picker_group_expansion(
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

pub(super) fn broadcaster_picker_dialog_vertical_geometry(
    viewport_height: Pixels,
) -> (Pixels, Pixels) {
    let margin = viewport_height * 0.1;
    (margin, viewport_height - margin * 2.0)
}

impl ListDelegate for BroadcasterPickerDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.entries.len()
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        let show_section_divider =
            broadcaster_picker_section_divider_before(&self.entries, self.view_mode, ix.row);
        let entry = self.entries.get(ix.row)?.clone();
        let root = self.root.clone();
        let kind = self.kind;
        let key = self.key;
        match entry {
            BroadcasterPickerEntry::Group(group) => Some(
                ListItem::new(SharedString::from(broadcaster_picker_group_element_id(
                    group.key,
                )))
                .h(BROADCASTER_PICKER_ENTRY_HEIGHT)
                .px(px(0.0))
                .py(px(0.0))
                .disabled(true)
                .child(render_broadcaster_picker_entry_content(
                    render_broadcaster_picker_group(&group, root, self.generating),
                    show_section_divider,
                    BROADCASTER_PICKER_ENTRY_HEIGHT,
                    BROADCASTER_PICKER_SECTION_DIVIDER_INSET,
                )),
            ),
            BroadcasterPickerEntry::Broadcaster(row) => {
                let selected = row.selected;
                let railgun_address = row.railgun_address.clone();
                Some(
                    ListItem::new(SharedString::from(format!(
                        "broadcaster-picker-list-row-{}",
                        stable_broadcaster_element_suffix(&row.railgun_address)
                    )))
                    .h(BROADCASTER_PICKER_ENTRY_HEIGHT)
                    .px(BROADCASTER_PICKER_ROW_HORIZONTAL_PADDING)
                    .py(px(0.0))
                    .rounded_md()
                    .border_1()
                    .border_color(if selected {
                        rgb(theme::SUCCESS)
                    } else {
                        rgb(theme::SURFACE)
                    })
                    .disabled(self.generating)
                    .on_click(move |_event, window, cx| {
                        cx.stop_propagation();
                        let railgun_address = railgun_address.clone();
                        let _ = root.update(cx, |root, cx| {
                            root.choose_broadcaster_from_picker(
                                kind,
                                key,
                                railgun_address,
                                window,
                                cx,
                            );
                        });
                    })
                    .child(render_broadcaster_picker_entry_content(
                        render_broadcaster_picker_row(&row),
                        show_section_divider,
                        BROADCASTER_PICKER_ENTRY_HEIGHT,
                        px(0.0),
                    )),
                )
            }
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .p(px(16.0))
            .rounded_md()
            .bg(rgb(theme::SURFACE))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(app_muted_text(self.empty_message.clone()))
    }

    fn set_selected_index(
        &mut self,
        _ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
    }
}

pub(super) fn selected_broadcaster_label(
    choice: &BroadcasterChoice,
    candidates: &[PublicBroadcasterCandidate],
) -> String {
    let BroadcasterChoice::Specific { railgun_address } = choice else {
        return "Specific broadcaster".to_string();
    };
    candidates
        .iter()
        .find(|candidate| candidate.railgun_address == *railgun_address)
        .map_or_else(
            || "Specific unavailable".to_string(),
            broadcaster_candidate_label,
        )
}

pub(super) fn selected_broadcaster_fee_warning(
    choice: &BroadcasterChoice,
    candidates: &[PublicBroadcasterCandidate],
    allow_suspicious_broadcasters: bool,
) -> Option<String> {
    if allow_suspicious_broadcasters {
        return None;
    }
    let BroadcasterChoice::Specific { railgun_address } = choice else {
        return None;
    };
    candidates
        .iter()
        .find(|candidate| candidate.railgun_address == *railgun_address)
        .and_then(broadcaster_candidate_fee_warning)
}

const fn stable_broadcaster_element_suffix(railgun_address: &str) -> &str {
    railgun_address
}

pub(super) fn broadcaster_candidate_label(candidate: &PublicBroadcasterCandidate) -> String {
    format_broadcaster_address_label(&candidate.railgun_address, candidate.identifier.as_deref())
}

fn broadcaster_candidate_estimated_fee_amount(
    candidate: &PublicBroadcasterCandidate,
    context: Option<&BroadcasterPickerFeeEstimateContext>,
) -> Option<U256> {
    context.map(|context| {
        if candidate.railgun_address == context.railgun_address {
            context.fee_amount
        } else {
            buffered_public_broadcaster_fee(broadcaster_fee_amount(
                candidate.fee,
                context.gas_limit,
                context.service_gas_price,
            ))
        }
    })
}

#[cfg(test)]
pub(super) fn broadcaster_candidate_estimated_fee_amount_for_estimate(
    candidate: &PublicBroadcasterCandidate,
    estimate: &PublicBroadcasterCostEstimate,
) -> Option<U256> {
    let context = BroadcasterPickerFeeEstimateContext::from_estimate(estimate);
    broadcaster_candidate_estimated_fee_amount(candidate, Some(&context))
}

fn format_estimated_fee_amount(
    candidate: &PublicBroadcasterCandidate,
    amount: U256,
    registry: Option<&EffectiveTokenRegistry>,
) -> String {
    token_display_metadata(registry, candidate.chain_id, &candidate.token).map_or_else(
        || format!("{amount} raw token units"),
        |info| {
            format!(
                "{} {}",
                format_token_amount(amount, info.decimals),
                info.symbol
            )
        },
    )
}

pub(super) fn broadcaster_candidate_fee_warning(
    candidate: &PublicBroadcasterCandidate,
) -> Option<String> {
    let BroadcasterFeePolicyStatus::Suspicious { premium_bps, .. } = candidate.fee_policy_status
    else {
        return None;
    };
    Some(match premium_bps {
        Some(premium_bps) => format!(
            "Fee outside allowed range ({})",
            format_premium_bps_compact(premium_bps)
        ),
        None => "Fee outside allowed range".to_string(),
    })
}

fn format_premium_bps_compact(premium_bps: i128) -> String {
    let sign = if premium_bps >= 0 { "+" } else { "-" };
    let abs_bps = premium_bps.checked_abs().unwrap_or(i128::MAX);
    let tenths = (abs_bps + 5) / 10;
    if tenths % 10 == 0 {
        format!("{sign}{}%", tenths / 10)
    } else {
        format!("{sign}{}.{:01}%", tenths / 10, tenths % 10)
    }
}

pub(super) fn broadcaster_candidate_matches_query(
    candidate: &PublicBroadcasterCandidate,
    query: &str,
) -> bool {
    if query.is_empty() {
        return true;
    }
    candidate
        .railgun_address
        .to_ascii_lowercase()
        .contains(query)
        || candidate.fees_id.to_ascii_lowercase().contains(query)
        || candidate
            .identifier
            .as_deref()
            .is_some_and(|identifier| identifier.to_ascii_lowercase().contains(query))
        || candidate.version.to_ascii_lowercase().contains(query)
        || candidate
            .token
            .to_checksum(None)
            .to_ascii_lowercase()
            .contains(query)
}

pub(super) fn render_broadcaster_picker_header(
    root: &Entity<WalletRoot>,
    query_input: &Entity<InputState>,
    filtered_count: usize,
    total_count: usize,
    fee_status_popover_open: bool,
) -> gpui::Div {
    let broadcaster_header = if filtered_count == total_count {
        format!("Broadcaster ({total_count})")
    } else {
        format!("Broadcaster ({filtered_count} of {total_count})")
    };
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .px(BROADCASTER_PICKER_HEADER_HORIZONTAL_PADDING)
        .pt(BROADCASTER_PICKER_HEADER_TOP_PADDING)
        .text_size(px(11.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .child(
            div()
                .flex_1()
                .min_w(BROADCASTER_PICKER_PRIMARY_MIN_WIDTH)
                .truncate()
                .child(broadcaster_header),
        )
        .child(
            div()
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(BROADCASTER_PICKER_FEE_WIDTH)
                        .flex_shrink(1.0)
                        .min_w(px(0.0))
                        .truncate()
                        .child("Est. tx fee"),
                )
                .child(
                    div()
                        .w(BROADCASTER_PICKER_STATUS_WIDTH)
                        .flex_shrink(1.0)
                        .min_w(px(0.0))
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(div().flex_1().min_w(px(0.0)).truncate().child("Fee status"))
                        .child({
                            let popover_root = root.clone();
                            let focus_query_input = query_input.clone();
                            let tooltip_enabled = !fee_status_popover_open;
                            Popover::new("broadcaster-picker-fee-status-popover")
                                .open(fee_status_popover_open)
                                .on_open_change(move |open, window, cx| {
                                    popover_root.update(cx, |root, cx| {
                                        root.set_broadcaster_picker_fee_status_popover_open(
                                            *open, cx,
                                        );
                                    });
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
                                .content(|_state, _window, _cx| render_fee_status_popover())
                        }),
                ),
        )
}

fn render_fee_status_info_icon(tooltip_enabled: bool) -> impl IntoElement {
    div()
        .id("broadcaster-picker-fee-status-info")
        .size(px(14.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::WARNING))
        .text_color(rgb(theme::WARNING))
        .text_size(px(9.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .hover(|this| this.bg(rgb(theme::SURFACE_HOVER)))
        .child("i")
        .when(tooltip_enabled, |this| {
            this.tooltip(|window, cx| {
                Tooltip::element(|_window, _cx| render_fee_status_popover()).build(window, cx)
            })
        })
}

fn render_fee_status_popover() -> gpui::Div {
    div()
        .w(px(360.0))
        .p(px(12.0))
        .flex()
        .flex_col()
        .gap_2()
        .text_size(px(12.0))
        .text_color(rgb(theme::TEXT))
        .child(
            div()
                .text_color(rgb(theme::WARNING))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("Fee status"),
        )
        .child(div().child(
            "Est. tx fee includes gas cost and the broadcaster's fee.",
        ))
        .child(div().child(
            "Incentivised broadcasters charge more than gas cost, so submitting earns them something.",
        ))
}

fn render_broadcaster_picker_row(row: &BroadcasterPickerRow) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .text_size(APP_TEXT_SIZE)
        .child(
            div()
                .flex_1()
                .min_w(BROADCASTER_PICKER_PRIMARY_MIN_WIDTH)
                .when(row.child_of.is_some(), |this| {
                    this.pl(
                        BROADCASTER_PICKER_GROUP_TOGGLE_SIZE + BROADCASTER_PICKER_GROUP_PRIMARY_GAP
                    )
                })
                .overflow_hidden()
                .whitespace_nowrap()
                .flex()
                .items_center()
                .gap(px(5.0))
                .text_color(rgb(if row.fee_tier.is_muted() {
                    theme::TEXT_SUBTLE
                } else {
                    theme::TEXT
                }))
                .font_family(APP_MONO_FONT_FAMILY)
                .font_weight(if row.fee_tier.is_muted() {
                    gpui::FontWeight::NORMAL
                } else {
                    gpui::FontWeight::SEMIBOLD
                })
                .child(div().min_w(px(0.0)).truncate().child(row.label.clone()))
                .children(row.favorite.then(|| {
                    div()
                        .flex_shrink(1.0)
                        .flex()
                        .items_center()
                        .text_color(rgb(theme::WARNING))
                        .child(Icon::new(IconName::Star).with_size(px(13.0)))
                })),
        )
        .child(
            div()
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_1()
                .child(render_broadcaster_picker_fee_cell(row))
                .child(render_broadcaster_picker_status_cell(row)),
        )
}

fn render_broadcaster_picker_section_divider(inset: Pixels) -> impl IntoElement {
    Separator::horizontal()
        .absolute()
        .top(px(0.0))
        .left(inset)
        .right(inset)
        .h(px(1.0))
        .color(rgb(theme::BORDER))
}

fn render_broadcaster_picker_entry_content(
    content: impl IntoElement,
    show_section_divider: bool,
    height: Pixels,
    divider_inset: Pixels,
) -> gpui::Div {
    div()
        .relative()
        .w_full()
        .h(height)
        .flex()
        .items_center()
        .when(show_section_divider, |this| {
            this.child(render_broadcaster_picker_section_divider(divider_inset))
        })
        .child(content)
}

fn render_broadcaster_picker_fee_cell(row: &BroadcasterPickerRow) -> impl IntoElement {
    render_broadcaster_picker_estimated_fee_cell(
        &row.estimated_fee_label,
        row.estimated_fee_usd_label.as_deref(),
        row.fee_tier.is_muted(),
    )
}

fn render_broadcaster_picker_estimated_fee_cell(
    token_label: &str,
    usd_label: Option<&str>,
    muted: bool,
) -> impl IntoElement {
    let token_label = token_label.to_string();
    let usd_label = usd_label.map(str::to_string);
    let (primary_color, secondary_color) = broadcaster_picker_fee_text_colors(muted);
    div()
        .w(BROADCASTER_PICKER_FEE_WIDTH)
        .flex_shrink(1.0)
        .min_w(px(0.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(
            div()
                .w_full()
                .whitespace_nowrap()
                .text_color(rgb(primary_color))
                .font_weight(if muted {
                    gpui::FontWeight::NORMAL
                } else {
                    gpui::FontWeight::SEMIBOLD
                })
                .child(usd_label.clone().unwrap_or_else(|| token_label.clone())),
        )
        .children(usd_label.as_ref().map(|_| {
            div()
                .w_full()
                .whitespace_nowrap()
                .text_color(rgb(secondary_color))
                .text_size(px(11.0))
                .child(token_label)
        }))
}

fn render_broadcaster_picker_status_cell(row: &BroadcasterPickerRow) -> gpui::Div {
    let id = format!(
        "broadcaster-picker-status-{}",
        stable_broadcaster_element_suffix(&row.railgun_address)
    );
    render_broadcaster_picker_tier_cell(
        id,
        row.fee_tier,
        row.show_uncompensated_badge,
        SharedString::from(row.fee_status_detail.clone()),
    )
}

fn render_broadcaster_picker_group(
    group: &BroadcasterPickerGroup,
    root: WeakEntity<WalletRoot>,
    disabled: bool,
) -> impl IntoElement {
    let group_key = group.key;
    let expanded = group.expanded;
    let selected_child_address = group.selected_child_address.clone();
    let group_revision = group.revision.clone();
    let uncompensated_detail = SharedString::from(group.detail.clone());
    div()
        .id(SharedString::from(format!(
            "{}-card",
            broadcaster_picker_group_element_id(group.key)
        )))
        .w_full()
        .h(BROADCASTER_PICKER_ENTRY_HEIGHT - px(4.0))
        .px(BROADCASTER_PICKER_ROW_HORIZONTAL_PADDING)
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::SURFACE))
        .text_size(APP_TEXT_SIZE)
        .when(
            group.fee_tier == BroadcasterPickerTier::Uncompensated,
            move |this| {
                let detail = uncompensated_detail;
                this.tooltip(move |window, cx| {
                    let Some(width) = broadcaster_picker_status_tooltip_width(
                        window.viewport_size().width,
                        window.rem_size(),
                    ) else {
                        return Tooltip::element(|_window, _cx| div())
                            .m(px(0.0))
                            .p(px(0.0))
                            .border_0()
                            .build(window, cx);
                    };
                    let detail = detail.clone();
                    Tooltip::element(move |_window, _cx| {
                        render_broadcaster_picker_group_detail_tooltip(detail.clone(), width)
                    })
                    .build(window, cx)
                })
            },
        )
        .when(!disabled, |this| {
            this.cursor_pointer()
                .hover(|this| this.bg(rgb(theme::SURFACE_HOVER)))
                .on_click(move |_event, _window, cx| {
                    cx.stop_propagation();
                    let _ = root.update(cx, |root, cx| {
                        root.toggle_broadcaster_picker_group(
                            group_key,
                            expanded,
                            selected_child_address.clone(),
                            group_revision.clone(),
                            cx,
                        );
                    });
                })
        })
        .child(
            div()
                .flex_1()
                .min_w(BROADCASTER_PICKER_PRIMARY_MIN_WIDTH)
                .flex()
                .items_center()
                .gap(BROADCASTER_PICKER_GROUP_PRIMARY_GAP)
                .child(
                    div()
                        .size(BROADCASTER_PICKER_GROUP_TOGGLE_SIZE)
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
                            .with_size(px(12.0)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .font_weight(if group.fee_tier.is_muted() {
                            gpui::FontWeight::NORMAL
                        } else {
                            gpui::FontWeight::SEMIBOLD
                        })
                        .text_color(rgb(if group.fee_tier.is_muted() {
                            theme::TEXT_SUBTLE
                        } else {
                            theme::TEXT
                        }))
                        .child(group.label.clone()),
                ),
        )
        .child(
            div()
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap_1()
                .child(render_broadcaster_picker_estimated_fee_cell(
                    &group.estimated_fee_label,
                    group.estimated_fee_usd_label.as_deref(),
                    group.fee_tier.is_muted(),
                ))
                .child(render_broadcaster_picker_tier_cell(
                    broadcaster_picker_group_element_id(group.key),
                    group.fee_tier,
                    false,
                    SharedString::from(group.detail.clone()),
                )),
        )
}

fn render_broadcaster_picker_tier_cell(
    id: impl AsRef<str>,
    tier: BroadcasterPickerTier,
    show_uncompensated_badge: bool,
    detail: SharedString,
) -> gpui::Div {
    div()
        .w(BROADCASTER_PICKER_STATUS_WIDTH)
        .flex_shrink(1.0)
        .children(
            tier.badge_label(show_uncompensated_badge)
                .map(|label| render_broadcaster_picker_status_badge(id, tier, label, detail)),
        )
}

fn render_broadcaster_picker_status_badge(
    id: impl AsRef<str>,
    tier: BroadcasterPickerTier,
    label: &'static str,
    detail: SharedString,
) -> impl IntoElement {
    let color = status_tier_color(tier);
    let tooltip_revision = broadcaster_picker_status_tooltip_revision(tier, &detail);
    let tooltip_detail = detail;
    div()
        .id(SharedString::from(format!(
            "{}-badge-{tooltip_revision:016x}",
            id.as_ref()
        )))
        .w(BROADCASTER_PICKER_STATUS_WIDTH)
        .flex_shrink(1.0)
        .flex()
        .overflow_hidden()
        .items_center()
        .px(px(6.0))
        .py(px(4.0))
        .rounded_sm()
        .border_1()
        .border_color(rgb(color))
        .text_color(rgb(color))
        .text_size(px(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .tooltip(move |window, cx| {
            let Some(tooltip_width) = broadcaster_picker_status_tooltip_width(
                window.viewport_size().width,
                window.rem_size(),
            ) else {
                return Tooltip::element(|_window, _cx| div())
                    .m(px(0.0))
                    .p(px(0.0))
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
        .child(div().flex_1().min_w(px(0.0)).truncate().child(label))
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
        .py(px(2.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().size(px(7.0)).rounded_full().bg(rgb(color)))
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(color))
                        .child(label),
                ),
        )
        .child(
            Separator::horizontal()
                .color(rgb(theme::BORDER_SUBTLE))
                .my(px(1.0)),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .whitespace_normal()
                .text_size(px(12.0))
                .line_height(px(18.0))
                .text_color(rgb(theme::TEXT))
                .child(detail),
        )
}

fn render_broadcaster_picker_group_detail_tooltip(
    detail: SharedString,
    width: Pixels,
) -> gpui::Div {
    div()
        .w(width)
        .whitespace_normal()
        .text_size(px(12.0))
        .line_height(px(18.0))
        .text_color(rgb(theme::TEXT))
        .child(detail)
}

pub(super) fn broadcaster_picker_status_tooltip_width(
    viewport_width: Pixels,
    rem_size: Pixels,
) -> Option<Pixels> {
    let tooltip_chrome = rem_size * 2.5 + px(2.0);
    let available_width = viewport_width - tooltip_chrome;
    (available_width > px(0.0))
        .then(|| available_width.min(BROADCASTER_PICKER_STATUS_TOOLTIP_WIDTH))
}

pub(super) fn broadcaster_picker_status_tooltip_revision(
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

pub(super) const fn broadcaster_picker_fee_text_colors(muted: bool) -> (u32, u32) {
    if muted {
        (theme::TEXT_MUTED, theme::TEXT_SUBTLE)
    } else {
        (theme::TEXT, theme::TEXT_MUTED)
    }
}

fn broadcaster_picker_group_element_id(key: BroadcasterPickerGroupKey) -> String {
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

pub(super) fn broadcaster_choice_supported_by_candidates(
    choice: &BroadcasterChoice,
    candidates: &[PublicBroadcasterCandidate],
    policy: BroadcasterFeePolicy,
) -> bool {
    let BroadcasterChoice::Specific { railgun_address } = choice else {
        return true;
    };
    fee_policy_eligible_public_broadcasters(candidates, policy)
        .iter()
        .any(|candidate| candidate.railgun_address == *railgun_address)
}

pub(super) fn should_preserve_estimate_after_broadcaster_policy_change(
    choice: &BroadcasterChoice,
    resolved_random_broadcaster: Option<&str>,
    random_estimate_in_flight: bool,
    candidates: &[PublicBroadcasterCandidate],
    policy: BroadcasterFeePolicy,
) -> bool {
    let railgun_address = match choice {
        BroadcasterChoice::Specific { railgun_address } => Some(railgun_address.as_str()),
        BroadcasterChoice::Random if !random_estimate_in_flight => resolved_random_broadcaster,
        BroadcasterChoice::Random => None,
    };
    railgun_address.is_some_and(|railgun_address| {
        fee_policy_eligible_public_broadcasters(candidates, policy)
            .iter()
            .any(|candidate| candidate.railgun_address == railgun_address)
    })
}
