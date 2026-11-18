#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::root) const fn proposal_stage_label(stage: GovernanceProposalStage) -> &'static str {
    match stage {
        GovernanceProposalStage::AwaitingSponsorship => "Awaiting sponsorship",
        GovernanceProposalStage::ReadyToCallVote => "Ready to call vote",
        GovernanceProposalStage::SponsorshipExpired => "Sponsorship expired",
        GovernanceProposalStage::VoteCallExpired => "Vote call expired",
        GovernanceProposalStage::VotingDelay => "Voting delay",
        GovernanceProposalStage::VotingOpen => "Voting open",
        GovernanceProposalStage::NayOnlyVoting => "Nay-only voting",
        GovernanceProposalStage::Failed => "Failed",
        GovernanceProposalStage::PassedAwaitingExecution => "Passed · awaiting execution",
        GovernanceProposalStage::PassedExecutable => "Passed · executable",
        GovernanceProposalStage::ExecutionExpired => "Execution expired",
        GovernanceProposalStage::Executed => "Executed",
    }
}
pub(super) const fn proposal_version_label(
    version: wallet_ops::GovernanceContractVersion,
) -> &'static str {
    match version {
        wallet_ops::GovernanceContractVersion::V2 => "V2",
        wallet_ops::GovernanceContractVersion::V1 => "V1",
    }
}
pub(super) fn next_proposal_page(current: usize, total_pages: usize) -> Option<usize> {
    current.checked_add(1).filter(|page| *page < total_pages)
}
pub(super) fn proposal_copy_id(
    proposal: &ResolvedProposal,
    field: &str,
    control: &str,
) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-{}-{}-{field}-{control}",
        proposal_version_label(proposal.proposal.contract_version),
        proposal.proposal.index,
    ))
}
pub(super) fn proposal_table_scroll_key(identity: &ProposalIdentity, ordinal: usize) -> String {
    format!(
        "wallet-proposal-table-scroll-v1-{}-{}-{}-{ordinal}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    )
}
pub(super) fn proposal_row_group(identity: &ProposalIdentity) -> SharedString {
    SharedString::from(format!(
        "wallet-proposal-row-group-{}-{}-{}",
        proposal_version_label(identity.contract_version),
        identity.contract_address,
        identity.index,
    ))
}
pub(super) fn format_deadline(deadline: &U256) -> String {
    format_datetime_short(deadline)
}
pub(in crate::root) fn format_date_short(timestamp: &U256) -> String {
    format_timestamp_with_pattern(timestamp, "%d %b %Y")
}
pub(super) fn format_datetime_short(timestamp: &U256) -> String {
    format_timestamp_with_pattern(timestamp, "%d %b %Y, %H:%M")
}
fn format_timestamp_with_pattern(timestamp: &U256, pattern: &str) -> String {
    let Ok(seconds) = i64::try_from(*timestamp) else {
        return format!("Unix timestamp {timestamp}");
    };
    let Some(utc) = DateTime::<Utc>::from_timestamp(seconds, 0) else {
        return format!("Unix timestamp {timestamp}");
    };
    utc.with_timezone(&Local).format(pattern).to_string()
}

pub(in crate::root) fn format_compact_rail_amount(amount: U256) -> String {
    let scale = U256::from(10).pow(U256::from(18));
    let whole = amount / scale;
    if whole < U256::from(1_000) {
        return railgun_ui::format_token_amount(amount, 18);
    }
    let whole = u128::try_from(whole).unwrap_or(u128::MAX);
    if whole < 1_000_000 {
        let integer = whole / 1_000;
        let decimal = (whole % 1_000) / 100;
        return compact_suffix(integer, decimal, 'K');
    }
    if whole < 1_000_000_000 {
        let integer = whole / 1_000_000;
        let decimal = (whole % 1_000_000) / 10_000;
        return compact_suffix_two_decimals(integer, decimal, 'M');
    }
    let integer = whole / 1_000_000_000;
    let decimal = (whole % 1_000_000_000) / 10_000_000;
    compact_suffix_two_decimals(integer, decimal, 'B')
}
pub(in crate::root) fn format_compact_rail_amount_with_unit(amount: U256) -> String {
    format!("{} RAIL", format_compact_rail_amount(amount))
}

pub(super) const fn proposal_capacity_kind(stage: GovernanceProposalStage) -> ProposalCapacityKind {
    match stage {
        GovernanceProposalStage::AwaitingSponsorship
        | GovernanceProposalStage::ReadyToCallVote
        | GovernanceProposalStage::SponsorshipExpired
        | GovernanceProposalStage::VoteCallExpired => ProposalCapacityKind::Sponsorship,
        GovernanceProposalStage::VotingDelay
        | GovernanceProposalStage::VotingOpen
        | GovernanceProposalStage::NayOnlyVoting
        | GovernanceProposalStage::Failed
        | GovernanceProposalStage::PassedAwaitingExecution
        | GovernanceProposalStage::PassedExecutable
        | GovernanceProposalStage::ExecutionExpired
        | GovernanceProposalStage::Executed => ProposalCapacityKind::Voting,
    }
}

fn compact_suffix(integer: u128, decimal: u128, suffix: char) -> String {
    if decimal == 0 {
        format!("{integer}{suffix}")
    } else {
        format!("{integer}.{decimal}{suffix}")
    }
}
fn compact_suffix_two_decimals(integer: u128, decimal: u128, suffix: char) -> String {
    if decimal == 0 {
        format!("{integer}{suffix}")
    } else if decimal.is_multiple_of(10) {
        format!("{integer}.{}{}", decimal / 10, suffix)
    } else {
        format!("{integer}.{decimal:02}{suffix}")
    }
}
pub(super) fn per_mille(numerator: U256, denominator: U256) -> u16 {
    if denominator.is_zero() {
        return 0;
    }
    let numerator = u128::try_from(numerator).unwrap_or(u128::MAX);
    let denominator = u128::try_from(denominator).unwrap_or(u128::MAX);
    u16::try_from((numerator.saturating_mul(1_000) / denominator).min(1_000)).unwrap_or(1_000)
}

pub(super) fn proposal_capacity_summary_state(
    available: Option<U256>,
    maximum: Option<U256>,
    loading_count: usize,
    unavailable_count: usize,
    participant_count: usize,
) -> ProposalCapacitySummaryState {
    if participant_count == 0 {
        return ProposalCapacitySummaryState::Empty;
    }
    if unavailable_count > 0 {
        return ProposalCapacitySummaryState::Unavailable;
    }
    if loading_count > 0 || available.is_none() || maximum.is_none() {
        return ProposalCapacitySummaryState::Loading;
    }
    let (Some(available), Some(maximum)) = (available, maximum) else {
        return ProposalCapacitySummaryState::Loading;
    };
    match (available, maximum) {
        (available, maximum) if available == maximum => ProposalCapacitySummaryState::Full,
        (available, maximum) if available.is_zero() && !maximum.is_zero() => {
            ProposalCapacitySummaryState::Exhausted
        }
        _ => ProposalCapacitySummaryState::Partial,
    }
}

pub(super) const fn proposal_closed_history_kind(
    stage: GovernanceProposalStage,
) -> Option<ProposalClosedHistoryKind> {
    match stage {
        GovernanceProposalStage::SponsorshipExpired | GovernanceProposalStage::VoteCallExpired => {
            Some(ProposalClosedHistoryKind::Sponsorship)
        }
        GovernanceProposalStage::Failed
        | GovernanceProposalStage::PassedAwaitingExecution
        | GovernanceProposalStage::PassedExecutable
        | GovernanceProposalStage::ExecutionExpired
        | GovernanceProposalStage::Executed => Some(ProposalClosedHistoryKind::Voting),
        _ => None,
    }
}

pub(super) fn vote_split(yay: U256, nay: U256) -> Option<u16> {
    let total = yay.checked_add(nay)?;
    if total.is_zero() {
        return None;
    }
    Some(per_mille(yay, total))
}
pub(super) fn format_ratio_percent(per_mille: u16) -> String {
    format!("{}.{:01}%", per_mille / 10, per_mille % 10)
}
pub(super) fn ratio_fraction(per_mille: u16) -> f32 {
    f32::from(per_mille) / 1_000.0
}
pub(super) fn countdown(deadline: &U256, now: U256) -> Option<String> {
    let remaining = deadline.checked_sub(now)?;
    let remaining = u64::try_from(remaining).ok()?;
    Some(format_compact_duration(Duration::from_secs(remaining)))
}
pub(super) fn list_voting_deadline(
    status: &GovernanceProposalStatus,
) -> Option<(U256, &'static str)> {
    match status.stage {
        GovernanceProposalStage::VotingOpen => {
            Some((*status.deadlines.yay_end.as_ref()?, "Yay voting ends in"))
        }
        GovernanceProposalStage::NayOnlyVoting => {
            Some((*status.deadlines.nay_end.as_ref()?, "Nay voting ends in"))
        }
        _ => None,
    }
}
