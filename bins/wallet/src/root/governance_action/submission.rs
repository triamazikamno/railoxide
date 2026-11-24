#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn review_proposal_action(
        &mut self,
        proposal: &GovernanceProposal,
        selection: ProposalActionSelection,
        amount: Option<U256>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.governance.action_flow.pending {
            return;
        }
        let key = proposal_participation_key(proposal, self.governance_context_key());
        if !self.proposal_participation_key_matches(proposal, &key) {
            self.governance.invalidate_action();
            self.governance.action_flow.selection = None;
            self.governance.action_flow.draft = None;
            self.governance.action_flow.error = Some(Arc::from(
                "Selected proposal changed; refresh before reviewing this action",
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
        let Some(account) = self
            .governance_participants()
            .into_iter()
            .find(|account| account.address == selection.actor)
        else {
            self.governance.action_flow.error =
                Some(Arc::from("Selected Public account is no longer enrolled"));
            cx.notify();
            return;
        };
        self.begin_governance_review(
            proposal.clone(),
            selection,
            amount,
            &account,
            wallet_id,
            window,
            cx,
        );
    }

    fn begin_governance_review(
        &mut self,
        proposal: GovernanceProposal,
        selection: ProposalActionSelection,
        amount: Option<U256>,
        account: &PublicAccountMetadata,
        wallet_id: Arc<str>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
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
        let key = proposal_participation_key(&proposal, self.governance_context_key());
        let expected_proposal = proposal.clone();
        let generation = self.governance.action_flow.generation.wrapping_add(1);
        self.governance.action_flow.generation = generation;
        self.governance.action_flow.recipe = Some(GovernanceDraftRecipe::Proposal {
            proposal: Box::new(expected_proposal.clone()),
            key: key.clone(),
            selection,
            amount,
        });
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let http = self.http.clone();
        let operation = self
            .governance
            .action_flow
            .operation(selection.actor, &self.http);
        let actor_uuid: Arc<str> = Arc::from(account.public_account_uuid.clone());
        let actor_source = account.source;
        self.governance.action_flow.pending = true;
        self.governance.action_flow.error = None;
        self.governance.action_flow.draft = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = build_governance_spend_draft(
                proposal,
                key.clone(),
                selection,
                amount,
                wallet_id,
                actor_uuid,
                actor_source,
                view_session,
                vault_store,
                effective_chain,
                http,
                operation,
                PublicActionGasFeeSelection::Auto,
            )
            .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.governance.action_flow.generation != generation
                    || !root
                        .governance
                        .action_flow
                        .proposal_recipe_matches(&key, selection, amount)
                    || root.governance_context_key() != key.context.clone()
                    || !root.proposal_participation_key_matches(&expected_proposal, &key)
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

    pub(in crate::root) fn submit_governance_authorized(
        &mut self,
        draft: GovernanceSpendDraft,
        spend_authorization: wallet_ops::DesktopPrivateSpendAuthorization,
        #[cfg(feature = "hardware")] window: &mut Window,
        #[cfg(not(feature = "hardware"))] window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(chain_id) = self.selected_evm_chain() else {
            return;
        };
        let Ok(resolved_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        if self.public_action_in_flight() {
            return;
        }
        let GovernanceSpendDraft {
            target,
            actor_uuid,
            actor: _,
            actor_source,
            context,
            resolved,
            review,
            proposal_review: _,
            staking_review: _,
            estimate,
            estimate_completed_at: _,
            gas_fee,
            view_session,
            vault_store,
            workflow,
            continuation,
            recipe,
            operation,
        } = draft;
        let continuation_operation = operation.clone();
        let submitted_chain = resolved_chain.clone();
        let continuation_view_session = view_session.clone();
        let continuation_vault_store = vault_store.clone();
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase = view_session.hardware_profile_session().and_then(|session| {
            self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
        });
        #[cfg(not(feature = "hardware"))]
        let trezor_app_passphrase = None;
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider = if actor_source == PublicAccountSource::HardwareDerived {
            Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
        } else {
            None
        };
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_provider = None;
        let chain_id = context.chain_id;
        let transaction_tracking =
            match self.public_transaction_tracking_context(chain_id, &actor_uuid) {
                Ok(context) => context,
                Err(error) => {
                    self.governance.action_flow.error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            };
        let active_wallet_id = self.selected_wallet_id.clone();
        let active_wallet_generation = self.active_wallet_generation;
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let progress_step = Self::governance_progress_step(
            &resolved.intent,
            continuation
                .as_ref()
                .map(|continuation| match continuation {
                    GovernanceContinuation::Reward { progress, .. } => progress.confirmed().len(),
                }),
        );
        let reward_progress_states = match (&workflow, continuation.as_ref()) {
            (None, Some(GovernanceContinuation::Reward { progress, .. })) => {
                governance_reward_progress_steps(progress, &resolved.intent)
            }
            _ => None,
        };
        let generation = if let Some(states) = reward_progress_states {
            self.start_public_action_progress_with_states(
                crate::root::public_action::PublicActionMode::Send,
                states,
                "Governance".to_owned(),
                None,
                actor_source,
                Some(command_tx),
                Some((estimate.max_fee_per_gas, estimate.max_priority_fee_per_gas)),
                true,
            )
        } else {
            let progress_steps = match &workflow {
                Some(GovernanceWorkflow::StakeApproval(_)) => vec![
                    PublicActionProgressStep::GovernanceApprove,
                    PublicActionProgressStep::Stake,
                ],
                Some(GovernanceWorkflow::UndelegateThenUnlock(_)) => vec![
                    PublicActionProgressStep::Undelegate,
                    PublicActionProgressStep::Unlock,
                ],
                None => vec![progress_step],
            };
            self.start_public_action_progress_with_steps(
                crate::root::public_action::PublicActionMode::Send,
                progress_steps,
                "Governance".to_owned(),
                None,
                actor_source,
                Some(command_tx),
                Some((estimate.max_fee_per_gas, estimate.max_priority_fee_per_gas)),
                true,
            )
        };
        self.public_form.action_progress_title_override = Some(Arc::from("Governance action"));
        self.public_form.sending = true;
        let progress_tx = Self::follow_public_action_progress(
            generation,
            chain_id,
            active_wallet_id.clone(),
            event_rx,
            None,
            window,
            cx,
        );
        let public_send = PublicSendRequest {
            executor_owner: self.executor_owner_for_public_chain(chain_id),
            transaction_tracking: Some(transaction_tracking),
            chain_id,
            effective_chain: resolved_chain,
            view_session,
            vault_store,
            authorization: Some(spend_authorization),
            trezor_app_passphrase,
            trezor_pin_matrix_provider,
            public_account_uuid: actor_uuid.to_string(),
            intent: resolved.raw.clone(),
            advanced_authorization: Some(PublicAdvancedTransactionAuthorization {
                payload_fingerprint: estimate.payload_fingerprint,
                gas_limit: estimate.gas_limit,
            }),
            reviewed_gas_limit: None,
            gateway_origin: None,
            gas_fee,
            command_rx: Some(command_rx),
            event_tx: Some(event_tx),
        };
        let request = GovernanceSubmissionRequest {
            review,
            rebuilt: resolved,
            estimate,
            public_send,
            progress_step,
        };
        let http = self.http.clone();
        let submitted_target = target;
        let join = self.spawn_public_transaction_submission(chain_id, async move {
            let client = operation.client().await?;
            if let Some(workflow) = workflow {
                Box::pin(wallet_ops::submit_governance_workflow_with_progress(
                    GovernanceWorkflowRequest {
                        initial: request,
                        workflow,
                    },
                    &http,
                    &client,
                    move |update| {
                        let _ = progress_tx.send(update);
                    },
                ))
                .await
                .map(|result| result.transactions)
            } else {
                wallet_ops::submit_governance_action_with_progress(
                    request,
                    &http,
                    &client,
                    move |update| {
                        let _ = progress_tx.send(update);
                    },
                )
                .await
                .map(|result| vec![result])
            }
        });
        self.public_form.action_task_abort_handle = join.abort_handle();
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if let (
                    Ok(Ok(_)),
                    Some(wallet_id),
                    GovernanceDraftRecipe::Proposal { proposal, .. },
                ) = (&result, active_wallet_id.as_deref(), &recipe)
                {
                    root.record_governance_action_success(
                        wallet_id,
                        active_wallet_generation,
                        &submitted_target,
                        proposal,
                        context.actor,
                        &submitted_chain,
                        &continuation_operation,
                        cx,
                    );
                }
                if root.selected_wallet_id != active_wallet_id
                    || root.selected_evm_chain() != Some(chain_id)
                {
                    return;
                }
                if !crate::root::public_action::public_action_accepts_update(
                    root.public_form.action_generation,
                    generation,
                    root.public_form.action_stopped,
                ) {
                    return;
                }
                root.public_form.sending = false;
                root.public_form.action_task_abort_handle = None;
                match result {
                    Ok(Ok(transactions)) => {
                        let hash = transactions
                            .last()
                            .and_then(|result| result.tx.tx_hash.parse::<B256>().ok());
                        if let Some(continuation) = continuation {
                            root.start_governance_continuation(
                                continuation,
                                submitted_target.clone(),
                                actor_source,
                                context.clone(),
                                continuation_view_session.clone(),
                                continuation_vault_store.clone(),
                                recipe.clone(),
                                continuation_operation.clone(),
                                hash,
                                window,
                                cx,
                            );
                        } else {
                            root.refresh_governance_after_target(&submitted_target, cx);
                        }
                    }
                    Ok(Err(error)) => {
                        root.fail_public_action_progress(
                            generation,
                            format_report_chain(&error),
                            cx,
                        );
                    }
                    Err(error) => {
                        root.fail_public_action_progress(
                            generation,
                            format!("Governance action task failed: {error}"),
                            cx,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn governance_progress_step(
        intent: &GovernanceActionIntent,
        reward_index: Option<usize>,
    ) -> PublicActionProgressStep {
        match intent {
            GovernanceActionIntent::Sponsor { .. } => PublicActionProgressStep::Sponsor,
            GovernanceActionIntent::Unsponsor { .. } => PublicActionProgressStep::Unsponsor,
            GovernanceActionIntent::CallVote { .. } => PublicActionProgressStep::CallVote,
            GovernanceActionIntent::Yay { .. } | GovernanceActionIntent::Nay { .. } => {
                PublicActionProgressStep::Vote
            }
            GovernanceActionIntent::GovernanceTokenApproval { .. } => {
                PublicActionProgressStep::GovernanceApprove
            }
            GovernanceActionIntent::Stake { .. } => PublicActionProgressStep::Stake,
            GovernanceActionIntent::Delegate { .. } => PublicActionProgressStep::Delegate,
            GovernanceActionIntent::Undelegate { .. } => PublicActionProgressStep::Undelegate,
            GovernanceActionIntent::Unlock { .. } => PublicActionProgressStep::Unlock,
            GovernanceActionIntent::PrincipalClaim { .. } => {
                PublicActionProgressStep::PrincipalClaim
            }
            GovernanceActionIntent::RewardClaim { .. } => {
                PublicActionProgressStep::RewardClaim(reward_index.unwrap_or_default() as u32)
            }
        }
    }

    /// Records a successful proposal action submitted in the current wallet session, whatever
    /// chain, tab or progress is shown now. It drops the actor's pre-action rows on the acted
    /// contract and the cached pages showing the proposal, which the action made stale (a called
    /// vote changes its voting interval), then reads the fresh proposal and the actor's
    /// participation once over the action's own route, which already carried this actor's
    /// transaction for this proposal. A failed read stores nothing and leaves the actor to be
    /// shown on request.
    pub(in crate::root) fn record_governance_action_success(
        &mut self,
        wallet_id: &str,
        wallet_generation: u64,
        target: &GovernanceRefreshTarget,
        proposal: &GovernanceProposal,
        actor: Address,
        effective_chain: &wallet_ops::settings::EffectiveChainConfig,
        operation: &wallet_ops::OperationHttpClientSlot,
        cx: &Context<'_, Self>,
    ) {
        let GovernanceRefreshTarget::Proposal(key) = target else {
            return;
        };
        if !self.is_active_wallet_generation(wallet_id, wallet_generation) {
            return;
        }
        let read = self
            .governance
            .proposal_participation
            .invalidate_actor_on_contract(key, actor);
        self.evict_acted_proposal(key, cx);
        let chain_id = key.context.chain_id;
        let wallet_id = wallet_id.to_owned();
        let proposal = proposal.clone();
        let effective_chain = effective_chain.clone();
        let operation = operation.clone();
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let state = match crate::root::governance::action_governance_route(
                chain_id,
                &effective_chain,
                &operation,
            )
            .await
            {
                Ok((route, _)) => {
                    read_proposal_action_state(chain_id, &proposal, actor, &route, &http).await
                }
                Err(error) => Err(error),
            };
            let Ok((fresh, _, _, participation)) = state else {
                return;
            };
            let _ = this.update(cx, |root, cx| {
                if root.is_active_wallet_generation(&wallet_id, wallet_generation)
                    && root.governance.proposal_participation.apply_action_read(
                        chain_id,
                        &fresh,
                        actor,
                        read,
                        participation,
                    )
                {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(in crate::root) fn refresh_governance_after_target(
        &mut self,
        target: &GovernanceRefreshTarget,
        cx: &mut Context<'_, Self>,
    ) {
        // A proposal action's participation and page were refreshed when its success was recorded.
        if let GovernanceRefreshTarget::Staking(key) = target {
            if self.governance.tab == crate::root::governance::GovernanceTab::Staking
                && self.governance_context_key() == *key
            {
                self.start_staking_refresh(cx);
            }
            self.refresh_selected_proposal_page(cx);
        }
    }
}
