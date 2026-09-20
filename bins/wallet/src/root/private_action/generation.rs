use super::*;
use wallet_ops::gateway::GatewayDraftExecution;
use zeroize::Zeroizing;

const SOFTWARE_SELF_BROADCAST_GAS_PAYER_PASSWORD_REQUIRED: &str = concat!(
    "The selected gas payer is a software Public account, so self-broadcast requires the vault ",
    "password. Choose a hardware Public account, manual calldata, or public broadcaster delivery."
);

fn gateway_private_estimate_is_ready(
    execution: Option<&GatewayDraftExecution>,
    estimate_is_current: bool,
    estimated_at: Option<std::time::Instant>,
    age_limited: bool,
) -> bool {
    execution.is_none_or(|execution| {
        // Quote age gates entry into approval. Execution recalculates fees after approval,
        // while an invalidated estimate must still stop submission.
        estimate_is_current
            && (!age_limited
                || execution.has_review_approval()
                || estimated_at.is_some_and(|at| at.elapsed() <= Duration::from_secs(30)))
    })
}

pub(in crate::root) fn sponsored_authorization_display(
    chain_id: u64,
    limit: SponsoredAuthorizationLimit,
    token_registry: &EffectiveTokenRegistry,
    anchor_cache: &TokenAnchorRateCache,
) -> SponsoredAuthorizationDisplay {
    let maximum_payment = limit
        .maximum_payment()
        .expect("sponsored authorization limit was validated when quoted");
    let amount_label = |amount| {
        let token_value = format_token_amount_ceiling_for_display(
            chain_id,
            limit.wrapped_native_token,
            amount,
            Some(token_registry),
        );
        let usd_micro_value =
            anchor_cache.cached_token_usd_micro_value(chain_id, limit.wrapped_native_token, amount);
        format!(
            "Up to {}",
            format_value_with_usd_label(
                token_value,
                amount,
                token_display_metadata(
                    Some(token_registry),
                    chain_id,
                    &limit.wrapped_native_token,
                )
                .map(|metadata| metadata.decimals),
                usd_micro_value,
                false,
            )
        )
    };
    SponsoredAuthorizationDisplay {
        gross_wrapped_native_spend: amount_label(maximum_payment.gross_wrapped_native_spend),
        max_fee_per_gas: format!("{} gwei", format_gwei(limit.max_fee_per_gas)),
        max_priority_fee_per_gas: format!("{} gwei", format_gwei(limit.max_priority_fee_per_gas)),
    }
}

impl WalletRoot {
    pub(in crate::root) fn generate_send_calldata_from_form(
        &mut self,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(draft) = self.send_spend_draft(key, cx) else {
            return;
        };
        let requires_gas_payer_password = self.selected_wallet_source().is_hardware_derived()
            && self_broadcast_requires_software_gas_payer_password(
                draft.delivery_mode,
                draft.self_broadcast_gas_payer_source,
            );
        let intent = if requires_gas_payer_password {
            SpendAuthorizationIntent::PrivateSendSelfBroadcastGasPassword(
                key,
                draft.sponsored_authorization_limit,
                self.send_forms
                    .get(&key)
                    .and_then(|form| form.gateway_execution.clone()),
            )
        } else {
            SpendAuthorizationIntent::PrivateSend(
                key,
                draft.sponsored_authorization_limit,
                self.send_forms
                    .get(&key)
                    .and_then(|form| form.gateway_execution.clone()),
                draft
                    .custom_fee_amount
                    .map(|amount| (draft.fee_token, amount)),
            )
        };
        let summary = if requires_gas_payer_password {
            private_send_gas_payer_authorization_summary(&draft)
        } else {
            private_send_authorization_summary(&draft)
        };
        let summary = if let Some(amount) = draft.custom_fee_amount {
            summary.with_custom_transaction_fee(super::fee_editor::custom_fee_label(
                draft.asset.chain_id,
                draft.fee_token,
                amount,
                &self.effective_token_registry,
            ))
        } else {
            summary
        };
        self.request_spend_authorization(intent, summary, window, cx);
    }

    pub(in crate::root) fn send_spend_draft(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) -> Option<SendSpendDraft> {
        let delivery_mode = {
            let form = self.send_forms.get(&key)?;
            if form.generating {
                return None;
            }
            form.delivery_mode
        };
        self.ensure_waku_for_delivery(delivery_mode, cx);
        let form = self.send_forms.get(&key)?;
        let self_broadcast = delivery_mode == DeliveryMode::SelfBroadcast;
        let estimate_is_current = if self_broadcast {
            matches!(
                form.sponsored_funding_estimate,
                Some(
                    SponsoredFundingEstimateState::Ready(_)
                        | SponsoredFundingEstimateState::PublicBalanceReady(_)
                )
            ) && self.gateway_self_broadcast_review_current(form.gateway_execution.as_ref(), cx)
        } else {
            form.cost_estimate.is_some() && !form.cost_estimate_pending && !form.estimating_cost
        };
        if (form.custom_fee_amount.is_some() && !estimate_is_current)
            || !gateway_private_estimate_is_ready(
                form.gateway_execution.as_ref(),
                estimate_is_current,
                form.gateway_estimated_at,
                !(self_broadcast
                    && form.self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship),
            )
        {
            self.set_send_form_error(
                key,
                "Refresh the current fee estimate before submitting this review.",
                cx,
            );
            return None;
        }
        let asset = form.asset.clone();
        let recipient_input = form.recipient_input.clone();
        let amount_input = form.amount_input.clone();
        let broadcaster_choice = form.broadcaster_choice.clone();
        let cost_estimate = form.cost_estimate.clone();
        let fee_token = form.selected_fee_token;
        let custom_fee_amount = form.custom_fee_amount;
        let self_broadcast_funding = effective_delivery_funding_mode(
            delivery_mode,
            self.effective_chain_configs.get(asset.chain_id),
            form.self_broadcast_funding,
        );
        let sponsored_incentive = if delivery_mode == DeliveryMode::SelfBroadcast
            && self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
        {
            match sponsored_incentive_from_text(
                form.sponsored_incentive,
                form.sponsored_custom_incentive_input
                    .read(cx)
                    .value()
                    .as_ref(),
            ) {
                Ok(incentive) => incentive,
                Err(error) => {
                    self.set_send_form_error(key, error.to_string(), cx);
                    return None;
                }
            }
        } else {
            form.sponsored_incentive
        };
        let self_broadcast_gas_payer_uuid = form.self_broadcast_gas_payer_uuid.clone();
        let self_broadcast_gas_fee = if delivery_mode == DeliveryMode::SelfBroadcast {
            match form.self_broadcast_gas_fee.selection(cx) {
                Ok(selection) => selection,
                Err(error) => {
                    self.set_send_form_error(key, error, cx);
                    return None;
                }
            }
        } else {
            SelfBroadcastGasFeeSelection::Auto
        };
        let self_broadcast_initial_gas_fee = if delivery_mode == DeliveryMode::SelfBroadcast {
            self_broadcast_initial_gas_values(
                &self_broadcast_gas_fee,
                form.self_broadcast_gas_fee.quote,
            )
        } else {
            None
        };
        let fee_mode = effective_fee_handling_mode(
            DeliveryFormKind::Send,
            asset.token,
            fee_token,
            form.fee_mode,
        );
        let allow_suspicious_broadcasters = form.allow_suspicious_broadcasters;
        let favorites_only_broadcasters = form.favorites_only_broadcasters;
        if delivery_mode == DeliveryMode::SelfBroadcast
            && self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
            && let Some(reason) = sponsored_self_broadcast_availability_reason(
                self.effective_chain_configs.get(asset.chain_id),
            )
        {
            self.set_send_form_error(key, reason, cx);
            return None;
        }

        let Some(view_session) = self.view_session.clone() else {
            self.set_send_form_error(key, "Unlock the wallet vault before sending", cx);
            return None;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.set_send_form_error(key, "Wallet vault storage is unavailable", cx);
            return None;
        };
        let Some(ChainUtxoState::Ready { session, .. }) = self.chain_states.get(&asset.chain_id)
        else {
            self.set_send_form_error(
                key,
                "You can prepare this form now, but generation is available after wallet sync finishes",
                cx,
            );
            return None;
        };
        let session = Arc::clone(session);
        if asset.max_batched.is_zero() {
            self.set_send_form_error(
                key,
                "No POI-verified private notes are spendable in a batched send",
                cx,
            );
            return None;
        }

        let recipient_raw = recipient_input.read(cx).value().to_string();
        let recipient_data = match parse_railgun_recipient(recipient_raw.as_str()) {
            Ok(recipient) => recipient,
            Err(error) => {
                self.set_send_form_error(key, error.to_string(), cx);
                return None;
            }
        };
        let recipient = recipient_raw.trim().to_string();
        let amount_raw = amount_input.read(cx).value().to_string();
        let amount = match parse_send_amount(amount_raw.as_str(), asset.decimals) {
            Ok(amount) if !amount.is_zero() => amount,
            Ok(_) => {
                self.set_send_form_error(key, "Enter an amount greater than zero", cx);
                return None;
            }
            Err(error) => {
                self.set_send_form_error(key, error.to_string(), cx);
                return None;
            }
        };
        if amount > asset.max_batched {
            self.set_send_form_error(
                key,
                format!(
                    "Amount exceeds max POI-verified batched transaction: {}",
                    format_send_amount_input(asset.max_batched, asset.decimals)
                ),
                cx,
            );
            return None;
        }

        let (
            self_broadcast_public_account_uuid,
            self_broadcast_gas_payer_display,
            self_broadcast_gas_payer_source,
            self_broadcast_gas_payer_address,
        ) = if delivery_mode == DeliveryMode::SelfBroadcast {
            let Some(uuid) = self_broadcast_gas_payer_uuid else {
                self.set_send_form_error(key, "Choose a Public transaction signer", cx);
                return None;
            };
            let Some(account) = self.selected_self_broadcast_gas_payer_account(Some(uuid.as_ref()))
            else {
                self.set_send_form_error(key, "Choose an active Public transaction signer", cx);
                return None;
            };
            let gas_payer_display = public_account_display_label(account).map_or_else(
                || short_address(&account.address),
                |label| format!("{label} · {}", short_address(&account.address)),
            );
            (
                Some(uuid.to_string()),
                Some(gas_payer_display),
                Some(account.source),
                Some(account.address),
            )
        } else {
            (None, None, None, None)
        };
        let signer_native_balance_snapshot =
            self_broadcast_public_account_uuid
                .as_deref()
                .map_or(U256::ZERO, |uuid| {
                    self_broadcast_native_balance_amount(
                        self.public_balance_snapshot.as_deref(),
                        asset.chain_id,
                        uuid,
                    )
                });

        let sponsored_authorization_limit = if self_broadcast_funding
            == SelfBroadcastFundingMode::PrivateSponsorship
        {
            let Some(effective_chain) = self.effective_chain_configs.get(asset.chain_id) else {
                self.set_send_form_error(key, "Selected chain settings are unavailable", cx);
                return None;
            };
            let Some((max_fee_per_gas, max_priority_fee_per_gas)) = self_broadcast_initial_gas_fee
            else {
                self.set_send_form_error(
                    key,
                    "Refresh self-broadcast gas fees before authorizing sponsorship",
                    cx,
                );
                return None;
            };
            match quote_sponsored_send_authorization_limit(
                asset.chain_id,
                effective_chain,
                &session.unspent_utxos(),
                asset.token,
                amount,
                &recipient_data,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                signer_native_balance_snapshot,
                sponsored_incentive,
                self_broadcast_gas_payer_address.expect("self-broadcast signer was validated"),
            ) {
                Ok(limit) => Some(limit),
                Err(error) => {
                    self.set_send_form_error(key, error.to_string(), cx);
                    return None;
                }
            }
        } else {
            None
        };
        let sponsored_authorization_display = sponsored_authorization_limit.map(|limit| {
            sponsored_authorization_display(
                asset.chain_id,
                limit,
                &self.effective_token_registry,
                &self.public_broadcaster_anchor_cache,
            )
        });

        let fee_rows = if delivery_mode == DeliveryMode::PublicBroadcaster {
            let rows = self.monitor_fee_rows();
            let policy = self.public_broadcaster_fee_policy(allow_suspicious_broadcasters);
            let public_broadcaster_selection = Self::public_broadcaster_submission_selection(
                &broadcaster_choice,
                cost_estimate.as_ref(),
            );
            let candidates = self.current_public_broadcaster_candidates(
                asset.chain_id,
                fee_token,
                false,
                false,
                favorites_only_broadcasters,
                policy,
            );
            let trust_filter = self.public_broadcaster_trust_filter(favorites_only_broadcasters);
            if let Err(error) = select_public_broadcaster_with_policy_and_trust(
                &candidates,
                &public_broadcaster_selection,
                policy,
                &trust_filter,
            ) {
                self.set_send_form_error(key, error.to_string(), cx);
                return None;
            }
            rows
        } else {
            Vec::new()
        };
        let fee_policy = self.public_broadcaster_fee_policy(allow_suspicious_broadcasters);

        Some(SendSpendDraft {
            custom_fee_amount,
            asset,
            delivery_mode,
            broadcaster_choice,
            cost_estimate,
            fee_token,
            self_broadcast_gas_fee,
            self_broadcast_funding,
            sponsored_incentive,
            sponsored_authorization_limit,
            sponsored_authorization_display,
            self_broadcast_initial_gas_fee,
            fee_mode,
            view_session,
            vault_store,
            session,
            recipient,
            amount,
            self_broadcast_public_account_uuid,
            self_broadcast_gas_payer_display,
            self_broadcast_gas_payer_source,
            fee_rows,
            fee_policy,
            favorites_only_broadcasters,
        })
    }

    pub(in crate::root) fn generate_send_calldata_authorized(
        &mut self,
        key: UnshieldAssetKey,
        spend_authorization: DesktopPrivateSpendAuthorization,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.generate_send_calldata_authorized_with_gas_password(
            key,
            spend_authorization,
            None,
            authorization_limit,
            window,
            cx,
        );
    }

    pub(in crate::root) fn generate_send_calldata_authorized_with_gas_password(
        &mut self,
        key: UnshieldAssetKey,
        spend_authorization: DesktopPrivateSpendAuthorization,
        gas_payer_password: Option<Zeroizing<String>>,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(resolved_chain) = self.effective_chain_configs.railgun(key.chain_id).cloned() else {
            return;
        };
        let Some(mut draft) = self.send_spend_draft(key, cx) else {
            return;
        };
        draft.sponsored_authorization_limit = authorization_limit;
        let SendSpendDraft {
            custom_fee_amount,
            asset,
            delivery_mode,
            broadcaster_choice,
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
            self_broadcast_public_account_uuid,
            self_broadcast_gas_payer_display,
            self_broadcast_gas_payer_source,
            fee_rows,
            fee_policy,
            favorites_only_broadcasters,
        } = draft;
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
                    self.set_send_form_error(key, error, cx);
                    return;
                }
            }
        } else {
            None
        };

        let self_broadcast_gas_fee =
            sponsored_authorization_limit.map_or(self_broadcast_gas_fee, |limit| {
                SelfBroadcastGasFeeSelection::Custom {
                    max_fee_per_gas: limit.max_fee_per_gas,
                    max_priority_fee_per_gas: limit.max_priority_fee_per_gas,
                }
            });
        let self_broadcast_vault_password = if delivery_mode == DeliveryMode::SelfBroadcast {
            if self_broadcast_gas_payer_source == Some(PublicAccountSource::HardwareDerived) {
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
                    DesktopPrivateSpendAuthorization::PreauthorizedSigner(_) => {
                        self.set_send_form_error(
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
            .send_forms
            .get(&key)
            .and_then(|form| form.gateway_execution.as_ref())
            && !execution.start()
        {
            return;
        }
        self.send_generation_seq = self.send_generation_seq.wrapping_add(1);
        let generation_id = self.send_generation_seq;
        if let Some(execution) = self
            .send_forms
            .get(&key)
            .and_then(|form| form.gateway_execution.as_ref())
        {
            execution.bind_generation(generation_id);
        }
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
        if let Some(form) = self.send_forms.get_mut(&key) {
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

        let self_broadcast_requires_device_approval =
            self_broadcast_gas_payer_source == Some(PublicAccountSource::HardwareDerived);
        match delivery_mode {
            DeliveryMode::PublicBroadcaster => {
                self.start_private_broadcaster_progress(
                    DeliveryFormKind::Send,
                    key,
                    generation_id,
                    asset.label.clone(),
                    asset.icon_path.clone(),
                    recipient.clone(),
                    cost_estimate.clone(),
                    self.public_broadcaster_response_timeout,
                    self.public_broadcaster_republish_interval,
                );
            }
            DeliveryMode::SelfBroadcast => {
                let signer =
                    self_broadcast_gas_payer_display.expect("self-broadcast signer was validated");
                if sponsored {
                    self.start_private_self_broadcast_progress(
                        DeliveryFormKind::Send,
                        key,
                        generation_id,
                        asset.label.clone(),
                        asset.icon_path.clone(),
                        recipient.clone(),
                        None,
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
                        DeliveryFormKind::Send,
                        key,
                        generation_id,
                        asset.label.clone(),
                        asset.icon_path.clone(),
                        recipient.clone(),
                        None,
                        signer,
                        self_broadcast_requires_device_approval,
                        self_broadcast_command_tx,
                        self_broadcast_initial_gas_fee,
                    );
                }
            }
            DeliveryMode::ManualCalldata => {}
        }

        let http = self.http.clone();
        let waku = if delivery_mode == DeliveryMode::PublicBroadcaster {
            let Some(waku) = self.active_waku() else {
                self.set_send_form_error(
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
                let request = DesktopSendCalldataRequest {
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
                    verify_proof: true,
                    progress_tx: Some(progress_tx),
                };
                self.spawn_public_transaction_submission(chain_id, async move {
                    prepare_desktop_send_calldata(request, &http)
                        .await
                        .map(SendResult::Manual)
                })
            }
            DeliveryMode::PublicBroadcaster => {
                let request = DesktopSendPublicBroadcasterRequest {
                    custom_fee_amount,
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
                    verify_proof: true,
                    fee_rows,
                    selection: Self::public_broadcaster_submission_selection(
                        &broadcaster_choice,
                        cost_estimate.as_ref(),
                    ),
                    fee_mode,
                    fee_policy,
                    trust_filter: self.public_broadcaster_trust_filter(favorites_only_broadcasters),
                    anchor_cache: Some(Arc::clone(&self.public_broadcaster_anchor_cache)),
                    waku: waku.expect("active Waku delivery client was validated"),
                    response_timeout: self.public_broadcaster_response_timeout,
                    republish_interval: self.public_broadcaster_republish_interval,
                    progress_tx: Some(progress_tx),
                };
                self.spawn_public_transaction_submission(chain_id, async move {
                    Box::pin(submit_desktop_send_public_broadcaster(request, &http))
                        .await
                        .map(|result| SendResult::PublicBroadcaster(Box::new(result)))
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
                        self.set_send_form_error(
                            key,
                            "Selected chain settings are unavailable",
                            cx,
                        );
                        self.clear_private_broadcaster_progress_state();
                        return;
                    };
                    let request = DesktopSponsoredSendSelfBroadcastRequest {
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
                        recipient,
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
                        Box::pin(submit_desktop_sponsored_send_self_broadcast(request, &http))
                            .await
                            .map(|result| SendResult::Sponsored(Box::new(result)))
                    })
                } else {
                    let request = DesktopSendSelfBroadcastRequest {
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
                        recipient,
                        verify_proof: true,
                        gas_fee: self_broadcast_gas_fee,
                        progress_tx: Some(progress_tx),
                        command_rx: self_broadcast_command_rx,
                        event_tx: self_broadcast_event_tx,
                    };
                    self.spawn_public_transaction_submission(chain_id, async move {
                        submit_desktop_send_self_broadcast(request, &http)
                            .await
                            .map(|result| SendResult::SelfBroadcast(Box::new(result)))
                    })
                }
            }
        };
        if delivery_mode != DeliveryMode::ManualCalldata
            && let Some(abort_handle) = join.abort_handle()
        {
            self.set_private_broadcaster_task_abort_handle(
                DeliveryFormKind::Send,
                key,
                generation_id,
                abort_handle,
            );
        }
        let terminal_progress_rx = progress_rx.clone();
        Self::watch_send_generation_stage(key, generation_id, progress_rx, window, cx);
        if let Some(event_rx) = self_broadcast_event_rx {
            Self::watch_self_broadcast_session_events(
                DeliveryFormKind::Send,
                key,
                generation_id,
                event_rx,
                window,
                cx,
            );
        }
        cx.spawn(async move |this, cx| {
            let result = join
                .await
                .unwrap_or_else(|error| Err(eyre::eyre!("send generation task failed: {error}")));
            let final_stage = *terminal_progress_rx.borrow();
            let _ = this.update(cx, |root, cx| {
                let mut progress_result = None;
                let mut self_broadcast_progress_result = None;
                let mut sponsored_progress_outcome = None;
                let mut progress_error = None;
                let mut clear_spend_authorization = false;
                {
                    let Some(form) = root.send_forms.get_mut(&key) else {
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
                            if let SendResult::PublicBroadcaster(result) = &result {
                                progress_result = Some((**result).clone());
                            }
                            if let SendResult::SelfBroadcast(result) = &result {
                                form.self_broadcast_estimated_native_gas_cost =
                                    Some(result.estimated_native_gas_cost);
                                self_broadcast_progress_result = Some((**result).clone());
                            }
                            if let SendResult::Sponsored(result) = &result {
                                sponsored_progress_outcome = Some(result.outcome.clone());
                            }
                            form.error = None;
                            form.result = Some(result);
                        }
                        Err(error) => {
                            let message = format_report_chain(&error);
                            if is_spend_authorization_failure_error(&message) {
                                clear_spend_authorization = true;
                            }
                            progress_error = Some(message.clone());
                            if form_error_clears_public_broadcaster_cost_estimate(
                                DeliveryFormKind::Send,
                                message.as_str(),
                            ) {
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
                    root.debounce_sponsored_funding_estimate(DeliveryFormKind::Send, key, cx);
                }
                if let Some(result) = progress_result {
                    root.finish_private_broadcaster_progress(
                        DeliveryFormKind::Send,
                        key,
                        generation_id,
                        final_stage,
                        result,
                        cx,
                    );
                }
                if let Some(result) = self_broadcast_progress_result {
                    root.finish_private_self_broadcast_progress(
                        DeliveryFormKind::Send,
                        key,
                        generation_id,
                        final_stage,
                        result,
                        cx,
                    );
                }
                if let Some(outcome) = sponsored_progress_outcome {
                    root.finish_private_sponsored_self_broadcast_progress(
                        DeliveryFormKind::Send,
                        key,
                        generation_id,
                        final_stage,
                        outcome,
                        cx,
                    );
                }
                if let Some(message) = progress_error {
                    root.fail_private_broadcaster_progress(
                        DeliveryFormKind::Send,
                        key,
                        generation_id,
                        final_stage,
                        message,
                        cx,
                    );
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root) fn watch_send_generation_stage(
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
                        let Some(form) = root.send_forms.get_mut(&key) else {
                            if root.update_private_broadcaster_progress_stage(
                                DeliveryFormKind::Send,
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
                                DeliveryFormKind::Send,
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
                            DeliveryFormKind::Send,
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

    pub(in crate::root) fn generate_unshield_calldata_from_form(
        &mut self,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(draft) = self.unshield_spend_draft(key, cx) else {
            return;
        };
        if self.unshield_requires_executor(&draft) {
            if draft.delivery_mode == DeliveryMode::ManualCalldata {
                self.set_unshield_form_error(key, "External-wallet export is unavailable for this stealth-account action. Select a supported delivery method.", cx);
                return;
            }
            self.request_executor_unshield_authorization(key, &draft, window, cx);
            return;
        }
        if let Some(form) = self.unshield_forms.get_mut(&key) {
            form.executor_review = None;
            form.executor_operation = None;
        }
        let requires_gas_payer_password = self.selected_wallet_source().is_hardware_derived()
            && self_broadcast_requires_software_gas_payer_password(
                draft.delivery_mode,
                draft.self_broadcast_gas_payer_source,
            );
        let intent = if requires_gas_payer_password {
            SpendAuthorizationIntent::PrivateUnshieldSelfBroadcastGasPassword(
                key,
                draft.sponsored_authorization_limit,
                self.unshield_forms
                    .get(&key)
                    .and_then(|form| form.gateway_execution.clone()),
            )
        } else {
            SpendAuthorizationIntent::PrivateUnshield(
                key,
                draft.sponsored_authorization_limit,
                self.unshield_forms
                    .get(&key)
                    .and_then(|form| form.gateway_execution.clone()),
                draft
                    .custom_fee_amount
                    .map(|amount| (draft.fee_token, amount)),
            )
        };
        let summary = if requires_gas_payer_password {
            private_unshield_gas_payer_authorization_summary(&draft)
        } else {
            private_unshield_authorization_summary(&draft)
        };
        let summary = if let Some(amount) = draft.custom_fee_amount {
            summary.with_custom_transaction_fee(super::fee_editor::custom_fee_label(
                draft.asset.chain_id,
                draft.fee_token,
                amount,
                &self.effective_token_registry,
            ))
        } else {
            summary
        };
        self.request_spend_authorization(intent, summary, window, cx);
    }

    pub(in crate::root) fn unshield_spend_draft(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) -> Option<UnshieldSpendDraft> {
        self.unshield_spend_draft_with_quote_admission(key, true, cx)
    }

    pub(in crate::root) fn unshield_spend_draft_for_prepared_review(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) -> Option<UnshieldSpendDraft> {
        self.unshield_spend_draft_with_quote_admission(key, false, cx)
    }

    fn unshield_spend_draft_with_quote_admission(
        &mut self,
        key: UnshieldAssetKey,
        require_current_quote: bool,
        cx: &mut Context<'_, Self>,
    ) -> Option<UnshieldSpendDraft> {
        self.refresh_unshield_native_top_up_state(key, cx);
        let delivery_mode = {
            let form = self.unshield_forms.get(&key)?;
            if form.generating {
                return None;
            }
            form.delivery_mode
        };
        self.ensure_waku_for_delivery(delivery_mode, cx);
        let form = self.unshield_forms.get(&key)?;
        let self_broadcast = delivery_mode == DeliveryMode::SelfBroadcast;
        let estimate_is_current = if self_broadcast {
            matches!(
                form.sponsored_funding_estimate,
                Some(
                    SponsoredFundingEstimateState::Ready(_)
                        | SponsoredFundingEstimateState::PublicBalanceReady(_)
                )
            ) && self.gateway_self_broadcast_review_current(form.gateway_execution.as_ref(), cx)
        } else {
            form.cost_estimate.is_some() && !form.cost_estimate_pending && !form.estimating_cost
        };
        if require_current_quote
            && ((form.custom_fee_amount.is_some() && !estimate_is_current)
                || !gateway_private_estimate_is_ready(
                    form.gateway_execution.as_ref(),
                    estimate_is_current,
                    form.gateway_estimated_at,
                    !(self_broadcast
                        && form.self_broadcast_funding
                            == SelfBroadcastFundingMode::PrivateSponsorship),
                ))
        {
            self.set_unshield_form_error(
                key,
                "Refresh the current fee estimate before submitting this review.",
                cx,
            );
            return None;
        }
        let asset = form.asset.clone();
        let unwrap = form.unwrap;
        let recipient_input = form.recipient_input.clone();
        let amount_input = form.amount_input.clone();
        let broadcaster_choice = form.broadcaster_choice.clone();
        let cost_estimate = form.cost_estimate.clone();
        let fee_token = form.selected_fee_token;
        let custom_fee_amount = form.custom_fee_amount;
        let self_broadcast_funding = effective_delivery_funding_mode(
            delivery_mode,
            self.effective_chain_configs.get(asset.chain_id),
            form.self_broadcast_funding,
        );
        let sponsored_incentive = if delivery_mode == DeliveryMode::SelfBroadcast
            && self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
        {
            match sponsored_incentive_from_text(
                form.sponsored_incentive,
                form.sponsored_custom_incentive_input
                    .read(cx)
                    .value()
                    .as_ref(),
            ) {
                Ok(incentive) => incentive,
                Err(error) => {
                    self.set_unshield_form_error(key, error.to_string(), cx);
                    return None;
                }
            }
        } else {
            form.sponsored_incentive
        };
        let self_broadcast_gas_payer_uuid = form.self_broadcast_gas_payer_uuid.clone();
        let self_broadcast_gas_fee = if delivery_mode == DeliveryMode::SelfBroadcast {
            match form.self_broadcast_gas_fee.selection(cx) {
                Ok(selection) => selection,
                Err(error) => {
                    self.set_unshield_form_error(key, error, cx);
                    return None;
                }
            }
        } else {
            SelfBroadcastGasFeeSelection::Auto
        };
        let self_broadcast_initial_gas_fee = if delivery_mode == DeliveryMode::SelfBroadcast {
            self_broadcast_initial_gas_values(
                &self_broadcast_gas_fee,
                form.self_broadcast_gas_fee.quote,
            )
        } else {
            None
        };
        let fee_mode = effective_fee_handling_mode(
            DeliveryFormKind::Unshield,
            asset.token,
            fee_token,
            form.fee_mode,
        );
        let allow_suspicious_broadcasters = form.allow_suspicious_broadcasters;
        let favorites_only_broadcasters = form.favorites_only_broadcasters;
        if delivery_mode == DeliveryMode::SelfBroadcast
            && self_broadcast_funding == SelfBroadcastFundingMode::PrivateSponsorship
            && let Some(reason) = sponsored_self_broadcast_availability_reason(
                self.effective_chain_configs.get(asset.chain_id),
            )
        {
            self.set_unshield_form_error(key, reason, cx);
            return None;
        }

        let Some(view_session) = self.view_session.clone() else {
            self.set_unshield_form_error(key, "Unlock the wallet vault before unshielding", cx);
            return None;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.set_unshield_form_error(key, "Wallet vault storage is unavailable", cx);
            return None;
        };
        let Some(ChainUtxoState::Ready { session, .. }) = self.chain_states.get(&asset.chain_id)
        else {
            self.set_unshield_form_error(
                key,
                "You can prepare this form now, but generation is available after wallet sync finishes",
                cx,
            );
            return None;
        };
        let session = Arc::clone(session);
        if asset.max_batched.is_zero() {
            self.set_unshield_form_error(
                key,
                "No POI-verified private notes are spendable in a batched unshield",
                cx,
            );
            return None;
        }

        let recipient_raw = recipient_input.read(cx).value().to_string();
        let Some(recipient) = parse_address(recipient_raw.trim()) else {
            self.set_unshield_form_error(key, "Enter a valid public EVM recipient address", cx);
            return None;
        };
        let amount_raw = amount_input.read(cx).value().to_string();
        let amount = match parse_unshield_amount(amount_raw.as_str(), asset.decimals) {
            Ok(amount) if !amount.is_zero() => amount,
            Ok(_) => {
                self.set_unshield_form_error(key, "Enter an amount greater than zero", cx);
                return None;
            }
            Err(error) => {
                self.set_unshield_form_error(key, error.to_string(), cx);
                return None;
            }
        };
        let max_entered_amount = unshield_form_max_entered_amount(form, delivery_mode, fee_mode)
            .unwrap_or(asset.max_batched);
        if amount > max_entered_amount {
            self.set_unshield_form_error(
                key,
                format!(
                    "Amount exceeds max POI-verified batched transaction: {}",
                    format_unshield_amount_input(max_entered_amount, asset.decimals)
                ),
                cx,
            );
            return None;
        }
        let native_top_up =
            enabled_native_top_up_plan(form.native_top_up_enabled, form.native_top_up.as_ref());

        let (
            self_broadcast_public_account_uuid,
            self_broadcast_gas_payer_display,
            self_broadcast_gas_payer_source,
            self_broadcast_gas_payer_address,
        ) = if delivery_mode == DeliveryMode::SelfBroadcast {
            let Some(uuid) = self_broadcast_gas_payer_uuid else {
                self.set_unshield_form_error(key, "Choose a Public transaction signer", cx);
                return None;
            };
            let Some(account) = self.selected_self_broadcast_gas_payer_account(Some(uuid.as_ref()))
            else {
                self.set_unshield_form_error(key, "Choose an active Public transaction signer", cx);
                return None;
            };
            let gas_payer_display = public_account_display_label(account).map_or_else(
                || short_address(&account.address),
                |label| format!("{label} · {}", short_address(&account.address)),
            );
            (
                Some(uuid.to_string()),
                Some(gas_payer_display),
                Some(account.source),
                Some(account.address),
            )
        } else {
            (None, None, None, None)
        };
        let signer_native_balance_snapshot =
            self_broadcast_public_account_uuid
                .as_deref()
                .map_or(U256::ZERO, |uuid| {
                    self_broadcast_native_balance_amount(
                        self.public_balance_snapshot.as_deref(),
                        asset.chain_id,
                        uuid,
                    )
                });

        let sponsored_authorization_limit = if self_broadcast_funding
            == SelfBroadcastFundingMode::PrivateSponsorship
        {
            let Some(effective_chain) = self.effective_chain_configs.get(asset.chain_id) else {
                self.set_unshield_form_error(key, "Selected chain settings are unavailable", cx);
                return None;
            };
            let Some((max_fee_per_gas, max_priority_fee_per_gas)) = self_broadcast_initial_gas_fee
            else {
                self.set_unshield_form_error(
                    key,
                    "Refresh self-broadcast gas fees before authorizing sponsorship",
                    cx,
                );
                return None;
            };
            match quote_sponsored_unshield_authorization_limit(
                asset.chain_id,
                effective_chain,
                &session.unspent_utxos(),
                asset.token,
                amount,
                fee_mode,
                recipient,
                unwrap,
                native_top_up.as_ref(),
                max_fee_per_gas,
                max_priority_fee_per_gas,
                signer_native_balance_snapshot,
                sponsored_incentive,
                self_broadcast_gas_payer_address.expect("self-broadcast signer was validated"),
            ) {
                Ok(limit) => Some(limit),
                Err(error) => {
                    self.set_unshield_form_error(key, error.to_string(), cx);
                    return None;
                }
            }
        } else {
            None
        };
        let sponsored_authorization_display = sponsored_authorization_limit.map(|limit| {
            sponsored_authorization_display(
                asset.chain_id,
                limit,
                &self.effective_token_registry,
                &self.public_broadcaster_anchor_cache,
            )
        });

        let fee_rows = if delivery_mode == DeliveryMode::PublicBroadcaster {
            let rows = self.monitor_fee_rows();
            let policy = self.public_broadcaster_fee_policy(allow_suspicious_broadcasters);
            let public_broadcaster_selection = Self::public_broadcaster_submission_selection(
                &broadcaster_choice,
                cost_estimate.as_ref(),
            );
            let candidates = self.current_public_broadcaster_candidates(
                asset.chain_id,
                fee_token,
                unwrap,
                native_top_up.is_some(),
                favorites_only_broadcasters,
                policy,
            );
            let trust_filter = self.public_broadcaster_trust_filter(favorites_only_broadcasters);
            if let Err(error) = select_public_broadcaster_with_policy_and_trust(
                &candidates,
                &public_broadcaster_selection,
                policy,
                &trust_filter,
            ) {
                self.set_unshield_form_error(key, error.to_string(), cx);
                return None;
            }
            rows
        } else {
            Vec::new()
        };
        let fee_policy = self.public_broadcaster_fee_policy(allow_suspicious_broadcasters);

        Some(UnshieldSpendDraft {
            custom_fee_amount,
            executor_review: self
                .unshield_forms
                .get(&key)
                .and_then(|form| form.executor_review.clone()),
            asset,
            unwrap,
            delivery_mode,
            broadcaster_choice,
            cost_estimate,
            fee_token,
            self_broadcast_gas_fee,
            self_broadcast_funding,
            sponsored_incentive,
            sponsored_authorization_limit,
            sponsored_authorization_display,
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
            fee_rows,
            fee_policy,
            favorites_only_broadcasters,
        })
    }

    pub(in crate::root) fn generate_unshield_calldata_authorized(
        &mut self,
        key: UnshieldAssetKey,
        spend_authorization: DesktopPrivateSpendAuthorization,
        authorization_limit: Option<SponsoredAuthorizationLimit>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.generate_unshield_calldata_authorized_with_gas_password(
            key,
            spend_authorization,
            None,
            authorization_limit,
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
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(resolved_chain) = self.effective_chain_configs.railgun(key.chain_id).cloned() else {
            return;
        };
        let Some(mut draft) = self.unshield_spend_draft(key, cx) else {
            return;
        };
        draft.sponsored_authorization_limit = authorization_limit;
        let requires_executor = self.unshield_requires_executor(&draft);
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
            broadcaster_choice,
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
            fee_rows,
            fee_policy,
            favorites_only_broadcasters,
        } = draft;
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
                super::executor::ExecutorUnshieldQuote::SelfBroadcast { gas_fee, .. } => {
                    Some(gas_fee)
                }
                super::executor::ExecutorUnshieldQuote::Broadcaster(_) => None,
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
            if self_broadcast_gas_payer_source == Some(PublicAccountSource::HardwareDerived) {
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
                    DesktopPrivateSpendAuthorization::PreauthorizedSigner(_) => {
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
        match delivery_mode {
            DeliveryMode::PublicBroadcaster => {
                self.start_private_broadcaster_progress(
                    DeliveryFormKind::Unshield,
                    key,
                    generation_id,
                    asset.label.clone(),
                    asset.icon_path.clone(),
                    recipient.to_checksum(None),
                    cost_estimate.clone(),
                    self.public_broadcaster_response_timeout,
                    self.public_broadcaster_republish_interval,
                );
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
                Some(super::super::stealth_accounts::StealthAccountTarget::new(
                    &session,
                    review.prepared.operation(),
                ));
        }

        let http = self.http.clone();
        let waku = if delivery_mode == DeliveryMode::PublicBroadcaster {
            let Some(waku) = self.active_waku() else {
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
                    executor: executor_review
                        .as_ref()
                        .map(|review| Arc::clone(&review.prepared)),
                    executor_maximum_private_fee: executor_review
                        .as_ref()
                        .and_then(|review| review.maximum_private_fee()),
                    executor_min_gas_price: executor_review
                        .as_ref()
                        .and_then(|review| review.broadcaster_min_gas_price()),
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
                    verify_proof: true,
                    fee_rows,
                    selection: Self::public_broadcaster_submission_selection(
                        &broadcaster_choice,
                        cost_estimate.as_ref(),
                    ),
                    fee_mode,
                    fee_policy,
                    trust_filter: self.public_broadcaster_trust_filter(favorites_only_broadcasters),
                    anchor_cache: Some(Arc::clone(&self.public_broadcaster_anchor_cache)),
                    waku: waku.expect("active Waku delivery client was validated"),
                    response_timeout: self.public_broadcaster_response_timeout,
                    republish_interval: self.public_broadcaster_republish_interval,
                    progress_tx: Some(progress_tx),
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
                        Box::pin(submit_desktop_sponsored_unshield_self_broadcast(
                            request, &http,
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
                        verify_proof: true,
                        gas_fee: self_broadcast_gas_fee,
                        progress_tx: Some(progress_tx),
                        command_rx: self_broadcast_command_rx,
                        event_tx: self_broadcast_event_tx,
                    };
                    self.spawn_public_transaction_submission(chain_id, async move {
                        Box::pin(submit_desktop_unshield_self_broadcast(request, &http))
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
                let mut refresh_public_balances = false;
                let affects_visible_public_account =
                    root.unshield_delivery_affects_visible_public_account(chain_id, recipient);
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
                            refresh_public_balances = match &result {
                                UnshieldResult::PublicBroadcaster(result)
                                    if matches!(
                                        result.result,
                                        PublicBroadcasterResultKind::Submitted { .. }
                                    ) =>
                                {
                                    affects_visible_public_account
                                }
                                UnshieldResult::SelfBroadcast(result)
                                    if result
                                        .tx
                                        .receipt()
                                        .is_some_and(|receipt| receipt.status) =>
                                {
                                    affects_visible_public_account
                                }
                                UnshieldResult::Sponsored(result)
                                    if matches!(
                                        result.outcome,
                                        SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(
                                            ref receipt
                                        ) if receipt.status
                                    ) =>
                                {
                                    affects_visible_public_account
                                }
                                _ => false,
                            };
                            if let UnshieldResult::PublicBroadcaster(result) = &result {
                                progress_result = Some((**result).clone());
                            }
                            if let UnshieldResult::SelfBroadcast(result) = &result {
                                form.self_broadcast_estimated_native_gas_cost =
                                    Some(result.estimated_native_gas_cost);
                                self_broadcast_progress_result = Some((**result).clone());
                            }
                            if let UnshieldResult::Sponsored(result) = &result {
                                sponsored_progress_outcome = Some(result.outcome.clone());
                            }
                            form.error = None;
                            form.result = Some(result);
                        }
                        Err(error) => {
                            let message = format_report_chain(&error);
                            if is_spend_authorization_failure_error(&message) {
                                clear_spend_authorization = true;
                            }
                            progress_error = Some(message.clone());
                            if form_error_clears_public_broadcaster_cost_estimate(
                                DeliveryFormKind::Unshield,
                                message.as_str(),
                            ) {
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
                if refresh_public_balances {
                    root.http.rpc_broker().invalidate_block(chain_id);
                    root.schedule_public_balance_refresh(cx);
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
                if let Some(message) = progress_error {
                    root.fail_private_broadcaster_progress(
                        DeliveryFormKind::Unshield,
                        key,
                        generation_id,
                        final_stage,
                        message,
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

    pub(in crate::root) fn watch_self_broadcast_session_events(
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        mut event_rx: mpsc::UnboundedReceiver<SelfBroadcastSessionEvent>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                let _ = this.update_in(cx, |root, window, cx| match event {
                    SelfBroadcastSessionEvent::PendingOutputPoiProofsRequired { required } => {
                        root.set_private_self_broadcast_unshield_poi_step(
                            kind,
                            key,
                            generation_id,
                            required,
                            cx,
                        );
                    }
                    SelfBroadcastSessionEvent::StepFailed { stage, message } => {
                        if root.record_private_broadcaster_progress_step_error(
                            kind,
                            key,
                            generation_id,
                            stage,
                            &message,
                            cx,
                        ) {
                            root.show_private_broadcaster_progress_dialog(window, cx);
                        }
                    }
                    SelfBroadcastSessionEvent::AttemptSubmitted(attempt) => {
                        root.record_private_self_broadcast_attempt(
                            kind,
                            key,
                            generation_id,
                            attempt,
                            cx,
                        );
                    }
                    SelfBroadcastSessionEvent::AttemptRejected { message, .. } => {
                        root.record_private_self_broadcast_attempt_rejected(
                            kind,
                            key,
                            generation_id,
                            message,
                            cx,
                        );
                    }
                    SelfBroadcastSessionEvent::HardwareProfileSessionRefreshed { session } => {
                        #[cfg(feature = "hardware")]
                        root.refresh_active_hardware_profile_session(session, cx);
                        #[cfg(not(feature = "hardware"))]
                        let _ = session;
                    }
                });
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_quote_age_does_not_expire_an_approved_spend() {
        let execution = GatewayDraftExecution::private("approval-expiry-test".into());
        let estimated_at = std::time::Instant::now();
        assert!(gateway_private_estimate_is_ready(
            Some(&execution),
            true,
            Some(estimated_at),
            true,
        ));
        let expired_at = estimated_at
            .checked_sub(Duration::from_mins(1))
            .expect("earlier quote timestamp");
        assert!(!gateway_private_estimate_is_ready(
            Some(&execution),
            true,
            Some(expired_at),
            true,
        ));

        assert!(gateway_private_estimate_is_ready(
            Some(&execution),
            true,
            Some(expired_at),
            false
        ));
        assert!(!gateway_private_estimate_is_ready(
            Some(&execution),
            false,
            Some(expired_at),
            false
        ));
        assert!(execution.approve_review());
        assert!(gateway_private_estimate_is_ready(
            Some(&execution),
            true,
            Some(expired_at),
            true,
        ));
        assert!(!gateway_private_estimate_is_ready(
            Some(&execution),
            false,
            Some(expired_at),
            true,
        ));
    }
}
