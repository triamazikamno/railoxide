use super::*;

impl WalletRoot {
    /// A clone of the pending request and its operation client, which a `WalletConnect` request
    /// gets on first use.
    pub(in crate::root::walletconnect) fn walletconnect_request_with_operation(
        &mut self,
        request_key: &str,
    ) -> Option<(WalletConnectRequestUi, wallet_ops::OperationHttpClientSlot)> {
        let request = self.walletconnect.pending_requests.get_mut(request_key)?;
        let operation = request
            .operation
            .get_or_insert_with(|| wallet_ops::OperationHttpClientSlot::new(self.http.clone()))
            .clone();
        Some((request.clone(), operation))
    }

    pub(in crate::root) fn walletconnect_hardware_executor_action(
        &self,
        key: &str,
        review_token: u64,
    ) -> Option<(u64, wallet_ops::HardwareExecutorAction)> {
        self.walletconnect
            .pending_requests
            .get(key)?
            .hardware_executor_action(review_token)
    }

    pub(in crate::root) fn reconcile_gateway_approvals(
        &mut self,
        handle: &wallet_ops::gateway::GatewayHandle,
        ready: &[Arc<wallet_ops::gateway::GatewayApprovalRequest>],
        cx: &mut Context<'_, Self>,
    ) {
        let removed: Vec<_> = self
            .walletconnect
            .pending_requests
            .iter()
            .filter(|(_, request)| request.request_control.is_some())
            .filter(|(key, request)| {
                !request.is_current()
                    || (!self.walletconnect.request_actions.contains(key.as_str())
                        && !ready
                            .iter()
                            .any(|item| request.key == format!("gateway:{}", item.id)))
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in removed {
            self.walletconnect.remove_pending_request(&key);
            self.walletconnect.request_actions.remove(&key);
            if self.walletconnect.request_dialog_key.as_deref() == Some(key.as_str()) {
                self.clear_spend_authorization(cx);
                self.discard_walletconnect_fee_for_request_replacement();
            }
        }
        if let (Some(store), Some(view)) = (&self.vault_store, &self.view_session) {
            for approval in ready {
                let key = format!("gateway:{}", approval.id);
                if !walletconnect_request_should_queue(
                    &self.walletconnect.pending_requests,
                    &self.walletconnect.handled_request_keys,
                    &key,
                ) {
                    continue;
                }
                // The approval reads share the request's operation client.
                let operation = wallet_ops::OperationHttpClientSlot::new(self.http.clone());
                let Some(request) = gateway_pending_request(
                    approval,
                    handle.approval_reads(approval.id.clone(), operation.clone()),
                    operation,
                ) else {
                    continue;
                };
                if let Some(authorization) = &approval.authorization {
                    let permission = &authorization.permission;
                    let resolution = store.resolve_dapp_session_account(
                        view,
                        &permission.public_account_uuid,
                        &permission.public_account_scope,
                        permission.owning_private_wallet_uuid.as_deref(),
                    );
                    if !matches!(resolution, Ok(WalletConnectSessionAccountResolution::Usable(account)) if account == authorization.account)
                    {
                        continue;
                    }
                } else if !matches!(
                    approval.parsed,
                    WalletConnectParsedRequest::WalletAddEthereumChain { .. }
                ) {
                    continue;
                }
                self.walletconnect.request_routes.insert(
                    request.key.clone(),
                    DappRequestRoute::Gateway {
                        handle: handle.clone(),
                        approval_id: approval.id.clone(),
                    },
                );
                self.walletconnect
                    .pending_requests
                    .insert(request.key.clone(), request);
            }
        }
        self.publish_gateway_summaries(self.gateway_request_summaries());
        self.load_gateway_asset_metadata(cx);
        self.ensure_walletconnect_pending_request_expiry_timer(cx);
        self.sync_platform_attention();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn approve_walletconnect_request(
        &mut self,
        request_key: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.request_actions.contains(request_key) {
            return;
        }
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key)
            .cloned()
        else {
            return;
        };
        // Debug UI fixture: the canned request has no session to answer, so Approve stops here.
        #[cfg(debug_assertions)]
        if crate::root::walletconnect::ui_fixture::is_fixture_request(&request) {
            self.walletconnect.error = Some(Arc::from(
                "The debug UI fixture's request can't be approved.",
            ));
            cx.notify();
            return;
        }
        if !request.approval_admitted(current_unix_seconds()) {
            self.walletconnect.error = Some(Arc::from(
                "WalletConnect request expired before approval could start.",
            ));
            cx.notify();
            return;
        }
        if request.request_control.is_some() && request.parsed.desktop_policy() {
            self.approve_gateway_policy(request, window, cx);
            return;
        }
        let reviewed_fee = if walletconnect_request_fee_eligible(&request.parsed) {
            match self.capture_walletconnect_reviewed_fee(&request, cx) {
                Ok(reviewed_fee) => Some(reviewed_fee),
                Err(error) => {
                    self.walletconnect.error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        tracing::info!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(request_key),
            method = request.item.method.as_str(),
            chain_id = request.item.chain_id.as_str(),
            dapp = request.item.dapp_name.as_str(),
            hardware = request.account_source == Some(PublicAccountSource::HardwareDerived),
            "walletconnect request approval selected"
        );
        if request.account_source == Some(PublicAccountSource::HardwareDerived) {
            self.submit_walletconnect_request_authorized(
                request_key,
                request.review_token,
                reviewed_fee,
                wallet_ops::DesktopPrivateSpendAuthorization::HardwarePublic,
                window,
                cx,
            );
        } else {
            let intent = SpendAuthorizationIntent::WalletConnectRequest {
                request_key: request_key.to_owned(),
                review_token: request.review_token,
                reviewed_fee: reviewed_fee.clone(),
            };
            let summary = {
                let context = match self.walletconnect_intent_context(&request) {
                    Ok(context) => context,
                    Err(message) => {
                        self.walletconnect.error = Some(Arc::from(message));
                        cx.notify();
                        return;
                    }
                };
                let mut intent = build_walletconnect_intent(&request, context);
                append_walletconnect_simulation_risk(
                    request.key.as_ref(),
                    self.walletconnect.walletconnect_fee_state.as_ref(),
                    &mut intent.risks,
                );
                walletconnect_request_authorization_summary_with_fee(
                    &request,
                    &intent,
                    reviewed_fee.as_ref(),
                )
            };
            self.request_spend_authorization(intent, summary, window, cx);
        }
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn submit_walletconnect_request_authorized(
        &mut self,
        request_key: &str,
        review_token: u64,
        reviewed_fee: Option<WalletConnectReviewedFeeProjection>,
        spend_authorization: wallet_ops::DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.request_actions.contains(request_key) {
            return;
        }
        let Some((request, operation)) = self.walletconnect_request_with_operation(request_key)
        else {
            return;
        };
        if !walletconnect_request_matches_review_token(&request, review_token) {
            self.walletconnect.error = Some(Arc::from(
                "WalletConnect request changed while authorization was open; review the current request before approving.",
            ));
            cx.notify();
            return;
        }
        if let Err(error) =
            validate_walletconnect_reviewed_fee_pairing(&request.parsed, reviewed_fee.as_ref())
        {
            self.walletconnect.error = Some(Arc::from(error));
            self.clear_spend_authorization(cx);
            cx.notify();
            return;
        }
        if walletconnect_request_fee_eligible(&request.parsed)
            && let Some(reviewed_fee) = reviewed_fee.as_ref()
            && let Err(error) = self.validate_walletconnect_reviewed_fee(&request, reviewed_fee, cx)
        {
            self.walletconnect.error = Some(Arc::from(error));
            self.clear_spend_authorization(cx);
            cx.notify();
            return;
        }
        let (Some(vault_store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            self.walletconnect.error = Some(Arc::from("Unlock a wallet before approving requests"));
            cx.notify();
            return;
        };
        let Some(effective_chain) = parse_caip2_chain_id(&request.item.chain_id)
            .and_then(|chain_id| self.walletconnect_effective_chain_config(chain_id))
        else {
            self.walletconnect.error = Some(Arc::from("Request chain is unavailable"));
            cx.notify();
            return;
        };
        let request = match self.revalidate_walletconnect_pending_request(
            &request,
            vault_store.as_ref(),
            view_session.as_ref(),
            current_unix_seconds(),
        ) {
            Ok((request, route)) => {
                self.walletconnect
                    .request_routes
                    .insert(request.key.clone(), route);
                request
            }
            Err(error) => {
                let response_sender = match self.walletconnect_response_sender(&request, cx) {
                    Ok(context) => context,
                    Err(context_error) => {
                        self.walletconnect.error = Some(context_error);
                        cx.notify();
                        return;
                    }
                };
                self.publish_invalid_walletconnect_pending_request(
                    request_key,
                    &error,
                    response_sender,
                    window,
                    cx,
                );
                return;
            }
        };
        let response_sender = match self.walletconnect_response_sender(&request, cx) {
            Ok(context) => context,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let transaction_tracking = if matches!(
            request.parsed,
            WalletConnectParsedRequest::EthSendTransaction { .. }
        ) {
            let tracking = parse_caip2_chain_id(&request.item.chain_id)
                .ok_or_else(|| "WalletConnect request chain is not EIP-155".to_owned())
                .and_then(|chain_id| {
                    self.public_transaction_tracking_context(
                        chain_id,
                        &request
                            .binding
                            .account
                            .as_ref()
                            .ok_or("Request has no account authorization")?
                            .public_account_uuid,
                    )
                });
            match tracking {
                Ok(context) => Some(context),
                Err(error) => {
                    self.walletconnect.error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase =
            if request.account_source == Some(PublicAccountSource::HardwareDerived) {
                view_session.hardware_profile_session().and_then(|session| {
                    self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
                })
            } else {
                None
            };
        #[cfg(not(feature = "hardware"))]
        let trezor_app_passphrase = None;
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider =
            if request.account_source == Some(PublicAccountSource::HardwareDerived) {
                Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
            } else {
                None
            };
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_provider = None;
        let http = self.http.clone();
        let hardware_request = request.account_source == Some(PublicAccountSource::HardwareDerived);
        let hash_fallback_confirmed = walletconnect_request_uses_hardware_typed_data_hash_fallback(
            &request,
            self.walletconnect_request_hardware_typed_data_mode(&request),
        );
        let progress_generation = hardware_request.then(|| {
            self.walletconnect
                .start_request_approval_progress(request_key, &request)
        });
        let approval_event_tx = progress_generation.map(|generation| {
            let (event_tx, event_rx) = mpsc::unbounded_channel();
            Self::spawn_walletconnect_approval_session_event_listener(
                request_key.to_owned(),
                generation,
                event_rx,
                cx,
            );
            event_tx
        });
        if self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .is_some_and(|state| {
                state.request_key.as_ref() == request_key && state.review_token == review_token
            })
        {
            self.walletconnect.walletconnect_fee_quote_task = None;
            self.walletconnect.walletconnect_gas_fee.refreshing = false;
        }
        self.walletconnect
            .request_actions
            .insert(request_key.to_owned());
        self.walletconnect.error = None;
        tracing::info!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(request_key),
            method = request.item.method.as_str(),
            chain_id = request.item.chain_id.as_str(),
            dapp = request.item.dapp_name.as_str(),
            "submitting authorized walletconnect request"
        );
        let request_key = request_key.to_owned();
        let native_control = request.request_control.clone();
        let executor_owner = parse_caip2_chain_id(&request.item.chain_id)
            .and_then(|chain_id| self.executor_owner_for_public_chain(chain_id));
        let join = self.spawn_public_transaction_submission(effective_chain.chain_id, async move {
            Box::pin(approve_walletconnect_request_task(
                request,
                vault_store,
                view_session,
                spend_authorization,
                trezor_app_passphrase,
                trezor_pin_matrix_provider,
                effective_chain,
                executor_owner,
                response_sender,
                http,
                operation,
                hash_fallback_confirmed,
                reviewed_fee,
                approval_event_tx,
                transaction_tracking,
            ))
            .await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.request_actions.remove(&request_key);
                if native_control.as_ref().is_some_and(|control| control.ensure_current().is_err()) {
                    root.walletconnect.remove_pending_request(&request_key);
                    root.sync_platform_attention();
                    cx.notify();
                    return;
                }
                match result {
                    Ok(Ok(outcome)) => {
                        tracing::info!(
                            target: "wallet::root::walletconnect",
                            request_key = %walletconnect_request_key_log_label(&request_key),
                            authorization_failed = outcome.authorization_failed,
                            response_published = outcome.response_published,
                            tx_submitted = outcome.submitted_tx_hash.is_some(),
                            "walletconnect request approval handled"
                        );
                        if outcome.hash_fallback_confirmation_required {
                            #[cfg(feature = "hardware")]
                            if let Some(session) = outcome.refreshed_hardware_session {
                                root.refresh_active_hardware_profile_session(session, cx);
                            }
                            root.walletconnect.request_approval_progress.remove(&request_key);
                            root.walletconnect.status = Some(Arc::from(
                                "Review the EIP-712 hash fallback warning, then continue if you still want to approve on device.",
                            ));
                            cx.notify();
                            return;
                        }
                        let completed_request = root.walletconnect.remove_pending_request(&request_key);
                        let show_completion = completed_request.is_some();
                        if let Some(request) = completed_request {
                            root.walletconnect.completed_request_dialogs.insert(
                                request_key.clone(),
                                WalletConnectCompletedRequestUi::from_outcome(request, &outcome),
                            );
                        }
                        if !show_completion {
                            root.stop_walletconnect_request_dialog_refresh();
                        }
                        if root.walletconnect.request_dialog_key.as_deref()
                            == Some(request_key.as_str())
                        {
                            root.clear_trezor_app_passphrase_input(window, cx);
                            if show_completion {
                                root.walletconnect.request_dialog_open = true;
                            } else {
                                root.clear_walletconnect_request_dialog_state(window, cx);
                                window.close_dialog(cx);
                            }
                        }
                        if let Some(error) = outcome.relay_error {
                            let status = outcome.submitted_tx_hash.as_ref().map_or_else(
                                || "WalletConnect request was handled locally, but the relay response publish failed.".to_owned(),
                                |tx_hash| format!(
                                    "WalletConnect transaction was submitted ({tx_hash}), but the relay response publish failed. The request was removed to avoid rebroadcasting."
                                ),
                            );
                            root.walletconnect.status = Some(Arc::from(status));
                            root.walletconnect.error = Some(Arc::from(error));
                        } else if outcome.authorization_failed {
                            root.clear_spend_authorization(cx);
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect request was not authorized; error response published.",
                            ));
                        } else if outcome.expired {
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect request expired before approval completed.",
                            ));
                        } else {
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect request approved and response published.",
                            ));
                        }
                    }
                    Ok(Err(error)) => {
                        if native_control.is_some() && !matches!(error, DappApprovalTaskError::AdmissionBusy) {
                            root.walletconnect.remove_pending_request(&request_key);
                        }
                        let error = error.to_string();
                        tracing::warn!(
                            target: "wallet::root::walletconnect",
                            request_key = %walletconnect_request_key_log_label(&request_key),
                            error = %error,
                            "walletconnect request approval failed"
                        );
                        if let Some(generation) = progress_generation {
                            root.walletconnect.fail_request_approval_progress(
                                &request_key,
                                generation,
                                error.clone(),
                            );
                        }
                        root.walletconnect.error = Some(Arc::from(error));
                    }
                    Err(error) => {
                        if native_control.is_some() {
                            root.walletconnect.remove_pending_request(&request_key);
                        }
                        let message = format!("Dapp approval task failed: {error}");
                        if let Some(generation) = progress_generation {
                            root.walletconnect.fail_request_approval_progress(
                                &request_key,
                                generation,
                                message.clone(),
                            );
                        }
                        root.walletconnect.error = Some(Arc::from(message));
                    }
                }
                root.refresh_walletconnect_gas_fee_quote(Arc::from(request_key.as_str()), cx);
                root.sync_platform_attention();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn spawn_walletconnect_approval_session_event_listener(
        request_key: String,
        generation: u64,
        mut event_rx: mpsc::UnboundedReceiver<PublicActionSessionEvent>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                let _ = this.update(cx, |root, cx| {
                    root.apply_walletconnect_approval_session_event(
                        &request_key,
                        generation,
                        event,
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    pub(in crate::root::walletconnect) fn apply_walletconnect_approval_session_event(
        &mut self,
        request_key: &str,
        generation: u64,
        event: PublicActionSessionEvent,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            PublicActionSessionEvent::AttemptHandoff { .. } => {
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::PrepareRequest,
                    PublicActionStepStatus::Done,
                    None,
                    None,
                );
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::ApproveOnDevice,
                    PublicActionStepStatus::Pending,
                    None,
                    None,
                );
            }
            PublicActionSessionEvent::AttemptSubmitted { attempt, .. } => {
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::ApproveOnDevice,
                    PublicActionStepStatus::Done,
                    None,
                    None,
                );
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::BroadcastTransaction,
                    PublicActionStepStatus::Done,
                    Some(attempt.tx_hash),
                    None,
                );
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::RespondToDapp,
                    PublicActionStepStatus::Pending,
                    None,
                    None,
                );
            }
            PublicActionSessionEvent::StepFailed { message, .. }
            | PublicActionSessionEvent::AttemptRejected { message, .. }
            | PublicActionSessionEvent::HardwareApprovalFailed { message } => {
                self.discard_active_trezor_session_if_stale(&message, cx);
                self.walletconnect
                    .fail_request_approval_progress(request_key, generation, message);
            }
            PublicActionSessionEvent::HardwareApprovalStarted => {
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::ApproveOnDevice,
                    PublicActionStepStatus::Pending,
                    None,
                    None,
                );
            }
            PublicActionSessionEvent::HardwareApprovalCompleted => {
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::ApproveOnDevice,
                    PublicActionStepStatus::Done,
                    None,
                    None,
                );
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::RespondToDapp,
                    PublicActionStepStatus::Pending,
                    None,
                    None,
                );
            }
            PublicActionSessionEvent::HardwareProfileSessionRefreshed { session } => {
                #[cfg(feature = "hardware")]
                self.refresh_active_hardware_profile_session(session, cx);
                #[cfg(not(feature = "hardware"))]
                let _ = session;
            }
            PublicActionSessionEvent::FeeAuthorizationRequired { .. } => {}
        }
        cx.notify();
    }
}
