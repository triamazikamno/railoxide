use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
#[cfg(test)]
pub(super) use ui::broadcaster_picker::BroadcasterPickerTier;
pub(super) use ui::broadcaster_picker::{
    BroadcasterPickerEntry, BroadcasterPickerFeeStatus, BroadcasterPickerGroup,
    BroadcasterPickerGroupKey, BroadcasterPickerGroupRevision, BroadcasterPickerRow,
    BroadcasterPickerSelectedCollapse, BroadcasterPickerViewMode,
    broadcaster_picker_section_divider_before, project_broadcaster_picker_entries,
    update_broadcaster_picker_group_expansion,
};
pub(super) use ui::broadcaster_picker::{
    broadcaster_picker_group_element_id, render_broadcaster_picker_row,
};

use crate::assets::CHEVRONS_DOWN_ICON_PATH;
use alloy::primitives::U256;
use gpui::{
    App, AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Pixels, Render,
    SharedString, Size, Styled, WeakEntity, Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Icon, IndexPath, WindowExt,
    input::{InputEvent, InputState},
    list::{ListDelegate, ListItem, ListState},
    separator::Separator,
    window_paddings,
};
use railgun_ui::{
    chain_name, format_broadcaster_address_label, format_token_amount, format_usd_micro_value,
};
use ui::broadcaster_picker::BroadcasterPickerLayout;
use ui::controls::{app_muted_text, app_strong_text};
use ui::theme;
use wallet_ops::{
    BroadcasterFeePolicy, BroadcasterFeePolicyStatus, PublicBroadcasterCandidate,
    PublicBroadcasterCostEstimate, PublicBroadcasterSelection, broadcaster_fee_amount,
    buffered_public_broadcaster_fee, fee_policy_eligible_public_broadcasters,
    public_broadcaster_service_gas_price, settings::EffectiveTokenRegistry,
    sort_specific_public_broadcasters,
};

use super::retry::retry_backoff_delay;
use super::{
    DeliveryFormKind, DeliveryMode, PRIVATE_ASSET_LIST_WIDTH, UnshieldAssetKey, WalletRoot,
    dialogs::render_broadcaster_picker_dialog_content,
    private_action::{PrivateEstimateInput, PrivateEstimateOutput, PrivateEstimateRequest},
    token_display_label, token_display_metadata,
};

const BROADCASTER_PICKER_LIVE_UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const BROADCASTER_PICKER_DIALOG_FIXED_CHROME_HEIGHT: Pixels = px(100.0);
pub(super) const BROADCASTER_PICKER_MIN_LIST_HEIGHT: Pixels = px(120.0);
pub(super) const BROADCASTER_PICKER_LIST_HORIZONTAL_PADDING: Pixels = px(8.0);
pub(super) const BROADCASTER_PICKER_LIST_TOP_PADDING: Pixels = px(2.0);
pub(super) const BROADCASTER_PICKER_LIST_BOTTOM_PADDING: Pixels = px(8.0);

const BROADCASTER_PICKER_ROW_HORIZONTAL_PADDING: Pixels = px(12.0);

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
    last_rem_size: Option<Pixels>,
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
            last_rem_size: None,
        }
    }
}

impl Render for BroadcasterPickerScrollIndicator {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let viewport_size = window.viewport_size();
        let viewport_changed = self.last_viewport_size != Some(viewport_size)
            || self.last_rem_size != Some(window.rem_size());
        self.last_rem_size = Some(window.rem_size());
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BroadcasterPickerSuspiciousFeeDirection {
    BelowRange,
    AboveRange,
    Unrepresentable,
}

#[derive(Clone)]
pub(super) struct BroadcasterPickerFeeEstimateContext {
    railgun_address: String,
    fee_amount: U256,
    gas_limit: u64,
    service_gas_price: u128,
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
    selected_index: Option<IndexPath>,
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
            selected_index: None,
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
                .selectable(true)
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
        let join = self
            .runtime
            .spawn(async move { request.estimate(&http).await });
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
    ) -> Option<PrivateEstimateRequest> {
        let input = match kind {
            DeliveryFormKind::Send => {
                let form = self.send_forms.get(&key)?;
                if form.generating || form.delivery_mode != DeliveryMode::PublicBroadcaster {
                    return None;
                }
                PrivateEstimateInput {
                    asset: form.asset.clone(),
                    recipient: String::new(),
                    amount: form.amount_input.read(cx).value().to_string(),
                    broadcaster: form.broadcaster_choice.clone(),
                    fee_token: form.selected_fee_token,
                    fee_mode: form.fee_mode,
                    allow_out_of_range: form.allow_suspicious_broadcasters,
                    favorites_only: form.favorites_only_broadcasters,
                    output: PrivateEstimateOutput::Send,
                }
            }
            DeliveryFormKind::Unshield => {
                let form = self.unshield_forms.get(&key)?;
                if form.generating || form.delivery_mode != DeliveryMode::PublicBroadcaster {
                    return None;
                }
                PrivateEstimateInput {
                    asset: form.asset.clone(),
                    recipient: String::new(),
                    amount: form.amount_input.read(cx).value().to_string(),
                    broadcaster: form.broadcaster_choice.clone(),
                    fee_token: form.selected_fee_token,
                    fee_mode: form.fee_mode,
                    allow_out_of_range: form.allow_suspicious_broadcasters,
                    favorites_only: form.favorites_only_broadcasters,
                    output: PrivateEstimateOutput::Unshield {
                        unwrap: form.unwrap,
                        native_top_up: form
                            .native_top_up_enabled
                            .then(|| form.native_top_up.clone())
                            .flatten(),
                    },
                }
            }
        };
        self.prepare_private_picker_estimate(input)
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
        let rows = self.private_broadcaster_picker_rows(
            &candidates,
            policy,
            fee_estimate_context.as_ref(),
            selected_address.as_deref(),
            estimated_fee_placeholder,
        );
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
        window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        let entry_height = window.rem_size() * 5.25;
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
                .h(entry_height)
                .px(px(0.0))
                .py(px(0.0))
                .disabled(self.generating)
                .child(render_broadcaster_picker_entry_content(
                    render_broadcaster_picker_group(&group, root, self.generating),
                    show_section_divider,
                    entry_height,
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
                    .h(entry_height)
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
                        render_broadcaster_picker_row(&row, BroadcasterPickerLayout::Standard),
                        show_section_divider,
                        entry_height,
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
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) {
        if self.generating {
            return;
        }
        let Some(entry) = self
            .selected_index
            .and_then(|ix| self.entries.get(ix.row))
            .cloned()
        else {
            return;
        };
        let kind = self.kind;
        let key = self.key;
        let _ = self.root.update(cx, |root, cx| match entry {
            BroadcasterPickerEntry::Broadcaster(row) => {
                root.choose_broadcaster_from_picker(kind, key, row.railgun_address, window, cx);
            }
            BroadcasterPickerEntry::Group(group) => root.toggle_broadcaster_picker_group(
                group.key,
                group.expanded,
                group.selected_child_address,
                group.revision,
                cx,
            ),
        });
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

fn render_broadcaster_picker_group(
    group: &BroadcasterPickerGroup,
    root: WeakEntity<WalletRoot>,
    disabled: bool,
) -> impl IntoElement {
    let group = group.clone();
    let command = group.clone();
    ui::broadcaster_picker::render_broadcaster_picker_group(
        group,
        BroadcasterPickerLayout::Standard,
        move |_, cx| {
            let _ = root.update(cx, |root, cx| {
                root.toggle_broadcaster_picker_group(
                    command.key,
                    command.expanded,
                    command.selected_child_address.clone(),
                    command.revision.clone(),
                    cx,
                );
            });
        },
        disabled,
    )
}

pub(super) fn render_broadcaster_picker_header(
    root: &Entity<WalletRoot>,
    query_input: &Entity<InputState>,
    filtered_count: usize,
    total_count: usize,
    fee_status_popover_open: bool,
) -> gpui::Div {
    let root = root.clone();
    ui::broadcaster_picker::render_broadcaster_picker_header(
        BroadcasterPickerLayout::Standard,
        query_input,
        filtered_count,
        total_count,
        fee_status_popover_open,
        move |open, cx| {
            root.update(cx, |root, cx| {
                root.set_broadcaster_picker_fee_status_popover_open(open, cx);
            });
        },
    )
}

impl WalletRoot {
    pub(in crate::root) fn private_broadcaster_picker_rows(
        &self,
        candidates: &[PublicBroadcasterCandidate],
        policy: BroadcasterFeePolicy,
        fee_estimate_context: Option<&BroadcasterPickerFeeEstimateContext>,
        selected_address: Option<&str>,
        estimated_fee_placeholder: &str,
    ) -> Vec<BroadcasterPickerRow> {
        candidates
            .iter()
            .enumerate()
            .map(|(sort_order, candidate)| {
                let estimated_fee_amount =
                    broadcaster_candidate_estimated_fee_amount(candidate, fee_estimate_context);
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
                    selected: selected_address == Some(candidate.railgun_address.as_str()),
                    child_of: None,
                }
            })
            .collect::<Vec<_>>()
    }
}
