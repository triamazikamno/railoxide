#[allow(clippy::wildcard_imports)]
use super::*;

impl<'a> PublicBroadcasterCostDisplay<'a> {
    pub(in crate::root) const fn fee_display(&self) -> PublicBroadcasterFeeDisplay<'_> {
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

    pub(in crate::root) const fn from_result(
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

    pub(in crate::root) const fn from_estimate(
        asset: &UnshieldAsset,
        estimate: &'a PublicBroadcasterCostEstimate,
        fee_anchor_rate: Option<U256>,
        registry: Option<&'a EffectiveTokenRegistry>,
    ) -> Self {
        Self::from_estimate_chain(asset.chain_id, estimate, fee_anchor_rate, registry)
    }

    pub(in crate::root) const fn from_estimate_chain(
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

    pub(in crate::root) fn outcome_rows(
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

    pub(in crate::root) fn fee_rows(
        &self,
        anchor_cache: &TokenAnchorRateCache,
    ) -> Vec<ui::private_action::DisplayRow> {
        self.fee_display().fee_rows(anchor_cache)
    }

    pub(in crate::root) fn private_spend_label(&self) -> &'static str {
        if self.action_token == self.fee_token {
            "Total private spend"
        } else {
            "Action-token private spend"
        }
    }

    pub(in crate::root) fn action_amount(&self, amount: U256) -> String {
        format_token_amount_for_display(self.chain_id, self.action_token, amount, self.registry)
    }

    pub(in crate::root) fn action_amount_with_usd(
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

    pub(in crate::root) fn fee_amount_with_usd(
        &self,
        anchor_cache: &TokenAnchorRateCache,
    ) -> String {
        self.fee_display().fee_amount_with_usd(anchor_cache)
    }

    pub(in crate::root) fn protocol_fee_value_with_usd(
        &self,
        anchor_cache: &TokenAnchorRateCache,
    ) -> String {
        self.action_amount_with_usd(self.protocol_fee_amount, anchor_cache)
    }

    pub(in crate::root) fn gas_value(&self) -> String {
        self.fee_display().gas_value()
    }

    pub(in crate::root) fn native_top_up_recipient_suffix(&self) -> Option<String> {
        self.native_top_up.as_ref().map(|top_up| {
            format_native_top_up_recipient_suffix(self.chain_id, top_up.native_amount)
        })
    }
}

impl PublicBroadcasterFeeDisplay<'_> {
    pub(in crate::root) fn fee_rows(
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

    pub(in crate::root) fn fee_amount(&self) -> String {
        format_token_amount_for_display(
            self.chain_id,
            self.fee_token,
            self.fee_amount,
            self.registry,
        )
    }

    pub(in crate::root) fn fee_amount_with_usd(
        &self,
        anchor_cache: &TokenAnchorRateCache,
    ) -> String {
        self.token_amount_value_with_usd(
            self.fee_amount(),
            anchor_cache,
            self.fee_token,
            self.fee_amount,
            false,
        )
    }

    pub(in crate::root) fn fee_breakdown(&self) -> PublicBroadcasterFeeBreakdown {
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

    pub(in crate::root) fn native_gas_cost_value(
        &self,
        breakdown: &PublicBroadcasterFeeBreakdown,
    ) -> String {
        format_native_token_amount_for_display(self.chain_id, breakdown.native_gas_cost)
    }

    pub(in crate::root) fn native_gas_cost_value_with_usd(
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

    pub(in crate::root) fn broadcaster_fee_value(
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

    pub(in crate::root) fn broadcaster_fee_value_with_usd(
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

    pub(in crate::root) fn gas_value(&self) -> String {
        format!(
            "~{} gas @ {} gwei",
            self.gas_limit,
            format_gwei(public_broadcaster_service_gas_price(self.min_gas_price))
        )
    }

    pub(super) fn token_amount_value_with_usd(
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
