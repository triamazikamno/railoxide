use super::*;
use std::time::Instant;
use wallet_ops::{
    DesktopUnshieldPublicBroadcasterEstimateRequest, ExecutorAsset, ExecutorDelivery,
    PreparedExecutorOperation, PublicBroadcasterSelection,
    estimate_desktop_unshield_public_broadcaster_cost, vault::ExecutorOperationId,
};

#[derive(Clone, PartialEq, Eq)]
struct ExecutorUnshieldBinding {
    custom_fee_amount: Option<U256>,
    chain_id: u64,
    token: Address,
    recipient: Address,
    amount: U256,
    unwrap: bool,
    native_top_up: Option<DesktopNativeTopUpPlan>,
    delivery: DeliveryMode,
    funding: SelfBroadcastFundingMode,
    payer: Option<String>,
    gas_fee: SelfBroadcastGasFeeSelection,
    fee_token: Address,
    fee_mode: FeeHandlingMode,
    broadcaster: BroadcasterChoice,
    favorites_only: bool,
    fee_policy: BroadcasterFeePolicy,
}

impl ExecutorUnshieldBinding {
    fn same_action(&self, other: &Self) -> bool {
        self.chain_id == other.chain_id
            && self.token == other.token
            && self.recipient == other.recipient
            && self.amount == other.amount
            && self.unwrap == other.unwrap
            && self.native_top_up == other.native_top_up
            && self.fee_mode == other.fee_mode
    }

    fn from_draft(draft: &UnshieldSpendDraft) -> Self {
        Self {
            custom_fee_amount: draft.custom_fee_amount,
            chain_id: draft.asset.chain_id,
            token: draft.asset.token,
            recipient: draft.recipient,
            amount: draft.amount,
            unwrap: draft.unwrap,
            native_top_up: draft.native_top_up.clone(),
            delivery: draft.delivery_mode,
            funding: draft.self_broadcast_funding,
            payer: draft.self_broadcast_public_account_uuid.clone(),
            gas_fee: draft.self_broadcast_gas_fee,
            fee_token: draft.fee_token,
            fee_mode: draft.fee_mode,
            broadcaster: draft.broadcaster_choice.clone(),
            favorites_only: draft.favorites_only_broadcasters,
            fee_policy: draft.fee_policy,
        }
    }
}

pub(in crate::root) struct ExecutorUnshieldOperation {
    id: ExecutorOperationId,
    binding: ExecutorUnshieldBinding,
}

impl ExecutorUnshieldOperation {
    fn for_action(
        previous: Option<&Self>,
        binding: ExecutorUnshieldBinding,
    ) -> Result<Self, wallet_ops::vault::ExecutorStoreError> {
        let id = previous
            .filter(|previous| previous.binding.same_action(&binding))
            .map_or_else(ExecutorOperationId::random, |previous| Ok(previous.id))?;
        Ok(Self { id, binding })
    }
}

pub(in crate::root) enum ExecutorUnshieldQuote {
    Broadcaster(Box<PublicBroadcasterCostEstimate>),
    SelfBroadcast {
        cost: DesktopSelfBroadcastCostEstimate,
        gas_fee: SelfBroadcastGasFeeSelection,
    },
}

impl ExecutorUnshieldQuote {
    pub(in crate::root) fn covers(&self, prepared: &Self) -> bool {
        match (self, prepared) {
            (Self::Broadcaster(approved), Self::Broadcaster(current)) => {
                approved.broadcaster.railgun_address == current.broadcaster.railgun_address
                    && approved.action_token == current.action_token
                    && approved.fee_token == current.fee_token
                    && approved.fee_mode == current.fee_mode
                    && approved.native_top_up == current.native_top_up
                    && current.fee_amount <= approved.fee_amount
                    && current.total_private_spend <= approved.total_private_spend
                    && current.recipient_amount >= approved.recipient_amount
            }
            (
                Self::SelfBroadcast {
                    cost: approved,
                    gas_fee: approved_gas,
                },
                Self::SelfBroadcast {
                    cost: current,
                    gas_fee: current_gas,
                },
            ) => {
                let (
                    SelfBroadcastGasFeeSelection::Custom {
                        max_fee_per_gas: approved_fee,
                        max_priority_fee_per_gas: approved_tip,
                    },
                    SelfBroadcastGasFeeSelection::Custom {
                        max_fee_per_gas: current_fee,
                        max_priority_fee_per_gas: current_tip,
                    },
                ) = (approved_gas, current_gas)
                else {
                    return false;
                };
                current.gas_limit <= approved.gas_limit
                    && current.gas_cost.maximum_cost <= approved.gas_cost.maximum_cost
                    && current.protocol_fees == approved.protocol_fees
                    && current_fee <= approved_fee
                    && current_tip <= approved_tip
            }
            _ => false,
        }
    }
}

pub(in crate::root) struct ExecutorUnshieldApproval {
    binding: ExecutorUnshieldBinding,
    quote: ExecutorUnshieldQuote,
    session: Arc<WalletSession>,
    view: Arc<DesktopViewSession>,
    form_identity: gpui::EntityId,
}

impl ExecutorUnshieldApproval {
    fn matches(&self, draft: &UnshieldSpendDraft) -> bool {
        self.binding == ExecutorUnshieldBinding::from_draft(draft)
            && Arc::ptr_eq(&self.session, &draft.session)
            && Arc::ptr_eq(&self.view, &draft.view_session)
    }
}

pub(in crate::root) struct ExecutorUnshieldReview {
    pub(in crate::root) prepared: Arc<PreparedExecutorOperation>,
    pub(in crate::root) quote: ExecutorUnshieldQuote,
    binding: ExecutorUnshieldBinding,
    session: Arc<WalletSession>,
    view: Arc<DesktopViewSession>,
}

impl ExecutorUnshieldReview {
    pub(in crate::root) fn matches(&self, draft: &UnshieldSpendDraft) -> bool {
        self.binding == ExecutorUnshieldBinding::from_draft(draft)
            && Arc::ptr_eq(&self.session, &draft.session)
            && Arc::ptr_eq(&self.view, &draft.view_session)
            && self
                .session
                .executor_owner()
                .is_some_and(|owner| owner.validate_preparation(&self.prepared).is_ok())
    }

    pub(in crate::root) fn maximum_private_fee(&self) -> Option<U256> {
        match &self.quote {
            ExecutorUnshieldQuote::Broadcaster(quote) => Some(quote.fee_amount),
            ExecutorUnshieldQuote::SelfBroadcast { .. } => None,
        }
    }

    pub(in crate::root) fn broadcaster_min_gas_price(&self) -> Option<u128> {
        match &self.quote {
            ExecutorUnshieldQuote::Broadcaster(quote) => Some(quote.min_gas_price),
            ExecutorUnshieldQuote::SelfBroadcast { .. } => None,
        }
    }

    pub(in crate::root) const fn maximum_gas(&self) -> Option<u64> {
        match &self.quote {
            ExecutorUnshieldQuote::SelfBroadcast { cost, .. } => Some(cost.gas_limit),
            ExecutorUnshieldQuote::Broadcaster(_) => None,
        }
    }
}

impl WalletRoot {
    pub(in crate::root) fn request_executor_unshield_authorization(
        &mut self,
        key: UnshieldAssetKey,
        draft: &UnshieldSpendDraft,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.unshield_forms.get(&key) else {
            return;
        };
        let quote = match draft.delivery_mode {
            DeliveryMode::PublicBroadcaster
                if !form.cost_estimate_pending && !form.estimating_cost =>
            {
                draft
                    .cost_estimate
                    .clone()
                    .map(|quote| ExecutorUnshieldQuote::Broadcaster(Box::new(quote)))
            }
            DeliveryMode::SelfBroadcast if !form.sponsored_estimate_pending => {
                match (
                    &form.sponsored_funding_estimate,
                    draft.self_broadcast_initial_gas_fee,
                ) {
                    (
                        Some(SponsoredFundingEstimateState::PublicBalanceReady(estimate)),
                        Some((max_fee_per_gas, max_priority_fee_per_gas)),
                    ) => Some(ExecutorUnshieldQuote::SelfBroadcast {
                        cost: estimate.cost.clone(),
                        gas_fee: SelfBroadcastGasFeeSelection::Custom {
                            max_fee_per_gas,
                            max_priority_fee_per_gas,
                        },
                    }),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(quote) = quote else {
            self.set_unshield_form_error(
                key,
                "Wait for the current fee estimate before authorizing this unshield.",
                cx,
            );
            return;
        };
        let summary = self.executor_unshield_summary(draft, &quote, None);
        let approval = Arc::new(ExecutorUnshieldApproval {
            binding: ExecutorUnshieldBinding::from_draft(draft),
            quote,
            session: Arc::clone(&draft.session),
            view: Arc::clone(&draft.view_session),
            form_identity: form.recipient_input.entity_id(),
        });
        let execution = form.gateway_execution.clone();
        self.request_spend_authorization(
            SpendAuthorizationIntent::PrepareExecutorUnshield(key, approval, execution),
            summary,
            window,
            cx,
        );
    }

    pub(in crate::root) fn unshield_requires_executor(&self, draft: &UnshieldSpendDraft) -> bool {
        (draft.unwrap || draft.native_top_up.is_some())
            && !self.selected_wallet_source().is_hardware_derived()
            && draft.self_broadcast_funding != SelfBroadcastFundingMode::PrivateSponsorship
            && self
                .effective_chain_configs
                .get(draft.asset.chain_id)
                .and_then(wallet_ops::settings::EffectiveChainConfig::accepted_executor_profile)
                .is_some()
    }

    pub(in crate::root) fn prepare_executor_unshield_review(
        &mut self,
        key: UnshieldAssetKey,
        approval: Arc<ExecutorUnshieldApproval>,
        authorization: DesktopPrivateSpendAuthorization,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let preparation_started = Instant::now();
        tracing::info!(target: "executor_preparation", step = "review_preparation", "started");
        let Some(draft) = self.unshield_spend_draft(key, cx) else {
            return;
        };
        if !self.unshield_requires_executor(&draft)
            || !approval.matches(&draft)
            || self
                .unshield_forms
                .get(&key)
                .is_none_or(|form| form.recipient_input.entity_id() != approval.form_identity)
        {
            self.set_unshield_form_error(key, "The selected action changed. Review it again.", cx);
            return;
        }
        let Some(owner) = draft.session.executor_owner() else {
            return;
        };
        let delivery = match draft.delivery_mode {
            DeliveryMode::ManualCalldata => {
                self.set_unshield_form_error(key, "External-wallet export is unavailable for this stealth-account action. Select a supported delivery method.", cx);
                return;
            }
            DeliveryMode::PublicBroadcaster => {
                let candidates = self.current_public_broadcaster_candidates(
                    key.chain_id,
                    draft.fee_token,
                    draft.unwrap,
                    draft.native_top_up.is_some(),
                    draft.favorites_only_broadcasters,
                    draft.fee_policy,
                );
                let candidate = select_public_broadcaster_with_policy_and_trust(
                    &candidates,
                    &Self::public_broadcaster_submission_selection(
                        &draft.broadcaster_choice,
                        draft.cost_estimate.as_ref(),
                    ),
                    draft.fee_policy,
                    &self.public_broadcaster_trust_filter(draft.favorites_only_broadcasters),
                );
                match candidate {
                    Ok(candidate) => ExecutorDelivery::PublicBroadcaster(Box::new(candidate)),
                    Err(error) => {
                        self.set_unshield_form_error(key, error.to_string(), cx);
                        return;
                    }
                }
            }
            DeliveryMode::SelfBroadcast => {
                let Some(account) = self.selected_self_broadcast_gas_payer_account(
                    draft.self_broadcast_public_account_uuid.as_deref(),
                ) else {
                    return;
                };
                ExecutorDelivery::SelfBroadcast {
                    sender: account.address,
                    sponsored: false,
                }
            }
        };
        let Some(chain) = self.effective_chain_configs.get(key.chain_id).cloned() else {
            return;
        };
        if let Err(error) = delivery.admit(
            chain
                .accepted_executor_profile()
                .expect("executor profile checked"),
        ) {
            self.set_unshield_form_error(key, error.to_string(), cx);
            return;
        }
        let form = self.unshield_forms.get_mut(&key).expect("validated form");
        let binding = ExecutorUnshieldBinding::from_draft(&draft);
        let reservation = match ExecutorUnshieldOperation::for_action(
            form.executor_operation.as_ref(),
            binding.clone(),
        ) {
            Ok(operation) => operation,
            Err(error) => {
                self.set_unshield_form_error(key, error.to_string(), cx);
                return;
            }
        };
        let operation = reservation.id;
        let execution = form.gateway_execution.clone();
        if execution
            .as_ref()
            .is_some_and(|execution| !execution.require_prepared_review())
        {
            return;
        }
        form.executor_operation = Some(reservation);
        form.executor_review = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        form.sponsored_estimate_id = 0;
        form.sponsored_estimate_pending = false;
        form.generating = true;
        form.error = None;
        let form_identity = form.recipient_input.entity_id();
        let http = self.http.clone();
        let trust_filter = self.public_broadcaster_trust_filter(draft.favorites_only_broadcasters);
        let anchor_cache = Arc::clone(&self.public_broadcaster_anchor_cache);
        let queued_at = Instant::now();
        tracing::info!(target: "executor_preparation", step = "runtime_dispatch", "started");
        let approved_fee_amount = match &approval.quote {
            ExecutorUnshieldQuote::Broadcaster(quote) => Some(quote.fee_amount),
            ExecutorUnshieldQuote::SelfBroadcast { .. } => None,
        };
        let join = self.runtime.spawn(async move {
            tracing::info!(
                target: "executor_preparation",
                step = "runtime_dispatch",
                elapsed_ms = queued_at.elapsed().as_millis(),
                "finished"
            );
            let result = async {
                let mut assets = vec![
                    ExecutorAsset::Native,
                    ExecutorAsset::Erc20(draft.asset.token),
                ];
                if let Some(top_up) = &draft.native_top_up {
                    assets.push(ExecutorAsset::Erc20(top_up.wrapped_native_token));
                }
                let purpose_summary = format!(
                    "Unshield {} → {}{}",
                    private_amount_label(draft.amount, &draft.asset, false),
                    draft.recipient,
                    if draft.unwrap { " (unwrap)" } else { "" },
                );
                let prepared = Arc::new(
                    owner
                        .prepare_authorized_operation(
                            operation,
                            delivery,
                            &authorization,
                            &assets,
                            Some(&purpose_summary),
                        )
                        .await?,
                );
                let quote_started = Instant::now();
                tracing::info!(target: "executor_preparation", step = "quote_refresh", "started");
                let quote = match prepared.delivery() {
                    ExecutorDelivery::PublicBroadcaster(candidate) => {
                        let quote = estimate_desktop_unshield_public_broadcaster_cost(
                            DesktopUnshieldPublicBroadcasterEstimateRequest {
                                custom_fee_amount: draft.custom_fee_amount,
                                approved_fee_amount,
                                executor: Some(Arc::clone(&prepared)),
                                chain_id: key.chain_id,
                                effective_chain: chain,
                                session: Arc::clone(&draft.session),
                                token: draft.asset.token,
                                fee_token: draft.fee_token,
                                amount: draft.amount,
                                recipient: draft.recipient,
                                unwrap: draft.unwrap,
                                native_top_up: native_top_up_request_from_plan(
                                    draft.native_top_up.as_ref(),
                                ),
                                fee_rows: draft.fee_rows,
                                selection: PublicBroadcasterSelection::Specific {
                                    railgun_address: candidate.railgun_address.clone(),
                                },
                                fee_mode: draft.fee_mode,
                                fee_policy: draft.fee_policy,
                                trust_filter,
                                anchor_cache: Some(anchor_cache),
                            },
                            &http,
                        )
                        .await?;
                        ExecutorUnshieldQuote::Broadcaster(Box::new(quote))
                    }
                    ExecutorDelivery::SelfBroadcast { .. } => {
                        let quote =
                            quote_desktop_self_broadcast_gas_fee(key.chain_id, &chain, &http)
                                .await?;
                        let (max_fee_per_gas, max_priority_fee_per_gas) =
                            self_broadcast_initial_gas_values(
                                &draft.self_broadcast_gas_fee,
                                Some(quote),
                            )
                            .ok_or_else(|| eyre::eyre!("Gas fee quote is unavailable"))?;
                        let cost = estimate_desktop_unshield_self_broadcast_cost(
                            Some(&chain.gas),
                            &draft.session.unspent_utxos_for_executor(&prepared)?,
                            draft.asset.token,
                            draft.amount,
                            draft.fee_mode,
                            draft.unwrap,
                            draft.native_top_up.as_ref(),
                            quote,
                            max_fee_per_gas,
                            max_priority_fee_per_gas,
                        )?;
                        ExecutorUnshieldQuote::SelfBroadcast {
                            cost,
                            gas_fee: SelfBroadcastGasFeeSelection::Custom {
                                max_fee_per_gas,
                                max_priority_fee_per_gas,
                            },
                        }
                    }
                    ExecutorDelivery::ManualExport => {
                        unreachable!("manual delivery rejected before allocation")
                    }
                };
                tracing::info!(
                    target: "executor_preparation",
                    step = "quote_refresh",
                    elapsed_ms = quote_started.elapsed().as_millis(),
                    "finished"
                );
                Ok::<_, eyre::Report>(ExecutorUnshieldReview {
                    prepared,
                    quote,
                    binding,
                    session: draft.session,
                    view: draft.view_session,
                })
            }
            .await
            .map_err(|error| error.to_string());
            tracing::info!(
                target: "executor_preparation",
                step = "prepare_review_data",
                elapsed_ms = preparation_started.elapsed().as_millis(),
                success = result.is_ok(),
                "finished"
            );
            (authorization, result)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            tracing::info!(
                target: "executor_preparation",
                step = "receive_review_data",
                elapsed_ms = preparation_started.elapsed().as_millis(),
                success = result.is_ok(),
                "finished"
            );
            let _ = this.update_in(cx, |root, window, cx| {
                let Some(form) = root.unshield_forms.get_mut(&key) else {
                    return;
                };
                if form.recipient_input.entity_id() != form_identity
                    || form
                        .executor_operation
                        .as_ref()
                        .is_none_or(|reserved| reserved.id != operation)
                {
                    return;
                }
                form.generating = false;
                let Ok((authorization, result)) = result else {
                    root.set_unshield_form_error(
                        key,
                        "Account preparation stopped. Review the action again.",
                        cx,
                    );
                    if let Some(execution) = &execution {
                        root.reject_gateway_private_authorization(execution, cx);
                    }
                    return;
                };
                let review = match result {
                    Ok(review) => Arc::new(review),
                    Err(error) => {
                        root.set_unshield_form_error(key, error, cx);
                        if let Some(execution) = &execution {
                            root.reject_gateway_private_authorization(execution, cx);
                        }
                        return;
                    }
                };
                let Some(mut current) = root.unshield_spend_draft_for_prepared_review(key, cx)
                else {
                    if let Some(execution) = &execution {
                        root.reject_gateway_private_authorization(execution, cx);
                    }
                    return;
                };
                if !review.matches(&current)
                    || execution.as_ref().is_some_and(|execution| {
                        execution.snapshot().status
                            == wallet_ops::gateway::GatewayDraftStatus::Failed
                    })
                {
                    root.set_unshield_form_error(
                        key,
                        "The action changed during preparation. Review it again.",
                        cx,
                    );
                    if let Some(execution) = &execution {
                        root.reject_gateway_private_authorization(execution, cx);
                    }
                    return;
                }
                let form = root.unshield_forms.get_mut(&key).expect("current form");
                match &review.quote {
                    ExecutorUnshieldQuote::Broadcaster(quote) => {
                        form.cost_estimate = Some((**quote).clone());
                        current.cost_estimate.clone_from(&form.cost_estimate);
                    }
                    ExecutorUnshieldQuote::SelfBroadcast { cost, .. } => {
                        form.sponsored_funding_estimate =
                            Some(SponsoredFundingEstimateState::PublicBalanceReady(Box::new(
                                PublicBalanceFundingEstimate {
                                    chain_id: key.chain_id,
                                    cost: cost.clone(),
                                },
                            )));
                    }
                }
                form.gateway_estimated_at = Some(Instant::now());
                form.executor_review = Some(Arc::clone(&review));
                current.executor_review = Some(Arc::clone(&review));
                let intent =
                    SpendAuthorizationIntent::ExecutorUnshield(key, Arc::clone(&review), execution);
                if approval.matches(&current) && approval.quote.covers(&review.quote) {
                    if intent.approve_gateway_review(root) {
                        root.continue_authorized_spend(intent, authorization, window, cx);
                    }
                } else {
                    let summary = root.executor_unshield_summary(
                        &current,
                        &review.quote,
                        Some((review.prepared.context().executor, &approval.quote)),
                    );
                    root.open_prepared_spend_review(intent, summary, authorization, window, cx);
                }
                tracing::info!(
                    target: "executor_preparation",
                    step = "review_preparation",
                    elapsed_ms = preparation_started.elapsed().as_millis(),
                    "finished"
                );
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn executor_unshield_summary(
        &self,
        draft: &UnshieldSpendDraft,
        quote: &ExecutorUnshieldQuote,
        previous: Option<(Address, &ExecutorUnshieldQuote)>,
    ) -> SpendAuthorizationSummary {
        let previous_quote = previous.map(|(_, quote)| quote);
        let exact_native_amount = |amount| {
            format!(
                "{} {}",
                format_unshield_amount_input(amount, Some(18)),
                native_token_display_label(draft.asset.chain_id),
            )
        };
        let exact_token_amount = |token, amount| {
            format_exact_token_amount_for_display(
                draft.asset.chain_id,
                token,
                amount,
                Some(&self.effective_token_registry),
            )
        };
        let mut rows = vec![
            SpendAuthorizationSummaryRow::new("Recipient", draft.recipient.to_checksum(None))
                .with_shortened_copyable(),
            SpendAuthorizationSummaryRow::new("Delivery", draft.delivery_mode.label()),
            SpendAuthorizationSummaryRow::new(
                "Chain",
                railgun_ui::chain_name(draft.asset.chain_id).unwrap_or("Chain"),
            )
            .with_icon(
                railgun_ui::chain_icon_asset_path(draft.asset.chain_id)
                    .map(WalletIconSource::embedded),
            ),
            SpendAuthorizationSummaryRow::new(
                "Fees",
                match draft.fee_mode {
                    FeeHandlingMode::AddToAmount => "Added to the entered amount",
                    FeeHandlingMode::DeductFromAmount => "Deducted from the entered amount",
                },
            ),
        ];
        match quote {
            ExecutorUnshieldQuote::Broadcaster(quote) => {
                let approved = match previous_quote {
                    Some(ExecutorUnshieldQuote::Broadcaster(quote)) => Some(quote),
                    _ => None,
                };
                rows.push(
                    SpendAuthorizationSummaryRow::new(
                        "Maximum broadcaster fee",
                        format_token_amount_ceiling_for_display(
                            draft.asset.chain_id,
                            draft.fee_token,
                            quote.fee_amount,
                            Some(&self.effective_token_registry),
                        ),
                    )
                    .with_amount_change(
                        approved
                            .filter(|approved| approved.fee_token == quote.fee_token)
                            .map(|approved| approved.fee_amount),
                        quote.fee_amount,
                        true,
                        |amount| exact_token_amount(quote.fee_token, amount),
                    ),
                );
                rows.push(
                    SpendAuthorizationSummaryRow::new(
                        "Minimum recipient amount",
                        if draft.unwrap {
                            format_native_token_amount_for_display(
                                draft.asset.chain_id,
                                quote.recipient_amount,
                            )
                        } else {
                            private_amount_label(quote.recipient_amount, &draft.asset, false)
                        },
                    )
                    .with_amount_change(
                        approved
                            .filter(|approved| approved.action_token == quote.action_token)
                            .map(|approved| approved.recipient_amount),
                        quote.recipient_amount,
                        false,
                        |amount| {
                            if draft.unwrap {
                                exact_native_amount(amount)
                            } else {
                                private_amount_label(amount, &draft.asset, false)
                            }
                        },
                    ),
                );
                rows.push(
                    SpendAuthorizationSummaryRow::new(
                        if approved.is_some_and(|approved| {
                            approved.broadcaster.railgun_address
                                != quote.broadcaster.railgun_address
                        }) {
                            "Broadcaster (changed)"
                        } else {
                            "Broadcaster"
                        },
                        quote.broadcaster.railgun_address.clone(),
                    )
                    .with_shortened_copyable(),
                );
            }
            ExecutorUnshieldQuote::SelfBroadcast { cost, gas_fee } => {
                let approved = match previous_quote {
                    Some(ExecutorUnshieldQuote::SelfBroadcast { cost, gas_fee }) => {
                        Some((cost, gas_fee))
                    }
                    _ => None,
                };
                for fee in &cost.protocol_fees {
                    rows.push(
                        SpendAuthorizationSummaryRow::new(
                            "Protocol fee",
                            format_token_amount_ceiling_for_display(
                                draft.asset.chain_id,
                                fee.token,
                                fee.amount,
                                Some(&self.effective_token_registry),
                            ),
                        )
                        .with_amount_change(
                            approved.map(|(cost, _)| {
                                cost.protocol_fees
                                    .iter()
                                    .filter(|previous| previous.token == fee.token)
                                    .map(|fee| fee.amount)
                                    .sum()
                            }),
                            fee.amount,
                            true,
                            |amount| exact_token_amount(fee.token, amount),
                        ),
                    );
                }
                if draft.native_top_up.is_some() {
                    rows.push(SpendAuthorizationSummaryRow::new(
                        "Recipient receives",
                        private_unshield_recipient_amount_label(draft),
                    ));
                } else if draft.unwrap {
                    let receiver_amount =
                        |cost: &DesktopSelfBroadcastCostEstimate| match draft.fee_mode {
                            FeeHandlingMode::AddToAmount => draft.amount,
                            FeeHandlingMode::DeductFromAmount => draft.amount.saturating_sub(
                                cost.protocol_fees
                                    .iter()
                                    .filter(|fee| fee.token == draft.asset.token)
                                    .map(|fee| fee.amount)
                                    .sum(),
                            ),
                        };
                    rows.push(
                        SpendAuthorizationSummaryRow::new(
                            "Recipient receives",
                            format_native_token_amount_for_display(
                                draft.asset.chain_id,
                                receiver_amount(cost),
                            ),
                        )
                        .with_amount_change(
                            approved.map(|(cost, _)| receiver_amount(cost)),
                            receiver_amount(cost),
                            false,
                            exact_native_amount,
                        ),
                    );
                }
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Gas payer",
                    draft
                        .self_broadcast_gas_payer_display
                        .as_deref()
                        .unwrap_or("Selected Public account"),
                ));
                rows.push(
                    SpendAuthorizationSummaryRow::new(
                        "Maximum gas cost",
                        format_native_token_amount_ceiling_for_display(
                            draft.asset.chain_id,
                            cost.gas_cost.maximum_cost,
                        ),
                    )
                    .with_amount_change(
                        approved.map(|(cost, _)| cost.gas_cost.maximum_cost),
                        cost.gas_cost.maximum_cost,
                        true,
                        exact_native_amount,
                    ),
                );
                rows.push(
                    SpendAuthorizationSummaryRow::new("Gas limit", cost.gas_limit.to_string())
                        .with_amount_change(
                            approved.map(|(cost, _)| U256::from(cost.gas_limit)),
                            U256::from(cost.gas_limit),
                            true,
                            |amount| amount.to_string(),
                        ),
                );
                if let SelfBroadcastGasFeeSelection::Custom {
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                } = gas_fee
                {
                    let approved_gas = approved.and_then(|(_, gas_fee)| match gas_fee {
                        SelfBroadcastGasFeeSelection::Custom {
                            max_fee_per_gas,
                            max_priority_fee_per_gas,
                        } => Some((*max_fee_per_gas, *max_priority_fee_per_gas)),
                        SelfBroadcastGasFeeSelection::Auto => None,
                    });
                    rows.push(
                        SpendAuthorizationSummaryRow::new(
                            "Maximum fee per gas",
                            format!("{} gwei", format_gwei(*max_fee_per_gas)),
                        )
                        .with_amount_change(
                            approved_gas.map(|(fee, _)| U256::from(fee)),
                            U256::from(*max_fee_per_gas),
                            true,
                            |amount| {
                                format!("{} gwei", format_unshield_amount_input(amount, Some(9)))
                            },
                        ),
                    );
                    rows.push(
                        SpendAuthorizationSummaryRow::new(
                            "Maximum priority fee per gas",
                            format!("{} gwei", format_gwei(*max_priority_fee_per_gas)),
                        )
                        .with_amount_change(
                            approved_gas.map(|(_, tip)| U256::from(tip)),
                            U256::from(*max_priority_fee_per_gas),
                            true,
                            |amount| {
                                format!("{} gwei", format_unshield_amount_input(amount, Some(9)))
                            },
                        ),
                    );
                }
            }
        }
        if let Some(top_up) = &draft.native_top_up {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Recipient gas top-up",
                format_native_token_amount_ceiling_for_display(
                    draft.asset.chain_id,
                    top_up.native_amount,
                ),
            ));
        }
        let (title, detail) = if let Some((executor, _)) = previous {
            rows.push(
                SpendAuthorizationSummaryRow::new("Stealth account", executor.to_checksum(None))
                    .with_shortened_copyable(),
            );
            (
                "Unshield quote changed",
                "Review the updated terms before signing. Deltas show changes from your approval; higher fees and lower recipient amounts are highlighted.",
            )
        } else {
            ("Private unshield", "")
        };
        let summary =
            SpendAuthorizationSummary::new(title, detail, rows).requiring_explicit_review();
        if let Some(amount) = draft.custom_fee_amount {
            summary.with_custom_transaction_fee(super::fee_editor::custom_fee_label(
                draft.asset.chain_id,
                draft.fee_token,
                amount,
                &self.effective_token_registry,
            ))
        } else {
            summary
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_retry_keeps_its_account_for_fees_but_not_another_recipient() {
        let first = ExecutorUnshieldOperation::for_action(
            None,
            ExecutorUnshieldBinding {
                custom_fee_amount: None,
                chain_id: 1,
                token: Address::repeat_byte(1),
                recipient: Address::repeat_byte(2),
                amount: U256::from(1_000),
                unwrap: true,
                native_top_up: None,
                delivery: DeliveryMode::SelfBroadcast,
                funding: SelfBroadcastFundingMode::PublicBalance,
                payer: Some("public-gas-payer".into()),
                gas_fee: SelfBroadcastGasFeeSelection::Auto,
                fee_token: Address::repeat_byte(1),
                fee_mode: FeeHandlingMode::DeductFromAmount,
                broadcaster: BroadcasterChoice::Random,
                favorites_only: false,
                fee_policy: BroadcasterFeePolicy::default(),
            },
        )
        .unwrap();
        let mut repriced = first.binding.clone();
        repriced.custom_fee_amount = Some(U256::from(100));
        // A changed fee invalidates the reviewed binding, but a retry keeps its account.
        assert!(repriced != first.binding);
        repriced.gas_fee = SelfBroadcastGasFeeSelection::Custom {
            max_fee_per_gas: 20,
            max_priority_fee_per_gas: 1,
        };
        let retry = ExecutorUnshieldOperation::for_action(Some(&first), repriced).unwrap();
        assert_eq!(retry.id, first.id);

        let mut redirected = retry.binding.clone();
        redirected.recipient = Address::repeat_byte(3);
        let next = ExecutorUnshieldOperation::for_action(Some(&retry), redirected).unwrap();
        assert_ne!(next.id, first.id);
    }
}
