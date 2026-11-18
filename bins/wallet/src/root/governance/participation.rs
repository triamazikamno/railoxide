use super::*;

impl WalletRoot {
    pub(in crate::root) fn start_governance_continuation(
        &mut self,
        continuation: GovernanceContinuation,
        target: GovernanceRefreshTarget,
        actor_source: wallet_ops::vault::PublicAccountSource,
        context: GovernanceActionContext,
        view_session: Arc<wallet_ops::vault::DesktopViewSession>,
        vault_store: Arc<wallet_ops::vault::DesktopVaultStore>,
        recipe: GovernanceDraftRecipe,
        operation: wallet_ops::OperationHttpClientSlot,
        confirmed_hash: Option<B256>,
        #[cfg(feature = "hardware")] window: &Window,
        #[cfg(not(feature = "hardware"))] window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let GovernanceContinuation::Reward {
            mut progress,
            evidence,
        } = continuation;
        let GovernanceRefreshTarget::Staking(context_key) = target.clone() else {
            self.refresh_governance_after_target(&target, cx);
            return;
        };
        let GovernanceDraftRecipe::Staking { selection, .. } = &recipe else {
            return;
        };
        let actor_uuid: Arc<str> = Arc::from(selection.actor_uuid.clone());
        let Ok(effective_chain) = self
            .effective_chain_configs
            .enabled(context.chain_id)
            .cloned()
        else {
            return;
        };
        let http = self.http.clone();
        let generation = self.governance.action_flow.generation.wrapping_add(1);
        self.governance.action_flow.generation = generation;
        self.governance.action_flow.recipe = Some(recipe.clone());
        self.governance.action_flow.pending = true;
        self.governance.action_flow.draft = None;
        cx.spawn_in(window, async move |this, cx| {
            let result: Result<Option<GovernanceSpendDraft>, String> = Box::pin(async {
                let contracts = governance_contracts(context.chain_id)
                    .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
                let confirmed_hash = confirmed_hash
                    .ok_or_else(|| "Reward transaction hash was unavailable".to_owned())?;
                let step = wallet_ops::next_reward_claim_batch_step(&progress, &evidence)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Reward claim sequence is complete".to_owned())?;
                wallet_ops::confirm_reward_claim_batch_step(
                    &mut progress,
                    &step,
                    &evidence,
                    confirmed_hash,
                )
                .map_err(|error| error.to_string())?;
                // The continuation reads fresh state over the submitted action's client.
                let (route, client) =
                    action_governance_route(context.chain_id, &effective_chain, &operation).await?;
                let continued = read_reward_continuation(
                    context.chain_id,
                    context.actor,
                    &progress.plan.reward_tokens,
                    &route,
                    &effective_chain,
                    &http,
                    &client,
                )
                .await?;
                let Some((fresh, fresh_steps)) = continued else {
                    return Ok(None);
                };
                let rebuilt = wallet_ops::rebuild_reward_claim_batch_continuation(
                    &progress,
                    &fresh,
                    &fresh_steps,
                )
                .map_err(|error| error.to_string())?;
                let next = wallet_ops::next_reward_claim_batch_step(&rebuilt, &fresh)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Reward claim sequence is complete".to_owned())?;
                build_typed_governance_spend_draft(
                    target.clone(),
                    Arc::from(context.private_wallet_uuid.clone()),
                    actor_uuid,
                    context.actor,
                    actor_source,
                    contracts.governor_rewards,
                    GovernanceContractKind::GovernorRewards,
                    rebuilt.plan.fingerprint,
                    next.intent,
                    view_session,
                    vault_store,
                    effective_chain,
                    http,
                    operation,
                    PublicActionGasFeeSelection::Auto,
                    None,
                    Some(GovernanceContinuation::Reward {
                        progress: rebuilt,
                        evidence: fresh,
                    }),
                    None,
                    recipe,
                )
                .await
                .map(Some)
            })
            .await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.selected_evm_chain() != Some(context.chain_id)
                    || root.selected_wallet_id.as_deref()
                        != Some(context.private_wallet_uuid.as_str())
                    || root.governance_context_key() != context_key
                    || root.governance.action_flow.generation != generation
                {
                    return;
                }
                root.governance.action_flow.pending = false;
                match result {
                    Ok(Some(draft)) => {
                        if draft.continuation.as_ref().is_some_and(|continuation| {
                            matches!(
                                continuation,
                                GovernanceContinuation::Reward { progress, .. }
                                    if !progress.confirmed().is_empty()
                            )
                        }) {
                            root.replace_public_action_dialog_for_confirmed_history(true);
                        }
                        root.authorize_prepared_governance_draft(draft, window, cx);
                    }
                    Ok(None) => {
                        root.governance.action_flow.error = None;
                        root.refresh_governance_after_target(&target, cx);
                    }
                    Err(error) => {
                        root.governance.action_flow.error = Some(Arc::from(error));
                        root.refresh_governance_after_target(&target, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Shows the memoized participation for `proposal` without reading.
    pub(in crate::root) fn show_proposal_participation(
        &mut self,
        proposal: &wallet_ops::GovernanceProposal,
    ) {
        if self.governance.tab != GovernanceTab::Proposals {
            return;
        }
        self.clean_governance_participants();
        let key = crate::root::governance_action::proposal_participation_key(
            proposal,
            self.governance_context_key(),
        );
        let proposal_action_key_changed =
            self.governance
                .action_flow
                .recipe
                .as_ref()
                .is_some_and(|recipe| match recipe {
                    GovernanceDraftRecipe::Proposal {
                        key: action_key, ..
                    } => action_key != &key,
                    GovernanceDraftRecipe::Staking { .. } => true,
                })
                || self
                    .governance
                    .action_flow
                    .draft
                    .as_ref()
                    .is_some_and(|draft| {
                        !matches!(
                            &draft.target,
                            GovernanceRefreshTarget::Proposal(action_key)
                                if action_key == &key
                        )
                    });
        if proposal_action_key_changed {
            self.governance.invalidate_action();
            self.governance.action_flow.selection = None;
            self.governance.action_flow.draft = None;
            self.governance.action_flow.pending = false;
            self.governance.action_flow.error = None;
        }
        if matches!(
            self.governance.action_flow.selection,
            Some(GovernanceActionSelection::Proposal(_))
        ) {
            self.governance.action_flow.selection = None;
        }
        self.governance.proposal_participation.show(&key);
    }

    /// Reads the selected proposal's participation for every participant when `refresh`, or
    /// only for those without a loaded row.
    pub(in crate::root) fn request_proposal_participation(
        &mut self,
        refresh: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(proposal) = self.proposals.selected_proposal().cloned() else {
            return;
        };
        self.show_proposal_participation(&proposal);
        let accounts = if refresh {
            self.governance
                .proposal_participation
                .key
                .as_ref()
                .map_or_else(Vec::new, |key| {
                    key.context
                        .participants
                        .iter()
                        .map(|participant| participant.address)
                        .collect()
                })
        } else {
            self.governance.proposal_participation.unloaded(&proposal)
        };
        self.start_proposal_participation(&proposal, accounts, cx);
        cx.notify();
    }

    fn start_proposal_participation(
        &mut self,
        proposal: &wallet_ops::GovernanceProposal,
        accounts: Vec<Address>,
        cx: &Context<'_, Self>,
    ) {
        if self.governance.tab != GovernanceTab::Proposals {
            return;
        }
        let key = crate::root::governance_action::proposal_participation_key(
            proposal,
            self.governance_context_key(),
        );
        if self.governance.proposal_participation.key.as_ref() != Some(&key) || accounts.is_empty()
        {
            return;
        }
        let generation = self
            .governance
            .proposal_participation
            .begin(&key, &accounts);
        let Ok(effective_chain) = self
            .effective_chain_configs
            .enabled(key.context.chain_id)
            .cloned()
        else {
            return;
        };
        let http = self.http.clone();
        let proposal = proposal.clone();
        let chain_id = key.context.chain_id;
        // `begin` superseded the previous reader, which stops after any read in flight. Waiting
        // for it, then for jitter if it was still running, keeps participant reads one at a time.
        let previous = self
            .governance
            .proposal_participation_reader
            .take()
            .map(|(task, finished)| (task, !finished.get()));
        let finished = Rc::new(Cell::new(false));
        let reader_finished = Rc::clone(&finished);
        let reader = cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            if let Some((previous, running)) = previous {
                previous.await;
                if running {
                    executor.timer(isolated_read_jitter()).await;
                }
            }
            let current_cx = cx.clone();
            read_participants_isolated(
                accounts,
                async || executor.timer(isolated_read_jitter()).await,
                || {
                    this.read_with(&current_cx, |root, _| {
                        root.governance
                            .proposal_participation
                            .current(&key, generation)
                    })
                    .unwrap_or(false)
                },
                async || open_isolated_governance_route(chain_id, &effective_chain, &http).await,
                async |account, route| {
                    let row = match route {
                        Ok(route) => wallet_ops::fetch_governance_participation(
                            chain_id, &proposal, account, &route, &http,
                        )
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|row| row.state.map_err(|error| error.to_string())),
                        Err(error) => Err(error),
                    };
                    let _ = this.update(&mut *cx, |root, cx| {
                        root.governance.proposal_participation.apply(
                            &key,
                            generation,
                            account,
                            row.map_err(Arc::from),
                        );
                        cx.notify();
                    });
                },
            )
            .await;
            reader_finished.set(true);
        });
        self.governance.proposal_participation_reader = Some((reader, finished));
    }

    pub(in crate::root) fn select_governance_tab(
        &mut self,
        tab: GovernanceTab,
        cx: &mut Context<'_, Self>,
    ) {
        if self.governance.tab == tab {
            return;
        }
        self.governance.tab = tab;
        self.invalidate_governance_context();
        match tab {
            GovernanceTab::Proposals if !self.proposals.checked && !self.proposals.loading => {
                self.start_proposals_refresh(false, cx);
            }
            GovernanceTab::Proposals => {
                let selected = self.proposals.selected_proposal().cloned();
                if let Some(proposal) = selected.as_ref() {
                    self.show_proposal_participation(proposal);
                }
            }
            GovernanceTab::Staking => self.start_staking_refresh(cx),
        }
        cx.notify();
    }

    pub(in crate::root) fn participant_picker_event(
        &mut self,
        event: &ComboboxEvent<SearchableVec<ParticipantChoice>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(event, ComboboxEvent::Confirm(_)) {
            self.governance
                .participant_picker
                .update(cx, |picker, cx| picker.set_query("", window, cx));
            return;
        }
        let saved = self
            .selected_wallet_id
            .as_deref()
            .and_then(|wallet| self.ui_state.governance_participants.get(wallet))
            .map_or(&[][..], Vec::as_slice);
        let Some(values) = participant_picker_change(
            event,
            &self.public_accounts,
            self.selected_wallet_id.as_deref(),
            self.governance.participant_picker_wallet.as_deref(),
            saved,
        ) else {
            return;
        };
        let Some(wallet_id) = self.selected_wallet_id.clone() else {
            return;
        };
        self.ui_state
            .governance_participants
            .insert(wallet_id.to_string(), values);
        self.save_ui_state();
        self.invalidate_governance_context();
        if self.governance.tab == GovernanceTab::Staking {
            self.start_staking_refresh(cx);
        } else {
            let selected = self.proposals.selected_proposal().cloned();
            if let Some(proposal) = selected.as_ref() {
                self.show_proposal_participation(proposal);
            }
        }
        cx.notify();
    }
}
