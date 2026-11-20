#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn governance_authorization_summary(
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
            crate::root::proposals::proposal_stage_label(projection.stage),
        ));
        if let (Some(remaining), Some(snapshot)) =
            (projection.power_remaining_after, projection.snapshot_power)
        {
            rows.push(SpendAuthorizationSummaryRow::new(
                "Power after",
                format!(
                    "{} of {} RAIL",
                    crate::root::proposals::format_compact_rail_amount(remaining),
                    crate::root::proposals::format_compact_rail_amount(snapshot),
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
                    crate::root::proposals::format_compact_rail_amount(after),
                    crate::root::proposals::format_compact_rail_amount(threshold),
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
                    crate::root::governance::unlock_period_label(*stake_locktime),
                ));
                rows.push(SpendAuthorizationSummaryRow::new(
                    "Claimable",
                    crate::root::proposals::format_date_short(projected_claim_timestamp),
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
