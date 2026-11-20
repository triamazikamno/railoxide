#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn proposal_participation_key_matches(
        &self,
        proposal: &GovernanceProposal,
        key: &ProposalParticipationKey,
    ) -> bool {
        proposal_participation_key_matches(
            proposal,
            self.proposals.selected_proposal(),
            &self.governance_context_key(),
            self.governance.proposal_participation.key.as_ref(),
            key,
        )
    }

    fn staking_action_admission_matches(
        &self,
        target: &GovernanceRefreshTarget,
        recipe: &GovernanceDraftRecipe,
    ) -> bool {
        let GovernanceDraftRecipe::Staking { selection, .. } = recipe else {
            return false;
        };
        let GovernanceRefreshTarget::Staking(context_key) = target else {
            return false;
        };
        self.governance_context_key() == *context_key
            && self.governance.staking.key.as_ref() == Some(context_key)
            && staking_action_identity_matches(
                self.governance.action_flow.staking_selection(),
                self.governance.action_flow.recipe.as_ref(),
                target,
                recipe,
            )
            && self.governance.staking.action_selection_ready(selection)
    }

    fn governance_authorization_identity_matches(&self, draft: &GovernanceSpendDraft) -> bool {
        let current_draft = match &draft.target {
            GovernanceRefreshTarget::Proposal(_) | GovernanceRefreshTarget::Staking(_) => {
                self.governance.action_flow.draft.as_ref()
            }
        };
        if !current_draft.is_some_and(|current| {
            current.estimate_completed_at == draft.estimate_completed_at
                && current.resolved.raw == draft.resolved.raw
                && current.resolved.fingerprint == draft.resolved.fingerprint
                && current.actor_uuid == draft.actor_uuid
                && current.actor == draft.actor
                && current.actor_source == draft.actor_source
                && current.context == draft.context
                && current.target == draft.target
                && current.recipe == draft.recipe
        }) {
            return false;
        }
        let expected_context = match &draft.target {
            GovernanceRefreshTarget::Proposal(key) => key.context.clone(),
            GovernanceRefreshTarget::Staking(key) => key.clone(),
        };
        if self.selected_wallet_id.as_deref() != Some(draft.context.private_wallet_uuid.as_str())
            || self.selected_evm_chain() != Some(draft.context.chain_id)
            || self.governance_context_key() != expected_context
            || !self.governance_participants().iter().any(|account| {
                account.public_account_uuid.as_str() == draft.actor_uuid.as_ref()
                    && account.address == draft.actor
                    && account.status == wallet_ops::vault::PublicAccountStatus::Active
            })
        {
            return false;
        }
        match (&draft.target, &draft.recipe) {
            (
                GovernanceRefreshTarget::Proposal(target_key),
                GovernanceDraftRecipe::Proposal { proposal, key, .. },
            ) => target_key == key && self.proposal_participation_key_matches(proposal, key),
            (
                GovernanceRefreshTarget::Staking(context_key),
                GovernanceDraftRecipe::Staking { .. },
            ) => {
                self.governance.staking.key.as_ref() == Some(context_key)
                    && staking_action_identity_matches(
                        self.governance.action_flow.staking_selection(),
                        self.governance.action_flow.recipe.as_ref(),
                        &draft.target,
                        &draft.recipe,
                    )
            }
            _ => false,
        }
    }

    fn governance_authorization_admission_matches(&self, draft: &GovernanceSpendDraft) -> bool {
        if !self.governance_authorization_identity_matches(draft) {
            return false;
        }
        match (&draft.target, &draft.recipe) {
            (GovernanceRefreshTarget::Staking(_), GovernanceDraftRecipe::Staking { .. }) => {
                self.staking_action_admission_matches(&draft.target, &draft.recipe)
            }
            _ => true,
        }
    }

    pub(in crate::root) fn authorize_prepared_governance_draft(
        &mut self,
        draft: GovernanceSpendDraft,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let target = draft.target.clone();
        self.governance.action_flow.draft = Some(draft.clone());
        if !self.governance_authorization_admission_matches(&draft) {
            self.reject_governance_authorization(
                &target,
                "Governance context changed; review the action again",
            );
            return;
        }
        window.close_dialog(cx);
        let summary = governance_authorization_summary(self, &draft);
        let actor_source = draft.actor_source;
        let intent = SpendAuthorizationIntent::Governance(Box::new(draft));
        if actor_source == PublicAccountSource::HardwareDerived {
            Self::open_hardware_public_action_authorization_dialog(intent, summary, window, cx);
        } else {
            self.request_spend_authorization(intent, summary, window, cx);
        }
    }

    pub(in crate::root) fn cancel_governance_authorization(
        &mut self,
        intent: &SpendAuthorizationIntent,
        cx: &mut Context<'_, Self>,
    ) {
        let SpendAuthorizationIntent::Governance(draft) = intent else {
            return;
        };
        match draft.target {
            GovernanceRefreshTarget::Proposal(_) => self.close_proposal_action(cx),
            GovernanceRefreshTarget::Staking(_) => self.close_staking_action(cx),
        }
    }

    fn reject_governance_authorization(&mut self, target: &GovernanceRefreshTarget, error: &str) {
        match target {
            GovernanceRefreshTarget::Proposal(_) | GovernanceRefreshTarget::Staking(_) => {
                self.governance.action_flow.draft = None;
                self.governance.action_flow.error = Some(Arc::from(error.to_owned()));
            }
        }
    }

    pub(in crate::root) fn revalidate_governance_authorized(
        &mut self,
        draft: &GovernanceSpendDraft,
        spend_authorization: wallet_ops::DesktopPrivateSpendAuthorization,
        #[cfg(feature = "hardware")] window: &mut Window,
        #[cfg(not(feature = "hardware"))] window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.governance_authorization_identity_matches(draft) {
            self.reject_governance_authorization(
                &draft.target,
                "Governance context changed; review the action again",
            );
            cx.notify();
            return;
        }
        let age = Instant::now().saturating_duration_since(draft.estimate_completed_at);
        if governance_authorization_estimate_is_fresh(age) {
            self.submit_governance_authorized(draft.clone(), spend_authorization, window, cx);
            return;
        }
        self.refresh_expired_governance_authorization(draft, window, cx);
    }

    fn refresh_expired_governance_authorization(
        &mut self,
        draft: &GovernanceSpendDraft,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        let chain_id = draft.context.chain_id;
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let target = draft.target.clone();
        let operation_generation = {
            self.governance.action_flow.generation =
                self.governance.action_flow.generation.wrapping_add(1);
            self.governance.action_flow.generation
        };
        match &target {
            GovernanceRefreshTarget::Proposal(_) | GovernanceRefreshTarget::Staking(_) => {
                self.governance.action_flow.pending = true;
            }
        }
        let actor = draft.actor;
        let raw = draft.resolved.raw.clone();
        let gas_fee = draft.gas_fee;
        let http = self.http.clone();
        let operation = draft.operation.clone();
        let expected_draft = draft.clone();
        cx.spawn_in(window, async move |this, cx| {
            let estimate = match operation.client().await {
                Ok(client) => {
                    wallet_ops::estimate_public_advanced_transaction(
                        PublicAdvancedTransactionEstimateRequest {
                            chain_id,
                            effective_chain,
                            from: actor,
                            intent: raw,
                            gas_fee,
                            access_list: None,
                        },
                        &http,
                        &client,
                    )
                    .await
                }
                Err(error) => Err(error),
            }
            .map_err(|error| format_report_chain(&error));
            let _ = this.update_in(cx, |root, window, cx| {
                let generation_current =
                    root.governance.action_flow.generation == operation_generation;
                let disposition = governance_expired_authorization_disposition(
                    generation_current,
                    generation_current
                        && root.governance_authorization_identity_matches(&expected_draft),
                );
                if disposition == GovernanceExpiredAuthorizationDisposition::StaleGeneration {
                    return;
                }
                root.governance.action_flow.pending = false;
                if disposition == GovernanceExpiredAuthorizationDisposition::CurrentIdentityMismatch
                {
                    root.reject_governance_authorization(
                        &target,
                        "Governance context changed; review the action again",
                    );
                    cx.notify();
                    return;
                }
                let Ok(estimate) = estimate else {
                    root.reject_governance_authorization(
                        &target,
                        "Exact Governance call could not be re-estimated; review the action again",
                    );
                    cx.notify();
                    return;
                };
                let mut refreshed = expected_draft.clone();
                refreshed.estimate = estimate.clone();
                refreshed.estimate_completed_at = Instant::now();
                let Ok(review) = GovernanceActionReview::from_resolved(
                    &refreshed.resolved,
                    refreshed.context.clone(),
                    Some(estimate.fee_projection(None, 18)),
                ) else {
                    root.reject_governance_authorization(
                        &target,
                        "Exact Governance call review became invalid; review the action again",
                    );
                    cx.notify();
                    return;
                };
                refreshed.review = review;
                let summary = governance_authorization_summary(root, &refreshed);
                match &target {
                    GovernanceRefreshTarget::Proposal(_) | GovernanceRefreshTarget::Staking(_) => {
                        root.governance.action_flow.draft = Some(refreshed.clone());
                    }
                }
                let intent = SpendAuthorizationIntent::Governance(Box::new(refreshed.clone()));
                if refreshed.actor_source == PublicAccountSource::HardwareDerived {
                    Self::open_hardware_public_action_authorization_dialog(
                        intent, summary, window, cx,
                    );
                } else {
                    root.request_spend_authorization(intent, summary, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}
