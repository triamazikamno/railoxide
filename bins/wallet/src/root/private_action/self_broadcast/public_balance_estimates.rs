use super::*;

impl WalletRoot {
    pub(super) fn start_public_balance_send_estimate(
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
        let Some(gas_limit_buffer) = self
            .effective_chain_configs
            .get(asset.chain_id)
            .map(|chain| chain.gas.gas_limit_buffer)
        else {
            return;
        };
        let utxos = session.unspent_utxos();
        let join = self.runtime.spawn_blocking(move || {
            estimate_desktop_send_self_broadcast_cost(
                asset.chain_id,
                gas_limit_buffer,
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

    pub(super) fn start_public_balance_unshield_estimate(
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
        let Some(chain) = self.effective_chain_configs.get(asset.chain_id) else {
            return;
        };
        let gas_limit_buffer = chain.gas.gas_limit_buffer;
        // Public-balance funding is never private sponsorship.
        let executor =
            wallet_ops::unshield_requires_executor(unwrap, native_top_up.is_some(), false);
        if executor
            && wallet_ops::unshield_executor_route(session.executor_owner().is_some(), chain)
                .is_err()
        {
            self.set_public_balance_estimate_if_current(
                DeliveryFormKind::Unshield,
                key,
                estimate_id,
                SponsoredFundingEstimateState::PublicBalanceUnavailable,
                cx,
            );
            return;
        }
        let join = self.runtime.spawn_blocking(move || {
            estimate_desktop_unshield_self_broadcast_cost(
                asset.chain_id,
                gas_limit_buffer,
                executor,
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

    pub(super) fn clear_sponsored_funding_estimate_if_current(
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
}
