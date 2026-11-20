#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn consume_programmatic_amount_input_change(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &Context<'_, Self>,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                let Some(expected) = form.pending_programmatic_amount_input.take() else {
                    return false;
                };
                form.amount_input.read(cx).value().as_ref() == expected
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                let Some(expected) = form.pending_programmatic_amount_input.take() else {
                    return false;
                };
                form.amount_input.read(cx).value().as_ref() == expected
            }),
        }
    }

    pub(in crate::root) fn set_private_action_amount_to_max(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let amount = match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get(&key) else {
                    return;
                };
                send_form_max_entered_amount(form, form.delivery_mode, form.fee_mode)
                    .unwrap_or(form.asset.max_batched)
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get(&key) else {
                    return;
                };
                unshield_max_fill_amount(
                    unshield_form_max_entered_amount(form, form.delivery_mode, form.fee_mode),
                    form.asset.max_batched,
                    form.fee_mode,
                )
            }
        };
        let changed = self.set_programmatic_amount_input(kind, key, amount, window, cx);
        if changed {
            if kind == DeliveryFormKind::Unshield {
                self.refresh_unshield_native_top_up_state(key, cx);
            }
            self.schedule_public_broadcaster_cost_estimate(kind, key, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn set_programmatic_amount_input(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        amount: U256,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating {
                    return false;
                }
                let value = format_send_amount_input(amount, form.asset.decimals);
                form.pending_programmatic_amount_input = Some(value.clone());
                form.error = None;
                form.result = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                set_programmatic_amount_input_value(&form.amount_input, value, window, cx);
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating {
                    return false;
                }
                let value = format_unshield_amount_input(amount, form.asset.decimals);
                form.pending_programmatic_amount_input = Some(value.clone());
                form.error = None;
                form.result = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                set_programmatic_amount_input_value(&form.amount_input, value, window, cx);
                true
            }),
        }
    }

    pub(in crate::root) fn set_private_action_asset(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => self.set_send_asset(key, asset, window, cx),
            DeliveryFormKind::Unshield => self.set_unshield_asset(key, asset, window, cx),
        }
    }

    pub(in crate::root) fn set_private_action_asset_token(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        token: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(asset) = self
            .private_action_asset_options(kind, key.chain_id)
            .into_iter()
            .find(|asset| asset.token == token)
        else {
            return;
        };
        self.set_private_action_asset(kind, key, asset, window, cx);
    }
}
