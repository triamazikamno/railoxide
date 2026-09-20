//! Self-broadcast inputs are evaluated without creating native editable forms.
use super::*;
use crate::root::private_action::{
    PrivateEstimateOutput, PublicBalanceFundingEstimate, SelfBroadcastFundingMode,
    SelfBroadcastNativeBalanceState, SponsoredAssetFee, SponsoredFundingEstimateState,
    default_self_broadcast_gas_payer_uuid, self_broadcast_gas_payer_label,
    self_broadcast_gas_payer_random_candidate, self_broadcast_initial_gas_values,
    self_broadcast_native_balance_amount, self_broadcast_native_balance_label,
    self_broadcast_native_balance_state, sponsored_estimate_failure_state,
    sponsored_estimate_from_authorization_limit, sponsored_funding_choice_visible,
    sponsored_incentive_from_text, sponsored_self_broadcast_availability_reason,
};
use crate::root::{ChainUtxoState, UnshieldAsset};
use wallet_ops::gateway::{
    GatewayPrivateDelivery, GatewayPrivateDraftEstimate, GatewayPrivateDraftInput,
    GatewayPrivateDraftKind, GatewayPrivateFeeMode, GatewayPrivateFunding, GatewayPrivateGasFee,
    GatewayPrivateIncentive, GatewayPrivateSelfBroadcastInput, GatewayPrivateSelfBroadcastOptions,
    GatewayPrivateSignerChoice,
};
use wallet_ops::{
    FeeHandlingMode, ListUtxosOutput, SelfBroadcastGasFeeQuote, SelfBroadcastGasFeeSelection,
    SponsoredIncentive, WalletSession, estimate_desktop_send_self_broadcast_cost,
    estimate_desktop_unshield_self_broadcast_cost, expected_eip1559_fee_per_gas,
    parse_railgun_recipient, quote_desktop_self_broadcast_gas_fee,
    quote_sponsored_send_authorization_limit, quote_sponsored_unshield_authorization_limit,
    unshield_protocol_fee_amount_for_fee_mode,
};

#[derive(Clone)]
pub(super) struct PreparedSelfBroadcastDraft {
    raw: GatewayPrivateDraftInput,
    pub(super) asset: UnshieldAsset,
    pub(super) recipient: String,
    pub(super) amount: U256,
    maximum: U256,
    pub(super) fee_mode: FeeHandlingMode,
    pub(super) output: PrivateEstimateOutput,
    pub(super) signer: PublicAccountMetadata,
    signer_balance: U256,
    pub(super) funding: SelfBroadcastFundingMode,
    pub(super) incentive: SponsoredIncentive,
    pub(super) gas_selection: SelfBroadcastGasFeeSelection,
    pub(super) gas_quote: Option<SelfBroadcastGasFeeQuote>,
    pub(super) estimate: SponsoredFundingEstimateState,
    snapshot: Arc<ListUtxosOutput>,
    effective_chain: EffectiveChainConfig,
}

struct SelfBroadcastDraftEstimation {
    gas_quote: Option<SelfBroadcastGasFeeQuote>,
    result: eyre::Result<PreparedSelfBroadcastDraft>,
}

async fn estimate_self_broadcast_draft(
    prepared: eyre::Result<(PreparedSelfBroadcastDraft, Arc<WalletSession>)>,
    quote: impl std::future::Future<Output = eyre::Result<SelfBroadcastGasFeeQuote>>,
) -> SelfBroadcastDraftEstimation {
    // Chain gas prices are useful while the recipient, amount or signer is still incomplete.
    let gas_quote = quote.await.ok();
    let result = match prepared {
        Ok((mut prepared, session)) => {
            prepared.gas_quote = gas_quote;
            tokio::task::spawn_blocking(move || prepared.estimate(&session))
                .await
                .map_err(|_| eyre::eyre!("Self-broadcast estimate was interrupted."))
        }
        Err(error) => Err(error),
    };
    SelfBroadcastDraftEstimation { gas_quote, result }
}

impl PreparedSelfBroadcastDraft {
    pub(super) fn is_current(&self, root: &WalletRoot, raw: &GatewayDraftPayload) -> bool {
        matches!(raw, GatewayDraftPayload::Private(raw) if *raw == self.raw)
            && matches!(root.chain_states.get(&self.raw.chain_id), Some(ChainUtxoState::Ready { snapshot, .. }) if Arc::ptr_eq(snapshot, &self.snapshot))
            && root.private_draft_asset(&self.raw).is_ok()
            && root.private_draft_recipient(&self.raw).is_ok()
            && root
                .gateway_private_signers(&self.raw.wallet)
                .contains(&self.signer)
            && self.signer_balance
                == self_broadcast_native_balance_amount(
                    root.public_balance_snapshot.as_deref(),
                    self.raw.chain_id,
                    &self.signer.public_account_uuid,
                )
            && root.effective_chain_configs.get(self.raw.chain_id) == Some(&self.effective_chain)
    }

    pub(super) fn retains_background_estimate(&self) -> bool {
        self.funding == SelfBroadcastFundingMode::PrivateSponsorship
            && matches!(self.estimate, SponsoredFundingEstimateState::Ready(_))
    }

    fn estimate(mut self, session: &WalletSession) -> Self {
        let Some((max_fee, tip)) =
            self_broadcast_initial_gas_values(&self.gas_selection, self.gas_quote)
        else {
            return self;
        };
        let quote = self
            .gas_quote
            .unwrap_or_else(|| SelfBroadcastGasFeeQuote::from_rpc_gas_price(max_fee));
        let utxos = session.unspent_utxos();
        if self.funding == SelfBroadcastFundingMode::PublicBalance {
            let cost = match &self.output {
                PrivateEstimateOutput::Send => estimate_desktop_send_self_broadcast_cost(
                    &utxos,
                    self.asset.token,
                    self.amount,
                    quote,
                    max_fee,
                    tip,
                ),
                PrivateEstimateOutput::Unshield {
                    unwrap,
                    native_top_up,
                } => estimate_desktop_unshield_self_broadcast_cost(
                    Some(&self.effective_chain)
                        .filter(|chain| {
                            (*unwrap || native_top_up.is_some())
                                && session.executor_owner().is_some()
                                && chain.accepted_executor_profile().is_some()
                        })
                        .map(|chain| &chain.gas),
                    &utxos,
                    self.asset.token,
                    self.amount,
                    self.fee_mode,
                    *unwrap,
                    native_top_up.as_ref(),
                    quote,
                    max_fee,
                    tip,
                ),
            };
            self.estimate = cost.map_or(
                SponsoredFundingEstimateState::PublicBalanceUnavailable,
                |cost| {
                    SponsoredFundingEstimateState::PublicBalanceReady(Box::new(
                        PublicBalanceFundingEstimate {
                            chain_id: self.raw.chain_id,
                            cost,
                        },
                    ))
                },
            );
            return self;
        }
        let chain = &self.effective_chain;
        let limit = match &self.output {
            PrivateEstimateOutput::Send => {
                parse_railgun_recipient(&self.recipient).and_then(|recipient| {
                    quote_sponsored_send_authorization_limit(
                        self.raw.chain_id,
                        chain,
                        &utxos,
                        self.asset.token,
                        self.amount,
                        &recipient,
                        max_fee,
                        tip,
                        self.signer_balance,
                        self.incentive,
                        self.signer.address,
                    )
                })
            }
            PrivateEstimateOutput::Unshield {
                unwrap,
                native_top_up,
            } => self
                .recipient
                .parse::<Address>()
                .map_err(eyre::Report::from)
                .and_then(|recipient| {
                    quote_sponsored_unshield_authorization_limit(
                        self.raw.chain_id,
                        chain,
                        &utxos,
                        self.asset.token,
                        self.amount,
                        self.fee_mode,
                        recipient,
                        *unwrap,
                        native_top_up.as_ref(),
                        max_fee,
                        tip,
                        self.signer_balance,
                        self.incentive,
                        self.signer.address,
                    )
                }),
        };
        self.estimate = match limit {
            Ok(limit) => {
                let protocol_fee = match self.output {
                    PrivateEstimateOutput::Send => None,
                    PrivateEstimateOutput::Unshield { .. } => {
                        let Ok(amount) =
                            unshield_protocol_fee_amount_for_fee_mode(self.amount, self.fee_mode)
                        else {
                            return self;
                        };
                        Some(SponsoredAssetFee {
                            token: self.asset.token,
                            amount,
                        })
                    }
                };
                sponsored_estimate_from_authorization_limit(
                    self.raw.chain_id,
                    limit,
                    expected_eip1559_fee_per_gas(quote, max_fee, tip),
                    protocol_fee,
                )
            }
            Err(error) => sponsored_estimate_failure_state(
                self.raw.chain_id,
                chain.wrapped_native_token,
                &error,
            ),
        };
        self
    }

    fn display(&self, root: &WalletRoot) -> GatewayPrivateDraftEstimate {
        let mut display = GatewayPrivateDraftEstimate::default();
        display.amount = format_send_amount_input(self.amount, self.asset.decimals);
        display.amount_label = crate::root::private_action::private_action_metric_display_amount(
            self.amount,
            self.asset.decimals,
        );
        display.max_amount = Some(format_send_amount_input(self.maximum, self.asset.decimals));
        display.max_amount_label = Some(
            crate::root::private_action::private_action_metric_display_amount(
                self.maximum,
                self.asset.decimals,
            ),
        );
        display.recipient = Some(self.recipient.clone());
        let (recipient_amount, unwrap, top_up) = match &self.output {
            PrivateEstimateOutput::Send => (self.amount, false, None),
            PrivateEstimateOutput::Unshield {
                unwrap,
                native_top_up,
            } => {
                let amount = native_top_up.as_ref().map_or_else(
                    || {
                        wallet_ops::unshield_receiver_amount_for_fee_mode(
                            self.amount,
                            self.fee_mode,
                        )
                        .and_then(|gross| {
                            unshield_protocol_fee_amount_for_fee_mode(self.amount, self.fee_mode)
                                .map(|fee| gross.saturating_sub(fee))
                        })
                        .ok()
                    },
                    |top_up| {
                        Some(
                            wallet_ops::native_top_up_primary_recipient_amount_for_fee_mode(
                                self.asset.token,
                                top_up.wrapped_native_token,
                                self.amount,
                                self.fee_mode,
                                top_up.native_amount,
                            ),
                        )
                    },
                );
                (amount.unwrap_or_default(), *unwrap, native_top_up.as_ref())
            }
        };
        let mut outcome = wallet_ops::gateway::GatewayPrivateDisplayRow::default();
        outcome.label = "Recipient receives".into();
        outcome.value = if unwrap {
            crate::root::format_native_token_amount_for_display(self.raw.chain_id, recipient_amount)
        } else {
            crate::root::private_action::private_amount_label(recipient_amount, &self.asset, true)
        };
        display.outcome.push(outcome);
        if let Some(top_up) = top_up {
            let mut row = wallet_ops::gateway::GatewayPrivateDisplayRow::default();
            row.label = "Recipient gas top-up".into();
            row.value = crate::root::format_native_token_amount_for_display(
                self.raw.chain_id,
                top_up.native_amount,
            );
            display.outcome.push(row);
        }
        display.protocol_fee_label =
            Some(public_action_protocol_fee_label(RAILGUN_PROTOCOL_FEE_BPS));
        if self.gas_quote.is_none()
            && matches!(self.gas_selection, SelfBroadcastGasFeeSelection::Auto)
        {
            display.gas_error =
                Some("Gas quote is unavailable. Refresh or enter custom gas fees.".into());
        } else {
            display.self_broadcast_fees =
                serde_json::to_value(root.sponsored_funding_estimate_display(&self.estimate)).ok();
        }
        display
    }
}

fn gas_selection(input: &GatewayPrivateGasFee) -> Result<SelfBroadcastGasFeeSelection, String> {
    match input {
        GatewayPrivateGasFee::Auto {} => Ok(SelfBroadcastGasFeeSelection::Auto),
        GatewayPrivateGasFee::Custom {
            max_fee_gwei,
            priority_fee_gwei,
        } => {
            let max_fee_per_gas = parse_gwei_to_wei(max_fee_gwei)?;
            let max_priority_fee_per_gas = parse_gwei_to_wei(priority_fee_gwei)?;
            crate::root::gas_fee::validate_custom_gas_fee(
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )?;
            Ok(SelfBroadcastGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            })
        }
    }
}

impl WalletRoot {
    pub(in crate::root) fn gateway_self_broadcast_review_current(
        &self,
        execution: Option<&GatewayDraftExecution>,
        cx: &gpui::App,
    ) -> bool {
        let Some(execution) = execution else {
            return true;
        };
        let book = self.gateway.drafts.borrow();
        let Some(record) = book.records.values().find(|record| {
            record
                .execution
                .as_ref()
                .is_some_and(|owner| owner.same_execution(execution))
        }) else {
            return false;
        };
        let Some(PreparedDraft::PrivateSelfBroadcast(prepared)) = &record.prepared else {
            return false;
        };
        if !prepared.is_current(self, &record.view.input) {
            return false;
        }
        let Some((kind, key)) = self.gateway_private_form(execution) else {
            return false;
        };
        let (signer, funding, incentive, gas, recipient, amount, fee_mode) = match kind {
            crate::root::DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get(&key) else {
                    return false;
                };
                (
                    &form.self_broadcast_gas_payer_uuid,
                    form.self_broadcast_funding,
                    form.sponsored_incentive,
                    &form.self_broadcast_gas_fee,
                    &form.recipient_value,
                    &form.amount_input,
                    form.fee_mode,
                )
            }
            crate::root::DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get(&key) else {
                    return false;
                };
                let PrivateEstimateOutput::Unshield {
                    unwrap,
                    native_top_up,
                } = &prepared.output
                else {
                    return false;
                };
                if form.unwrap != *unwrap
                    || form.native_top_up_enabled != native_top_up.is_some()
                    || form.native_top_up.as_ref() != native_top_up.as_ref()
                {
                    return false;
                }
                (
                    &form.self_broadcast_gas_payer_uuid,
                    form.self_broadcast_funding,
                    form.sponsored_incentive,
                    &form.self_broadcast_gas_fee,
                    &form.recipient_value,
                    &form.amount_input,
                    form.fee_mode,
                )
            }
        };
        signer.as_deref() == Some(prepared.signer.public_account_uuid.as_str())
            && funding == prepared.funding
            && incentive == prepared.incentive
            && gas
                .selection(cx)
                .is_ok_and(|selection| selection == prepared.gas_selection)
            && recipient.as_ref() == prepared.recipient
            && fee_mode == prepared.fee_mode
            && parse_send_amount(amount.read(cx).value().as_ref(), prepared.asset.decimals)
                .is_ok_and(|amount| amount == prepared.amount)
    }

    pub(super) fn begin_gateway_self_broadcast(
        &mut self,
        prepared: PreparedSelfBroadcastDraft,
        execution: &GatewayDraftExecution,
        estimated_at: Option<Instant>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        use crate::root::{DeliveryFormKind, DeliveryMode, UnshieldAssetKey};
        let key = UnshieldAssetKey::from_asset(&prepared.asset);
        let kind = match prepared.output {
            PrivateEstimateOutput::Send => DeliveryFormKind::Send,
            PrivateEstimateOutput::Unshield { .. } => DeliveryFormKind::Unshield,
        };
        window.activate_window();
        match kind {
            DeliveryFormKind::Send => self.initialize_send_form(prepared.asset.clone(), window, cx),
            DeliveryFormKind::Unshield => {
                self.initialize_unshield_form(prepared.asset.clone(), window, cx);
            }
        }
        self.set_private_action_recipient(kind, key, &prepared.recipient, window, cx);
        self.set_programmatic_amount_input(kind, key, prepared.amount, window, cx);
        let incentive_text = match prepared.incentive {
            SponsoredIncentive::Custom(percent) => percent,
            _ => 5,
        }
        .to_string();
        let incentive_input = crate::root::new_prefilled_input(window, cx, "1-100", incentive_text);
        match kind {
            DeliveryFormKind::Send => {
                let form = self
                    .send_forms
                    .get_mut(&key)
                    .expect("new private submission state");
                form.gateway_execution = Some(execution.clone());
                form.gateway_estimated_at = estimated_at;
                form.delivery_mode = DeliveryMode::SelfBroadcast;
                form.selected_fee_token = prepared.asset.token;
                form.fee_mode = prepared.fee_mode;
                form.self_broadcast_gas_payer_uuid =
                    Some(Arc::from(prepared.signer.public_account_uuid.as_str()));
                form.self_broadcast_funding = prepared.funding;
                form.sponsored_incentive = prepared.incentive;
                form.sponsored_custom_incentive_input = incentive_input;
                install_gas_selection(&mut form.self_broadcast_gas_fee, &prepared, window, cx);
                form.sponsored_funding_estimate = Some(prepared.estimate);
                form.sponsored_estimate_pending = false;
                form.sponsored_estimate_id = 0;
                form.estimate_id = 0;
                self.generate_send_calldata_from_form(key, window, cx);
            }
            DeliveryFormKind::Unshield => {
                let form = self
                    .unshield_forms
                    .get_mut(&key)
                    .expect("new private submission state");
                form.gateway_execution = Some(execution.clone());
                form.gateway_estimated_at = estimated_at;
                form.delivery_mode = DeliveryMode::SelfBroadcast;
                form.selected_fee_token = prepared.asset.token;
                form.fee_mode = prepared.fee_mode;
                form.self_broadcast_gas_payer_uuid =
                    Some(Arc::from(prepared.signer.public_account_uuid.as_str()));
                form.self_broadcast_funding = prepared.funding;
                form.sponsored_incentive = prepared.incentive;
                form.sponsored_custom_incentive_input = incentive_input;
                install_gas_selection(&mut form.self_broadcast_gas_fee, &prepared, window, cx);
                if let PrivateEstimateOutput::Unshield {
                    unwrap,
                    native_top_up,
                } = prepared.output
                {
                    form.unwrap = unwrap;
                    form.native_top_up_enabled = native_top_up.is_some();
                    form.native_top_up = native_top_up;
                }
                form.sponsored_funding_estimate = Some(prepared.estimate);
                form.sponsored_estimate_pending = false;
                form.sponsored_estimate_id = 0;
                form.estimate_id = 0;
                self.generate_unshield_calldata_from_form(key, window, cx);
            }
        }
        if !window.has_active_dialog(cx) {
            self.reject_gateway_private_authorization(execution, cx);
        }
        self.watch_gateway_drafts(cx);
    }

    fn prepare_gateway_self_broadcast(
        &self,
        input: &GatewayPrivateDraftInput,
        recipient: String,
    ) -> eyre::Result<(PreparedSelfBroadcastDraft, Arc<WalletSession>)> {
        let GatewayPrivateDelivery::SelfBroadcast {
            delivery:
                GatewayPrivateSelfBroadcastInput::SelfBroadcast {
                    signer,
                    funding,
                    fee,
                },
        } = &input.delivery
        else {
            eyre::bail!("Choose self-broadcast delivery.");
        };
        let asset = self.private_draft_asset(input).map_err(eyre::Report::msg)?;
        self.private_draft_recipient(input)
            .map_err(eyre::Report::msg)?;
        let signer = self
            .gateway_private_signers(&input.wallet)
            .into_iter()
            .find(|account| Some(account.public_account_uuid.as_str()) == signer.as_deref())
            .ok_or_else(|| eyre::eyre!("Choose a Public account for this transaction."))?;
        if let Some(reason) = signer_unavailable_reason(&signer) {
            eyre::bail!(reason);
        }
        let options = self.gateway_self_broadcast_options(input);
        if let Some(error) = options.signer_error {
            eyre::bail!(error);
        }
        let (funding, incentive) = match funding {
            GatewayPrivateFunding::PublicBalance {} => (
                SelfBroadcastFundingMode::PublicBalance,
                SponsoredIncentive::Standard,
            ),
            GatewayPrivateFunding::Sponsorship { incentive } => {
                if let Some(reason) = options.sponsorship_unavailable {
                    eyre::bail!(reason);
                }
                let incentive = match incentive {
                    GatewayPrivateIncentive::Economy {} => SponsoredIncentive::Economy,
                    GatewayPrivateIncentive::Standard {} => SponsoredIncentive::Standard,
                    GatewayPrivateIncentive::Priority {} => SponsoredIncentive::Priority,
                    GatewayPrivateIncentive::Custom { percent } => {
                        sponsored_incentive_from_text(SponsoredIncentive::Custom(5), percent)
                            .map_err(eyre::Report::msg)?
                    }
                };
                (SelfBroadcastFundingMode::PrivateSponsorship, incentive)
            }
        };
        let fee_mode = match input.fee_mode {
            GatewayPrivateFeeMode::Deduct => FeeHandlingMode::DeductFromAmount,
            GatewayPrivateFeeMode::AddOnTop => FeeHandlingMode::AddToAmount,
        };
        let maximum = match input.kind {
            GatewayPrivateDraftKind::PrivateSend => {
                parse_railgun_recipient(recipient.trim())?;
                asset.max_batched
            }
            GatewayPrivateDraftKind::Unshield => {
                recipient
                    .parse::<Address>()
                    .map_err(|_| eyre::eyre!("Enter a public recipient"))?;
                crate::root::unshield_max_entered_amount_for_mode(asset.max_batched, fee_mode)
            }
        };
        let amount = if input.max {
            maximum
        } else {
            parse_send_amount(&input.amount, asset.decimals)?
        };
        if amount.is_zero() || amount > maximum {
            eyre::bail!("Enter an amount within the available private balance.");
        }
        // Top-up construction must use the exact entered amount, including the native Max policy.
        let mut output_input = input.clone();
        output_input.max = false;
        output_input.amount = format_send_amount_input(amount, asset.decimals);
        let output = self
            .private_draft_output(&output_input, &asset, &recipient, fee_mode)
            .map_err(eyre::Report::msg)?;
        let gas_selection = gas_selection(fee).map_err(eyre::Report::msg)?;
        let Some(ChainUtxoState::Ready {
            snapshot, session, ..
        }) = self.chain_states.get(&input.chain_id)
        else {
            eyre::bail!("Generation is available after wallet sync finishes.");
        };
        Ok((
            PreparedSelfBroadcastDraft {
                raw: input.clone(),
                asset,
                recipient,
                amount,
                maximum,
                fee_mode,
                output,
                signer_balance: self_broadcast_native_balance_amount(
                    self.public_balance_snapshot.as_deref(),
                    input.chain_id,
                    &signer.public_account_uuid,
                ),
                signer,
                funding,
                incentive,
                gas_selection,
                gas_quote: None,
                estimate: SponsoredFundingEstimateState::Unavailable,
                snapshot: snapshot.clone(),
                effective_chain: self
                    .effective_chain_configs
                    .railgun(input.chain_id)
                    .cloned()?,
            },
            session.clone(),
        ))
    }

    pub(super) fn start_gateway_self_broadcast_estimate(
        &self,
        peer_id: &str,
        draft_id: &str,
        revision: u64,
        recipient: Result<String, String>,
        cx: &Context<'_, Self>,
    ) {
        let mut book = self.gateway.drafts.borrow_mut();
        let Some(record) = book.records.get_mut(peer_id).filter(|record| {
            record.view.draft_id == draft_id
                && record.view.revision == revision
                && record.execution.is_none()
        }) else {
            return;
        };
        let GatewayDraftPayload::Private(input) = &record.view.input else {
            return;
        };
        let chain_id = input.chain_id;
        let Ok(effective_chain) = self.effective_chain_configs.railgun(chain_id).cloned() else {
            return;
        };
        let prepared = recipient
            .map_err(eyre::Report::msg)
            .and_then(|recipient| self.prepare_gateway_self_broadcast(input, recipient));
        let complete = prepared.is_ok();
        let http = self.http.clone();
        let job = self.runtime.spawn(async move {
            estimate_self_broadcast_draft(
                prepared,
                quote_desktop_self_broadcast_gas_fee(chain_id, &effective_chain, &http),
            )
            .await
        });
        record.estimation = Some(job.abort_handle());
        let peer_id = peer_id.to_owned();
        let draft_id = draft_id.to_owned();
        drop(book);
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |root, cx| {
                let mut book = root.gateway.drafts.borrow_mut();
                let Some(record) = book.records.get_mut(&peer_id).filter(|record| {
                    record.view.draft_id == draft_id
                        && record.view.revision == revision
                        && record.execution.is_none()
                        && record.wallet_generation == root.active_wallet_generation
                        && root
                            .view_session
                            .as_ref()
                            .is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
                }) else {
                    return;
                };
                record.estimation = None;
                let result = match result {
                    Ok(estimation) => {
                        if let Some(quote) = estimation.gas_quote {
                            record.view.gas_quote = Some(GatewayDraftGasQuote::new(
                                format_gwei(quote.suggested_max_fee_per_gas),
                                format_gwei(quote.suggested_max_priority_fee_per_gas),
                            ));
                        }
                        estimation.result
                    }
                    Err(_) => Err(eyre::eyre!("Self-broadcast estimate was interrupted.")),
                };
                match result {
                    Ok(prepared) if prepared.is_current(root, &record.view.input) => {
                        record.view.estimate =
                            Some(GatewayDraftEstimatePayload::Private(prepared.display(root)));
                        let ready = matches!(
                            prepared.estimate,
                            SponsoredFundingEstimateState::Ready(_)
                                | SponsoredFundingEstimateState::PublicBalanceReady(_)
                        );
                        record.view.status = if ready {
                            GatewayDraftStatus::Ready
                        } else {
                            GatewayDraftStatus::Editing
                        };
                        record.prepared =
                            ready.then(|| PreparedDraft::PrivateSelfBroadcast(Box::new(prepared)));
                        record.estimated_at = Some(Instant::now());
                        record.view.message.clear();
                    }
                    result => {
                        record.prepared = None;
                        record.view.status = GatewayDraftStatus::Editing;
                        record.view.message = result.err().map_or_else(
                            || "Private funds or signer changed. Refresh the estimate.".into(),
                            |error| error.to_string(),
                        );
                        record.view.estimate = complete.then(|| {
                            let mut display = GatewayPrivateDraftEstimate::default();
                            display.self_broadcast_fees = serde_json::to_value(
                                ui::private_action::self_broadcast::FeeDisplay::Error(
                                    record.view.message.clone(),
                                ),
                            )
                            .ok();
                            GatewayDraftEstimatePayload::Private(display)
                        });
                    }
                }
                drop(book);
                root.publish_gateway_desktop_state();
                cx.notify();
            });
        })
        .detach();
    }

    fn gateway_private_signers(&self, wallet: &str) -> Vec<PublicAccountMetadata> {
        if self
            .view_session
            .as_ref()
            .is_none_or(|session| session.wallet_id() != wallet)
        {
            return Vec::new();
        }
        self.public_accounts
            .iter()
            .filter(|account| account.is_active_for_wallet(wallet))
            .cloned()
            .collect()
    }

    pub(super) fn gateway_self_broadcast_options(
        &self,
        input: &GatewayPrivateDraftInput,
    ) -> GatewayPrivateSelfBroadcastOptions {
        let mut options = GatewayPrivateSelfBroadcastOptions::default();
        let accounts = self.gateway_private_signers(&input.wallet);
        let eligible_accounts = accounts
            .iter()
            .filter(|account| signer_unavailable_reason(account).is_none())
            .cloned()
            .collect::<Vec<_>>();
        options.default_signer =
            default_self_broadcast_gas_payer_uuid(&eligible_accounts).map(|uuid| uuid.to_string());
        options.show_sponsorship =
            sponsored_funding_choice_visible(self.effective_chain_configs.get(input.chain_id));
        options.sponsorship_unavailable = sponsored_self_broadcast_availability_reason(
            self.effective_chain_configs.get(input.chain_id),
        )
        .map(str::to_owned);
        let (selected, sponsored) = match &input.delivery {
            GatewayPrivateDelivery::SelfBroadcast {
                delivery:
                    GatewayPrivateSelfBroadcastInput::SelfBroadcast {
                        signer, funding, ..
                    },
            } => (
                signer.as_deref(),
                matches!(funding, GatewayPrivateFunding::Sponsorship { .. }),
            ),
            GatewayPrivateDelivery::Broadcaster(_) => (None, false),
        };
        if let GatewayPrivateDelivery::SelfBroadcast {
            delivery: GatewayPrivateSelfBroadcastInput::SelfBroadcast { fee, funding, .. },
        } = &input.delivery
        {
            options.gas_error = gas_selection(fee).err();
            if let GatewayPrivateFunding::Sponsorship {
                incentive: GatewayPrivateIncentive::Custom { percent },
            } = funding
            {
                options.incentive_error =
                    sponsored_incentive_from_text(SponsoredIncentive::Custom(5), percent)
                        .err()
                        .map(str::to_owned);
            }
        }
        options.signers = accounts
            .iter()
            .map(|account| {
                let mut choice = GatewayPrivateSignerChoice::default();
                choice.id.clone_from(&account.public_account_uuid);
                choice.label = self_broadcast_gas_payer_label(account);
                choice.address_label = railgun_ui::short_address(&account.address);
                choice.balance_label = format!(
                    "{} {}",
                    self_broadcast_native_balance_label(
                        self.public_balance_snapshot.as_deref(),
                        input.chain_id,
                        &account.public_account_uuid,
                    ),
                    crate::root::native_token_display_label(input.chain_id)
                );
                choice.unavailable = signer_unavailable_reason(account).map(str::to_owned);
                choice.random_candidate = choice.unavailable.is_none()
                    && if sponsored {
                        Some(account.public_account_uuid.as_str()) != selected
                    } else {
                        self_broadcast_gas_payer_random_candidate(
                            account,
                            selected,
                            input.chain_id,
                            self.public_balance_snapshot.as_deref(),
                        )
                    };
                choice
            })
            .collect();
        options.signer_error = selected.and_then(|uuid| {
            match options.signers.iter().find(|choice| choice.id == uuid) {
                None => Some(
                    "The selected Public account is unavailable. Choose a signer again.".into(),
                ),
                Some(choice) if choice.unavailable.is_some() => choice.unavailable.clone(),
                Some(_)
                    if !sponsored
                        && self_broadcast_native_balance_state(
                            self.public_balance_snapshot.as_deref(),
                            input.chain_id,
                            uuid,
                        ) == SelfBroadcastNativeBalanceState::Zero =>
                {
                    Some(crate::root::private_action::SELF_BROADCAST_ZERO_GAS_PAYER_WARNING.into())
                }
                Some(_) => None,
            }
        });
        options
    }
}

fn signer_unavailable_reason(account: &PublicAccountMetadata) -> Option<&'static str> {
    (account.source == PublicAccountSource::HardwareDerived && !cfg!(feature = "hardware"))
        .then_some("Hardware signing is unavailable in this desktop build.")
}

fn install_gas_selection(
    editor: &mut crate::root::gas_fee::Eip1559GasFeeEditorState,
    prepared: &PreparedSelfBroadcastDraft,
    window: &mut Window,
    cx: &mut Context<'_, WalletRoot>,
) {
    use crate::root::gas_fee::Eip1559GasFeeMode;
    editor.quote = prepared.gas_quote;
    editor.refresh_id = editor.refresh_id.wrapping_add(1);
    editor.refreshing = false;
    editor.error = None;
    editor.quote_error = None;
    editor.mode = match prepared.gas_selection {
        SelfBroadcastGasFeeSelection::Auto => Eip1559GasFeeMode::Auto,
        SelfBroadcastGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } => {
            let max_fee = format_gwei(max_fee_per_gas);
            let tip = format_gwei(max_priority_fee_per_gas);
            editor.pending_programmatic_max_fee_input = Some(Arc::from(max_fee.as_str()));
            editor.pending_programmatic_max_priority_fee_input = Some(Arc::from(tip.as_str()));
            editor
                .max_fee_input
                .update(cx, |input, cx| input.set_value(max_fee, window, cx));
            editor
                .max_priority_fee_input
                .update(cx, |input, cx| input.set_value(tip, window, cx));
            Eip1559GasFeeMode::Custom
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn incomplete_self_broadcast_inputs_still_fetch_gas_quote() {
        let quote = SelfBroadcastGasFeeQuote::from_rpc_gas_price(1_000_000_000);
        let estimation = estimate_self_broadcast_draft(
            Err(eyre::eyre!("Enter a public recipient")),
            std::future::ready(Ok(quote)),
        )
        .await;

        assert_eq!(estimation.gas_quote, Some(quote));
        assert!(
            estimation.result.is_err(),
            "a gas quote cannot prepare a spend"
        );
    }
}
