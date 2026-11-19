use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use alloy::primitives::{Address, B256, FixedBytes, U256, address};
use alloy::sol_types::SolCall;
use markdown::{ParseOptions, mdast::Node, to_mdast};

use super::{
    CONTENT_WIDTH, DecodedProposalAction, DocumentCompletion, MAX_MDAST_NODES,
    MAX_PREPARED_SOURCE_BYTES, MAX_TABLE_RENDER_WIDTH_PX, PreparationBudget,
    ProposalActionIdentity, ProposalBlock, ProposalCapacityKind, ProposalCapacitySummaryState,
    ProposalClosedHistoryKind, ProposalDecodedDetail, ProposalDelegator, ProposalDetailTab,
    ProposalDocumentState, ProposalErc20, ProposalGovernanceToken, ProposalGovernorRewards,
    ProposalOpStackSender, ProposalOwnable, ProposalPreparationFailure, ProposalPresentation,
    ProposalProxyAdmin, ProposalRailgun, ProposalSelection, ProposalTreasury,
    ProposalWrappedNative, ProposalsPage, ProposalsState, ResolvedProposal, TABLE_COLUMN_CHROME_PX,
    TABLE_OUTER_BORDER_PX, TimelineDeadline, action_calldata_hex, compact_calldata_display,
    decode_proposal_action, ensure_mdast_node_limit, format_compact_rail_amount,
    format_compact_rail_amount_with_unit, inert_raw_fallback_source, list_voting_deadline,
    next_proposal_page, parse_proposal_blocks, prepare_proposal_presentation,
    prepared_markdown_source_len, proposal_action_id, proposal_action_target_label,
    proposal_capacity_kind, proposal_capacity_summary_state, proposal_closed_history_kind,
    proposal_decoded_preview, proposal_role_label, proposal_table_scroll_key,
    send_document_completions, table_fingerprint, timeline_deadline_completed,
    timeline_sponsorship_completed, vote_split,
};
use crate::root::markdown_sanitize::{contains_activatable_markdown, visible_node_text};
use ui::theme::APP_TEXT_SIZE;
use wallet_ops::{
    GovernanceContractRules, GovernanceContractSummary, GovernanceContractVersion,
    GovernanceDocument, GovernanceOverview, GovernanceProposal, GovernanceProposalAction,
    GovernanceProposalDeadlines, GovernanceProposalStage, GovernanceProposalStatus,
    TokenAnchorRateCache,
};

fn test_proposal(index: u64, document: ProposalDocumentState) -> ResolvedProposal {
    ResolvedProposal {
        proposal: GovernanceProposal {
            contract_version: wallet_ops::GovernanceContractVersion::V2,
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
        },
        rules: GovernanceContractRules {
            sponsor_threshold: U256::ZERO,
            quorum: U256::ZERO,
            sponsor_window: U256::from(1),
            voting_start_offset: U256::from(1),
            voting_yay_end_offset: U256::from(2),
            voting_nay_end_offset: U256::from(3),
            execution_start_offset: U256::from(4),
            execution_end_offset: U256::from(5),
        },
        document,
    }
}

#[test]
fn participation_capacity_follows_proposal_stage() {
    let cases = [
        (
            GovernanceProposalStage::AwaitingSponsorship,
            ProposalCapacityKind::Sponsorship,
        ),
        (
            GovernanceProposalStage::ReadyToCallVote,
            ProposalCapacityKind::Sponsorship,
        ),
        (
            GovernanceProposalStage::SponsorshipExpired,
            ProposalCapacityKind::Sponsorship,
        ),
        (
            GovernanceProposalStage::VoteCallExpired,
            ProposalCapacityKind::Sponsorship,
        ),
        (
            GovernanceProposalStage::VotingDelay,
            ProposalCapacityKind::Voting,
        ),
        (
            GovernanceProposalStage::VotingOpen,
            ProposalCapacityKind::Voting,
        ),
        (
            GovernanceProposalStage::NayOnlyVoting,
            ProposalCapacityKind::Voting,
        ),
        (
            GovernanceProposalStage::Failed,
            ProposalCapacityKind::Voting,
        ),
        (
            GovernanceProposalStage::PassedAwaitingExecution,
            ProposalCapacityKind::Voting,
        ),
        (
            GovernanceProposalStage::PassedExecutable,
            ProposalCapacityKind::Voting,
        ),
        (
            GovernanceProposalStage::ExecutionExpired,
            ProposalCapacityKind::Voting,
        ),
        (
            GovernanceProposalStage::Executed,
            ProposalCapacityKind::Voting,
        ),
    ];
    for (stage, expected) in cases {
        assert_eq!(proposal_capacity_kind(stage), expected);
    }
}

#[test]
fn participation_summary_state_classifies_capacity_and_failures() {
    let cases = [
        (
            Some(U256::from(3)),
            Some(U256::from(3)),
            0,
            0,
            1,
            ProposalCapacitySummaryState::Full,
        ),
        (
            Some(U256::ZERO),
            Some(U256::ZERO),
            0,
            0,
            1,
            ProposalCapacitySummaryState::Full,
        ),
        (
            Some(U256::ZERO),
            Some(U256::from(3)),
            0,
            0,
            1,
            ProposalCapacitySummaryState::Exhausted,
        ),
        (
            Some(U256::from(1)),
            Some(U256::from(3)),
            0,
            0,
            1,
            ProposalCapacitySummaryState::Partial,
        ),
        (None, None, 1, 0, 1, ProposalCapacitySummaryState::Loading),
        (
            Some(U256::from(1)),
            None,
            0,
            1,
            1,
            ProposalCapacitySummaryState::Unavailable,
        ),
        (
            None,
            None,
            1,
            1,
            1,
            ProposalCapacitySummaryState::Unavailable,
        ),
        (
            Some(U256::ZERO),
            Some(U256::ZERO),
            0,
            0,
            0,
            ProposalCapacitySummaryState::Empty,
        ),
    ];
    for (available, maximum, loading, unavailable, participants, expected) in cases {
        assert_eq!(
            proposal_capacity_summary_state(available, maximum, loading, unavailable, participants,),
            expected
        );
    }
}

#[test]
fn participation_history_maps_only_terminal_stages() {
    for stage in [
        GovernanceProposalStage::SponsorshipExpired,
        GovernanceProposalStage::VoteCallExpired,
    ] {
        assert_eq!(
            proposal_closed_history_kind(stage),
            Some(ProposalClosedHistoryKind::Sponsorship)
        );
    }
    for stage in [
        GovernanceProposalStage::Failed,
        GovernanceProposalStage::PassedAwaitingExecution,
        GovernanceProposalStage::PassedExecutable,
        GovernanceProposalStage::ExecutionExpired,
        GovernanceProposalStage::Executed,
    ] {
        assert_eq!(
            proposal_closed_history_kind(stage),
            Some(ProposalClosedHistoryKind::Voting)
        );
    }
    assert_eq!(
        proposal_closed_history_kind(GovernanceProposalStage::VotingDelay),
        None
    );
}

fn resolved_document() -> ProposalDocumentState {
    resolved_document_with_title("resolved")
}

fn resolved_document_with_title(title: &str) -> ProposalDocumentState {
    ProposalDocumentState::Resolved {
        document: GovernanceDocument {
            title: title.to_string(),
            description: String::new(),
            available: true,
        },
        presentation: ProposalPresentation::RawParseFallback(inert_raw_fallback_source("")),
    }
}

mod call_decoding;
mod decoded_previews;
mod documents;
mod page_state_and_presentation;
mod preview_context;
