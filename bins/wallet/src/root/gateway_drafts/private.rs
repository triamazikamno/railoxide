//! Private evaluation shares the draft book, never an editable native form.
use super::*;
use crate::root::{
    ChainUtxoState, DeliveryFormKind, UnshieldAsset,
    broadcaster_picker::BroadcasterChoice,
    is_effective_wrapped_native_token,
    private_action::{
        PrivateEstimateInput, PrivateEstimateOutput, private_action_metric_display_amount,
    },
};
use wallet_ops::{
    FeeHandlingMode, ListUtxosOutput, PublicBroadcasterCostEstimate,
    gateway::{
        GatewayBroadcasterChoice, GatewayPrivateAmountMetric, GatewayPrivateAssetChoice,
        GatewayPrivateDisplayRow, GatewayPrivateDraftEstimate, GatewayPrivateDraftInput,
        GatewayPrivateDraftKind, GatewayPrivateDraftOptions, GatewayPrivateDraftPicker,
        GatewayPrivateFeeMode, GatewayPrivateTopUpOption,
    },
};

pub(super) struct PrivateDraftPicker {
    pub(super) view_id: String,
    lifetime: Arc<()>,
    pub(super) query: String,
    estimate: Option<PreparedPrivateDraft>,
    estimated_at: Option<Instant>,
    job: Option<tokio::task::AbortHandle>,
    retry: crate::root::broadcaster_picker::BroadcasterPickerFeeEstimateRetryState,
    retry_at: Option<Instant>,
}

impl PrivateDraftPicker {
    pub(super) fn new(view_id: String, query: String) -> Self {
        Self {
            view_id,
            query,
            lifetime: Arc::new(()),
            estimate: None,
            estimated_at: None,
            job: None,
            retry: crate::root::broadcaster_picker::BroadcasterPickerFeeEstimateRetryState::default(
            ),
            retry_at: None,
        }
    }
}

impl Drop for PrivateDraftPicker {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.abort();
        }
    }
}

#[derive(Clone)]
pub(super) struct PreparedPrivateDraft {
    pub(super) input: PrivateEstimateInput,
    pub(super) estimate: PublicBroadcasterCostEstimate,
    snapshot: Arc<ListUtxosOutput>,
}

impl PreparedPrivateDraft {
    pub(super) fn is_current(&self, root: &WalletRoot, raw: &GatewayDraftPayload) -> bool {
        let GatewayDraftPayload::Private(raw) = raw else {
            return false;
        };
        if !matches!(root.chain_states.get(&self.input.asset.chain_id), Some(ChainUtxoState::Ready { snapshot, .. }) if Arc::ptr_eq(snapshot, &self.snapshot))
            || self.estimate.entered_amount > self.estimate.max_entered_amount
            || root.selected_chain != self.input.asset.chain_id
            || root.private_draft_recipient(raw).is_err()
            || root.private_draft_asset(raw).is_err()
        {
            return false;
        }
        let (unwrap, top_up) = match &self.input.output {
            PrivateEstimateOutput::Send => (false, false),
            PrivateEstimateOutput::Unshield {
                unwrap,
                native_top_up,
            } => (*unwrap, native_top_up.is_some()),
        };
        let policy = root.public_broadcaster_fee_policy(self.input.allow_out_of_range);
        let candidates = root.current_public_broadcaster_candidates(
            self.input.asset.chain_id,
            self.input.fee_token,
            unwrap,
            top_up,
            self.input.favorites_only,
            policy,
        );
        let selection = wallet_ops::PublicBroadcasterSelection::Specific {
            railgun_address: self.estimate.broadcaster.railgun_address.clone(),
        };
        wallet_ops::select_public_broadcaster_with_policy_and_trust(
            &candidates,
            &selection,
            policy,
            &root.public_broadcaster_trust_filter(self.input.favorites_only),
        )
        .is_ok_and(|candidate| {
            candidate.fees_id == self.estimate.broadcaster.fees_id
                && candidate.fee == self.estimate.broadcaster.fee
                && candidate.fee_expiration == self.estimate.broadcaster.fee_expiration
        })
    }
}

pub(super) fn valid_private_input_size(input: &GatewayPrivateDraftInput) -> bool {
    input.wallet.len() <= 128
        && input.asset.len() <= 128
        && input.fee_token.len() <= 128
        && input.amount.len() <= 100
        && input.recipient.len() <= 1024
        && input
            .address_book_entry
            .as_ref()
            .is_none_or(|id| id.len() <= 128)
        && match &input.broadcaster {
            GatewayBroadcasterChoice::Random => true,
            GatewayBroadcasterChoice::Specific { id } => id.len() <= 1024,
        }
}

const fn kind(input: &GatewayPrivateDraftInput) -> DeliveryFormKind {
    match input.kind {
        GatewayPrivateDraftKind::PrivateSend => DeliveryFormKind::Send,
        GatewayPrivateDraftKind::Unshield => DeliveryFormKind::Unshield,
    }
}

pub(super) fn resolve_private_draft_contact(
    store: &DesktopVaultStore,
    wallet: &DesktopViewSession,
    input: &GatewayPrivateDraftInput,
) -> Result<String, String> {
    let Some(id) = &input.address_book_entry else {
        return Ok(input.recipient.clone());
    };
    let address = match input.kind {
        GatewayPrivateDraftKind::PrivateSend => store
            .list_private_address_book_entries_for_session(wallet)
            .map_err(|_| "Address book is unavailable")?
            .into_iter()
            .find(|entry| entry.entry_uuid == *id)
            .map(|entry| entry.address),
        GatewayPrivateDraftKind::Unshield => store
            .list_public_address_book_entries_for_session(wallet)
            .map_err(|_| "Address book is unavailable")?
            .into_iter()
            .find(|entry| entry.entry_uuid == *id)
            .map(|entry| entry.address.to_checksum(None)),
    }
    .ok_or("Address book entry is unavailable. Choose another recipient.")?;
    if address != input.recipient {
        return Err("Address book entry changed. Choose the recipient again.".into());
    }
    Ok(address)
}

impl WalletRoot {
    fn private_draft_asset(
        &self,
        input: &GatewayPrivateDraftInput,
    ) -> Result<UnshieldAsset, String> {
        if self
            .view_session
            .as_ref()
            .is_none_or(|wallet| wallet.wallet_id() != input.wallet)
            || self.selected_chain != input.chain_id
        {
            return Err("Wallet or network changed. Open a new draft.".into());
        }
        let token = input
            .asset
            .parse::<Address>()
            .map_err(|_| "Choose an available asset")?;
        self.private_action_asset_options(kind(input), input.chain_id)
            .into_iter()
            .find(|asset| asset.token == token)
            .ok_or_else(|| "Choose an available private asset".into())
    }

    fn private_draft_recipient(&self, input: &GatewayPrivateDraftInput) -> Result<String, String> {
        if input.address_book_entry.is_none() {
            return Ok(input.recipient.clone());
        }
        let wallet = self
            .view_session
            .as_ref()
            .ok_or("Unlock the desktop wallet")?;
        let store = self
            .vault_store
            .as_ref()
            .ok_or("Wallet storage is unavailable")?;
        resolve_private_draft_contact(store, wallet, input)
    }

    fn private_draft_inputs(
        &self,
        input: &GatewayPrivateDraftInput,
        recipient: String,
    ) -> Result<PrivateEstimateInput, String> {
        let asset = self.private_draft_asset(input)?;
        let fee_token = input
            .fee_token
            .parse::<Address>()
            .map_err(|_| "Choose a fee token")?;
        let fee_mode = match input.fee_mode {
            GatewayPrivateFeeMode::Deduct => FeeHandlingMode::DeductFromAmount,
            GatewayPrivateFeeMode::AddOnTop => FeeHandlingMode::AddToAmount,
        };
        let fee_mode =
            crate::root::effective_fee_handling_mode(kind(input), asset.token, fee_token, fee_mode);
        let output = match input.kind {
            GatewayPrivateDraftKind::PrivateSend => {
                if input.unwrap || input.native_top_up {
                    return Err("Native output options are only available for Unshield.".into());
                }
                PrivateEstimateOutput::Send
            }
            GatewayPrivateDraftKind::Unshield => {
                if input.unwrap
                    && !is_effective_wrapped_native_token(
                        &self.effective_chain_configs,
                        input.chain_id,
                        asset.token,
                    )
                {
                    return Err("This asset cannot be unwrapped.".into());
                }
                let native_top_up = if input.native_top_up {
                    let recipient = recipient
                        .parse::<Address>()
                        .map_err(|_| "Enter a public recipient")?;
                    let amount = if input.max {
                        asset.max_batched
                    } else {
                        wallet_ops::parse_unshield_amount(&input.amount, asset.decimals)
                            .map_err(|error| error.to_string())?
                    };
                    Some(
                        self.unshield_native_top_up_state(
                            input.chain_id,
                            recipient,
                            asset.token,
                            input.unwrap,
                            amount,
                            fee_mode,
                        )
                        .plan
                        .ok_or("Native top-up is unavailable for these inputs or private funds.")?,
                    )
                } else {
                    None
                };
                PrivateEstimateOutput::Unshield {
                    unwrap: input.unwrap,
                    native_top_up,
                }
            }
        };
        let amount = if input.max {
            format_send_amount_input(asset.max_batched, asset.decimals)
        } else {
            input.amount.clone()
        };
        Ok(PrivateEstimateInput {
            asset,
            recipient,
            amount,
            fee_token,
            fee_mode,
            output,
            broadcaster: match &input.broadcaster {
                GatewayBroadcasterChoice::Random => BroadcasterChoice::Random,
                GatewayBroadcasterChoice::Specific { id } => BroadcasterChoice::Specific {
                    railgun_address: id.clone(),
                },
            },
            allow_out_of_range: input.allow_out_of_range,
            favorites_only: input.favorites_only,
        })
    }

    pub(super) fn gateway_private_draft_options(
        &self,
        input: &GatewayPrivateDraftInput,
        prepared: Option<&PreparedPrivateDraft>,
        picker: Option<&PrivateDraftPicker>,
    ) -> GatewayPrivateDraftOptions {
        let mut options = GatewayPrivateDraftOptions::default();
        let icon = |source: &Option<crate::assets::WalletIconSource>| match source {
            Some(crate::assets::WalletIconSource::Embedded(path))
                if path.starts_with("railgun-ui/") =>
            {
                Some(path.clone())
            }
            _ => None,
        };
        options.assets = self
            .private_action_asset_options(kind(input), input.chain_id)
            .iter()
            .map(|asset| {
                let mut choice = GatewayPrivateAssetChoice::default();
                choice.id = asset.token.to_string();
                choice.label.clone_from(&asset.label);
                choice.max_amount = format_send_amount_input(asset.max_batched, asset.decimals);
                choice.max_amount_label =
                    private_action_metric_display_amount(asset.max_batched, asset.decimals);
                choice.available =
                    private_action_metric_display_amount(asset.poi_verified_total, asset.decimals);
                choice.icon = icon(&asset.icon_path);
                choice
            })
            .collect();
        let policy = self.public_broadcaster_fee_policy(input.allow_out_of_range);
        options.fee_tokens = self
            .current_public_broadcaster_fee_token_options(
                input.chain_id,
                input.unwrap,
                input.native_top_up,
                input.favorites_only,
                policy,
            )
            .iter()
            .map(|token| {
                let mut choice = GatewayPrivateAssetChoice::default();
                choice.id = token.token.to_string();
                choice.label.clone_from(&token.label);
                choice.max_amount = format_send_amount_input(token.max_spendable, token.decimals);
                choice.max_amount_label =
                    private_action_metric_display_amount(token.max_spendable, token.decimals);
                choice.available.clone_from(&choice.max_amount_label);
                choice.broadcaster_count = token.eligible_broadcaster_count;
                choice.icon = icon(&token.icon_path);
                choice
            })
            .collect();
        let Ok(asset) = self.private_draft_asset(input) else {
            return options;
        };
        options.metrics = crate::root::private_action::private_action_metrics(&asset)
            .into_iter()
            .map(|metric| {
                let mut row = GatewayPrivateAmountMetric::default();
                row.label = metric.label.into();
                row.value = crate::root::private_action::private_action_metric_display_amount(
                    metric.amount,
                    asset.decimals,
                );
                row.amount = format_send_amount_input(metric.amount, asset.decimals);
                row
            })
            .collect();
        if asset.total > asset.max_batched {
            options.warnings.push(
                ui::private_action::spend_capacity_warning(
                    input.kind == GatewayPrivateDraftKind::Unshield,
                )
                .into(),
            );
        }
        if input.native_top_up {
            options
                .warnings
                .push(ui::private_action::NATIVE_TOP_UP_LINKAGE_WARNING.into());
        }
        let Ok(fee_token) = input.fee_token.parse::<Address>() else {
            return options;
        };
        let fee_options = self.current_public_broadcaster_fee_token_options(
            input.chain_id,
            input.unwrap,
            input.native_top_up,
            input.favorites_only,
            policy,
        );
        if let Some(warning) = crate::root::public_broadcaster_fee_token_warning(
            &self.monitor_fee_rows(),
            input.chain_id,
            &fee_options,
            fee_token,
            &self.public_broadcaster_trust_filter(input.favorites_only),
        ) {
            options.warnings.push(warning.into());
        }
        options.show_fee_mode = crate::root::public_broadcaster::should_show_fee_mode_toggle(
            kind(input),
            asset.token,
            fee_token,
        );
        if input.kind == GatewayPrivateDraftKind::Unshield {
            if is_effective_wrapped_native_token(
                &self.effective_chain_configs,
                input.chain_id,
                asset.token,
            ) {
                options.unwrap_labels = crate::root::native_wrapped_output_labels(input.chain_id)
                    .map(|(native, wrapped)| [native.to_owned(), wrapped.to_owned()]);
            }
            let recipient = prepared
                .map_or(input.recipient.as_str(), |prepared| {
                    prepared.input.recipient.as_str()
                })
                .parse::<Address>();
            let amount = if input.max {
                Ok(asset.max_batched)
            } else {
                wallet_ops::parse_unshield_amount(&input.amount, asset.decimals)
            };
            if let (Ok(recipient), Ok(amount)) = (recipient, amount) {
                let fee_mode = match input.fee_mode {
                    GatewayPrivateFeeMode::Deduct => FeeHandlingMode::DeductFromAmount,
                    GatewayPrivateFeeMode::AddOnTop => FeeHandlingMode::AddToAmount,
                };
                let fee_mode = crate::root::effective_fee_handling_mode(
                    kind(input),
                    asset.token,
                    fee_token,
                    fee_mode,
                );
                if let Some(plan) = self
                    .unshield_native_top_up_state(
                        input.chain_id,
                        recipient,
                        asset.token,
                        input.unwrap,
                        amount,
                        fee_mode,
                    )
                    .plan
                {
                    let mut top_up = GatewayPrivateTopUpOption::default();
                    top_up.label = format!(
                        "Also send {} for gas",
                        crate::root::native_token_display_label(input.chain_id)
                    );
                    top_up.funding_detail = format!(
                        "Recipient receives {}; funded from private {}.",
                        crate::root::format_native_token_amount_for_display(
                            input.chain_id,
                            plan.native_amount
                        ),
                        crate::root::format_token_amount_for_display(
                            input.chain_id,
                            plan.wrapped_native_token,
                            plan.wrapped_native_amount,
                            Some(&self.effective_token_registry)
                        )
                    );
                    options.native_top_up = Some(top_up);
                }
            }
        }
        let candidates = self.current_public_broadcaster_candidates(
            input.chain_id,
            fee_token,
            input.unwrap,
            input.native_top_up,
            input.favorites_only,
            policy,
        );
        let choice = match &input.broadcaster {
            GatewayBroadcasterChoice::Random => BroadcasterChoice::Random,
            GatewayBroadcasterChoice::Specific { id } => BroadcasterChoice::Specific {
                railgun_address: id.clone(),
            },
        };
        if let Some(warning) = crate::root::broadcaster_picker::selected_broadcaster_fee_warning(
            &choice,
            &candidates,
            input.allow_out_of_range,
        ) {
            options.warnings.push(warning);
        }
        let candidates = if input.allow_out_of_range {
            candidates
        } else {
            wallet_ops::fee_policy_eligible_public_broadcasters(&candidates, policy)
        };
        let candidates = wallet_ops::sort_specific_public_broadcasters(
            candidates,
            &self.public_broadcaster_sort_seed,
        );
        options.candidate_count = candidates.len();
        options.specific_label =
            crate::root::broadcaster_picker::selected_broadcaster_label(&choice, &candidates);
        if let Some(picker) = picker {
            let view_id = &picker.view_id;
            let query = &picker.query;
            let context = prepared.or(picker.estimate.as_ref()).map(|prepared| {
                crate::root::broadcaster_picker::BroadcasterPickerFeeEstimateContext::from_estimate(
                    &prepared.estimate,
                )
            });
            let selected = match &input.broadcaster {
                GatewayBroadcasterChoice::Specific { id } => Some(id.as_str()),
                GatewayBroadcasterChoice::Random => None,
            };
            let candidates: Vec<_> = candidates
                .into_iter()
                .filter(|candidate| {
                    crate::root::broadcaster_picker::broadcaster_candidate_matches_query(
                        candidate, query,
                    )
                })
                .collect();
            let rows = self.private_broadcaster_picker_rows(
                &candidates,
                policy,
                context.as_ref(),
                selected,
                if picker.job.is_some() {
                    "Estimating…"
                } else if picker.retry_at.is_some() {
                    "Retrying…"
                } else {
                    "Estimate unavailable"
                },
            );
            let mut projection = GatewayPrivateDraftPicker::default();
            projection.view_id.clone_from(view_id);
            projection.query.clone_from(query);
            projection.total_count = options.candidate_count;
            if let Ok(rows) = rows
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
            {
                projection.rows = rows;
                options.picker = Some(projection);
            }
        }
        options
    }

    pub(super) fn refresh_gateway_private_drafts(&self, cx: &Context<'_, Self>) {
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

    pub(super) fn estimate_gateway_private_draft(&self, peer_id: &str, cx: &Context<'_, Self>) {
        let mut book = self.gateway.drafts.borrow_mut();
        let Some(record) = book.records.get_mut(peer_id) else {
            return;
        };
        let GatewayDraftPayload::Private(input) = &record.view.input else {
            return;
        };
        let input = input.clone();
        record.prepared = None;
        record.estimated_at = Some(Instant::now());
        record.view.gas_quote = None;
        record.view.status = GatewayDraftStatus::Estimating;
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
        let recipient = match self
            .private_draft_recipient(&input)
            .and_then(|recipient| self.private_draft_asset(&input).map(|_| recipient))
        {
            Ok(recipient) => recipient,
            Err(error) => {
                record.view.status = GatewayDraftStatus::Editing;
                record.view.message = error;
                return;
            }
        };
        let draft_id = record.view.draft_id.clone();
        let revision = record.view.revision;
        let wallet = record.wallet.clone();
        let generation = record.wallet_generation;
        let ethereum = self.effective_chain_configs.get(&1).cloned();
        let http = self.http.clone();
        let join = self.runtime.spawn(async move {
            if input.kind == GatewayPrivateDraftKind::Unshield
                && !recipient.trim().is_empty()
                && recipient.trim().parse::<Address>().is_err()
            {
                resolve_public_ens_recipient(recipient.trim(), ethereum.as_ref(), &http)
                    .await
                    .map(|recipient| recipient.to_checksum(None))
                    .map_err(|_| {
                        "Recipient could not be resolved. Check the address or ENS name.".to_owned()
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
                let result = resolved.map_err(|_| "Recipient resolution was interrupted.".to_owned())
                    .and_then(std::convert::identity)
                    .and_then(|recipient| root.private_draft_recipient(input).and_then(|_| root.private_draft_inputs(input, recipient)));
                let result = result.and_then(|input| root.prepare_private_broadcaster_estimate(&input).map(|request| (input, request)));
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
                        record.view.message = error;
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
                if !current || root.selected_chain != input.asset.chain_id {
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

    pub(super) fn gateway_private_form(
        &self,
        execution: &GatewayDraftExecution,
    ) -> Option<(DeliveryFormKind, crate::root::UnshieldAssetKey)> {
        self.send_forms
            .iter()
            .find(|(_, form)| {
                form.gateway_execution
                    .as_ref()
                    .is_some_and(|owner| owner.same_execution(execution))
            })
            .map(|(key, _)| (DeliveryFormKind::Send, *key))
            .or_else(|| {
                self.unshield_forms
                    .iter()
                    .find(|(_, form)| {
                        form.gateway_execution
                            .as_ref()
                            .is_some_and(|owner| owner.same_execution(execution))
                    })
                    .map(|(key, _)| (DeliveryFormKind::Unshield, *key))
            })
    }

    pub(in crate::root) fn release_gateway_private_form(
        &mut self,
        execution: &GatewayDraftExecution,
        cx: &mut Context<'_, Self>,
    ) {
        if execution.generation().is_some()
            || execution.snapshot().status != GatewayDraftStatus::Failed
        {
            return;
        }
        match self.gateway_private_form(execution) {
            Some((DeliveryFormKind::Send, key)) => self.close_send_form(key, cx),
            Some((DeliveryFormKind::Unshield, key)) => self.close_unshield_form(key, cx),
            None => {}
        }
    }

    pub(in crate::root) fn reject_gateway_private_authorization(
        &mut self,
        execution: &GatewayDraftExecution,
        cx: &mut Context<'_, Self>,
    ) {
        if execution.generation().is_some() {
            return;
        }
        let error = self
            .gateway_private_form(execution)
            .and_then(|(kind, key)| match kind {
                DeliveryFormKind::Send => self.send_forms.get(&key)?.error.as_deref(),
                DeliveryFormKind::Unshield => self.unshield_forms.get(&key)?.error.as_deref(),
            });
        execution.reject_private_review(
            error
                .unwrap_or("Transaction inputs changed. Update the draft and submit again.")
                .to_owned(),
        );
        self.release_gateway_private_form(execution, cx);
    }

    pub(super) fn submit_gateway_private_draft(
        &mut self,
        peer_id: &str,
        draft_id: &str,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        use crate::root::{DeliveryMode, UnshieldAssetKey};
        let mut book = self.gateway.drafts.borrow_mut();
        let Some(record) = book.records.get_mut(peer_id).filter(|record| {
            record.view.draft_id == draft_id
                && record.view.revision == revision
                && record.execution.is_none()
                && record.view.status == GatewayDraftStatus::Ready
                && record.wallet_generation == self.active_wallet_generation
                && self
                    .view_session
                    .as_ref()
                    .is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
        }) else {
            return;
        };
        if window.has_active_dialog(cx)
            || !self.send_forms.is_empty()
            || !self.unshield_forms.is_empty()
            || self.private_broadcaster_progress.is_some()
            || self.public_form.sending
            || self.public_form.shielding
        {
            record.view.message =
                "Finish or close the current action in the desktop app first.".into();
            return;
        }
        let Some(PreparedDraft::Private(prepared)) = &record.prepared else {
            return;
        };
        if !prepared.is_current(self, &record.view.input)
            || record
                .estimated_at
                .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME)
        {
            record.view.status = GatewayDraftStatus::Editing;
            drop(book);
            self.estimate_gateway_private_draft(peer_id, cx);
            return;
        }
        let Some(PreparedDraft::Private(prepared)) = record.prepared.take() else {
            return;
        };
        let estimated_at = record.estimated_at;
        let execution =
            GatewayDraftExecution::private(alloy::hex::encode(rand::random::<[u8; 16]>()));
        record.execution = Some(execution.clone());
        record.picker = None;
        drop(book);
        let input = prepared.input;
        let key = UnshieldAssetKey::from_asset(&input.asset);
        let kind = match &input.output {
            PrivateEstimateOutput::Send => DeliveryFormKind::Send,
            PrivateEstimateOutput::Unshield { .. } => DeliveryFormKind::Unshield,
        };
        window.activate_window();
        match &input.output {
            PrivateEstimateOutput::Send => {
                self.initialize_send_form(input.asset, window, cx);
                let form = self
                    .send_forms
                    .get_mut(&key)
                    .expect("new private submission state");
                form.gateway_execution = Some(execution.clone());
                form.delivery_mode = DeliveryMode::PublicBroadcaster;
                form.selected_fee_token = input.fee_token;
                form.broadcaster_choice = input.broadcaster;
                form.fee_mode = input.fee_mode;
                form.allow_suspicious_broadcasters = input.allow_out_of_range;
                form.favorites_only_broadcasters = input.favorites_only;
            }
            PrivateEstimateOutput::Unshield {
                unwrap,
                native_top_up,
            } => {
                self.initialize_unshield_form(input.asset, window, cx);
                let form = self
                    .unshield_forms
                    .get_mut(&key)
                    .expect("new private submission state");
                form.gateway_execution = Some(execution.clone());
                form.delivery_mode = DeliveryMode::PublicBroadcaster;
                form.selected_fee_token = input.fee_token;
                form.broadcaster_choice = input.broadcaster;
                form.fee_mode = input.fee_mode;
                form.allow_suspicious_broadcasters = input.allow_out_of_range;
                form.favorites_only_broadcasters = input.favorites_only;
                form.unwrap = *unwrap;
                form.native_top_up_enabled = native_top_up.is_some();
                form.native_top_up.clone_from(native_top_up);
            }
        }
        self.set_private_action_recipient(kind, key, &input.recipient, window, cx);
        self.set_programmatic_amount_input(kind, key, prepared.estimate.entered_amount, window, cx);
        match kind {
            DeliveryFormKind::Send => {
                let form = self
                    .send_forms
                    .get_mut(&key)
                    .expect("private submission state");
                form.cost_estimate = Some(prepared.estimate);
                form.gateway_estimated_at = estimated_at;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.estimate_id = 0;
                self.generate_send_calldata_from_form(key, window, cx);
            }
            DeliveryFormKind::Unshield => {
                let form = self
                    .unshield_forms
                    .get_mut(&key)
                    .expect("private submission state");
                form.cost_estimate = Some(prepared.estimate);
                form.gateway_estimated_at = estimated_at;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.estimate_id = 0;
                self.generate_unshield_calldata_from_form(key, window, cx);
            }
        }
        if !window.has_active_dialog(cx) {
            self.reject_gateway_private_authorization(&execution, cx);
        }
        self.watch_gateway_drafts(cx);
    }
}

fn private_estimate_display(
    root: &WalletRoot,
    input: &PrivateEstimateInput,
    estimate: &PublicBroadcasterCostEstimate,
) -> GatewayPrivateDraftEstimate {
    use crate::root::public_broadcaster_cost::PublicBroadcasterCostDisplay;
    let display = PublicBroadcasterCostDisplay::from_estimate(
        &input.asset,
        estimate,
        crate::root::broadcaster_candidate_anchor_rate(&estimate.broadcaster),
        Some(&root.effective_token_registry),
    );
    let mut projection = GatewayPrivateDraftEstimate::default();
    projection.amount = format_send_amount_input(estimate.entered_amount, input.asset.decimals);
    projection.amount_label = display.action_amount(estimate.entered_amount);
    projection.max_amount = Some(format_send_amount_input(
        estimate.max_entered_amount,
        input.asset.decimals,
    ));
    projection.max_amount_label = Some(private_action_metric_display_amount(
        estimate.max_entered_amount,
        input.asset.decimals,
    ));
    projection.recipient = Some(input.recipient.clone());
    projection.broadcaster =
        crate::root::broadcaster_picker::broadcaster_candidate_label(&estimate.broadcaster);
    projection.shape =
        crate::root::public_broadcaster_cost::public_broadcaster_estimate_shape(estimate);
    projection.network_gas = display.gas_value();
    let to_wire = |row: ui::private_action::DisplayRow| {
        let mut wire = GatewayPrivateDisplayRow::default();
        wire.label = row.label;
        wire.value = row.value;
        wire.suffix = row.suffix;
        wire
    };
    projection.outcome = display
        .outcome_rows(&root.public_broadcaster_anchor_cache)
        .into_iter()
        .map(to_wire)
        .collect();
    projection.fee_breakdown = display
        .fee_rows(&root.public_broadcaster_anchor_cache)
        .into_iter()
        .map(to_wire)
        .collect();
    projection.transaction_fee = display.fee_amount_with_usd(&root.public_broadcaster_anchor_cache);
    projection
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retiring_a_picker_aborts_its_pending_estimate() {
        let picker_job = tokio::spawn(std::future::pending::<()>());
        let mut picker = PrivateDraftPicker::new("view".into(), String::new());
        picker.job = Some(picker_job.abort_handle());
        drop(picker);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), picker_job)
                .await
                .expect("bounded cancellation")
                .expect_err("closed view cancels its work")
                .is_cancelled()
        );
    }
}
