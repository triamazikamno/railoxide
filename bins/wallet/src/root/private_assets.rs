use std::collections::BTreeMap;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use gpui::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, Pixels, SharedString, Styled,
    Window, div, img, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt, button::ButtonVariants,
    scroll::ScrollableElement,
};
use railgun_ui::{format_token_amount, format_usd_micro_value, short_address};
use ui::controls::{app_button, app_button_base, app_muted_text, app_strong_text};
use ui::theme::{self};
use wallet_ops::{
    ListUtxosOutput, TokenAnchorRateCache, TokenTotal, UtxoPpoiState, WalletPpoiWorkflowStatus,
    max_send_amount_from_outputs as planner_max_send_amount_from_outputs,
    max_unshield_amount_from_outputs as planner_max_unshield_amount_from_outputs,
    settings::EffectiveTokenRegistry,
};

use crate::assets::{RailgunActionIcon, WalletIconSource};

use super::chain_load::loading_summary;
use super::public_account::render_public_address_qr_dialog_content;
use super::utxo::{
    UtxoDisplayRow, blocked_shield_refund_action_available, blocked_shield_refund_origin_resolving,
    blocked_shield_rescue_display_rows, display_rows_from_output, now_epoch_secs,
    ppoi_workflow_status_detail, ppoi_workflow_status_title, recoverable_poi_candidate_count,
    shield_poi_wait_display, shield_poi_wait_time_display, short_hash,
};
use super::{
    ChainUtxoState, PUBLIC_ADDRESS_QR_DIALOG_WIDTH, UnshieldAsset, WalletRoot, centered_message,
    count_label, dialog_max_height, parse_address, rgb_with_alpha, secondary_dialog_content_width,
    token_display_metadata,
};

#[cfg(test)]
use super::UnshieldAssetKey;

#[derive(Clone)]
pub(super) struct FormattedTokenTotal {
    pub(super) chain_id: u64,
    pub(super) token: Option<Address>,
    pub(super) label: String,
    pub(super) amount: String,
    pub(super) usd_micro_amount: Option<U256>,
    pub(super) usd_amount: Option<String>,
    pub(super) pending_poi_amount: String,
    pub(super) pending_incoming_amount: String,
    pub(super) pending_outgoing_amount: String,
    pub(super) total: Option<U256>,
    pub(super) poi_verified_total: Option<U256>,
    pub(super) pending_poi_total: Option<U256>,
    pub(super) pending_incoming_total: Option<U256>,
    pub(super) pending_outgoing_total: Option<U256>,
    pub(super) decimals: Option<u8>,
    pub(super) icon_path: Option<WalletIconSource>,
}

#[derive(Clone)]
struct PrivatePendingAssetLine {
    label: String,
    amount: String,
    shield_wait: Option<PrivatePendingShieldWait>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PrivatePendingShieldWait {
    pub(super) output_count: usize,
    pub(super) latest_source_block_timestamp: u64,
    pub(super) total_value: Option<U256>,
    pub(super) has_delayed: bool,
}

#[derive(Clone)]
pub(super) struct PrivatePendingSummary {
    blocked_shield_rows: Vec<UtxoDisplayRow>,
    pending_poi_assets: Vec<PrivatePendingAssetLine>,
    pending_incoming_assets: Vec<PrivatePendingAssetLine>,
    pending_outgoing_assets: Vec<PrivatePendingAssetLine>,
    affected_asset_count: usize,
    recoverable_poi_outputs: usize,
    recoverable_ppoi_states: Vec<UtxoPpoiState>,
    ppoi_workflow_status: WalletPpoiWorkflowStatus,
    poi_refreshing: bool,
    poi_refresh_session: Option<Arc<wallet_ops::WalletSession>>,
}

pub(super) fn max_unshield_amount_from_snapshot(
    snapshot: &ListUtxosOutput,
    token: Address,
) -> U256 {
    planner_max_unshield_amount_from_outputs(&snapshot.utxos, token)
}

pub(super) fn max_send_amount_from_snapshot(snapshot: &ListUtxosOutput, token: Address) -> U256 {
    planner_max_send_amount_from_outputs(&snapshot.utxos, token)
}

pub(super) fn format_private_asset_rows_from_snapshot(
    snapshot: &ListUtxosOutput,
    registry: Option<&EffectiveTokenRegistry>,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> Vec<FormattedTokenTotal> {
    let mut rows =
        format_private_asset_rows(snapshot.chain_id, &snapshot.totals, registry, anchor_cache);
    apply_pending_asset_amounts(snapshot, registry, anchor_cache, &mut rows);
    rows.sort_by_key(|b| std::cmp::Reverse(b.usd_micro_amount));
    rows
}

pub(super) fn format_private_asset_rows(
    chain_id: u64,
    totals: &[TokenTotal],
    registry: Option<&EffectiveTokenRegistry>,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> Vec<FormattedTokenTotal> {
    totals
        .iter()
        .map(|total| format_total_parts(chain_id, total, registry, anchor_cache))
        .collect()
}

pub(super) fn private_asset_display_amounts(
    asset: &FormattedTokenTotal,
) -> (String, Option<String>) {
    match asset.usd_amount.as_ref() {
        Some(usd_amount) => (
            usd_amount.clone(),
            Some(format!("{} {}", asset.amount, asset.label)),
        ),
        None => (asset.amount.clone(), None),
    }
}

pub(super) const fn private_send_action_tooltip(
    can_send: bool,
    actions_available: bool,
    syncing: bool,
    unavailable_balance_message: &'static str,
) -> &'static str {
    private_action_tooltip(
        can_send,
        actions_available,
        syncing,
        "Prepare private send calldata",
        "Open private send form while wallet sync finishes",
        unavailable_balance_message,
    )
}

pub(super) const fn private_unshield_action_tooltip(
    can_unshield: bool,
    actions_available: bool,
    syncing: bool,
    unavailable_balance_message: &'static str,
) -> &'static str {
    private_action_tooltip(
        can_unshield,
        actions_available,
        syncing,
        "Prepare unshield calldata",
        "Open unshield form while wallet sync finishes",
        unavailable_balance_message,
    )
}

const fn private_action_tooltip(
    can_act: bool,
    actions_available: bool,
    syncing: bool,
    ready_message: &'static str,
    syncing_message: &'static str,
    unavailable_balance_message: &'static str,
) -> &'static str {
    if can_act && syncing {
        syncing_message
    } else if can_act {
        ready_message
    } else if actions_available {
        unavailable_balance_message
    } else {
        "Available after wallet session starts"
    }
}

pub(super) fn total_private_balance_usd_amount(rows: &[FormattedTokenTotal]) -> Option<String> {
    let mut total: Option<U256> = None;
    for amount in rows.iter().filter_map(|row| row.usd_micro_amount) {
        total = Some(match total {
            Some(total) => total.checked_add(amount)?,
            None => amount,
        });
    }
    total.map(format_usd_micro_value)
}

#[cfg(test)]
pub(super) fn format_total(chain_id: u64, total: &TokenTotal) -> String {
    let formatted = format_total_parts(chain_id, total, None, None);
    format!("{} {}", formatted.label, formatted.amount)
}

fn format_total_parts(
    chain_id: u64,
    total: &TokenTotal,
    registry: Option<&EffectiveTokenRegistry>,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> FormattedTokenTotal {
    let total_raw = U256::from_str_radix(&total.total, 10).ok();
    let poi_verified_total_raw = U256::from_str_radix(&total.poi_verified_total, 10).ok();
    let pending_poi_total = pending_poi_total(total_raw, poi_verified_total_raw);
    let Some(address) = parse_address(&total.token) else {
        return FormattedTokenTotal {
            chain_id,
            token: None,
            label: total.token.clone(),
            amount: total.total.clone(),
            usd_micro_amount: None,
            usd_amount: None,
            pending_poi_amount: format_pending_poi_amount(pending_poi_total, None),
            pending_incoming_amount: "0".to_string(),
            pending_outgoing_amount: "0".to_string(),
            total: total_raw,
            poi_verified_total: poi_verified_total_raw,
            pending_poi_total,
            pending_incoming_total: Some(U256::ZERO),
            pending_outgoing_total: Some(U256::ZERO),
            decimals: None,
            icon_path: None,
        };
    };
    let Some(token) = token_display_metadata(registry, chain_id, &address) else {
        return FormattedTokenTotal {
            chain_id,
            token: Some(address),
            label: short_address(&address),
            amount: total.total.clone(),
            usd_micro_amount: None,
            usd_amount: None,
            pending_poi_amount: format_pending_poi_amount(pending_poi_total, None),
            pending_incoming_amount: "0".to_string(),
            pending_outgoing_amount: "0".to_string(),
            total: total_raw,
            poi_verified_total: poi_verified_total_raw,
            pending_poi_total,
            pending_incoming_total: Some(U256::ZERO),
            pending_outgoing_total: Some(U256::ZERO),
            decimals: None,
            icon_path: None,
        };
    };
    let amount = total_raw.map_or_else(
        || total.total.clone(),
        |value| format_token_amount(value, token.decimals),
    );
    let usd_micro_amount = total_raw
        .and_then(|value| private_asset_usd_micro_amount(chain_id, address, value, anchor_cache));
    let usd_amount = usd_micro_amount.map(format_usd_micro_value);
    FormattedTokenTotal {
        chain_id,
        token: Some(address),
        label: token.symbol,
        amount,
        usd_micro_amount,
        usd_amount,
        pending_poi_amount: format_pending_poi_amount(pending_poi_total, Some(token.decimals)),
        pending_incoming_amount: "0".to_string(),
        pending_outgoing_amount: "0".to_string(),
        total: total_raw,
        poi_verified_total: poi_verified_total_raw,
        pending_poi_total,
        pending_incoming_total: Some(U256::ZERO),
        pending_outgoing_total: Some(U256::ZERO),
        decimals: Some(token.decimals),
        icon_path: token.icon_path,
    }
}

fn apply_pending_asset_amounts(
    snapshot: &ListUtxosOutput,
    registry: Option<&EffectiveTokenRegistry>,
    anchor_cache: Option<&TokenAnchorRateCache>,
    rows: &mut Vec<FormattedTokenTotal>,
) {
    let mut pending: BTreeMap<Address, (U256, U256)> = BTreeMap::new();
    for row in &snapshot.utxos {
        if !row.pending_new && !row.pending_spent && !row.local_pending_spent {
            continue;
        }
        let Some(token) = parse_address(&row.token) else {
            continue;
        };
        let Ok(value) = U256::from_str_radix(&row.value, 10) else {
            continue;
        };
        let entry = pending.entry(token).or_default();
        if row.pending_new {
            entry.0 += value;
        }
        if row.pending_spent || row.local_pending_spent {
            entry.1 += value;
        }
    }

    for (token, (incoming, outgoing)) in pending {
        let index = rows.iter().position(|row| row.token == Some(token));
        let row = if let Some(index) = index {
            &mut rows[index]
        } else {
            rows.push(format_total_parts(
                snapshot.chain_id,
                &TokenTotal {
                    token: token.to_checksum(None),
                    total: "0".to_string(),
                    poi_verified_total: "0".to_string(),
                },
                registry,
                anchor_cache,
            ));
            rows.last_mut().expect("pending row inserted")
        };
        row.pending_incoming_total = Some(incoming);
        row.pending_outgoing_total = Some(outgoing);
        row.pending_incoming_amount = format_pending_amount(incoming, row.decimals);
        row.pending_outgoing_amount = format_pending_amount(outgoing, row.decimals);
    }
}

fn private_asset_usd_micro_amount(
    chain_id: u64,
    token: Address,
    total: U256,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> Option<U256> {
    anchor_cache.and_then(|cache| cache.cached_token_usd_micro_value(chain_id, token, total))
}

fn pending_poi_total(total: Option<U256>, poi_verified_total: Option<U256>) -> Option<U256> {
    total
        .zip(poi_verified_total)
        .map(|(total, poi_verified_total)| total.saturating_sub(poi_verified_total))
}

fn format_pending_poi_amount(pending_poi_total: Option<U256>, decimals: Option<u8>) -> String {
    pending_poi_total.as_ref().map_or_else(
        || "0".to_string(),
        |value| {
            if let Some(decimals) = decimals {
                format_token_amount(*value, decimals)
            } else {
                value.to_string()
            }
        },
    )
}

fn format_pending_amount(value: U256, decimals: Option<u8>) -> String {
    decimals.map_or_else(
        || value.to_string(),
        |decimals| format_token_amount(value, decimals),
    )
}

pub(super) fn should_show_pending_poi_amount(pending_poi_total: Option<U256>) -> bool {
    pending_poi_total.is_some_and(|amount| !amount.is_zero())
}

pub(super) fn should_show_pending_amount(pending_total: Option<U256>) -> bool {
    pending_total.is_some_and(|amount| !amount.is_zero())
}

#[cfg(test)]
pub(super) fn private_pending_summary(
    assets: &[FormattedTokenTotal],
    snapshot: &ListUtxosOutput,
    blocked_shield_rows: Vec<UtxoDisplayRow>,
    poi_refreshing: bool,
    poi_refresh_session: Option<Arc<wallet_ops::WalletSession>>,
) -> Option<PrivatePendingSummary> {
    private_pending_summary_with_workflow(
        assets,
        snapshot,
        blocked_shield_rows,
        WalletPpoiWorkflowStatus::default(),
        poi_refreshing,
        poi_refresh_session,
    )
}

pub(super) fn private_pending_summary_with_workflow(
    assets: &[FormattedTokenTotal],
    snapshot: &ListUtxosOutput,
    blocked_shield_rows: Vec<UtxoDisplayRow>,
    ppoi_workflow_status: WalletPpoiWorkflowStatus,
    poi_refreshing: bool,
    poi_refresh_session: Option<Arc<wallet_ops::WalletSession>>,
) -> Option<PrivatePendingSummary> {
    let shield_waits = pending_shield_waits_by_token(snapshot, now_epoch_secs());
    let pending_poi_assets = assets
        .iter()
        .filter(|asset| should_show_pending_poi_amount(asset.pending_poi_total))
        .map(|asset| PrivatePendingAssetLine {
            label: asset.label.clone(),
            amount: asset.pending_poi_amount.clone(),
            shield_wait: asset
                .token
                .and_then(|token| shield_waits.get(&token).copied())
                .filter(|wait| pending_shield_wait_matches_total(*wait, asset.pending_poi_total)),
        })
        .collect::<Vec<_>>();
    let pending_incoming_assets = assets
        .iter()
        .filter(|asset| should_show_pending_amount(asset.pending_incoming_total))
        .map(|asset| PrivatePendingAssetLine {
            label: asset.label.clone(),
            amount: asset.pending_incoming_amount.clone(),
            shield_wait: None,
        })
        .collect::<Vec<_>>();
    let pending_outgoing_assets = assets
        .iter()
        .filter(|asset| should_show_pending_amount(asset.pending_outgoing_total))
        .map(|asset| PrivatePendingAssetLine {
            label: asset.label.clone(),
            amount: asset.pending_outgoing_amount.clone(),
            shield_wait: None,
        })
        .collect::<Vec<_>>();
    let affected_asset_count = assets
        .iter()
        .filter(|asset| {
            should_show_pending_poi_amount(asset.pending_poi_total)
                || should_show_pending_amount(asset.pending_incoming_total)
                || should_show_pending_amount(asset.pending_outgoing_total)
                || asset.token.is_some_and(|token| {
                    snapshot.utxos.iter().any(|row| {
                        row.is_ppoi_retry_eligible() && parse_address(&row.token) == Some(token)
                    })
                })
        })
        .count();
    let recoverable_poi_outputs = recoverable_poi_candidate_count(snapshot);
    let mut recoverable_ppoi_states = Vec::new();
    for state in snapshot
        .utxos
        .iter()
        .filter(|row| row.is_ppoi_retry_eligible())
        .map(|row| row.ppoi_state)
    {
        if !recoverable_ppoi_states.contains(&state) {
            recoverable_ppoi_states.push(state);
        }
    }

    if pending_poi_assets.is_empty()
        && pending_incoming_assets.is_empty()
        && pending_outgoing_assets.is_empty()
        && blocked_shield_rows.is_empty()
        && recoverable_poi_outputs == 0
        && !ppoi_workflow_status.has_outstanding()
    {
        return None;
    }

    Some(PrivatePendingSummary {
        blocked_shield_rows,
        pending_poi_assets,
        pending_incoming_assets,
        pending_outgoing_assets,
        affected_asset_count,
        recoverable_poi_outputs,
        recoverable_ppoi_states,
        ppoi_workflow_status,
        poi_refreshing,
        poi_refresh_session,
    })
}

pub(super) fn pending_shield_waits_by_token(
    snapshot: &ListUtxosOutput,
    now_epoch_secs: u64,
) -> BTreeMap<Address, PrivatePendingShieldWait> {
    let mut waits = BTreeMap::new();
    for row in display_rows_from_output(snapshot, "", false) {
        let Some(display) = shield_poi_wait_display(&row, now_epoch_secs) else {
            continue;
        };
        let Some(token) = parse_address(&row.token_address) else {
            continue;
        };
        let wait = waits.entry(token).or_insert(PrivatePendingShieldWait {
            output_count: 0,
            latest_source_block_timestamp: 0,
            total_value: Some(U256::ZERO),
            has_delayed: false,
        });
        wait.output_count += 1;
        wait.latest_source_block_timestamp = wait
            .latest_source_block_timestamp
            .max(row.source_block_timestamp);
        wait.total_value = wait
            .total_value
            .and_then(|total| row.raw_value.and_then(|value| total.checked_add(value)));
        wait.has_delayed |= display.delayed;
    }
    waits
}

pub(super) fn pending_shield_wait_matches_total(
    wait: PrivatePendingShieldWait,
    pending_poi_total: Option<U256>,
) -> bool {
    wait.total_value
        .is_some_and(|total_value| pending_poi_total == Some(total_value))
}

pub(super) const fn private_pending_summary_detail(
    summary: &PrivatePendingSummary,
) -> Option<&'static str> {
    if summary.blocked_shield_rows.is_empty() {
        None
    } else if summary.blocked_shield_rows.len() == 1 {
        Some("A blocked Shield can be reviewed and refunded.")
    } else {
        Some("Blocked Shields can be reviewed and refunded.")
    }
}

pub(super) fn private_pending_summary_title(summary: &PrivatePendingSummary) -> String {
    if !summary.blocked_shield_rows.is_empty() {
        "Private assets need attention".to_string()
    } else if !summary.pending_poi_assets.is_empty()
        || !summary.pending_incoming_assets.is_empty()
        || summary.recoverable_poi_outputs > 0
    {
        format!(
            "{} not yet spendable",
            pending_asset_count_label(summary.affected_asset_count)
        )
    } else if summary.ppoi_workflow_status.needs_attention > 0
        || summary.ppoi_workflow_status.recovery_needs_attention > 0
    {
        "Outgoing proof recovery needs attention".to_string()
    } else if summary.ppoi_workflow_status.awaiting_public_txid_data > 0 {
        "Waiting for public transaction proof data".to_string()
    } else if summary.ppoi_workflow_status.awaiting_poi_data > 0 {
        "Waiting for PPOI data".to_string()
    } else if summary.ppoi_workflow_status.awaiting_validation > 0
        && summary.ppoi_workflow_status.awaiting_recovery == 0
        && summary.ppoi_workflow_status.awaiting_submission == 0
    {
        "Awaiting outgoing proof verification".to_string()
    } else if summary.ppoi_workflow_status.has_outstanding() {
        "Outgoing proof recovery pending".to_string()
    } else {
        format!(
            "{} awaiting confirmation",
            pending_asset_count_label(summary.affected_asset_count)
        )
    }
}

pub(super) fn build_unshield_asset(
    snapshot: &ListUtxosOutput,
    asset: &FormattedTokenTotal,
) -> Option<UnshieldAsset> {
    let token = asset.token?;
    let total = asset.total?;
    let poi_verified_total = asset.poi_verified_total?;
    let max_batched = max_unshield_amount_from_snapshot(snapshot, token);
    if max_batched.is_zero() {
        return None;
    }
    Some(UnshieldAsset {
        chain_id: asset.chain_id,
        token,
        label: asset.label.clone(),
        decimals: asset.decimals,
        total,
        poi_verified_total,
        max_batched,
        icon_path: asset.icon_path.clone(),
    })
}

pub(super) fn build_send_asset(
    snapshot: &ListUtxosOutput,
    asset: &FormattedTokenTotal,
) -> Option<UnshieldAsset> {
    let token = asset.token?;
    let total = asset.total?;
    let poi_verified_total = asset.poi_verified_total?;
    let max_batched = max_send_amount_from_snapshot(snapshot, token);
    if max_batched.is_zero() {
        return None;
    }
    Some(UnshieldAsset {
        chain_id: asset.chain_id,
        token,
        label: asset.label.clone(),
        decimals: asset.decimals,
        total,
        poi_verified_total,
        max_batched,
        icon_path: asset.icon_path.clone(),
    })
}

impl WalletRoot {
    fn open_private_receive_address_dialog(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(address) = self
            .view_session
            .as_ref()
            .and_then(|session| session.receive_address().ok())
        else {
            return;
        };
        window.close_all_dialogs(cx);
        let dialog_width =
            (window.viewport_size().width * 0.92).min(PUBLIC_ADDRESS_QR_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let address_text = SharedString::from(address);
        let copy_id = SharedString::from("wallet-private-receive-address-copy");
        window.open_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Private receive address"))
                .child(render_public_address_qr_dialog_content(
                    None,
                    address_text.clone(),
                    None,
                    copy_id.clone(),
                    content_width,
                ))
        });
    }

    pub(super) fn render_private_assets_body(&self, root: &Entity<Self>) -> gpui::AnyElement {
        if self.view_session.is_none() {
            return centered_message("Choose a wallet to continue").into_any_element();
        }
        match self.chain_states.get(&self.selected_chain) {
            Some(ChainUtxoState::Error { message, .. }) => self
                .render_chain_error_body(root, message.as_ref())
                .into_any_element(),
            Some(ChainUtxoState::Loading { progress }) => {
                centered_message(loading_summary(*progress)).into_any_element()
            }
            Some(
                state @ ChainUtxoState::Syncing {
                    snapshot, progress, ..
                },
            ) => self.render_private_asset_snapshot(
                root,
                snapshot,
                state.private_action_forms_available(),
                true,
                *progress,
            ),
            Some(state @ ChainUtxoState::Ready { snapshot, .. }) => self
                .render_private_asset_snapshot(
                    root,
                    snapshot,
                    state.private_action_forms_available(),
                    false,
                    None,
                ),
            Some(ChainUtxoState::Idle) | None => {
                centered_message("Select a chain to load private balances").into_any_element()
            }
        }
    }

    fn render_private_asset_snapshot(
        &self,
        root: &Entity<Self>,
        snapshot: &ListUtxosOutput,
        actions_available: bool,
        syncing: bool,
        progress: Option<wallet_ops::SyncProgressUpdate>,
    ) -> gpui::AnyElement {
        let assets = format_private_asset_rows_from_snapshot(
            snapshot,
            Some(&self.effective_token_registry),
            Some(&self.public_broadcaster_anchor_cache),
        );
        let total_balance = total_private_balance_usd_amount(&assets);
        let receive_available = self
            .view_session
            .as_ref()
            .and_then(|session| session.receive_address().ok())
            .is_some();
        let send_asset = assets
            .iter()
            .find_map(|asset| build_send_asset(snapshot, asset));
        let unshield_asset = assets
            .iter()
            .find_map(|asset| build_unshield_asset(snapshot, asset));
        let pending_summary = self.private_pending_summary_from_snapshot(snapshot, &assets);
        if assets.is_empty() {
            let message = if syncing {
                loading_summary(progress)
            } else {
                "No private assets found".to_string()
            };
            if let Some(summary) = pending_summary.as_ref() {
                let empty = if receive_available {
                    Self::render_private_empty_state(root.clone(), message, receive_available)
                        .into_any_element()
                } else {
                    centered_message(message).into_any_element()
                };
                return div()
                    .size_full()
                    .min_w(px(0.0))
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .py(px(16.0))
                    .child(
                        div()
                            .w(super::PRIVATE_ASSET_LIST_WIDTH)
                            .max_w_full()
                            .mx_auto()
                            .child(Self::render_private_pending_status_card(root, summary)),
                    )
                    .child(div().flex_1().min_h(px(0.0)).child(empty))
                    .into_any_element();
            }
            if receive_available {
                return Self::render_private_empty_state(root.clone(), message, receive_available)
                    .into_any_element();
            }
            return centered_message(message).into_any_element();
        }
        let has_total_balance = total_balance.is_some();

        div()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .overflow_y_scrollbar()
            .child(
                div()
                    .w(super::PRIVATE_ASSET_LIST_WIDTH)
                    .max_w_full()
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .py(px(16.0))
                    .when_some(total_balance, |column, total_balance| {
                        column.child(Self::render_private_balance_hero(
                            root.clone(),
                            total_balance,
                            receive_available,
                            send_asset,
                            unshield_asset,
                            actions_available,
                            syncing,
                        ))
                    })
                    .when(!has_total_balance && receive_available, |column| {
                        column.child(Self::render_private_receive_action(
                            root.clone(),
                            receive_available,
                        ))
                    })
                    .when_some(pending_summary, |column, summary| {
                        column.child(Self::render_private_pending_status_card(root, &summary))
                    })
                    .children(assets.into_iter().enumerate().map(|(ix, asset)| {
                        Self::render_private_asset_row(
                            root.clone(),
                            ix,
                            asset,
                            snapshot,
                            actions_available,
                            syncing,
                        )
                        .into_any_element()
                    })),
            )
            .into_any_element()
    }

    fn render_private_empty_state(
        root: Entity<Self>,
        message: String,
        receive_available: bool,
    ) -> gpui::Div {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .child(app_muted_text(message))
                    .child(Self::render_private_receive_button(
                        root,
                        "wallet-private-empty-receive",
                        receive_available,
                    )),
            )
    }

    fn render_private_receive_action(root: Entity<Self>, receive_available: bool) -> gpui::Div {
        div()
            .w_full()
            .flex()
            .justify_center()
            .child(Self::render_private_receive_button(
                root,
                "wallet-private-standalone-receive",
                receive_available,
            ))
    }

    fn render_private_receive_button(
        root: Entity<Self>,
        id: &'static str,
        receive_available: bool,
    ) -> impl IntoElement {
        app_button(id, "Receive")
            .child(Icon::new(RailgunActionIcon::QrCode).small())
            .outline()
            .when(receive_available, |button| button.bg(rgb(theme::SURFACE)))
            .disabled(!receive_available)
            .tooltip("Show private receive address")
            .on_click(move |_event, window, cx| {
                root.update(cx, |root, cx| {
                    root.open_private_receive_address_dialog(window, cx);
                });
            })
    }

    pub(super) fn open_private_pending_status_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.private_pending_status_dialog_open = true;
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(520.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            let content = content_root
                .read(cx)
                .current_private_pending_summary()
                .map_or_else(
                    || private_pending_status_empty(content_width),
                    |summary| {
                        Self::render_private_pending_status_dialog_content(
                            &content_root,
                            &summary,
                            content_width,
                        )
                    },
                );
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Private asset status"))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, _cx| {
                        root.private_pending_status_dialog_open = false;
                    });
                })
                .child(content)
        });
    }

    fn current_private_pending_summary(&self) -> Option<PrivatePendingSummary> {
        let snapshot = self.chain_states.get(&self.selected_chain)?.snapshot()?;
        let assets = format_private_asset_rows_from_snapshot(
            snapshot,
            Some(&self.effective_token_registry),
            Some(&self.public_broadcaster_anchor_cache),
        );
        self.private_pending_summary_from_snapshot(snapshot, &assets)
    }

    fn private_pending_summary_from_snapshot(
        &self,
        snapshot: &ListUtxosOutput,
        assets: &[FormattedTokenTotal],
    ) -> Option<PrivatePendingSummary> {
        let chain_state = self.chain_states.get(&self.selected_chain);
        let blocked_shield_rows = blocked_shield_rescue_display_rows(
            snapshot,
            &self.blocked_shield_rescue_rows,
            &self.blocked_shield_refunds_in_flight,
        );
        private_pending_summary_with_workflow(
            assets,
            snapshot,
            blocked_shield_rows,
            chain_state.map_or_else(
                WalletPpoiWorkflowStatus::default,
                ChainUtxoState::ppoi_workflow_status,
            ),
            chain_state.is_some_and(ChainUtxoState::poi_refreshing),
            chain_state.and_then(ChainUtxoState::poi_refresh_session),
        )
    }

    pub(super) fn private_pending_status_has_shield_timer(&self) -> bool {
        let Some(snapshot) = self
            .chain_states
            .get(&self.selected_chain)
            .and_then(ChainUtxoState::snapshot)
        else {
            return false;
        };
        let waits = pending_shield_waits_by_token(snapshot, now_epoch_secs());
        format_private_asset_rows_from_snapshot(
            snapshot,
            Some(&self.effective_token_registry),
            Some(&self.public_broadcaster_anchor_cache),
        )
        .iter()
        .filter(|asset| should_show_pending_poi_amount(asset.pending_poi_total))
        .any(|asset| {
            asset
                .token
                .and_then(|token| waits.get(&token).copied())
                .is_some_and(|wait| {
                    pending_shield_wait_matches_total(wait, asset.pending_poi_total)
                })
        })
    }

    fn render_private_pending_status_card(
        root: &Entity<Self>,
        summary: &PrivatePendingSummary,
    ) -> gpui::Div {
        let details_root = root.clone();
        let detail = private_pending_summary_detail(summary);

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .bg(rgb_with_alpha(theme::WARNING, 0.08))
            .p(px(14.0))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        Icon::new(RailgunActionIcon::Clock)
                            .small()
                            .text_color(rgb(theme::WARNING)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(app_strong_text(private_pending_summary_title(summary)))
                            .when_some(detail, |column, detail| {
                                column.child(
                                    app_muted_text(detail)
                                        .line_height(px(18.0))
                                        .whitespace_normal(),
                                )
                            }),
                    )
                    .child(
                        app_button_base("wallet-private-pending-details")
                            .ghost()
                            .xsmall()
                            .compact()
                            .child("Details")
                            .on_click(move |_event, window, cx| {
                                details_root.update(cx, |root, cx| {
                                    root.open_private_pending_status_dialog(window, cx);
                                });
                            }),
                    ),
            )
    }

    fn render_private_pending_status_dialog_content(
        root: &Entity<Self>,
        summary: &PrivatePendingSummary,
        content_width: Pixels,
    ) -> gpui::Div {
        let retry_available = summary.poi_refresh_session.is_some();
        let close_root = root.clone();
        let retry_root = root.clone();
        let retrying = summary.poi_refreshing;
        let recoverable = summary.recoverable_poi_outputs;
        let show_recovery = recoverable > 0;
        let workflow_retry_count = usize::try_from(
            summary
                .ppoi_workflow_status
                .needs_attention
                .saturating_add(summary.ppoi_workflow_status.awaiting_recovery),
        )
        .unwrap_or(usize::MAX);
        let retry_count = recoverable.saturating_add(workflow_retry_count);

        div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .when(!summary.blocked_shield_rows.is_empty(), |this| {
                this.child(private_blocked_shield_detail_section(
                    root,
                    &summary.blocked_shield_rows,
                ))
            })
            .when(summary.ppoi_workflow_status.has_outstanding(), |this| {
                this.child(private_ppoi_workflow_section(
                    summary.ppoi_workflow_status,
                    summary.poi_refreshing,
                ))
            })
            .when(!summary.pending_incoming_assets.is_empty(), |this| {
                this.child(private_pending_detail_section(
                    "Pending incoming",
                    pending_asset_count_label(summary.pending_incoming_assets.len()),
                    "Waiting for the network to confirm.",
                    &summary.pending_incoming_assets,
                    "+",
                    theme::WARNING,
                    RailgunActionIcon::Clock,
                ))
            })
            .when(!summary.pending_outgoing_assets.is_empty(), |this| {
                this.child(private_pending_detail_section(
                    "Pending outgoing",
                    pending_asset_count_label(summary.pending_outgoing_assets.len()),
                    "Waiting for the network to confirm.",
                    &summary.pending_outgoing_assets,
                    "-",
                    theme::WARNING,
                    RailgunActionIcon::Clock,
                ))
            })
            .when(!summary.pending_poi_assets.is_empty(), |this| {
                this.child(private_pending_detail_section(
                    "Not yet spendable",
                    pending_asset_count_label(summary.pending_poi_assets.len()),
                    "These need Private Proof of Innocence (PPOI) verification before they can be spent. It usually completes on its own.",
                    &summary.pending_poi_assets,
                    "",
                    theme::WARNING,
                    RailgunActionIcon::Clock,
                ))
            })
            .when(show_recovery, |this| {
                this.child(private_pending_retry_section(
                    recoverable,
                    &summary.recoverable_ppoi_states,
                ))
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .child(
                        app_button("wallet-private-pending-dialog-close", "Close")
                            .outline()
                            .small()
                            .on_click(move |_event, window, cx| {
                                close_root.update(cx, |root, _cx| {
                                    root.private_pending_status_dialog_open = false;
                                });
                                window.close_dialog(cx);
                            }),
                    )
                    .when(retry_count > 0, |actions| {
                        actions.child(
                            app_button(
                                "wallet-private-pending-dialog-retry-poi",
                                retry_poi_label(retry_count, retrying),
                            )
                            .primary()
                            .small()
                            .disabled(!retry_available)
                            .on_click(move |_event, _window, cx| {
                                retry_root.update(cx, |root, cx| {
                                    Self::retry_poi_submissions(root.selected_chain_session(), cx);
                                });
                            }),
                        )
                    }),
            )
    }

    fn render_private_balance_hero(
        root: Entity<Self>,
        total_balance: String,
        receive_available: bool,
        send_asset: Option<UnshieldAsset>,
        unshield_asset: Option<UnshieldAsset>,
        actions_available: bool,
        syncing: bool,
    ) -> gpui::Div {
        let receive_root = root.clone();
        let send_root = root.clone();
        let unshield_root = root;
        let can_send = actions_available && send_asset.is_some();
        let can_unshield = actions_available && unshield_asset.is_some();

        div()
            .w_full()
            .min_h(px(210.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .px(px(24.0))
            .py(px(34.0))
            .child(
                div()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .text_size(theme::ASSET_SYMBOL_TEXT_SIZE)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Total private balance"),
            )
            .child(
                div()
                    .text_color(rgb(theme::WARNING))
                    .text_size(px(44.0))
                    .line_height(px(48.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::from(total_balance)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(Self::render_private_receive_button(
                        receive_root,
                        "wallet-private-hero-receive",
                        receive_available,
                    ))
                    .child(
                        app_button("wallet-private-hero-send", "Send")
                            .child(Icon::new(RailgunActionIcon::Send).small())
                            .outline()
                            .when(can_send, |button| button.bg(rgb(theme::SURFACE)))
                            .disabled(!can_send)
                            .tooltip(private_send_action_tooltip(
                                can_send,
                                actions_available,
                                syncing,
                                "No sendable private asset available",
                            ))
                            .on_click(move |_event, window, cx| {
                                let Some(asset) = send_asset.clone() else {
                                    return;
                                };
                                send_root.update(cx, |root, cx| {
                                    root.open_send_form(asset, window, cx);
                                });
                            }),
                    )
                    .child(
                        app_button("wallet-private-hero-unshield", "Unshield")
                            .child(Icon::new(IconName::Globe).small())
                            .outline()
                            .when(can_unshield, |button| button.bg(rgb(theme::SURFACE)))
                            .disabled(!can_unshield)
                            .tooltip(private_unshield_action_tooltip(
                                can_unshield,
                                actions_available,
                                syncing,
                                "No unshieldable private asset available",
                            ))
                            .on_click(move |_event, window, cx| {
                                let Some(asset) = unshield_asset.clone() else {
                                    return;
                                };
                                unshield_root.update(cx, |root, cx| {
                                    root.open_unshield_form(asset, window, cx);
                                });
                            }),
                    ),
            )
    }

    fn render_private_asset_row(
        root: Entity<Self>,
        ix: usize,
        asset: FormattedTokenTotal,
        snapshot: &ListUtxosOutput,
        actions_available: bool,
        syncing: bool,
    ) -> gpui::Div {
        let send_asset = build_send_asset(snapshot, &asset);
        let can_send = actions_available && send_asset.is_some();
        let unshield_asset = build_unshield_asset(snapshot, &asset);
        let can_unshield = actions_available && unshield_asset.is_some();
        let send_tooltip = private_send_action_tooltip(
            can_send,
            actions_available,
            syncing,
            "No spendable private balance for this token",
        );
        let unshield_tooltip = private_unshield_action_tooltip(
            can_unshield,
            actions_available,
            syncing,
            "No unshieldable private balance for this token",
        );
        let send_opacity = if can_send { 1.0 } else { 0.5 };
        let unshield_opacity = if can_unshield { 1.0 } else { 0.5 };
        let show_pending_poi = should_show_pending_poi_amount(asset.pending_poi_total);
        let pending_poi_amount = asset.pending_poi_amount.clone();
        let show_pending_incoming = should_show_pending_amount(asset.pending_incoming_total);
        let show_pending_outgoing = should_show_pending_amount(asset.pending_outgoing_total);
        let pending_incoming_amount = asset.pending_incoming_amount.clone();
        let pending_outgoing_amount = asset.pending_outgoing_amount.clone();
        let asset_label = asset.label.clone();
        let (primary_amount, secondary_amount) = private_asset_display_amounts(&asset);
        let row_group = SharedString::from(format!("wallet-private-asset-row-{ix}"));
        let send_root = root.clone();
        let unshield_root = root;

        div()
            .group(row_group.clone())
            .w_full()
            .flex()
            .items_center()
            .gap_4()
            .p(px(16.0))
            .rounded_lg()
            .bg(rgb(theme::SURFACE))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .flex()
                    .items_center()
                    .text_size(theme::ASSET_SYMBOL_TEXT_SIZE)
                    .text_color(rgb(theme::TEXT))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(private_asset_label_row(
                        SharedString::from(asset.label.clone()),
                        asset.icon_path,
                    )),
            )
            .child(
                div()
                    .group(row_group.clone())
                    .flex()
                    .items_center()
                    .gap_2()
                    .opacity(0.0)
                    .group_hover(row_group, |this| this.opacity(1.0))
                    .hover(|this| this.opacity(1.0))
                    .child(
                        app_button(
                            SharedString::from(format!("wallet-asset-send-{ix}")),
                            "Send",
                        )
                        .child(Icon::new(RailgunActionIcon::Send).small())
                        .outline()
                        .when(can_send, |button| button.bg(rgb(theme::SURFACE)))
                        .disabled(!can_send)
                        .opacity(send_opacity)
                        .tooltip(send_tooltip)
                        .on_click(move |_event, window, cx| {
                            let Some(asset) = send_asset.clone() else {
                                return;
                            };
                            send_root.update(cx, |root, cx| {
                                root.open_send_form(asset, window, cx);
                            });
                        }),
                    )
                    .child(
                        app_button(
                            SharedString::from(format!("wallet-asset-unshield-{ix}")),
                            "Unshield",
                        )
                        .child(Icon::new(IconName::Globe).small())
                        .outline()
                        .when(can_unshield, |button| button.bg(rgb(theme::SURFACE)))
                        .disabled(!can_unshield)
                        .opacity(unshield_opacity)
                        .tooltip(unshield_tooltip)
                        .on_click(move |_event, window, cx| {
                            let Some(asset) = unshield_asset.clone() else {
                                return;
                            };
                            unshield_root.update(cx, |root, cx| {
                                root.open_unshield_form(asset, window, cx);
                            });
                        }),
                    ),
            )
            .child(
                div()
                    .min_w(px(150.0))
                    .flex()
                    .flex_col()
                    .items_end()
                    .child(
                        div()
                            .text_color(rgb(theme::WARNING))
                            .text_size(theme::BALANCE_TEXT_SIZE)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(SharedString::from(primary_amount)),
                    )
                    .when_some(secondary_amount, |column, secondary_amount| {
                        column.child(
                            app_muted_text(secondary_amount)
                                .whitespace_nowrap()
                                .text_align(gpui::TextAlign::Right),
                        )
                    })
                    .when(show_pending_poi, |column| {
                        column.child(private_asset_pending_label(format!(
                            "{pending_poi_amount} {asset_label} not yet spendable"
                        )))
                    })
                    .when(show_pending_incoming, |column| {
                        column.child(private_asset_pending_label(format!(
                            "+{pending_incoming_amount} {asset_label} arriving"
                        )))
                    })
                    .when(show_pending_outgoing, |column| {
                        column.child(private_asset_pending_label(format!(
                            "-{pending_outgoing_amount} {asset_label} leaving"
                        )))
                    }),
            )
    }
}

fn private_asset_label_row(label: SharedString, icon_path: Option<WalletIconSource>) -> gpui::Div {
    let mut row = div().flex().items_center().gap_2();
    if let Some(path) = icon_path {
        row = row.child(img(path).size(px(32.0)).rounded_full().flex_none());
    }
    row.child(label)
}

fn private_asset_pending_label(label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap_1()
        .text_color(rgb(theme::TEXT_MUTED))
        .text_size(px(12.0))
        .child(Icon::new(RailgunActionIcon::Clock).xsmall())
        .child(
            app_muted_text(label)
                .text_size(px(12.0))
                .whitespace_nowrap()
                .text_align(gpui::TextAlign::Right),
        )
}

pub(super) fn retry_poi_label(count: usize, retrying: bool) -> String {
    if retrying {
        "Queue PPOI retry".to_string()
    } else if count == 1 {
        "Retry".to_string()
    } else {
        format!("Retry ({count})")
    }
}

fn private_pending_status_empty(content_width: Pixels) -> gpui::Div {
    div()
        .w(content_width)
        .text_size(px(13.0))
        .line_height(px(19.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .child("Nothing needs attention right now.")
}

fn private_pending_detail_section(
    title: &'static str,
    count_label: String,
    detail: &'static str,
    assets: &[PrivatePendingAssetLine],
    prefix: &'static str,
    accent_color: u32,
    icon: RailgunActionIcon,
) -> gpui::Div {
    let content = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(private_pending_section_header(
            title,
            count_label,
            accent_color,
            icon,
        ))
        .child(
            app_muted_text(detail)
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal(),
        )
        .children(
            assets
                .iter()
                .map(|asset| private_pending_asset_amount_row(asset, prefix)),
        );
    private_pending_section_card(accent_color, content)
}

fn private_ppoi_workflow_section(status: WalletPpoiWorkflowStatus, refreshing: bool) -> gpui::Div {
    let count = usize::try_from(status.outstanding_count()).unwrap_or(usize::MAX);
    let color = if status.needs_attention > 0 || status.recovery_needs_attention > 0 {
        theme::DANGER
    } else {
        theme::WARNING
    };
    let title = ppoi_workflow_status_title(status, refreshing).unwrap_or("Outgoing proof recovery");
    let content = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(private_pending_section_header(
            title,
            count_label(count, "PPOI output"),
            color,
            RailgunActionIcon::Clock,
        ))
        .child(
            app_muted_text(ppoi_workflow_status_detail(status))
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal(),
        )
        .child(
            app_muted_text(
                "Outgoing proof recovery runs automatically and does not affect your displayed balance.",
            )
            .text_size(px(12.0))
            .line_height(px(17.0))
            .whitespace_normal(),
        );
    private_pending_section_card(color, content)
}

fn private_pending_section_card(accent_color: u32, content: gpui::Div) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_3()
        .p(px(10.0))
        .child(private_pending_section_accent(accent_color))
        .child(content.flex_1().min_w(px(0.0)))
}

fn private_pending_section_accent(accent_color: u32) -> gpui::Div {
    div()
        .w(px(3.0))
        .min_h(px(48.0))
        .h_full()
        .flex_none()
        .rounded_full()
        .bg(rgb_with_alpha(accent_color, 0.82))
}

fn private_pending_section_header(
    title: &'static str,
    count_label: String,
    accent_color: u32,
    icon: RailgunActionIcon,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(Icon::new(icon).xsmall().text_color(rgb(accent_color)))
                .child(app_strong_text(title)),
        )
        .child(app_muted_text(count_label).text_size(px(12.0)))
}

fn private_blocked_shield_detail_section(
    root: &Entity<WalletRoot>,
    rows: &[UtxoDisplayRow],
) -> gpui::Div {
    let content = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(private_pending_section_header(
            "Blocked Shields",
            pending_output_count_label(rows.len()),
            theme::DANGER,
            RailgunActionIcon::Shield,
        ))
        .child(
            app_muted_text("These can't be spent privately. When the original account is known, you can refund them to it.")
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal(),
        )
        .children(
            rows.iter()
                .enumerate()
                .map(|(ix, row)| private_blocked_shield_row(root, ix, row)),
        );
    private_pending_section_card(theme::DANGER, content)
}

fn private_blocked_shield_row(
    root: &Entity<WalletRoot>,
    ix: usize,
    row: &UtxoDisplayRow,
) -> gpui::Div {
    let status = blocked_shield_status_label(row);
    let action_available = blocked_shield_refund_action_available(row);
    let resolving = blocked_shield_refund_origin_resolving(row);
    let action_label = blocked_shield_action_label(row);
    let action_row = row.clone();
    let action_root = root.clone();
    let mut action = app_button(
        SharedString::from(format!("wallet-private-blocked-shield-action-{ix}")),
        action_label,
    )
    .small();
    if row
        .blocked_shield_rescue
        .as_ref()
        .is_some_and(|rescue| rescue.eligible)
    {
        action = action.danger();
    } else {
        action = action.outline();
    }
    action = action.loading(resolving).disabled(resolving);
    if action_available {
        action = action.on_click(move |_event, window, cx| {
            let row = action_row.clone();
            action_root.update(cx, |root, cx| {
                root.begin_blocked_shield_refund(&row, window, cx);
            });
        });
    } else {
        action = action.disabled(true);
    }

    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .text_size(px(12.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_color(rgb(theme::TEXT))
                        .child(SharedString::from(format!("{} {}", row.amount, row.token))),
                )
                .child(
                    app_muted_text(format!("{} · {}", status, short_hash(&row.source_tx_hash)))
                        .text_size(px(12.0)),
                ),
        )
        .child(action)
}

fn blocked_shield_action_label(row: &UtxoDisplayRow) -> &'static str {
    if blocked_shield_refund_action_available(row) || blocked_shield_refund_origin_resolving(row) {
        "Refund"
    } else {
        "Unavailable"
    }
}

fn blocked_shield_status_label(row: &UtxoDisplayRow) -> String {
    let Some(rescue) = row.blocked_shield_rescue.as_ref() else {
        return "Refund unavailable".to_string();
    };
    if rescue.eligible {
        return "Refund available".to_string();
    }
    if blocked_shield_refund_action_available(row) {
        return "Origin check needed".to_string();
    }
    rescue
        .disabled_reason
        .clone()
        .unwrap_or_else(|| "Refund unavailable".to_string())
}

fn private_pending_asset_amount_row(
    asset: &PrivatePendingAssetLine,
    prefix: &'static str,
) -> gpui::Div {
    let shield_wait = asset.shield_wait.and_then(|wait| {
        shield_poi_wait_time_display(wait.latest_source_block_timestamp, now_epoch_secs())
            .map(|display| (wait, display))
    });
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .text_size(px(12.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .truncate()
                        .text_color(rgb(theme::TEXT))
                        .child(SharedString::from(asset.label.clone())),
                )
                .when_some(shield_wait, |this, (wait, display)| {
                    let label = if wait.has_delayed && wait.output_count == 1 {
                        "Shield · taking longer than usual".to_string()
                    } else if wait.has_delayed {
                        format!(
                            "{} Shields · some taking longer than usual",
                            wait.output_count
                        )
                    } else if wait.output_count == 1 {
                        format!("Shield · ready in {}", display.label)
                    } else {
                        format!(
                            "{} Shields · ready in up to {}",
                            wait.output_count, display.label
                        )
                    };
                    this.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_size(px(11.0))
                            .line_height(px(14.0))
                            .text_color(rgb(theme::WARNING))
                            .child(Icon::new(RailgunActionIcon::Clock).xsmall())
                            .child(SharedString::from(label)),
                    )
                }),
        )
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(format!("{prefix}{}", asset.amount))),
        )
}

fn private_pending_retry_section(recoverable: usize, states: &[UtxoPpoiState]) -> gpui::Div {
    let content = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(private_pending_section_header(
            "Retry PPOI",
            pending_output_count_label(recoverable),
            theme::DANGER,
            RailgunActionIcon::Clock,
        ))
        .child(
            app_muted_text("Submission is normally automatic. Retrying re-checks these and any related outputs from this wallet.")
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal(),
        )
        .children(states.iter().map(|state| {
            app_muted_text(super::utxo::ppoi_state_detail(*state))
                .text_size(px(12.0))
                .line_height(px(17.0))
                .whitespace_normal()
        }));
    private_pending_section_card(theme::DANGER, content)
}

fn pending_output_count_label(count: usize) -> String {
    if count == 1 {
        "1 output".to_string()
    } else {
        format!("{count} outputs")
    }
}

fn pending_asset_count_label(count: usize) -> String {
    if count == 1 {
        "1 asset".to_string()
    } else {
        format!("{count} assets")
    }
}

pub(super) fn refresh_form_asset_from_snapshot(
    snapshot: &ListUtxosOutput,
    current: &UnshieldAsset,
    send: bool,
    registry: Option<&EffectiveTokenRegistry>,
) -> UnshieldAsset {
    let formatted = format_private_asset_rows(snapshot.chain_id, &snapshot.totals, registry, None)
        .into_iter()
        .find(|asset| asset.token == Some(current.token));
    let total = formatted
        .as_ref()
        .and_then(|asset| asset.total)
        .unwrap_or_default();
    let poi_verified_total = formatted
        .as_ref()
        .and_then(|asset| asset.poi_verified_total)
        .unwrap_or_default();
    let max_batched = if send {
        max_send_amount_from_snapshot(snapshot, current.token)
    } else {
        max_unshield_amount_from_snapshot(snapshot, current.token)
    };

    UnshieldAsset {
        chain_id: current.chain_id,
        token: current.token,
        label: formatted
            .as_ref()
            .map_or_else(|| current.label.clone(), |asset| asset.label.clone()),
        decimals: formatted
            .as_ref()
            .and_then(|asset| asset.decimals)
            .or(current.decimals),
        total,
        poi_verified_total,
        max_batched,
        icon_path: formatted
            .as_ref()
            .and_then(|asset| asset.icon_path.clone())
            .or_else(|| current.icon_path.clone()),
    }
}

#[cfg(test)]
pub(super) fn send_asset_key_from_formatted(
    asset: &FormattedTokenTotal,
) -> Option<UnshieldAssetKey> {
    unshield_asset_key_from_formatted(asset)
}

#[cfg(test)]
pub(super) fn send_key_matches_asset(key: UnshieldAssetKey, asset: &FormattedTokenTotal) -> bool {
    send_asset_key_from_formatted(asset) == Some(key)
}

#[cfg(test)]
pub(super) fn unshield_asset_key_from_formatted(
    asset: &FormattedTokenTotal,
) -> Option<UnshieldAssetKey> {
    asset
        .token
        .map(|token| UnshieldAssetKey::new(asset.chain_id, token))
}

#[cfg(test)]
pub(super) fn unshield_key_matches_asset(
    key: UnshieldAssetKey,
    asset: &FormattedTokenTotal,
) -> bool {
    unshield_asset_key_from_formatted(asset) == Some(key)
}
