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

mod authorization;
mod drafts;
mod submission;
mod summary;

pub(super) use drafts::{
    build_governance_spend_draft, build_typed_governance_spend_draft, validate_proposal_action,
};
use drafts::{proposal_participation_key_matches, read_proposal_action_state};
use summary::governance_authorization_summary;

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
    /// The action's operation client, which its re-estimates, submission and continuation use.
    pub operation: wallet_ops::OperationHttpClientSlot,
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

/// Chain, contract version, contract, proposal index and account of a loaded row.
type LoadedParticipationKey = (u64, GovernanceContractVersion, Address, U256, Address);

/// Chain, contract version, contract and account of an action's invalidation.
type ContractAccountKey = (u64, GovernanceContractVersion, Address, Address);

#[derive(Clone, Debug, Default)]
pub(super) struct ProposalParticipationState {
    pub key: Option<ProposalParticipationKey>,
    pub generation: u64,
    /// `Loading` and `Unavailable` rows of the current request for `key`.
    requested: BTreeMap<Address, ProposalParticipationRow>,
    /// `Ready` rows loaded this wallet session, with the sequence of the read that loaded each.
    loaded: BTreeMap<LoadedParticipationKey, (ProposalParticipationRow, u64)>,
    /// Sequence at the latest successful action of an account on a contract. Reads started
    /// before it are not stored there.
    invalidated: BTreeMap<ContractAccountKey, u64>,
    /// Sequence source of reads and invalidations; it never resets.
    sequence: u64,
    /// Sequence of the current request's read.
    request_read: u64,
    /// Sequence at the last session clear. Reads started before it are not stored.
    session_start: u64,
}

const fn participation_intervals(participation: &GovernanceParticipation) -> (U256, U256) {
    (
        participation.sponsorship_snapshot.interval,
        participation.voting_snapshot.interval,
    )
}

const fn proposal_intervals(proposal: &GovernanceProposal) -> (U256, U256) {
    (
        proposal.sponsor_snapshot_interval,
        proposal.voting_snapshot_interval,
    )
}

impl ProposalParticipationState {
    const fn next_sequence(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    /// Stores `participation` read by `read` unless an action on its contract or a session clear
    /// came after the read started, or the stored row is newer. Snapshot intervals only move
    /// forward for a proposal, so a row with older intervals is stale; with equal intervals the
    /// later read wins.
    fn admit(
        &mut self,
        loaded: LoadedParticipationKey,
        read: u64,
        participation: GovernanceParticipation,
    ) -> bool {
        let (chain, version, contract, _, account) = loaded;
        if read <= self.session_start
            || self
                .invalidated
                .get(&(chain, version, contract, account))
                .is_some_and(|invalidated| *invalidated > read)
        {
            return false;
        }
        if let Some((ProposalParticipationRow::Ready(stored), stored_read)) =
            self.loaded.get(&loaded)
            && (participation_intervals(stored), *stored_read)
                > (participation_intervals(&participation), read)
        {
            return false;
        }
        self.loaded.insert(
            loaded,
            (
                ProposalParticipationRow::Ready(Box::new(participation)),
                read,
            ),
        );
        true
    }

    const fn loaded_key(
        key: &ProposalParticipationKey,
        account: Address,
    ) -> LoadedParticipationKey {
        (
            key.context.chain_id,
            key.version,
            key.contract,
            key.index,
            account,
        )
    }

    /// Shows `key` without reading; an in-flight request for the same key keeps running.
    pub(super) fn show(&mut self, key: &ProposalParticipationKey) {
        if self.key.as_ref() != Some(key) {
            self.generation = self.generation.wrapping_add(1);
            self.key = Some(key.clone());
            self.requested.clear();
        }
    }

    /// Starts a request for `accounts`, replacing the current request's rows.
    pub(super) fn begin(&mut self, key: &ProposalParticipationKey, accounts: &[Address]) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.request_read = self.next_sequence();
        self.key = Some(key.clone());
        self.requested = accounts
            .iter()
            .map(|account| (*account, ProposalParticipationRow::Loading))
            .collect();
        self.generation
    }

    pub(super) fn current(&self, key: &ProposalParticipationKey, generation: u64) -> bool {
        self.generation == generation && self.key.as_ref() == Some(key)
    }

    /// Applies a read of the current request; a read retired by an action is dropped.
    pub(super) fn apply(
        &mut self,
        key: &ProposalParticipationKey,
        generation: u64,
        account: Address,
        row: Result<GovernanceParticipation, Arc<str>>,
    ) -> bool {
        if !self.current(key, generation)
            || !matches!(
                self.requested.get(&account),
                Some(ProposalParticipationRow::Loading)
            )
        {
            return false;
        }
        match row {
            Ok(value) => {
                self.requested.remove(&account);
                self.admit(Self::loaded_key(key, account), self.request_read, value);
            }
            Err(error) => {
                self.requested
                    .insert(account, ProposalParticipationRow::Unavailable(error));
            }
        }
        true
    }

    /// The row shown for `account` on `proposal`, or `None` when it is not loaded.
    pub(super) fn row(
        &self,
        proposal: &GovernanceProposal,
        account: &Address,
    ) -> Option<&ProposalParticipationRow> {
        let key = self.key.as_ref().filter(|key| {
            key.version == proposal.contract_version
                && key.contract == proposal.contract_address
                && key.index == proposal.index
        })?;
        if let Some(row) = self.requested.get(account) {
            return Some(row);
        }
        // A row read before the vote was called has a stale voting interval and is not loaded.
        self.loaded
            .get(&Self::loaded_key(key, *account))
            .map(|(row, _)| row)
            .filter(|row| {
                matches!(row, ProposalParticipationRow::Ready(participation)
                    if participation_intervals(participation) == proposal_intervals(proposal))
            })
    }

    /// The key's participants without a `Ready` row for `proposal`. A row newer than `proposal`
    /// shows once its page refreshes, so it is not read again.
    pub(super) fn unloaded(&self, proposal: &GovernanceProposal) -> Vec<Address> {
        self.key.as_ref().map_or_else(Vec::new, |key| {
            key.context
                .participants
                .iter()
                .map(|participant| participant.address)
                .filter(|account| {
                    !matches!(
                        self.row(proposal, account),
                        Some(ProposalParticipationRow::Ready(_))
                    ) && !self
                        .loaded
                        .get(&Self::loaded_key(key, *account))
                        .is_some_and(|(row, _)| {
                            matches!(row, ProposalParticipationRow::Ready(participation)
                                if participation_intervals(participation)
                                    > proposal_intervals(proposal))
                        })
                })
                .collect()
        })
    }

    #[cfg(test)]
    pub(super) fn loading(&self) -> bool {
        self.requested
            .values()
            .any(|row| matches!(row, ProposalParticipationRow::Loading))
    }

    /// Drops the actor's pre-action rows on every proposal of the acted contract, since
    /// `lastSponsored` is per account and contract, and retires every read of the actor there
    /// started before, including the current request's. Returns the sequence of the action's own
    /// read, which `apply_action_read` takes.
    pub(super) fn invalidate_actor_on_contract(
        &mut self,
        key: &ProposalParticipationKey,
        actor: Address,
    ) -> u64 {
        let acted = (key.context.chain_id, key.version, key.contract, actor);
        let same_contract = |(chain, version, contract, _, account): &LoadedParticipationKey| {
            (*chain, *version, *contract, *account) == acted
        };
        let invalidated = self.next_sequence();
        self.invalidated.insert(acted, invalidated);
        self.loaded.retain(|loaded, _| !same_contract(loaded));
        if self
            .key
            .as_ref()
            .is_some_and(|current| same_contract(&Self::loaded_key(current, actor)))
        {
            self.requested.remove(&actor);
        }
        self.next_sequence()
    }

    /// Stores the actor's row read after its action on `chain_id`'s fresh `proposal`. It replaces
    /// the current request's row for the actor there, which would otherwise hide it.
    pub(super) fn apply_action_read(
        &mut self,
        chain_id: u64,
        proposal: &GovernanceProposal,
        actor: Address,
        read: u64,
        participation: GovernanceParticipation,
    ) -> bool {
        let loaded = (
            chain_id,
            proposal.contract_version,
            proposal.contract_address,
            proposal.index,
            actor,
        );
        if !self.admit(loaded, read, participation) {
            return false;
        }
        if self
            .key
            .as_ref()
            .is_some_and(|key| Self::loaded_key(key, actor) == loaded)
        {
            self.requested.remove(&actor);
        }
        true
    }

    /// Stops the current request; loaded rows stay for the session.
    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.key = None;
        self.requested.clear();
    }

    /// Discards everything loaded this wallet session, and every read still outstanding.
    pub(super) fn clear_session(&mut self) {
        self.invalidate();
        self.loaded.clear();
        self.invalidated.clear();
        self.session_start = self.sequence;
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
