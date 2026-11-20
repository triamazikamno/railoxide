#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn set_walletconnect_gas_fee_mode(
        &mut self,
        request_key: &Arc<str>,
        mode: crate::root::gas_fee::Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.walletconnect.request_dialog_open
            || self.walletconnect.request_dialog_key.as_deref() != Some(request_key.as_ref())
            || self
                .walletconnect
                .walletconnect_fee_state
                .as_ref()
                .is_none_or(|state| state.request_key.as_ref() != request_key.as_ref())
        {
            return;
        }
        if self.walletconnect.walletconnect_gas_fee.mode == mode {
            return;
        }
        if mode == crate::root::gas_fee::Eip1559GasFeeMode::Custom {
            self.walletconnect
                .walletconnect_gas_fee
                .seed_custom_from_auto_if_empty(window, cx);
        }
        self.walletconnect.walletconnect_gas_fee.mode = mode;
        self.walletconnect.walletconnect_gas_fee.refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        self.walletconnect_fee_state_changed_by_editor(cx);
        cx.notify();
    }

    pub(in crate::root) fn customize_walletconnect_gas_fee_from_auto(
        &mut self,
        request_key: &Arc<str>,
        target: crate::root::gas_fee::Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .is_none_or(|state| state.request_key.as_ref() != request_key.as_ref())
        {
            return;
        }
        if !self
            .walletconnect
            .walletconnect_gas_fee
            .overwrite_custom_from_auto(window, cx)
        {
            return;
        }
        self.walletconnect.walletconnect_gas_fee.mode =
            crate::root::gas_fee::Eip1559GasFeeMode::Custom;
        self.walletconnect.walletconnect_gas_fee.refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        let input = match target {
            crate::root::gas_fee::Eip1559GasFeeEditTarget::MaxFee => self
                .walletconnect
                .walletconnect_gas_fee
                .max_fee_input
                .clone(),
            crate::root::gas_fee::Eip1559GasFeeEditTarget::MaxTip => self
                .walletconnect
                .walletconnect_gas_fee
                .max_priority_fee_input
                .clone(),
        };
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.walletconnect_fee_state_changed_by_editor(cx);
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn walletconnect_fee_editor_changed(
        &mut self,
        cx: &Context<'_, Self>,
    ) {
        self.walletconnect.walletconnect_gas_fee.refreshing = false;
        self.walletconnect.walletconnect_gas_fee.refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        self.walletconnect_fee_state_changed_by_editor(cx);
    }

    pub(in crate::root) fn refresh_walletconnect_fee_usd_values(&mut self) {
        let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() else {
            return;
        };
        let chain_id = self
            .walletconnect
            .pending_requests
            .get(state.request_key.as_ref())
            .and_then(|request| parse_chain_id(&request.item.chain_id).ok());
        let revalue = |projection: &mut PublicActionFeeProjection| {
            projection.expected_native_usd_micro_value = chain_id.and_then(|id| {
                self.public_broadcaster_anchor_cache
                    .cached_native_usd_micro_value(id, projection.expected_gas_cost)
            });
            projection.maximum_native_usd_micro_value = chain_id.and_then(|id| {
                self.public_broadcaster_anchor_cache
                    .cached_native_usd_micro_value(id, projection.maximum_gas_cost)
            });
        };
        if let Some(projection) = &mut state.last_successful_display_projection {
            revalue(projection);
        }
        match &mut state.status {
            WalletConnectFeeStatus::EstimatedFromOperation(projection)
            | WalletConnectFeeStatus::Simulated(projection) => revalue(projection),
            _ => {}
        }
    }

    pub(super) fn walletconnect_fee_state_changed_by_editor(&mut self, cx: &Context<'_, Self>) {
        self.walletconnect.walletconnect_fee_quote_task = None;
        self.walletconnect.walletconnect_gas_fee.refreshing = false;
        let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() else {
            return;
        };
        state.invalidate();
        state.editor_generation = self.walletconnect.walletconnect_gas_fee.refresh_id;
        state.error = None;
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(state.request_key.as_ref())
            .cloned()
        else {
            return;
        };
        let Some(raw_gas) = walletconnect_request_raw_gas(&request) else {
            state.status = WalletConnectFeeStatus::AwaitingSimulation;
            return;
        };
        let Ok(selection) = self.walletconnect.walletconnect_gas_fee.selection(cx) else {
            state.status = WalletConnectFeeStatus::UnavailableFailed;
            return;
        };
        let chain_id = match parse_chain_id(&request.item.chain_id) {
            Ok(chain_id) => chain_id,
            Err(error) => {
                state.status = WalletConnectFeeStatus::UnavailableFailed;
                state.error = Some(Arc::from(error));
                return;
            }
        };
        let gas_limit_buffer = self
            .effective_chain_configs
            .get(chain_id)
            .map_or(0, |chain| chain.gas.gas_limit_buffer);
        match walletconnect_fee_projection(
            chain_id,
            raw_gas,
            gas_limit_buffer,
            self.walletconnect.walletconnect_gas_fee.quote,
            selection,
            self.public_broadcaster_anchor_cache
                .cached_native_usd_rate(chain_id),
            self.effective_chain_configs
                .get(chain_id)
                .map_or(18, |chain| chain.native_currency.decimals),
        ) {
            Ok((projection, _)) => {
                state.apply_successful_operation_projection(projection);
            }
            Err(error) => {
                state.status = WalletConnectFeeStatus::UnavailableFailed;
                state.error = Some(Arc::from(error));
            }
        }
    }
}
