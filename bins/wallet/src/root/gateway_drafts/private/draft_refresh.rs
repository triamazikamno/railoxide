#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root::gateway_drafts) fn refresh_gateway_private_drafts(
        &self,
        cx: &Context<'_, Self>,
    ) {
        let peers: Vec<_> = self
            .gateway
            .drafts
            .borrow()
            .records
            .iter()
            .filter_map(|(peer, record)| {
                let GatewayDraftPayload::Private(input) = &record.view.input else {
                    return None;
                };
                if record.execution.is_some()
                    || record.estimation.is_some()
                    || record
                        .picker
                        .as_ref()
                        .is_some_and(|picker| picker.job.is_some())
                    || input.recipient.trim().is_empty()
                    || (!input.max && input.amount.trim().is_empty())
                    || !self.private_action_generation_ready(input.chain_id)
                    || record
                        .estimated_at
                        .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
                {
                    return None;
                }
                if let Some(PreparedDraft::Private(prepared)) = &record.prepared
                    && prepared.is_current(self, &record.view.input)
                    && record
                        .estimated_at
                        .is_some_and(|at| at.elapsed() < ESTIMATE_LIFETIME)
                {
                    return None;
                }
                Some(peer.clone())
            })
            .collect();
        for peer in peers {
            self.estimate_gateway_private_draft(&peer, cx);
        }
        self.refresh_gateway_private_pickers(cx);
        // Publish the native submission context while spend approval is pending.
        for (execution, asset, recipient, amount) in self
            .send_forms
            .values()
            .filter_map(|form| {
                Some((
                    form.gateway_execution.as_ref()?,
                    &form.asset,
                    &form.recipient_value,
                    &form.amount_input,
                ))
            })
            .chain(self.unshield_forms.values().filter_map(|form| {
                Some((
                    form.gateway_execution.as_ref()?,
                    &form.asset,
                    &form.recipient_value,
                    &form.amount_input,
                ))
            }))
        {
            if execution.generation().is_some()
                || execution.snapshot().status != GatewayDraftStatus::Attention
            {
                continue;
            }
            let current = execution.snapshot();
            let Some(mut projection) = current.private else {
                continue;
            };
            projection.summary = format!("{} {}", amount.read(cx).value(), asset.label);
            projection.context = [
                ("Asset", asset.label.clone()),
                ("Recipient", recipient.to_string()),
                ("Entered amount", amount.read(cx).value().to_string()),
            ]
            .into_iter()
            .map(|(label, value)| {
                let mut row = GatewayPrivateDisplayRow::default();
                row.label = label.into();
                row.value = value;
                row
            })
            .collect();
            execution.update_private(
                GatewayDraftStatus::Attention,
                current.step_label,
                current.message,
                current.warning,
                projection,
            );
        }
    }

    fn refresh_gateway_private_pickers(&self, cx: &Context<'_, Self>) {
        let mut book = self.gateway.drafts.borrow_mut();
        for (peer_id, record) in &mut book.records {
            let Some(picker) = record.picker.as_mut() else {
                continue;
            };
            // A completed draft quote is the preferred shared context. Never duplicate its job.
            if record.execution.is_some() || record.estimation.is_some() || picker.job.is_some() {
                continue;
            }
            let prepared = match &record.prepared {
                Some(PreparedDraft::Private(prepared)) => Some(prepared.as_ref()),
                _ => None,
            };
            if let Some(prepared) =
                prepared.filter(|prepared| prepared.is_current(self, &record.view.input))
            {
                picker.estimate = Some(prepared.clone());
                picker.estimated_at = record.estimated_at;
                picker.retry.reset();
                picker.retry_at = None;
            }
            if picker
                .estimate
                .as_ref()
                .is_some_and(|estimate| estimate.is_current(self, &record.view.input))
                && picker
                    .estimated_at
                    .is_some_and(|at| at.elapsed() < ESTIMATE_LIFETIME)
            {
                continue;
            }
            if let Some(at) = picker.retry_at {
                if at > Instant::now() {
                    continue;
                }
                picker.retry_at = None;
                picker.retry.clear_if_current(record.view.revision);
            }
            let GatewayDraftPayload::Private(raw) = &record.view.input else {
                continue;
            };
            let recipient = match raw.kind {
                GatewayPrivateDraftKind::PrivateSend => self
                    .view_session
                    .as_ref()
                    .and_then(|wallet| wallet.receive_address().ok()),
                GatewayPrivateDraftKind::Unshield => Some(Address::ZERO.to_string()),
            };
            let request = recipient
                .and_then(|recipient| self.private_draft_inputs(raw, recipient).ok())
                .and_then(|input| {
                    self.prepare_private_picker_estimate(input.clone())
                        .map(|request| (input, request))
                });
            let Some((input, request)) = request else {
                picker.retry_at =
                    Some(Instant::now() + picker.retry.mark_scheduled(record.view.revision));
                continue;
            };
            let Some(ChainUtxoState::Ready { snapshot, .. }) = self.chain_states.get(&raw.chain_id)
            else {
                continue;
            };
            let snapshot = snapshot.clone();
            let peer_id = peer_id.clone();
            let draft_id = record.view.draft_id.clone();
            let revision = record.view.revision;
            let lifetime = picker.lifetime.clone();
            let wallet = record.wallet.clone();
            let generation = record.wallet_generation;
            let http = self.http.clone();
            let job = self
                .runtime
                .spawn(async move { request.estimate(&http).await });
            picker.job = Some(job.abort_handle());
            cx.spawn(async move |this, cx| {
                let result = job.await;
                let _ = this.update(cx, |root, cx| {
                    let mut book = root.gateway.drafts.borrow_mut();
                    let Some(record) = book.records.get_mut(&peer_id).filter(|record| {
                        record.view.draft_id == draft_id
                            && record.view.revision == revision
                            && record.execution.is_none()
                            && root.active_wallet_generation == generation
                            && root
                                .view_session
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &wallet))
                    }) else {
                        return;
                    };
                    let Some(picker) = record
                        .picker
                        .as_mut()
                        .filter(|picker| Arc::ptr_eq(&picker.lifetime, &lifetime))
                    else {
                        return;
                    };
                    picker.job = None;
                    let prepared =
                        result
                            .ok()
                            .and_then(Result::ok)
                            .map(|estimate| PreparedPrivateDraft {
                                input,
                                estimate,
                                snapshot,
                            });
                    let prepared =
                        prepared.filter(|prepared| prepared.is_current(root, &record.view.input));
                    picker.retry.finish_attempt(prepared.is_some());
                    if let Some(prepared) = prepared {
                        picker.estimate = Some(prepared);
                        picker.estimated_at = Some(Instant::now());
                        picker.retry_at = None;
                    } else {
                        picker.retry_at =
                            Some(Instant::now() + picker.retry.mark_scheduled(revision));
                    }
                    drop(book);
                    root.publish_gateway_desktop_state();
                    cx.notify();
                });
            })
            .detach();
        }
    }

    pub(in crate::root::gateway_drafts) fn estimate_gateway_private_draft(
        &self,
        peer_id: &str,
        cx: &Context<'_, Self>,
    ) {
        let mut book = self.gateway.drafts.borrow_mut();
        let Some(record) = book.records.get_mut(peer_id) else {
            return;
        };
        let GatewayDraftPayload::Private(input) = &record.view.input else {
            return;
        };
        let input = input.clone();
        let retain = matches!(&record.prepared, Some(PreparedDraft::PrivateSelfBroadcast(prepared))
            if prepared.retains_background_estimate() && prepared.is_current(self, &record.view.input));
        if !retain {
            record.prepared = None;
            record.view.status = GatewayDraftStatus::Estimating;
        }
        record.estimated_at = Some(Instant::now());
        if input.delivery.broadcaster().is_some() {
            record.view.gas_quote = None;
        }
        record.view.message.clear();
        let recipients = match input.kind {
            GatewayPrivateDraftKind::PrivateSend => self.private_send_recipient_options(),
            GatewayPrivateDraftKind::Unshield => self.private_unshield_recipient_options(),
        };
        record.view.recipients = recipients
            .into_iter()
            .map(|option| GatewayDraftRecipient {
                id: match option.source {
                    crate::root::private_action::RecipientOptionSource::PrivateAddressBook => self
                        .private_address_book
                        .iter()
                        .find(|entry| entry.address == option.address.as_ref())
                        .map(|entry| entry.entry_uuid.clone()),
                    crate::root::private_action::RecipientOptionSource::PublicAddressBook => self
                        .public_address_book
                        .iter()
                        .find(|entry| entry.address.to_checksum(None) == option.address.as_ref())
                        .map(|entry| entry.entry_uuid.clone()),
                    _ => None,
                }
                .unwrap_or_default(),
                label: option.label.to_string(),
                address: option.address.to_string(),
            })
            .collect();
        let recipient = self
            .private_draft_recipient(&input)
            .and_then(|recipient| self.private_draft_asset(&input).map(|_| recipient));
        if input.delivery.broadcaster().is_some()
            && let Err(error) = &recipient
        {
            record.view.status = GatewayDraftStatus::Editing;
            record.view.message.clone_from(error);
            return;
        }
        let draft_id = record.view.draft_id.clone();
        let revision = record.view.revision;
        let wallet = record.wallet.clone();
        let generation = record.wallet_generation;
        let ethereum = self.effective_chain_configs.get(1).cloned();
        let http = self.http.clone();
        let operation = record.operation(&http);
        let join = self.runtime.spawn(async move {
            let recipient = recipient?;
            if input.kind == GatewayPrivateDraftKind::Unshield
                && !recipient.trim().is_empty()
                && recipient.trim().parse::<Address>().is_err()
            {
                let client = operation.client().await.map_err(|_| {
                    "Recipient could not be resolved. Check the name or use an address.".to_owned()
                })?;
                resolve_public_ens_recipient(
                    recipient.trim(),
                    ethereum
                        .as_ref()
                        .ok_or_else(|| "Ethereum is unavailable".to_string())?,
                    &http,
                    &client,
                )
                .await
                .map(|recipient| recipient.to_checksum(None))
                .map_err(|_| {
                    "Recipient could not be resolved. Check the name or use an address.".to_owned()
                })
            } else {
                Ok(recipient)
            }
        });
        record.estimation = Some(join.abort_handle());
        drop(book);
        let peer_id = peer_id.to_owned();
        cx.spawn(async move |this, cx| {
            let resolved = join.await;
            let _ = this.update(cx, |root, cx| {
                (|| {
                let mut book = root.gateway.drafts.borrow_mut();
                let Some(record) = book.records.get_mut(&peer_id).filter(|record| {
                    record.view.draft_id == draft_id && record.view.revision == revision && record.execution.is_none()
                        && root.active_wallet_generation == generation
                        && root.view_session.as_ref().is_some_and(|current| Arc::ptr_eq(current, &wallet))
                }) else { return };
                record.estimation = None;
                let GatewayDraftPayload::Private(input) = &record.view.input else { return };
                if input.delivery.broadcaster().is_none() {
                    drop(book);
                    let recipient = resolved.map_err(|_| "Recipient resolution was interrupted.".to_owned())
                        .and_then(std::convert::identity);
                    root.start_gateway_self_broadcast_estimate(&peer_id, &draft_id, revision, recipient, cx);
                    return;
                }
                let result = resolved.map_err(|_| "Recipient resolution was interrupted.".to_owned())
                    .and_then(std::convert::identity)
                    .and_then(|recipient| root.private_draft_recipient(input).and_then(|_| root.private_draft_inputs(input, recipient)));
                let result = result.map_err(eyre::Report::msg).and_then(|input| root.prepare_private_broadcaster_estimate(&input).map(|request| (input, request)));
                let (input, request) = match result {
                    Ok((input, Some(request))) => (input, request),
                    Ok(_) => {
                        record.view.status = GatewayDraftStatus::Editing;
                        record.view.message = if root.private_action_generation_ready(input.chain_id) {
                            "Complete the inputs and choose an available broadcaster."
                        } else { "You can prepare this form now, but generation is available after wallet sync finishes" }.into();
                        return;
                    }
                    Err(error) => {
                        record.view.status = GatewayDraftStatus::Editing;
                        record.view.message = error.to_string();
                        return;
                    }
                };
                let Some(ChainUtxoState::Ready { snapshot, .. }) = root.chain_states.get(&input.asset.chain_id) else { return };
                let snapshot = snapshot.clone();
                let http = root.http.clone();
                let job = root.runtime.spawn(async move { request.estimate(&http).await });
                record.estimation = Some(job.abort_handle());
                drop(book);
                Self::finish_gateway_private_estimate(&peer_id, &draft_id, revision, input, snapshot, job, true, cx);
                })();
                root.publish_gateway_desktop_state();
                cx.notify();
            });
        }).detach();
    }

    fn finish_gateway_private_estimate(
        peer_id: &str,
        draft_id: &str,
        revision: u64,
        input: PrivateEstimateInput,
        snapshot: Arc<ListUtxosOutput>,
        job: tokio::task::JoinHandle<eyre::Result<PublicBroadcasterCostEstimate>>,
        retry_max: bool,
        cx: &Context<'_, Self>,
    ) {
        let peer_id = peer_id.to_owned();
        let draft_id = draft_id.to_owned();
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |root, cx| {
                let mut book = root.gateway.drafts.borrow_mut();
                let Some(record) = book.records.get_mut(&peer_id).filter(|record| {
                    record.view.draft_id == draft_id && record.view.revision == revision && record.execution.is_none()
                        && root.active_wallet_generation == record.wallet_generation
                        && root.view_session.as_ref().is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
                }) else { return };
                record.estimation = None;
                let current = matches!(root.chain_states.get(&input.asset.chain_id), Some(ChainUtxoState::Ready { snapshot: current, .. }) if Arc::ptr_eq(&snapshot, current));
                if !current || root.last_evm_chain != input.asset.chain_id {
                    record.view.status = GatewayDraftStatus::Editing;
                    record.view.message = "Private funds changed. Refresh the estimate.".into();
                } else {
                    match result {
                        Ok(Ok(estimate)) => {
                            let mut input = input;
                            input.amount = format_send_amount_input(estimate.entered_amount, input.asset.decimals);
                            record.view.estimate = Some(GatewayDraftEstimatePayload::Private(private_estimate_display(root, &input, &estimate)));
                            record.prepared = Some(PreparedDraft::Private(Box::new(PreparedPrivateDraft { input, estimate, snapshot })));
                            record.estimated_at = Some(Instant::now());
                            record.view.status = GatewayDraftStatus::Ready;
                        }
                        Ok(Err(error)) if retry_max && matches!(&record.view.input, GatewayDraftPayload::Private(raw) if raw.max) => {
                            let message = crate::root::format_report_chain(&error);
                            let maximum = crate::root::private_action::form_error_public_broadcaster_max_entered_amount(&message);
                            let retry = maximum.filter(|maximum| !maximum.is_zero() && wallet_ops::parse_send_amount(&input.amount, input.asset.decimals).is_ok_and(|amount| *maximum < amount))
                                .and_then(|maximum| {
                                    let GatewayDraftPayload::Private(raw) = &record.view.input else { return None };
                                    let mut adjusted = raw.clone();
                                    adjusted.max = false;
                                    adjusted.amount = format_send_amount_input(maximum, input.asset.decimals);
                                    let input = root.private_draft_inputs(&adjusted, input.recipient.clone()).ok()?;
                                    let request = root.prepare_private_broadcaster_estimate(&input).ok()??;
                                    Some((input, request))
                                });
                            if let Some((input, request)) = retry {
                                let http = root.http.clone();
                                let job = root.runtime.spawn(async move { request.estimate(&http).await });
                                record.estimation = Some(job.abort_handle());
                                drop(book);
                                Self::finish_gateway_private_estimate(&peer_id, &draft_id, revision, input, snapshot, job, false, cx);
                                return;
                            }
                            record.view.status = GatewayDraftStatus::Editing;
                            record.view.message = crate::root::private_action::format_form_error_for_asset(&message, &input.asset, input.fee_token, Some(&root.effective_token_registry));
                        }
                        error => {
                            record.view.status = GatewayDraftStatus::Editing;
                            record.view.message = match error {
                                Ok(Err(error)) => crate::root::private_action::format_form_error_for_asset(&crate::root::format_report_chain(&error), &input.asset, input.fee_token, Some(&root.effective_token_registry)),
                                _ => "Private fee estimate was interrupted. Refresh to try again.".into(),
                            };
                        }
                    }
                }
                drop(book);
                root.publish_gateway_desktop_state();
                cx.notify();
            });
        }).detach();
    }
}
