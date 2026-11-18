use super::*;

pub(in crate::root) async fn build_staking_action_draft(
    selection: StakingActionSelection,
    context_key: GovernanceContextKey,
    wallet_id: Arc<str>,
    actor_source: wallet_ops::vault::PublicAccountSource,
    amount_input: String,
    delegate_input: String,
    view_session: Arc<wallet_ops::vault::DesktopViewSession>,
    vault_store: Arc<wallet_ops::vault::DesktopVaultStore>,
    token_decimals: Option<u8>,
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    http: wallet_ops::HttpContext,
    operation: wallet_ops::OperationHttpClientSlot,
    reward_evidence_mode: RewardEvidenceMode,
    gas_fee_selection: PublicActionGasFeeSelection,
) -> Result<GovernanceSpendDraft, String> {
    // The action's one client carries every governance read and estimate of this preparation.
    let (route, client) =
        action_governance_route(context_key.chain_id, &effective_chain, &operation).await?;
    validate_governance_deployment(context_key.chain_id, &route, &http)
        .await
        .map_err(|error| error.to_string())?;
    let recipe = GovernanceDraftRecipe::Staking {
        selection: selection.clone(),
        context_key: context_key.clone(),
        amount_input: amount_input.clone(),
        delegate_input: delegate_input.clone(),
        token_decimals,
    };
    let chain_id = context_key.chain_id;
    let actor_uuid: Arc<str> = Arc::from(selection.actor_uuid.clone());
    let contracts = governance_contracts(chain_id)
        .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
    let actor = selection.actor;
    let target = GovernanceRefreshTarget::Staking(context_key);
    let (contract, contract_kind, observed_state, intent, workflow, continuation, staking_review) =
        match selection.kind {
            StakingActionKind::Stake => {
                let amount = wallet_ops::parse_send_amount(amount_input.trim(), token_decimals)
                    .map_err(|error| error.to_string())?;
                let amount = (!amount.is_zero())
                    .then_some(amount)
                    .ok_or_else(|| "Amount must be greater than zero".to_owned())?;
                let (balance, allowance) =
                    fetch_governance_token_balance_allowance(chain_id, actor, &route, &http)
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| {
                            "Governance token is not deployed on this chain".to_owned()
                        })?;
                let plan = plan_stake(actor, contracts.staking, balance, allowance, amount)
                    .map_err(|error| error.to_string())?;
                let (contract, intent, workflow) = if let Some(approval) = plan.approval.clone() {
                    (
                        contracts.governance_token,
                        approval,
                        Some(wallet_ops::GovernanceWorkflow::StakeApproval(plan.clone())),
                    )
                } else {
                    (
                        contracts.staking,
                        plan.stake
                            .ok_or_else(|| "Stake plan did not produce a stake call".to_owned())?,
                        None,
                    )
                };
                (
                    contract,
                    if contract == contracts.governance_token {
                        GovernanceContractKind::GovernanceToken
                    } else {
                        GovernanceContractKind::Staking
                    },
                    plan.observed_state,
                    intent,
                    workflow,
                    None,
                    Some(GovernanceStakingReviewProjection::Stake { amount }),
                )
            }
            StakingActionKind::Delegate { stake_id } => {
                let delegate = Address::from_str(delegate_input.trim())
                    .map_err(|_| "Enter a valid delegate address".to_owned())?;
                let position =
                    fresh_stake_position(chain_id, actor, stake_id, &route, &http).await?;
                let plan =
                    plan_delegate(actor, &position, delegate).map_err(|error| error.to_string())?;
                (
                    contracts.staking,
                    GovernanceContractKind::Staking,
                    plan.observed_state,
                    plan.intent,
                    None,
                    None,
                    Some(GovernanceStakingReviewProjection::Delegation(plan.evidence)),
                )
            }
            StakingActionKind::Undelegate { stake_id } => {
                let position =
                    fresh_stake_position(chain_id, actor, stake_id, &route, &http).await?;
                let plan = plan_undelegate(actor, &position).map_err(|error| error.to_string())?;
                (
                    contracts.staking,
                    GovernanceContractKind::Staking,
                    plan.observed_state,
                    plan.intent,
                    None,
                    None,
                    Some(GovernanceStakingReviewProjection::Delegation(plan.evidence)),
                )
            }
            StakingActionKind::Unlock { stake_id } => {
                let position =
                    fresh_stake_position(chain_id, actor, stake_id, &route, &http).await?;
                let metrics = fetch_staking_global_metrics(chain_id, &route, &http)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
                let projected_claim_timestamp = metrics
                    .chain_time
                    .checked_add(metrics.stake_locktime)
                    .ok_or_else(|| "Unlock claim timestamp overflowed".to_owned())?;
                let plan = plan_unlock(actor, &position).map_err(|error| error.to_string())?;
                let (intent, observed_state, workflow, previous_delegate, owner, amount) =
                    match plan {
                        wallet_ops::UnlockPlan::Direct(plan) => (
                            plan.intent,
                            plan.observed_state,
                            None,
                            plan.delegate,
                            plan.owner,
                            plan.amount,
                        ),
                        wallet_ops::UnlockPlan::UndelegateFirst(plan) => (
                            plan.intent.clone(),
                            plan.observed_state,
                            Some(wallet_ops::GovernanceWorkflow::UndelegateThenUnlock(plan)),
                            position.delegate,
                            position.owner,
                            position.amount,
                        ),
                    };
                (
                    contracts.staking,
                    GovernanceContractKind::Staking,
                    observed_state,
                    intent,
                    workflow,
                    None,
                    Some(GovernanceStakingReviewProjection::Unlock {
                        owner,
                        stake_id,
                        amount,
                        previous_delegate,
                        stake_locktime: metrics.stake_locktime,
                        projected_claim_timestamp,
                    }),
                )
            }
            StakingActionKind::PrincipalClaim { stake_id } => {
                let position =
                    fresh_stake_position(chain_id, actor, stake_id, &route, &http).await?;
                let metrics = fetch_staking_global_metrics(chain_id, &route, &http)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
                let plan = plan_principal_claim(actor, &position, metrics.chain_time)
                    .map_err(|error| error.to_string())?;
                let review = GovernanceStakingReviewProjection::PrincipalClaim(plan.clone());
                (
                    contracts.staking,
                    GovernanceContractKind::Staking,
                    plan.observed_state,
                    plan.intent,
                    None,
                    None,
                    Some(review),
                )
            }
            StakingActionKind::RewardClaimSelected { tokens } => {
                if tokens.is_empty() {
                    return Err("Select at least one reward to claim".to_owned());
                }
                if tokens.windows(2).any(|pair| pair[0] >= pair[1]) {
                    return Err(
                        "Selected reward tokens must be unique and ascending by address".to_owned(),
                    );
                }
                let (plan, step, evidence) = plan_reward_claim_draft(
                    chain_id,
                    actor,
                    &tokens,
                    &route,
                    &effective_chain,
                    &http,
                    &client,
                    reward_evidence_mode,
                )
                .await?;
                (
                    contracts.governor_rewards,
                    GovernanceContractKind::GovernorRewards,
                    plan.fingerprint,
                    step.intent,
                    None,
                    Some(GovernanceContinuation::Reward {
                        progress: wallet_ops::RewardClaimProgress::new(plan),
                        evidence,
                    }),
                    None,
                )
            }
        };
    build_typed_governance_spend_draft(
        target,
        wallet_id,
        actor_uuid,
        actor,
        actor_source,
        contract,
        contract_kind,
        observed_state,
        intent,
        view_session,
        vault_store,
        effective_chain,
        http,
        operation,
        gas_fee_selection,
        workflow,
        continuation,
        staking_review,
        recipe,
    )
    .await
}

/// Plan a claim for `requested_tokens`, which must already be non-empty and ascending by address.
pub(super) async fn plan_reward_claim_draft(
    chain_id: u64,
    actor: Address,
    requested_tokens: &[Address],
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
    reward_evidence_mode: RewardEvidenceMode,
) -> Result<
    (
        wallet_ops::RewardClaimPlan,
        wallet_ops::RewardClaimStepPlan,
        wallet_ops::RewardBatchEvidence,
    ),
    String,
> {
    let (evidence, reviewed_steps) = match reward_evidence_mode {
        RewardEvidenceMode::CachedInitial { evidence, steps } => {
            if evidence.reward_tokens.as_slice() != requested_tokens {
                return Err(
                    "Cached reward evidence token set changed; refresh before reviewing again"
                        .to_owned(),
                );
            }
            (*evidence, steps.filter(|steps| !steps.is_empty()))
        }
        RewardEvidenceMode::Fresh => (
            fetch_reward_selection_evidence(chain_id, actor, requested_tokens, route, http).await?,
            None,
        ),
    };
    // The selection form planned these steps against this same evidence, so the split does not
    // have to be estimated again; `plan_reward_claim_batch` still validates them below.
    let steps = match reviewed_steps {
        Some(steps) => steps,
        None => {
            plan_reward_steps_for_evidence(
                chain_id,
                actor,
                &evidence,
                route,
                effective_chain,
                http,
                client,
            )
            .await?
        }
    };
    let plan = wallet_ops::plan_reward_claim_batch(chain_id, actor, actor, true, &evidence, &steps)
        .map_err(|error| error.to_string())?;
    let step = plan
        .steps
        .first()
        .cloned()
        .ok_or_else(|| "Reward claim plan is empty".to_owned())?;
    Ok((plan, step, evidence))
}

/// Read fresh shared-range evidence for `requested_tokens`, which must already be non-empty and
/// ascending by address. One token reads the single-token path and is widened to a batch shape.
pub(super) async fn fetch_reward_selection_evidence(
    chain_id: u64,
    actor: Address,
    requested_tokens: &[Address],
    route: &wallet_ops::ClassRoute,
    http: &wallet_ops::HttpContext,
) -> Result<wallet_ops::RewardBatchEvidence, String> {
    let metadata = fetch_interval_metadata(chain_id, requested_tokens, route, http)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Reward metadata unavailable".to_owned())?;
    let snapshots = fetch_account_snapshots(
        chain_id,
        actor,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    if let [token] = requested_tokens[..] {
        let evidence = fetch_reward_evidence(
            chain_id,
            actor,
            token,
            &metadata,
            &snapshots,
            route,
            http,
            wallet_ops::MulticallChunkSize::default(),
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "No unclaimed reward is available".to_owned())?;
        return Ok(evidence.into());
    }
    fetch_reward_batch_evidence(
        chain_id,
        actor,
        requested_tokens,
        &metadata,
        &snapshots,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "No unclaimed reward is available".to_owned())
}

async fn plan_reward_steps_for_evidence(
    chain_id: u64,
    actor: Address,
    evidence: &wallet_ops::RewardBatchEvidence,
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<Vec<wallet_ops::RewardClaimStep>, String> {
    plan_reward_steps_with_fee(
        chain_id,
        actor,
        evidence,
        route,
        effective_chain,
        http,
        client,
    )
    .await
    .map(|(steps, _)| steps)
}

/// Plan the claim steps for this evidence and report the fee per gas the estimates were priced
/// at, which is what the selection form multiplies by the planned gas. Governance reads use
/// `route`, and gas estimates use `client`, the action's client that `route` is isolated with.
async fn plan_reward_steps_with_fee(
    chain_id: u64,
    actor: Address,
    evidence: &wallet_ops::RewardBatchEvidence,
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<(Vec<wallet_ops::RewardClaimStep>, u128), String> {
    let gas_ceiling = wallet_ops::fetch_latest_block_gas_limit(route, http)
        .await
        .map_err(|error| error.to_string())?;
    let contract = contracts_for_reward(chain_id)?;
    if let Ok(estimate) = estimate_reward_intent(
        chain_id,
        actor,
        contract,
        evidence.to_claim_intent(),
        effective_chain,
        http,
        client,
    )
    .await
        && estimate.gas_limit <= gas_ceiling
    {
        return Ok((
            vec![wallet_ops::RewardClaimStep {
                starting_interval: evidence.starting_interval,
                ending_interval: evidence.ending_interval,
                reward_tokens: evidence.reward_tokens.clone(),
                subtotal: U256::ZERO,
                expected_amounts: evidence.expected_amounts.clone(),
                estimated_gas: estimate.raw_gas_limit,
            }],
            estimate.expected_fee_per_gas,
        ));
    }
    exact_reward_steps(
        chain_id,
        actor,
        evidence,
        gas_ceiling,
        route,
        effective_chain,
        http,
        client,
    )
    .await
}

/// Estimate the selection and its one-token alternatives against one shared evidence range.
pub(super) async fn estimate_reward_selection(
    chain_id: u64,
    actor: Address,
    tokens: &[Address],
    available: wallet_ops::RewardBatchEvidence,
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<RewardSelectionEstimate, Arc<str>> {
    let evidence = (!tokens.is_empty())
        .then(|| available.project(tokens))
        .transpose()
        .map_err(|error| Arc::from(error.to_string()))?;
    let baseline = if tokens.len() <= 1 {
        wallet_ops::estimate_reward_claim_baseline(
            chain_id,
            actor,
            &available,
            effective_chain,
            http,
            client,
        )
        .await
        .ok()
    } else {
        None
    };
    let (steps, expected_fee_per_gas) = match &evidence {
        Some(evidence) => plan_reward_steps_with_fee(
            chain_id,
            actor,
            evidence,
            route,
            effective_chain,
            http,
            client,
        )
        .await
        .map_err(Arc::from)?,
        None => (
            Vec::new(),
            baseline
                .as_ref()
                .map_or(0, |estimate| estimate.expected_fee_per_gas),
        ),
    };
    let selected_gas =
        reward_selection_gas(&steps).ok_or_else(|| Arc::from("Estimated claim gas overflowed"))?;
    let comparison_gas = if tokens.is_empty() {
        baseline.as_ref().map(|estimate| estimate.raw_gas_limit)
    } else {
        Some(selected_gas)
    };
    let comparisons = available.reward_tokens.iter().copied().map(|token| {
        let available = &available;
        let baseline = baseline.as_ref();
        async move {
            let mut alternative = tokens.to_vec();
            let selected = match alternative.binary_search(&token) {
                Ok(index) => {
                    alternative.remove(index);
                    true
                }
                Err(index) => {
                    alternative.insert(index, token);
                    false
                }
            };
            let alternative_gas = if alternative.is_empty() {
                baseline.map(|estimate| estimate.raw_gas_limit)
            } else if let Ok(evidence) = available.project(&alternative) {
                plan_reward_steps_with_fee(
                    chain_id,
                    actor,
                    &evidence,
                    route,
                    effective_chain,
                    http,
                    client,
                )
                .await
                .ok()
                .and_then(|(steps, _)| reward_selection_gas(&steps))
            } else {
                None
            };
            let added = comparison_gas
                .zip(alternative_gas)
                .and_then(|(current, alternative)| {
                    reward_claim_added_gas(selected, current, alternative)
                });
            (token, added)
        }
    });
    let added_gas = futures_util::stream::iter(comparisons)
        .buffer_unordered(STAKING_REWARD_EVIDENCE_CONCURRENCY)
        .collect::<BTreeMap<_, _>>()
        .await;
    let shared_gas = reward_claim_shared_gas(tokens, comparison_gas, &added_gas);
    Ok(RewardSelectionEstimate {
        evidence,
        steps,
        expected_fee_per_gas,
        added_gas,
        shared_gas,
    })
}

fn reward_selection_gas(steps: &[wallet_ops::RewardClaimStep]) -> Option<u64> {
    steps
        .iter()
        .try_fold(0_u64, |total, step| total.checked_add(step.estimated_gas))
}

pub(super) const fn reward_claim_added_gas(
    selected: bool,
    current: u64,
    alternative: u64,
) -> Option<u64> {
    if selected {
        current.checked_sub(alternative)
    } else {
        alternative.checked_sub(current)
    }
}

pub(super) fn reward_claim_shared_gas(
    selected: &[Address],
    total_gas: Option<u64>,
    added_gas: &BTreeMap<Address, Option<u64>>,
) -> Option<u64> {
    selected.iter().try_fold(total_gas?, |remaining, token| {
        remaining.checked_sub((*added_gas.get(token)?)?)
    })
}

/// The estimated cost of one planned claim selection at the fee per gas its steps were estimated
/// at. It fails closed rather than showing a wrapped number.
pub(super) fn reward_selection_cost(
    steps: &[wallet_ops::RewardClaimStep],
    fee_per_gas: u128,
) -> Option<U256> {
    let gas = reward_selection_gas(steps)?;
    U256::from(gas).checked_mul(U256::from(fee_per_gas))
}

/// Reprice cached gas with the current quote; retain the estimate's price until a quote arrives.
pub(super) fn reward_claim_fee_per_gas(
    selection: PublicActionGasFeeSelection,
    quote: Option<wallet_ops::SelfBroadcastGasFeeQuote>,
    auto_fee_per_gas: u128,
) -> u128 {
    match selection {
        PublicActionGasFeeSelection::Auto => quote.map_or(auto_fee_per_gas, |quote| {
            wallet_ops::expected_eip1559_fee_per_gas(
                quote,
                quote.suggested_max_fee_per_gas,
                quote.suggested_max_priority_fee_per_gas,
            )
        }),
        PublicActionGasFeeSelection::Custom {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } => quote.map_or(max_fee_per_gas, |quote| {
            wallet_ops::expected_eip1559_fee_per_gas(
                quote,
                max_fee_per_gas,
                max_priority_fee_per_gas,
            )
        }),
    }
}

fn contracts_for_reward(chain_id: u64) -> Result<Address, String> {
    governance_contracts(chain_id)
        .map(|contracts| contracts.governor_rewards)
        .ok_or_else(|| "Reward contracts unavailable".to_owned())
}

async fn estimate_reward_intent(
    chain_id: u64,
    actor: Address,
    contract: Address,
    action: GovernanceActionIntent,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<wallet_ops::PublicAdvancedTransactionEstimate, String> {
    let context = GovernanceActionContext {
        private_wallet_uuid: String::new(),
        chain_id,
        public_account_uuid: String::new(),
        actor,
        contract,
        contract_kind: GovernanceContractKind::GovernorRewards,
        observed_state: B256::ZERO,
    };
    let resolved = action
        .resolve(&context)
        .map_err(|error| error.to_string())?;
    wallet_ops::estimate_public_advanced_transaction(
        PublicAdvancedTransactionEstimateRequest {
            chain_id,
            effective_chain: effective_chain.clone(),
            from: actor,
            intent: resolved.raw,
            gas_fee: PublicActionGasFeeSelection::Auto,
            access_list: None,
        },
        http,
        client,
    )
    .await
    .map_err(|error| error.to_string())
}

async fn exact_reward_steps(
    chain_id: u64,
    actor: Address,
    evidence: &wallet_ops::RewardBatchEvidence,
    gas_ceiling: u64,
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<(Vec<wallet_ops::RewardClaimStep>, u128), String> {
    let amounts = wallet_ops::fetch_reward_batch_interval_amounts(
        chain_id,
        actor,
        evidence,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    if amounts.is_empty() {
        return Err("Reward interval amounts are empty".to_owned());
    }
    for (index, amount) in amounts.iter().enumerate() {
        let expected = evidence.starting_interval + U256::from(index);
        if amount.interval != expected {
            return Err("Reward interval amounts are not consecutive".to_owned());
        }
    }
    let contract = contracts_for_reward(chain_id)?;
    let mut pending = vec![(0_usize, amounts.len())];
    let mut accepted = Vec::new();
    let mut expected_fee_per_gas = 0_u128;
    while let Some((start, end)) = pending.pop() {
        let mut expected_amounts = vec![U256::ZERO; evidence.reward_tokens.len()];
        for amount in &amounts[start..end] {
            for (total, value) in expected_amounts.iter_mut().zip(&amount.subtotals) {
                *total = total
                    .checked_add(*value)
                    .ok_or_else(|| "Reward interval subtotal overflowed".to_owned())?;
            }
        }
        if expected_amounts.iter().all(U256::is_zero) {
            continue;
        }
        let action = GovernanceActionIntent::RewardClaim {
            reward_tokens: evidence.reward_tokens.clone(),
            starting_interval: amounts[start].interval,
            ending_interval: amounts[end - 1].interval,
            snapshot_hints: evidence.hints[start..end].to_vec(),
            expected_amounts: expected_amounts.clone(),
        };
        match estimate_reward_intent(
            chain_id,
            actor,
            contract,
            action,
            effective_chain,
            http,
            client,
        )
        .await
        {
            Ok(estimate) if estimate.gas_limit <= gas_ceiling => {
                expected_fee_per_gas = estimate.expected_fee_per_gas;
                accepted.push(wallet_ops::RewardClaimStep {
                    starting_interval: amounts[start].interval,
                    ending_interval: amounts[end - 1].interval,
                    reward_tokens: evidence.reward_tokens.clone(),
                    subtotal: U256::ZERO,
                    expected_amounts,
                    estimated_gas: estimate.raw_gas_limit,
                });
            }
            Ok(_) | Err(_) => {
                let Some(boundary) =
                    wallet_ops::reward_batch_positive_split_boundary(&amounts, start, end)
                else {
                    return Err("Reward range does not fit the gas ceiling".to_owned());
                };
                pending.push((boundary + 1, end));
                pending.push((start, boundary + 1));
            }
        }
    }
    accepted.sort_by_key(|step| step.starting_interval);
    let mut expected = evidence.starting_interval;
    let mut total = vec![U256::ZERO; evidence.reward_tokens.len()];
    for step in &accepted {
        if step.starting_interval != expected || step.expected_amounts.iter().all(U256::is_zero) {
            return Err("Reward ranges are not consecutive positive coverage".to_owned());
        }
        expected = step
            .ending_interval
            .checked_add(U256::from(1_u8))
            .ok_or_else(|| "Reward interval overflowed".to_owned())?;
        for (total, amount) in total.iter_mut().zip(&step.expected_amounts) {
            *total = total
                .checked_add(*amount)
                .ok_or_else(|| "Reward subtotal overflowed".to_owned())?;
        }
    }
    if expected != evidence.ending_interval + U256::from(1_u8) || total != evidence.expected_amounts
    {
        return Err("Reward ranges do not cover the reviewed evidence".to_owned());
    }
    Ok((accepted, expected_fee_per_gas))
}

async fn fresh_stake_position(
    chain_id: u64,
    actor: Address,
    stake_id: U256,
    route: &wallet_ops::ClassRoute,
    http: &wallet_ops::HttpContext,
) -> Result<StakePosition, String> {
    let metrics = fetch_staking_global_metrics(chain_id, route, http)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Staking is not deployed on this chain".to_owned())?;
    let result = fetch_account_stakes(
        chain_id,
        actor,
        metrics.chain_time,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let stakes = result.stakes?;
    stakes
        .into_iter()
        .find(|stake| stake.id == stake_id)
        .ok_or_else(|| "Stake no longer exists; refresh before authorizing".to_owned())
}

/// Fresh evidence and claim steps for continuing a reward claim after a confirmed split step,
/// read over the actor's `route`. `None` means nothing is left to claim.
pub(super) async fn read_reward_continuation(
    chain_id: u64,
    actor: Address,
    reward_tokens: &[Address],
    route: &wallet_ops::ClassRoute,
    effective_chain: &wallet_ops::settings::EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
    client: &wallet_ops::OperationHttpClient,
) -> Result<
    Option<(
        wallet_ops::RewardBatchEvidence,
        Vec<wallet_ops::RewardClaimStep>,
    )>,
    String,
> {
    validate_governance_deployment(chain_id, route, http)
        .await
        .map_err(|error| error.to_string())?;
    let metadata = fetch_interval_metadata(chain_id, reward_tokens, route, http)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Reward metadata unavailable".to_owned())?;
    let snapshots = fetch_account_snapshots(
        chain_id,
        actor,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let fresh = fetch_reward_batch_evidence(
        chain_id,
        actor,
        reward_tokens,
        &metadata,
        &snapshots,
        route,
        http,
        wallet_ops::MulticallChunkSize::default(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let Some(fresh) = fresh else {
        return Ok(None);
    };
    let steps = plan_reward_steps_for_evidence(
        chain_id,
        actor,
        &fresh,
        route,
        effective_chain,
        http,
        client,
    )
    .await?;
    Ok(Some((fresh, steps)))
}

/// Reads each participant over a route of its own, one at a time in random order. Before every
/// participant after the first it awaits `jitter`, and it stops once `is_current` reports that
/// the refresh was superseded. A route that fails to open reaches `read` as that participant's
/// error, with no shared-route fallback. `read` owns the route, so it closes when `read` ends.
pub(super) async fn read_participants_isolated<P>(
    mut participants: Vec<P>,
    mut jitter: impl AsyncFnMut(),
    mut is_current: impl FnMut() -> bool,
    mut open_route: impl AsyncFnMut() -> Result<wallet_ops::ClassRoute, String>,
    mut read: impl AsyncFnMut(P, Result<wallet_ops::ClassRoute, String>),
) {
    participants.shuffle(&mut rand::rng());
    for (index, participant) in participants.into_iter().enumerate() {
        if index > 0 {
            jitter().await;
        }
        if !is_current() {
            return;
        }
        let route = open_route().await;
        read(participant, route).await;
    }
}
