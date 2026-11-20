#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn proposal_participation_key_matches(
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

/// The fresh proposal, its rules, the chain time and the actor's participation, all read over
/// the actor's `route`.
pub(in crate::root) async fn read_proposal_action_state(
    chain_id: u64,
    proposal: &GovernanceProposal,
    actor: Address,
    route: &wallet_ops::ClassRoute,
    http: &wallet_ops::HttpContext,
) -> Result<
    (
        GovernanceProposal,
        GovernanceContractRules,
        U256,
        GovernanceParticipation,
    ),
    String,
> {
    let overview = wallet_ops::fetch_governance_overview(chain_id, route, http)
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
    let page = global_index / crate::root::proposals::PROPOSALS_PAGE_SIZE;
    let page_rows = wallet_ops::fetch_governance_page(
        &overview,
        page,
        NonZeroUsize::new(crate::root::proposals::PROPOSALS_PAGE_SIZE).expect("nonzero page size"),
        route,
        http,
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
    let chain_time = wallet_ops::fetch_governance_chain_time(route, http)
        .await
        .map_err(|error| format_report_chain(&error))?;
    let participation =
        wallet_ops::fetch_governance_participation(chain_id, &fresh, actor, route, http)
            .await
            .map_err(|error| format_report_chain(&error))?
            .state
            .map_err(|error| error.to_string())?;
    Ok((fresh, rules, chain_time, participation))
}

pub(in crate::root) async fn build_governance_spend_draft(
    proposal: GovernanceProposal,
    key: ProposalParticipationKey,
    selection: ProposalActionSelection,
    amount: Option<U256>,
    wallet_id: Arc<str>,
    actor_uuid: Arc<str>,
    actor_source: PublicAccountSource,
    view_session: Arc<wallet_ops::vault::DesktopViewSession>,
    vault_store: Arc<wallet_ops::vault::DesktopVaultStore>,
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    http: wallet_ops::HttpContext,
    operation: wallet_ops::OperationHttpClientSlot,
    gas_fee_selection: PublicActionGasFeeSelection,
) -> Result<GovernanceSpendDraft, String> {
    let recipe_proposal = proposal.clone();
    let recipe_key = key.clone();
    let recipe_amount = amount;
    let (route, client) = crate::root::governance::action_governance_route(
        key.context.chain_id,
        &effective_chain,
        &operation,
    )
    .await?;
    let (fresh, rules, chain_time, participation) = read_proposal_action_state(
        key.context.chain_id,
        &proposal,
        selection.actor,
        &route,
        &http,
    )
    .await?;
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
        &client,
    )
    .await
    .map_err(|error| format_report_chain(&error))?;
    let estimate_completed_at = Instant::now();
    let review = GovernanceActionReview::from_resolved(
        &resolved,
        context,
        Some(estimate.fee_projection(None, 18)),
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
        operation,
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

pub(in crate::root) async fn build_typed_governance_spend_draft(
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
    effective_chain: wallet_ops::settings::EffectiveChainConfig,
    http: wallet_ops::HttpContext,
    operation: wallet_ops::OperationHttpClientSlot,
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
    let client = operation
        .client()
        .await
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
        &client,
    )
    .await
    .map_err(|error| format_report_chain(&error))?;
    let estimate_completed_at = Instant::now();
    let review = GovernanceActionReview::from_resolved(
        &resolved,
        context,
        Some(estimate.fee_projection(None, 18)),
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
        operation,
    })
}

pub(in crate::root) fn participation_capacity(
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

pub(in crate::root) fn validate_proposal_action(
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
