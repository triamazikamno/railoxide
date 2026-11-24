use super::*;

impl WalletRoot {
    pub(in crate::root) fn submit_public_send_from_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_action_in_flight() {
            return;
        }
        self.clear_public_action_progress_state();
        let Some(draft) = self.public_send_draft(cx) else {
            return;
        };
        let summary = public_send_authorization_summary(&draft);
        let hardware_derived = draft.public_account_source == PublicAccountSource::HardwareDerived;
        let intent = SpendAuthorizationIntent::PublicSend(Box::new(draft));
        if hardware_derived {
            Self::open_hardware_public_action_authorization_dialog(intent, summary, window, cx);
        } else {
            self.request_spend_authorization(intent, summary, window, cx);
        }
    }

    pub(in crate::root) fn public_send_draft(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<PublicSendDraft> {
        let Some(asset) = self.public_form.selected_asset else {
            self.public_form.send_error = Some(Arc::from("Select an asset to send"));
            cx.notify();
            return None;
        };
        let Some(public_account_uuid) = self.public_form.selected_account_uuid.clone() else {
            self.public_form.send_error = Some(Arc::from("Select a public account first"));
            cx.notify();
            return None;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.send_error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return None;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.public_form.send_error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return None;
        };
        let chain_id = self.last_evm_chain;
        let asset_decimals = public_asset_decimals(
            self.effective_chain_configs.get(chain_id),
            asset,
            Some(&self.effective_token_registry),
        );
        let asset_label = public_action_asset_label(
            self.effective_chain_configs.get(chain_id),
            asset,
            Some(&self.effective_token_registry),
        );
        let asset_icon_path =
            public_asset_icon_path(chain_id, asset, Some(&self.effective_token_registry));
        let Some(public_account) = self.public_account_for_uuid(Some(public_account_uuid.as_ref()))
        else {
            self.public_form.send_error = Some(Arc::from("Selected public account was not found"));
            cx.notify();
            return None;
        };
        let public_account_label = public_account_display_label(public_account)
            .unwrap_or_else(|| short_address(&public_account.address));
        let public_account_source = public_account.source;
        let wrap_mode = matches!(
            self.public_form.action_mode,
            PublicActionMode::Wrap | PublicActionMode::Unwrap
        );
        let (amount, recipient, intent, advanced_estimate, reviewed_limit, gas_fee, fee_display) =
            if self.public_form.public_send_kind.is_advanced() && !wrap_mode {
                if asset != PublicAssetId::Native {
                    self.public_form.send_error = Some(Arc::from(
                        "Advanced transactions are available only from the native asset",
                    ));
                    cx.notify();
                    return None;
                }
                let parsed = parse_advanced_public_send_intent(
                    self.public_form.public_send_kind,
                    self.public_form
                        .advanced_send_to_input
                        .read(cx)
                        .value()
                        .as_ref(),
                    self.public_form
                        .advanced_send_value_input
                        .read(cx)
                        .value()
                        .as_ref(),
                    self.public_form
                        .advanced_send_data_input
                        .read(cx)
                        .value()
                        .as_ref(),
                    asset_decimals,
                );
                let intent = match parsed {
                    Ok(intent) => intent,
                    Err(error) => {
                        self.set_advanced_public_send_validation_error(error);
                        cx.notify();
                        return None;
                    }
                };
                let Some(estimate) = self.public_form.advanced_send_estimate.clone() else {
                    self.public_form.send_error =
                        Some(Arc::from(advanced_public_send_estimate_required_message(
                            self.public_form.advanced_send_estimate_invalidated,
                        )));
                    cx.notify();
                    return None;
                };
                let PublicTransactionIntent::Raw { to, value, .. } = &intent else {
                    unreachable!("advanced parser returns a raw intent")
                };
                let expected_token_value =
                    self.configured_native_amount_label(chain_id, estimate.expected_gas_cost);
                let expected_usd_micro_value = self
                    .public_broadcaster_anchor_cache
                    .cached_native_usd_micro_value(chain_id, estimate.expected_gas_cost);
                let maximum_token_value =
                    self.configured_native_amount_label(chain_id, estimate.max_gas_cost);
                let maximum_usd_micro_value = self
                    .public_broadcaster_anchor_cache
                    .cached_native_usd_micro_value(chain_id, estimate.max_gas_cost);
                let fee_display = PublicActionFeeDisplay {
                    gas_limit: None,
                    expected_gas_cost: Some(format_value_with_usd_label(
                        expected_token_value,
                        estimate.expected_gas_cost,
                        self.effective_chain_configs
                            .get(chain_id)
                            .map(|chain| chain.native_currency.decimals),
                        expected_usd_micro_value,
                        false,
                    )),
                    maximum_gas_cost: Some(format_value_with_usd_label(
                        maximum_token_value,
                        estimate.max_gas_cost,
                        self.effective_chain_configs
                            .get(chain_id)
                            .map(|chain| chain.native_currency.decimals),
                        maximum_usd_micro_value,
                        false,
                    )),
                    show_maximum_gas_cost: public_action_maximum_gas_cost_is_significant(
                        estimate.expected_gas_cost,
                        estimate.max_gas_cost,
                    ),
                    protocol_fee: None,
                };
                (
                    *value,
                    to.unwrap_or_default(),
                    intent,
                    Some(estimate.clone()),
                    None,
                    PublicActionGasFeeSelection::Custom {
                        max_fee_per_gas: estimate.max_fee_per_gas,
                        max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
                    },
                    fee_display,
                )
            } else {
                let mode = match self.public_form.action_mode {
                    mode @ (PublicActionMode::Wrap | PublicActionMode::Unwrap) => mode,
                    PublicActionMode::Shield | PublicActionMode::Send => PublicActionMode::Send,
                };
                let amount_input = self
                    .public_form
                    .send_amount_input
                    .read(cx)
                    .value()
                    .to_string();
                let amount = match parse_send_amount(&amount_input, asset_decimals) {
                    Ok(amount) if !amount.is_zero() => amount,
                    Ok(_) => {
                        self.public_form.send_error =
                            Some(Arc::from("Amount must be greater than zero"));
                        cx.notify();
                        return None;
                    }
                    Err(error) => {
                        self.public_form.send_error = Some(Arc::from(error.to_string()));
                        cx.notify();
                        return None;
                    }
                };
                // A wrap or unwrap calls the wrapped native token, recorded as its recipient.
                let recipient = if mode == PublicActionMode::Send {
                    parse_address(
                        self.public_form
                            .send_recipient_input
                            .read(cx)
                            .value()
                            .as_ref(),
                    )
                } else {
                    self.effective_chain_configs
                        .get(chain_id)
                        .and_then(|chain| chain.wrapped_native_token)
                        .filter(|wrapped| {
                            let expected = if mode == PublicActionMode::Wrap {
                                PublicAssetId::Native
                            } else {
                                PublicAssetId::Erc20(*wrapped)
                            };
                            asset == expected
                        })
                };
                let Some(recipient) = recipient else {
                    self.public_form.send_error =
                        Some(Arc::from(if mode == PublicActionMode::Send {
                            "Enter a valid EVM recipient address"
                        } else {
                            "This chain has no wrapped native token for this asset"
                        }));
                    cx.notify();
                    return None;
                };
                if mode == PublicActionMode::Unwrap
                    && let Some(error) = self.selected_public_unwrap_gas_error(asset, cx)
                {
                    self.public_form.send_error = Some(Arc::from(error));
                    cx.notify();
                    return None;
                }
                let gas_fee = match self.public_action_authorized_gas_fee_selection(
                    mode,
                    PublicShieldTransactionProfile::Railoxide,
                    PublicAssetId::Native,
                    cx,
                ) {
                    Ok(selection) => selection,
                    Err(error) => {
                        self.public_form.send_error = Some(Arc::from(error));
                        cx.notify();
                        return None;
                    }
                };
                let fee_display = self.public_action_fee_display(mode, Some(amount), cx);
                let reviewed_limit = self
                    .public_form_reviewed_gas_limits(mode, asset)
                    .map(|(_, _, gas_limit)| gas_limit);
                if fee_display.expected_gas_cost.is_none() || fee_display.maximum_gas_cost.is_none()
                {
                    self.public_form.send_error = Some(Arc::from(
                        "Wait for an estimated gas cost before authorizing the send",
                    ));
                    cx.notify();
                    return None;
                }
                let intent = match mode {
                    PublicActionMode::Wrap => PublicTransactionIntent::WrapNative {
                        wrapped: recipient,
                        amount,
                    },
                    PublicActionMode::Unwrap => PublicTransactionIntent::UnwrapNative {
                        wrapped: recipient,
                        amount,
                    },
                    PublicActionMode::Shield | PublicActionMode::Send => {
                        PublicTransactionIntent::Transfer {
                            asset,
                            amount,
                            recipient,
                        }
                    }
                };
                (
                    amount,
                    recipient,
                    intent,
                    None,
                    reviewed_limit,
                    gas_fee,
                    fee_display,
                )
            };
        let operation = self.public_form_operation(&public_account_uuid);
        Some(PublicSendDraft {
            gateway_execution: None,
            chain_id,
            asset,
            asset_label,
            asset_icon_path,
            asset_decimals,
            public_account_uuid,
            public_account_label,
            public_account_source,
            view_session,
            vault_store,
            amount,
            recipient,
            intent,
            advanced_estimate,
            reviewed_gas_limit: reviewed_limit,
            gas_fee,
            fee_display,
            operation,
        })
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn submit_public_send_authorized(
        &mut self,
        draft: PublicSendDraft,
        spend_authorization: wallet_ops::DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(resolved_chain) = self
            .effective_chain_configs
            .enabled(self.last_evm_chain)
            .cloned()
        else {
            return;
        };
        if self.public_action_in_flight() {
            if let Some(execution) = &draft.gateway_execution {
                execution.finish(false);
            }
            return;
        }
        if let Some(execution) = &draft.gateway_execution {
            if self.last_evm_chain != draft.chain_id
                || self.public_form.selected_account_uuid.as_deref()
                    != Some(draft.public_account_uuid.as_ref())
                || !self
                    .view_session
                    .as_ref()
                    .is_some_and(|wallet| wallet.is_same_wallet_session(&draft.view_session))
            {
                execution.finish(false);
                return;
            }
            if !execution.start() {
                return;
            }
        }
        let PublicSendDraft {
            gateway_execution,
            chain_id,
            asset,
            asset_label,
            public_account_uuid,
            public_account_source,
            view_session,
            vault_store,
            intent,
            advanced_estimate,
            reviewed_gas_limit,
            gas_fee,
            operation,
            ..
        } = draft;
        let transaction_tracking =
            match self.public_transaction_tracking_context(chain_id, &public_account_uuid) {
                Ok(context) => context,
                Err(error) => {
                    if let Some(execution) = &gateway_execution {
                        execution.finish(false);
                    }
                    self.public_form.send_error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            };
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase = view_session.hardware_profile_session().and_then(|session| {
            self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
        });
        #[cfg(not(feature = "hardware"))]
        let trezor_app_passphrase = None;
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider =
            if matches!(&public_account_source, PublicAccountSource::HardwareDerived) {
                Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
            } else {
                None
            };
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_provider = None;
        self.public_form.sending = true;
        self.public_form.send_error = None;
        let http = self.http.clone();
        let active_wallet_id = self.selected_wallet_id.clone();
        let icon_path =
            public_asset_icon_path(chain_id, asset, Some(&self.effective_token_registry));
        let mode = match intent {
            PublicTransactionIntent::WrapNative { .. } => PublicActionMode::Wrap,
            PublicTransactionIntent::UnwrapNative { .. } => PublicActionMode::Unwrap,
            PublicTransactionIntent::Transfer { .. } | PublicTransactionIntent::Raw { .. } => {
                PublicActionMode::Send
            }
        };
        let initial_gas_fee = self.public_action_initial_gas_values(mode, &gas_fee);
        let advanced_submission = advanced_estimate.is_some();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let generation = self.start_public_action_progress(
            mode,
            asset,
            asset_label,
            icon_path,
            public_account_source,
            Some(command_tx),
            initial_gas_fee,
            advanced_submission,
        );
        if let Some(execution) = &gateway_execution {
            execution.bind_generation(generation);
        }
        let progress_tx = Self::follow_public_action_progress(
            generation,
            chain_id,
            active_wallet_id.clone(),
            event_rx,
            gateway_execution.clone(),
            window,
            cx,
        );
        let request = PublicSendRequest {
            executor_owner: self.executor_owner_for_public_chain(chain_id),
            transaction_tracking: Some(transaction_tracking),
            chain_id,
            effective_chain: resolved_chain,
            view_session,
            vault_store,
            authorization: Some(spend_authorization),
            trezor_app_passphrase,
            trezor_pin_matrix_provider,
            public_account_uuid: public_account_uuid.to_string(),
            intent,
            advanced_authorization: advanced_estimate.map(|estimate| {
                PublicAdvancedTransactionAuthorization {
                    payload_fingerprint: estimate.payload_fingerprint,
                    gas_limit: estimate.gas_limit,
                }
            }),
            reviewed_gas_limit,
            gateway_origin: gateway_execution
                .as_ref()
                .and_then(wallet_ops::gateway::GatewayDraftExecution::origin),
            gas_fee,
            command_rx: Some(command_rx),
            event_tx: Some(event_tx),
        };
        let submitted_public_account_uuid = Arc::clone(&public_account_uuid);
        let join = self.spawn_public_transaction_submission(chain_id, async move {
            let client = operation.client().await?;
            submit_public_send_with_progress(request, &http, &client, move |update| {
                let _ = progress_tx.send(update);
            })
            .await
        });
        self.public_form.action_task_abort_handle = join.abort_handle();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            if let Some(execution) = &gateway_execution {
                execution.finish(matches!(&result, Ok(Ok(_))));
            }
            let _ = this.update(cx, |root, cx| {
                if root.selected_wallet_id != active_wallet_id || root.last_evm_chain != chain_id {
                    return;
                }
                if !public_action_accepts_update(
                    root.public_form.action_generation,
                    generation,
                    root.public_form.action_stopped,
                ) {
                    return;
                }
                root.public_form.sending = false;
                root.public_form.action_task_abort_handle = None;
                match result {
                    Ok(Ok(result)) => {
                        root.public_form.action_command_tx = None;
                        root.public_form.action_action_error = None;
                        root.public_form.action_contract_address =
                            result.tx.contract_address.as_deref().map(Arc::from);
                        root.refresh_public_action_submitter(
                            chain_id,
                            &submitted_public_account_uuid,
                            cx,
                        );
                    }
                    Ok(Err(error)) => {
                        let message = format_report_chain(&error);
                        if is_spend_authorization_failure_error(&message) {
                            root.clear_spend_authorization(cx);
                        }
                        root.discard_active_trezor_session_if_stale(&message, cx);
                        root.public_form.action_command_tx = None;
                        if advanced_submission {
                            root.invalidate_advanced_public_send_estimate();
                        }
                        root.fail_public_action_progress(generation, message.clone(), cx);
                        root.public_form.send_error = Some(Arc::from(message));
                    }
                    Err(error) => {
                        let message = format!("Public send task failed: {error}");
                        root.public_form.action_command_tx = None;
                        if advanced_submission {
                            root.invalidate_advanced_public_send_estimate();
                        }
                        root.fail_public_action_progress(generation, message.clone(), cx);
                        root.public_form.send_error = Some(Arc::from(message));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
