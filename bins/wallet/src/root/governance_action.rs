use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256, keccak256};
use gpui::{Context, Window};
use gpui_component::WindowExt;
use railgun_ui::governance_contracts;
use tokio::sync::mpsc;
use wallet_ops::vault::{PublicAccountMetadata, PublicAccountSource};
use wallet_ops::{
    GovernanceActionContext, GovernanceActionIntent, GovernanceActionReview,
    GovernanceCapacityError, GovernanceContractKind, GovernanceContractRules,
    GovernanceContractVersion, GovernanceGuardError, GovernanceParticipation, GovernanceProposal,
    GovernanceProposalStage, GovernanceResolvedAction, GovernanceSubmissionRequest,
    GovernanceWorkflow, GovernanceWorkflowRequest, PublicActionGasFeeSelection,
    PublicActionProgressStep, PublicAdvancedTransactionAuthorization,
    PublicAdvancedTransactionEstimate, PublicAdvancedTransactionEstimateRequest, PublicSendRequest,
    derive_governance_proposal_status, guard_call_vote, guard_nay_vote, guard_sponsor,
    guard_unsponsor, guard_yay_vote,
};
use zeroize::Zeroizing;

use super::governance::GovernanceContextKey;
use super::public_action::{
    PublicActionStepInterval, PublicActionStepState, PublicActionStepStatus,
};
use super::spend_authorization::{
    SpendAuthorizationIntent, SpendAuthorizationSummary, SpendAuthorizationSummaryRow,
};
use super::tokens::{
    format_native_token_amount_for_display, format_token_amount_for_display,
    format_value_with_usd_label, token_display_metadata,
};
use super::{WalletRoot, format_report_chain};

pub(super) const GOVERNANCE_SPEND_AUTHORIZATION_TTL: Duration = Duration::from_mins(2);

/// A reviewable Governance call.  This intentionally contains only public metadata and the
/// encrypted-vault handles needed by the existing Public-account signer.
#[derive(Clone)]
pub(super) struct GovernanceSpendDraft {
    pub target: GovernanceRefreshTarget,
    pub actor_uuid: Arc<str>,
    pub actor: Address,
    pub actor_source: PublicAccountSource,
    pub context: GovernanceActionContext,
    pub resolved: GovernanceResolvedAction,
    pub review: GovernanceActionReview,
    pub proposal_review: Option<GovernanceProposalReviewProjection>,
    pub staking_review: Option<GovernanceStakingReviewProjection>,
    pub estimate: PublicAdvancedTransactionEstimate,
    pub estimate_completed_at: Instant,
    pub gas_fee: PublicActionGasFeeSelection,
    pub view_session: Arc<wallet_ops::vault::DesktopViewSession>,
    pub vault_store: Arc<wallet_ops::vault::DesktopVaultStore>,
    pub workflow: Option<GovernanceWorkflow>,
    pub continuation: Option<GovernanceContinuation>,
    pub recipe: GovernanceDraftRecipe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GovernanceProposalReviewProjection {
    pub stage: GovernanceProposalStage,
    pub power_remaining_after: Option<U256>,
    pub snapshot_power: Option<U256>,
    pub proposal_sponsorship_after: Option<U256>,
    pub sponsorship_threshold: Option<U256>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GovernanceStakingReviewProjection {
    Stake {
        amount: U256,
    },
    Delegation(wallet_ops::DelegationEvidence),
    Unlock {
        owner: Address,
        stake_id: U256,
        amount: U256,
        previous_delegate: Address,
        stake_locktime: U256,
        projected_claim_timestamp: U256,
    },
    PrincipalClaim(wallet_ops::PrincipalClaimPlan),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum GovernanceDraftRecipe {
    Proposal {
        proposal: Box<GovernanceProposal>,
        key: ProposalParticipationKey,
        selection: ProposalActionSelection,
        amount: Option<U256>,
    },
    Staking {
        selection: super::governance::StakingActionSelection,
        context_key: GovernanceContextKey,
        amount_input: String,
        delegate_input: String,
        token_decimals: Option<u8>,
    },
}

#[derive(Clone, Debug)]
pub(super) enum GovernanceContinuation {
    Reward {
        progress: wallet_ops::RewardClaimProgress,
        evidence: wallet_ops::RewardBatchEvidence,
    },
}

pub(super) fn governance_reward_progress_steps(
    progress: &wallet_ops::RewardClaimProgress,
    intent: &GovernanceActionIntent,
) -> Option<Vec<PublicActionStepState>> {
    let GovernanceActionIntent::RewardClaim {
        starting_interval,
        ending_interval,
        ..
    } = intent
    else {
        return None;
    };
    let mut steps = progress
        .confirmed()
        .iter()
        .enumerate()
        .map(|(index, confirmed)| PublicActionStepState {
            step: PublicActionProgressStep::RewardClaim(index as u32),
            status: PublicActionStepStatus::Done,
            tx_hash: Some(Arc::from(confirmed.transaction_hash.to_string())),
            message: None,
            interval: Some(PublicActionStepInterval {
                start: confirmed.step.starting_interval,
                end: confirmed.step.ending_interval,
            }),
        })
        .collect::<Vec<_>>();
    steps.push(PublicActionStepState {
        step: PublicActionProgressStep::RewardClaim(progress.confirmed().len() as u32),
        status: PublicActionStepStatus::Pending,
        tx_hash: None,
        message: None,
        interval: Some(PublicActionStepInterval {
            start: *starting_interval,
            end: *ending_interval,
        }),
    });
    Some(steps)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum GovernanceRefreshTarget {
    Proposal(ProposalParticipationKey),
    Staking(GovernanceContextKey),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProposalActionKind {
    Sponsor,
    Unsponsor,
    CallVote,
    Yay,
    Nay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ProposalActionSelection {
    pub actor: Address,
    pub kind: ProposalActionKind,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ProposalParticipationKey {
    pub version: GovernanceContractVersion,
    pub contract: Address,
    pub index: U256,
    pub context: GovernanceContextKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ProposalParticipationRow {
    Loading,
    Ready(Box<GovernanceParticipation>),
    Unavailable(Arc<str>),
}

#[derive(Clone, Debug, Default)]
pub(super) struct ProposalParticipationState {
    pub key: Option<ProposalParticipationKey>,
    pub generation: u64,
    pub loading: bool,
    pub rows: BTreeMap<Address, ProposalParticipationRow>,
}

impl ProposalParticipationState {
    pub(super) fn begin(&mut self, key: &ProposalParticipationKey) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.key = Some(key.clone());
        self.loading = true;
        self.rows = key
            .context
            .participants
            .iter()
            .map(|participant| (participant.address, ProposalParticipationRow::Loading))
            .collect();
        self.generation
    }

    pub(super) fn apply(
        &mut self,
        key: &ProposalParticipationKey,
        generation: u64,
        rows: Vec<wallet_ops::GovernanceParticipationRow>,
    ) -> bool {
        if self.generation != generation || self.key.as_ref() != Some(key) {
            return false;
        }
        for row in rows {
            let state = match row.state {
                Ok(value) => ProposalParticipationRow::Ready(Box::new(value)),
                Err(error) => ProposalParticipationRow::Unavailable(Arc::from(error.to_string())),
            };
            self.rows.insert(row.account, state);
        }
        self.loading = false;
        true
    }

    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.key = None;
        self.loading = false;
        self.rows.clear();
    }
}

pub(super) fn staking_action_identity_matches(
    current_selection: Option<&super::governance::StakingActionSelection>,
    current_recipe: Option<&GovernanceDraftRecipe>,
    target: &GovernanceRefreshTarget,
    recipe: &GovernanceDraftRecipe,
) -> bool {
    let (
        GovernanceRefreshTarget::Staking(target_key),
        GovernanceDraftRecipe::Staking {
            selection,
            context_key,
            amount_input,
            delegate_input,
            token_decimals,
        },
    ) = (target, recipe)
    else {
        return false;
    };
    target_key == context_key
        && current_selection == Some(selection)
        && current_recipe.is_some_and(|current_recipe| {
            matches!(
                current_recipe,
                GovernanceDraftRecipe::Staking {
                    selection: current_selection,
                    context_key: current_context_key,
                    amount_input: current_amount_input,
                    delegate_input: current_delegate_input,
                    token_decimals: current_token_decimals,
                } if current_selection == selection
                    && current_context_key == context_key
                    && current_amount_input == amount_input
                    && current_delegate_input == delegate_input
                    && current_token_decimals == token_decimals
            )
        })
}

impl WalletRoot {
    pub(super) fn proposal_participation_key_matches(
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
            || self.selected_chain != draft.context.chain_id
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

    pub(super) fn authorize_prepared_governance_draft(
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

    pub(super) fn cancel_governance_authorization(
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

    pub(super) fn revalidate_governance_authorized(
        &mut self,
        draft: &GovernanceSpendDraft,
        vault_password: Zeroizing<String>,
        protected_software_seed_session: Option<
            Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        >,
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
            self.submit_governance_authorized(
                draft.clone(),
                vault_password,
                protected_software_seed_session,
                window,
                cx,
            );
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
        let chain_id = draft.context.chain_id;
        let actor = draft.actor;
        let raw = draft.resolved.raw.clone();
        let gas_fee = draft.gas_fee;
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let http = self.http.clone();
        let expected_draft = draft.clone();
        cx.spawn_in(window, async move |this, cx| {
            let estimate = wallet_ops::estimate_public_advanced_transaction(
                PublicAdvancedTransactionEstimateRequest {
                    chain_id,
                    effective_chain,
                    from: actor,
                    intent: raw,
                    gas_fee,
                    access_list: None,
                },
                &http,
            )
            .await
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
                    Some(estimate.fee_projection(None)),
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

    pub(super) fn review_proposal_action(
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
        let chain_id = self.selected_chain;
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let http = self.http.clone();
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

    pub(super) fn submit_governance_authorized(
        &mut self,
        draft: GovernanceSpendDraft,
        vault_password: Zeroizing<String>,
        protected_software_seed_session: Option<
            Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        >,
        #[cfg(feature = "hardware")] window: &mut Window,
        #[cfg(not(feature = "hardware"))] window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.sending {
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
        } = draft;
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
        let active_wallet_id = self.selected_wallet_id.clone();
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
                super::public_action::PublicActionMode::Send,
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
                super::public_action::PublicActionMode::Send,
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
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        Self::spawn_public_action_progress_listener(
            generation,
            chain_id,
            active_wallet_id.clone(),
            progress_rx,
            cx,
        );
        Self::spawn_public_action_session_event_listener(
            generation,
            chain_id,
            active_wallet_id.clone(),
            event_rx,
            cx,
        );
        Self::show_public_action_progress_dialog_after_close(window, cx);
        let public_send = PublicSendRequest {
            chain_id,
            effective_chain: self.effective_chain_configs.get(&chain_id).cloned(),
            view_session,
            vault_store,
            vault_password,
            protected_software_seed_session,
            trezor_app_passphrase,
            trezor_pin_matrix_provider,
            public_account_uuid: actor_uuid.to_string(),
            intent: resolved.raw.clone(),
            advanced_authorization: Some(PublicAdvancedTransactionAuthorization {
                payload_fingerprint: estimate.payload_fingerprint,
                gas_limit: estimate.gas_limit,
            }),
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
        let join = self.runtime.spawn(async move {
            if let Some(workflow) = workflow {
                wallet_ops::submit_governance_workflow_with_progress(
                    GovernanceWorkflowRequest {
                        initial: request,
                        workflow,
                    },
                    &http,
                    move |update| {
                        let _ = progress_tx.send(update);
                    },
                )
                .await
                .map(|result| result.transactions)
            } else {
                wallet_ops::submit_governance_action_with_progress(request, &http, move |update| {
                    let _ = progress_tx.send(update);
                })
                .await
                .map(|result| vec![result])
            }
        });
        self.public_form.action_task_abort_handle = Some(join.abort_handle());
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.selected_wallet_id != active_wallet_id || root.selected_chain != chain_id {
                    return;
                }
                if !super::public_action::public_action_accepts_update(
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

    fn governance_progress_step(
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

    fn refresh_governance_proposal_participation_for_key(
        &mut self,
        key: &ProposalParticipationKey,
        cx: &mut Context<'_, Self>,
    ) {
        let selected = self
            .proposals
            .selected_proposal()
            .filter(|proposal| {
                proposal.contract_version == key.version
                    && proposal.contract_address == key.contract
                    && proposal.index == key.index
            })
            .cloned();
        if let Some(proposal) = selected.as_ref() {
            self.start_proposal_participation(proposal, cx);
        }
        self.refresh_selected_proposal_page(cx);
    }

    pub(super) fn refresh_governance_after_target(
        &mut self,
        target: &GovernanceRefreshTarget,
        cx: &mut Context<'_, Self>,
    ) {
        match target {
            GovernanceRefreshTarget::Proposal(key) => {
                self.refresh_governance_proposal_participation_for_key(key, cx);
            }
            GovernanceRefreshTarget::Staking(key) => {
                if self.governance.tab == super::governance::GovernanceTab::Staking
                    && self.governance_context_key() == *key
                {
                    self.start_staking_refresh(cx);
                }
                self.refresh_selected_proposal_page(cx);
            }
        }
    }
}

pub(super) const fn proposal_participation_key(
    proposal: &GovernanceProposal,
    context: GovernanceContextKey,
) -> ProposalParticipationKey {
    ProposalParticipationKey {
        version: proposal.contract_version,
        contract: proposal.contract_address,
        index: proposal.index,
        context,
    }
}

fn proposal_participation_key_matches(
    proposal: &GovernanceProposal,
    selected_proposal: Option<&GovernanceProposal>,
    context: &GovernanceContextKey,
    current_key: Option<&ProposalParticipationKey>,
    expected_key: &ProposalParticipationKey,
) -> bool {
    proposal_participation_key(proposal, context.clone()) == *expected_key
        && selected_proposal.is_some_and(|selected| {
            proposal_participation_key(selected, context.clone()) == *expected_key
        })
        && current_key == Some(expected_key)
}

async fn build_governance_spend_draft(
    proposal: GovernanceProposal,
    key: ProposalParticipationKey,
    selection: ProposalActionSelection,
    amount: Option<U256>,
    wallet_id: Arc<str>,
    actor_uuid: Arc<str>,
    actor_source: PublicAccountSource,
    view_session: Arc<wallet_ops::vault::DesktopViewSession>,
    vault_store: Arc<wallet_ops::vault::DesktopVaultStore>,
    effective_chain: Option<wallet_ops::settings::EffectiveChainConfig>,
    http: wallet_ops::HttpContext,
    gas_fee_selection: PublicActionGasFeeSelection,
) -> Result<GovernanceSpendDraft, String> {
    let recipe_proposal = proposal.clone();
    let recipe_key = key.clone();
    let recipe_amount = amount;
    let overview = wallet_ops::fetch_governance_overview(
        key.context.chain_id,
        effective_chain.as_ref(),
        &http,
    )
    .await
    .map_err(|error| format_report_chain(&error))?
    .ok_or_else(|| "Governance is not deployed on this chain".to_owned())?;
    let v2_count = usize::try_from(overview.v2.proposal_count)
        .map_err(|_| "Governance proposal count is too large".to_owned())?;
    let (global_index, expected_address) = match proposal.contract_version {
        GovernanceContractVersion::V2 => {
            let index = usize::try_from(proposal.index)
                .map_err(|_| "Proposal index is too large".to_owned())?;
            let global = v2_count
                .checked_sub(index + 1)
                .ok_or_else(|| "Proposal is no longer available".to_owned())?;
            (global, overview.v2.address)
        }
        GovernanceContractVersion::V1 => {
            let summary = overview
                .v1
                .as_ref()
                .ok_or_else(|| "Legacy governance is unavailable".to_owned())?;
            let v1_count = usize::try_from(summary.proposal_count)
                .map_err(|_| "Governance proposal count is too large".to_owned())?;
            let index = usize::try_from(proposal.index)
                .map_err(|_| "Proposal index is too large".to_owned())?;
            let offset = v1_count
                .checked_sub(index + 1)
                .ok_or_else(|| "Proposal is no longer available".to_owned())?;
            (
                v2_count
                    .checked_add(offset)
                    .ok_or_else(|| "Proposal position overflowed".to_owned())?,
                summary.address,
            )
        }
    };
    if expected_address != proposal.contract_address {
        return Err("Proposal contract changed; refresh before authorizing".to_owned());
    }
    let page = global_index / super::proposals::PROPOSALS_PAGE_SIZE;
    let page_rows = wallet_ops::fetch_governance_page(
        &overview,
        page,
        NonZeroUsize::new(super::proposals::PROPOSALS_PAGE_SIZE).expect("nonzero page size"),
        effective_chain.as_ref(),
        &http,
    )
    .await
    .map_err(|error| format_report_chain(&error))?;
    let fresh = page_rows
        .into_iter()
        .find(|candidate| {
            candidate.contract_version == proposal.contract_version
                && candidate.contract_address == proposal.contract_address
                && candidate.index == proposal.index
        })
        .ok_or_else(|| {
            "Proposal changed or is no longer available; refresh before authorizing".to_owned()
        })?;
    let rules = match fresh.contract_version {
        GovernanceContractVersion::V2 => overview.v2.rules.clone(),
        GovernanceContractVersion::V1 => overview
            .v1
            .as_ref()
            .ok_or_else(|| "Legacy governance rules are unavailable".to_owned())?
            .rules
            .clone(),
    };
    let chain_time = wallet_ops::fetch_governance_chain_time(
        key.context.chain_id,
        effective_chain.as_ref(),
        &http,
    )
    .await
    .map_err(|error| format_report_chain(&error))?;
    let rows = wallet_ops::fetch_governance_participation(
        key.context.chain_id,
        &fresh,
        &[selection.actor],
        effective_chain.as_ref(),
        &http,
    )
    .await
    .map_err(|error| format_report_chain(&error))?;
    let participation = rows
        .into_iter()
        .find(|row| row.account == selection.actor)
        .ok_or_else(|| {
            "Selected account participation is unavailable; refresh before authorizing".to_owned()
        })?
        .state
        .map_err(|error| error.to_string())?;
    let amount = amount.unwrap_or_default();
    validate_proposal_action(
        &fresh,
        &rules,
        chain_time,
        &participation,
        selection.kind,
        Some(amount),
    )?;
    let observed_state = proposal_observed_state(&fresh, &participation, selection.actor);
    let context = GovernanceActionContext {
        private_wallet_uuid: wallet_id.to_string(),
        chain_id: key.context.chain_id,
        public_account_uuid: actor_uuid.to_string(),
        actor: selection.actor,
        contract: fresh.contract_address,
        contract_kind: GovernanceContractKind::Voting,
        observed_state,
    };
    let action = match selection.kind {
        ProposalActionKind::Sponsor => GovernanceActionIntent::Sponsor {
            proposal_version: fresh.contract_version,
            proposal_index: fresh.index,
            amount,
            snapshot_interval: participation.sponsorship_snapshot.interval,
            snapshot_hint: participation.sponsorship_snapshot.hint,
        },
        ProposalActionKind::Unsponsor => GovernanceActionIntent::Unsponsor {
            proposal_version: fresh.contract_version,
            proposal_index: fresh.index,
            amount,
        },
        ProposalActionKind::CallVote => GovernanceActionIntent::CallVote {
            proposal_version: fresh.contract_version,
            proposal_index: fresh.index,
        },
        ProposalActionKind::Yay => GovernanceActionIntent::Yay {
            proposal_version: fresh.contract_version,
            proposal_index: fresh.index,
            amount,
            snapshot_interval: participation.voting_snapshot.interval,
            snapshot_hint: participation.voting_snapshot.hint,
        },
        ProposalActionKind::Nay => GovernanceActionIntent::Nay {
            proposal_version: fresh.contract_version,
            proposal_index: fresh.index,
            amount,
            snapshot_interval: participation.voting_snapshot.interval,
            snapshot_hint: participation.voting_snapshot.hint,
        },
    };
    let resolved = action
        .resolve(&context)
        .map_err(|error| error.to_string())?;
    let estimate = wallet_ops::estimate_public_advanced_transaction(
        PublicAdvancedTransactionEstimateRequest {
            chain_id: context.chain_id,
            effective_chain,
            from: context.actor,
            intent: resolved.raw.clone(),
            gas_fee: gas_fee_selection,
            access_list: None,
        },
        &http,
    )
    .await
    .map_err(|error| format_report_chain(&error))?;
    let estimate_completed_at = Instant::now();
    let review = GovernanceActionReview::from_resolved(
        &resolved,
        context,
        Some(estimate.fee_projection(None)),
    )
    .map_err(|error| error.to_string())?;
    let stage = derive_governance_proposal_status(&fresh, &rules, chain_time)
        .map_err(|error| error.to_string())?
        .stage;
    let (power_remaining_after, snapshot_power) = match selection.kind {
        ProposalActionKind::Sponsor | ProposalActionKind::Unsponsor => {
            let capacity = participation
                .sponsorship_capacity()
                .map_err(|error| error.to_string())?;
            let remaining = capacity
                .remaining
                .ok_or_else(|| "Sponsorship capacity has no remaining power".to_owned())?;
            let after = match selection.kind {
                ProposalActionKind::Sponsor => remaining
                    .checked_sub(amount)
                    .ok_or_else(|| "Sponsorship power after action overflowed".to_owned())?,
                ProposalActionKind::Unsponsor => remaining
                    .checked_add(amount)
                    .ok_or_else(|| "Sponsorship power after action overflowed".to_owned())?,
                _ => unreachable!(),
            };
            (Some(after), capacity.snapshot_power)
        }
        ProposalActionKind::Yay | ProposalActionKind::Nay => {
            let capacity = participation
                .voting_capacity()
                .map_err(|error| error.to_string())?;
            let remaining = capacity
                .remaining
                .ok_or_else(|| "Voting capacity has no remaining power".to_owned())?;
            let after = remaining
                .checked_sub(amount)
                .ok_or_else(|| "Voting power after action overflowed".to_owned())?;
            (Some(after), capacity.snapshot_power)
        }
        ProposalActionKind::CallVote => (None, None),
    };
    let (proposal_sponsorship_after, sponsorship_threshold) =
        match selection.kind {
            ProposalActionKind::Sponsor => {
                (
                    Some(fresh.sponsorship.checked_add(amount).ok_or_else(|| {
                        "Proposal sponsorship after action overflowed".to_owned()
                    })?),
                    Some(rules.sponsor_threshold),
                )
            }
            ProposalActionKind::Unsponsor => {
                (
                    Some(fresh.sponsorship.checked_sub(amount).ok_or_else(|| {
                        "Proposal sponsorship after action underflowed".to_owned()
                    })?),
                    Some(rules.sponsor_threshold),
                )
            }
            ProposalActionKind::CallVote | ProposalActionKind::Yay | ProposalActionKind::Nay => {
                (None, None)
            }
        };
    let proposal_review = GovernanceProposalReviewProjection {
        stage,
        power_remaining_after,
        snapshot_power,
        proposal_sponsorship_after,
        sponsorship_threshold,
    };
    let gas_fee = match gas_fee_selection {
        PublicActionGasFeeSelection::Auto => PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: estimate.max_fee_per_gas,
            max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
        },
        custom @ PublicActionGasFeeSelection::Custom { .. } => custom,
    };
    Ok(GovernanceSpendDraft {
        target: GovernanceRefreshTarget::Proposal(proposal_participation_key(&fresh, key.context)),
        actor_uuid,
        actor: selection.actor,
        actor_source,
        context: review.context.clone(),
        resolved,
        review,
        proposal_review: Some(proposal_review),
        staking_review: None,
        estimate,
        estimate_completed_at,
        gas_fee,
        view_session,
        vault_store,
        workflow: None,
        continuation: None,
        recipe: GovernanceDraftRecipe::Proposal {
            proposal: Box::new(recipe_proposal),
            key: recipe_key,
            selection,
            amount: recipe_amount,
        },
    })
}

fn proposal_observed_state(
    proposal: &GovernanceProposal,
    participation: &GovernanceParticipation,
    actor: Address,
) -> B256 {
    let mut bytes = Vec::with_capacity(512);
    bytes.extend_from_slice(proposal.contract_address.as_slice());
    bytes.extend_from_slice(actor.as_slice());
    for value in [
        proposal.index,
        proposal.publish_time,
        proposal.vote_call_time,
        proposal.sponsorship,
        proposal.yay_votes,
        proposal.nay_votes,
        proposal.sponsor_snapshot_interval,
        proposal.voting_snapshot_interval,
        participation.current_voting_power,
        participation.sponsorship_snapshot.interval,
        participation.sponsorship_snapshot.voting_power,
        participation.sponsorship_snapshot.hint,
        participation.voting_snapshot.interval,
        participation.voting_snapshot.voting_power,
        participation.voting_snapshot.hint,
        participation.sponsored,
        participation.voted,
        participation.last_sponsored.last_sponsor_time,
        participation.last_sponsored.proposal_id,
    ] {
        bytes.extend_from_slice(&value.to_be_bytes::<32>());
    }
    bytes.push(u8::from(proposal.executed));
    keccak256(bytes)
}

fn governance_authorization_summary(
    root: &WalletRoot,
    draft: &GovernanceSpendDraft,
) -> SpendAuthorizationSummary {
    let method = match &draft.resolved.intent {
        GovernanceActionIntent::Sponsor { .. } => "sponsorProposal",
        GovernanceActionIntent::Unsponsor { .. } => "unsponsorProposal",
        GovernanceActionIntent::CallVote { .. } => "callVote",
        GovernanceActionIntent::Yay { .. } | GovernanceActionIntent::Nay { .. } => "vote",
        GovernanceActionIntent::GovernanceTokenApproval { .. } => "approve",
        GovernanceActionIntent::Stake { .. } => "stake",
        GovernanceActionIntent::Delegate { .. } => "delegate",
        GovernanceActionIntent::Undelegate { .. } => "undelegate",
        GovernanceActionIntent::Unlock { .. } => "unlock",
        GovernanceActionIntent::PrincipalClaim { .. } => "claim principal",
        GovernanceActionIntent::RewardClaim { reward_tokens, .. } if reward_tokens.len() > 1 => {
            "claim all rewards"
        }
        GovernanceActionIntent::RewardClaim { .. } => "claim rewards",
    };
    let amount_token = match &draft.resolved.intent {
        GovernanceActionIntent::Sponsor { .. }
        | GovernanceActionIntent::Unsponsor { .. }
        | GovernanceActionIntent::Yay { .. }
        | GovernanceActionIntent::Nay { .. }
        | GovernanceActionIntent::GovernanceTokenApproval { .. }
        | GovernanceActionIntent::Stake { .. } => {
            governance_contracts(draft.context.chain_id).map(|contracts| contracts.governance_token)
        }
        GovernanceActionIntent::CallVote { .. }
        | GovernanceActionIntent::Delegate { .. }
        | GovernanceActionIntent::Undelegate { .. }
        | GovernanceActionIntent::RewardClaim { .. } => None,
        GovernanceActionIntent::Unlock { .. } | GovernanceActionIntent::PrincipalClaim { .. } => {
            governance_contracts(draft.context.chain_id).map(|contracts| contracts.governance_token)
        }
    };
    let action_amount = draft.review.amount.or_else(|| {
        draft
            .staking_review
            .as_ref()
            .and_then(|projection| match projection {
                GovernanceStakingReviewProjection::Stake { amount }
                | GovernanceStakingReviewProjection::Unlock { amount, .. } => Some(*amount),
                GovernanceStakingReviewProjection::PrincipalClaim(plan) => Some(plan.amount),
                GovernanceStakingReviewProjection::Delegation(_) => None,
            })
    });
    let amount_row = match (action_amount, amount_token) {
        (Some(amount), Some(token)) => {
            let metadata = token_display_metadata(
                Some(&root.effective_token_registry),
                draft.context.chain_id,
                &token,
            );
            let amount = format_token_amount_for_display(
                draft.context.chain_id,
                token,
                amount,
                Some(&root.effective_token_registry),
            );
            SpendAuthorizationSummaryRow::new("Amount", amount)
                .with_icon(metadata.and_then(|metadata| metadata.icon_path))
        }
        (Some(amount), None) => SpendAuthorizationSummaryRow::new("Amount", amount.to_string()),
        (None, _) => SpendAuthorizationSummaryRow::new("Amount", "none"),
    };
    let chain_id = draft.context.chain_id;
    let expected_gas_cost = draft.estimate.expected_gas_cost;
    let estimated_fee = format_value_with_usd_label(
        format_native_token_amount_for_display(chain_id, expected_gas_cost),
        expected_gas_cost,
        Some(18),
        root.public_broadcaster_anchor_cache
            .cached_native_usd_micro_value(chain_id, expected_gas_cost),
        false,
    );
    let mut rows = vec![
        SpendAuthorizationSummaryRow::new("Actor", draft.actor.to_checksum(None)),
        SpendAuthorizationSummaryRow::new("Chain", chain_id.to_string()),
        SpendAuthorizationSummaryRow::new("Contract", draft.context.contract.to_checksum(None)),
        SpendAuthorizationSummaryRow::new("Method", method),
        SpendAuthorizationSummaryRow::new(
            "Native value",
            format_native_token_amount_for_display(chain_id, draft.review.native_value),
        ),
        SpendAuthorizationSummaryRow::new("Estimated fee", estimated_fee),
    ];
    if let GovernanceActionIntent::RewardClaim {
        reward_tokens,
        expected_amounts,
        ..
    } = &draft.resolved.intent
    {
        for (token, amount) in reward_tokens.iter().zip(expected_amounts) {
            let metadata =
                token_display_metadata(Some(&root.effective_token_registry), chain_id, token);
            let amount = format_token_amount_for_display(
                chain_id,
                *token,
                *amount,
                Some(&root.effective_token_registry),
            );
            rows.push(
                SpendAuthorizationSummaryRow::new("Reward", amount)
                    .with_icon(metadata.and_then(|metadata| metadata.icon_path)),
            );
        }
    } else {
        rows.push(amount_row);
    }
    let mut warnings = Vec::new();
    if let Some(projection) = draft.proposal_review.as_ref() {
        rows.push(SpendAuthorizationSummaryRow::new(
            "Proposal stage",
            super::proposals::proposal_stage_label(projection.stage),
        ));
        if let (Some(remaining), Some(snapshot)) =
            (projection.power_remaining_after, projection.snapshot_power)
        {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Power after",
                format!(
                    "{} of {} RAIL",
                    super::proposals::format_compact_rail_amount(remaining),
                    super::proposals::format_compact_rail_amount(snapshot),
                ),
            ));
        }
        if let (Some(after), Some(threshold)) = (
            projection.proposal_sponsorship_after,
            projection.sponsorship_threshold,
        ) {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Sponsorship after",
                format!(
                    "{} of {} required",
                    super::proposals::format_compact_rail_amount(after),
                    super::proposals::format_compact_rail_amount(threshold),
                ),
            ));
        }
    }
    if let Some(projection) = draft.staking_review.as_ref() {
        match projection {
            GovernanceStakingReviewProjection::Delegation(evidence) => {
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Current delegate",
                    evidence.previous_delegate.to_checksum(None),
                ));
                rows.push(SpendAuthorizationSummaryRow::new(
                    "New delegate",
                    evidence.next_delegate.to_checksum(None),
                ));
                warnings.push(Arc::from(
                    "Delegation changes this stake's voting power and future reward recipient.",
                ));
            }
            GovernanceStakingReviewProjection::Unlock {
                stake_id,
                amount: _,
                previous_delegate,
                stake_locktime,
                projected_claim_timestamp,
                ..
            } => {
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Stake",
                    stake_id.to_string(),
                ));
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Unlock period",
                    super::governance::unlock_period_label(*stake_locktime),
                ));
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Claimable",
                    super::proposals::format_date_short(projected_claim_timestamp),
                ));
                if *previous_delegate == draft.actor {
                    warnings.push(Arc::from(
                        "Unlocking stops this stake's voting power and reward accrual.",
                    ));
                } else {
                    warnings.push(Arc::from(
                        "This delegated stake must be undelegated before it can be unlocked.",
                    ));
                }
            }
            GovernanceStakingReviewProjection::PrincipalClaim(plan) => {
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Stake",
                    plan.stake_id.to_string(),
                ));
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Recipient",
                    draft.actor.to_checksum(None),
                ));
            }
            GovernanceStakingReviewProjection::Stake { .. } => {}
        }
    }
    if let Some(workflow) = &draft.workflow {
        let plan = match workflow {
            GovernanceWorkflow::StakeApproval(_) => "Approve → Stake",
            GovernanceWorkflow::UndelegateThenUnlock(_) => "Undelegate → Unlock",
        };
        rows.push(SpendAuthorizationSummaryRow::new("Plan", plan));
    }
    if let GovernanceActionIntent::RewardClaim {
        starting_interval,
        ending_interval,
        ..
    } = &draft.resolved.intent
    {
        let ranges = format!("{starting_interval}–{ending_interval}");
        rows.push(SpendAuthorizationSummaryRow::new("Reward ranges", ranges));
    }
    let description = if draft.workflow.is_some() {
        "Review the initial exact call; the ordered second call is rebuilt after confirmation and fresh state."
    } else {
        "Review the exact zero-value contract call before signing."
    };
    SpendAuthorizationSummary::new("Governance action", description, rows)
        .with_payload(
            "calldata",
            alloy::hex::encode_prefixed(&draft.review.calldata),
        )
        .with_warnings(warnings)
        .requiring_explicit_review()
}

pub(super) async fn build_typed_governance_spend_draft(
    target: GovernanceRefreshTarget,
    wallet_id: Arc<str>,
    actor_uuid: Arc<str>,
    actor: Address,
    actor_source: PublicAccountSource,
    contract: Address,
    contract_kind: GovernanceContractKind,
    observed_state: B256,
    action: GovernanceActionIntent,
    view_session: Arc<wallet_ops::vault::DesktopViewSession>,
    vault_store: Arc<wallet_ops::vault::DesktopVaultStore>,
    effective_chain: Option<wallet_ops::settings::EffectiveChainConfig>,
    http: wallet_ops::HttpContext,
    gas_fee_selection: PublicActionGasFeeSelection,
    workflow: Option<GovernanceWorkflow>,
    continuation: Option<GovernanceContinuation>,
    staking_review: Option<GovernanceStakingReviewProjection>,
    recipe: GovernanceDraftRecipe,
) -> Result<GovernanceSpendDraft, String> {
    let chain_id = match &target {
        GovernanceRefreshTarget::Proposal(key) => key.context.chain_id,
        GovernanceRefreshTarget::Staking(key) => key.chain_id,
    };
    let context = GovernanceActionContext {
        private_wallet_uuid: wallet_id.to_string(),
        chain_id,
        public_account_uuid: actor_uuid.to_string(),
        actor,
        contract,
        contract_kind,
        observed_state,
    };
    let resolved = action
        .resolve(&context)
        .map_err(|error| error.to_string())?;
    let estimate = wallet_ops::estimate_public_advanced_transaction(
        PublicAdvancedTransactionEstimateRequest {
            chain_id,
            effective_chain,
            from: actor,
            intent: resolved.raw.clone(),
            gas_fee: gas_fee_selection,
            access_list: None,
        },
        &http,
    )
    .await
    .map_err(|error| format_report_chain(&error))?;
    let estimate_completed_at = Instant::now();
    let review = GovernanceActionReview::from_resolved(
        &resolved,
        context,
        Some(estimate.fee_projection(None)),
    )
    .map_err(|error| error.to_string())?;
    let gas_fee = match gas_fee_selection {
        PublicActionGasFeeSelection::Auto => PublicActionGasFeeSelection::Custom {
            max_fee_per_gas: estimate.max_fee_per_gas,
            max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
        },
        custom @ PublicActionGasFeeSelection::Custom { .. } => custom,
    };
    Ok(GovernanceSpendDraft {
        target,
        actor_uuid,
        actor,
        actor_source,
        context: review.context.clone(),
        resolved,
        review,
        proposal_review: None,
        staking_review,
        estimate,
        estimate_completed_at,
        gas_fee,
        view_session,
        vault_store,
        workflow,
        continuation,
        recipe,
    })
}

pub(super) fn participation_capacity(
    participation: &GovernanceParticipation,
    voting: bool,
) -> Result<U256, GovernanceCapacityError> {
    let capacity = if voting {
        participation.voting_capacity()?
    } else {
        participation.sponsorship_capacity()?
    };
    capacity
        .remaining
        .ok_or(GovernanceCapacityError::SnapshotUnavailable)
}

pub(super) fn validate_proposal_action(
    proposal: &GovernanceProposal,
    rules: &GovernanceContractRules,
    chain_time: U256,
    participation: &GovernanceParticipation,
    kind: ProposalActionKind,
    amount: Option<U256>,
) -> Result<(), String> {
    let amount = amount.unwrap_or_default();
    match kind {
        ProposalActionKind::Sponsor => guard_sponsor(
            proposal,
            rules,
            Some(chain_time),
            amount,
            Some(&participation.last_sponsored),
        ),
        ProposalActionKind::Unsponsor => guard_unsponsor(
            proposal,
            rules,
            Some(chain_time),
            amount,
            participation.sponsored,
        ),
        ProposalActionKind::CallVote => guard_call_vote(proposal, rules, Some(chain_time)),
        ProposalActionKind::Yay => guard_yay_vote(proposal, rules, Some(chain_time), amount),
        ProposalActionKind::Nay => guard_nay_vote(proposal, rules, Some(chain_time), amount),
    }
    .map_err(|error: GovernanceGuardError| error.to_string())?;
    let capacity = matches!(kind, ProposalActionKind::Yay | ProposalActionKind::Nay)
        .then(|| participation_capacity(participation, true))
        .transpose()
        .map_err(|error| error.to_string())?;
    if let Some(remaining) = capacity
        && amount > remaining
    {
        return Err("Amount exceeds remaining voting capacity".to_owned());
    }
    if matches!(kind, ProposalActionKind::Sponsor) {
        let remaining =
            participation_capacity(participation, false).map_err(|error| error.to_string())?;
        if amount > remaining {
            return Err("Amount exceeds remaining sponsorship capacity".to_owned());
        }
    }
    Ok(())
}

fn governance_authorization_estimate_is_fresh(age: Duration) -> bool {
    age < GOVERNANCE_SPEND_AUTHORIZATION_TTL
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GovernanceExpiredAuthorizationDisposition {
    StaleGeneration,
    CurrentIdentityMismatch,
    CurrentIdentityMatch,
}

const fn governance_expired_authorization_disposition(
    generation_current: bool,
    identity_current: bool,
) -> GovernanceExpiredAuthorizationDisposition {
    if !generation_current {
        GovernanceExpiredAuthorizationDisposition::StaleGeneration
    } else if identity_current {
        GovernanceExpiredAuthorizationDisposition::CurrentIdentityMatch
    } else {
        GovernanceExpiredAuthorizationDisposition::CurrentIdentityMismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root::governance::GovernanceParticipantIdentity;

    #[test]
    fn governance_action_flow_keeps_one_protocol_selection() {
        let mut flow = crate::root::governance::GovernanceActionFlowState::default();
        flow.set_proposal_selection(ProposalActionSelection {
            actor: Address::ZERO,
            kind: ProposalActionKind::CallVote,
        });
        assert!(matches!(
            flow.selection,
            Some(crate::root::governance::GovernanceActionSelection::Proposal(_))
        ));
        assert!(flow.staking_selection().is_none());

        flow.set_staking_selection(crate::root::governance::StakingActionSelection {
            actor_uuid: "account".to_owned(),
            actor: Address::ZERO,
            kind: crate::root::governance::StakingActionKind::Stake,
        });
        assert!(!matches!(
            flow.selection,
            Some(crate::root::governance::GovernanceActionSelection::Proposal(_))
        ));
        assert_eq!(
            flow.staking_selection()
                .map(|selection| selection.actor_uuid.as_str()),
            Some("account"),
        );
    }

    #[test]
    fn governance_intents_use_semantic_progress_steps() {
        assert_eq!(
            WalletRoot::governance_progress_step(
                &GovernanceActionIntent::Yay {
                    proposal_version: GovernanceContractVersion::V2,
                    proposal_index: U256::ZERO,
                    amount: U256::ZERO,
                    snapshot_interval: U256::ZERO,
                    snapshot_hint: U256::ZERO,
                },
                None,
            ),
            PublicActionProgressStep::Vote,
        );
        assert_eq!(
            WalletRoot::governance_progress_step(
                &GovernanceActionIntent::RewardClaim {
                    reward_tokens: Vec::new(),
                    starting_interval: U256::ZERO,
                    ending_interval: U256::ZERO,
                    snapshot_hints: Vec::new(),
                    expected_amounts: Vec::new(),
                },
                Some(3),
            ),
            PublicActionProgressStep::RewardClaim(3),
        );
    }

    #[test]
    fn reward_progress_projection_preserves_ranges_and_hashes() {
        let reward_tokens = vec![Address::from([1; 20])];
        let step_intent = |start, end| GovernanceActionIntent::RewardClaim {
            reward_tokens: reward_tokens.clone(),
            starting_interval: U256::from(start),
            ending_interval: U256::from(end),
            snapshot_hints: vec![U256::ZERO],
            expected_amounts: vec![U256::from(1)],
        };
        let make_step = |start, end| wallet_ops::RewardClaimStepPlan {
            token: reward_tokens[0],
            recipient: Address::ZERO,
            starting_interval: U256::from(start),
            ending_interval: U256::from(end),
            hints: vec![U256::ZERO],
            expected_amount: U256::from(1),
            reward_tokens: reward_tokens.clone(),
            expected_amounts: vec![U256::from(1)],
            intent: step_intent(start, end),
            evidence_fingerprint: B256::ZERO,
        };
        let first = make_step(2, 3);
        let second = make_step(4, 6);
        let mut progress = wallet_ops::RewardClaimProgress::new(wallet_ops::RewardClaimPlan {
            chain_id: 1,
            actor: Address::ZERO,
            recipient: Address::ZERO,
            token: reward_tokens[0],
            reward_tokens: reward_tokens.clone(),
            steps: vec![first.clone(), second.clone()],
            evidence_fingerprint: B256::ZERO,
            fingerprint: B256::ZERO,
        });
        progress
            .confirmed_steps
            .push(wallet_ops::RewardClaimConfirmedStep {
                step: first,
                transaction_hash: B256::from([0x11; 32]),
            });
        progress
            .confirmed_steps
            .push(wallet_ops::RewardClaimConfirmedStep {
                step: second,
                transaction_hash: B256::from([0x22; 32]),
            });

        let projected = governance_reward_progress_steps(&progress, &step_intent(8, 9))
            .expect("reward intent projects to shared progress");
        let first_hash = B256::from([0x11; 32]).to_string();
        let second_hash = B256::from([0x22; 32]).to_string();
        assert_eq!(projected.len(), 3);
        assert_eq!(
            projected[0].interval.map(|range| (range.start, range.end)),
            Some((U256::from(2), U256::from(3)))
        );
        assert_eq!(projected[0].tx_hash.as_deref(), Some(first_hash.as_str()));
        assert_eq!(
            projected[1].interval.map(|range| (range.start, range.end)),
            Some((U256::from(4), U256::from(6)))
        );
        assert_eq!(projected[1].tx_hash.as_deref(), Some(second_hash.as_str()));
        assert_eq!(
            projected[2].interval.map(|range| (range.start, range.end)),
            Some((U256::from(8), U256::from(9)))
        );
        assert_eq!(projected[2].status, PublicActionStepStatus::Pending);
        assert!(projected[2].tx_hash.is_none());
    }

    fn test_proposal(index: u64) -> GovernanceProposal {
        GovernanceProposal {
            contract_version: GovernanceContractVersion::V2,
            index: U256::from(index),
            contract_address: Address::ZERO,
            proposer: Address::ZERO,
            proposal_document: String::new(),
            publish_time: U256::ZERO,
            vote_call_time: U256::ZERO,
            sponsorship: U256::ZERO,
            executed: false,
            yay_votes: U256::ZERO,
            nay_votes: U256::ZERO,
            sponsor_snapshot_interval: U256::ZERO,
            voting_snapshot_interval: U256::ZERO,
            actions: Vec::new(),
        }
    }

    #[test]
    fn proposal_key_requires_selected_proposal_and_current_participation() {
        let context = GovernanceContextKey {
            wallet_id: Some("wallet".to_owned()),
            chain_id: 1,
            participants: vec![GovernanceParticipantIdentity {
                uuid: "account".to_owned(),
                address: Address::ZERO,
            }],
        };
        let proposal = test_proposal(1);
        let other_proposal = test_proposal(2);
        let key = proposal_participation_key(&proposal, context.clone());

        assert!(proposal_participation_key_matches(
            &proposal,
            Some(&proposal),
            &context,
            Some(&key),
            &key,
        ));
        assert!(!proposal_participation_key_matches(
            &proposal,
            Some(&other_proposal),
            &context,
            Some(&key),
            &key,
        ));
        assert!(!proposal_participation_key_matches(
            &proposal,
            Some(&proposal),
            &context,
            None,
            &key,
        ));
    }

    #[test]
    fn governance_authorization_estimate_expires_at_ttl_boundary() {
        assert!(governance_authorization_estimate_is_fresh(
            Duration::from_secs(119)
        ));
        assert!(!governance_authorization_estimate_is_fresh(
            GOVERNANCE_SPEND_AUTHORIZATION_TTL
        ));
        assert!(!governance_authorization_estimate_is_fresh(
            Duration::from_secs(121)
        ));
    }

    #[test]
    fn expired_authorization_completion_prioritizes_generation_ownership() {
        assert_eq!(
            governance_expired_authorization_disposition(false, false),
            GovernanceExpiredAuthorizationDisposition::StaleGeneration
        );
        assert_eq!(
            governance_expired_authorization_disposition(false, true),
            GovernanceExpiredAuthorizationDisposition::StaleGeneration
        );
        assert_eq!(
            governance_expired_authorization_disposition(true, false),
            GovernanceExpiredAuthorizationDisposition::CurrentIdentityMismatch
        );
        assert_eq!(
            governance_expired_authorization_disposition(true, true),
            GovernanceExpiredAuthorizationDisposition::CurrentIdentityMatch
        );
    }
}
