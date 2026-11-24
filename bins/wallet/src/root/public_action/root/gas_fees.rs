use super::*;

impl WalletRoot {
    pub(super) fn reset_public_action_gas_fee_quotes(&mut self) {
        for gas_fee in [
            &mut self.public_form.send_gas_fee,
            &mut self.public_form.shield_gas_fee,
            &mut self.public_form.wrap_gas_fee,
            &mut self.public_form.unwrap_gas_fee,
        ] {
            gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
            gas_fee.quote = None;
            gas_fee.refreshing = false;
            gas_fee.error = None;
            gas_fee.quote_error = None;
        }
        self.public_form.shield_gas_fee_authorization_ceiling = None;
        self.public_form.shield_gas_fee_route = None;
    }

    pub(in crate::root) fn invalidate_public_action_gas_fee_quote(
        &mut self,
        action_mode: PublicActionMode,
    ) {
        let gas_fee = self.public_form.gas_fee_mut(action_mode);
        gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
        gas_fee.refreshing = false;
        gas_fee.quote = None;
        gas_fee.quote_error = None;
        if action_mode == PublicActionMode::Shield {
            self.public_form.shield_gas_fee_authorization_ceiling = None;
            self.public_form.shield_gas_fee_route = None;
        }
    }

    /// The asset and profile a quote for `mode` is requested and admitted for.
    pub(in crate::root) fn public_action_quote_route(
        &self,
        mode: PublicActionMode,
    ) -> PublicShieldRoute {
        let asset = self
            .public_form
            .selected_asset
            .unwrap_or(PublicAssetId::Native);
        let profile = if mode == PublicActionMode::Shield && self.mimic_railway_shield_effective() {
            PublicShieldTransactionProfile::Railway
        } else {
            PublicShieldTransactionProfile::Railoxide
        };
        (asset, profile)
    }

    /// The quote for `mode`; a Shield quote only when admitted for the current route.
    pub(in crate::root) fn public_action_quote(
        &self,
        mode: PublicActionMode,
    ) -> Option<PublicActionGasFeeQuote> {
        match mode {
            PublicActionMode::Shield => public_shield_quote_for_route(
                self.public_form.shield_gas_fee.quote,
                self.public_form.shield_gas_fee_route,
                self.public_action_quote_route(mode),
            ),
            PublicActionMode::Send | PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                self.public_form.gas_fee(mode).quote
            }
        }
    }

    pub(in crate::root) fn set_public_action_gas_fee_mode(
        &mut self,
        action_mode: PublicActionMode,
        mode: Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let submitting = self.public_form.sending || self.public_form.shielding;
        let gas_fee = self.public_form.gas_fee_mut(action_mode);
        if submitting || gas_fee.mode == mode {
            return;
        }
        if mode == Eip1559GasFeeMode::Custom {
            gas_fee.seed_custom_from_auto_if_empty(window, cx);
        }
        gas_fee.mode = mode;
        if action_mode == PublicActionMode::Send {
            self.invalidate_advanced_public_send_estimate();
        }
        self.set_public_action_error(action_mode, None);
        cx.notify();
    }

    pub(in crate::root) fn customize_public_action_gas_fee_from_auto(
        &mut self,
        action_mode: PublicActionMode,
        target: Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.sending || self.public_form.shielding {
            return;
        }
        let gas_fee = self.public_form.gas_fee_mut(action_mode);
        if !gas_fee.overwrite_custom_from_auto(window, cx) {
            return;
        }
        let focus_input = match target {
            Eip1559GasFeeEditTarget::MaxFee => gas_fee.max_fee_input.clone(),
            Eip1559GasFeeEditTarget::MaxTip => gas_fee.max_priority_fee_input.clone(),
        };
        gas_fee.mode = Eip1559GasFeeMode::Custom;
        if action_mode == PublicActionMode::Send {
            self.invalidate_advanced_public_send_estimate();
        }
        self.set_public_action_error(action_mode, None);
        focus_input.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(in crate::root) fn refresh_public_action_gas_fee_quote(
        &mut self,
        action_mode: PublicActionMode,
        cx: &mut Context<'_, Self>,
    ) {
        #[cfg(debug_assertions)]
        if public_shield_ui_fixture().is_some() {
            return;
        }
        let submitting = self.public_form.sending || self.public_form.shielding;
        let gas_fee = self.public_form.gas_fee_mut(action_mode);
        if submitting || gas_fee.refreshing {
            return;
        }
        gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
        gas_fee.refreshing = true;
        gas_fee.quote_error = None;
        let refresh_id = gas_fee.refresh_id;
        let chain_id = self.last_evm_chain;
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        let route = self.public_action_quote_route(action_mode);
        let (asset, profile) = route;
        cx.spawn(async move |this, cx| {
            let result = quote_public_action_gas_fee_bundle_with_profile(
                chain_id,
                &effective_chain,
                profile,
                asset,
                &http,
            )
            .await;
            let _ = this.update(cx, |root, cx| {
                if root.public_form.gas_fee(action_mode).refresh_id != refresh_id {
                    return;
                }
                // A quote for an earlier asset or profile never serves the current route.
                if root.public_action_quote_route(action_mode) != route {
                    root.public_form.gas_fee_mut(action_mode).refreshing = false;
                    return;
                }
                match result {
                    Ok(bundle) => {
                        let standard = bundle.standard;
                        let authorization_ceiling = bundle.authorization_ceiling;
                        let gas_fee = root.public_form.gas_fee_mut(action_mode);
                        gas_fee.refreshing = false;
                        gas_fee.quote = Some(standard);
                        gas_fee.quote_error = None;
                        if action_mode == PublicActionMode::Shield {
                            root.public_form.shield_gas_fee_authorization_ceiling =
                                Some(authorization_ceiling);
                            root.public_form.shield_gas_fee_route = Some(route);
                        }
                        if action_mode == PublicActionMode::Send {
                            root.invalidate_advanced_public_send_estimate();
                        }
                        root.set_public_action_error(action_mode, None);
                    }
                    Err(_error) => {
                        let gas_fee = root.public_form.gas_fee_mut(action_mode);
                        let error = if gas_fee.quote.is_some() {
                            "Gas quote refresh failed; using the last successful quote."
                        } else {
                            "Gas quote is unavailable."
                        };
                        gas_fee.refreshing = false;
                        gas_fee.quote_error = Some(Arc::from(error));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root) fn public_action_gas_fee_selection(
        &self,
        mode: PublicActionMode,
        cx: &Context<'_, Self>,
    ) -> Result<PublicActionGasFeeSelection, String> {
        self.public_form.gas_fee(mode).selection(cx)
    }

    pub(in crate::root) fn public_action_authorized_gas_fee_selection(
        &self,
        mode: PublicActionMode,
        profile: PublicShieldTransactionProfile,
        asset: PublicAssetId,
        cx: &App,
    ) -> Result<PublicActionGasFeeSelection, String> {
        let selection = self.public_form.gas_fee(mode).selection(cx)?;
        let quote = self.public_action_quote(mode);
        authorized_public_action_gas_fee_selection(
            selection,
            quote,
            profile,
            asset,
            self.last_evm_chain,
        )
    }

    pub(in crate::root) fn public_action_fee_display(
        &self,
        mode: PublicActionMode,
        amount: Option<U256>,
        cx: &App,
    ) -> PublicActionFeeDisplay {
        let Some(asset) = self.public_form.selected_asset else {
            return PublicActionFeeDisplay {
                gas_limit: None,
                expected_gas_cost: None,
                maximum_gas_cost: None,
                show_maximum_gas_cost: false,
                protocol_fee: None,
            };
        };
        let native_shield_gas_limit = self
            .native_shield_stealth_applies(mode)
            .then(|| self.native_shield_gas(cx))
            .flatten()
            .map(|(_, gas_limit)| gas_limit);
        PublicActionFeeDisplay::from_estimate(
            self.effective_chain_configs.get(self.last_evm_chain),
            self.public_action_estimated_gas_cost(mode, asset, cx),
            native_shield_gas_limit,
            (mode == PublicActionMode::Shield)
                .then(|| amount.map(|amount| (asset, public_shield_protocol_fee_amount(amount))))
                .flatten(),
            &self.effective_token_registry,
            &self.public_broadcaster_anchor_cache,
        )
    }

    /// The per-transaction gas limits `mode`'s maximum gas cost is priced at, from the same chain
    /// settings and profile, which its submission signs within.
    pub(super) fn public_form_reviewed_gas_limits(
        &self,
        mode: PublicActionMode,
        asset: PublicAssetId,
    ) -> Option<(u64, u64, u64)> {
        let (_, profile) = self.public_action_quote_route(mode);
        public_action_reviewed_gas_limits(
            self.last_evm_chain,
            public_action_kind(mode),
            asset,
            profile,
            self.effective_chain_configs
                .get(self.last_evm_chain)?
                .gas
                .gas_limit_buffer,
        )
    }

    pub(super) fn public_action_estimated_gas_cost(
        &self,
        mode: PublicActionMode,
        asset: PublicAssetId,
        cx: &App,
    ) -> Option<Eip1559GasCostProjection> {
        let kind = public_action_kind(mode);
        let gas_fee = self.public_form.gas_fee(mode);
        let quote = self.public_action_quote(mode);
        if self.native_shield_stealth_applies(mode) {
            let (raw_gas_limit, gas_limit) = self.native_shield_gas(cx)?;
            return native_shield_gas_cost_projection(
                self.last_evm_chain,
                self.public_shield_profile(),
                raw_gas_limit,
                gas_limit,
                gas_fee.selection(cx).ok()?,
                quote,
            );
        }
        let (_, profile) = self.public_action_quote_route(mode);
        let gas_fee_mode = match gas_fee.mode {
            Eip1559GasFeeMode::Auto => PublicActionGasFeeMode::Auto,
            Eip1559GasFeeMode::Custom => PublicActionGasFeeMode::Custom,
        };
        let authorization_ceiling =
            public_action_uses_railway_authorization_ceiling(mode, profile, asset, gas_fee_mode)
                .then_some(self.public_form.shield_gas_fee_authorization_ceiling)
                .flatten();
        estimate_public_action_gas_cost_with_profile_and_ceiling(
            self.last_evm_chain,
            self.effective_chain_configs.get(self.last_evm_chain)?,
            kind,
            asset,
            profile,
            gas_fee.selection(cx).ok()?,
            quote,
            authorization_ceiling,
        )
        .ok()
    }

    /// The Unwrap gas guard for the selected account at the current quote.
    pub(super) fn selected_public_unwrap_gas_error(
        &self,
        asset: PublicAssetId,
        cx: &App,
    ) -> Option<String> {
        let public_account_uuid = self.public_form.selected_account_uuid.as_deref()?;
        let status = self
            .public_account_for_uuid(Some(public_account_uuid))?
            .status;
        let native_balance = self
            .public_balance_entry(public_account_uuid, PublicAssetId::Native, status)
            .and_then(|entry| entry.amount.amount());
        let maximum_gas_cost = self
            .public_action_estimated_gas_cost(PublicActionMode::Unwrap, asset, cx)?
            .maximum_cost;
        public_unwrap_gas_error(native_balance, maximum_gas_cost, |cost| {
            self.configured_native_amount_label(self.last_evm_chain, cost)
        })
    }

    pub(in crate::root) fn public_action_initial_gas_values(
        &self,
        mode: PublicActionMode,
        selection: &PublicActionGasFeeSelection,
    ) -> Option<(u128, u128)> {
        match selection {
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } => Some((*max_fee_per_gas, *max_priority_fee_per_gas)),
            PublicActionGasFeeSelection::Auto => {
                let quote = self.public_action_quote(mode)?;
                Some((
                    quote.suggested_max_fee_per_gas,
                    quote.suggested_max_priority_fee_per_gas,
                ))
            }
        }
    }
}

const fn public_action_kind(mode: PublicActionMode) -> PublicActionKind {
    match mode {
        PublicActionMode::Shield => PublicActionKind::Shield,
        PublicActionMode::Send => PublicActionKind::Send,
        PublicActionMode::Wrap => PublicActionKind::Wrap,
        PublicActionMode::Unwrap => PublicActionKind::Unwrap,
    }
}
