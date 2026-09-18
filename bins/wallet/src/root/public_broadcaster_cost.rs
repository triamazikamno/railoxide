use std::sync::Arc;

use alloy::primitives::{Address, U256};
use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement, SharedString, Styled, div, px, rgb,
};
use gpui_component::{IconName, Sizable, spinner::Spinner};
use railgun_ui::format_token_amount;
use ui::controls::{app_muted_text, app_strong_text};
use ui::theme;
use wallet_ops::{
    PublicBroadcasterCandidate, PublicBroadcasterCostEstimate, PublicBroadcasterFeeBreakdown,
    PublicBroadcasterFeeMargin, PublicBroadcasterSubmissionResult, TokenAnchorRateCache,
    fixed_token_anchor_rate, public_broadcaster_fee_breakdown,
    public_broadcaster_service_gas_price, settings::EffectiveTokenRegistry,
};

use super::broadcaster_picker::{BroadcasterPickerFeeEstimateContext, broadcaster_candidate_label};
use super::private_action::{PrivateEstimateInput, PrivateEstimateOutput, delivery_element_id};
use super::private_broadcaster::PrivateBroadcasterProgressState;
use super::public_action::public_action_protocol_fee_label;
use super::spend_authorization::spend_authorization_recipient_display;
use super::{
    COST_ESTIMATE_DEBOUNCE, DeliveryFormKind, DeliveryMode, UnshieldAsset, UnshieldAssetKey,
    WalletRoot, broadcaster_candidate_anchor_rate, copyable_mono_field,
    format_native_token_amount_for_display, format_native_top_up_recipient_suffix,
    format_report_chain, format_token_amount_for_display, format_value_with_usd_label,
    should_show_distinct_amount, token_display_metadata,
};

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

impl<'a> PublicBroadcasterCostDisplay<'a> {
    pub(super) const fn fee_display(&self) -> PublicBroadcasterFeeDisplay<'_> {
        PublicBroadcasterFeeDisplay {
            broadcaster: self.broadcaster,
            registry: self.registry,
            chain_id: self.chain_id,
            fee_token: self.fee_token,
            fee_amount: self.fee_amount,
            gas_limit: self.gas_limit,
            min_gas_price: self.min_gas_price,
            fee_anchor_rate: self.fee_anchor_rate,
        }
    }

    pub(super) const fn from_result(
        result: &'a PublicBroadcasterSubmissionResult,
        fee_anchor_rate: Option<U256>,
        registry: Option<&'a EffectiveTokenRegistry>,
    ) -> Self {
        Self {
            broadcaster: &result.broadcaster,
            registry,
            chain_id: result.broadcaster.chain_id,
            action_token: result.action_token,
            fee_token: result.fee_token,
            entered_amount: result.entered_amount,
            recipient_amount: result.recipient_amount,
            total_private_spend: result.total_private_spend,
            fee_amount: result.fee_amount,
            protocol_fee_amount: result.protocol_fee_amount,
            protocol_fee_bps: result.protocol_fee_bps,
            gas_limit: result.gas_limit,
            min_gas_price: result.min_gas_price,
            fee_anchor_rate,
            native_top_up: result.native_top_up.as_ref(),
        }
    }

    pub(super) const fn from_estimate(
        asset: &UnshieldAsset,
        estimate: &'a PublicBroadcasterCostEstimate,
        fee_anchor_rate: Option<U256>,
        registry: Option<&'a EffectiveTokenRegistry>,
    ) -> Self {
        Self::from_estimate_chain(asset.chain_id, estimate, fee_anchor_rate, registry)
    }

    pub(super) const fn from_estimate_chain(
        chain_id: u64,
        estimate: &'a PublicBroadcasterCostEstimate,
        fee_anchor_rate: Option<U256>,
        registry: Option<&'a EffectiveTokenRegistry>,
    ) -> Self {
        Self {
            broadcaster: &estimate.broadcaster,
            registry,
            chain_id,
            action_token: estimate.action_token,
            fee_token: estimate.fee_token,
            entered_amount: estimate.entered_amount,
            recipient_amount: estimate.recipient_amount,
            total_private_spend: estimate.total_private_spend,
            fee_amount: estimate.fee_amount,
            protocol_fee_amount: estimate.protocol_fee_amount,
            protocol_fee_bps: estimate.protocol_fee_bps,
            gas_limit: estimate.gas_limit,
            min_gas_price: estimate.min_gas_price,
            fee_anchor_rate,
            native_top_up: estimate.native_top_up.as_ref(),
        }
    }

    pub(super) fn outcome_rows(
        &self,
        anchor_cache: &TokenAnchorRateCache,
    ) -> Vec<ui::private_action::DisplayRow> {
        use ui::private_action::DisplayRow;
        let mut rows = vec![DisplayRow {
            label: "Recipient receives".into(),
            value: self.action_amount_with_usd(self.recipient_amount, anchor_cache),
            suffix: self.native_top_up_recipient_suffix(),
        }];
        if should_show_distinct_amount(self.entered_amount, self.total_private_spend) {
            rows.push(DisplayRow {
                label: self.private_spend_label().into(),
                value: self.action_amount(self.total_private_spend),
                suffix: None,
            });
        }
        if !self.protocol_fee_bps.is_zero() {
            rows.push(DisplayRow {
                label: public_action_protocol_fee_label(self.protocol_fee_bps),
                value: self.protocol_fee_value_with_usd(anchor_cache),
                suffix: None,
            });
        }
        rows
    }

    pub(super) fn fee_rows(
        &self,
        anchor_cache: &TokenAnchorRateCache,
    ) -> Vec<ui::private_action::DisplayRow> {
        self.fee_display().fee_rows(anchor_cache)
    }

    pub(super) fn private_spend_label(&self) -> &'static str {
        if self.action_token == self.fee_token {
            "Total private spend"
        } else {
            "Action-token private spend"
        }
    }

    pub(super) fn action_amount(&self, amount: U256) -> String {
        format_token_amount_for_display(self.chain_id, self.action_token, amount, self.registry)
    }

    pub(super) fn action_amount_with_usd(
        &self,
        amount: U256,
        anchor_cache: &TokenAnchorRateCache,
    ) -> String {
        format_value_with_usd_label(
            self.action_amount(amount),
            amount,
            token_display_metadata(self.registry, self.chain_id, &self.action_token)
                .map(|metadata| metadata.decimals),
            anchor_cache.cached_token_usd_micro_value(self.chain_id, self.action_token, amount),
            false,
        )
    }

    pub(super) fn fee_amount_with_usd(&self, anchor_cache: &TokenAnchorRateCache) -> String {
        self.fee_display().fee_amount_with_usd(anchor_cache)
    }

    pub(super) fn protocol_fee_value_with_usd(
        &self,
        anchor_cache: &TokenAnchorRateCache,
    ) -> String {
        self.action_amount_with_usd(self.protocol_fee_amount, anchor_cache)
    }

    pub(super) fn gas_value(&self) -> String {
        self.fee_display().gas_value()
    }

    pub(super) fn native_top_up_recipient_suffix(&self) -> Option<String> {
        self.native_top_up.as_ref().map(|top_up| {
            format_native_top_up_recipient_suffix(self.chain_id, top_up.native_amount)
        })
    }
}

impl PublicBroadcasterFeeDisplay<'_> {
    pub(super) fn fee_rows(
        &self,
        anchor_cache: &TokenAnchorRateCache,
    ) -> Vec<ui::private_action::DisplayRow> {
        let breakdown = self.fee_breakdown();
        [
            (
                "Gas cost",
                self.native_gas_cost_value_with_usd(&breakdown, anchor_cache),
            ),
            (
                "Broadcaster's fee",
                self.broadcaster_fee_value_with_usd(&breakdown, anchor_cache),
            ),
        ]
        .into_iter()
        .map(|(label, value)| ui::private_action::DisplayRow {
            label: label.into(),
            value,
            suffix: None,
        })
        .collect()
    }

    pub(super) fn fee_amount(&self) -> String {
        format_token_amount_for_display(
            self.chain_id,
            self.fee_token,
            self.fee_amount,
            self.registry,
        )
    }

    pub(super) fn fee_amount_with_usd(&self, anchor_cache: &TokenAnchorRateCache) -> String {
        self.token_amount_value_with_usd(
            self.fee_amount(),
            anchor_cache,
            self.fee_token,
            self.fee_amount,
            false,
        )
    }

    pub(super) fn fee_breakdown(&self) -> PublicBroadcasterFeeBreakdown {
        public_broadcaster_fee_breakdown(
            self.fee_amount,
            self.gas_limit,
            self.min_gas_price,
            self.fee_token_anchor_rate(),
        )
    }

    fn fee_token_anchor_rate(&self) -> Option<U256> {
        self.fee_anchor_rate
            .or_else(|| broadcaster_candidate_anchor_rate(self.broadcaster))
            .or_else(|| fixed_token_anchor_rate(self.chain_id, self.fee_token))
    }

    pub(super) fn native_gas_cost_value(
        &self,
        breakdown: &PublicBroadcasterFeeBreakdown,
    ) -> String {
        format_native_token_amount_for_display(self.chain_id, breakdown.native_gas_cost)
    }

    pub(super) fn native_gas_cost_value_with_usd(
        &self,
        breakdown: &PublicBroadcasterFeeBreakdown,
        anchor_cache: &TokenAnchorRateCache,
    ) -> String {
        let token_value = self.native_gas_cost_value(breakdown);
        format_value_with_usd_label(
            token_value,
            breakdown.native_gas_cost,
            Some(18),
            anchor_cache.cached_native_usd_micro_value(self.chain_id, breakdown.native_gas_cost),
            false,
        )
    }

    pub(super) fn broadcaster_fee_value(
        &self,
        breakdown: &PublicBroadcasterFeeBreakdown,
    ) -> String {
        breakdown.broadcaster_fee.map_or_else(
            || "unavailable (no anchor)".to_string(),
            |margin| {
                format_public_broadcaster_fee_margin(
                    self.chain_id,
                    self.fee_token,
                    margin,
                    self.registry,
                )
            },
        )
    }

    pub(super) fn broadcaster_fee_value_with_usd(
        &self,
        breakdown: &PublicBroadcasterFeeBreakdown,
        anchor_cache: &TokenAnchorRateCache,
    ) -> String {
        let token_value = self.broadcaster_fee_value(breakdown);
        let Some(margin) = breakdown.broadcaster_fee else {
            return format_value_with_usd_label(token_value, U256::ZERO, None, None, false);
        };
        let (negative, amount) = match margin {
            PublicBroadcasterFeeMargin::Zero => (false, U256::ZERO),
            PublicBroadcasterFeeMargin::Positive(amount) => (false, amount),
            PublicBroadcasterFeeMargin::Negative(amount) => (true, amount),
        };
        self.token_amount_value_with_usd(
            token_value,
            anchor_cache,
            self.fee_token,
            amount,
            negative,
        )
    }

    pub(super) fn gas_value(&self) -> String {
        format!(
            "~{} gas @ {} gwei",
            self.gas_limit,
            format_gwei(public_broadcaster_service_gas_price(self.min_gas_price))
        )
    }

    fn token_amount_value_with_usd(
        &self,
        token_value: String,
        anchor_cache: &TokenAnchorRateCache,
        token: Address,
        amount: U256,
        negative: bool,
    ) -> String {
        format_value_with_usd_label(
            token_value,
            amount,
            token_display_metadata(self.registry, self.chain_id, &token)
                .map(|metadata| metadata.decimals),
            anchor_cache.cached_token_usd_micro_value(self.chain_id, token, amount),
            negative,
        )
    }
}

impl WalletRoot {
    pub(super) fn schedule_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.can_schedule_public_broadcaster_cost_estimate(kind, key) {
            return;
        }

        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key) {
                    form.estimate_id = estimate_id;
                    form.cost_estimate = None;
                    form.cost_estimate_pending = false;
                    form.estimating_cost = false;
                    form.error = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key) {
                    form.estimate_id = estimate_id;
                    form.cost_estimate = None;
                    form.cost_estimate_pending = false;
                    form.estimating_cost = false;
                    form.error = None;
                }
            }
        }
        cx.notify();

        match kind {
            DeliveryFormKind::Send => self.estimate_send_public_broadcaster_cost_from_form(key, cx),
            DeliveryFormKind::Unshield => {
                self.estimate_unshield_public_broadcaster_cost_from_form(key, cx);
            }
        }
    }

    pub(super) fn debounce_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.can_schedule_public_broadcaster_cost_estimate(kind, key) {
            return;
        }

        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key) {
                    form.estimate_id = estimate_id;
                    form.cost_estimate = None;
                    form.cost_estimate_pending = true;
                    form.estimating_cost = false;
                    form.error = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key) {
                    form.estimate_id = estimate_id;
                    form.cost_estimate = None;
                    form.cost_estimate_pending = true;
                    form.estimating_cost = false;
                    form.error = None;
                }
            }
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            tokio::time::sleep(COST_ESTIMATE_DEBOUNCE).await;
            let _ = this.update(cx, |root, cx| {
                let current_id = match kind {
                    DeliveryFormKind::Send => {
                        root.send_forms.get(&key).map(|form| form.estimate_id)
                    }
                    DeliveryFormKind::Unshield => {
                        root.unshield_forms.get(&key).map(|form| form.estimate_id)
                    }
                };
                if current_id != Some(estimate_id) {
                    return;
                }
                match kind {
                    DeliveryFormKind::Send => {
                        root.estimate_send_public_broadcaster_cost_from_form(key, cx);
                    }
                    DeliveryFormKind::Unshield => {
                        root.estimate_unshield_public_broadcaster_cost_from_form(key, cx);
                    }
                }
            });
        })
        .detach();
    }

    fn can_schedule_public_broadcaster_cost_estimate(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).is_some_and(|form| {
                !form.generating && form.delivery_mode == DeliveryMode::PublicBroadcaster
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).is_some_and(|form| {
                !form.generating && form.delivery_mode == DeliveryMode::PublicBroadcaster
            }),
        }
    }

    pub(super) fn clear_pending_public_broadcaster_cost_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                let changed = form.cost_estimate_pending || form.estimating_cost;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.estimate_id = 0;
                changed
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                let changed = form.cost_estimate_pending || form.estimating_cost;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.estimate_id = 0;
                changed
            }),
        };
        if changed {
            cx.notify();
        }
    }

    pub(super) fn reschedule_ready_public_broadcaster_cost_estimates(
        &mut self,
        chain_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let send_keys = self
            .send_forms
            .iter()
            .filter_map(|(key, form)| {
                (key.chain_id == chain_id
                    && form.delivery_mode == DeliveryMode::PublicBroadcaster
                    && !form.generating
                    && public_broadcaster_estimate_needs_ready_retry(
                        form.cost_estimate.is_some()
                            || form.result.is_some()
                            || form.error.is_some(),
                        form.cost_estimate_pending || form.estimating_cost,
                    ))
                .then_some(*key)
            })
            .collect::<Vec<_>>();
        let unshield_keys = self
            .unshield_forms
            .iter()
            .filter_map(|(key, form)| {
                (key.chain_id == chain_id
                    && form.delivery_mode == DeliveryMode::PublicBroadcaster
                    && !form.generating
                    && public_broadcaster_estimate_needs_ready_retry(
                        form.cost_estimate.is_some()
                            || form.result.is_some()
                            || form.error.is_some(),
                        form.cost_estimate_pending || form.estimating_cost,
                    ))
                .then_some(*key)
            })
            .collect::<Vec<_>>();

        for key in send_keys {
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
        }
        for key in unshield_keys {
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
        }
    }

    fn estimate_send_public_broadcaster_cost_from_form(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.send_forms.get(&key) else {
            return;
        };
        if form.generating
            || form.estimating_cost
            || form.delivery_mode != DeliveryMode::PublicBroadcaster
        {
            return;
        }
        let input = PrivateEstimateInput {
            custom_fee_amount: form.custom_fee_amount,
            asset: form.asset.clone(),
            recipient: form.recipient_input.read(cx).value().to_string(),
            amount: form.amount_input.read(cx).value().to_string(),
            broadcaster: form.broadcaster_choice.clone(),
            fee_token: form.selected_fee_token,
            fee_mode: form.fee_mode,
            allow_out_of_range: form.allow_suspicious_broadcasters,
            favorites_only: form.favorites_only_broadcasters,
            output: PrivateEstimateOutput::Send,
        };
        if self.recipient_combobox_search_active(DeliveryFormKind::Send, key) {
            self.clear_pending_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
            return;
        }
        let request = match self.prepare_private_broadcaster_estimate(&input) {
            Ok(Some(request)) => request,
            Ok(None) => {
                self.clear_pending_public_broadcaster_cost_estimate(
                    DeliveryFormKind::Send,
                    key,
                    cx,
                );
                return;
            }
            Err(error) => {
                self.set_send_form_error(key, error, cx);
                return;
            }
        };

        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        if let Some(form) = self.send_forms.get_mut(&key) {
            form.cost_estimate_pending = false;
            form.estimating_cost = true;
            form.error = None;
            form.estimate_id = estimate_id;
        }
        cx.notify();

        let http = self.http.clone();
        let join = self
            .runtime
            .spawn(async move { request.estimate(&http).await });
        cx.spawn(async move |this, cx| {
            let result = match join.await {
                Ok(result) => result,
                Err(error) => Err(eyre::eyre!("send cost estimate task failed: {error}")),
            };
            let _ = this.update(cx, |root, cx| {
                let Some(form) = root.send_forms.get_mut(&key) else {
                    return;
                };
                if form.estimate_id != estimate_id {
                    return;
                }
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                let picker_context = match result {
                    Ok(estimate) => {
                        let context = BroadcasterPickerFeeEstimateContext::from(&estimate);
                        form.error = None;
                        form.cost_estimate = Some(estimate);
                        form.gateway_estimated_at = form
                            .gateway_execution
                            .as_ref()
                            .map(|_| std::time::Instant::now());
                        Some(context)
                    }
                    Err(error) => {
                        form.cost_estimate = None;
                        form.error = Some(Arc::from(format_report_chain(&error)));
                        None
                    }
                };
                if let Some(context) = picker_context {
                    root.adopt_broadcaster_picker_fee_estimate(
                        DeliveryFormKind::Send,
                        key,
                        context,
                        cx,
                    );
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn estimate_unshield_public_broadcaster_cost_from_form(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        self.refresh_unshield_native_top_up_state(key, cx);
        let Some(form) = self.unshield_forms.get(&key) else {
            return;
        };
        if form.generating
            || form.estimating_cost
            || form.delivery_mode != DeliveryMode::PublicBroadcaster
        {
            return;
        }
        let input = PrivateEstimateInput {
            custom_fee_amount: form.custom_fee_amount,
            asset: form.asset.clone(),
            recipient: form.recipient_input.read(cx).value().to_string(),
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
        };
        if self.recipient_combobox_search_active(DeliveryFormKind::Unshield, key) {
            self.clear_pending_public_broadcaster_cost_estimate(
                DeliveryFormKind::Unshield,
                key,
                cx,
            );
            return;
        }
        let request = match self.prepare_private_broadcaster_estimate(&input) {
            Ok(Some(request)) => request,
            Ok(None) => {
                self.clear_pending_public_broadcaster_cost_estimate(
                    DeliveryFormKind::Unshield,
                    key,
                    cx,
                );
                return;
            }
            Err(error) => {
                self.set_unshield_form_error(key, error, cx);
                return;
            }
        };

        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        if let Some(form) = self.unshield_forms.get_mut(&key) {
            form.cost_estimate_pending = false;
            form.estimating_cost = true;
            form.error = None;
            form.estimate_id = estimate_id;
        }
        cx.notify();

        let http = self.http.clone();
        let join = self
            .runtime
            .spawn(async move { request.estimate(&http).await });
        cx.spawn(async move |this, cx| {
            let result = match join.await {
                Ok(result) => result,
                Err(error) => Err(eyre::eyre!("unshield cost estimate task failed: {error}")),
            };
            let _ = this.update(cx, |root, cx| {
                let Some(form) = root.unshield_forms.get_mut(&key) else {
                    return;
                };
                if form.estimate_id != estimate_id {
                    return;
                }
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                let picker_context = match result {
                    Ok(estimate) => {
                        let context = BroadcasterPickerFeeEstimateContext::from(&estimate);
                        form.error = None;
                        form.cost_estimate = Some(estimate);
                        form.gateway_estimated_at = form
                            .gateway_execution
                            .as_ref()
                            .map(|_| std::time::Instant::now());
                        Some(context)
                    }
                    Err(error) => {
                        form.cost_estimate = None;
                        form.error = Some(Arc::from(format_report_chain(&error)));
                        None
                    }
                };
                if let Some(context) = picker_context {
                    root.adopt_broadcaster_picker_fee_estimate(
                        DeliveryFormKind::Unshield,
                        key,
                        context,
                        cx,
                    );
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

pub(super) const fn public_broadcaster_estimate_needs_ready_retry(
    has_completed_state: bool,
    in_flight: bool,
) -> bool {
    !has_completed_state && !in_flight
}

fn render_transaction_fee_breakdown(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    display: &PublicBroadcasterCostDisplay<'_>,
    anchor_cache: &TokenAnchorRateCache,
    open: bool,
    custom_fee: bool,
    edit_action: Option<AnyElement>,
) -> impl IntoElement {
    let mut total = display.fee_amount_with_usd(anchor_cache);
    if custom_fee {
        total.push_str(" · Custom");
    }
    ui::private_action::transaction_fee_breakdown(
        delivery_element_id(key, kind, "transaction-fee-breakdown"),
        total,
        display.fee_rows(anchor_cache),
        display.gas_value(),
        open,
        edit_action,
        move |open, _, cx| {
            root.update(cx, |root, cx| {
                root.set_transaction_fee_breakdown_open(kind, key, open, cx);
            });
        },
    )
}

pub(super) fn render_public_broadcaster_tx_hash_row(
    tx_hash: String,
    button_id: SharedString,
) -> gpui::Div {
    copyable_mono_field("Tx hash", tx_hash, button_id)
}

pub(super) fn render_public_broadcaster_cost_estimate(
    root: Entity<WalletRoot>,
    key: UnshieldAssetKey,
    kind: DeliveryFormKind,
    asset: &UnshieldAsset,
    estimate: &PublicBroadcasterCostEstimate,
    fee_anchor_rate: Option<U256>,
    registry: Option<&EffectiveTokenRegistry>,
    anchor_cache: &TokenAnchorRateCache,
    transaction_fee_breakdown_open: bool,
    refreshing: bool,
    custom_fee_amount: Option<U256>,
    fee_editable: bool,
) -> gpui::Div {
    let display =
        PublicBroadcasterCostDisplay::from_estimate(asset, estimate, fee_anchor_rate, registry);
    ui::private_action::estimated_outcome(
        broadcaster_candidate_label(display.broadcaster),
        display.outcome_rows(anchor_cache),
        render_transaction_fee_breakdown(
            root.clone(),
            key,
            kind,
            &display,
            anchor_cache,
            transaction_fee_breakdown_open,
            custom_fee_amount.is_some(),
            fee_editable.then(|| {
                super::private_action::fee_editor::fee_edit_button(root.clone(), kind, key)
                    .into_any_element()
            }),
        ),
        public_broadcaster_estimate_shape(estimate),
        delivery_element_id(key, kind, "refresh-estimate"),
        refreshing,
        move |_, cx| {
            root.update(cx, |root, cx| {
                root.schedule_public_broadcaster_cost_estimate(kind, key, cx);
            });
        },
    )
}

pub(super) fn public_broadcaster_estimate_shape(
    estimate: &PublicBroadcasterCostEstimate,
) -> String {
    format!(
        "Shape: {} proofs · {} inputs · {} private outputs · {} public outputs · {} RelayAdapt calls{}",
        estimate.transaction_count,
        estimate.input_count,
        estimate.private_output_count,
        estimate.public_output_count,
        estimate.relay_call_count,
        if estimate.uses_relay_adapt {
            " · RelayAdapt"
        } else {
            ""
        }
    )
}

pub(super) const fn public_broadcaster_cost_status(
    pending: bool,
    estimating: bool,
) -> Option<CostEstimateStatus> {
    if pending {
        None
    } else if estimating {
        Some(CostEstimateStatus::Estimating)
    } else {
        None
    }
}

pub(super) const fn public_broadcaster_cost_status_text(
    status: CostEstimateStatus,
) -> (&'static str, &'static str) {
    match status {
        CostEstimateStatus::Estimating => (
            "Estimating public broadcaster cost...",
            "Using current gas price, transaction fee rate, and selected private note shape.",
        ),
    }
}

pub(super) fn render_public_broadcaster_cost_status(
    _tick: usize,
    status: CostEstimateStatus,
) -> gpui::Div {
    let (title, detail) = public_broadcaster_cost_status_text(status);
    div()
        .flex()
        .items_center()
        .gap_3()
        .p(px(12.0))
        .rounded_md()
        .bg(rgb(theme::SURFACE_ELEVATED))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(
            Spinner::new()
                .icon(IconName::LoaderCircle)
                .color(rgb(theme::INFO).into())
                .with_size(px(18.0)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(app_strong_text(title))
                .child(app_muted_text(detail)),
        )
}

pub(super) fn cost_estimate_detail_text(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_color(rgb(theme::TEXT_SUBTLE))
        .text_size(COST_ESTIMATE_DETAIL_TEXT_SIZE)
        .line_height(px(15.0))
        .child(text.into())
}

pub(super) fn render_private_broadcaster_progress_context(
    progress: &PrivateBroadcasterProgressState,
    context: &PrivateBroadcasterProgressContext<'_>,
    broadcaster_action: Option<AnyElement>,
) -> gpui::Div {
    ui::private_submission::transaction_context(
        private_broadcaster_progress_context_rows(progress, context),
        broadcaster_action,
    )
}

pub(super) fn private_broadcaster_progress_context_rows(
    progress: &PrivateBroadcasterProgressState,
    context: &PrivateBroadcasterProgressContext<'_>,
) -> Vec<ui::private_action::DisplayRow> {
    use ui::private_action::DisplayRow;
    let display = &context.display;
    let mut rows = [
        (
            "Broadcaster",
            broadcaster_candidate_label(display.broadcaster),
        ),
        (
            "Recipient",
            spend_authorization_recipient_display(progress.recipient.as_ref()),
        ),
        (
            "Entered amount",
            display.action_amount(display.entered_amount),
        ),
    ]
    .into_iter()
    .map(|(label, value)| DisplayRow {
        label: label.into(),
        value,
        suffix: None,
    })
    .collect::<Vec<_>>();
    rows.extend(display.outcome_rows(context.anchor_cache));
    rows.push(DisplayRow {
        label: "Network gas".into(),
        value: display.gas_value(),
        suffix: None,
    });
    rows
}

pub(super) fn private_broadcaster_context_row(
    label: impl Into<SharedString>,
    value: String,
) -> gpui::Div {
    private_broadcaster_context_row_with_action(label, value, None)
}

pub(super) fn private_broadcaster_context_row_with_action(
    label: impl Into<SharedString>,
    value: String,
    action: Option<AnyElement>,
) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .child(app_muted_text(label).flex_none())
        .child(strong_wrapping_value(value, None).children(action))
}

fn strong_wrapping_value(value: String, suffix: Option<String>) -> gpui::Div {
    let row = div()
        .min_w(px(0.0))
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_1();

    if let Some(suffix) = suffix {
        return row
            .child(
                app_strong_text(value)
                    .text_align(gpui::TextAlign::Right)
                    .whitespace_nowrap()
                    .flex_none(),
            )
            .child(
                app_strong_text(suffix)
                    .text_align(gpui::TextAlign::Right)
                    .whitespace_nowrap()
                    .flex_none(),
            );
    }

    row.child(
        app_strong_text(value)
            .min_w(px(0.0))
            .text_align(gpui::TextAlign::Right)
            .whitespace_normal(),
    )
}
