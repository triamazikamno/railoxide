#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn refresh_walletconnect_gas_fee_quote(
        &mut self,
        request_key: Arc<str>,
        cx: &Context<'_, Self>,
    ) {
        if self
            .walletconnect
            .request_actions
            .contains(request_key.as_ref())
            || !self.walletconnect.request_dialog_open
            || self.walletconnect.request_dialog_key.as_deref() != Some(request_key.as_ref())
            || self
                .walletconnect
                .walletconnect_fee_state
                .as_ref()
                .is_none_or(|state| state.request_key.as_ref() != request_key.as_ref())
        {
            return;
        }
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key.as_ref())
            .cloned()
        else {
            return;
        };
        // Debug UI fixture: Refresh shows the canned request's fixed estimate again.
        #[cfg(debug_assertions)]
        if crate::root::walletconnect::ui_fixture::is_fixture_request(&request) {
            self.install_ui_fixture_walletconnect_fee(cx);
            return;
        }
        let Ok(chain_id) = parse_chain_id(&request.item.chain_id) else {
            return;
        };
        if self.walletconnect.walletconnect_gas_fee.refreshing {
            return;
        }
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        if !request.is_current() {
            return;
        }
        let expiry_timestamp = request.timeout_timestamp();
        let http = self.http.clone();
        let network_mode = http.network_mode();
        let refresh_id = self
            .walletconnect
            .walletconnect_gas_fee
            .refresh_id
            .wrapping_add(1);
        self.walletconnect.walletconnect_gas_fee.refresh_id = refresh_id;
        self.walletconnect.walletconnect_gas_fee.refreshing = true;
        self.walletconnect.walletconnect_gas_fee.quote_error = None;
        let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() else {
            return;
        };
        state.begin_attempt(refresh_id);
        state.status = WalletConnectFeeStatus::Fetching;
        state.error = None;
        let guard = state.completion_guard();
        let review_token = request.review_token;
        let payload_fingerprint = state.payload_fingerprint;
        let request_generation = state.request_generation;
        let retry_attempt = state.retry_attempt;
        self.walletconnect.walletconnect_fee_quote_task = Some(cx.spawn(async move |this, cx| {
            let result = quote_walletconnect_fee_with_retry(
                chain_id,
                effective_chain,
                http,
                expiry_timestamp,
                request.rpc_reads.clone(),
                request.request_control.clone(),
            )
            .await;
            let _ = this.update(cx, |root, cx| {
                let request_matches = root
                    .walletconnect
                    .pending_requests
                    .get(request_key.as_ref())
                    .is_some_and(|request| {
                        request.is_current()
                            && request.review_token == review_token
                            && request.timeout_timestamp()
                                == root
                                    .walletconnect
                                    .walletconnect_fee_state
                                    .as_ref()
                                    .and_then(|state| state.expiry_timestamp)
                            && walletconnect_request_payload_fingerprint(request)
                                .is_ok_and(|fingerprint| fingerprint == payload_fingerprint)
                    });
                let current = request_matches
                    && !root
                        .walletconnect
                        .request_actions
                        .contains(request_key.as_ref())
                    && root
                        .walletconnect
                        .walletconnect_fee_state
                        .as_ref()
                        .is_some_and(|state| {
                            state.is_completion_current(
                                request_key.as_ref(),
                                review_token,
                                payload_fingerprint,
                                guard,
                                root.walletconnect.request_dialog_open
                                    && root.walletconnect.request_dialog_key.as_deref()
                                        == Some(request_key.as_ref())
                                    && root.walletconnect.walletconnect_fee_request_generation
                                        == request_generation,
                                current_unix_seconds(),
                            ) && root.walletconnect.walletconnect_gas_fee.refresh_id == refresh_id
                        });
                if !current {
                    tracing::debug!(target: LOG_TARGET,
                        chain_id,
                        network_mode = %network_mode,
                        attempt = retry_attempt,
                        "WalletConnect fee quote completion discarded as stale"
                    );
                    return;
                }
                root.walletconnect.walletconnect_gas_fee.refreshing = false;
                match result {
                    Ok(quote) => {
                        root.walletconnect.walletconnect_gas_fee.quote = Some(quote);
                        root.walletconnect.walletconnect_gas_fee.quote_error = None;
                        let simulate_after_quote = root
                            .walletconnect
                            .walletconnect_fee_state
                            .as_ref()
                            .is_some_and(|state| state.simulation_requested);
                        root.walletconnect_fee_state_changed_by_editor(cx);
                        if simulate_after_quote {
                            root.simulate_current_walletconnect_transaction(
                                request_key.as_ref(),
                                cx,
                            );
                        }
                    }
                    Err(error) => {
                        root.walletconnect.walletconnect_gas_fee.quote_error =
                            Some(Arc::from(error.clone()));
                        if let Some(state) = root.walletconnect.walletconnect_fee_state.as_mut() {
                            state.status = WalletConnectFeeStatus::UnavailableFailed;
                            state.error = Some(Arc::from(error));
                            state.simulation_requested = false;
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    pub(in crate::root::walletconnect) fn retry_walletconnect_fee(
        &mut self,
        request_key: &str,
        cx: &Context<'_, Self>,
    ) {
        let Some(state) = self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .filter(|state| {
                state.request_key.as_ref() == request_key
                    && matches!(state.status, WalletConnectFeeStatus::UnavailableFailed)
            })
        else {
            return;
        };
        let simulation_retryable = state.simulation_retryable;
        if let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() {
            state.begin_manual_retry();
        }
        if simulation_retryable {
            self.simulate_current_walletconnect_transaction(request_key, cx);
        } else {
            self.refresh_walletconnect_gas_fee_quote(Arc::from(request_key), cx);
        }
    }

    pub(in crate::root::walletconnect) fn simulate_current_walletconnect_transaction(
        &mut self,
        request_key: &str,
        cx: &Context<'_, Self>,
    ) {
        if !self.walletconnect.request_dialog_open
            || self.walletconnect.request_dialog_key.as_deref() != Some(request_key)
        {
            return;
        }
        let Some((request, operation)) = self.walletconnect_request_with_operation(request_key)
        else {
            return;
        };
        if !walletconnect_request_can_simulate(&request)
            || !request.approval_admitted(current_unix_seconds())
        {
            return;
        }
        let Some(state) = self.walletconnect.walletconnect_fee_state.as_mut() else {
            return;
        };
        state.simulation_requested = true;
        state.simulation_retryable = true;
        let selection = match self.walletconnect.walletconnect_gas_fee.selection(cx) {
            Ok(selection) => selection,
            Err(error) => {
                state.status = WalletConnectFeeStatus::UnavailableFailed;
                state.error = Some(Arc::from(error));
                state.simulation_requested = false;
                return;
            }
        };
        let quote = match (selection, self.walletconnect.walletconnect_gas_fee.quote) {
            (PublicActionGasFeeSelection::Auto, Some(quote)) => Some(quote),
            (PublicActionGasFeeSelection::Auto, None) => {
                state.status = WalletConnectFeeStatus::AwaitingSimulation;
                let key = Arc::<str>::from(request_key);
                self.refresh_walletconnect_gas_fee_quote(key, cx);
                return;
            }
            (
                PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas, ..
                },
                None,
            ) => Some(SelfBroadcastGasFeeQuote::from_rpc_gas_price(
                max_fee_per_gas,
            )),
            (_, quote) => quote,
        };
        let Some(quote) = quote else {
            state.status = WalletConnectFeeStatus::AwaitingSimulation;
            return;
        };
        let chain_id = match parse_chain_id(&request.item.chain_id) {
            Ok(chain_id) => chain_id,
            Err(error) => {
                state.status = WalletConnectFeeStatus::UnavailableFailed;
                state.error = Some(Arc::from(error));
                state.simulation_requested = false;
                return;
            }
        };
        let Ok(resolved) = resolve_public_action_gas_fee(
            chain_id,
            PublicShieldTransactionProfile::Railoxide,
            PublicAssetId::Native,
            selection,
            Some(quote),
        ) else {
            state.status = WalletConnectFeeStatus::UnavailableFailed;
            state.simulation_requested = false;
            return;
        };
        state.status = WalletConnectFeeStatus::AwaitingSimulation;
        state.begin_attempt(self.walletconnect.walletconnect_gas_fee.refresh_id);
        let guard = state.completion_guard();
        let payload_fingerprint = state.payload_fingerprint;
        let review_token = state.review_token;
        let request_generation = state.request_generation;
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let mut estimate_request =
            match walletconnect_transaction_estimate_request(&request, chain_id, effective_chain) {
                Ok(request) => request,
                Err(error) => {
                    state.status = WalletConnectFeeStatus::UnavailableFailed;
                    state.error = Some(Arc::from(error));
                    state.simulation_requested = false;
                    return;
                }
            };
        estimate_request.gas_fee = selection;
        let http = self.http.clone();
        let network_mode = http.network_mode();
        let retry_attempt = state.retry_attempt;
        let request_key = Arc::<str>::from(request_key);
        cx.spawn(async move |this, cx| {
            let result = Box::pin(walletconnect_await_before_request_expiry(
                request.timeout_timestamp(),
                async {
                    if !request.is_current() {
                        return WalletConnectSimulationResult::Error(WalletConnectSimulationError::Unavailable(Arc::from("Dapp approval is no longer current")));
                    }
                    let simulation = simulate_walletconnect_transaction(estimate_request, quote, resolved, &http, &operation, request.rpc_reads.as_ref());
                    if let Some(control) = &request.request_control {
                        tokio::select! {
                            result = simulation => result,
                            () = control.cancelled() => WalletConnectSimulationResult::Error(WalletConnectSimulationError::Unavailable(Arc::from("Dapp approval is no longer current"))),
                        }
                    } else { simulation.await }
                },
            ))
            .await
            .unwrap_or_else(|_| {
                WalletConnectSimulationResult::Error(WalletConnectSimulationError::Unavailable(
                    Arc::from("WalletConnect request expired before simulation completed"),
                ))
            });
            let _ = this.update(cx, |root, _cx| {
                let current = !root
                    .walletconnect
                    .request_actions
                    .contains(request_key.as_ref())
                    && root
                        .walletconnect
                        .pending_requests
                        .get(request_key.as_ref())
                        .is_some_and(|request| {
                            request.is_current() && request.review_token == review_token
                                && request.timeout_timestamp()
                                    == root
                                        .walletconnect
                                        .walletconnect_fee_state
                                        .as_ref()
                                        .and_then(|state| state.expiry_timestamp)
                                && walletconnect_request_payload_fingerprint(request)
                                    .is_ok_and(|fingerprint| fingerprint == payload_fingerprint)
                        })
                    && root
                        .walletconnect
                        .walletconnect_fee_state
                        .as_ref()
                        .is_some_and(|state| {
                            state.is_completion_current(
                                request_key.as_ref(),
                                review_token,
                                payload_fingerprint,
                                guard,
                                root.walletconnect.request_dialog_open
                                    && root.walletconnect.request_dialog_key.as_deref()
                                        == Some(request_key.as_ref())
                                    && root.walletconnect.walletconnect_fee_request_generation
                                        == request_generation
                                    && root.walletconnect.walletconnect_gas_fee.refresh_id
                                        == guard.editor_generation,
                                current_unix_seconds(),
                            )
                        });
                if !current {
                    tracing::debug!(target: LOG_TARGET,
                        chain_id,
                        network_mode = %network_mode,
                        attempt = retry_attempt,
                        "WalletConnect simulation completion discarded as stale"
                    );
                    return;
                }
                let native_usd_micro_rate = root
                    .public_broadcaster_anchor_cache
                    .cached_native_usd_rate(chain_id);
                let Some(state) = root.walletconnect.walletconnect_fee_state.as_mut() else {
                    return;
                };
                state.simulation_requested = false;
                match result {
                    WalletConnectSimulationResult::Complete(estimate) => {
                        let projection = estimate.fee_projection(native_usd_micro_rate, root.effective_chain_configs.get(chain_id).map_or(18, |chain| chain.native_currency.decimals));
                        state.apply_successful_simulation_projection(projection);
                        state.error = None;
                        state.simulation_retryable = false;
                    }
                    WalletConnectSimulationResult::Error(
                        WalletConnectSimulationError::Reverted(error),
                    ) => {
                        state.status = WalletConnectFeeStatus::WouldRevert;
                        state.error = Some(error);
                        state.simulation_retryable = false;
                    }
                    WalletConnectSimulationResult::Error(
                        WalletConnectSimulationError::Unavailable(error),
                    ) => {
                        state.status = WalletConnectFeeStatus::UnavailableFailed;
                        state.error = Some(error);
                    }
                }
            });
        })
        .detach();
    }
}
