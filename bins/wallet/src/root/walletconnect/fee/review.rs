#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root::walletconnect) fn capture_walletconnect_reviewed_fee(
        &self,
        request: &WalletConnectRequestUi,
        cx: &Context<'_, Self>,
    ) -> Result<WalletConnectReviewedFeeProjection, String> {
        if !request.is_current() {
            return Err("Dapp approval is no longer current".to_owned());
        }
        let payload_fingerprint = walletconnect_request_payload_fingerprint(request)?;
        let selection = self.walletconnect.walletconnect_gas_fee.selection(cx)?;
        let state = self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .filter(|state| state.request_key.as_ref() == request.key);
        let state_projection = state.and_then(WalletConnectFeeState::authoritative_projection);
        let chain_id = parse_chain_id(&request.item.chain_id)?;
        let raw_gas_limit = walletconnect_request_raw_gas(request);
        let gas_limit_buffer = self
            .effective_chain_configs
            .get(chain_id)
            .map_or(0, |chain| chain.gas.gas_limit_buffer);
        let projection = state_projection.or_else(|| {
            let raw_gas_limit = raw_gas_limit?;
            let quote = self.walletconnect.walletconnect_gas_fee.quote;
            walletconnect_fee_projection(
                chain_id,
                raw_gas_limit,
                gas_limit_buffer,
                quote,
                selection,
                self.public_broadcaster_anchor_cache
                    .cached_native_usd_rate(chain_id),
                self.effective_chain_configs
                    .get(chain_id)?
                    .native_currency
                    .decimals,
            )
            .ok()
            .map(|(projection, _)| projection)
        });
        let basis = projection.map_or(WalletConnectReviewedFeeBasis::Unresolved, |projection| {
            match projection.source {
                PublicActionFeeSource::OperationTable => {
                    WalletConnectReviewedFeeBasis::OperationTable
                }
                PublicActionFeeSource::NetworkSimulation => {
                    WalletConnectReviewedFeeBasis::NetworkSimulation
                }
            }
        });
        let (selected_max_fee_per_gas, selected_max_priority_fee_per_gas) = projection.map_or_else(
            || match selection {
                PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                } => (Some(max_fee_per_gas), Some(max_priority_fee_per_gas)),
                PublicActionGasFeeSelection::Auto => (None, None),
            },
            |projection| {
                (
                    Some(projection.max_fee_per_gas),
                    Some(projection.max_priority_fee_per_gas),
                )
            },
        );
        Ok(WalletConnectReviewedFeeProjection {
            request_key: Arc::from(request.key.as_str()),
            review_token: request.review_token,
            payload_fingerprint,
            editor_generation: self.walletconnect.walletconnect_gas_fee.refresh_id,
            selection,
            selected_max_fee_per_gas,
            selected_max_priority_fee_per_gas,
            raw_gas_limit: projection
                .map(|projection| projection.raw_gas_limit)
                .or(raw_gas_limit),
            gas_limit: projection.map(|projection| projection.gas_limit),
            source: projection.map(|projection| projection.source),
            expected_gas_cost: projection.map(|projection| projection.expected_gas_cost),
            maximum_gas_cost: projection.map(|projection| projection.maximum_gas_cost),
            basis,
        })
    }

    pub(in crate::root::walletconnect) fn validate_walletconnect_reviewed_fee(
        &self,
        request: &WalletConnectRequestUi,
        reviewed: &WalletConnectReviewedFeeProjection,
        cx: &Context<'_, Self>,
    ) -> Result<(), String> {
        if reviewed.request_key.as_ref() != request.key
            || reviewed.review_token != request.review_token
            || !self.walletconnect.request_dialog_open
            || self.walletconnect.request_dialog_key.as_deref() != Some(request.key.as_str())
            || !request.approval_admitted(current_unix_seconds())
        {
            return Err(
                "WalletConnect fee review is stale; review the current request again.".to_owned(),
            );
        }
        let fingerprint = walletconnect_request_payload_fingerprint(request)?;
        if !walletconnect_reviewed_fee_request_context_matches(
            reviewed,
            request.key.as_str(),
            request.review_token,
            fingerprint,
        ) {
            return Err(
                "WalletConnect transaction changed after fee review; review it again.".to_owned(),
            );
        }
        let selection = self.walletconnect.walletconnect_gas_fee.selection(cx)?;
        if !walletconnect_reviewed_fee_editor_is_current(
            reviewed,
            self.walletconnect.walletconnect_gas_fee.refresh_id,
            selection,
        ) {
            return Err(
                "WalletConnect fee selection changed after review; review it again.".to_owned(),
            );
        }
        let Some(state) = self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .filter(|state| state.request_key.as_ref() == request.key)
        else {
            return Err("WalletConnect fee review is no longer available.".to_owned());
        };
        if state.review_token != request.review_token || state.payload_fingerprint != fingerprint {
            return Err("WalletConnect fee review is no longer current.".to_owned());
        }
        if state.editor_generation != self.walletconnect.walletconnect_gas_fee.refresh_id {
            return Err("WalletConnect fee review is no longer current.".to_owned());
        }
        if state.simulation_requested || matches!(state.status, WalletConnectFeeStatus::WouldRevert)
        {
            return Err(
                "WalletConnect simulation changed after review; review it again.".to_owned(),
            );
        }
        let current_projection = state.authoritative_projection();
        match reviewed.basis {
            WalletConnectReviewedFeeBasis::Unresolved => {
                if current_projection.is_some()
                    && matches!(
                        state.status,
                        WalletConnectFeeStatus::Simulated(_) | WalletConnectFeeStatus::WouldRevert
                    )
                {
                    return Err(
                        "WalletConnect simulation changed after review; review it again."
                            .to_owned(),
                    );
                }
            }
            WalletConnectReviewedFeeBasis::OperationTable => {
                if !matches!(
                    state.status,
                    WalletConnectFeeStatus::EstimatedFromOperation(_)
                ) || !current_projection
                    .is_some_and(|projection| projection_matches_review(projection, reviewed))
                {
                    return Err(
                        "WalletConnect fee estimate changed after review; review it again."
                            .to_owned(),
                    );
                }
            }
            WalletConnectReviewedFeeBasis::NetworkSimulation => {
                if !matches!(state.status, WalletConnectFeeStatus::Simulated(_))
                    || !current_projection
                        .is_some_and(|projection| projection_matches_review(projection, reviewed))
                {
                    return Err(
                        "WalletConnect simulation changed after review; review it again."
                            .to_owned(),
                    );
                }
            }
        }
        Ok(())
    }

    pub(in crate::root::walletconnect) fn discard_walletconnect_fee_for_request_replacement(
        &mut self,
    ) {
        self.walletconnect.walletconnect_fee_quote_task = None;
        self.walletconnect.walletconnect_fee_state = None;
        self.walletconnect.walletconnect_gas_fee.refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        self.walletconnect.walletconnect_gas_fee.quote = None;
        self.walletconnect.walletconnect_gas_fee.refreshing = false;
        self.walletconnect.walletconnect_gas_fee.quote_error = None;
        self.walletconnect.walletconnect_fee_request_generation = self
            .walletconnect
            .walletconnect_fee_request_generation
            .wrapping_add(1);
    }

    pub(in crate::root::walletconnect) fn attach_walletconnect_fee_state(
        &mut self,
        request_key: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key)
            .cloned()
        else {
            self.discard_walletconnect_fee_state(window, cx);
            return;
        };
        if !walletconnect_request_fee_eligible(&request.parsed) {
            self.discard_walletconnect_fee_state(window, cx);
            return;
        }
        // Debug UI fixture: the canned request's chain isn't activated, and its fee is a fixed
        // estimate in place of a quote.
        #[cfg(debug_assertions)]
        if crate::root::walletconnect::ui_fixture::is_fixture_request(&request) {
            self.attach_ui_fixture_walletconnect_fee_state(&request, window, cx);
            return;
        }
        let Ok(payload_fingerprint) = walletconnect_request_payload_fingerprint(&request) else {
            self.discard_walletconnect_fee_state(window, cx);
            return;
        };
        if let Ok(chain_id) = parse_chain_id(&request.item.chain_id) {
            self.activate_chain_background(chain_id);
        }
        if self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .is_some_and(|state| {
                state.request_key.as_ref() == request_key
                    && state.review_token == request.review_token
                    && state.payload_fingerprint == payload_fingerprint
            })
        {
            return;
        }

        self.walletconnect_fee_lifecycle_reset(window, cx);
        self.walletconnect.walletconnect_fee_request_generation = self
            .walletconnect
            .walletconnect_fee_request_generation
            .wrapping_add(1);
        let request_generation = self.walletconnect.walletconnect_fee_request_generation;
        let request_key_arc = Arc::<str>::from(request_key);
        let editor_generation = self.walletconnect.walletconnect_gas_fee.refresh_id;
        self.walletconnect.walletconnect_fee_state = Some(WalletConnectFeeState::new(
            &request,
            payload_fingerprint,
            request_generation,
            self.walletconnect.request_dialog_refresh_generation,
            editor_generation,
        ));
        self.refresh_walletconnect_gas_fee_quote(request_key_arc, cx);
    }

    /// Debug UI fixture: attach the canned request's fee state as `attach_walletconnect_fee_state`
    /// does, without activating its chain, and show its fixed estimate in place of a quote.
    #[cfg(debug_assertions)]
    fn attach_ui_fixture_walletconnect_fee_state(
        &mut self,
        request: &WalletConnectRequestUi,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // The canned transaction is well formed, so this always has a fingerprint.
        let Ok(payload_fingerprint) = walletconnect_request_payload_fingerprint(request) else {
            return;
        };
        if self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .is_some_and(|state| {
                state.request_key.as_ref() == request.key
                    && state.review_token == request.review_token
                    && state.payload_fingerprint == payload_fingerprint
            })
        {
            return;
        }
        self.walletconnect_fee_lifecycle_reset(window, cx);
        self.walletconnect.walletconnect_fee_request_generation = self
            .walletconnect
            .walletconnect_fee_request_generation
            .wrapping_add(1);
        self.walletconnect.walletconnect_fee_state = Some(WalletConnectFeeState::new(
            request,
            payload_fingerprint,
            self.walletconnect.walletconnect_fee_request_generation,
            self.walletconnect.request_dialog_refresh_generation,
            self.walletconnect.walletconnect_gas_fee.refresh_id,
        ));
        self.install_ui_fixture_walletconnect_fee(cx);
    }

    /// Debug UI fixture: apply the canned request's fixed quote as a successful refresh does,
    /// without asking the network.
    #[cfg(debug_assertions)]
    pub(super) fn install_ui_fixture_walletconnect_fee(&mut self, cx: &Context<'_, Self>) {
        let gas_fee = &mut self.walletconnect.walletconnect_gas_fee;
        gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
        gas_fee.quote = Some(crate::root::walletconnect::ui_fixture::FEE_QUOTE);
        gas_fee.quote_error = None;
        self.walletconnect_fee_state_changed_by_editor(cx);
    }

    pub(in crate::root::walletconnect) fn discard_walletconnect_fee_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.walletconnect_fee_lifecycle_reset(window, cx);
        self.walletconnect.walletconnect_fee_state = None;
        self.walletconnect.walletconnect_fee_request_generation = self
            .walletconnect
            .walletconnect_fee_request_generation
            .wrapping_add(1);
    }

    fn walletconnect_fee_lifecycle_reset(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.walletconnect.walletconnect_fee_quote_task = None;
        self.walletconnect
            .walletconnect_gas_fee
            .reset_for_request(window, cx);
        if let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() {
            state.invalidate();
        }
    }
}
