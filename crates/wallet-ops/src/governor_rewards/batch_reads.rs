use super::*;

/// Build one shared-range claim review. Every configured token participates in the calculator
/// call, including tokens whose amount is zero. A token with no completed endpoint fails closed
/// because omitting it would change the reviewed atomic claim.
pub async fn fetch_reward_batch_evidence(
    chain_id: u64,
    account: Address,
    tokens: &[Address],
    metadata: &GovernorRewardsIntervalMetadata,
    snapshots: &[AccountSnapshot],
    route: &ClassRoute,
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
    let lower_bound = reward_starting_interval(snapshots, metadata.multiplier)?;
    if lower_bound > ending_interval {
        return Ok(None);
    }
    let count = usize::try_from(ending_interval - lower_bound)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| eyre!("reward interval range exceeds platform limits"))?;
    let route = RpcRoute::from(route.clone());
    let mut claimed = vec![vec![None; count]; tokens.len()];
    let calls = tokens
        .iter()
        .enumerate()
        .flat_map(|(token_index, &token)| {
            (0..count).map(move |offset| (token_index, token, offset))
        })
        .collect::<Vec<_>>();
    for chunk in calls.chunks(chunk_size.get()) {
        let calldata: Vec<(Address, Bytes)> = chunk
            .iter()
            .map(|&(_, token, offset)| {
                (
                    contracts.governor_rewards,
                    GovernorRewards::getClaimedCall {
                        account,
                        token,
                        interval: lower_bound + U256::from(offset),
                    }
                    .abi_encode()
                    .into(),
                )
            })
            .collect();
        let values = http
            .rpc_broker()
            .submit_calls_decoded_as::<GovernorRewards::getClaimedCall>(
                route.clone(),
                calldata,
                WalletRpcOrigin::GovernorRewards.into(),
            )
            .await?;
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
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| eyre!("claimed flag result is missing"))
        })
        .collect::<Result<Vec<_>>>()?;
    let starting_interval = claimed
        .iter()
        .filter_map(|flags| {
            flags.iter().enumerate().find_map(|(offset, &is_claimed)| {
                (!is_claimed).then_some(lower_bound + U256::from(offset))
            })
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
    let values = http
        .rpc_broker()
        .submit_calls_decoded_as::<GovernorRewards::calculateRewardsCall>(
            route.clone(),
            vec![(
                contracts.governor_rewards,
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
            )],
            WalletRpcOrigin::GovernorRewards.into(),
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
                .filter_map(|(offset, &is_claimed)| {
                    let interval = lower_bound + U256::from(offset);
                    (is_claimed && interval >= starting_interval).then_some(interval)
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
    route: &ClassRoute,
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
    let route = RpcRoute::from(route.clone());
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
        let calldata: Vec<(Address, Bytes)> = chunk
            .iter()
            .map(|&(_, token, offset)| {
                (
                    contracts.governor_rewards,
                    GovernorRewards::getClaimedCall {
                        account,
                        token,
                        interval: evidence.starting_interval + U256::from(offset),
                    }
                    .abi_encode()
                    .into(),
                )
            })
            .collect();
        let values = http
            .rpc_broker()
            .submit_calls_decoded_as::<GovernorRewards::getClaimedCall>(
                route.clone(),
                calldata,
                WalletRpcOrigin::GovernorRewards.into(),
            )
            .await?;
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
    route: &ClassRoute,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<RewardBatchAuthorizationState> {
    validate_reward_batch_authorization_evidence(evidence)?;
    let claimed_intervals =
        fetch_reward_batch_claimed_intervals(chain_id, account, evidence, route, http, chunk_size)
            .await?;
    let Some(contracts) = governance_contracts(chain_id) else {
        return Err(eyre!("reward contracts are unavailable"));
    };
    let route = RpcRoute::from(route.clone());
    let values = http
        .rpc_broker()
        .submit_calls_decoded_as::<GovernorRewards::calculateRewardsCall>(
            route.clone(),
            vec![(
                contracts.governor_rewards,
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
            )],
            WalletRpcOrigin::GovernorRewards.into(),
        )
        .await?;
    let result = values[0].clone();
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

/// Fetch the latest block gas limit over `route`.
pub async fn fetch_latest_block_gas_limit(route: &ClassRoute, http: &HttpContext) -> Result<u64> {
    let gas_limit = fetch_latest_block(route, http, WalletRpcOrigin::GovernorRewards)
        .await?
        .header
        .gas_limit();
    if gas_limit == 0 {
        return Err(eyre!("latest block gas limit was zero"));
    }
    Ok(gas_limit)
}

/// Calculate one exact per-interval amount vector for a shared multi-token range.
pub async fn fetch_reward_batch_interval_amounts(
    chain_id: u64,
    account: Address,
    evidence: &RewardBatchEvidence,
    route: &ClassRoute,
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
    let route = RpcRoute::from(route.clone());
    let mut amounts = Vec::with_capacity(count);
    for chunk in chunk_indices(count, chunk_size) {
        let calldata: Vec<(Address, Bytes)> = chunk
            .iter()
            .map(|&offset| {
                (
                    contracts.governor_rewards,
                    GovernorRewards::calculateRewardsCall {
                        tokens: evidence.reward_tokens.clone(),
                        account,
                        startingInterval: evidence.starting_interval + U256::from(offset),
                        endingInterval: evidence.starting_interval + U256::from(offset),
                        hints: vec![evidence.hints[offset]],
                        ignoreClaimed: true,
                    }
                    .abi_encode()
                    .into(),
                )
            })
            .collect();
        let values = http
            .rpc_broker()
            .submit_calls_decoded_as::<GovernorRewards::calculateRewardsCall>(
                route.clone(),
                calldata,
                WalletRpcOrigin::GovernorRewards.into(),
            )
            .await?;
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
