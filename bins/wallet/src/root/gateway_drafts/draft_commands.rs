use super::*;

impl WalletRoot {
    pub(in crate::root) fn apply_gateway_draft_command(
        &mut self,
        peer_id: &str,
        command: GatewayDraftCommand,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(wallet) = self.view_session.clone() else {
            return;
        };
        match command {
            GatewayDraftCommand::Create { request_id, input } => {
                if !self.gateway.drafts.borrow_mut().create(
                    peer_id,
                    request_id,
                    input,
                    wallet,
                    self.active_wallet_generation,
                ) {
                    return;
                }
                self.estimate_gateway_draft(peer_id, cx);
            }
            GatewayDraftCommand::Update {
                draft_id,
                revision,
                input,
            } => {
                if !self.gateway.drafts.borrow_mut().update(
                    peer_id,
                    &draft_id,
                    revision,
                    input,
                    &wallet,
                    self.active_wallet_generation,
                ) {
                    return;
                }
                self.estimate_gateway_draft(peer_id, cx);
            }
            GatewayDraftCommand::Submit { draft_id, revision } => {
                self.submit_gateway_draft(peer_id, &draft_id, revision, window, cx);
            }
            GatewayDraftCommand::Cancel { draft_id } => {
                let mut book = self.gateway.drafts.borrow_mut();
                let Some(record) = book
                    .records
                    .get_mut(peer_id)
                    .filter(|record| record.view.draft_id == draft_id)
                else {
                    return;
                };
                if let Some(execution) = record.execution.clone() {
                    drop(book);
                    if execution.snapshot().private.is_some() && execution.generation().is_some() {
                        return; // Private operations use execution-scoped controls.
                    }
                    if let Some(generation) = execution.generation() {
                        if generation == self.public_form.action_generation
                            && self.public_form.action_stop_available
                        {
                            self.stop_public_action_progress(cx);
                            if self.public_form.action_stopped {
                                execution.stopped();
                            }
                        }
                    } else {
                        let reviewing =
                            execution.snapshot().status == GatewayDraftStatus::Attention;
                        execution.cancel_review();
                        let owns_visible_review = execution.snapshot().private.is_none()
                            || self.gateway_private_form(&execution).is_some();
                        if reviewing
                            && execution.snapshot().status == GatewayDraftStatus::Failed
                            && owns_visible_review
                        {
                            // Closing this review also releases the desktop dialog for an explicit retry.
                            window.close_dialog(cx);
                        }
                        self.release_gateway_private_form(&execution, cx);
                    }
                } else {
                    if let Some(job) = record.estimation.take() {
                        job.abort();
                    }
                    book.records.remove(peer_id);
                }
            }
            GatewayDraftCommand::PrivatePicker {
                draft_id,
                revision,
                view_id,
                open,
                query,
            } => {
                if view_id.is_empty() || view_id.len() > 128 || query.len() > 1024 {
                    return;
                }
                let mut book = self.gateway.drafts.borrow_mut();
                let Some(record) = book.records.get_mut(peer_id).filter(|record| {
                    record.view.draft_id == draft_id
                        && record.view.revision == revision
                        && record.execution.is_none()
                        && matches!(&record.view.input, GatewayDraftPayload::Private(input) if input.delivery.broadcaster().is_some())
                }) else {
                    return;
                };
                if open {
                    let query = query.trim().to_ascii_lowercase();
                    if let Some(picker) = record
                        .picker
                        .as_mut()
                        .filter(|picker| picker.view_id == view_id)
                    {
                        picker.query = query;
                    } else {
                        record.picker = Some(private::PrivateDraftPicker::new(view_id, query));
                    }
                } else if record
                    .picker
                    .as_ref()
                    .is_some_and(|picker| picker.view_id == view_id)
                {
                    record.picker = None;
                }
            }
            GatewayDraftCommand::PrivateControl {
                draft_id,
                execution_id,
                control,
            } => {
                let execution = self
                    .gateway
                    .drafts
                    .borrow()
                    .records
                    .get(peer_id)
                    .filter(|record| {
                        record.view.draft_id == draft_id
                            && record.wallet_generation == self.active_wallet_generation
                            && self
                                .view_session
                                .as_ref()
                                .is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
                    })
                    .and_then(|record| record.execution.clone());
                if let Some(execution) = execution {
                    self.gateway_private_control(&execution, &execution_id, control, cx);
                }
            }
            GatewayDraftCommand::Dismiss { draft_id } => {
                self.gateway.drafts.borrow_mut().dismiss(peer_id, &draft_id);
            }
        }
        self.watch_gateway_drafts(cx);
        self.publish_gateway_desktop_state();
        cx.notify();
    }

    fn gateway_draft_context(
        &self,
        input: &GatewayDraftInput,
    ) -> Result<EstimationContext, String> {
        let wallet = self
            .view_session
            .clone()
            .ok_or("Unlock the desktop wallet")?;
        if self.last_evm_chain != input.chain_id
            || self.public_form.selected_account_uuid.as_deref() != Some(input.account.as_str())
        {
            return Err("Account or network changed. Open a new draft.".into());
        }
        let account = self
            .public_accounts
            .iter()
            .find(|account| {
                account.public_account_uuid == input.account
                    && account.is_active_for_wallet(wallet.wallet_id())
                    && account.is_available_on_chain(input.chain_id)
            })
            .cloned()
            .ok_or("This public account is unavailable")?;
        let asset = if input.asset == "native" {
            PublicAssetId::Native
        } else {
            PublicAssetId::Erc20(
                input
                    .asset
                    .parse()
                    .map_err(|_| "Choose an available asset")?,
            )
        };
        let balances = self
            .public_balance_snapshot
            .as_ref()
            .filter(|snapshot| snapshot.chain_id == input.chain_id)
            .and_then(|snapshot| {
                snapshot
                    .accounts
                    .iter()
                    .find(|entry| entry.account == account)
            })
            .ok_or("Refresh balances before preparing a transaction")?;
        let entry = balances
            .balances
            .iter()
            .find(|entry| entry.asset.id == asset)
            .ok_or("Choose an available asset")?;
        let balance = entry
            .amount
            .amount()
            .ok_or("Asset balance is unavailable")?;
        let native_balance = balances
            .balances
            .iter()
            .find(|entry| entry.asset.id == PublicAssetId::Native)
            .and_then(|entry| entry.amount.amount())
            .ok_or("Native gas balance is unavailable")?;
        let store = self
            .vault_store
            .clone()
            .ok_or("Wallet storage is unavailable")?;
        let recipient = if input.kind == GatewayDraftKind::Shield {
            None
        } else if let Some(id) = &input.address_book_entry {
            Some(
                store
                    .list_public_address_book_entries_for_session(&wallet)
                    .map_err(|_| "Address book is unavailable")?
                    .into_iter()
                    .find(|entry| entry.entry_uuid == *id)
                    .ok_or("Address book entry is unavailable. Choose another recipient.")?
                    .address,
            )
        } else {
            input.recipient.trim().parse::<Address>().ok()
        };
        Ok(EstimationContext {
            input: input.clone(),
            native_payer: native_shield_payer(
                account.source,
                self.selected_wallet_source().is_hardware_derived(),
            ),
            account,
            asset,
            symbol: entry.asset.symbol.clone(),
            decimals: entry.asset.decimals,
            balance,
            native_balance,
            recipient,
            effective_chain: self
                .effective_chain_configs
                .enabled(input.chain_id)
                .cloned()
                .map_err(|_| "Chain is unavailable")?,
            ethereum: self.effective_chain_configs.get(1).cloned(),
            registry: self.effective_token_registry.clone(),
            anchors: self.public_broadcaster_anchor_cache.clone(),
            http: self.http.clone(),
            wallet,
            store,
            icon: public_asset_icon_path(
                input.chain_id,
                asset,
                Some(&self.effective_token_registry),
            ),
        })
    }

    fn estimate_gateway_draft(&mut self, peer_id: &str, cx: &mut Context<'_, Self>) {
        let input = self.gateway.drafts.borrow().records[peer_id]
            .view
            .input
            .clone();
        let Some(input) = input.public() else {
            if matches!(&input, GatewayDraftPayload::Private(input) if input.delivery.broadcaster().is_some())
            {
                self.ensure_waku_for_delivery(crate::root::DeliveryMode::PublicBroadcaster, cx);
            }
            self.estimate_gateway_private_draft(peer_id, cx);
            return;
        };
        let context = self.gateway_draft_context(input);
        let mut book = self.gateway.drafts.borrow_mut();
        let record = book.records.get_mut(peer_id).expect("admitted draft");
        record.view.recipients = self
            .view_session
            .as_ref()
            .zip(self.vault_store.as_ref())
            .and_then(|(wallet, store)| {
                store
                    .list_public_address_book_entries_for_session(wallet)
                    .ok()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|entry| GatewayDraftRecipient {
                id: entry.entry_uuid,
                label: entry.label,
                address: entry.address.to_checksum(None),
            })
            .collect();
        record.view.status = GatewayDraftStatus::Estimating;
        record.view.message.clear();
        let context = match context {
            Ok(context) => context,
            Err(error) => {
                record.view.gas_quote = None;
                record.view.status = GatewayDraftStatus::Editing;
                record.view.message = error;
                return;
            }
        };
        record.context_binding = Some(context.binding());
        record.prepared = None;
        record.estimated_at = None;
        record.view.estimate = None;
        let draft_id = record.view.draft_id.clone();
        let revision = record.view.revision;
        let wallet = record.wallet.clone();
        let wallet_generation = record.wallet_generation;
        let operation = record.operation(&self.http);
        let join = self.runtime.spawn(estimate_draft(context, operation));
        record.estimation = Some(join.abort_handle());
        drop(book);
        let peer_id = peer_id.to_owned();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |root, cx| {
                if !root
                    .view_session
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &wallet))
                    || root.active_wallet_generation != wallet_generation
                {
                    return;
                }
                let mut book = root.gateway.drafts.borrow_mut();
                let Some(record) = book.records.get_mut(&peer_id).filter(|record| {
                    record.view.draft_id == draft_id
                        && record.view.revision == revision
                        && record.execution.is_none()
                }) else {
                    return;
                };
                record.estimation = None;
                if root.last_evm_chain != record.view.input.chain_id()
                    || root.public_form.selected_account_uuid.as_deref()
                        != record
                            .view
                            .input
                            .public()
                            .map(|input| input.account.as_str())
                {
                    record.view.status = GatewayDraftStatus::Editing;
                    record.view.message = "Account or network changed. Open a new draft.".into();
                } else {
                    let result = if let Ok(estimation) = result {
                        record.view.gas_quote = estimation.gas_quote;
                        estimation.result
                    } else {
                        record.view.gas_quote = None;
                        Err("Estimate unavailable. Refresh the gas price to try again.".into())
                    };
                    match result {
                        Ok((prepared, estimate)) => {
                            record.estimated_at = prepared.as_ref().map(|_| Instant::now());
                            record.view.status = if prepared.is_some() {
                                GatewayDraftStatus::Ready
                            } else {
                                record.view.message = "Enter a recipient".into();
                                GatewayDraftStatus::Editing
                            };
                            record.prepared = prepared;
                            record.view.estimate =
                                Some(GatewayDraftEstimatePayload::Public(estimate));
                        }
                        Err(message) => {
                            record.view.status = GatewayDraftStatus::Editing;
                            record.view.message = message;
                        }
                    }
                }
                drop(book);
                root.publish_gateway_desktop_state();
                cx.notify();
            });
        })
        .detach();
    }

    fn submit_gateway_draft(
        &mut self,
        peer_id: &str,
        draft_id: &str,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mut book = self.gateway.drafts.borrow_mut();
        let Some(record) = book.records.get_mut(peer_id).filter(|record| {
            record.view.draft_id == draft_id
                && record.view.revision == revision
                && record.execution.is_none()
                && record.view.status == GatewayDraftStatus::Ready
                && self
                    .view_session
                    .as_ref()
                    .is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
                && self.active_wallet_generation == record.wallet_generation
        }) else {
            return;
        };
        let Some(input) = record.view.input.public() else {
            drop(book);
            self.submit_gateway_private_draft(peer_id, draft_id, revision, window, cx);
            return;
        };
        if self.public_form.sending || self.public_form.shielding || window.has_active_dialog(cx) {
            record.view.message =
                "Finish or close the current action in the desktop app first.".into();
            return;
        }
        if record
            .estimated_at
            .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME)
        {
            drop(book);
            self.estimate_gateway_draft(peer_id, cx);
            return;
        }
        match self.gateway_draft_context(input) {
            Ok(context) if record.context_binding.as_ref() != Some(&context.binding()) => {
                drop(book);
                self.estimate_gateway_draft(peer_id, cx);
                return;
            }
            Ok(_) => {}
            Err(error) => {
                record.view.gas_quote = None;
                record.view.status = GatewayDraftStatus::Editing;
                record.view.message = error;
                record.prepared = None;
                return;
            }
        }
        let Some(mut prepared) = record.prepared.take() else {
            return;
        };
        // Bound before authorization, as the Shield form binds its own.
        if let PreparedDraft::Shield(draft) = &mut prepared {
            match bind_draft_native_operation(
                &mut self.public_form.native_shield_operation,
                draft.native_preparation.is_some(),
                NativeShieldKey {
                    wallet_id: self.selected_wallet_id.clone(),
                    chain_id: draft.chain_id,
                    public_account_uuid: Arc::clone(&draft.public_account_uuid),
                    profile: draft.profile,
                    amount: draft.amount,
                },
            ) {
                Ok(operation) => draft.native_operation = operation,
                Err(error) => {
                    record.view.message = error.to_string();
                    record.prepared = Some(prepared);
                    return;
                }
            }
        }
        let execution =
            GatewayDraftExecution::default().with_origin(self.gateway_peer_name(peer_id));
        record.execution = Some(execution.clone());
        let (intent, summary, hardware) = match &mut prepared {
            PreparedDraft::Send(draft) => {
                draft.gateway_execution = Some(execution);
                (
                    SpendAuthorizationIntent::PublicSend(draft.clone()),
                    public_send_authorization_summary(draft),
                    draft.public_account_source == PublicAccountSource::HardwareDerived,
                )
            }
            PreparedDraft::Private(_) | PreparedDraft::PrivateSelfBroadcast(_) => return,
            PreparedDraft::Shield(draft) => {
                draft.gateway_execution = Some(execution);
                (
                    SpendAuthorizationIntent::PublicShield(draft.clone()),
                    public_shield_authorization_summary(draft),
                    draft.public_account_source == PublicAccountSource::HardwareDerived,
                )
            }
        };
        drop(book);
        let summary = summary.requiring_explicit_review();
        self.clear_public_action_progress_state();
        window.activate_window();
        if hardware {
            Self::open_hardware_public_action_authorization_dialog(intent, summary, window, cx);
        } else {
            self.request_spend_authorization(intent, summary, window, cx);
        }
        self.watch_gateway_drafts(cx);
    }

    pub(super) fn watch_gateway_drafts(&mut self, cx: &Context<'_, Self>) {
        if self.gateway.draft_watch.is_some() {
            return;
        }
        self.gateway.draft_watch = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let keep = this
                    .update(cx, |root, cx| {
                        root.refresh_gateway_private_drafts(cx);
                        root.publish_gateway_desktop_state();
                        let active = root.gateway.drafts.borrow().records.values().any(|record| {
                            (record.execution.is_none()
                                && matches!(record.view.input, GatewayDraftPayload::Private(_)))
                                || record.execution.as_ref().is_some_and(|execution| {
                                    matches!(
                                        execution.snapshot().status,
                                        GatewayDraftStatus::Attention
                                            | GatewayDraftStatus::InProgress
                                    )
                                })
                        });
                        if !active {
                            root.gateway.draft_watch = None;
                        }
                        cx.notify();
                        active
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }
}
