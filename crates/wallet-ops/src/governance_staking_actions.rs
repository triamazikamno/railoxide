//! Pure planning and guards for staking, delegation, principal, and reward writes.
//!
//! These helpers do not sign, submit, or estimate transactions.  They turn fresh read models into
//! typed intents and keep the continuation boundaries explicit for writes whose first transaction
//! changes the state needed by the next one.

use alloy::primitives::{Address, B256, U256, keccak256};
use railgun_ui::governance_contracts;
use thiserror::Error;

use crate::{
    GovernanceActionIntent, RewardBatchEvidence, RewardClaimStep, RewardEvidence, StakePosition,
    StakeState,
};

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum GovernancePlanningError {
    #[error("governance action amount must be positive")]
    NonPositiveAmount,
    #[error("stake amount exceeds the fresh governance-token balance")]
    InsufficientBalance,
    #[error("fresh governance-token allowance is still below the stake amount")]
    InsufficientAllowance,
    #[error("stake continuation is not required for this allowance")]
    StakeContinuationNotRequired,
    #[error("stake inputs changed before continuation")]
    StakeInputsChanged,
    #[error("approval amount cannot use the unbounded U256 maximum")]
    UnboundedApproval,
    #[error("stake state is not active")]
    StakeNotActive,
    #[error("stake owner does not match the acting account")]
    WrongStakeOwner,
    #[error("stake has already been claimed")]
    StakeClaimed,
    #[error("stake is already unlocking")]
    StakeUnlocking,
    #[error("delegate cannot be the zero address")]
    ZeroDelegate,
    #[error("delegate is unchanged")]
    UnchangedDelegate,
    #[error("stake identity changed before unlock continuation")]
    UnlockIdentityChanged,
    #[error("stake delegate has not returned to its owner")]
    UnlockStillDelegated,
    #[error("stake locktime is zero")]
    MissingLocktime,
    #[error("stake locktime has not elapsed")]
    LocktimeNotElapsed,
    #[error("reward actor and recipient must match")]
    RewardActorRecipientMismatch,
    #[error("reward actor is inactive")]
    InactiveActor,
    #[error("reward token is not configured for this chain")]
    UnsupportedRewardToken,
    #[error("reward amount must be positive")]
    ZeroReward,
    #[error("reward range is invalid")]
    InvalidRewardRange,
    #[error("reward hints do not match the inclusive interval range")]
    InvalidRewardHints,
    #[error("reward evidence token does not match the planned token")]
    RewardTokenMismatch,
    #[error("reward evidence changed before continuation")]
    StaleRewardEvidence,
    #[error("reward continuation changed chain or configured registry")]
    RewardChainChanged,
    #[error("reward step history is not confirmed")]
    UnconfirmedRewardStep,
    #[error("reward token addresses must be strictly ascending and unique")]
    InvalidRewardTokenOrder,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakeObservedState {
    pub actor: Address,
    pub staking: Address,
    pub balance: U256,
    pub allowance: U256,
}

impl StakeObservedState {
    #[must_use]
    pub fn fingerprint(&self, amount: U256) -> B256 {
        let mut bytes = Vec::with_capacity(32 * 5 + 24);
        bytes.extend_from_slice(b"railoxide:stake-observed:v1");
        append_address(&mut bytes, self.actor);
        append_address(&mut bytes, self.staking);
        append_u256(&mut bytes, self.balance);
        append_u256(&mut bytes, self.allowance);
        append_u256(&mut bytes, amount);
        keccak256(bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakePlan {
    pub actor: Address,
    pub staking: Address,
    pub amount: U256,
    pub approval: Option<GovernanceActionIntent>,
    /// This is absent until an approval-required plan has confirmed and rebuilt from fresh state.
    pub stake: Option<GovernanceActionIntent>,
    pub observed_state: B256,
    /// Approval-required plans leave this empty until block-scoped confirmation and a fresh
    /// allowance read pass through `continue_stake_after_approval`.
    pub requires_approval_confirmation: bool,
}

pub fn plan_stake(
    actor: Address,
    staking: Address,
    balance: U256,
    allowance: U256,
    amount: U256,
) -> Result<StakePlan, GovernancePlanningError> {
    if amount.is_zero() {
        return Err(GovernancePlanningError::NonPositiveAmount);
    }
    if amount > balance {
        return Err(GovernancePlanningError::InsufficientBalance);
    }
    if allowance < amount && amount == U256::MAX {
        return Err(GovernancePlanningError::UnboundedApproval);
    }
    let observed = StakeObservedState {
        actor,
        staking,
        balance,
        allowance,
    };
    let requires_approval_confirmation = allowance < amount;
    let approval =
        requires_approval_confirmation.then_some(GovernanceActionIntent::GovernanceTokenApproval {
            spender: staking,
            amount,
        });
    Ok(StakePlan {
        actor,
        staking,
        amount,
        approval,
        stake: (!requires_approval_confirmation)
            .then_some(GovernanceActionIntent::Stake { amount }),
        observed_state: observed.fingerprint(amount),
        requires_approval_confirmation,
    })
}

/// Rebuild the stake leg only after approval confirmation and a fresh balance/allowance read.
pub fn continue_stake_after_approval(
    initial: &StakePlan,
    fresh_balance: U256,
    fresh_allowance: U256,
) -> Result<StakePlan, GovernancePlanningError> {
    if !initial.requires_approval_confirmation
        || initial.approval.is_none()
        || initial.stake.is_some()
    {
        return Err(GovernancePlanningError::StakeContinuationNotRequired);
    }
    let rebuilt = plan_stake(
        initial.actor,
        initial.staking,
        fresh_balance,
        fresh_allowance,
        initial.amount,
    )?;
    if rebuilt.actor != initial.actor
        || rebuilt.staking != initial.staking
        || rebuilt.amount != initial.amount
        || rebuilt.stake
            != Some(GovernanceActionIntent::Stake {
                amount: initial.amount,
            })
    {
        return Err(GovernancePlanningError::StakeInputsChanged);
    }
    if rebuilt.approval.is_some() || rebuilt.stake.is_none() {
        return Err(GovernancePlanningError::InsufficientAllowance);
    }
    Ok(StakePlan {
        requires_approval_confirmation: false,
        ..rebuilt
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationEvidence {
    pub actor: Address,
    pub owner: Address,
    pub stake_id: U256,
    pub amount: U256,
    pub previous_delegate: Address,
    pub next_delegate: Address,
    pub impact: DelegationImpact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationImpact {
    pub voting_power_moves: bool,
    pub reward_recipient_moves: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelegationPlan {
    pub intent: GovernanceActionIntent,
    pub evidence: DelegationEvidence,
    pub observed_state: B256,
}

fn validate_active_owner(
    actor: Address,
    position: &StakePosition,
) -> Result<(), GovernancePlanningError> {
    if position.owner != actor {
        return Err(GovernancePlanningError::WrongStakeOwner);
    }
    if position.claimed_time != U256::ZERO {
        return Err(GovernancePlanningError::StakeClaimed);
    }
    if position.state != StakeState::Active {
        return Err(GovernancePlanningError::StakeNotActive);
    }
    Ok(())
}

pub fn plan_delegate(
    actor: Address,
    position: &StakePosition,
    delegate: Address,
) -> Result<DelegationPlan, GovernancePlanningError> {
    validate_active_owner(actor, position)?;
    if delegate == Address::ZERO {
        return Err(GovernancePlanningError::ZeroDelegate);
    }
    if delegate == position.delegate {
        return Err(GovernancePlanningError::UnchangedDelegate);
    }
    let evidence = DelegationEvidence {
        actor,
        owner: position.owner,
        stake_id: position.id,
        amount: position.amount,
        previous_delegate: position.delegate,
        next_delegate: delegate,
        impact: DelegationImpact {
            voting_power_moves: true,
            reward_recipient_moves: true,
        },
    };
    Ok(DelegationPlan {
        intent: GovernanceActionIntent::Delegate {
            stake_id: position.id,
            delegate,
        },
        observed_state: delegation_fingerprint(&evidence),
        evidence,
    })
}

pub fn plan_undelegate(
    actor: Address,
    position: &StakePosition,
) -> Result<DelegationPlan, GovernancePlanningError> {
    validate_active_owner(actor, position)?;
    if position.delegate == position.owner {
        return Err(GovernancePlanningError::UnchangedDelegate);
    }
    let evidence = DelegationEvidence {
        actor,
        owner: position.owner,
        stake_id: position.id,
        amount: position.amount,
        previous_delegate: position.delegate,
        next_delegate: position.owner,
        impact: DelegationImpact {
            voting_power_moves: true,
            reward_recipient_moves: true,
        },
    };
    Ok(DelegationPlan {
        intent: GovernanceActionIntent::Undelegate {
            stake_id: position.id,
        },
        observed_state: delegation_fingerprint(&evidence),
        evidence,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnlockPlan {
    Direct(UnlockIntent),
    UndelegateFirst(UndelegateFirstPlan),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnlockIntent {
    pub intent: GovernanceActionIntent,
    pub owner: Address,
    pub stake_id: U256,
    pub amount: U256,
    pub delegate: Address,
    pub observed_state: B256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UndelegateFirstPlan {
    pub intent: GovernanceActionIntent,
    pub owner: Address,
    pub stake_id: U256,
    pub amount: U256,
    pub previous_delegate: Address,
    pub observed_state: B256,
}

pub fn plan_unlock(
    actor: Address,
    position: &StakePosition,
) -> Result<UnlockPlan, GovernancePlanningError> {
    validate_active_owner(actor, position)?;
    let observed_state = stake_position_fingerprint(position);
    if position.delegate != position.owner {
        return Ok(UnlockPlan::UndelegateFirst(UndelegateFirstPlan {
            intent: GovernanceActionIntent::Undelegate {
                stake_id: position.id,
            },
            owner: position.owner,
            stake_id: position.id,
            amount: position.amount,
            previous_delegate: position.delegate,
            observed_state,
        }));
    }
    Ok(UnlockPlan::Direct(UnlockIntent {
        intent: GovernanceActionIntent::Unlock {
            stake_id: position.id,
        },
        owner: position.owner,
        stake_id: position.id,
        amount: position.amount,
        delegate: position.delegate,
        observed_state,
    }))
}

/// Build the unlock leg only from fresh state after the undelegate transaction confirms.
pub fn continue_unlock_after_undelegation(
    first: &UndelegateFirstPlan,
    fresh: &StakePosition,
) -> Result<UnlockIntent, GovernancePlanningError> {
    let owner_matches = fresh.owner == first.owner;
    let stake_id_matches = fresh.id == first.stake_id;
    let amount_matches = fresh.amount == first.amount;
    if !owner_matches || !stake_id_matches || !amount_matches {
        return Err(GovernancePlanningError::UnlockIdentityChanged);
    }
    if fresh.delegate != fresh.owner {
        return Err(GovernancePlanningError::UnlockStillDelegated);
    }
    validate_active_owner(first.owner, fresh)?;
    Ok(UnlockIntent {
        intent: GovernanceActionIntent::Unlock { stake_id: fresh.id },
        owner: fresh.owner,
        stake_id: fresh.id,
        amount: fresh.amount,
        delegate: fresh.delegate,
        observed_state: stake_position_fingerprint(fresh),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrincipalClaimPlan {
    pub intent: GovernanceActionIntent,
    pub owner: Address,
    pub recipient: Address,
    pub stake_id: U256,
    pub amount: U256,
    pub locktime: U256,
    pub chain_time: U256,
    pub observed_state: B256,
}

pub fn plan_principal_claim(
    actor: Address,
    position: &StakePosition,
    latest_chain_time: U256,
) -> Result<PrincipalClaimPlan, GovernancePlanningError> {
    if position.owner != actor {
        return Err(GovernancePlanningError::WrongStakeOwner);
    }
    if position.claimed_time != U256::ZERO {
        return Err(GovernancePlanningError::StakeClaimed);
    }
    if position.locktime.is_zero() {
        return Err(GovernancePlanningError::MissingLocktime);
    }
    if latest_chain_time <= position.locktime {
        return Err(GovernancePlanningError::LocktimeNotElapsed);
    }
    if position.amount.is_zero() {
        return Err(GovernancePlanningError::NonPositiveAmount);
    }
    Ok(PrincipalClaimPlan {
        intent: GovernanceActionIntent::PrincipalClaim {
            stake_id: position.id,
        },
        owner: position.owner,
        recipient: position.owner,
        stake_id: position.id,
        amount: position.amount,
        locktime: position.locktime,
        chain_time: latest_chain_time,
        observed_state: principal_claim_fingerprint(actor, position, latest_chain_time),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardClaimStepPlan {
    /// Legacy single-row fields. Bulk claims use the vectors below.
    pub token: Address,
    pub recipient: Address,
    pub starting_interval: U256,
    pub ending_interval: U256,
    pub hints: Vec<U256>,
    pub expected_amount: U256,
    pub reward_tokens: Vec<Address>,
    pub expected_amounts: Vec<U256>,
    pub intent: GovernanceActionIntent,
    pub evidence_fingerprint: B256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardClaimPlan {
    pub chain_id: u64,
    pub actor: Address,
    pub recipient: Address,
    pub token: Address,
    pub reward_tokens: Vec<Address>,
    pub steps: Vec<RewardClaimStepPlan>,
    pub evidence_fingerprint: B256,
    pub fingerprint: B256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardClaimConfirmedStep {
    pub step: RewardClaimStepPlan,
    pub transaction_hash: B256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardClaimProgress {
    pub plan: RewardClaimPlan,
    pub confirmed_steps: Vec<RewardClaimConfirmedStep>,
    next_step_index: usize,
    chain_id: u64,
}

impl RewardClaimProgress {
    #[must_use]
    pub const fn new(plan: RewardClaimPlan) -> Self {
        Self {
            chain_id: plan.chain_id,
            plan,
            confirmed_steps: Vec::new(),
            next_step_index: 0,
        }
    }

    #[must_use]
    pub fn confirmed(&self) -> &[RewardClaimConfirmedStep] {
        &self.confirmed_steps
    }

    #[must_use]
    pub fn reattach_confirmed_history(&self, plan: RewardClaimPlan) -> Self {
        Self {
            chain_id: plan.chain_id,
            plan,
            confirmed_steps: self.confirmed_steps.clone(),
            next_step_index: 0,
        }
    }
}

pub fn plan_reward_claim(
    chain_id: u64,
    actor: Address,
    recipient: Address,
    token: Address,
    evidence: &RewardEvidence,
    steps: &[RewardClaimStep],
) -> Result<RewardClaimPlan, GovernancePlanningError> {
    if actor != recipient {
        return Err(GovernancePlanningError::RewardActorRecipientMismatch);
    }
    if !configured_reward_token(chain_id, token) {
        return Err(GovernancePlanningError::UnsupportedRewardToken);
    }
    validate_reward_evidence(token, evidence)?;
    if steps.is_empty() {
        return Err(GovernancePlanningError::ZeroReward);
    }
    let expected_range = inclusive_count(evidence.starting_interval, evidence.ending_interval)?;
    if expected_range != evidence.hints.len() || expected_range != evidence.staking_intervals.len()
    {
        return Err(GovernancePlanningError::InvalidRewardHints);
    }
    let mut plans = Vec::with_capacity(steps.len());
    let mut expected_next = evidence.starting_interval;
    let mut subtotal = U256::ZERO;
    for step in steps {
        if step.starting_interval < evidence.starting_interval
            || step.ending_interval > evidence.ending_interval
            || step.starting_interval > step.ending_interval
        {
            return Err(GovernancePlanningError::InvalidRewardRange);
        }
        if step.starting_interval != expected_next {
            return Err(GovernancePlanningError::InvalidRewardRange);
        }
        if step.subtotal.is_zero() {
            return Err(GovernancePlanningError::ZeroReward);
        }
        let start = usize::try_from(step.starting_interval - evidence.starting_interval)
            .map_err(|_| GovernancePlanningError::InvalidRewardRange)?;
        let count = inclusive_count(step.starting_interval, step.ending_interval)?;
        let end = start
            .checked_add(count)
            .ok_or(GovernancePlanningError::InvalidRewardRange)?;
        if end > evidence.hints.len() {
            return Err(GovernancePlanningError::InvalidRewardHints);
        }
        let hints = evidence.hints[start..end].to_vec();
        subtotal = subtotal
            .checked_add(step.subtotal)
            .ok_or(GovernancePlanningError::ZeroReward)?;
        expected_next = step
            .ending_interval
            .checked_add(U256::from(1_u8))
            .ok_or(GovernancePlanningError::InvalidRewardRange)?;
        let intent = GovernanceActionIntent::RewardClaim {
            reward_tokens: vec![token],
            starting_interval: step.starting_interval,
            ending_interval: step.ending_interval,
            snapshot_hints: hints.clone(),
            expected_amounts: vec![step.subtotal],
        };
        let fingerprint = reward_evidence_fingerprint(evidence);
        plans.push(RewardClaimStepPlan {
            token,
            recipient,
            starting_interval: step.starting_interval,
            ending_interval: step.ending_interval,
            hints,
            expected_amount: step.subtotal,
            reward_tokens: vec![token],
            expected_amounts: vec![step.subtotal],
            intent,
            evidence_fingerprint: fingerprint,
        });
    }
    if expected_next
        != evidence
            .ending_interval
            .checked_add(U256::from(1_u8))
            .ok_or(GovernancePlanningError::InvalidRewardRange)?
        || subtotal != evidence.amount
    {
        return Err(GovernancePlanningError::InvalidRewardRange);
    }
    let evidence_fingerprint = reward_evidence_fingerprint(evidence);
    Ok(RewardClaimPlan {
        chain_id,
        actor,
        recipient,
        token,
        reward_tokens: vec![token],
        steps: plans,
        evidence_fingerprint,
        fingerprint: reward_plan_fingerprint(chain_id, evidence),
    })
}

/// Keep inactive participant rows read-only by checking account lifecycle before planning.
pub fn plan_active_reward_claim(
    chain_id: u64,
    actor: Address,
    recipient: Address,
    active: bool,
    token: Address,
    evidence: &RewardEvidence,
    steps: &[RewardClaimStep],
) -> Result<RewardClaimPlan, GovernancePlanningError> {
    if !active {
        return Err(GovernancePlanningError::InactiveActor);
    }
    plan_reward_claim(chain_id, actor, recipient, token, evidence, steps)
}

/// Plan an atomic claim for every configured reward token in one shared interval range.
pub fn plan_reward_claim_batch(
    chain_id: u64,
    actor: Address,
    recipient: Address,
    active: bool,
    evidence: &RewardBatchEvidence,
    steps: &[RewardClaimStep],
) -> Result<RewardClaimPlan, GovernancePlanningError> {
    if !active {
        return Err(GovernancePlanningError::InactiveActor);
    }
    if actor != recipient {
        return Err(GovernancePlanningError::RewardActorRecipientMismatch);
    }
    if evidence.reward_tokens.is_empty() {
        return Err(GovernancePlanningError::InvalidRewardTokenOrder);
    }
    validate_reward_token_order(&evidence.reward_tokens)?;
    if evidence.expected_amounts.len() != evidence.reward_tokens.len()
        || evidence.expected_amounts.iter().all(U256::is_zero)
    {
        return Err(GovernancePlanningError::ZeroReward);
    }
    if evidence
        .reward_tokens
        .iter()
        .any(|&token| !configured_reward_token(chain_id, token))
    {
        return Err(GovernancePlanningError::UnsupportedRewardToken);
    }
    let expected_range = inclusive_count(evidence.starting_interval, evidence.ending_interval)?;
    if evidence.hints.len() != expected_range
        || evidence.staking_intervals.len() != expected_range
        || evidence.claimed_intervals.len() != evidence.reward_tokens.len()
    {
        return Err(GovernancePlanningError::InvalidRewardHints);
    }
    let mut plans = Vec::with_capacity(steps.len());
    let mut expected_next = evidence.starting_interval;
    let mut totals = vec![U256::ZERO; evidence.reward_tokens.len()];
    for step in steps {
        if step.reward_tokens != evidence.reward_tokens
            || step.expected_amounts.len() != evidence.expected_amounts.len()
            || step.starting_interval < evidence.starting_interval
            || step.ending_interval > evidence.ending_interval
            || step.starting_interval > step.ending_interval
            || step.starting_interval != expected_next
        {
            return Err(GovernancePlanningError::InvalidRewardRange);
        }
        if step.expected_amounts.iter().all(U256::is_zero) {
            return Err(GovernancePlanningError::ZeroReward);
        }
        let start = usize::try_from(step.starting_interval - evidence.starting_interval)
            .map_err(|_| GovernancePlanningError::InvalidRewardRange)?;
        let count = inclusive_count(step.starting_interval, step.ending_interval)?;
        let end = start
            .checked_add(count)
            .ok_or(GovernancePlanningError::InvalidRewardRange)?;
        if end > evidence.hints.len() {
            return Err(GovernancePlanningError::InvalidRewardHints);
        }
        for (total, amount) in totals.iter_mut().zip(&step.expected_amounts) {
            *total = total
                .checked_add(*amount)
                .ok_or(GovernancePlanningError::ZeroReward)?;
        }
        expected_next = step
            .ending_interval
            .checked_add(U256::from(1_u8))
            .ok_or(GovernancePlanningError::InvalidRewardRange)?;
        let hints = evidence.hints[start..end].to_vec();
        let intent = GovernanceActionIntent::RewardClaim {
            reward_tokens: evidence.reward_tokens.clone(),
            starting_interval: step.starting_interval,
            ending_interval: step.ending_interval,
            snapshot_hints: hints.clone(),
            expected_amounts: step.expected_amounts.clone(),
        };
        plans.push(RewardClaimStepPlan {
            token: Address::ZERO,
            recipient,
            starting_interval: step.starting_interval,
            ending_interval: step.ending_interval,
            hints,
            expected_amount: U256::ZERO,
            reward_tokens: evidence.reward_tokens.clone(),
            expected_amounts: step.expected_amounts.clone(),
            intent,
            evidence_fingerprint: reward_batch_evidence_fingerprint(evidence),
        });
    }
    if plans.is_empty()
        || expected_next
            != evidence
                .ending_interval
                .checked_add(U256::from(1_u8))
                .ok_or(GovernancePlanningError::InvalidRewardRange)?
        || totals != evidence.expected_amounts
    {
        return Err(GovernancePlanningError::InvalidRewardRange);
    }
    Ok(RewardClaimPlan {
        chain_id,
        actor,
        recipient,
        token: Address::ZERO,
        reward_tokens: evidence.reward_tokens.clone(),
        steps: plans,
        evidence_fingerprint: reward_batch_evidence_fingerprint(evidence),
        fingerprint: reward_batch_plan_fingerprint(chain_id, evidence),
    })
}

/// Return only the next step, and only when a fresh calculation still matches the reviewed one.
pub fn next_reward_claim_step(
    progress: &RewardClaimProgress,
    fresh_evidence: &RewardEvidence,
) -> Result<Option<RewardClaimStepPlan>, GovernancePlanningError> {
    if reward_evidence_fingerprint(fresh_evidence) != progress.plan.evidence_fingerprint {
        return Err(GovernancePlanningError::StaleRewardEvidence);
    }
    if progress.next_step_index > progress.plan.steps.len() {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    }
    Ok(progress.plan.steps.get(progress.next_step_index).cloned())
}

pub fn confirm_reward_claim_step(
    progress: &mut RewardClaimProgress,
    step: &RewardClaimStepPlan,
    fresh_evidence: &RewardEvidence,
    transaction_hash: B256,
) -> Result<(), GovernancePlanningError> {
    let Some(next) = next_reward_claim_step(progress, fresh_evidence)? else {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    };
    if &next != step {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    }
    progress.confirmed_steps.push(RewardClaimConfirmedStep {
        step: next,
        transaction_hash,
    });
    progress.next_step_index += 1;
    Ok(())
}

/// Rebuild the remaining sequence from a newly calculated reward row after a confirmed step.
/// Confirmed history stays attached to the returned progress value, while the next handoff starts
/// at the first step in the fresh plan.
pub fn rebuild_reward_claim_continuation(
    progress: &RewardClaimProgress,
    fresh_evidence: &RewardEvidence,
    fresh_steps: &[RewardClaimStep],
) -> Result<RewardClaimProgress, GovernancePlanningError> {
    if progress.confirmed_steps.is_empty() {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    }
    if progress.plan.chain_id != progress.chain_id {
        return Err(GovernancePlanningError::RewardChainChanged);
    }
    let plan = plan_reward_claim(
        progress.chain_id,
        progress.plan.actor,
        progress.plan.recipient,
        progress.plan.token,
        fresh_evidence,
        fresh_steps,
    )?;
    Ok(RewardClaimProgress {
        plan,
        confirmed_steps: progress.confirmed_steps.clone(),
        next_step_index: 0,
        chain_id: progress.chain_id,
    })
}

pub fn next_reward_claim_batch_step(
    progress: &RewardClaimProgress,
    fresh_evidence: &RewardBatchEvidence,
) -> Result<Option<RewardClaimStepPlan>, GovernancePlanningError> {
    if reward_batch_evidence_fingerprint(fresh_evidence) != progress.plan.evidence_fingerprint {
        return Err(GovernancePlanningError::StaleRewardEvidence);
    }
    if progress.next_step_index > progress.plan.steps.len() {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    }
    Ok(progress.plan.steps.get(progress.next_step_index).cloned())
}

pub fn confirm_reward_claim_batch_step(
    progress: &mut RewardClaimProgress,
    step: &RewardClaimStepPlan,
    fresh_evidence: &RewardBatchEvidence,
    transaction_hash: B256,
) -> Result<(), GovernancePlanningError> {
    let Some(next) = next_reward_claim_batch_step(progress, fresh_evidence)? else {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    };
    if &next != step {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    }
    progress.confirmed_steps.push(RewardClaimConfirmedStep {
        step: next,
        transaction_hash,
    });
    progress.next_step_index += 1;
    Ok(())
}

pub fn rebuild_reward_claim_batch_continuation(
    progress: &RewardClaimProgress,
    fresh_evidence: &RewardBatchEvidence,
    fresh_steps: &[RewardClaimStep],
) -> Result<RewardClaimProgress, GovernancePlanningError> {
    if progress.confirmed_steps.is_empty() {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    }
    if progress.plan.chain_id != progress.chain_id {
        return Err(GovernancePlanningError::RewardChainChanged);
    }
    let Some(last_confirmed) = progress.confirmed_steps.last() else {
        return Err(GovernancePlanningError::UnconfirmedRewardStep);
    };
    if fresh_evidence.reward_tokens != progress.plan.reward_tokens
        || fresh_evidence.starting_interval <= last_confirmed.step.ending_interval
    {
        return Err(GovernancePlanningError::StaleRewardEvidence);
    }
    let plan = plan_reward_claim_batch(
        progress.chain_id,
        progress.plan.actor,
        progress.plan.recipient,
        true,
        fresh_evidence,
        fresh_steps,
    )?;
    Ok(RewardClaimProgress {
        plan,
        confirmed_steps: progress.confirmed_steps.clone(),
        next_step_index: 0,
        chain_id: progress.chain_id,
    })
}

pub fn validate_reward_token_order(tokens: &[Address]) -> Result<(), GovernancePlanningError> {
    if tokens.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(GovernancePlanningError::InvalidRewardTokenOrder);
    }
    Ok(())
}

#[must_use]
pub fn configured_reward_token(chain_id: u64, token: Address) -> bool {
    governance_contracts(chain_id).is_some_and(|contracts| {
        contracts
            .reward_tokens
            .iter()
            .any(|entry| entry.token == token)
    })
}

#[must_use]
pub fn stake_position_fingerprint(position: &StakePosition) -> B256 {
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(b"railoxide:stake-position:v1");
    append_address(&mut bytes, position.owner);
    append_u256(&mut bytes, position.id);
    append_address(&mut bytes, position.delegate);
    append_u256(&mut bytes, position.amount);
    append_u256(&mut bytes, position.staketime);
    append_u256(&mut bytes, position.locktime);
    append_u256(&mut bytes, position.claimed_time);
    bytes.push(match position.state {
        StakeState::Active => 0,
        StakeState::Unlocking => 1,
        StakeState::Claimable => 2,
        StakeState::Claimed => 3,
    });
    keccak256(bytes)
}

#[must_use]
pub fn reward_evidence_fingerprint(evidence: &RewardEvidence) -> B256 {
    let mut bytes = Vec::with_capacity(256 + evidence.hints.len() * 32);
    bytes.extend_from_slice(b"railoxide:reward-evidence:v1");
    append_address(&mut bytes, evidence.token);
    append_u256(&mut bytes, evidence.starting_interval);
    append_u256(&mut bytes, evidence.ending_interval);
    append_u256(&mut bytes, evidence.amount);
    for value in evidence
        .staking_intervals
        .iter()
        .chain(evidence.hints.iter())
        .chain(evidence.claimed_intervals.iter())
    {
        append_u256(&mut bytes, *value);
    }
    keccak256(bytes)
}

#[must_use]
pub fn reward_batch_evidence_fingerprint(evidence: &RewardBatchEvidence) -> B256 {
    let mut bytes = Vec::with_capacity(256 + evidence.hints.len() * 32);
    bytes.extend_from_slice(b"railoxide:reward-batch-evidence:v1");
    for token in &evidence.reward_tokens {
        append_address(&mut bytes, *token);
    }
    append_u256(&mut bytes, evidence.starting_interval);
    append_u256(&mut bytes, evidence.ending_interval);
    for amount in &evidence.expected_amounts {
        append_u256(&mut bytes, *amount);
    }
    for value in &evidence.staking_intervals {
        append_u256(&mut bytes, *value);
    }
    for value in &evidence.hints {
        append_u256(&mut bytes, *value);
    }
    for intervals in &evidence.claimed_intervals {
        append_u256(&mut bytes, U256::from(intervals.len()));
        for interval in intervals {
            append_u256(&mut bytes, *interval);
        }
    }
    keccak256(bytes)
}

#[must_use]
pub fn reward_plan_fingerprint(chain_id: u64, evidence: &RewardEvidence) -> B256 {
    let mut bytes = Vec::with_capacity(40);
    bytes.extend_from_slice(b"railoxide:reward-plan:v1");
    bytes.extend_from_slice(&chain_id.to_be_bytes());
    bytes.extend_from_slice(reward_evidence_fingerprint(evidence).as_slice());
    keccak256(bytes)
}

#[must_use]
pub fn reward_batch_plan_fingerprint(chain_id: u64, evidence: &RewardBatchEvidence) -> B256 {
    let mut bytes = Vec::with_capacity(40);
    bytes.extend_from_slice(b"railoxide:reward-batch-plan:v1");
    bytes.extend_from_slice(&chain_id.to_be_bytes());
    bytes.extend_from_slice(reward_batch_evidence_fingerprint(evidence).as_slice());
    keccak256(bytes)
}

#[must_use]
pub fn delegation_evidence_fingerprint(evidence: &DelegationEvidence) -> B256 {
    delegation_fingerprint(evidence)
}

#[must_use]
pub fn principal_claim_fingerprint(
    actor: Address,
    position: &StakePosition,
    latest_chain_time: U256,
) -> B256 {
    let mut bytes = Vec::with_capacity(160);
    bytes.extend_from_slice(b"railoxide:principal-claim:v1");
    append_address(&mut bytes, actor);
    append_address(&mut bytes, position.owner);
    append_u256(&mut bytes, position.id);
    append_u256(&mut bytes, position.amount);
    append_u256(&mut bytes, position.locktime);
    append_u256(&mut bytes, position.claimed_time);
    append_u256(&mut bytes, latest_chain_time);
    keccak256(bytes)
}

fn validate_reward_evidence(
    token: Address,
    evidence: &RewardEvidence,
) -> Result<(), GovernancePlanningError> {
    if evidence.token != token {
        return Err(GovernancePlanningError::RewardTokenMismatch);
    }
    if evidence.amount.is_zero() {
        return Err(GovernancePlanningError::ZeroReward);
    }
    if evidence.starting_interval > evidence.ending_interval {
        return Err(GovernancePlanningError::InvalidRewardRange);
    }
    if inclusive_count(evidence.starting_interval, evidence.ending_interval)?
        != evidence.hints.len()
    {
        return Err(GovernancePlanningError::InvalidRewardHints);
    }
    Ok(())
}

fn inclusive_count(start: U256, end: U256) -> Result<usize, GovernancePlanningError> {
    if start > end {
        return Err(GovernancePlanningError::InvalidRewardRange);
    }
    usize::try_from(
        end.checked_sub(start)
            .and_then(|value| value.checked_add(U256::from(1_u8)))
            .ok_or(GovernancePlanningError::InvalidRewardRange)?,
    )
    .map_err(|_| GovernancePlanningError::InvalidRewardRange)
}

fn delegation_fingerprint(evidence: &DelegationEvidence) -> B256 {
    let mut bytes = Vec::with_capacity(192);
    bytes.extend_from_slice(b"railoxide:delegation:v1");
    append_address(&mut bytes, evidence.actor);
    append_address(&mut bytes, evidence.owner);
    append_u256(&mut bytes, evidence.stake_id);
    append_u256(&mut bytes, evidence.amount);
    append_address(&mut bytes, evidence.previous_delegate);
    append_address(&mut bytes, evidence.next_delegate);
    keccak256(bytes)
}

fn append_address(bytes: &mut Vec<u8>, address: Address) {
    bytes.extend_from_slice(address.as_slice());
}
fn append_u256(bytes: &mut Vec<u8>, value: U256) {
    bytes.extend_from_slice(&value.to_be_bytes::<32>());
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn position(delegate: Address, state: StakeState) -> StakePosition {
        StakePosition {
            owner: address!("1111111111111111111111111111111111111111"),
            id: U256::from(7),
            delegate,
            amount: U256::from(42),
            staketime: U256::from(1),
            locktime: U256::ZERO,
            claimed_time: U256::ZERO,
            state,
        }
    }

    #[test]
    fn stake_approval_is_exact_and_requires_fresh_continuation() {
        let staking = address!("2222222222222222222222222222222222222222");
        let actor = address!("1111111111111111111111111111111111111111");
        let sufficient = plan_stake(
            actor,
            staking,
            U256::from(10),
            U256::from(10),
            U256::from(10),
        )
        .unwrap();
        assert!(sufficient.approval.is_none());
        assert_eq!(
            sufficient.stake,
            Some(GovernanceActionIntent::Stake {
                amount: U256::from(10)
            })
        );
        let required = plan_stake(
            actor,
            staking,
            U256::from(10),
            U256::from(2),
            U256::from(10),
        )
        .unwrap();
        assert_eq!(
            required.approval,
            Some(GovernanceActionIntent::GovernanceTokenApproval {
                spender: staking,
                amount: U256::from(10)
            })
        );
        assert!(required.stake.is_none());
        assert!(continue_stake_after_approval(&required, U256::from(10), U256::from(9)).is_err());
        let continued =
            continue_stake_after_approval(&required, U256::from(10), U256::from(10)).unwrap();
        assert!(!continued.requires_approval_confirmation);
        assert_eq!(
            continued.stake,
            Some(GovernanceActionIntent::Stake {
                amount: U256::from(10)
            })
        );
        assert!(plan_stake(actor, staking, U256::MAX, U256::MAX, U256::MAX).is_ok());
        assert!(
            plan_stake(
                actor,
                staking,
                U256::MAX,
                U256::MAX - U256::from(1),
                U256::MAX
            )
            .is_err()
        );
        assert!(plan_stake(actor, staking, U256::MAX, U256::ZERO, U256::ZERO).is_err());
        assert!(plan_stake(actor, staking, U256::from(9), U256::ZERO, U256::from(10)).is_err());
    }

    #[test]
    fn delegation_requires_active_owned_changed_stake() {
        let actor = address!("1111111111111111111111111111111111111111");
        let other = address!("3333333333333333333333333333333333333333");
        let current = address!("4444444444444444444444444444444444444444");
        assert!(plan_delegate(actor, &position(current, StakeState::Active), other).is_ok());
        assert!(plan_delegate(actor, &position(current, StakeState::Active), current).is_err());
        assert!(
            plan_delegate(actor, &position(current, StakeState::Active), Address::ZERO).is_err()
        );
        assert!(plan_delegate(actor, &position(current, StakeState::Unlocking), other).is_err());
        assert!(plan_undelegate(actor, &position(current, StakeState::Active)).is_ok());
        assert!(plan_undelegate(actor, &position(actor, StakeState::Active)).is_err());
    }

    #[test]
    fn delegated_unlock_has_no_unlock_until_fresh_owner_delegation() {
        let actor = address!("1111111111111111111111111111111111111111");
        let external = address!("3333333333333333333333333333333333333333");
        let first = plan_unlock(actor, &position(external, StakeState::Active)).unwrap();
        let UnlockPlan::UndelegateFirst(first) = first else {
            panic!("expected undelegate first")
        };
        assert_eq!(
            first.intent,
            GovernanceActionIntent::Undelegate {
                stake_id: U256::from(7)
            }
        );
        assert!(
            continue_unlock_after_undelegation(&first, &position(external, StakeState::Active))
                .is_err()
        );
        let mut changed_amount = position(actor, StakeState::Active);
        changed_amount.amount = U256::from(43);
        assert!(continue_unlock_after_undelegation(&first, &changed_amount).is_err());
        let fresh = position(actor, StakeState::Active);
        assert!(continue_unlock_after_undelegation(&first, &fresh).is_ok());
        assert!(matches!(
            plan_unlock(actor, &fresh).unwrap(),
            UnlockPlan::Direct(_)
        ));
    }

    #[test]
    fn principal_claim_uses_strict_chain_time_and_owner_recipient() {
        let actor = address!("1111111111111111111111111111111111111111");
        let mut p = position(actor, StakeState::Unlocking);
        p.locktime = U256::from(10);
        assert!(plan_principal_claim(actor, &p, U256::from(10)).is_err());
        assert!(plan_principal_claim(actor, &p, U256::from(11)).is_ok());
        p.claimed_time = U256::from(12);
        assert!(plan_principal_claim(actor, &p, U256::from(13)).is_err());
    }

    #[test]
    fn rewards_require_configured_token_positive_evidence_and_sequential_fresh_steps() {
        let actor = address!("1111111111111111111111111111111111111111");
        let token = governance_contracts(1).unwrap().reward_tokens[0].token;
        let evidence = RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::from(1),
            staking_intervals: vec![U256::ZERO, U256::from(1)],
            hints: vec![U256::ZERO, U256::from(1)],
            claimed_intervals: Vec::new(),
            amount: U256::from(5),
        };
        let reviewed = vec![
            RewardClaimStep {
                reward_tokens: vec![token],
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                subtotal: U256::from(2),
                expected_amounts: vec![U256::from(2)],
                estimated_gas: 1,
            },
            RewardClaimStep {
                reward_tokens: vec![token],
                starting_interval: U256::from(1),
                ending_interval: U256::from(1),
                subtotal: U256::from(3),
                expected_amounts: vec![U256::from(3)],
                estimated_gas: 1,
            },
        ];
        let plan = plan_reward_claim(1, actor, actor, token, &evidence, &reviewed).unwrap();
        let mut progress = RewardClaimProgress::new(plan);
        let first = next_reward_claim_step(&progress, &evidence)
            .unwrap()
            .unwrap();
        let first_hash = B256::from([7_u8; 32]);
        confirm_reward_claim_step(&mut progress, &first, &evidence, first_hash).unwrap();
        assert_eq!(progress.confirmed().len(), 1);
        let second = next_reward_claim_step(&progress, &evidence)
            .unwrap()
            .unwrap();
        assert_eq!(second.starting_interval, U256::from(1));
        assert!(
            next_reward_claim_step(
                &progress,
                &RewardEvidence {
                    amount: U256::from(6),
                    ..evidence.clone()
                }
            )
            .is_err()
        );
        let fresh = RewardEvidence {
            token,
            starting_interval: U256::from(1),
            ending_interval: U256::from(1),
            staking_intervals: vec![U256::from(1)],
            hints: vec![U256::from(1)],
            claimed_intervals: Vec::new(),
            amount: U256::from(3),
        };
        let mut switched = progress.clone();
        switched.plan.chain_id = 56;
        assert_eq!(
            rebuild_reward_claim_continuation(
                &switched,
                &fresh,
                &[RewardClaimStep {
                    reward_tokens: vec![token],
                    starting_interval: U256::from(1),
                    ending_interval: U256::from(1),
                    subtotal: U256::from(3),
                    expected_amounts: vec![U256::from(3)],
                    estimated_gas: 1
                }]
            ),
            Err(GovernancePlanningError::RewardChainChanged)
        );
        let rebuilt = rebuild_reward_claim_continuation(
            &progress,
            &fresh,
            &[RewardClaimStep {
                reward_tokens: vec![token],
                starting_interval: U256::from(1),
                ending_interval: U256::from(1),
                subtotal: U256::from(3),
                expected_amounts: vec![U256::from(3)],
                estimated_gas: 1,
            }],
        )
        .unwrap();
        assert_eq!(rebuilt.confirmed().len(), 1);
        assert_eq!(rebuilt.confirmed()[0].step, first);
        assert_eq!(rebuilt.confirmed()[0].transaction_hash, first_hash);
        assert_eq!(
            next_reward_claim_step(&rebuilt, &fresh)
                .unwrap()
                .unwrap()
                .expected_amount,
            U256::from(3)
        );
        assert!(plan_reward_claim(56, actor, actor, token, &evidence, &reviewed).is_err());
        assert!(
            plan_reward_claim(
                1,
                actor,
                actor,
                token,
                &RewardEvidence {
                    amount: U256::ZERO,
                    ..evidence.clone()
                },
                &reviewed
            )
            .is_err()
        );
        assert!(
            plan_reward_claim(
                1,
                actor,
                actor,
                address!("5555555555555555555555555555555555555555"),
                &evidence,
                &reviewed
            )
            .is_err()
        );
        assert!(validate_reward_token_order(&[token, token]).is_err());
    }

    #[test]
    fn batch_reward_plan_keeps_each_token_amount_separate() {
        let actor = address!("1111111111111111111111111111111111111111");
        let mut tokens = governance_contracts(1)
            .unwrap()
            .reward_tokens
            .iter()
            .take(2)
            .map(|entry| entry.token)
            .collect::<Vec<_>>();
        tokens.sort();
        let evidence = RewardBatchEvidence {
            reward_tokens: tokens.clone(),
            starting_interval: U256::ZERO,
            ending_interval: U256::from(1),
            staking_intervals: vec![U256::ZERO, U256::from(1)],
            hints: vec![U256::ZERO, U256::from(1)],
            claimed_intervals: vec![Vec::new(), Vec::new()],
            expected_amounts: vec![U256::from(5), U256::from(7)],
        };
        let steps = vec![
            RewardClaimStep {
                reward_tokens: tokens.clone(),
                starting_interval: U256::ZERO,
                ending_interval: U256::ZERO,
                subtotal: U256::ZERO,
                expected_amounts: vec![U256::from(2), U256::from(3)],
                estimated_gas: 1,
            },
            RewardClaimStep {
                reward_tokens: tokens.clone(),
                starting_interval: U256::from(1),
                ending_interval: U256::from(1),
                subtotal: U256::ZERO,
                expected_amounts: vec![U256::from(3), U256::from(4)],
                estimated_gas: 1,
            },
        ];
        let plan = plan_reward_claim_batch(1, actor, actor, true, &evidence, &steps).unwrap();
        assert_eq!(plan.reward_tokens, tokens);
        assert_eq!(
            plan.steps[0].expected_amounts,
            vec![U256::from(2), U256::from(3)]
        );
        let GovernanceActionIntent::RewardClaim {
            expected_amounts, ..
        } = &plan.steps[0].intent
        else {
            panic!("batch plan did not produce a reward claim intent");
        };
        assert_eq!(expected_amounts, &vec![U256::from(2), U256::from(3)]);

        let mut progress = RewardClaimProgress::new(plan);
        let first = next_reward_claim_batch_step(&progress, &evidence)
            .unwrap()
            .unwrap();
        let first_hash = B256::from([8_u8; 32]);
        confirm_reward_claim_batch_step(&mut progress, &first, &evidence, first_hash).unwrap();

        let changed_amounts = RewardBatchEvidence {
            expected_amounts: vec![U256::from(6), U256::from(7)],
            ..evidence.clone()
        };
        assert_eq!(
            rebuild_reward_claim_batch_continuation(
                &progress,
                &changed_amounts,
                &[
                    RewardClaimStep {
                        reward_tokens: tokens.clone(),
                        starting_interval: U256::ZERO,
                        ending_interval: U256::ZERO,
                        subtotal: U256::ZERO,
                        expected_amounts: vec![U256::from(3), U256::from(3)],
                        estimated_gas: 1,
                    },
                    RewardClaimStep {
                        reward_tokens: tokens.clone(),
                        starting_interval: U256::from(1),
                        ending_interval: U256::from(1),
                        subtotal: U256::ZERO,
                        expected_amounts: vec![U256::from(3), U256::from(4)],
                        estimated_gas: 1,
                    },
                ],
            ),
            Err(GovernancePlanningError::StaleRewardEvidence)
        );

        let changed_tokens = RewardBatchEvidence {
            reward_tokens: vec![
                tokens[0],
                governance_contracts(1).unwrap().reward_tokens[2].token,
            ],
            ..evidence.clone()
        };
        assert_eq!(
            rebuild_reward_claim_batch_continuation(&progress, &changed_tokens, &steps,),
            Err(GovernancePlanningError::StaleRewardEvidence)
        );

        let next_evidence = RewardBatchEvidence {
            starting_interval: U256::from(1),
            ending_interval: U256::from(1),
            staking_intervals: vec![U256::from(1)],
            hints: vec![U256::from(1)],
            expected_amounts: vec![U256::from(3), U256::from(4)],
            ..evidence
        };
        let rebuilt = rebuild_reward_claim_batch_continuation(
            &progress,
            &next_evidence,
            &[RewardClaimStep {
                reward_tokens: tokens,
                starting_interval: U256::from(1),
                ending_interval: U256::from(1),
                subtotal: U256::ZERO,
                expected_amounts: vec![U256::from(3), U256::from(4)],
                estimated_gas: 1,
            }],
        )
        .unwrap();
        assert_eq!(rebuilt.confirmed().len(), 1);
        assert_eq!(rebuilt.confirmed()[0].transaction_hash, first_hash);
        assert_eq!(rebuilt.plan.steps[0].starting_interval, U256::from(1));
    }
}
