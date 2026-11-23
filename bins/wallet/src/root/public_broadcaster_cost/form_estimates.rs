#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(super) fn estimate_send_public_broadcaster_cost_from_form(
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
            shown_broadcaster: form.shown_broadcaster.clone(),
            refused_broadcasters: form.refused_broadcasters.clone(),
            input_pin: form.input_pin.clone(),
            executor_operation: None,
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
                self.set_send_form_error(key, error.to_string(), cx);
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
                form.input_pin_conflict = input_pin_conflict(&result);
                let picker_context = match result {
                    Ok(estimate) => {
                        let context = BroadcasterPickerFeeEstimateContext::from(&estimate);
                        form.error = None;
                        if form.broadcaster_choice == BroadcasterChoice::Random {
                            form.shown_broadcaster =
                                Some(estimate.broadcaster.railgun_address.clone());
                        }
                        form.latest_estimate = Some(estimate.clone());
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

    pub(super) fn estimate_unshield_public_broadcaster_cost_from_form(
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
            shown_broadcaster: form.shown_broadcaster.clone(),
            refused_broadcasters: form.refused_broadcasters.clone(),
            input_pin: form.input_pin.clone(),
            executor_operation: form.executor_operation.clone(),
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
                self.set_unshield_form_error(key, error.to_string(), cx);
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
                form.input_pin_conflict = input_pin_conflict(&result);
                let picker_context = match result {
                    Ok(estimate) => {
                        let context = BroadcasterPickerFeeEstimateContext::from(&estimate);
                        form.error = None;
                        if form.broadcaster_choice == BroadcasterChoice::Random {
                            form.shown_broadcaster =
                                Some(estimate.broadcaster.railgun_address.clone());
                        }
                        form.latest_estimate = Some(estimate.clone());
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

/// Whether an estimate failed because the form's pinned notes can't make this attempt.
fn input_pin_conflict(result: &eyre::Result<PublicBroadcasterCostEstimate>) -> bool {
    result.as_ref().is_err_and(|error| {
        error
            .downcast_ref::<wallet_ops::PublicBroadcasterInputPinUnavailable>()
            .is_some()
    })
}
