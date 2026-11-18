use super::*;

impl WalletRoot {
    pub(in crate::root) fn clean_governance_participants(&mut self) {
        let Some(wallet_id) = self.selected_wallet_id.as_deref() else {
            return;
        };
        if self.public_accounts.is_empty() {
            return;
        }
        let persisted = self
            .ui_state
            .governance_participants
            .get(wallet_id)
            .cloned()
            .unwrap_or_default();
        let resolution = normalize_participant_ids(&persisted, &self.public_accounts, wallet_id);
        if resolution.changed {
            self.ui_state
                .governance_participants
                .insert(wallet_id.to_owned(), resolution.uuids);
            self.save_ui_state();
        }
    }

    pub(in crate::root) fn governance_participants(&self) -> Vec<PublicAccountMetadata> {
        let Some(wallet_id) = self.selected_wallet_id.as_deref() else {
            return Vec::new();
        };
        let persisted = self
            .ui_state
            .governance_participants
            .get(wallet_id)
            .map_or(&[][..], Vec::as_slice);
        let resolution = normalize_participant_ids(persisted, &self.public_accounts, wallet_id);
        resolution
            .uuids
            .iter()
            .filter_map(|uuid| {
                self.public_accounts
                    .iter()
                    .find(|account| {
                        account.public_account_uuid == *uuid
                            && account.is_scoped_to_wallet(wallet_id)
                    })
                    .cloned()
            })
            .collect()
    }

    pub(in crate::root) fn governance_context_key(&self) -> GovernanceContextKey {
        let participants = self
            .governance_participants()
            .into_iter()
            .map(|account| GovernanceParticipantIdentity {
                uuid: account.public_account_uuid,
                address: account.address,
            })
            .collect();
        GovernanceContextKey {
            wallet_id: self.selected_wallet_id.as_ref().map(ToString::to_string),
            chain_id: self.last_evm_chain,
            participants,
        }
    }

    pub(in crate::root) fn invalidate_governance_context(&mut self) {
        self.governance.staking.invalidate();
        self.governance.proposal_participation.invalidate();
        self.governance.staking_tables.clear();
        self.governance.compact_position_details = None;
        self.governance.invalidate_action();
        self.governance.action_flow.selection = None;
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        self.governance.participant_time_tick_generation = None;
    }

    pub(in crate::root) fn open_proposal_action(
        &mut self,
        actor: Address,
        kind: ProposalActionKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.governance.invalidate_action();
        self.governance
            .action_flow
            .set_proposal_selection(ProposalActionSelection { actor, kind });
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        self.governance
            .proposal_action_amount_input
            .update(cx, |input, cx| {
                input.set_value("", window, cx);
                cx.notify();
            });
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(440.0));
        let dialog_max_height = crate::root::dialog_max_height(window);
        let content_width = crate::root::secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(crate::root::proposals::proposal_action_title(kind))
                .on_ok(|_, _, _| false)
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.close_proposal_action(cx);
                    });
                })
                .child(content_root.read(cx).render_proposal_action_dialog_content(
                    content_root.clone(),
                    content_width,
                    cx,
                ))
        });
        if matches!(kind, ProposalActionKind::CallVote)
            && let Some(proposal) = self.proposals.selected_proposal().cloned()
        {
            self.review_proposal_action(
                &proposal,
                ProposalActionSelection { actor, kind },
                None,
                window,
                cx,
            );
        }
        cx.notify();
    }

    pub(in crate::root) fn close_proposal_action(&mut self, cx: &mut Context<'_, Self>) {
        self.governance.invalidate_action();
        self.governance.action_flow.selection = None;
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        cx.notify();
    }

    pub(in crate::root) fn open_staking_action(
        &mut self,
        actor_uuid: &str,
        actor: Address,
        kind: StakingActionKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.governance.invalidate_action();
        let dialog_title = staking_action_title(&kind);
        let focus_input = match &kind {
            StakingActionKind::Stake => Some(self.governance.proposal_action_amount_input.clone()),
            StakingActionKind::Delegate { .. } => {
                Some(self.governance.staking_delegate_input.clone())
            }
            _ => None,
        };
        let selects_rewards = matches!(kind, StakingActionKind::RewardClaimSelected { .. });
        let reviews_immediately = !selects_rewards
            && !matches!(
                kind,
                StakingActionKind::Delegate { .. } | StakingActionKind::Stake
            );
        let selection = StakingActionSelection {
            actor_uuid: actor_uuid.to_owned(),
            actor,
            kind,
        };
        self.governance.action_flow.set_staking_selection(selection);
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        if selects_rewards {
            self.governance.action_flow.seed_reward_selection = true;
            self.governance.reward_gas_fee.reset_for_request(window, cx);
        }
        self.governance
            .proposal_action_amount_input
            .update(cx, |input, cx| {
                input.set_value("", window, cx);
                cx.notify();
            });
        self.governance
            .staking_delegate_input
            .update(cx, |input, cx| {
                input.set_value("", window, cx);
                cx.notify();
            });
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(if selects_rewards {
            REWARD_SELECTION_DIALOG_WIDTH
        } else {
            STAKING_ACTION_DIALOG_WIDTH
        });
        let dialog_max_height = crate::root::dialog_max_height(window);
        let content_width = crate::root::secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text(dialog_title))
                .on_ok(|_, _, _| false)
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.close_staking_action(cx);
                    });
                })
                .child(content_root.read(cx).render_staking_action_dialog_content(
                    content_root.clone(),
                    content_width,
                    cx,
                ))
        });
        if let Some(focus_input) = focus_input {
            cx.defer_in(window, move |_root, window, cx| {
                focus_input.read(cx).focus_handle(cx).focus(window, cx);
            });
        }
        if reviews_immediately {
            self.review_staking_action(window, cx);
        } else if selects_rewards {
            self.refresh_governance_gas_fee_quote(cx);
            self.start_reward_selection_estimate(Duration::ZERO, cx);
        }
        cx.notify();
    }

    pub(in crate::root) fn governance_gas_fee_changed(&mut self, cx: &mut Context<'_, Self>) {
        if self.governance.action_flow.pending
            || self
                .governance
                .action_flow
                .reward_claim_selection_tokens()
                .is_none()
        {
            return;
        }
        self.governance.reward_gas_fee.error = self
            .governance
            .reward_gas_fee
            .selection(cx)
            .err()
            .map(Arc::from);
        self.governance.action_flow.error = None;
        // The form derives costs from cached gas. Price edits leave the plan and selection intact.
        cx.notify();
    }

    pub(in crate::root) fn set_governance_gas_fee_mode(
        &mut self,
        mode: Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.governance.action_flow.pending
            || self
                .governance
                .action_flow
                .reward_claim_selection_tokens()
                .is_none()
            || self.governance.reward_gas_fee.mode == mode
        {
            return;
        }
        let gas_fee = &mut self.governance.reward_gas_fee;
        if mode == Eip1559GasFeeMode::Custom {
            gas_fee.seed_custom_from_auto_if_empty(window, cx);
        }
        gas_fee.mode = mode;
        self.governance_gas_fee_changed(cx);
    }

    pub(in crate::root) fn customize_governance_gas_fee_from_auto(
        &mut self,
        target: Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.governance.action_flow.pending
            || self
                .governance
                .action_flow
                .reward_claim_selection_tokens()
                .is_none()
        {
            return;
        }
        let gas_fee = &mut self.governance.reward_gas_fee;
        if !gas_fee.overwrite_custom_from_auto(window, cx) {
            return;
        }
        let focus_input = match target {
            Eip1559GasFeeEditTarget::MaxFee => gas_fee.max_fee_input.clone(),
            Eip1559GasFeeEditTarget::MaxTip => gas_fee.max_priority_fee_input.clone(),
        };
        gas_fee.mode = Eip1559GasFeeMode::Custom;
        self.governance_gas_fee_changed(cx);
        focus_input.read(cx).focus_handle(cx).focus(window, cx);
    }

    pub(in crate::root) fn refresh_governance_gas_fee_quote(&mut self, cx: &mut Context<'_, Self>) {
        if self.governance.action_flow.pending
            || self
                .governance
                .action_flow
                .reward_claim_selection_tokens()
                .is_none()
            || self.governance.reward_gas_fee.refreshing
        {
            return;
        }
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        let context_key = self.governance_context_key();
        let gas_fee = &mut self.governance.reward_gas_fee;
        gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
        gas_fee.refreshing = true;
        gas_fee.quote_error = None;
        let refresh_id = gas_fee.refresh_id;
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let result = wallet_ops::quote_public_action_gas_fee_bundle_with_profile(
                chain_id,
                &effective_chain,
                wallet_ops::PublicShieldTransactionProfile::Railoxide,
                wallet_ops::PublicAssetId::Native,
                &http,
            )
            .await;
            let _ = this.update(cx, |root, cx| {
                if root.governance.reward_gas_fee.refresh_id != refresh_id
                    || root.governance_context_key() != context_key
                {
                    return;
                }
                let gas_fee = &mut root.governance.reward_gas_fee;
                gas_fee.refreshing = false;
                match result {
                    Ok(bundle) => gas_fee.quote = Some(bundle.standard),
                    Err(_) => {
                        gas_fee.quote_error = Some(Arc::from(if gas_fee.quote.is_some() {
                            "Gas quote refresh failed; using the last successful quote."
                        } else {
                            "Gas quote is unavailable."
                        }));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Add or drop one reward in the open claim selection and re-estimate the selected call.
    pub(in crate::root) fn set_reward_claim_selection_token(
        &mut self,
        token: Address,
        selected: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .governance
            .action_flow
            .set_reward_claim_selection_token(token, selected)
        {
            return;
        }
        self.governance.action_flow.seed_reward_selection = false;
        self.start_reward_selection_estimate(REWARD_SELECTION_ESTIMATE_DEBOUNCE, cx);
        cx.notify();
    }

    /// Replace the open claim selection and re-estimate the selected call.
    pub(in crate::root) fn set_reward_claim_selection_all(
        &mut self,
        tokens: Vec<Address>,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .governance
            .action_flow
            .set_reward_claim_selection_all(tokens)
        {
            return;
        }
        self.governance.action_flow.seed_reward_selection = false;
        self.start_reward_selection_estimate(REWARD_SELECTION_ESTIMATE_DEBOUNCE, cx);
        cx.notify();
    }

    /// Plan and price the exact selected claim. A result is stored only while the selection it
    /// was requested for is still the open one.
    fn start_reward_selection_estimate(&mut self, debounce: Duration, cx: &Context<'_, Self>) {
        let generation = self
            .governance
            .action_flow
            .begin_reward_selection_estimate();
        let Some(selection) = self.governance.action_flow.staking_selection().cloned() else {
            return;
        };
        let StakingActionKind::RewardClaimSelected { tokens } = selection.kind else {
            return;
        };
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        let context_key = self.governance_context_key();
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        let actor = selection.actor;
        let operation = self.governance.action_flow.operation(actor, &self.http);
        let actor_uuid = selection.actor_uuid;
        let mut available_tokens = governance_contracts(chain_id)
            .map_or(&[][..], |contracts| contracts.reward_tokens)
            .iter()
            .filter(|entry| {
                self.governance
                    .staking
                    .positive_reward_evidence(&actor_uuid, entry.token)
                    .is_some()
            })
            .map(|entry| entry.token)
            .collect::<Vec<_>>();
        available_tokens.sort_unstable();
        cx.spawn(async move |this, cx| {
            if !debounce.is_zero() {
                cx.background_executor().timer(debounce).await;
            }
            let current = this.update(cx, |root, _cx| {
                (root.governance.action_flow.reward_selection_generation == generation
                    && root.governance.action_flow.reward_claim_selection_tokens()
                        == Some(tokens.as_slice()))
                .then(|| {
                    root.governance.staking.cached_reward_evidence_at(
                        &context_key,
                        &actor_uuid,
                        actor,
                        &available_tokens,
                        Instant::now(),
                    )
                })
            });
            let Ok(Some(cached)) = current else {
                return;
            };
            let result = async {
                // The action's one client carries every governance read and estimate.
                let (route, client) =
                    action_governance_route(chain_id, &effective_chain, &operation)
                        .await
                        .map_err(Arc::<str>::from)?;
                let available = match cached {
                    Some(evidence) => evidence,
                    None => fetch_reward_selection_evidence(
                        chain_id,
                        actor,
                        &available_tokens,
                        &route,
                        &http,
                    )
                    .await
                    .map_err(Arc::<str>::from)?,
                };
                estimate_reward_selection(
                    chain_id,
                    actor,
                    &tokens,
                    available,
                    &route,
                    &effective_chain,
                    &http,
                    &client,
                )
                .await
            }
            .await;
            let _ = this.update(cx, |root, cx| {
                if root
                    .governance
                    .action_flow
                    .apply_reward_selection_estimate(generation, result)
                {
                    if root.governance.action_flow.seed_reward_selection {
                        root.governance.action_flow.seed_reward_selection = false;
                        if let Some(RewardSelectionEstimateState::Estimated(estimate)) =
                            &root.governance.action_flow.reward_selection_estimate
                        {
                            let selected = default_reward_claim_selection_for(
                                &root.governance.staking,
                                chain_id,
                                &actor_uuid,
                                &available_tokens,
                                estimate,
                                root.governance.reward_gas_fee.quote,
                                &root.public_broadcaster_anchor_cache,
                            );
                            if selected != tokens {
                                root.governance
                                    .action_flow
                                    .set_reward_claim_selection_all(selected);
                                root.start_reward_selection_estimate(Duration::ZERO, cx);
                            }
                        }
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn render_staking_action_dialog_content(
        &self,
        root: Entity<Self>,
        content_width: gpui::Pixels,
        cx: &App,
    ) -> gpui::Div {
        let stale_content = |close_root: Entity<Self>| {
            div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .child(Alert::error(
                    "governance-staking-action-stale-selection",
                    "The selected staking action is no longer available. Refresh before trying again.",
                ))
                .child(
                    div().flex().justify_end().child(
                        app_button_base("governance-staking-action-stale-cancel")
                            .ghost()
                            .small()
                            .child("Cancel")
                            .on_click(move |_event, window, cx| {
                                close_root.update(cx, Self::close_staking_action);
                                window.close_dialog(cx);
                            }),
                    ),
                )
        };
        let Some(selection) = self.governance.action_flow.staking_selection() else {
            return stale_content(root);
        };
        let context_key = self.governance_context_key();
        if self.governance.staking.key.as_ref() != Some(&context_key)
            || !self.governance.staking.action_selection_ready(selection)
        {
            return stale_content(root);
        }
        render_staking_action_form(&root, self, content_width, cx)
            .unwrap_or_else(|| stale_content(root))
    }

    pub(in crate::root) fn review_staking_action(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.governance.action_flow.pending {
            return;
        }
        let Some(selection) = self.governance.action_flow.staking_selection().cloned() else {
            return;
        };
        let context_key = self.governance_context_key();
        if self.governance.staking.key.as_ref() != Some(&context_key)
            || !self.governance.staking.action_selection_ready(&selection)
        {
            self.governance.action_flow.error = Some(Arc::from(
                "Refresh staking data before reviewing this action",
            ));
            cx.notify();
            return;
        }
        let Some(wallet_id) = self.selected_wallet_id.clone() else {
            self.governance.action_flow.error =
                Some(Arc::from("Select a wallet before reviewing this action"));
            cx.notify();
            return;
        };
        let Some(account) = self.governance_participants().into_iter().find(|account| {
            account.public_account_uuid == selection.actor_uuid
                && account.address == selection.actor
        }) else {
            self.governance.action_flow.error =
                Some(Arc::from("Selected Public account is no longer enrolled"));
            cx.notify();
            return;
        };
        if account.status != PublicAccountStatus::Active {
            self.governance.action_flow.error =
                Some(Arc::from("Inactive participant accounts are read-only"));
            cx.notify();
            return;
        }
        let Some(view_session) = self.view_session.clone() else {
            self.governance.action_flow.error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.governance.action_flow.error =
                Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return;
        };
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        let actor_source = account.source;
        let amount_input = self
            .governance
            .proposal_action_amount_input
            .read(cx)
            .value()
            .to_string();
        let delegate_input = self
            .governance
            .staking_delegate_input
            .read(cx)
            .value()
            .to_string();
        let token_decimals = governance_contracts(chain_id)
            .and_then(|contracts| {
                self.effective_token_registry
                    .get(chain_id, &contracts.governance_token)
            })
            .map(|token| token.decimals);
        let reward_evidence_mode = match &selection.kind {
            StakingActionKind::RewardClaimSelected { tokens } => {
                if tokens.is_empty() {
                    self.governance.action_flow.error =
                        Some(Arc::from("Select at least one reward to claim"));
                    cx.notify();
                    return;
                }
                reward_evidence_mode_for_selection(
                    self.governance
                        .action_flow
                        .reward_selection_estimate
                        .as_ref(),
                    tokens,
                    self.governance.staking.cached_reward_evidence_at(
                        &context_key,
                        &selection.actor_uuid,
                        selection.actor,
                        tokens,
                        Instant::now(),
                    ),
                )
            }
            _ => RewardEvidenceMode::Fresh,
        };
        let gas_fee_selection = if matches!(
            selection.kind,
            StakingActionKind::RewardClaimSelected { .. }
        ) {
            match self.governance.reward_gas_fee.selection(cx) {
                Ok(selection) => selection,
                Err(error) => {
                    self.governance.reward_gas_fee.error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            }
        } else {
            PublicActionGasFeeSelection::Auto
        };
        let operation = self
            .governance
            .action_flow
            .operation(selection.actor, &self.http);
        let generation = self.governance.action_flow.generation.wrapping_add(1);
        self.governance.action_flow.generation = generation;
        self.governance.action_flow.recipe = Some(GovernanceDraftRecipe::Staking {
            selection: selection.clone(),
            context_key: context_key.clone(),
            amount_input: amount_input.clone(),
            delegate_input: delegate_input.clone(),
            token_decimals,
        });
        self.governance.action_flow.pending = true;
        self.governance.action_flow.error = None;
        self.governance.action_flow.draft = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = Box::pin(build_staking_action_draft(
                selection.clone(),
                context_key.clone(),
                wallet_id.clone(),
                actor_source,
                amount_input.clone(),
                delegate_input.clone(),
                view_session,
                vault_store,
                token_decimals,
                effective_chain.clone(),
                http.clone(),
                operation,
                reward_evidence_mode,
                gas_fee_selection,
            ))
            .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.governance.action_flow.generation != generation
                    || !root.governance.action_flow.staking_recipe_matches(
                        &selection,
                        &context_key,
                        &amount_input,
                        &delegate_input,
                        token_decimals,
                    )
                    || root.governance_context_key() != context_key.clone()
                {
                    return;
                }
                root.governance.action_flow.pending = false;
                match result {
                    Ok(draft) => root.authorize_prepared_governance_draft(draft, window, cx),
                    Err(error) => root.governance.action_flow.error = Some(Arc::from(error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root) fn close_staking_action(&mut self, cx: &mut Context<'_, Self>) {
        self.governance.invalidate_action();
        self.governance.compact_position_details = None;
        self.governance.action_flow.selection = None;
        self.governance.action_flow.draft = None;
        self.governance.action_flow.error = None;
        self.governance.action_flow.pending = false;
        cx.notify();
    }

    pub(super) fn set_compact_position_details(
        &mut self,
        key: Option<(String, U256)>,
        cx: &mut Context<'_, Self>,
    ) {
        self.governance.compact_position_details = key;
        let tables = self
            .governance
            .staking_tables
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for table in tables {
            table.update(cx, TableState::refresh);
        }
        cx.notify();
    }
}
