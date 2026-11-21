use super::*;

impl WalletRoot {
    pub(in crate::root) fn sponsored_funding_estimate_display(
        &self,
        state: &SponsoredFundingEstimateState,
    ) -> SponsoredFundingEstimateDisplay {
        match state {
            SponsoredFundingEstimateState::PublicBalanceReady(estimate) => {
                let format_gas_cost = |amount| {
                    let token_value =
                        format_native_token_amount_for_display(estimate.chain_id, amount);
                    let usd_micro_value = self
                        .public_broadcaster_anchor_cache
                        .cached_native_usd_micro_value(estimate.chain_id, amount);
                    format_value_with_usd_label(
                        token_value,
                        amount,
                        Some(18),
                        usd_micro_value,
                        false,
                    )
                };
                let gas_cost = estimate.cost.gas_cost;
                let protocol_fee = (!estimate.cost.protocol_fees.is_empty()).then(|| {
                    estimate
                        .cost
                        .protocol_fees
                        .iter()
                        .map(|fee| {
                            self.sponsored_token_cost_label(
                                estimate.chain_id,
                                fee.token,
                                fee.amount,
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" + ")
                });
                SponsoredFundingEstimateDisplay::PublicBalance {
                    expected_gas_cost: format_gas_cost(gas_cost.expected_cost),
                    maximum_gas_cost: public_action_maximum_gas_cost_is_significant(
                        gas_cost.expected_cost,
                        gas_cost.maximum_cost,
                    ).then(|| format_gas_cost(gas_cost.maximum_cost)),
                    protocol_fee,
                }
            }
            SponsoredFundingEstimateState::PublicBalanceUnavailable => {
                SponsoredFundingEstimateDisplay::PublicBalanceError
            }
            SponsoredFundingEstimateState::Ready(estimate) => {
                let expected_excess_deposit = estimate.expected_excess_deposit();
                let expected_excess_deposit_usd_micro_value = self
                    .public_broadcaster_anchor_cache
                    .cached_native_usd_micro_value(estimate.chain_id, expected_excess_deposit);
                let primary_unshield_protocol_fee =
                    estimate.primary_unshield_protocol_fee.map(|fee| {
                        self.sponsored_token_cost_label(estimate.chain_id, fee.token, fee.amount)
                    });
                SponsoredFundingEstimateDisplay::Ready {
                    expected_sponsorship_cost: self.sponsored_token_cost_label(
                        estimate.chain_id,
                        estimate.wrapped_native_token,
                        estimate.expected_sponsorship_cost(),
                    ),
                    gas_cost: self.sponsored_native_cost_label(
                        estimate.chain_id,
                        estimate.expected_network_gas_cost(),
                    ),
                    builder_premium: self
                        .sponsored_native_cost_label(estimate.chain_id, estimate.builder_premium()),
                    primary_unshield_protocol_fee,
                    expected_excess_deposit: self.sponsored_native_cost_label(
                        estimate.chain_id,
                        expected_excess_deposit,
                    ),
                    maximum_spend: self.sponsored_token_cost_label(
                        estimate.chain_id,
                        estimate.wrapped_native_token,
                        estimate.maximum_payment.gross_wrapped_native_spend,
                    ),
                    show_excess_deposit_breakdown:
                        sponsored_excess_deposit_breakdown_visible(
                            expected_excess_deposit,
                            expected_excess_deposit_usd_micro_value,
                        ),
                }
            }
            SponsoredFundingEstimateState::InsufficientWrappedNative {
                chain_id,
                token: Some(token),
                available,
                required,
            } => SponsoredFundingEstimateDisplay::Error(format!(
                "Insufficient private wrapped-native balance for sponsored fees. Available {}; required {}.",
                format_exact_token_amount_for_display(
                    *chain_id,
                    *token,
                    *available,
                    Some(&self.effective_token_registry),
                ),
                format_token_amount_ceiling_for_display(
                    *chain_id,
                    *token,
                    *required,
                    Some(&self.effective_token_registry),
                ),
            )),
            SponsoredFundingEstimateState::InsufficientWrappedNative { .. } => {
                SponsoredFundingEstimateDisplay::Error(
                    "Insufficient private wrapped-native balance for sponsored fees.".to_string(),
                )
            }
            SponsoredFundingEstimateState::InsufficientWrappedNativeForQuote {
                chain_id,
                token: Some(token),
                available,
            } => SponsoredFundingEstimateDisplay::Error(format!(
                "Insufficient private wrapped-native balance to derive the maximum sponsored plan. Available {}; the required maximum is unavailable until the plan is fundable.",
                format_exact_token_amount_for_display(
                    *chain_id,
                    *token,
                    *available,
                    Some(&self.effective_token_registry),
                ),
            )),
            SponsoredFundingEstimateState::InsufficientWrappedNativeForQuote { .. } => {
                SponsoredFundingEstimateDisplay::Error(
                    "Insufficient private wrapped-native balance to derive the maximum sponsored plan."
                        .to_string(),
                )
            }
            SponsoredFundingEstimateState::Unavailable => SponsoredFundingEstimateDisplay::Error(
                "Sponsored fee estimate is unavailable for the current inputs.".to_string(),
            ),
        }
    }

    fn sponsored_native_cost_label(&self, chain_id: u64, amount: U256) -> String {
        let token_value = format_native_token_amount_ceiling_for_display(chain_id, amount);
        let usd_micro_value = self
            .public_broadcaster_anchor_cache
            .cached_native_usd_micro_value(chain_id, amount);
        format_value_with_usd_label(token_value, amount, Some(18), usd_micro_value, false)
    }

    fn sponsored_token_cost_label(&self, chain_id: u64, token: Address, amount: U256) -> String {
        let token_value = format_token_amount_ceiling_for_display(
            chain_id,
            token,
            amount,
            Some(&self.effective_token_registry),
        );
        let usd_micro_value = self
            .public_broadcaster_anchor_cache
            .cached_token_usd_micro_value(chain_id, token, amount);
        format_value_with_usd_label(
            token_value,
            amount,
            token_display_metadata(Some(&self.effective_token_registry), chain_id, &token)
                .map(|metadata| metadata.decimals),
            usd_micro_value,
            false,
        )
    }

    pub(in crate::root) fn debounce_sponsored_funding_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        self.schedule_sponsored_funding_estimate(
            kind,
            key,
            SponsoredEstimateRefreshMode::Invalidate,
            cx,
        );
    }

    pub(in crate::root) fn revalidate_sponsored_funding_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a `delivery:` mode's form keeps its stand-in card on background
        // refreshes, which a refused schedule would clear (`clears_current(false)` clears in
        // Retain mode too). Still notify because the gas quote success callback relies on
        // revalidation to redraw.
        #[cfg(debug_assertions)]
        {
            if crate::root::private_swap::ui_fixture_holds_delivery() {
                cx.notify();
                return;
            }
        }
        self.schedule_sponsored_funding_estimate(
            kind,
            key,
            SponsoredEstimateRefreshMode::Retain,
            cx,
        );
    }

    fn schedule_sponsored_funding_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        refresh_mode: SponsoredEstimateRefreshMode,
        cx: &mut Context<'_, Self>,
    ) {
        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        let can_schedule = self.can_schedule_sponsored_funding_estimate(kind, key);
        let clear_current = refresh_mode.clears_current(can_schedule);
        let form = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).map(|form| {
                if clear_current {
                    form.sponsored_funding_estimate = None;
                }
                form.sponsored_estimate_id = estimate_id;
                form.sponsored_estimate_pending = can_schedule;
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).map(|form| {
                if clear_current {
                    form.sponsored_funding_estimate = None;
                }
                form.sponsored_estimate_id = estimate_id;
                form.sponsored_estimate_pending = can_schedule;
            }),
        };
        if form.is_none() {
            return;
        }
        cx.notify();
        if !can_schedule {
            return;
        }
        cx.spawn(async move |this, cx| {
            tokio::time::sleep(SPONSORED_FUNDING_ESTIMATE_DEBOUNCE).await;
            let _ = this.update(cx, |root, cx| {
                let current_id = match kind {
                    DeliveryFormKind::Send => root
                        .send_forms
                        .get(&key)
                        .map(|form| form.sponsored_estimate_id),
                    DeliveryFormKind::Unshield => root
                        .unshield_forms
                        .get(&key)
                        .map(|form| form.sponsored_estimate_id),
                };
                if current_id == Some(estimate_id) {
                    root.start_sponsored_funding_estimate(kind, key, estimate_id, cx);
                }
            });
        })
        .detach();
    }

    fn can_schedule_sponsored_funding_estimate(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> bool {
        // Debug UI fixture: a `delivery:` mode's forms schedule no sponsored estimate.
        // Refusing clears the held card on an edit, as on any form, while
        // `revalidate_sponsored_funding_estimate` returns first so background refreshes keep it.
        #[cfg(debug_assertions)]
        {
            if crate::root::private_swap::ui_fixture_holds_delivery() {
                return false;
            }
        }
        let (delivery_mode, generating) = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .map(|form| (form.delivery_mode, form.generating)),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .map(|form| (form.delivery_mode, form.generating)),
        }
        .unwrap_or((DeliveryMode::ManualCalldata, true));
        !generating && delivery_mode == DeliveryMode::SelfBroadcast
    }

    fn start_sponsored_funding_estimate(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        estimate_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let funding = match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).map(|form| {
                effective_self_broadcast_funding_mode(
                    self.effective_chain_configs.get(form.asset.chain_id),
                    form.self_broadcast_funding,
                )
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).map(|form| {
                effective_self_broadcast_funding_mode(
                    self.effective_chain_configs.get(form.asset.chain_id),
                    form.self_broadcast_funding,
                )
            }),
        };
        if funding == Some(SelfBroadcastFundingMode::PublicBalance) {
            match kind {
                DeliveryFormKind::Send => {
                    self.start_public_balance_send_estimate(key, estimate_id, cx);
                }
                DeliveryFormKind::Unshield => {
                    self.start_public_balance_unshield_estimate(key, estimate_id, cx);
                }
            }
            return;
        }
        match kind {
            DeliveryFormKind::Send => {
                self.start_sponsored_send_estimate(key, estimate_id, cx);
            }
            DeliveryFormKind::Unshield => {
                self.start_sponsored_unshield_estimate(key, estimate_id, cx);
            }
        }
    }
}
