use super::*;

impl WalletRoot {
    pub(in crate::root) fn set_self_broadcast_gas_payer(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        public_account_uuid: Option<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let payer = public_account_uuid.clone();
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.self_broadcast_gas_payer_uuid == public_account_uuid {
                    return false;
                }
                form.self_broadcast_gas_payer_uuid = public_account_uuid;
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.self_broadcast_gas_payer_uuid == public_account_uuid {
                    return false;
                }
                form.self_broadcast_gas_payer_uuid = public_account_uuid;
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
        };
        if changed {
            self.sync_self_broadcast_gas_payer_select(kind, key, window, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
            self.schedule_account_public_balance_refresh(
                key.chain_id,
                payer.as_deref(),
                Some(window.window_handle()),
                cx,
            );
        }
    }

    pub(in crate::root) fn choose_random_self_broadcast_gas_payer(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.active_self_broadcast_gas_payer_accounts();
        let (selected_uuid, funding) = match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).map_or(
                (None, SelfBroadcastFundingMode::PublicBalance),
                |form| {
                    (
                        form.self_broadcast_gas_payer_uuid.clone(),
                        form.self_broadcast_funding,
                    )
                },
            ),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).map_or(
                (None, SelfBroadcastFundingMode::PublicBalance),
                |form| {
                    (
                        form.self_broadcast_gas_payer_uuid.clone(),
                        form.self_broadcast_funding,
                    )
                },
            ),
        };
        let Some(account_uuid) = random_self_broadcast_gas_payer_uuid_for_funding(
            &accounts,
            selected_uuid.as_deref(),
            key.chain_id,
            self.public_balance_snapshot.as_deref(),
            funding,
        ) else {
            return;
        };
        self.set_self_broadcast_gas_payer(kind, key, Some(account_uuid), window, cx);
    }

    pub(in crate::root) fn set_self_broadcast_gas_fee_mode(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        mode: Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.self_broadcast_gas_fee.mode == mode {
                    return false;
                }
                if mode == Eip1559GasFeeMode::Custom {
                    form.self_broadcast_gas_fee
                        .seed_custom_from_auto_if_empty(window, cx);
                }
                form.self_broadcast_gas_fee.mode = mode;
                if mode == Eip1559GasFeeMode::Auto {
                    form.self_broadcast_gas_fee.error = None;
                }
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.self_broadcast_gas_fee.mode == mode {
                    return false;
                }
                if mode == Eip1559GasFeeMode::Custom {
                    form.self_broadcast_gas_fee
                        .seed_custom_from_auto_if_empty(window, cx);
                }
                form.self_broadcast_gas_fee.mode = mode;
                if mode == Eip1559GasFeeMode::Auto {
                    form.self_broadcast_gas_fee.error = None;
                }
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
        };
        if changed {
            self.sync_self_broadcast_custom_gas_fee_error(kind, key, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(super) fn sync_self_broadcast_custom_gas_fee_error(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &App,
    ) {
        let error = match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).and_then(|form| {
                (form.self_broadcast_gas_fee.mode == Eip1559GasFeeMode::Custom)
                    .then(|| form.self_broadcast_gas_fee.selection(cx).err())
                    .flatten()
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).and_then(|form| {
                (form.self_broadcast_gas_fee.mode == Eip1559GasFeeMode::Custom)
                    .then(|| form.self_broadcast_gas_fee.selection(cx).err())
                    .flatten()
            }),
        }
        .map(Arc::<str>::from);
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && form.self_broadcast_gas_fee.mode == Eip1559GasFeeMode::Custom
                {
                    form.self_broadcast_gas_fee.error = error;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && form.self_broadcast_gas_fee.mode == Eip1559GasFeeMode::Custom
                {
                    form.self_broadcast_gas_fee.error = error;
                }
            }
        }
    }

    pub(in crate::root) fn customize_self_broadcast_gas_fee_from_auto(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        target: Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mut focus_input: Option<Entity<InputState>> = None;
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating
                    || !form
                        .self_broadcast_gas_fee
                        .overwrite_custom_from_auto(window, cx)
                {
                    return false;
                }
                focus_input = Some(match target {
                    Eip1559GasFeeEditTarget::MaxFee => {
                        form.self_broadcast_gas_fee.max_fee_input.clone()
                    }
                    Eip1559GasFeeEditTarget::MaxTip => {
                        form.self_broadcast_gas_fee.max_priority_fee_input.clone()
                    }
                });
                form.self_broadcast_gas_fee.mode = Eip1559GasFeeMode::Custom;
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating
                    || !form
                        .self_broadcast_gas_fee
                        .overwrite_custom_from_auto(window, cx)
                {
                    return false;
                }
                focus_input = Some(match target {
                    Eip1559GasFeeEditTarget::MaxFee => {
                        form.self_broadcast_gas_fee.max_fee_input.clone()
                    }
                    Eip1559GasFeeEditTarget::MaxTip => {
                        form.self_broadcast_gas_fee.max_priority_fee_input.clone()
                    }
                });
                form.self_broadcast_gas_fee.mode = Eip1559GasFeeMode::Custom;
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
        };
        if changed {
            if let Some(input) = focus_input {
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
            self.sync_self_broadcast_custom_gas_fee_error(kind, key, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn refresh_self_broadcast_gas_fee_quote(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let chain_id = key.chain_id;
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let refresh_id = match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get_mut(&key) else {
                    return;
                };
                if form.generating || form.self_broadcast_gas_fee.refreshing {
                    return;
                }
                form.self_broadcast_gas_fee.refresh_id =
                    form.self_broadcast_gas_fee.refresh_id.wrapping_add(1);
                form.self_broadcast_gas_fee.refreshing = true;
                form.self_broadcast_gas_fee.quote_error = None;
                form.self_broadcast_gas_fee.refresh_id
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get_mut(&key) else {
                    return;
                };
                if form.generating || form.self_broadcast_gas_fee.refreshing {
                    return;
                }
                form.self_broadcast_gas_fee.refresh_id =
                    form.self_broadcast_gas_fee.refresh_id.wrapping_add(1);
                form.self_broadcast_gas_fee.refreshing = true;
                form.self_broadcast_gas_fee.quote_error = None;
                form.self_broadcast_gas_fee.refresh_id
            }
        };
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let result =
                quote_desktop_self_broadcast_gas_fee(chain_id, &effective_chain, &http).await;
            let _ = this.update(cx, |root, cx| {
                let gas_fee = match kind {
                    DeliveryFormKind::Send => root
                        .send_forms
                        .get_mut(&key)
                        .map(|form| &mut form.self_broadcast_gas_fee),
                    DeliveryFormKind::Unshield => root
                        .unshield_forms
                        .get_mut(&key)
                        .map(|form| &mut form.self_broadcast_gas_fee),
                };
                let Some(gas_fee) = gas_fee else {
                    return;
                };
                if gas_fee.refresh_id != refresh_id {
                    return;
                }
                gas_fee.refreshing = false;
                let quote_updated = match result {
                    Ok(quote) => {
                        gas_fee.quote = Some(quote);
                        gas_fee.quote_error = None;
                        true
                    }
                    Err(_error) => {
                        gas_fee.quote_error = Some(Arc::from(if gas_fee.quote.is_some() {
                            "Gas quote refresh failed; using the last successful quote."
                        } else {
                            "Gas quote is unavailable."
                        }));
                        false
                    }
                };
                root.sync_self_broadcast_custom_gas_fee_error(kind, key, cx);
                if quote_updated {
                    root.revalidate_sponsored_funding_estimate(kind, key, cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }
}
