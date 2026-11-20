use std::sync::Arc;

use alloy::primitives::{Address, U256};
use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, div,
    px, rgb,
};
use gpui_component::{IconName, Sizable, spinner::Spinner};
use railgun_ui::format_token_amount;
use ui::clipboard::clipboard_with_toast;
use ui::controls::{app_muted_text, app_shimmer_text, app_strong_text};
use ui::theme;
use wallet_ops::{
    PublicBroadcasterCandidate, PublicBroadcasterCostEstimate, PublicBroadcasterFeeBreakdown,
    PublicBroadcasterFeeMargin, PublicBroadcasterSubmissionResult, TokenAnchorRateCache,
    fixed_token_anchor_rate, public_broadcaster_fee_breakdown,
    public_broadcaster_service_gas_price, settings::EffectiveTokenRegistry,
};

use super::broadcaster_picker::{
    BroadcasterChoice, BroadcasterPickerFeeEstimateContext, broadcaster_candidate_label,
};
use super::private_action::{PrivateEstimateInput, PrivateEstimateOutput, delivery_element_id};
use super::private_broadcaster::PrivateBroadcasterProgressState;
use super::public_action::public_action_protocol_fee_label;
use super::spend_authorization::{
    automatic_private_fee_bounds, spend_authorization_recipient_display,
};
use super::utxo::short_hash;
use super::{
    COST_ESTIMATE_DEBOUNCE, DeliveryFormKind, DeliveryMode, UnshieldAsset, UnshieldAssetKey,
    WalletRoot, broadcaster_candidate_anchor_rate, format_native_token_amount_for_display,
    format_native_top_up_recipient_suffix, format_report_chain,
    format_token_amount_ceiling_for_display, format_token_amount_for_display,
    format_value_with_usd_label, should_show_distinct_amount, token_display_metadata,
};

mod estimate_scheduling;
mod form_estimates;

mod cost_display;
mod cost_render;
mod progress_context;

pub(super) use cost_render::*;
pub(super) use progress_context::*;

const COST_ESTIMATE_DETAIL_TEXT_SIZE: gpui::Pixels = px(12.0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CostEstimateStatus {
    Estimating,
}

pub(super) struct PublicBroadcasterCostDisplay<'a> {
    pub(super) broadcaster: &'a PublicBroadcasterCandidate,
    registry: Option<&'a EffectiveTokenRegistry>,
    pub(super) chain_id: u64,
    pub(super) action_token: Address,
    fee_token: Address,
    pub(super) entered_amount: U256,
    pub(super) recipient_amount: U256,
    pub(super) total_private_spend: U256,
    fee_amount: U256,
    protocol_fee_amount: U256,
    pub(super) protocol_fee_bps: U256,
    gas_limit: u64,
    min_gas_price: u128,
    fee_anchor_rate: Option<U256>,
    native_top_up: Option<&'a wallet_ops::DesktopNativeTopUpPlan>,
}

pub(super) struct PublicBroadcasterFeeDisplay<'a> {
    pub(super) broadcaster: &'a PublicBroadcasterCandidate,
    pub(super) registry: Option<&'a EffectiveTokenRegistry>,
    pub(super) chain_id: u64,
    pub(super) fee_token: Address,
    pub(super) fee_amount: U256,
    pub(super) gas_limit: u64,
    pub(super) min_gas_price: u128,
    pub(super) fee_anchor_rate: Option<U256>,
}

pub(super) struct PrivateBroadcasterProgressContext<'a> {
    pub(super) display: PublicBroadcasterCostDisplay<'a>,
    pub(super) anchor_cache: &'a TokenAnchorRateCache,
}

pub(super) fn format_public_broadcaster_fee_margin(
    chain_id: u64,
    fee_token: Address,
    margin: PublicBroadcasterFeeMargin,
    registry: Option<&EffectiveTokenRegistry>,
) -> String {
    match margin {
        PublicBroadcasterFeeMargin::Zero => {
            format_token_amount_for_display(chain_id, fee_token, U256::ZERO, registry)
        }
        PublicBroadcasterFeeMargin::Positive(amount) => {
            format_token_amount_for_display(chain_id, fee_token, amount, registry)
        }
        PublicBroadcasterFeeMargin::Negative(amount) => {
            format!(
                "-{}",
                format_token_amount_for_display(chain_id, fee_token, amount, registry)
            )
        }
    }
}

pub(super) const fn should_render_public_broadcaster_cost_preview(
    delivery_mode: DeliveryMode,
    has_result: bool,
    has_error: bool,
) -> bool {
    matches!(delivery_mode, DeliveryMode::PublicBroadcaster) && !has_result && !has_error
}

fn format_gwei(wei: u128) -> String {
    format_token_amount(U256::from(wei), 9)
}
