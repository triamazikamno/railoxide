use super::*;

/// Read interval metadata and the configured token endpoints through the configured RPC pool.
pub async fn fetch_interval_metadata(
    chain_id: u64,
    tokens: &[Address],
    route: &ClassRoute,
    http: &HttpContext,
) -> Result<Option<GovernorRewardsIntervalMetadata>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Ok(None);
    };
    let route = RpcRoute::from(route.clone());
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
    let values = http
        .rpc_broker()
        .submit_calls_decoded_as::<GovernorRewards::STAKING_DISTRIBUTION_INTERVAL_MULTIPLIERCall>(
            route.clone(),
            calls
                .into_iter()
                .map(|calldata| (contracts.governor_rewards, calldata))
                .collect(),
            WalletRpcOrigin::GovernorRewards.into(),
        )
        .await?;
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
    route: &ClassRoute,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Option<RewardEvidence>> {
    let mut results = fetch_reward_evidence_multi(
        chain_id,
        account,
        &[token],
        metadata,
        snapshots,
        route,
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
    route: &ClassRoute,
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
    let lower_bound = reward_starting_interval(snapshots, metadata.multiplier)?;
    let route = RpcRoute::from(route.clone());
    let mut claimed: Vec<Option<Vec<Option<bool>>>> = Vec::with_capacity(tokens.len());
    let mut errors: Vec<Option<String>> = vec![None; tokens.len()];
    let mut calls = Vec::new();
    for (token_index, &token) in tokens.iter().enumerate() {
        let Some(&next) = metadata.next_earmark_intervals.get(&token) else {
            errors[token_index] = Some("token has no earmark endpoint".into());
            claimed.push(None);
            continue;
        };
        let Ok(count) = usize::try_from(next.saturating_sub(lower_bound)) else {
            errors[token_index] = Some("reward interval count exceeds platform limits".into());
            claimed.push(None);
            continue;
        };
        claimed.push(Some(vec![None; count]));
        for offset in 0..count {
            calls.push((token_index, token, offset));
        }
    }
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
        match http
            .rpc_broker()
            .submit_calls_decoded_as::<GovernorRewards::getClaimedCall>(
                route.clone(),
                calldata,
                WalletRpcOrigin::GovernorRewards.into(),
            )
            .await
        {
            Ok(values) => {
                for (&(token_index, _, offset), value) in chunk.iter().zip(values) {
                    match value {
                        Ok(flag) => {
                            match claimed
                                .get_mut(token_index)
                                .and_then(Option::as_mut)
                                .and_then(|flags| flags.get_mut(offset))
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
            .map(|(offset, flag)| flag.map(|flag| (lower_bound + U256::from(offset), flag)))
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
        let calldata: Vec<(Address, Bytes)> = calculator_calls
            .iter()
            .map(|(_, evidence)| {
                (
                    contracts.governor_rewards,
                    GovernorRewards::calculateRewardsCall {
                        tokens: vec![evidence.token],
                        account,
                        startingInterval: evidence.starting_interval,
                        endingInterval: evidence.ending_interval,
                        hints: evidence.hints.clone(),
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

/// Calculate the exact reward subtotal for every interval in the evidence.
pub async fn fetch_reward_interval_amounts(
    chain_id: u64,
    account: Address,
    evidence: &RewardEvidence,
    route: &ClassRoute,
    http: &HttpContext,
    chunk_size: MulticallChunkSize,
) -> Result<Vec<RewardIntervalAmount>> {
    let Some(contracts) = governance_contracts(chain_id) else {
        return Err(eyre!("reward contracts are unavailable"));
    };
    let route = RpcRoute::from(route.clone());
    let count = usize::try_from(evidence.ending_interval - evidence.starting_interval)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| eyre!("reward interval range exceeds platform limits"))?;
    if evidence.hints.len() != count {
        return Err(eyre!("reward evidence hints do not match interval range"));
    }
    let mut amounts = Vec::with_capacity(count);
    for chunk in chunk_indices(count, chunk_size) {
        let calldata: Vec<(Address, Bytes)> = chunk
            .iter()
            .map(|&offset| {
                (
                    contracts.governor_rewards,
                    GovernorRewards::calculateRewardsCall {
                        tokens: vec![evidence.token],
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
