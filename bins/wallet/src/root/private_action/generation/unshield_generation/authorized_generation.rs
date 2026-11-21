use super::*;

impl WalletRoot {
    pub(in crate::root) fn generate_unshield_calldata_authorized(
        &mut self,
        key: UnshieldAssetKey,
        spend_authorization: DesktopPrivateSpendAuthorization,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        fee_review: Option<&PrivateFeeReview>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.generate_unshield_calldata_authorized_with_gas_password(
            key,
            spend_authorization,
            None,
            authorization_limit,
            fee_review,
            window,
            cx,
        );
    }

    pub(in crate::root) fn generate_unshield_calldata_authorized_with_gas_password(
        &mut self,
        key: UnshieldAssetKey,
        spend_authorization: DesktopPrivateSpendAuthorization,
        gas_payer_password: Option<Zeroizing<String>>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        fee_review: Option<&PrivateFeeReview>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a `delivery:` mode's form is held, so approving must prove and send
        // nothing.
        #[cfg(debug_assertions)]
        {
            if crate::root::private_swap::ui_fixture_holds_delivery() {
                self.set_unshield_form_error(key, "The UI fixture's form sends nothing.", cx);
                return;
            }
        }
        let Ok(resolved_chain) = self.effective_chain_configs.railgun(key.chain_id).cloned() else {
            return;
        };
        let Some(mut draft) = self.unshield_spend_draft(key, cx) else {
            return;
        };
        draft.sponsored_authorization_limit = authorization_limit;
        let requires_executor = Self::unshield_requires_executor(&draft);
        if (requires_executor && draft.executor_review.is_none())
            || draft
                .executor_review
                .as_ref()
                .is_some_and(|review| !requires_executor || !review.matches(&draft))
        {
            self.set_unshield_form_error(
                key,
                "Refresh and approve the stealth-account quote before signing.",
                cx,
            );
            return;
        }
        let UnshieldSpendDraft {
            custom_fee_amount,
            executor_review,
            asset,
            unwrap,
            delivery_mode,
            broadcaster_choice: _,
            cost_estimate,
            fee_token,
            self_broadcast_gas_fee,
            self_broadcast_funding,
            sponsored_incentive,
            sponsored_authorization_limit,
            sponsored_authorization_display: _,
            self_broadcast_initial_gas_fee,
            fee_mode,
            view_session,
            vault_store,
            session,
            recipient,
            amount,
            native_top_up,
            self_broadcast_public_account_uuid,
            self_broadcast_gas_payer_display,
            self_broadcast_gas_payer_source,
            fee_rows: _,
            fee_policy,
            favorites_only_broadcasters,
        } = draft;
        let input_pin = self
            .unshield_forms
            .get(&key)
            .and_then(|form| form.input_pin.clone());
        if input_pin.is_some()
            && (delivery_mode != DeliveryMode::PublicBroadcaster || executor_review.is_some())
        {
            self.set_unshield_form_error(key, PINNED_FORM_LEFT_BROADCASTER, cx);
            return;
        }
        // Executor reviews carry their own approved maximum; automatic fees use the reviewed one.
        let maximum_private_fee = executor_review
            .as_ref()
            .and_then(|review| review.maximum_private_fee())
            .or_else(|| {
                fee_review
                    .filter(|_| custom_fee_amount.is_none())
                    .map(|review| review.bounds.maximum_fee())
            });
        let public_broadcaster_quote = if delivery_mode == DeliveryMode::PublicBroadcaster {
            let quote = match executor_review.as_ref().map(|review| &review.quote) {
                Some(
                    crate::root::private_action::executor::ExecutorUnshieldQuote::Broadcaster(
                        quote,
                    ),
                ) => Some((**quote).clone()),
                _ => cost_estimate
                    .clone()
                    .filter(|_| custom_fee_amount.is_some() || maximum_private_fee.is_some()),
            };
            let Some(quote) = quote else {
                self.set_unshield_form_error(
                    key,
                    "Refresh the current fee estimate before submitting this review.",
                    cx,
                );
                return;
            };
            Some(quote)
        } else {
            None
        };
        let transaction_tracking = if delivery_mode == DeliveryMode::SelfBroadcast {
            let tracking = self_broadcast_public_account_uuid
                .as_deref()
                .ok_or_else(|| "Select a public gas payer before submitting".to_owned())
                .and_then(|account| {
                    self.public_transaction_tracking_context(asset.chain_id, account)
                });
            match tracking {
                Ok(context) => Some(context),
                Err(error) => {
                    self.set_unshield_form_error(key, error, cx);
                    return;
                }
            }
        } else {
            None
        };
        let native_top_up_request = native_top_up_request_from_plan(native_top_up.as_ref());

        let self_broadcast_gas_fee = executor_review
            .as_ref()
            .and_then(|review| match review.quote {
                crate::root::private_action::executor::ExecutorUnshieldQuote::SelfBroadcast {
                    gas_fee,
                    ..
                } => Some(gas_fee),
                crate::root::private_action::executor::ExecutorUnshieldQuote::Broadcaster(_) => {
                    None
                }
            })
            .unwrap_or_else(|| {
                sponsored_authorization_limit.map_or(self_broadcast_gas_fee, |limit| {
                    SelfBroadcastGasFeeSelection::Custom {
                        max_fee_per_gas: limit.max_fee_per_gas,
                        max_priority_fee_per_gas: limit.max_priority_fee_per_gas,
                    }
                })
            });
        let self_broadcast_initial_gas_fee =
            self_broadcast_initial_gas_values(&self_broadcast_gas_fee, None)
                .or(self_broadcast_initial_gas_fee);
        let self_broadcast_vault_password = if delivery_mode == DeliveryMode::SelfBroadcast {
            if self_broadcast_gas_payer_source == Some(PublicAccountSource::HardwareDerived)
                || matches!(&spend_authorization, DesktopPrivateSpendAuthorization::HardwareExecutor(hardware)
                    if self_broadcast_public_account_uuid.as_deref().is_some_and(|uuid| hardware.is_gas_payment_for(uuid)))
            {
                None
            } else if let Some(password) = gas_payer_password {
                Some(password)
            } else {
                match &spend_authorization {
                    DesktopPrivateSpendAuthorization::VaultPassword(password) => {
                        Some(password.clone())
                    }
                    DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed {
                        password, ..
                    } => Some(password.clone()),
                    DesktopPrivateSpendAuthorization::HardwareExecutor(hardware) => {
                        match self_broadcast_public_account_uuid
                            .as_deref()
                            .ok_or_else(|| eyre::eyre!("Select the reviewed gas payer"))
                            .and_then(|uuid| hardware.gas_payer_password(uuid))
                        {
                            Ok(password) => Some(password),
                            Err(error) => {
                                self.set_unshield_form_error(key, error.to_string(), cx);
                                return;
                            }
                        }
                    }
                    DesktopPrivateSpendAuthorization::PreauthorizedSigner(_)
                    | DesktopPrivateSpendAuthorization::HardwarePublic => {
                        self.set_unshield_form_error(
                            key,
                            SOFTWARE_SELF_BROADCAST_GAS_PAYER_PASSWORD_REQUIRED,
                            cx,
                        );
                        return;
                    }
                }
            }
        } else {
            None
        };
        let protected_seed_session = spend_authorization.protected_seed_session();

        if let Some(execution) = self
            .unshield_forms
            .get(&key)
            .and_then(|form| form.gateway_execution.as_ref())
            && !execution.start()
        {
            return;
        }
        self.unshield_generation_seq = self.unshield_generation_seq.wrapping_add(1);
        let generation_id = self.unshield_generation_seq;
        if let Some(execution) = self
            .unshield_forms
            .get(&key)
            .and_then(|form| form.gateway_execution.as_ref())
        {
            execution.bind_generation(generation_id);
        }
        let gateway_origin = self
            .unshield_forms
            .get(&key)
            .and_then(|form| form.gateway_execution.as_ref()?.origin());
        let (progress_tx, progress_rx) = watch::channel(TransactionGenerationStage::default());
        let sponsored = delivery_mode == DeliveryMode::SelfBroadcast
            && self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship;
        let (sponsored_command_tx, sponsored_command_rx) = if sponsored {
            let (tx, rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let (self_broadcast_command_tx, self_broadcast_command_rx) =
            if delivery_mode == DeliveryMode::SelfBroadcast && !sponsored {
                let (tx, rx) = mpsc::unbounded_channel();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
        let (self_broadcast_event_tx, self_broadcast_event_rx) =
            if delivery_mode == DeliveryMode::SelfBroadcast {
                let (tx, rx) = mpsc::unbounded_channel();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
        if let Some(form) = self.unshield_forms.get_mut(&key) {
            form.generation_id = generation_id;
            form.generating = true;
            form.generation_stage = TransactionGenerationStage::default();
            form.cost_estimate_pending = false;
            form.estimating_cost = false;
            form.estimate_id = 0;
            form.self_broadcast_estimated_native_gas_cost = None;
            form.error = None;
            form.result = None;
        }
        cx.notify();

        let recipient_output = native_top_up.as_ref().map(|top_up| {
            let recipient_amount = native_top_up_primary_recipient_amount_for_fee_mode(
                asset.token,
                top_up.wrapped_native_token,
                amount,
                fee_mode,
                top_up.native_amount,
            );
            private_amount_label(recipient_amount, &asset, false)
        });

        let self_broadcast_requires_device_approval =
            self_broadcast_gas_payer_source == Some(PublicAccountSource::HardwareDerived);
        let mut route_tx = None;
        let mut payload_inputs_tx = None;
        match delivery_mode {
            DeliveryMode::PublicBroadcaster => {
                route_tx = Some(self.start_private_broadcaster_progress(
                    DeliveryFormKind::Unshield,
                    key,
                    generation_id,
                    asset.label.clone(),
                    asset.icon_path.clone(),
                    recipient.to_checksum(None),
                    cost_estimate,
                    self.public_broadcaster_response_timeout,
                    cx,
                ));
                let (tx, rx) = tokio::sync::oneshot::channel();
                payload_inputs_tx = Some(tx);
                if let Some(progress) = self.private_broadcaster_progress.as_mut() {
                    progress.payload_inputs_rx = Some(rx);
                }
            }
            DeliveryMode::SelfBroadcast => {
                let signer =
                    self_broadcast_gas_payer_display.expect("self-broadcast signer was validated");
                if sponsored {
                    self.start_private_self_broadcast_progress(
                        DeliveryFormKind::Unshield,
                        key,
                        generation_id,
                        asset.label.clone(),
                        asset.icon_path.clone(),
                        recipient.to_checksum(None),
                        recipient_output,
                        signer,
                        self_broadcast_requires_device_approval,
                        None,
                        None,
                    );
                    if let Some(progress) = self.private_broadcaster_progress.as_mut() {
                        progress.sponsored_funding = true;
                        progress.sponsored_self_broadcast_command_tx = sponsored_command_tx;
                    }
                } else {
                    self.start_private_self_broadcast_progress(
                        DeliveryFormKind::Unshield,
                        key,
                        generation_id,
                        asset.label.clone(),
                        asset.icon_path.clone(),
                        recipient.to_checksum(None),
                        recipient_output,
                        signer,
                        self_broadcast_requires_device_approval,
                        self_broadcast_command_tx,
                        self_broadcast_initial_gas_fee,
                    );
                }
            }
            DeliveryMode::ManualCalldata => {}
        }

        if let Some(review) = &executor_review
            && let Some(progress) = &mut self.private_broadcaster_progress
        {
            progress.stealth_account =
                Some(crate::root::stealth_accounts::StealthAccountTarget::new(
                    &session,
                    review.prepared.operation(),
                ));
        }

        let http = self.http.clone();
        let waku = if delivery_mode == DeliveryMode::PublicBroadcaster {
            let Some(waku) = self.public_broadcaster_waku() else {
                self.set_unshield_form_error(
                    key,
                    "Public broadcaster delivery is unavailable. Ensure the vault is unlocked and Waku connectivity is active, then try again",
                    cx,
                );
                self.clear_private_broadcaster_progress_state();
                return;
            };
            Some(waku)
        } else {
            None
        };
        let chain_id = asset.chain_id;
        let token = asset.token;
        let join = match delivery_mode {
            DeliveryMode::ManualCalldata => {
                let request = DesktopUnshieldCalldataRequest {
                    chain_id,
                    effective_chain: resolved_chain,
                    view_session,
                    session,
                    vault_store,
                    spend_authorization,
                    token,
                    fee_token,
                    amount,
                    fee_mode,
                    recipient,
                    unwrap,
                    native_top_up: native_top_up_request,
                    verify_proof: true,
                    progress_tx: Some(progress_tx),
                };
                self.spawn_public_transaction_submission(chain_id, async move {
                    prepare_desktop_unshield_calldata(request, &http)
                        .await
                        .map(|result| UnshieldResult::Manual(Box::new(result)))
                })
            }
            DeliveryMode::PublicBroadcaster => {
                let request = DesktopUnshieldPublicBroadcasterRequest {
                    custom_fee_amount,
                    input_pin,
                    executor: executor_review
                        .as_ref()
                        .map(|review| Arc::clone(&review.prepared)),
                    maximum_private_fee,
                    chain_id,
                    effective_chain: resolved_chain,
                    view_session,
                    session,
                    vault_store,
                    spend_authorization,
                    token,
                    fee_token,
                    amount,
                    recipient,
                    unwrap,
                    native_top_up: native_top_up_request,
                    gateway_origin,
                    verify_proof: true,
                    quote: public_broadcaster_quote
                        .expect("public broadcaster quote was validated"),
                    fee_mode,
                    fee_policy,
                    trust_filter: self.public_broadcaster_trust_filter(favorites_only_broadcasters),
                    anchor_cache: Some(Arc::clone(&self.public_broadcaster_anchor_cache)),
                    waku: waku.expect("active Waku delivery client was validated"),
                    response_timeout: self.public_broadcaster_response_timeout,
                    republish_interval: self.public_broadcaster_republish_interval,
                    progress_tx: Some(progress_tx),
                    route_tx,
                    payload_inputs_tx,
                };
                self.spawn_public_transaction_submission(chain_id, async move {
                    Box::pin(submit_desktop_unshield_public_broadcaster(request, &http))
                        .await
                        .map(|result| UnshieldResult::PublicBroadcaster(Box::new(result)))
                })
            }
            DeliveryMode::SelfBroadcast => {
                #[cfg(feature = "hardware")]
                let trezor_pin_matrix_provider = view_session
                    .hardware_profile_session()
                    .filter(|session| {
                        session.device_kind == wallet_ops::hardware::HardwareDeviceKind::Trezor
                    })
                    .map(|_| self.trezor_pin_matrix_provider_for_operation(window, cx));
                #[cfg(not(feature = "hardware"))]
                let trezor_pin_matrix_provider = None;
                let public_account_uuid = self_broadcast_public_account_uuid
                    .expect("self-broadcast signer was validated");
                if sponsored {
                    let Some(effective_chain) = self.effective_chain_configs.get(chain_id).cloned()
                    else {
                        self.set_unshield_form_error(
                            key,
                            "Selected chain settings are unavailable",
                            cx,
                        );
                        self.clear_private_broadcaster_progress_state();
                        return;
                    };
                    let request = DesktopSponsoredUnshieldSelfBroadcastRequest {
                        transaction_tracking,
                        chain_id,
                        effective_chain,
                        view_session,
                        session,
                        vault_store,
                        spend_authorization,
                        vault_password: self_broadcast_vault_password,
                        protected_software_seed_session: protected_seed_session,
                        trezor_pin_matrix_provider,
                        public_account_uuid,
                        token,
                        amount,
                        fee_mode,
                        recipient,
                        unwrap,
                        native_top_up: native_top_up_request,
                        gateway_origin,
                        verify_proof: true,
                        gas_fee: self_broadcast_gas_fee,
                        incentive: sponsored_incentive,
                        authorization_limit: sponsored_authorization_limit
                            .expect("sponsored authorization limit was created"),
                        progress_tx: Some(progress_tx),
                        command_rx: sponsored_command_rx
                            .expect("sponsored command receiver was created"),
                        event_tx: self_broadcast_event_tx,
                    };
                    self.spawn_public_transaction_submission(chain_id, async move {
                        let client = http.operation_http_client().await?;
                        Box::pin(submit_desktop_sponsored_unshield_self_broadcast(
                            request, &http, &client,
                        ))
                        .await
                        .map(|result| UnshieldResult::Sponsored(Box::new(result)))
                    })
                } else {
                    let request = DesktopUnshieldSelfBroadcastRequest {
                        executor: executor_review
                            .as_ref()
                            .map(|review| Arc::clone(&review.prepared)),
                        executor_maximum_gas: executor_review
                            .as_ref()
                            .and_then(|review| review.maximum_gas()),
                        transaction_tracking,
                        chain_id,
                        effective_chain: resolved_chain,
                        view_session,
                        session,
                        vault_store,
                        spend_authorization,
                        vault_password: self_broadcast_vault_password,
                        protected_software_seed_session: protected_seed_session,
                        trezor_pin_matrix_provider,
                        public_account_uuid,
                        token,
                        fee_token,
                        amount,
                        fee_mode,
                        recipient,
                        unwrap,
                        native_top_up: native_top_up_request,
                        gateway_origin,
                        verify_proof: true,
                        gas_fee: self_broadcast_gas_fee,
                        progress_tx: Some(progress_tx),
                        command_rx: self_broadcast_command_rx,
                        event_tx: self_broadcast_event_tx,
                    };
                    self.spawn_public_transaction_submission(chain_id, async move {
                        let client = http.operation_http_client().await?;
                        Box::pin(submit_desktop_unshield_self_broadcast(
                            request, &http, &client,
                        ))
                        .await
                        .map(|result| UnshieldResult::SelfBroadcast(Box::new(result)))
                    })
                }
            }
        };
        if delivery_mode != DeliveryMode::ManualCalldata
            && let Some(abort_handle) = join.abort_handle()
        {
            self.set_private_broadcaster_task_abort_handle(
                DeliveryFormKind::Unshield,
                key,
                generation_id,
                abort_handle,
            );
        }
        let terminal_progress_rx = progress_rx.clone();
        Self::watch_unshield_generation_stage(key, generation_id, progress_rx, window, cx);
        if let Some(event_rx) = self_broadcast_event_rx {
            Self::watch_self_broadcast_session_events(
                DeliveryFormKind::Unshield,
                key,
                generation_id,
                event_rx,
                window,
                cx,
            );
        }
        cx.spawn(async move |this, cx| {
            let result = join.await.unwrap_or_else(|error| {
                Err(eyre::eyre!("unshield generation task failed: {error}"))
            });
            let final_stage = *terminal_progress_rx.borrow();
            let _ = this.update(cx, |root, cx| {
                let mut progress_result = None;
                let mut self_broadcast_progress_result = None;
                let mut sponsored_progress_outcome = None;
                let mut progress_error = None;
                let mut clear_spend_authorization = false;
                let mut refresh_public_account = None;
                let visible_public_account =
                    root.unshield_delivery_visible_public_account(chain_id, recipient);
                {
                    let Some(form) = root.unshield_forms.get_mut(&key) else {
                        return;
                    };
                    if form.asset.chain_id != chain_id || form.asset.token != token {
                        return;
                    }
                    if form.generation_id != generation_id || !form.generating {
                        return;
                    }
                    form.generating = false;
                    match result {
                        Ok(result) => {
                            refresh_public_account = match &result {
                                UnshieldResult::PublicBroadcaster(result)
                                    if matches!(
                                        result.result,
                                        PublicBroadcasterResultKind::Submitted { .. }
                                    ) =>
                                {
                                    visible_public_account
                                }
                                UnshieldResult::SelfBroadcast(result)
                                    if result
                                        .tx
                                        .receipt()
                                        .is_some_and(|receipt| receipt.status) =>
                                {
                                    visible_public_account
                                }
                                UnshieldResult::Sponsored(result)
                                    if matches!(
                                        result.outcome,
                                        SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(
                                            ref receipt
                                        ) if receipt.status
                                    ) =>
                                {
                                    visible_public_account
                                }
                                _ => None,
                            };
                            if let UnshieldResult::PublicBroadcaster(result) = &result {
                                progress_result = Some((**result).clone());
                                form.input_pin =
                                    input_pin_after_attempt(form.input_pin.take(), result);
                            }
                            if let UnshieldResult::SelfBroadcast(result) = &result {
                                form.self_broadcast_estimated_native_gas_cost =
                                    Some(result.estimated_native_gas_cost);
                                self_broadcast_progress_result = Some((**result).clone());
                            }
                            if let UnshieldResult::Sponsored(result) = &result {
                                sponsored_progress_outcome = Some(result.outcome.clone());
                            }
                            // The next transaction gets a fresh random broadcaster, not this one.
                            form.shown_broadcaster = None;
                            form.error = None;
                            form.result = Some(result);
                        }
                        Err(error) => {
                            let (message, pin_conflict) = submission_error_message(&error);
                            if is_spend_authorization_failure_error(&message) {
                                clear_spend_authorization = true;
                            }
                            if pin_conflict {
                                form.input_pin_conflict = true;
                            }
                            progress_error = Some((message.clone(), pin_conflict));
                            // See the Send handler: the estimate stays valid for "Continue anyway…".
                            if !pin_conflict
                                && form_error_clears_public_broadcaster_cost_estimate(
                                    DeliveryFormKind::Unshield,
                                    message.as_str(),
                                )
                            {
                                form.cost_estimate = None;
                            }
                            form.result = None;
                            form.error = Some(Arc::from(message));
                        }
                    }
                }
                if clear_spend_authorization {
                    root.clear_spend_authorization(cx);
                }
                if progress_error.is_some()
                    || matches!(
                        sponsored_progress_outcome.as_ref(),
                        Some(SponsoredSelfBroadcastSessionOutcome::Stopped { .. })
                    )
                {
                    root.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
                }
                if let Some(account) = refresh_public_account {
                    root.http.rpc_broker().invalidate_block(chain_id);
                    root.schedule_account_public_balance_refresh(chain_id, [account], None, cx);
                }
                if let Some(result) = progress_result {
                    root.finish_private_broadcaster_progress(
                        DeliveryFormKind::Unshield,
                        key,
                        generation_id,
                        final_stage,
                        result,
                        cx,
                    );
                }
                if let Some(result) = self_broadcast_progress_result {
                    root.finish_private_self_broadcast_progress(
                        DeliveryFormKind::Unshield,
                        key,
                        generation_id,
                        final_stage,
                        result,
                        cx,
                    );
                }
                if let Some(outcome) = sponsored_progress_outcome {
                    root.finish_private_sponsored_self_broadcast_progress(
                        DeliveryFormKind::Unshield,
                        key,
                        generation_id,
                        final_stage,
                        outcome,
                        cx,
                    );
                }
                if let Some((message, pin_conflict)) = progress_error {
                    root.fail_private_broadcaster_progress(
                        DeliveryFormKind::Unshield,
                        key,
                        generation_id,
                        final_stage,
                        message,
                        pin_conflict,
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root) fn watch_unshield_generation_stage(
        key: UnshieldAssetKey,
        generation_id: u64,
        mut progress_rx: watch::Receiver<TransactionGenerationStage>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while progress_rx.changed().await.is_ok() {
                let stage = *progress_rx.borrow_and_update();
                if this
                    .update_in(cx, |root, window, cx| {
                        let Some(form) = root.unshield_forms.get_mut(&key) else {
                            if root.update_private_broadcaster_progress_stage(
                                DeliveryFormKind::Unshield,
                                key,
                                generation_id,
                                stage,
                                cx,
                            ) {
                                root.show_private_broadcaster_progress_dialog(window, cx);
                            }
                            return;
                        };
                        if form.generation_id != generation_id || !form.generating {
                            if root.update_private_broadcaster_progress_stage(
                                DeliveryFormKind::Unshield,
                                key,
                                generation_id,
                                stage,
                                cx,
                            ) {
                                root.show_private_broadcaster_progress_dialog(window, cx);
                            }
                            return;
                        }
                        form.generation_stage = stage;
                        if root.update_private_broadcaster_progress_stage(
                            DeliveryFormKind::Unshield,
                            key,
                            generation_id,
                            stage,
                            cx,
                        ) {
                            root.show_private_broadcaster_progress_dialog(window, cx);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}
