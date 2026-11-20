use std::collections::BTreeMap;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use gpui::{
    Context, Entity, InteractiveElement, IntoElement, ParentElement, Pixels, SharedString, Styled,
    Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable, WindowExt, button::ButtonVariants,
    scroll::ScrollableElement,
};
use railgun_ui::{format_token_amount, format_usd_micro_value, short_address};
use ui::controls::{app_button, app_muted_shimmer_text, app_muted_text, app_strong_text};
use ui::dialog::WalletDialogExt as _;
use ui::theme::{self};
use wallet_ops::{
    ListUtxosOutput, TokenAnchorRateCache, TokenTotal, UtxoPpoiState, WalletPpoiWorkflowStatus,
    max_send_amount_from_outputs as planner_max_send_amount_from_outputs,
    max_unshield_amount_from_outputs as planner_max_unshield_amount_from_outputs,
    settings::EffectiveTokenRegistry,
};

use crate::assets::{RailgunActionIcon, WalletIconSource};

use super::chain_load::loading_summary;
use super::private_swap::swap_entry_availability;
use super::public_account::render_public_address_qr_dialog_content;
use super::utxo::{
    UtxoDisplayRow, blocked_shield_refund_action_available, blocked_shield_refund_origin_resolving,
    blocked_shield_rescue_display_rows, display_rows_from_output, now_epoch_secs,
    ppoi_workflow_status_detail, ppoi_workflow_status_title, recoverable_poi_candidate_count,
    shield_poi_wait_display, shield_poi_wait_time_display, short_hash,
};
use super::{
    ChainUtxoState, PUBLIC_ADDRESS_QR_DIALOG_WIDTH, UnshieldAsset, WalletRoot, count_label,
    dialog_max_height, parse_address, rgb_with_alpha, secondary_dialog_content_width,
    token_display_metadata,
};

#[cfg(test)]
use super::UnshieldAssetKey;

mod asset_list;
mod balance_hero;
mod pending_status;

mod pending_summary;

pub(super) use pending_status::*;
pub(super) use pending_summary::*;

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

#[derive(Default)]
pub(super) struct PrivateAssetPresentationCache {
    current: Option<CachedPrivateAssets>,
}

struct CachedPrivateAssets {
    wallet_generation: u64,
    snapshot: Arc<ListUtxosOutput>,
    registry: EffectiveTokenRegistry,
    rates_revision: u64,
    presentation: Arc<PrivateAssetPresentation>,
}

pub(super) struct PrivateAssetPresentation {
    pub(super) rows: Vec<FormattedTokenTotal>,
    pub(super) total: Option<String>,
}

impl PrivateAssetPresentationCache {
    pub(super) fn clear(&mut self) {
        self.current = None;
    }
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
