use super::*;

impl WalletRoot {
    pub(super) fn start_sponsored_send_estimate(
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
        let Some(effective_chain) = self.effective_chain_configs.get(asset.chain_id).cloned()
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
        let wrapped_native_token = effective_chain.wrapped_native_token;
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

    pub(super) fn start_sponsored_unshield_estimate(
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
        let Some(effective_chain) = self.effective_chain_configs.get(asset.chain_id).cloned()
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
        let wrapped_native_token = effective_chain.wrapped_native_token;
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
}
