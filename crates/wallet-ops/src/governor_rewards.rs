//! `GovernorRewards` read models and checked reward-claim planning.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::consensus::BlockHeader as _;
use alloy::network::BlockResponse as _;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{CallItem, Failure, Provider};
use alloy::rpc::types::BlockNumberOrTag;
use alloy::sol;
use alloy::sol_types::SolCall;
use broadcaster_core::query_rpc_pool::QueryRpcPool;
use eyre::{Result, eyre};
use railgun_ui::governance_contracts;
use sync_service::ChainConfigDefaults;
use tokio::time::timeout;

use crate::settings::EffectiveChainConfig;
use crate::staking::{
    AccountSnapshot, MulticallChunkSize, chunk_indices, reward_staking_interval, snapshot_hint,
};
use crate::{HttpContext, effective_rpc_urls_for_chain, query_rpc_pool_with_http_client};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Select a reviewed reward range. `claimed` must contain one entry for each interval beginning at
/// zero; claimed entries are retained as evidence but contribute zero to the displayed contract
/// subtotal. A zero endpoint has no completed interval and therefore no evidence.
pub fn reward_evidence(
    token: Address,
    next_earmark: U256,
    claimed: &[(U256, bool)],
    snapshots: &[AccountSnapshot],
    multiplier: U256,
    amount: U256,
) -> Result<Option<RewardEvidence>> {
    if next_earmark.is_zero() {
        return Ok(None);
    }
    let ending_interval = next_earmark - U256::from(1_u8);
    let expected_count = usize::try_from(next_earmark)
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
        let expected = U256::from(expected);
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

/// Read interval metadata and the configured token endpoints through the configured RPC pool.
pub async fn fetch_interval_metadata(
    chain_id: u64,
    tokens: &[Address],
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<Option<GovernorRewardsIntervalMetadata>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(None);
    };
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut calls = vec![
        GovernorRewards::STAKING_DISTRIBUTION_INTERVAL_MULTIPLIERCall {}
            .abi_encode()
            .into(),
        GovernorRewards::STAKING_DEPLOY_TIMECall {}
            .abi_encode()
            .into(),
        GovernorRewards::DISTRIBUTION_INTERVALCall {}
            .abi_encode()
            .into(),
        GovernorRewards::currentIntervalCall {}.abi_encode().into(),
    ];
    calls.extend(tokens.iter().map(|&token| {
        GovernorRewards::nextEarmarkIntervalCall { token }
            .abi_encode()
            .into()
    }));
    let values = multicall_values::<GovernorRewards::STAKING_DISTRIBUTION_INTERVAL_MULTIPLIERCall>(
        &pool,
        multicall_address,
        contracts.governor_rewards,
        calls,
    )
    .await?;
    if values.len() != 4 + tokens.len() {
        return Err(eyre!(
            "GovernorRewards metadata returned wrong result count"
        ));
    }
    let mut values = values.into_iter();
    let multiplier = values
        .next()
        .ok_or_else(|| eyre!("multiplier result is missing"))?
        .map_err(|error| eyre!("multiplier call failed: {error:?}"))?;
    let staking_deploy_time = values
        .next()
        .ok_or_else(|| eyre!("staking deploy time result is missing"))?
        .map_err(|error| eyre!("staking deploy time call failed: {error:?}"))?;
    let distribution_interval = values
        .next()
        .ok_or_else(|| eyre!("distribution interval result is missing"))?
        .map_err(|error| eyre!("distribution interval call failed: {error:?}"))?;
    let current_interval = values
        .next()
        .ok_or_else(|| eyre!("current interval result is missing"))?
        .map_err(|error| eyre!("current interval call failed: {error:?}"))?;
    let mut next_earmark_intervals = BTreeMap::new();
    for token in tokens {
        let value = values
            .next()
            .ok_or_else(|| eyre!("next earmark result is missing"))?;
        next_earmark_intervals.insert(
            *token,
            value.map_err(|error| eyre!("next earmark interval call failed: {error:?}"))?,
        );
    }
    Ok(Some(GovernorRewardsIntervalMetadata {
        multiplier,
        staking_deploy_time,
        distribution_interval,
        current_interval,
        next_earmark_intervals,
    }))
}

/// Fetch claimed flags and validate the exact calculator result for one account/token pair.
pub async fn fetch_reward_evidence(
    chain_id: u64,
    account: Address,
    token: Address,
    metadata: &GovernorRewardsIntervalMetadata,
    snapshots: &[AccountSnapshot],
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Option<RewardEvidence>> {
    let mut results = fetch_reward_evidence_multi(
        chain_id,
        account,
        &[token],
        metadata,
        snapshots,
        effective_chain,
        http,
        chunk_size,
    )
    .await?;
    results
        .pop()
        .ok_or_else(|| eyre!("reward evidence result is missing"))?
        .evidence
        .map_err(|error| eyre!("{error}"))
}

/// Fetch claimed state for all tokens, then batch the independent reward calculations.
pub async fn fetch_reward_evidence_multi(
    chain_id: u64,
    account: Address,
    tokens: &[Address],
    metadata: &GovernorRewardsIntervalMetadata,
    snapshots: &[AccountSnapshot],
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<RewardEvidenceResult>> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(tokens
            .iter()
            .map(|&token| RewardEvidenceResult {
                token,
                evidence: Ok(None),
            })
            .collect());
    };
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut claimed: Vec<Option<Vec<Option<bool>>>> = Vec::with_capacity(tokens.len());
    let mut errors: Vec<Option<String>> = vec![None; tokens.len()];
    let mut calls = Vec::new();
    for (token_index, &token) in tokens.iter().enumerate() {
        let Some(&next) = metadata.next_earmark_intervals.get(&token) else {
            errors[token_index] = Some("token has no earmark endpoint".into());
            claimed.push(None);
            continue;
        };
        let Ok(count) = usize::try_from(next) else {
            errors[token_index] = Some("reward interval count exceeds platform limits".into());
            claimed.push(None);
            continue;
        };
        claimed.push(Some(vec![None; count]));
        for interval in 0..count {
            calls.push((token_index, token, interval));
        }
    }
    for chunk in calls.chunks(chunk_size.get()) {
        let encoded = chunk
            .iter()
            .map(|&(_, token, interval)| {
                GovernorRewards::getClaimedCall {
                    account,
                    token,
                    interval: U256::from(interval),
                }
                .abi_encode()
                .into()
            })
            .collect();
        match multicall_values::<GovernorRewards::getClaimedCall>(
            &pool,
            multicall_address,
            contracts.governor_rewards,
            encoded,
        )
        .await
        {
            Ok(values) if values.len() == chunk.len() => {
                for (&(token_index, _, interval), value) in chunk.iter().zip(values) {
                    match value {
                        Ok(flag) => {
                            match claimed
                                .get_mut(token_index)
                                .and_then(Option::as_mut)
                                .and_then(|flags| flags.get_mut(interval))
                            {
                                Some(slot) => *slot = Some(flag),
                                None if errors[token_index].is_none() => {
                                    errors[token_index] =
                                        Some("claimed flag result mapping is missing".into());
                                }
                                None => {}
                            }
                        }
                        Err(error) if errors[token_index].is_none() => {
                            errors[token_index] =
                                Some(format!("claimed flag call failed: {error:?}"));
                        }
                        Err(_) => {}
                    }
                }
            }
            Ok(values) => {
                for &(token_index, _, _) in chunk {
                    if errors[token_index].is_none() {
                        errors[token_index] = Some(format!(
                            "claimed flags returned {}, expected {}",
                            values.len(),
                            chunk.len()
                        ));
                    }
                }
            }
            Err(error) => {
                for &(token_index, _, _) in chunk {
                    if errors[token_index].is_none() {
                        errors[token_index] = Some(error.to_string());
                    }
                }
            }
        }
    }

    let mut evidence = vec![None; tokens.len()];
    let mut calculator_calls = Vec::new();
    for (token_index, &token) in tokens.iter().enumerate() {
        if errors[token_index].is_some() {
            continue;
        }
        let Some(&next) = metadata.next_earmark_intervals.get(&token) else {
            continue;
        };
        if next.is_zero() {
            errors[token_index] = Some("token has no completed earmark interval".into());
            continue;
        }
        let Some(flags) = claimed[token_index].as_ref() else {
            continue;
        };
        let Some(flags) = flags
            .iter()
            .enumerate()
            .map(|(i, flag)| flag.map(|flag| (U256::from(i), flag)))
            .collect::<Option<Vec<_>>>()
        else {
            errors[token_index] = Some("claimed flag result is missing".into());
            continue;
        };
        match reward_evidence(
            token,
            next,
            &flags,
            snapshots,
            metadata.multiplier,
            U256::ZERO,
        ) {
            Ok(Some(value)) => {
                calculator_calls.push((token_index, value));
            }
            Ok(None) => {}
            Err(error) => errors[token_index] = Some(error.to_string()),
        }
    }
    if !calculator_calls.is_empty() {
        let encoded = calculator_calls
            .iter()
            .map(|(_, evidence)| {
                GovernorRewards::calculateRewardsCall {
                    tokens: vec![evidence.token],
                    account,
                    startingInterval: evidence.starting_interval,
                    endingInterval: evidence.ending_interval,
                    hints: evidence.hints.clone(),
                    ignoreClaimed: true,
                }
                .abi_encode()
                .into()
            })
            .collect();
        let values = multicall_values::<GovernorRewards::calculateRewardsCall>(
            &pool,
            multicall_address,
            contracts.governor_rewards,
            encoded,
        )
        .await?;
        if values.len() != calculator_calls.len() {
            return Err(eyre!("reward calculator returned wrong result count"));
        }
        for ((token_index, mut value), result) in calculator_calls.into_iter().zip(values) {
            match result {
                Ok(amounts) if amounts.len() == 1 => {
                    value.amount = amounts[0];
                    evidence[token_index] = Some(value);
                }
                Ok(amounts) => {
                    errors[token_index] = Some(format!(
                        "reward calculator returned {} amounts, expected one",
                        amounts.len()
                    ));
                }
                Err(error) => {
                    errors[token_index] = Some(format!("reward calculator call failed: {error:?}"));
                }
            }
        }
    }
    Ok(tokens
        .iter()
        .enumerate()
        .map(|(index, &token)| RewardEvidenceResult {
            token,
            evidence: errors[index]
                .clone()
                .map_or_else(|| Ok(evidence[index].clone()), Err),
        })
        .collect())
}

/// Build one shared-range claim review. Every configured token participates in the calculator
/// call, including tokens whose amount is zero. A token with no completed endpoint fails closed
/// because omitting it would change the reviewed atomic claim.
pub async fn fetch_reward_batch_evidence(
    chain_id: u64,
    account: Address,
    tokens: &[Address],
    metadata: &GovernorRewardsIntervalMetadata,
    snapshots: &[AccountSnapshot],
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Option<RewardBatchEvidence>> {
    if tokens.is_empty() {
        return Err(eyre!("reward claim all requires at least one token"));
    }
    if tokens.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(eyre!(
            "reward token addresses must be strictly ascending and unique"
        ));
    }
    let Some(contracts) = governance_contracts(chain_id) else {
        return Err(eyre!("reward contracts are unavailable"));
    };
    let mut endpoints = Vec::with_capacity(tokens.len());
    for &token in tokens {
        let next = *metadata
            .next_earmark_intervals
            .get(&token)
            .ok_or_else(|| eyre!("token has no earmark endpoint"))?;
        if next.is_zero() {
            return Err(eyre!("token has no completed earmark interval"));
        }
        endpoints.push(next);
    }
    let ending_interval = endpoints
        .iter()
        .copied()
        .min()
        .and_then(|next| next.checked_sub(U256::from(1_u8)))
        .ok_or_else(|| eyre!("reward endpoint is invalid"))?;
    let count = usize::try_from(ending_interval)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| eyre!("reward interval range exceeds platform limits"))?;
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut claimed = vec![vec![None; count]; tokens.len()];
    let calls = tokens
        .iter()
        .enumerate()
        .flat_map(|(token_index, &token)| {
            (0..count).map(move |interval| (token_index, token, interval))
        })
        .collect::<Vec<_>>();
    for chunk in calls.chunks(chunk_size.get()) {
        let encoded = chunk
            .iter()
            .map(|&(_, token, interval)| {
                GovernorRewards::getClaimedCall {
                    account,
                    token,
                    interval: U256::from(interval),
                }
                .abi_encode()
                .into()
            })
            .collect();
        let values = multicall_values::<GovernorRewards::getClaimedCall>(
            &pool,
            multicall_address,
            contracts.governor_rewards,
            encoded,
        )
        .await?;
        if values.len() != chunk.len() {
            return Err(eyre!("claimed flags returned wrong result count"));
        }
        for (&(token_index, _, interval), value) in chunk.iter().zip(values) {
            claimed[token_index][interval] =
                Some(value.map_err(|error| eyre!("claimed flag call failed: {error:?}"))?);
        }
    }
    let claimed = claimed
        .into_iter()
        .map(|flags| {
            flags
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| eyre!("claimed flag result is missing"))
        })
        .collect::<Result<Vec<_>>>()?;
    let starting_interval = claimed
        .iter()
        .filter_map(|flags| {
            flags
                .iter()
                .enumerate()
                .find_map(|(index, &is_claimed)| (!is_claimed).then_some(U256::from(index)))
        })
        .min();
    let Some(starting_interval) = starting_interval else {
        return Ok(None);
    };
    let interval_count = usize::try_from(
        ending_interval
            .checked_sub(starting_interval)
            .and_then(|value| value.checked_add(U256::from(1_u8)))
            .ok_or_else(|| eyre!("reward interval range is invalid"))?,
    )
    .map_err(|_| eyre!("reward interval range exceeds platform limits"))?;
    let staking_intervals = (0..interval_count)
        .map(|offset| {
            reward_staking_interval(starting_interval + U256::from(offset), metadata.multiplier)
        })
        .collect::<Result<Vec<_>>>()?;
    let hints = staking_intervals
        .iter()
        .map(|&interval| snapshot_hint(snapshots, interval))
        .collect::<Result<Vec<_>>>()?;
    let values = multicall_values::<GovernorRewards::calculateRewardsCall>(
        &pool,
        multicall_address,
        contracts.governor_rewards,
        vec![
            GovernorRewards::calculateRewardsCall {
                tokens: tokens.to_vec(),
                account,
                startingInterval: starting_interval,
                endingInterval: ending_interval,
                hints: hints.clone(),
                ignoreClaimed: true,
            }
            .abi_encode()
            .into(),
        ],
    )
    .await?;
    let Some(result) = values.into_iter().next() else {
        return Err(eyre!("reward calculator returned no result"));
    };
    let expected_amounts =
        result.map_err(|error| eyre!("reward calculator call failed: {error:?}"))?;
    if expected_amounts.len() != tokens.len() {
        return Err(eyre!(
            "reward calculator returned {}, expected {} amounts",
            expected_amounts.len(),
            tokens.len()
        ));
    }
    if expected_amounts.iter().all(U256::is_zero) {
        return Ok(None);
    }
    let claimed_intervals = claimed
        .iter()
        .map(|flags| {
            flags
                .iter()
                .enumerate()
                .filter_map(|(index, &is_claimed)| {
                    (is_claimed && U256::from(index) >= starting_interval)
                        .then_some(U256::from(index))
                })
                .collect()
        })
        .collect();
    Ok(Some(RewardBatchEvidence {
        reward_tokens: tokens.to_vec(),
        starting_interval,
        ending_interval,
        staking_intervals,
        hints,
        claimed_intervals,
        expected_amounts,
    }))
}

/// Fetch only claimed flags for the exact token set and inclusive interval range in reviewed
/// evidence. This is intentionally narrower than a full reward refresh for authorization checks.
pub async fn fetch_reward_batch_claimed_intervals(
    chain_id: u64,
    account: Address,
    evidence: &RewardBatchEvidence,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<Vec<U256>>> {
    validate_reward_batch_claimed_intervals(evidence, &evidence.claimed_intervals)?;
    let Some(contracts) = governance_contracts(chain_id) else {
        return Err(eyre!("reward contracts are unavailable"));
    };
    let count = usize::try_from(
        evidence
            .ending_interval
            .checked_sub(evidence.starting_interval)
            .and_then(|value| value.checked_add(U256::from(1_u8)))
            .ok_or_else(|| eyre!("reward interval range is invalid"))?,
    )
    .map_err(|_| eyre!("reward interval range exceeds platform limits"))?;
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut claimed = vec![vec![None; count]; evidence.reward_tokens.len()];
    let calls = evidence
        .reward_tokens
        .iter()
        .enumerate()
        .flat_map(|(token_index, &token)| {
            (0..count).map(move |offset| (token_index, token, offset))
        })
        .collect::<Vec<_>>();
    for chunk in calls.chunks(chunk_size.get()) {
        let encoded = chunk
            .iter()
            .map(|&(_, token, offset)| {
                GovernorRewards::getClaimedCall {
                    account,
                    token,
                    interval: evidence.starting_interval + U256::from(offset),
                }
                .abi_encode()
                .into()
            })
            .collect();
        let values = multicall_values::<GovernorRewards::getClaimedCall>(
            &pool,
            multicall_address,
            contracts.governor_rewards,
            encoded,
        )
        .await?;
        if values.len() != chunk.len() {
            return Err(eyre!("claimed flags returned wrong result count"));
        }
        for (&(token_index, _, offset), value) in chunk.iter().zip(values) {
            claimed[token_index][offset] =
                Some(value.map_err(|error| eyre!("claimed flag call failed: {error:?}"))?);
        }
    }
    let claimed = claimed
        .into_iter()
        .map(|flags| {
            flags
                .into_iter()
                .enumerate()
                .map(|(offset, value)| {
                    value
                        .ok_or_else(|| eyre!("claimed flag result is missing"))
                        .map(|is_claimed| (offset, is_claimed))
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|flags| {
            flags
                .into_iter()
                .filter_map(|(offset, is_claimed)| {
                    is_claimed.then_some(evidence.starting_interval + U256::from(offset))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    validate_reward_batch_claimed_intervals(evidence, &claimed)?;
    Ok(claimed)
}

/// Fetch the exact claimed flags and calculator amounts used to authorize a reviewed batch.
/// Calculator values intentionally ignore claimed state so a delegated stake owner cannot mutate
/// the snapshot fallback and retain authorization with an unchanged claimed bitmap.
pub async fn fetch_reward_batch_authorization_state(
    chain_id: u64,
    account: Address,
    evidence: &RewardBatchEvidence,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<RewardBatchAuthorizationState> {
    validate_reward_batch_authorization_evidence(evidence)?;
    let claimed_intervals = fetch_reward_batch_claimed_intervals(
        chain_id,
        account,
        evidence,
        effective_chain,
        http,
        chunk_size,
    )
    .await?;
    let Some(contracts) = governance_contracts(chain_id) else {
        return Err(eyre!("reward contracts are unavailable"));
    };
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let values = multicall_values::<GovernorRewards::calculateRewardsCall>(
        &pool,
        multicall_address,
        contracts.governor_rewards,
        vec![
            GovernorRewards::calculateRewardsCall {
                tokens: evidence.reward_tokens.clone(),
                account,
                startingInterval: evidence.starting_interval,
                endingInterval: evidence.ending_interval,
                hints: evidence.hints.clone(),
                ignoreClaimed: true,
            }
            .abi_encode()
            .into(),
        ],
    )
    .await?;
    if values.len() != 1 {
        return Err(eyre!("reward calculator returned wrong result count"));
    }
    let Some(result) = values.into_iter().next() else {
        return Err(eyre!("reward calculator returned no result"));
    };
    let amounts = result.map_err(|error| eyre!("reward calculator call failed: {error:?}"))?;
    if amounts.len() != evidence.reward_tokens.len() {
        return Err(eyre!(
            "reward calculator returned {}, expected {} amounts",
            amounts.len(),
            evidence.reward_tokens.len()
        ));
    }
    let state = RewardBatchAuthorizationState {
        claimed_intervals,
        amounts,
    };
    validate_reward_batch_authorization_state(evidence, &state)?;
    Ok(state)
}

/// Fetch the latest block gas limit from the configured query pool.
pub async fn fetch_latest_block_gas_limit(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<u64> {
    let (pool, _) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut last_error = None;
    for _ in 0..pool.len() {
        let Some(handle) = pool.random_provider() else {
            break;
        };
        match timeout(
            RPC_TIMEOUT,
            handle
                .provider
                .get_block_by_number(BlockNumberOrTag::Latest),
        )
        .await
        {
            Ok(Ok(Some(block))) if block.header().gas_limit() > 0 => {
                return Ok(block.header().gas_limit());
            }
            Ok(Ok(Some(_))) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("latest block gas limit was zero"));
            }
            Ok(Ok(None)) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("latest block was unavailable"));
            }
            Ok(Err(_)) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("latest block RPC request failed"));
            }
            Err(_) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("latest block RPC request timed out"));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no healthy query RPC available")))
}

/// Calculate the exact reward subtotal for every interval in the evidence.
pub async fn fetch_reward_interval_amounts(
    chain_id: u64,
    account: Address,
    evidence: &RewardEvidence,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<RewardIntervalAmount>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Err(eyre!("reward contracts are unavailable"));
    };
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let count = usize::try_from(evidence.ending_interval - evidence.starting_interval)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| eyre!("reward interval range exceeds platform limits"))?;
    if evidence.hints.len() != count {
        return Err(eyre!("reward evidence hints do not match interval range"));
    }
    let mut amounts = Vec::with_capacity(count);
    for chunk in chunk_indices(count, chunk_size) {
        let calls = chunk
            .iter()
            .map(|&offset| {
                GovernorRewards::calculateRewardsCall {
                    tokens: vec![evidence.token],
                    account,
                    startingInterval: evidence.starting_interval + U256::from(offset),
                    endingInterval: evidence.starting_interval + U256::from(offset),
                    hints: vec![evidence.hints[offset]],
                    ignoreClaimed: true,
                }
                .abi_encode()
                .into()
            })
            .collect();
        let values = multicall_values::<GovernorRewards::calculateRewardsCall>(
            &pool,
            multicall_address,
            contracts.governor_rewards,
            calls,
        )
        .await?;
        if values.len() != chunk.len() {
            return Err(eyre!("reward calculator returned wrong result count"));
        }
        for (offset, value) in chunk.iter().zip(values) {
            let result =
                value.map_err(|error| eyre!("reward calculator call failed: {error:?}"))?;
            if result.len() != 1 {
                return Err(eyre!("reward calculator returned one amount per interval"));
            }
            amounts.push(RewardIntervalAmount {
                interval: evidence.starting_interval + U256::from(*offset),
                subtotal: result[0],
            });
        }
    }
    let total = amounts.iter().try_fold(U256::ZERO, |total, amount| {
        total
            .checked_add(amount.subtotal)
            .ok_or_else(|| eyre!("reward subtotal overflows U256"))
    })?;
    if total != evidence.amount {
        return Err(eyre!(
            "per-interval reward subtotals do not match evidence amount"
        ));
    }
    Ok(amounts)
}

/// Calculate one exact per-interval amount vector for a shared multi-token range.
pub async fn fetch_reward_batch_interval_amounts(
    chain_id: u64,
    account: Address,
    evidence: &RewardBatchEvidence,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<RewardBatchIntervalAmount>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Err(eyre!("reward contracts are unavailable"));
    };
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
    let count = usize::try_from(
        evidence
            .ending_interval
            .checked_sub(evidence.starting_interval)
            .and_then(|value| value.checked_add(U256::from(1_u8)))
            .ok_or_else(|| eyre!("reward interval range is invalid"))?,
    )
    .map_err(|_| eyre!("reward interval range exceeds platform limits"))?;
    if evidence.hints.len() != count {
        return Err(eyre!("reward evidence hints do not match interval range"));
    }
    let (pool, multicall_address) = provider_for_chain(chain_id, effective_chain, http)?;
    let mut amounts = Vec::with_capacity(count);
    for chunk in chunk_indices(count, chunk_size) {
        let calls = chunk
            .iter()
            .map(|&offset| {
                GovernorRewards::calculateRewardsCall {
                    tokens: evidence.reward_tokens.clone(),
                    account,
                    startingInterval: evidence.starting_interval + U256::from(offset),
                    endingInterval: evidence.starting_interval + U256::from(offset),
                    hints: vec![evidence.hints[offset]],
                    ignoreClaimed: true,
                }
                .abi_encode()
                .into()
            })
            .collect();
        let values = multicall_values::<GovernorRewards::calculateRewardsCall>(
            &pool,
            multicall_address,
            contracts.governor_rewards,
            calls,
        )
        .await?;
        if values.len() != chunk.len() {
            return Err(eyre!("reward calculator returned wrong result count"));
        }
        for (offset, value) in chunk.iter().zip(values) {
            let result =
                value.map_err(|error| eyre!("reward calculator call failed: {error:?}"))?;
            if result.len() != evidence.reward_tokens.len() {
                return Err(eyre!("reward calculator returned one amount per token"));
            }
            amounts.push(RewardBatchIntervalAmount {
                interval: evidence.starting_interval + U256::from(*offset),
                subtotals: result,
            });
        }
    }
    Ok(amounts)
}

async fn multicall_values<C: SolCall + 'static>(
    pool: &QueryRpcPool,
    multicall_address: Address,
    target: Address,
    calls: Vec<Bytes>,
) -> Result<Vec<std::result::Result<C::Return, Failure>>> {
    let mut last_error = None;
    for _ in 0..pool.len() {
        let Some(handle) = pool.random_provider() else {
            break;
        };
        let mut multicall = handle
            .provider
            .multicall()
            .dynamic::<C>()
            .address(multicall_address);
        for call in calls.iter().cloned() {
            multicall = multicall.add_call_dynamic(CallItem::new(target, call));
        }
        match timeout(RPC_TIMEOUT, multicall.try_aggregate(false)).await {
            Ok(Ok(values)) => return Ok(values),
            Ok(Err(error)) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("multicall failed: {error}"));
            }
            Err(_) => {
                pool.mark_bad_provider(&handle);
                last_error = Some(eyre!("multicall timed out"));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| eyre!("no healthy query RPC available")))
}

fn provider_for_chain(
    chain_id: u64,
    effective_chain: Option<&EffectiveChainConfig>,
    http: &HttpContext,
) -> Result<(Arc<QueryRpcPool>, Address)> {
    let defaults = ChainConfigDefaults::for_chain(chain_id)
        .ok_or_else(|| eyre!("unsupported chain id {chain_id}"))?;
    let rpc_urls = effective_rpc_urls_for_chain(&defaults, effective_chain)?;
    let multicall_address = effective_chain
        .map(|chain| Address::from_str(&chain.multicall_contract))
        .transpose()?
        .unwrap_or(defaults.multicall_contract);
    Ok((
        query_rpc_pool_with_http_client(rpc_urls, http),
        multicall_address,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_excludes_claimed_and_unearmarked_ranges() {
        let snapshots = vec![
            AccountSnapshot {
                interval: U256::ZERO,
                voting_power: U256::ZERO,
            },
            AccountSnapshot {
                interval: U256::from(4),
                voting_power: U256::ZERO,
            },
        ];
        let evidence = reward_evidence(
            Address::ZERO,
            U256::from(3),
            &[
                (U256::ZERO, false),
                (U256::from(1), true),
                (U256::from(2), false),
            ],
            &snapshots,
            U256::from(2),
            U256::ZERO,
        )
        .unwrap()
        .unwrap();
        assert_eq!(evidence.starting_interval, U256::ZERO);
        assert_eq!(evidence.ending_interval, U256::from(2));
        assert_eq!(evidence.claimed_intervals, vec![U256::from(1)]);
        assert_eq!(
            evidence.staking_intervals,
            vec![U256::ZERO, U256::from(2), U256::from(4)]
        );
        assert!(
            reward_evidence(
                Address::ZERO,
                U256::ZERO,
                &[],
                &snapshots,
                U256::from(2),
                U256::ZERO
            )
            .unwrap()
            .is_none()
        );
        assert!(
            reward_evidence(
                Address::ZERO,
                U256::from(2),
                &[(U256::ZERO, true), (U256::from(1), true)],
                &snapshots,
                U256::from(2),
                U256::ZERO
            )
            .unwrap()
            .is_none()
        );
        assert!(
            reward_evidence(
                Address::ZERO,
                U256::from(2),
                &[(U256::ZERO, false), (U256::from(2), false)],
                &snapshots,
                U256::from(2),
                U256::ZERO
            )
            .is_err()
        );
    }

    #[test]
    fn claimed_flag_decode_is_strict() {
        let valid = GovernorRewards::getClaimedCall::abi_encode_returns(&true);
        assert!(decode_claimed_flag(&valid).unwrap());
        assert!(decode_claimed_flag(&valid[..31]).is_err());
    }

    #[test]
    fn claimed_interval_comparison_rejects_mismatch_and_malformed_shape() {
        let token = Address::from([1; 20]);
        let evidence = RewardBatchEvidence {
            reward_tokens: vec![token],
            starting_interval: U256::from(2),
            ending_interval: U256::from(3),
            staking_intervals: vec![U256::ZERO; 2],
            hints: vec![U256::ZERO; 2],
            claimed_intervals: vec![vec![U256::from(3)]],
            expected_amounts: vec![U256::ONE],
        };
        assert!(reward_batch_claimed_intervals_match(&evidence, &[vec![U256::from(3)]]).unwrap());
        assert!(!reward_batch_claimed_intervals_match(&evidence, &[vec![U256::from(2)]]).unwrap());
        assert!(reward_batch_claimed_intervals_match(&evidence, &[]).is_err());
        assert!(reward_batch_claimed_intervals_match(&evidence, &[vec![U256::from(4)]]).is_err());
    }

    #[test]
    fn authorization_comparison_requires_claimed_and_calculated_amounts() {
        let token = Address::from([1; 20]);
        let evidence = RewardBatchEvidence {
            reward_tokens: vec![token],
            starting_interval: U256::from(2),
            ending_interval: U256::from(3),
            staking_intervals: vec![U256::ZERO; 2],
            hints: vec![U256::ZERO; 2],
            claimed_intervals: vec![vec![U256::from(3)]],
            expected_amounts: vec![U256::ONE],
        };
        let unchanged = RewardBatchAuthorizationState {
            claimed_intervals: vec![vec![U256::from(3)]],
            amounts: vec![U256::ONE],
        };
        assert!(reward_batch_authorization_state_match(&evidence, &unchanged).unwrap());
        assert!(
            !reward_batch_authorization_state_match(
                &evidence,
                &RewardBatchAuthorizationState {
                    claimed_intervals: vec![vec![U256::from(2)]],
                    amounts: vec![U256::ONE],
                }
            )
            .unwrap()
        );
        assert!(
            !reward_batch_authorization_state_match(
                &evidence,
                &RewardBatchAuthorizationState {
                    claimed_intervals: vec![vec![U256::from(3)]],
                    amounts: vec![U256::from(2)],
                }
            )
            .unwrap()
        );
        assert!(
            reward_batch_authorization_state_match(
                &evidence,
                &RewardBatchAuthorizationState {
                    claimed_intervals: vec![vec![U256::from(3)]],
                    amounts: Vec::new(),
                }
            )
            .is_err()
        );
        assert!(
            reward_batch_authorization_state_match(
                &RewardBatchEvidence {
                    expected_amounts: Vec::new(),
                    ..evidence
                },
                &unchanged
            )
            .is_err()
        );
    }

    #[test]
    fn planner_covers_consecutive_ranges_without_empty_steps() {
        let entries = vec![
            RewardIntervalSubtotal {
                interval: U256::ZERO,
                subtotal: U256::from(2),
                gas: 5,
            },
            RewardIntervalSubtotal {
                interval: U256::from(1),
                subtotal: U256::from(3),
                gas: 5,
            },
            RewardIntervalSubtotal {
                interval: U256::from(2),
                subtotal: U256::from(4),
                gas: 5,
            },
        ];
        let steps = plan_reward_claim_steps(&entries, 10).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(
            (
                steps[0].starting_interval,
                steps[0].ending_interval,
                steps[0].subtotal
            ),
            (U256::ZERO, U256::from(1), U256::from(5))
        );
        assert_eq!(
            (
                steps[1].starting_interval,
                steps[1].ending_interval,
                steps[1].subtotal
            ),
            (U256::from(2), U256::from(2), U256::from(4))
        );
        assert!(plan_reward_claim_steps(&entries[..2], 0).is_err());
        let gap = [
            entries[0].clone(),
            RewardIntervalSubtotal {
                interval: U256::from(2),
                ..entries[1].clone()
            },
        ];
        assert!(plan_reward_claim_steps(&gap, 10).is_err());
        let over_ceiling = [RewardIntervalSubtotal {
            interval: U256::ZERO,
            subtotal: U256::ZERO,
            gas: 11,
        }];
        assert!(plan_reward_claim_steps(&over_ceiling, 10).is_err());
        let overflow = [
            RewardIntervalSubtotal {
                interval: U256::ZERO,
                subtotal: U256::MAX,
                gas: 1,
            },
            RewardIntervalSubtotal {
                interval: U256::from(1),
                subtotal: U256::from(1),
                gas: 1,
            },
        ];
        assert!(plan_reward_claim_steps(&overflow, 10).is_err());
    }

    #[test]
    fn positive_split_keeps_zero_runs_with_positive_children() {
        let amounts = |values: &[u64]| {
            values
                .iter()
                .enumerate()
                .map(|(index, &subtotal)| RewardIntervalAmount {
                    interval: U256::from(index),
                    subtotal: U256::from(subtotal),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            reward_positive_split_boundary(&amounts(&[0, 4]), 0, 2),
            None
        );
        assert_eq!(
            reward_positive_split_boundary(&amounts(&[4, 0]), 0, 2),
            None
        );
        assert_eq!(
            reward_positive_split_boundary(&amounts(&[4, 0, 4]), 0, 3),
            Some(1)
        );
        assert_eq!(
            reward_positive_split_boundary(&amounts(&[0, 0]), 0, 2),
            None
        );
    }

    #[test]
    fn batch_planner_preserves_per_token_amounts_across_steps() {
        let tokens = vec![Address::from([1; 20]), Address::from([2; 20])];
        let reviewed = vec![
            RewardBatchIntervalSubtotal {
                interval: U256::ZERO,
                subtotals: vec![U256::from(2), U256::from(20)],
                gas: 5,
            },
            RewardBatchIntervalSubtotal {
                interval: U256::from(1),
                subtotals: vec![U256::from(3), U256::from(40)],
                gas: 5,
            },
        ];
        let steps = plan_reward_claim_batch_steps(&reviewed, &tokens, 5).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].reward_tokens, tokens);
        assert_eq!(
            steps[0].expected_amounts,
            vec![U256::from(2), U256::from(20)]
        );
        assert_eq!(
            steps[1].expected_amounts,
            vec![U256::from(3), U256::from(40)]
        );
    }
}
