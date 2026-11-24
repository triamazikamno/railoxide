#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn submit_public_shield_from_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_action_in_flight() {
            return;
        }
        // A fixture's wrap fallback is real; a Custom fee would otherwise let it send.
        #[cfg(debug_assertions)]
        if public_shield_ui_fixture().is_some() {
            return;
        }
        self.clear_public_action_progress_state();
        let Some(draft) = self.public_shield_draft(cx) else {
            return;
        };
        let summary = public_shield_authorization_summary(&draft)
            .with_privacy_hints(self.shield_privacy_presentation(cx));
        let hardware_derived = draft.public_account_source == PublicAccountSource::HardwareDerived;
        let intent = SpendAuthorizationIntent::PublicShield(Box::new(draft));
        if hardware_derived {
            Self::open_hardware_public_action_authorization_dialog(intent, summary, window, cx);
        } else {
            self.request_spend_authorization(intent, summary, window, cx);
        }
    }

    pub(in crate::root) fn public_shield_draft(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<PublicShieldDraft> {
        let Some(asset) = self.public_form.selected_asset else {
            self.public_form.shield_error = Some(Arc::from("Select an asset to shield"));
            cx.notify();
            return None;
        };
        let Some(public_account_uuid) = self.public_form.selected_account_uuid.clone() else {
            self.public_form.shield_error = Some(Arc::from("Select a public account first"));
            cx.notify();
            return None;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.shield_error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return None;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.public_form.shield_error = Some(Arc::from("Wallet vault storage is unavailable"));
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
            self.public_form.shield_error =
                Some(Arc::from("Selected public account was not found"));
            cx.notify();
            return None;
        };
        let public_account_label = public_account_display_label(public_account)
            .unwrap_or_else(|| short_address(&public_account.address));
        let public_account_source = public_account.source;
        let amount_input = self
            .public_form
            .shield_amount_input
            .read(cx)
            .value()
            .to_string();
        let amount = match parse_send_amount(&amount_input, asset_decimals) {
            Ok(amount) if !amount.is_zero() => amount,
            Ok(_) => {
                self.public_form.shield_error = Some(Arc::from("Amount must be greater than zero"));
                cx.notify();
                return None;
            }
            Err(error) => {
                self.public_form.shield_error = Some(Arc::from(error.to_string()));
                cx.notify();
                return None;
            }
        };
        let profile = if self.mimic_railway_shield_effective() {
            PublicShieldTransactionProfile::Railway
        } else {
            PublicShieldTransactionProfile::Railoxide
        };
        let gas_fee_mode = match self.public_form.shield_gas_fee.mode {
            Eip1559GasFeeMode::Auto => PublicActionGasFeeMode::Auto,
            Eip1559GasFeeMode::Custom => PublicActionGasFeeMode::Custom,
        };
        let gas_fee = match self.public_action_authorized_gas_fee_selection(
            PublicActionMode::Shield,
            profile,
            asset,
            cx,
        ) {
            Ok(selection) => selection,
            Err(error) => {
                self.public_form.shield_error = Some(Arc::from(error));
                cx.notify();
                return None;
            }
        };
        let authorized_fee_ceiling = if public_action_uses_railway_authorization_ceiling(
            PublicActionMode::Shield,
            profile,
            asset,
            gas_fee_mode,
        ) {
            let Some(ceiling) = self.public_form.shield_gas_fee_authorization_ceiling else {
                self.public_form.shield_error =
                    Some(Arc::from("Wait for the Railway authorization ceiling"));
                cx.notify();
                return None;
            };
            ceiling
        } else {
            gas_fee
        };
        if let Some(reason) = self.native_shield_unavailable(PublicActionMode::Shield) {
            self.public_form.shield_error = Some(Arc::from(format!(
                "Native shielding is unavailable: {reason}"
            )));
            cx.notify();
            return None;
        }
        // The route is bound here: the stealth route by its preparation, the wrap fallback by
        // having none.
        let native_preparation = if self.native_shield_stealth_applies(PublicActionMode::Shield) {
            let Some(preparation) = self.current_native_shield_preparation(cx) else {
                self.public_form.shield_error =
                    Some(Arc::from("Wait for the native shield to be prepared"));
                cx.notify();
                return None;
            };
            Some(preparation)
        } else {
            None
        };
        let fee_display =
            self.public_action_fee_display(PublicActionMode::Shield, Some(amount), cx);
        let reviewed_gas_limits = native_preparation
            .is_none()
            .then(|| self.public_form_reviewed_gas_limits(PublicActionMode::Shield, asset))
            .flatten();
        if fee_display.expected_gas_cost.is_none()
            || fee_display.maximum_gas_cost.is_none()
            || fee_display.protocol_fee.is_none()
        {
            self.public_form.shield_error = Some(Arc::from(
                "Wait for fee estimates before authorizing the shield",
            ));
            cx.notify();
            return None;
        }
        // Bound before authorization, which a Ledger or Trezor wallet gives for this operation.
        let native_operation = if native_preparation.is_some() {
            match bind_native_shield_operation(
                &mut self.public_form.native_shield_operation,
                NativeShieldKey {
                    wallet_id: self.selected_wallet_id.clone(),
                    chain_id,
                    public_account_uuid: Arc::clone(&public_account_uuid),
                    profile,
                    amount,
                },
            ) {
                Ok(operation) => Some(operation),
                Err(error) => {
                    self.public_form.shield_error = Some(Arc::from(error.to_string()));
                    cx.notify();
                    return None;
                }
            }
        } else {
            None
        };
        let operation = self.public_form_operation(&public_account_uuid);
        Some(PublicShieldDraft {
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
            profile,
            approval_mode: profile.approval_mode(self.public_form.shield_approval_mode),
            native_preparation,
            native_operation,
            reviewed_gas_limits,
            gas_fee,
            gas_fee_mode,
            authorized_fee_ceiling,
            fee_display,
            operation,
        })
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn submit_public_shield_authorized(
        &mut self,
        draft: PublicShieldDraft,
        spend_authorization: wallet_ops::DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(resolved_chain) = self
            .effective_chain_configs
            .railgun(self.last_evm_chain)
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
        let PublicShieldDraft {
            gateway_execution,
            chain_id,
            asset,
            asset_label,
            public_account_uuid,
            public_account_source,
            view_session,
            vault_store,
            amount,
            profile,
            approval_mode,
            native_preparation,
            native_operation,
            reviewed_gas_limits,
            gas_fee,
            gas_fee_mode,
            authorized_fee_ceiling,
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
                    self.public_form.shield_error = Some(Arc::from(error));
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
        self.public_form.shielding = true;
        self.public_form.shield_error = None;
        // A preparation serves one submission; a later attempt is prepared and reviewed anew.
        self.public_form.native_shield_preparation.invalidate();
        let http = self.http.clone();
        let active_wallet_id = self.selected_wallet_id.clone();
        let icon_path =
            public_asset_icon_path(chain_id, asset, Some(&self.effective_token_registry));
        let initial_gas_fee =
            self.public_action_initial_gas_values(PublicActionMode::Shield, &gas_fee);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let generation = self.start_public_action_progress_with_steps(
            PublicActionMode::Shield,
            public_action_progress_steps_for_source(
                PublicActionMode::Shield,
                asset,
                public_account_source,
                native_preparation.is_none(),
            ),
            asset_label,
            icon_path,
            public_account_source,
            Some(command_tx),
            initial_gas_fee,
            false,
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
        let request = PublicShieldRequest {
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
            asset,
            amount,
            profile,
            approval_mode,
            native_preparation,
            native_operation,
            reviewed_gas_limits,
            gateway_origin: gateway_execution
                .as_ref()
                .and_then(wallet_ops::gateway::GatewayDraftExecution::origin),
            gas_fee,
            gas_fee_mode,
            authorized_fee_ceiling,
            command_rx: Some(command_rx),
            event_tx: Some(event_tx),
        };
        let submitted_public_account_uuid = Arc::clone(&public_account_uuid);
        let native_stealth = request.native_preparation.is_some();
        let join = self.spawn_public_transaction_submission(chain_id, async move {
            let client = operation.client().await?;
            submit_public_shield_with_progress(request, &http, &client, move |update| {
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
                if root.selected_wallet_id != active_wallet_id {
                    return;
                }
                if root.last_evm_chain != chain_id {
                    return;
                }
                if !public_action_accepts_update(
                    root.public_form.action_generation,
                    generation,
                    root.public_form.action_stopped,
                ) {
                    return;
                }
                root.public_form.shielding = false;
                root.public_form.action_task_abort_handle = None;
                if native_stealth && !matches!(&result, Ok(Ok(_))) {
                    root.refresh_native_shield_preparation(cx);
                }
                match result {
                    Ok(Ok(_result)) => {
                        if native_stealth {
                            root.public_form.native_shield_operation = None;
                        }
                        root.public_form.action_command_tx = None;
                        root.public_form.action_action_error = None;
                        root.public_form.mimic_railway_shield =
                            root.mimic_railway_shields_by_default;
                        root.refresh_public_action_submitter(
                            chain_id,
                            &submitted_public_account_uuid,
                            cx,
                        );
                    }
                    Ok(Err(error)) => {
                        // A retired stealth account, or one that may hold an undelivered
                        // shield's value, is never reused.
                        if error
                            .downcast_ref::<wallet_ops::ExecutorAccountUsed>()
                            .is_some()
                            || error
                                .downcast_ref::<wallet_ops::NativeShieldUndelivered>()
                                .is_some()
                        {
                            root.public_form.native_shield_operation = None;
                        }
                        let message = format_report_chain(&error);
                        if is_spend_authorization_failure_error(&message) {
                            root.clear_spend_authorization(cx);
                        }
                        root.discard_active_trezor_session_if_stale(&message, cx);
                        root.public_form.action_command_tx = None;
                        root.fail_public_action_progress(generation, message.clone(), cx);
                        root.public_form.shield_error = Some(Arc::from(message));
                    }
                    Err(error) => {
                        let message = format!("Public shield task failed: {error}");
                        root.public_form.action_command_tx = None;
                        root.fail_public_action_progress(generation, message.clone(), cx);
                        root.public_form.shield_error = Some(Arc::from(message));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
