//! `GovernorRewards` read models and checked reward-claim planning.

use std::collections::BTreeMap;

use alloy::consensus::BlockHeader as _;
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use eyre::{Result, eyre};
use railgun_ui::governance_contracts;

use crate::staking::{
    AccountSnapshot, MulticallChunkSize, chunk_indices, fetch_latest_block,
    reward_staking_interval, snapshot_hint,
};
use crate::{ClassRoute, HttpContext, RpcRoute, WalletRpcOrigin};

mod batch_reads;
mod evidence_reads;

pub use batch_reads::{
    fetch_latest_block_gas_limit, fetch_reward_batch_authorization_state,
    fetch_reward_batch_claimed_intervals, fetch_reward_batch_evidence,
    fetch_reward_batch_interval_amounts,
};
pub use evidence_reads::{
    fetch_interval_metadata, fetch_reward_evidence, fetch_reward_evidence_multi,
    fetch_reward_interval_amounts,
};

sol! {
    interface GovernorRewards {
        function staking() external view returns (address);
        function STAKING_DISTRIBUTION_INTERVAL_MULTIPLIER() external view returns (uint256);
        function STAKING_DEPLOY_TIME() external view returns (uint256);
        function DISTRIBUTION_INTERVAL() external view returns (uint256);
        function currentInterval() external view returns (uint256);
        function nextEarmarkInterval(address token) external view returns (uint256);
        function getClaimed(address account, address token, uint256 interval) external view returns (bool);
        function earmarked(address token, uint256 interval) external view returns (uint256);
        function precalculatedGlobalSnapshots(uint256 interval) external view returns (uint256);
        function calculateRewards(address[] tokens, address account, uint256 startingInterval, uint256 endingInterval, uint256[] hints, bool ignoreClaimed) external view returns (uint256[]);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernorRewardsIntervalMetadata {
    pub multiplier: U256,
    pub staking_deploy_time: U256,
    pub distribution_interval: U256,
    pub current_interval: U256,
    pub next_earmark_intervals: BTreeMap<Address, U256>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardEvidence {
    pub token: Address,
    pub starting_interval: U256,
    pub ending_interval: U256,
    pub staking_intervals: Vec<U256>,
    pub hints: Vec<U256>,
    pub claimed_intervals: Vec<U256>,
    pub amount: U256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardEvidenceResult {
    pub token: Address,
    pub evidence: std::result::Result<Option<RewardEvidence>, String>,
}

/// Fresh shared-range evidence for one atomic `GovernorRewards` claim. The amount at each token
/// index belongs to that token, so callers must never add values across indices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardBatchEvidence {
    pub reward_tokens: Vec<Address>,
    pub starting_interval: U256,
    pub ending_interval: U256,
    pub staking_intervals: Vec<U256>,
    pub hints: Vec<U256>,
    pub claimed_intervals: Vec<Vec<U256>>,
    pub expected_amounts: Vec<U256>,
}

impl From<RewardEvidence> for RewardBatchEvidence {
    fn from(evidence: RewardEvidence) -> Self {
        Self {
            reward_tokens: vec![evidence.token],
            starting_interval: evidence.starting_interval,
            ending_interval: evidence.ending_interval,
            staking_intervals: evidence.staking_intervals,
            hints: evidence.hints,
            claimed_intervals: vec![evidence.claimed_intervals],
            expected_amounts: vec![evidence.amount],
        }
    }
}

impl From<&RewardEvidence> for RewardBatchEvidence {
    fn from(evidence: &RewardEvidence) -> Self {
        evidence.clone().into()
    }
}

impl RewardBatchEvidence {
    /// Build the aggregate claim intent from this evidence, retaining verification metadata here.
    #[must_use]
    pub fn to_claim_intent(&self) -> crate::GovernanceActionIntent {
        crate::GovernanceActionIntent::RewardClaim {
            reward_tokens: self.reward_tokens.clone(),
            starting_interval: self.starting_interval,
            ending_interval: self.ending_interval,
            snapshot_hints: self.hints.clone(),
            expected_amounts: self.expected_amounts.clone(),
        }
    }

    /// Narrow evidence read for one token set onto a subset of its tokens. The shared interval
    /// range, staking intervals, and snapshot hints stay exactly as read, because they do not
    /// depend on which tokens the claim call carries; only the per-token columns are selected.
    pub fn project(&self, tokens: &[Address]) -> Result<Self> {
        if tokens.is_empty() || tokens.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(eyre!(
                "reward token addresses must be strictly ascending and unique"
            ));
        }
        if self.claimed_intervals.len() != self.reward_tokens.len()
            || self.expected_amounts.len() != self.reward_tokens.len()
        {
            return Err(eyre!("reward evidence columns do not match its token set"));
        }
        let mut claimed_intervals = Vec::with_capacity(tokens.len());
        let mut expected_amounts = Vec::with_capacity(tokens.len());
        for token in tokens {
            let index = self
                .reward_tokens
                .iter()
                .position(|candidate| candidate == token)
                .ok_or_else(|| eyre!("reward evidence does not cover token {token}"))?;
            claimed_intervals.push(self.claimed_intervals[index].clone());
            expected_amounts.push(self.expected_amounts[index]);
        }
        Ok(Self {
            reward_tokens: tokens.to_vec(),
            starting_interval: self.starting_interval,
            ending_interval: self.ending_interval,
            staking_intervals: self.staking_intervals.clone(),
            hints: self.hints.clone(),
            claimed_intervals,
            expected_amounts,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardBatchAuthorizationState {
    pub claimed_intervals: Vec<Vec<U256>>,
    pub amounts: Vec<U256>,
}

/// Validate the shape of claimed-interval evidence for an exact reward token/range review.
pub fn validate_reward_batch_claimed_intervals(
    evidence: &RewardBatchEvidence,
    claimed_intervals: &[Vec<U256>],
) -> Result<()> {
    if evidence.reward_tokens.is_empty()
        || evidence
            .reward_tokens
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
    {
        return Err(eyre!(
            "reward token addresses must be strictly ascending and unique"
        ));
    }
    if evidence.starting_interval > evidence.ending_interval {
        return Err(eyre!("reward interval range is invalid"));
    }
    if evidence.claimed_intervals.len() != evidence.reward_tokens.len()
        || claimed_intervals.len() != evidence.reward_tokens.len()
    {
        return Err(eyre!("claimed interval vector has wrong token count"));
    }
    for intervals in evidence.claimed_intervals.iter().chain(claimed_intervals) {
        if intervals.windows(2).any(|pair| pair[0] >= pair[1])
            || intervals.iter().any(|&interval| {
                interval < evidence.starting_interval || interval > evidence.ending_interval
            })
        {
            return Err(eyre!(
                "claimed interval vector is outside the reviewed range"
            ));
        }
    }
    Ok(())
}

/// Compare freshly read claimed intervals with the reviewed evidence without rereading any
/// historical metadata, snapshots, or reward calculations.
pub fn reward_batch_claimed_intervals_match(
    evidence: &RewardBatchEvidence,
    claimed_intervals: &[Vec<U256>],
) -> Result<bool> {
    validate_reward_batch_claimed_intervals(evidence, claimed_intervals)?;
    Ok(evidence.claimed_intervals == claimed_intervals)
}

fn reward_batch_interval_count(evidence: &RewardBatchEvidence) -> Result<usize> {
    usize::try_from(
        evidence
            .ending_interval
            .checked_sub(evidence.starting_interval)
            .and_then(|value| value.checked_add(U256::from(1_u8)))
            .ok_or_else(|| eyre!("reward interval range is invalid"))?,
    )
    .map_err(|_| eyre!("reward interval range exceeds platform limits"))
}

fn validate_reward_batch_authorization_evidence(evidence: &RewardBatchEvidence) -> Result<()> {
    validate_reward_batch_claimed_intervals(evidence, &evidence.claimed_intervals)?;
    let count = reward_batch_interval_count(evidence)?;
    if evidence.hints.len() != count || evidence.staking_intervals.len() != count {
        return Err(eyre!("reward evidence vectors do not match interval range"));
    }
    if evidence.expected_amounts.len() != evidence.reward_tokens.len() {
        return Err(eyre!("reward evidence amount vector has wrong token count"));
    }
    Ok(())
}

pub fn validate_reward_batch_authorization_state(
    evidence: &RewardBatchEvidence,
    state: &RewardBatchAuthorizationState,
) -> Result<()> {
    validate_reward_batch_authorization_evidence(evidence)?;
    if state.claimed_intervals.len() != evidence.reward_tokens.len() {
        return Err(eyre!(
            "authorization claimed interval vector has wrong token count"
        ));
    }
    validate_reward_batch_claimed_intervals(evidence, &state.claimed_intervals)?;
    if state.amounts.len() != evidence.reward_tokens.len() {
        return Err(eyre!("authorization amount vector has wrong token count"));
    }
    Ok(())
}

pub fn reward_batch_authorization_state_match(
    evidence: &RewardBatchEvidence,
    state: &RewardBatchAuthorizationState,
) -> Result<bool> {
    validate_reward_batch_authorization_state(evidence, state)?;
    let claimed_matches = evidence.claimed_intervals == state.claimed_intervals;
    let amounts_match = evidence.expected_amounts == state.amounts;
    Ok(claimed_matches && amounts_match)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardIntervalSubtotal {
    pub interval: U256,
    pub subtotal: U256,
    pub gas: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardIntervalAmount {
    pub interval: U256,
    pub subtotal: U256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardClaimStep {
    pub reward_tokens: Vec<Address>,
    pub starting_interval: U256,
    pub ending_interval: U256,
    /// Kept for the single-token planner. Bulk callers use `expected_amounts` exclusively.
    pub subtotal: U256,
    pub expected_amounts: Vec<U256>,
    pub estimated_gas: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardBatchIntervalSubtotal {
    pub interval: U256,
    pub subtotals: Vec<U256>,
    pub gas: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewardBatchIntervalAmount {
    pub interval: U256,
    pub subtotals: Vec<U256>,
}

/// Find the inclusive-range boundary nearest the midpoint which leaves a positive subtotal on
/// both sides. Zero intervals remain attached to whichever neighboring positive range contains
/// them; an all-zero range has no valid split.
#[must_use]
pub fn reward_positive_split_boundary(
    amounts: &[RewardIntervalAmount],
    start: usize,
    end: usize,
) -> Option<usize> {
    if start >= end || end > amounts.len() {
        return None;
    }
    let mut prefix = vec![U256::ZERO; end - start + 1];
    for (index, amount) in amounts[start..end].iter().enumerate() {
        prefix[index + 1] = prefix[index].checked_add(amount.subtotal)?;
    }
    if prefix[end - start].is_zero() {
        return None;
    }
    let total = prefix[end - start];
    let midpoint = (start + end - 1) / 2;
    (start..end - 1)
        .filter(|boundary| {
            let left = prefix[boundary + 1 - start];
            let right = total
                .checked_sub(left)
                .is_some_and(|value| !value.is_zero());
            !left.is_zero() && right
        })
        .min_by_key(|boundary| boundary.abs_diff(midpoint))
}

pub fn decode_claimed_flag(bytes: &[u8]) -> Result<bool> {
    let claimed = GovernorRewards::getClaimedCall::abi_decode_returns_validate(bytes)
        .map_err(|error| eyre!("claimed flag ABI decode failed: {error}"))?;
    Ok(claimed)
}

/// Staking snapshots record voting power before a change. An initial run of zero-power snapshots
/// proves that every staking interval through its final snapshot is ineligible, inclusively.
/// Reward interval `r` samples staking interval `r * multiplier`. Later zero-power snapshots must
/// not skip earlier eligibility. Without an initial zero snapshot, retain interval zero.
fn reward_starting_interval(snapshots: &[AccountSnapshot], multiplier: U256) -> Result<U256> {
    snapshot_hint(snapshots, U256::ZERO)?;
    let Some(last_zero) = snapshots
        .iter()
        .take_while(|snapshot| snapshot.voting_power.is_zero())
        .last()
    else {
        return Ok(U256::ZERO);
    };
    last_zero
        .interval
        .checked_div(multiplier)
        .and_then(|interval| interval.checked_add(U256::ONE))
        .ok_or_else(|| eyre!("reward starting interval conversion is invalid"))
}

/// Select a reviewed reward range. `claimed` must contain one entry for each interval from the
/// snapshot-derived lower bound up to, but excluding, `next_earmark`. Claimed entries are retained
/// as evidence but contribute zero to the displayed contract subtotal.
pub fn reward_evidence(
    token: Address,
    next_earmark: U256,
    claimed: &[(U256, bool)],
    snapshots: &[AccountSnapshot],
    multiplier: U256,
    amount: U256,
) -> Result<Option<RewardEvidence>> {
    let lower_bound = reward_starting_interval(snapshots, multiplier)?;
    if next_earmark <= lower_bound {
        return Ok(None);
    }
    let ending_interval = next_earmark - U256::from(1_u8);
    let expected_count = usize::try_from(next_earmark - lower_bound)
        .map_err(|_| eyre!("reward endpoint exceeds platform limits"))?;
    if claimed.len() != expected_count {
        return Err(eyre!(
            "claimed flags returned {}, expected {}",
            claimed.len(),
            expected_count
        ));
    }
    let mut first_unclaimed = None;
    let mut claimed_intervals = Vec::new();
    for (expected, &(interval, is_claimed)) in claimed.iter().enumerate() {
        let expected = lower_bound + U256::from(expected);
        if interval != expected {
            return Err(eyre!("claimed intervals are not contiguous"));
        }
        if interval > ending_interval {
            return Err(eyre!("claimed interval exceeds endpoint"));
        }
        if !is_claimed && first_unclaimed.is_none() {
            first_unclaimed = Some(interval);
        }
        if is_claimed && first_unclaimed.is_some() {
            claimed_intervals.push(interval);
        }
    }
    let Some(starting_interval) = first_unclaimed else {
        return Ok(None);
    };
    let count = usize::try_from(ending_interval - starting_interval)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| eyre!("reward interval range exceeds platform limits"))?;
    let mut staking_intervals = Vec::with_capacity(count);
    let mut hints = Vec::with_capacity(count);
    for offset in 0..count {
        let interval = starting_interval + U256::from(offset);
        let staking_interval = reward_staking_interval(interval, multiplier)?;
        staking_intervals.push(staking_interval);
        hints.push(snapshot_hint(snapshots, staking_interval)?);
    }
    Ok(Some(RewardEvidence {
        token,
        starting_interval,
        ending_interval,
        staking_intervals,
        hints,
        claimed_intervals,
        amount,
    }))
}

/// Partition reviewed consecutive interval subtotals under a safe gas ceiling. Every interval is
/// represented exactly once and a single interval over the ceiling is rejected.
pub fn plan_reward_claim_steps(
    reviewed: &[RewardIntervalSubtotal],
    gas_ceiling: u64,
) -> Result<Vec<RewardClaimStep>> {
    if gas_ceiling == 0 {
        return Err(eyre!("reward gas ceiling must be positive"));
    }
    if reviewed.is_empty() {
        return Ok(Vec::new());
    }
    for pair in reviewed.windows(2) {
        let expected = pair[0]
            .interval
            .checked_add(U256::from(1_u8))
            .ok_or_else(|| eyre!("reward interval overflows U256"))?;
        if pair[1].interval != expected {
            return Err(eyre!("reward review intervals contain a gap or overlap"));
        }
    }
    let mut steps = Vec::new();
    let mut start = reviewed[0].interval;
    let mut end = start;
    let mut subtotal = U256::ZERO;
    let mut gas = 0_u64;
    for entry in reviewed {
        if entry.gas > gas_ceiling {
            return Err(eyre!("single reward interval exceeds gas ceiling"));
        }
        if gas
            .checked_add(entry.gas)
            .is_none_or(|value| value > gas_ceiling)
        {
            if gas == 0 {
                return Err(eyre!("reward range cannot fit gas ceiling"));
            }
            steps.push(RewardClaimStep {
                reward_tokens: Vec::new(),
                starting_interval: start,
                ending_interval: end,
                subtotal,
                expected_amounts: vec![subtotal],
                estimated_gas: gas,
            });
            start = entry.interval;
            subtotal = U256::ZERO;
            gas = 0;
        }
        subtotal = subtotal
            .checked_add(entry.subtotal)
            .ok_or_else(|| eyre!("reward subtotal overflows U256"))?;
        gas = gas
            .checked_add(entry.gas)
            .ok_or_else(|| eyre!("reward gas total overflows u64"))?;
        end = entry.interval;
    }
    if gas == 0 {
        return Err(eyre!("reward planner produced an empty step"));
    }
    if subtotal.is_zero() {
        return Err(eyre!("reward planner produced a zero-subtotal step"));
    }
    steps.push(RewardClaimStep {
        reward_tokens: Vec::new(),
        starting_interval: start,
        ending_interval: end,
        subtotal,
        expected_amounts: vec![subtotal],
        estimated_gas: gas,
    });
    Ok(steps)
}

/// Partition shared multi-token interval amounts under a gas ceiling. Amount vectors retain token
/// units independently, and every returned step covers a consecutive range exactly once.
pub fn plan_reward_claim_batch_steps(
    reviewed: &[RewardBatchIntervalSubtotal],
    reward_tokens: &[Address],
    gas_ceiling: u64,
) -> Result<Vec<RewardClaimStep>> {
    if reward_tokens.is_empty() || reward_tokens.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(eyre!(
            "reward token addresses must be strictly ascending and unique"
        ));
    }
    if gas_ceiling == 0 {
        return Err(eyre!("reward gas ceiling must be positive"));
    }
    if reviewed.is_empty() {
        return Ok(Vec::new());
    }
    for entry in reviewed {
        if entry.subtotals.len() != reward_tokens.len() {
            return Err(eyre!("reward interval amount vector has wrong length"));
        }
        if entry.gas > gas_ceiling {
            return Err(eyre!("single reward interval exceeds gas ceiling"));
        }
    }
    for pair in reviewed.windows(2) {
        let expected = pair[0]
            .interval
            .checked_add(U256::from(1_u8))
            .ok_or_else(|| eyre!("reward interval overflows U256"))?;
        if pair[1].interval != expected {
            return Err(eyre!("reward review intervals contain a gap or overlap"));
        }
    }
    let mut steps = Vec::new();
    let mut start = reviewed[0].interval;
    let mut end = start;
    let mut amounts = vec![U256::ZERO; reward_tokens.len()];
    let mut gas = 0_u64;
    for entry in reviewed {
        if gas
            .checked_add(entry.gas)
            .is_none_or(|value| value > gas_ceiling)
        {
            if gas == 0 {
                return Err(eyre!("reward range cannot fit gas ceiling"));
            }
            if amounts.iter().all(U256::is_zero) {
                return Err(eyre!("reward planner produced a zero-subtotal step"));
            }
            steps.push(RewardClaimStep {
                reward_tokens: reward_tokens.to_vec(),
                starting_interval: start,
                ending_interval: end,
                subtotal: U256::ZERO,
                expected_amounts: amounts,
                estimated_gas: gas,
            });
            start = entry.interval;
            amounts = vec![U256::ZERO; reward_tokens.len()];
            gas = 0;
        }
        for (total, amount) in amounts.iter_mut().zip(&entry.subtotals) {
            *total = total
                .checked_add(*amount)
                .ok_or_else(|| eyre!("reward subtotal overflows U256"))?;
        }
        gas = gas
            .checked_add(entry.gas)
            .ok_or_else(|| eyre!("reward gas total overflows u64"))?;
        end = entry.interval;
    }
    if gas == 0 || amounts.iter().all(U256::is_zero) {
        return Err(eyre!("reward planner produced an empty or zero step"));
    }
    steps.push(RewardClaimStep {
        reward_tokens: reward_tokens.to_vec(),
        starting_interval: start,
        ending_interval: end,
        subtotal: U256::ZERO,
        expected_amounts: amounts,
        estimated_gas: gas,
    });
    Ok(steps)
}

/// Return a split boundary only when both children retain at least one positive token amount.
#[must_use]
pub fn reward_batch_positive_split_boundary(
    amounts: &[RewardBatchIntervalAmount],
    start: usize,
    end: usize,
) -> Option<usize> {
    if start >= end || end > amounts.len() {
        return None;
    }
    let midpoint = (start + end - 1) / 2;
    (start..end - 1)
        .filter(|boundary| {
            let left_positive = amounts[start..=*boundary]
                .iter()
                .any(|amount| amount.subtotals.iter().any(|value| !value.is_zero()));
            let right_positive = amounts[boundary + 1..end]
                .iter()
                .any(|amount| amount.subtotals.iter().any(|value| !value.is_zero()));
            left_positive && right_positive
        })
        .min_by_key(|boundary| boundary.abs_diff(midpoint))
}

#[cfg(test)]
mod tests;
