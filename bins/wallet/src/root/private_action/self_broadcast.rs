use super::*;

pub(in crate::root) use ui::private_action::self_broadcast::SPONSORSHIP_LABEL as BLOCK_BUILDER_SPONSORSHIP_LABEL;
const SPONSORED_FUNDING_ESTIMATE_DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SponsoredEstimateRefreshMode {
    Invalidate,
    Retain,
}

impl SponsoredEstimateRefreshMode {
    pub(in crate::root) const fn clears_current(self, can_schedule: bool) -> bool {
        !can_schedule || matches!(self, Self::Invalidate)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) struct SponsoredAssetFee {
    pub(in crate::root) token: Address,
    pub(in crate::root) amount: U256,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) struct SponsoredFundingEstimate {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) wrapped_native_token: Address,
    pub(in crate::root) expected_payment: SponsorshipPayment,
    pub(in crate::root) maximum_payment: SponsorshipPayment,
    pub(in crate::root) primary_unshield_protocol_fee: Option<SponsoredAssetFee>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) struct PublicBalanceFundingEstimate {
    pub(in crate::root) chain_id: u64,
    pub(in crate::root) cost: DesktopSelfBroadcastCostEstimate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::root) enum SponsoredFundingEstimateState {
    PublicBalanceReady(Box<PublicBalanceFundingEstimate>),
    PublicBalanceUnavailable,
    Ready(Box<SponsoredFundingEstimate>),
    InsufficientWrappedNative {
        chain_id: u64,
        token: Option<Address>,
        available: U256,
        required: U256,
    },
    InsufficientWrappedNativeForQuote {
        chain_id: u64,
        token: Option<Address>,
        available: U256,
    },
    Unavailable,
}

pub(in crate::root) fn sponsored_estimate_allows_submission(
    funding: SelfBroadcastFundingMode,
    state: Option<&SponsoredFundingEstimateState>,
    _pending: bool,
) -> bool {
    funding != SelfBroadcastFundingMode::PrivateSponsorship
        || matches!(state, Some(SponsoredFundingEstimateState::Ready(_)))
}

pub(in crate::root) fn sponsored_estimate_failure_state(
    chain_id: u64,
    wrapped_native_token: Option<Address>,
    error: &eyre::Report,
) -> SponsoredFundingEstimateState {
    match error.downcast_ref::<SponsorshipError>() {
        Some(SponsorshipError::InsufficientWrappedNative {
            available,
            required,
        }) => SponsoredFundingEstimateState::InsufficientWrappedNative {
            chain_id,
            token: wrapped_native_token,
            available: *available,
            required: *required,
        },
        Some(SponsorshipError::InsufficientWrappedNativeForQuote { available }) => {
            SponsoredFundingEstimateState::InsufficientWrappedNativeForQuote {
                chain_id,
                token: wrapped_native_token,
                available: *available,
            }
        }
        _ => SponsoredFundingEstimateState::Unavailable,
    }
}

pub(in crate::root) fn sponsored_estimate_from_authorization_limit(
    chain_id: u64,
    limit: SponsoredAuthorizationLimit,
    expected_fee_per_gas: u128,
    primary_unshield_protocol_fee: Option<SponsoredAssetFee>,
) -> SponsoredFundingEstimateState {
    let Ok(expected_payment) = sponsorship_payment(
        limit.max_transaction_gas_limit,
        expected_fee_per_gas,
        limit.signer_native_balance_snapshot,
        limit.incentive,
    ) else {
        return SponsoredFundingEstimateState::Unavailable;
    };
    let Ok(maximum_payment) = limit.maximum_payment() else {
        return SponsoredFundingEstimateState::Unavailable;
    };
    SponsoredFundingEstimateState::Ready(Box::new(SponsoredFundingEstimate {
        chain_id,
        wrapped_native_token: limit.wrapped_native_token,
        expected_payment,
        maximum_payment,
        primary_unshield_protocol_fee,
    }))
}

pub(in crate::root) use ui::private_action::self_broadcast::FeeDisplay as SponsoredFundingEstimateDisplay;

pub(in crate::root) fn sponsored_excess_deposit_breakdown_visible(
    excess_deposit: U256,
    usd_micro_value: Option<U256>,
) -> bool {
    const MINIMUM_USD_MICRO_VALUE: u64 = 10_000;
    !excess_deposit.is_zero()
        && usd_micro_value.is_none_or(|value| value >= U256::from(MINIMUM_USD_MICRO_VALUE))
}

impl SponsoredFundingEstimate {
    pub(in crate::root) const fn builder_premium(&self) -> U256 {
        self.maximum_payment
            .gross_wrapped_native_spend
            .saturating_sub(self.maximum_payment.reimbursement_base)
    }

    pub(in crate::root) const fn expected_excess_deposit(&self) -> U256 {
        self.maximum_payment
            .funding_principal
            .saturating_sub(self.expected_payment.outer_gas_cap)
    }

    pub(in crate::root) const fn expected_network_gas_cost(&self) -> U256 {
        self.expected_payment
            .outer_gas_cap
            .saturating_add(self.maximum_payment.funding_gas_cap)
    }

    pub(in crate::root) const fn expected_sponsorship_cost(&self) -> U256 {
        self.expected_network_gas_cost()
            .saturating_add(self.builder_premium())
    }
}

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
                    self.effective_chain_configs.get(&form.asset.chain_id),
                    form.self_broadcast_funding,
                )
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).map(|form| {
                effective_self_broadcast_funding_mode(
                    self.effective_chain_configs.get(&form.asset.chain_id),
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

    fn start_public_balance_send_estimate(
        &mut self,
        key: UnshieldAssetKey,
        estimate_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.send_forms.get(&key) else {
            return;
        };
        let asset = form.asset.clone();
        if parse_railgun_recipient(form.recipient_value.trim()).is_err() {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        }
        let Ok(amount) =
            parse_send_amount(form.amount_input.read(cx).value().as_ref(), asset.decimals)
        else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        if amount.is_zero() || amount > asset.max_batched {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        }
        let Ok(gas_fee) = form.self_broadcast_gas_fee.selection(cx) else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Some((max_fee_per_gas, max_priority_fee_per_gas)) =
            self_broadcast_initial_gas_values(&gas_fee, form.self_broadcast_gas_fee.quote)
        else {
            self.set_public_balance_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                SponsoredFundingEstimateState::PublicBalanceUnavailable,
                cx,
            );
            return;
        };
        let quote = form
            .self_broadcast_gas_fee
            .quote
            .unwrap_or_else(|| SelfBroadcastGasFeeQuote::from_rpc_gas_price(max_fee_per_gas));
        let Some(ChainUtxoState::Ready { session, .. }) = self.chain_states.get(&asset.chain_id)
        else {
            return;
        };
        let utxos = session.unspent_utxos();
        let join = self.runtime.spawn_blocking(move || {
            estimate_desktop_send_self_broadcast_cost(
                &utxos,
                asset.token,
                amount,
                quote,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )
            .map_or(
                SponsoredFundingEstimateState::PublicBalanceUnavailable,
                |cost| {
                    SponsoredFundingEstimateState::PublicBalanceReady(Box::new(
                        PublicBalanceFundingEstimate {
                            chain_id: asset.chain_id,
                            cost,
                        },
                    ))
                },
            )
        });
        Self::finish_public_balance_estimate(DeliveryFormKind::Send, key, estimate_id, join, cx);
    }

    fn start_public_balance_unshield_estimate(
        &mut self,
        key: UnshieldAssetKey,
        estimate_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        self.refresh_unshield_native_top_up_state(key, cx);
        let Some(form) = self.unshield_forms.get(&key) else {
            return;
        };
        let asset = form.asset.clone();
        if parse_address(form.recipient_value.trim()).is_none() {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        }
        let Ok(amount) =
            parse_unshield_amount(form.amount_input.read(cx).value().as_ref(), asset.decimals)
        else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let fee_mode = effective_fee_handling_mode(
            DeliveryFormKind::Unshield,
            asset.token,
            form.selected_fee_token,
            form.fee_mode,
        );
        let max_entered_amount =
            unshield_form_max_entered_amount(form, form.delivery_mode, fee_mode)
                .unwrap_or(asset.max_batched);
        if amount.is_zero() || amount > max_entered_amount {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        }
        let Ok(gas_fee) = form.self_broadcast_gas_fee.selection(cx) else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Some((max_fee_per_gas, max_priority_fee_per_gas)) =
            self_broadcast_initial_gas_values(&gas_fee, form.self_broadcast_gas_fee.quote)
        else {
            self.set_public_balance_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                SponsoredFundingEstimateState::PublicBalanceUnavailable,
                cx,
            );
            return;
        };
        let quote = form
            .self_broadcast_gas_fee
            .quote
            .unwrap_or_else(|| SelfBroadcastGasFeeQuote::from_rpc_gas_price(max_fee_per_gas));
        let Some(ChainUtxoState::Ready { session, .. }) = self.chain_states.get(&asset.chain_id)
        else {
            return;
        };
        let utxos = session.unspent_utxos();
        let unwrap = form.unwrap;
        let native_top_up =
            enabled_native_top_up_plan(form.native_top_up_enabled, form.native_top_up.as_ref());
        let join = self.runtime.spawn_blocking(move || {
            estimate_desktop_unshield_self_broadcast_cost(
                &utxos,
                asset.token,
                amount,
                fee_mode,
                unwrap,
                native_top_up.as_ref(),
                quote,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )
            .map_or(
                SponsoredFundingEstimateState::PublicBalanceUnavailable,
                |cost| {
                    SponsoredFundingEstimateState::PublicBalanceReady(Box::new(
                        PublicBalanceFundingEstimate {
                            chain_id: asset.chain_id,
                            cost,
                        },
                    ))
                },
            )
        });
        Self::finish_public_balance_estimate(
            DeliveryFormKind::Unshield,
            key,
            estimate_id,
            join,
            cx,
        );
    }

    fn finish_public_balance_estimate(
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        estimate_id: u64,
        join: tokio::task::JoinHandle<SponsoredFundingEstimateState>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let estimate = join
                .await
                .unwrap_or(SponsoredFundingEstimateState::PublicBalanceUnavailable);
            let _ = this.update(cx, |root, cx| {
                root.set_public_balance_estimate_if_current(kind, key, estimate_id, estimate, cx);
            });
        })
        .detach();
    }

    fn set_public_balance_estimate_if_current(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        estimate_id: u64,
        estimate: SponsoredFundingEstimateState,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.sponsored_estimate_id != estimate_id {
                    return false;
                }
                let changed = form.sponsored_funding_estimate.as_ref() != Some(&estimate)
                    || form.sponsored_estimate_pending;
                form.sponsored_funding_estimate = Some(estimate);
                form.sponsored_estimate_pending = false;
                changed
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.sponsored_estimate_id != estimate_id {
                    return false;
                }
                let changed = form.sponsored_funding_estimate.as_ref() != Some(&estimate)
                    || form.sponsored_estimate_pending;
                form.sponsored_funding_estimate = Some(estimate);
                form.sponsored_estimate_pending = false;
                changed
            }),
        };
        if changed {
            cx.notify();
        }
    }

    fn clear_sponsored_funding_estimate_if_current(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        estimate_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let form = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).map(|form| {
                if form.sponsored_estimate_id != estimate_id {
                    return false;
                }
                let changed = form.sponsored_funding_estimate.take().is_some()
                    || form.sponsored_estimate_pending;
                form.sponsored_estimate_pending = false;
                changed
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).map(|form| {
                if form.sponsored_estimate_id != estimate_id {
                    return false;
                }
                let changed = form.sponsored_funding_estimate.take().is_some()
                    || form.sponsored_estimate_pending;
                form.sponsored_estimate_pending = false;
                changed
            }),
        };
        if form == Some(true) {
            cx.notify();
        }
    }

    fn start_sponsored_send_estimate(
        &mut self,
        key: UnshieldAssetKey,
        estimate_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.send_forms.get(&key) else {
            return;
        };
        let asset = form.asset.clone();
        let Ok(recipient) = parse_railgun_recipient(form.recipient_value.trim()) else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Ok(amount) =
            parse_send_amount(form.amount_input.read(cx).value().as_ref(), asset.decimals)
        else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        if amount.is_zero() || amount > asset.max_batched {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        }
        let Ok(incentive) = sponsored_incentive_from_text(
            form.sponsored_incentive,
            form.sponsored_custom_incentive_input
                .read(cx)
                .value()
                .as_ref(),
        ) else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Ok(gas_fee) = form.self_broadcast_gas_fee.selection(cx) else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Some((max_fee_per_gas, max_priority_fee_per_gas)) =
            self_broadcast_initial_gas_values(&gas_fee, form.self_broadcast_gas_fee.quote)
        else {
            return;
        };
        let gas_quote = form
            .self_broadcast_gas_fee
            .quote
            .unwrap_or_else(|| SelfBroadcastGasFeeQuote::from_rpc_gas_price(max_fee_per_gas));
        let expected_fee_per_gas =
            expected_eip1559_fee_per_gas(gas_quote, max_fee_per_gas, max_priority_fee_per_gas);
        let Some(signer) = self
            .selected_self_broadcast_gas_payer_account(
                form.self_broadcast_gas_payer_uuid.as_deref(),
            )
            .map(|account| account.address)
        else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let signer_native_balance_snapshot =
            form.self_broadcast_gas_payer_uuid
                .as_deref()
                .map_or(U256::ZERO, |uuid| {
                    self_broadcast_native_balance_amount(
                        self.public_balance_snapshot.as_deref(),
                        asset.chain_id,
                        uuid,
                    )
                });
        let Some(effective_chain) = self.effective_chain_configs.get(&asset.chain_id).cloned()
        else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Send,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Some(ChainUtxoState::Ready { session, .. }) = self.chain_states.get(&asset.chain_id)
        else {
            return;
        };
        let wrapped_native_token = effective_chain
            .wrapped_native_token
            .as_deref()
            .and_then(parse_address);
        let session = Arc::clone(session);
        let join = self.runtime.spawn_blocking(move || {
            let limit = match quote_sponsored_send_authorization_limit(
                asset.chain_id,
                &effective_chain,
                &session.unspent_utxos(),
                asset.token,
                amount,
                &recipient,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                signer_native_balance_snapshot,
                incentive,
                signer,
            ) {
                Ok(limit) => limit,
                Err(error) => {
                    return sponsored_estimate_failure_state(
                        asset.chain_id,
                        wrapped_native_token,
                        &error,
                    );
                }
            };
            sponsored_estimate_from_authorization_limit(
                asset.chain_id,
                limit,
                expected_fee_per_gas,
                None,
            )
        });
        cx.spawn(async move |this, cx| {
            let quoted = join
                .await
                .unwrap_or(SponsoredFundingEstimateState::Unavailable);
            let _ = this.update(cx, |root, cx| {
                let Some(form) = root.send_forms.get_mut(&key) else {
                    return;
                };
                let resolved = Some(quoted);
                if form.sponsored_estimate_id == estimate_id
                    && (form.sponsored_funding_estimate != resolved
                        || form.sponsored_estimate_pending)
                {
                    form.sponsored_funding_estimate = resolved;
                    form.sponsored_estimate_pending = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn start_sponsored_unshield_estimate(
        &mut self,
        key: UnshieldAssetKey,
        estimate_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        self.refresh_unshield_native_top_up_state(key, cx);
        let Some(form) = self.unshield_forms.get(&key) else {
            return;
        };
        let asset = form.asset.clone();
        let Some(recipient) = parse_address(form.recipient_value.trim()) else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Ok(amount) =
            parse_unshield_amount(form.amount_input.read(cx).value().as_ref(), asset.decimals)
        else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let fee_mode = effective_fee_handling_mode(
            DeliveryFormKind::Unshield,
            asset.token,
            form.selected_fee_token,
            form.fee_mode,
        );
        let max_entered_amount =
            unshield_form_max_entered_amount(form, form.delivery_mode, fee_mode)
                .unwrap_or(asset.max_batched);
        if amount.is_zero() || amount > max_entered_amount {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        }
        let Ok(incentive) = sponsored_incentive_from_text(
            form.sponsored_incentive,
            form.sponsored_custom_incentive_input
                .read(cx)
                .value()
                .as_ref(),
        ) else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Ok(gas_fee) = form.self_broadcast_gas_fee.selection(cx) else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Some((max_fee_per_gas, max_priority_fee_per_gas)) =
            self_broadcast_initial_gas_values(&gas_fee, form.self_broadcast_gas_fee.quote)
        else {
            return;
        };
        let gas_quote = form
            .self_broadcast_gas_fee
            .quote
            .unwrap_or_else(|| SelfBroadcastGasFeeQuote::from_rpc_gas_price(max_fee_per_gas));
        let expected_fee_per_gas =
            expected_eip1559_fee_per_gas(gas_quote, max_fee_per_gas, max_priority_fee_per_gas);
        let Some(signer) = self
            .selected_self_broadcast_gas_payer_account(
                form.self_broadcast_gas_payer_uuid.as_deref(),
            )
            .map(|account| account.address)
        else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let signer_native_balance_snapshot =
            form.self_broadcast_gas_payer_uuid
                .as_deref()
                .map_or(U256::ZERO, |uuid| {
                    self_broadcast_native_balance_amount(
                        self.public_balance_snapshot.as_deref(),
                        asset.chain_id,
                        uuid,
                    )
                });
        let Some(effective_chain) = self.effective_chain_configs.get(&asset.chain_id).cloned()
        else {
            self.clear_sponsored_funding_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                cx,
            );
            return;
        };
        let Some(ChainUtxoState::Ready { session, .. }) = self.chain_states.get(&asset.chain_id)
        else {
            return;
        };
        let wrapped_native_token = effective_chain
            .wrapped_native_token
            .as_deref()
            .and_then(parse_address);
        let session = Arc::clone(session);
        let unwrap = form.unwrap;
        let native_top_up =
            enabled_native_top_up_plan(form.native_top_up_enabled, form.native_top_up.as_ref());
        let join = self.runtime.spawn_blocking(move || {
            let limit = match quote_sponsored_unshield_authorization_limit(
                asset.chain_id,
                &effective_chain,
                &session.unspent_utxos(),
                asset.token,
                amount,
                fee_mode,
                recipient,
                unwrap,
                native_top_up.as_ref(),
                max_fee_per_gas,
                max_priority_fee_per_gas,
                signer_native_balance_snapshot,
                incentive,
                signer,
            ) {
                Ok(limit) => limit,
                Err(error) => {
                    return sponsored_estimate_failure_state(
                        asset.chain_id,
                        wrapped_native_token,
                        &error,
                    );
                }
            };
            let Ok(protocol_fee) = unshield_protocol_fee_amount_for_fee_mode(amount, fee_mode)
            else {
                return SponsoredFundingEstimateState::Unavailable;
            };
            sponsored_estimate_from_authorization_limit(
                asset.chain_id,
                limit,
                expected_fee_per_gas,
                Some(SponsoredAssetFee {
                    token: asset.token,
                    amount: protocol_fee,
                }),
            )
        });
        cx.spawn(async move |this, cx| {
            let quoted = join
                .await
                .unwrap_or(SponsoredFundingEstimateState::Unavailable);
            let _ = this.update(cx, |root, cx| {
                let Some(form) = root.unshield_forms.get_mut(&key) else {
                    return;
                };
                let resolved = Some(quoted);
                if form.sponsored_estimate_id == estimate_id
                    && (form.sponsored_funding_estimate != resolved
                        || form.sponsored_estimate_pending)
                {
                    form.sponsored_funding_estimate = resolved;
                    form.sponsored_estimate_pending = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn set_self_broadcast_funding_mode(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        funding: SelfBroadcastFundingMode,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && !form.generating
                {
                    form.self_broadcast_funding = funding;
                    form.error = None;
                    form.result = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && !form.generating
                {
                    form.self_broadcast_funding = funding;
                    form.error = None;
                    form.result = None;
                }
            }
        }
        self.debounce_sponsored_funding_estimate(kind, key, cx);
    }

    pub(in crate::root) fn set_sponsored_incentive(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        incentive: SponsoredIncentive,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && !form.generating
                {
                    form.sponsored_incentive = incentive;
                    form.error = None;
                    form.result = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && !form.generating
                {
                    form.sponsored_incentive = incentive;
                    form.error = None;
                    form.result = None;
                }
            }
        }
        self.debounce_sponsored_funding_estimate(kind, key, cx);
    }

    pub(in crate::root) fn set_sponsored_custom_incentive_from_text(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        value: &str,
        cx: &mut Context<'_, Self>,
    ) {
        let parsed = value
            .trim()
            .parse::<u8>()
            .ok()
            .map(SponsoredIncentive::Custom)
            .filter(|incentive| incentive.percent().is_ok());
        let error = parsed.is_none().then(|| {
            Arc::<str>::from("Custom builder incentive must be an integer from 1% through 100%.")
        });
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && !form.generating
                {
                    if let Some(incentive) = parsed {
                        form.sponsored_incentive = incentive;
                    }
                    form.error = error;
                    form.result = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && !form.generating
                {
                    if let Some(incentive) = parsed {
                        form.sponsored_incentive = incentive;
                    }
                    form.error = error;
                    form.result = None;
                }
            }
        }
        self.debounce_sponsored_funding_estimate(kind, key, cx);
    }

    pub(in crate::root) fn active_self_broadcast_gas_payer_accounts(
        &self,
    ) -> Vec<PublicAccountMetadata> {
        self.public_accounts
            .iter()
            .filter(|account| account.status == PublicAccountStatus::Active)
            .cloned()
            .collect()
    }

    pub(in crate::root) fn default_self_broadcast_gas_payer_uuid(&self) -> Option<Arc<str>> {
        default_self_broadcast_gas_payer_uuid(&self.active_self_broadcast_gas_payer_accounts())
    }

    pub(in crate::root) fn new_self_broadcast_gas_payer_select(
        &self,
        chain_id: u64,
        selected_uuid: Option<&str>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<SelectState<FullWidthSelectItems<SelfBroadcastGasPayerSelectItem>>> {
        let accounts = self.active_self_broadcast_gas_payer_accounts();
        let items = self_broadcast_gas_payer_select_items(
            &accounts,
            chain_id,
            self.public_balance_snapshot.as_deref(),
        );
        let selected_index = self_broadcast_gas_payer_select_index(&items, selected_uuid);
        cx.new(|cx| {
            SelectState::new(FullWidthSelectItems::new(items), selected_index, window, cx)
                .searchable(true)
        })
    }

    pub(in crate::root) fn new_private_action_asset_select(
        &self,
        kind: DeliveryFormKind,
        chain_id: u64,
        selected_token: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> (
        Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>,
        Vec<PrivateActionAssetSelectItem>,
    ) {
        let assets = self.private_action_asset_options(kind, chain_id);
        let items = private_action_asset_select_items(&assets);
        let selected_index = private_action_asset_select_index(&items, selected_token);
        let state_items = items.clone();
        (
            cx.new(|cx| {
                SelectState::new(SearchableVec::new(items), selected_index, window, cx)
                    .searchable(true)
            }),
            state_items,
        )
    }

    pub(in crate::root) fn sync_self_broadcast_gas_payer_selects(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.active_self_broadcast_gas_payer_accounts();
        let snapshot = self.public_balance_snapshot.clone();
        let mut changed = Vec::new();
        for (key, form) in &mut self.send_forms {
            let selected = normalized_self_broadcast_gas_payer_uuid(
                form.self_broadcast_gas_payer_uuid.as_ref(),
                &accounts,
            );
            if form.self_broadcast_gas_payer_uuid != selected {
                changed.push((DeliveryFormKind::Send, *key));
            }
            form.self_broadcast_gas_payer_uuid.clone_from(&selected);
            sync_self_broadcast_gas_payer_select_entity(
                &form.self_broadcast_gas_payer_select,
                &accounts,
                form.asset.chain_id,
                snapshot.as_deref(),
                selected.as_ref(),
                window,
                cx,
            );
        }
        for (key, form) in &mut self.unshield_forms {
            let selected = normalized_self_broadcast_gas_payer_uuid(
                form.self_broadcast_gas_payer_uuid.as_ref(),
                &accounts,
            );
            if form.self_broadcast_gas_payer_uuid != selected {
                changed.push((DeliveryFormKind::Unshield, *key));
            }
            form.self_broadcast_gas_payer_uuid.clone_from(&selected);
            sync_self_broadcast_gas_payer_select_entity(
                &form.self_broadcast_gas_payer_select,
                &accounts,
                form.asset.chain_id,
                snapshot.as_deref(),
                selected.as_ref(),
                window,
                cx,
            );
        }
        for (kind, key) in changed {
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn revalidate_sponsored_estimates_for_public_balance_change(
        &mut self,
        previous: Option<&PublicBalanceSnapshot>,
        current: &PublicBalanceSnapshot,
        cx: &mut Context<'_, Self>,
    ) {
        let mut changed = Vec::new();
        for (key, form) in &self.send_forms {
            if !form.generating
                && form.asset.chain_id == current.chain_id
                && form.delivery_mode == DeliveryMode::SelfBroadcast
                && form.self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
                && form
                    .self_broadcast_gas_payer_uuid
                    .as_deref()
                    .is_some_and(|uuid| {
                        sponsored_signer_balance_snapshot_changed(
                            previous,
                            current,
                            current.chain_id,
                            uuid,
                        )
                    })
            {
                changed.push((DeliveryFormKind::Send, *key));
            }
        }
        for (key, form) in &self.unshield_forms {
            if !form.generating
                && form.asset.chain_id == current.chain_id
                && form.delivery_mode == DeliveryMode::SelfBroadcast
                && form.self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
                && form
                    .self_broadcast_gas_payer_uuid
                    .as_deref()
                    .is_some_and(|uuid| {
                        sponsored_signer_balance_snapshot_changed(
                            previous,
                            current,
                            current.chain_id,
                            uuid,
                        )
                    })
            {
                changed.push((DeliveryFormKind::Unshield, *key));
            }
        }
        for (kind, key) in changed {
            self.revalidate_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn sync_self_broadcast_gas_payer_select(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.active_self_broadcast_gas_payer_accounts();
        let snapshot = self.public_balance_snapshot.clone();
        match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get_mut(&key) else {
                    return;
                };
                sync_self_broadcast_gas_payer_select_entity(
                    &form.self_broadcast_gas_payer_select,
                    &accounts,
                    form.asset.chain_id,
                    snapshot.as_deref(),
                    form.self_broadcast_gas_payer_uuid.as_ref(),
                    window,
                    cx,
                );
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get_mut(&key) else {
                    return;
                };
                sync_self_broadcast_gas_payer_select_entity(
                    &form.self_broadcast_gas_payer_select,
                    &accounts,
                    form.asset.chain_id,
                    snapshot.as_deref(),
                    form.self_broadcast_gas_payer_uuid.as_ref(),
                    window,
                    cx,
                );
            }
        }
    }

    pub(in crate::root) fn selected_self_broadcast_gas_payer_account(
        &self,
        selected_uuid: Option<&str>,
    ) -> Option<&PublicAccountMetadata> {
        let selected_uuid = selected_uuid?;
        self.public_accounts.iter().find(|account| {
            account.status == PublicAccountStatus::Active
                && account.public_account_uuid == selected_uuid
        })
    }
}

pub(in crate::root) const fn sponsored_self_broadcast_availability_reason(
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
) -> Option<&'static str> {
    let Some(chain) = chain else {
        return Some("Selected chain settings are unavailable.");
    };
    if chain.sponsored_bundle_relays.is_empty() {
        return Some("No compatible sponsored relay is configured for this chain.");
    }
    if chain.wrapped_native_token.is_none() {
        return Some("This chain has no configured wrapped-native token.");
    }
    if chain.coinbase_payer.is_none() {
        return Some("This chain has no configured reviewed coinbase payer.");
    }
    None
}

pub(in crate::root) fn sponsored_funding_choice_visible(
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
) -> bool {
    chain.is_some_and(|chain| !chain.sponsored_bundle_relays.is_empty())
}

pub(in crate::root) const fn sponsored_funding_enabled(
    choice_visible: bool,
    unavailable_reason: Option<&str>,
) -> bool {
    choice_visible && unavailable_reason.is_none()
}

pub(in crate::root) fn effective_self_broadcast_funding_mode(
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    selected: SelfBroadcastFundingMode,
) -> SelfBroadcastFundingMode {
    if sponsored_funding_choice_visible(chain) {
        selected
    } else {
        SelfBroadcastFundingMode::PublicBalance
    }
}

pub(in crate::root) fn effective_delivery_funding_mode(
    delivery_mode: DeliveryMode,
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    selected: SelfBroadcastFundingMode,
) -> SelfBroadcastFundingMode {
    if delivery_mode == DeliveryMode::SelfBroadcast {
        effective_self_broadcast_funding_mode(chain, selected)
    } else {
        SelfBroadcastFundingMode::PublicBalance
    }
}

pub(in crate::root) fn sponsored_incentive_from_text(
    selected: SponsoredIncentive,
    custom: &str,
) -> Result<SponsoredIncentive, &'static str> {
    if !matches!(selected, SponsoredIncentive::Custom(_)) {
        return Ok(selected);
    }
    let percent = custom
        .trim()
        .parse::<u8>()
        .map_err(|_| "Custom builder incentive must be an integer from 1% through 100%.")?;
    let incentive = SponsoredIncentive::Custom(percent);
    incentive
        .percent()
        .map_err(|_| "Custom builder incentive must be an integer from 1% through 100%.")?;
    Ok(incentive)
}

pub(in crate::root) fn default_self_broadcast_gas_payer_uuid(
    accounts: &[PublicAccountMetadata],
) -> Option<Arc<str>> {
    (accounts.len() == 1).then(|| Arc::from(accounts[0].public_account_uuid.as_str()))
}

#[cfg(test)]
pub(in crate::root) fn self_broadcast_gas_payer_matches_search(
    account: &PublicAccountMetadata,
    query: &str,
) -> bool {
    self_broadcast_gas_payer_fields_match(
        public_account_display_label(account).as_deref(),
        &account.address,
        query,
    )
}

pub(in crate::root) fn self_broadcast_gas_payer_fields_match(
    label: Option<&str>,
    address: &Address,
    query: &str,
) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    let full_address = address.to_checksum(None).to_ascii_lowercase();
    let lower_hex_address = format!("{address:#x}");
    let short = short_address(address).to_ascii_lowercase();
    label.is_some_and(|label| label.to_ascii_lowercase().contains(&query))
        || full_address.contains(&query)
        || lower_hex_address.contains(&query)
        || short.contains(&query)
}

pub(in crate::root) fn self_broadcast_gas_payer_label(account: &PublicAccountMetadata) -> String {
    public_account_display_label(account).unwrap_or_else(|| short_address(&account.address))
}

pub(in crate::root) fn self_broadcast_native_balance_entry(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
) -> Option<PublicBalanceEntry> {
    public_balance_entry_for_chain(
        snapshot,
        chain_id,
        public_account_uuid,
        PublicAssetId::Native,
        PublicAccountStatus::Active,
    )
}

pub(in crate::root) fn self_broadcast_native_balance_amount(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
) -> U256 {
    self_broadcast_native_balance_entry(snapshot, chain_id, public_account_uuid)
        .and_then(|entry| entry.amount.amount())
        .unwrap_or_default()
}

pub(in crate::root) fn sponsored_signer_balance_snapshot_changed(
    previous: Option<&PublicBalanceSnapshot>,
    current: &PublicBalanceSnapshot,
    chain_id: u64,
    public_account_uuid: &str,
) -> bool {
    let previous = self_broadcast_native_balance_entry(previous, chain_id, public_account_uuid)
        .map(|entry| entry.amount);
    let current = self_broadcast_native_balance_entry(Some(current), chain_id, public_account_uuid)
        .map(|entry| entry.amount);
    previous != current
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::root) enum SelfBroadcastNativeBalanceState {
    Unknown,
    Zero,
    Positive,
}

pub(in crate::root) fn self_broadcast_native_balance_state(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
) -> SelfBroadcastNativeBalanceState {
    match self_broadcast_native_balance_entry(snapshot, chain_id, public_account_uuid)
        .map(|entry| entry.amount)
    {
        Some(PublicBalanceAmount::Available(amount)) if amount.is_zero() => {
            SelfBroadcastNativeBalanceState::Zero
        }
        Some(PublicBalanceAmount::Available(_)) => SelfBroadcastNativeBalanceState::Positive,
        Some(PublicBalanceAmount::Unavailable) | None => SelfBroadcastNativeBalanceState::Unknown,
    }
}

pub(in crate::root) fn self_broadcast_native_balance_label(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
) -> String {
    self_broadcast_native_balance_entry(snapshot, chain_id, public_account_uuid).map_or_else(
        || "unavailable".to_string(),
        |entry| public_balance_amount_label(&entry.amount, entry.asset.decimals),
    )
}

#[cfg(test)]
pub(in crate::root) fn random_self_broadcast_gas_payer_uuid(
    accounts: &[PublicAccountMetadata],
    selected_uuid: Option<&str>,
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
) -> Option<Arc<str>> {
    random_self_broadcast_gas_payer_uuid_for_funding(
        accounts,
        selected_uuid,
        chain_id,
        snapshot,
        SelfBroadcastFundingMode::PublicBalance,
    )
}

pub(in crate::root) fn random_self_broadcast_gas_payer_uuid_for_funding(
    accounts: &[PublicAccountMetadata],
    selected_uuid: Option<&str>,
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
    funding: SelfBroadcastFundingMode,
) -> Option<Arc<str>> {
    let candidates = accounts
        .iter()
        .filter(|account| {
            Some(account.public_account_uuid.as_str()) != selected_uuid
                && (funding == SelfBroadcastFundingMode::PrivateSponsorship
                    || self_broadcast_native_balance_state(
                        snapshot,
                        chain_id,
                        &account.public_account_uuid,
                    ) != SelfBroadcastNativeBalanceState::Zero)
        })
        .collect::<Vec<_>>();
    candidates
        .choose(&mut rand::rng())
        .map(|account| Arc::from(account.public_account_uuid.as_str()))
}

pub(in crate::root) fn self_broadcast_initial_gas_values(
    selection: &SelfBroadcastGasFeeSelection,
    quote: Option<SelfBroadcastGasFeeQuote>,
) -> Option<(u128, u128)> {
    match *selection {
        SelfBroadcastGasFeeSelection::Auto => quote.map(|quote| {
            (
                quote.suggested_max_fee_per_gas,
                quote.suggested_max_priority_fee_per_gas,
            )
        }),
        SelfBroadcastGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } => Some((max_fee_per_gas, max_priority_fee_per_gas)),
    }
}

pub(in crate::root) fn self_broadcast_gas_payer_random_candidate(
    account: &PublicAccountMetadata,
    selected_uuid: Option<&str>,
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
) -> bool {
    Some(account.public_account_uuid.as_str()) != selected_uuid
        && self_broadcast_native_balance_state(snapshot, chain_id, &account.public_account_uuid)
            != SelfBroadcastNativeBalanceState::Zero
}

pub(in crate::root) fn normalized_self_broadcast_gas_payer_uuid(
    selected_uuid: Option<&Arc<str>>,
    accounts: &[PublicAccountMetadata],
) -> Option<Arc<str>> {
    selected_uuid
        .filter(|uuid| {
            accounts
                .iter()
                .any(|account| account.public_account_uuid.as_str() == uuid.as_ref())
        })
        .cloned()
        .or_else(|| default_self_broadcast_gas_payer_uuid(accounts))
}

pub(in crate::root) fn self_broadcast_gas_payer_select_items(
    accounts: &[PublicAccountMetadata],
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
) -> Vec<SelfBroadcastGasPayerSelectItem> {
    accounts
        .iter()
        .map(|account| SelfBroadcastGasPayerSelectItem {
            public_account_uuid: Arc::from(account.public_account_uuid.as_str()),
            label: Arc::from(self_broadcast_gas_payer_label(account)),
            address: account.address,
            chain_id,
            balance_label: Arc::from(self_broadcast_native_balance_label(
                snapshot,
                chain_id,
                &account.public_account_uuid,
            )),
        })
        .collect()
}

pub(in crate::root) fn self_broadcast_gas_payer_select_index(
    items: &[SelfBroadcastGasPayerSelectItem],
    selected_uuid: Option<&str>,
) -> Option<IndexPath> {
    let selected_uuid = selected_uuid?;
    items
        .iter()
        .position(|item| item.public_account_uuid.as_ref() == selected_uuid)
        .map(|index| IndexPath::default().row(index))
}

pub(in crate::root) fn sync_self_broadcast_gas_payer_select_entity(
    select: &Entity<SelectState<FullWidthSelectItems<SelfBroadcastGasPayerSelectItem>>>,
    accounts: &[PublicAccountMetadata],
    chain_id: u64,
    snapshot: Option<&PublicBalanceSnapshot>,
    selected_uuid: Option<&Arc<str>>,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    let items = self_broadcast_gas_payer_select_items(accounts, chain_id, snapshot);
    select.update(cx, |select, cx| {
        select.set_items(FullWidthSelectItems::new(items), window, cx);
        if let Some(uuid) = selected_uuid {
            select.set_selected_value(uuid, window, cx);
        } else {
            select.set_selected_index(None, window, cx);
        }
    });
}

pub(in crate::root) fn private_action_asset_select_items(
    assets: &[UnshieldAsset],
) -> Vec<PrivateActionAssetSelectItem> {
    assets
        .iter()
        .map(|asset| PrivateActionAssetSelectItem {
            token: asset.token,
            label: Arc::from(asset.label.as_str()),
            icon_path: asset.icon_path.clone(),
        })
        .collect()
}

pub(in crate::root) fn private_action_asset_select_index(
    items: &[PrivateActionAssetSelectItem],
    selected_token: Address,
) -> Option<IndexPath> {
    items
        .iter()
        .position(|item| item.token == selected_token)
        .map(|index| IndexPath::default().row(index))
}

pub(in crate::root) fn sync_private_action_asset_select_entity(
    select: &Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>,
    assets: &[UnshieldAsset],
    selected_token: Address,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    let items = private_action_asset_select_items(assets);
    let selected_index = private_action_asset_select_index(&items, selected_token);
    select.update(cx, |select, cx| {
        select.set_items(SearchableVec::new(items), window, cx);
        select.set_selected_index(selected_index, window, cx);
    });
}
