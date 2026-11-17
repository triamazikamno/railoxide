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

#[test]
fn proposal_block_parser_preserves_all_description_list_items() {
    let source = "Making way for Railgun v3, this proposal:\n\n- Aims to make it easier for others (through a logic contract upgrade) to build & run public broadcaster code, ideally resulting in a more reliable and easier to use broadcaster network.\n- Updates the cyptographic circuits of RAILGUN to improve gas efficiency, remove redundant code for efficiency purposes, and add additional security features.\n- Adds new randomness to the circuits via an additional ceremony.\n- No other changes.\n\nThis will be a full upgrade of the RAILGUN logic contract and circuits if the YES vote is successful.\n\nPlease feel free to read through the code of this RAILGUN v2.5 proposal and vote YES if you support these upgrades.\n";
    let blocks = parse_proposal_blocks(source).expect("description should parse");
    assert_eq!(blocks.len(), 3);
    let ProposalBlock::List(list) = &blocks[1] else {
        panic!("description list should remain a list block");
    };
    assert_eq!(list.items.len(), 4);
    let contents = list
        .items
        .iter()
        .map(|item| match item.blocks.first() {
            Some(ProposalBlock::Markdown(content)) => content.as_str(),
            _ => panic!("list item should retain paragraph content"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        contents,
        vec![
            "Aims to make it easier for others (through a logic contract upgrade) to build & run public broadcaster code, ideally resulting in a more reliable and easier to use broadcaster network.",
            "Updates the cyptographic circuits of RAILGUN to improve gas efficiency, remove redundant code for efficiency purposes, and add additional security features.",
            "Adds new randomness to the circuits via an additional ceremony.",
            "No other changes.",
        ]
    );
}

#[test]
fn proposal_block_parser_keeps_only_effective_referenced_definitions() {
    let source = "[first][guide]\n\n[guide]: https://example.com/first\n[guide]: https://example.com/second\n[unused]: https://example.com/unused\n";
    let blocks = parse_proposal_blocks(source).expect("description should parse");
    assert_eq!(blocks.len(), 1);
    let ProposalBlock::Markdown(content) = &blocks[0] else {
        panic!("reference paragraph should remain a markdown leaf");
    };
    assert!(content.contains("[guide]: https://example.com/first"));
    assert!(!content.contains("https://example.com/second"));
    assert!(!content.contains("https://example.com/unused"));
}

#[test]
fn proposal_block_parser_coalesces_exact_ranges_and_respects_boundaries() {
    let source = "first\n\n[one][guide]\n\n[two][guide]\n\n[guide]: https://example.com/guide\n\n- item\n\n> quote\n\nlast";
    let blocks = parse_proposal_blocks(source).expect("description should parse");
    assert_eq!(blocks.len(), 4);
    let ProposalBlock::Markdown(content) = &blocks[0] else {
        panic!("ordinary nodes should be coalesced into one markdown leaf");
    };
    assert!(content.starts_with("first\n\n"));
    let Node::Root(rendered) =
        to_mdast(content, &ParseOptions::gfm()).expect("coalesced markdown should parse")
    else {
        panic!("coalesced markdown should have a root");
    };
    let visible = visible_node_text(&Node::Root(rendered));
    assert!(visible.contains("one (https://example.com/guide)"));
    assert!(visible.contains("two (https://example.com/guide)"));
    assert_eq!(
        content
            .matches("[guide]: https://example.com/guide")
            .count(),
        1
    );
    assert!(matches!(&blocks[1], ProposalBlock::List(_)));
    assert!(matches!(&blocks[2], ProposalBlock::Blockquote(_)));
    assert_eq!(blocks[3], ProposalBlock::Markdown("last".to_string()));
}

#[test]
fn proposal_block_parser_keeps_tables_as_exact_scoped_boundaries() {
    let source = "before\n\n| Name | Link |\n| --- | --- |\n| one | [guide][guide] |\n| two | ![diagram](https://image.example) |\n\n[guide]: https://example.com/guide\n\nafter";
    let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
        panic!("table description should prepare");
    };
    assert_eq!(prepared.table_count, 1);
    assert_eq!(prepared.blocks.len(), 3);
    assert_eq!(
        prepared.blocks[0],
        ProposalBlock::Markdown("before".to_string())
    );
    let ProposalBlock::Table(table) = &prepared.blocks[1] else {
        panic!("table should remain a structural block");
    };
    assert_eq!(table.ordinal, 0);
    assert!(table.source.starts_with("| Name | Link |\n| --- | --- |"));
    assert!(
        table
            .render_source
            .starts_with("| Name | Link |\n| --- | --- |")
    );
    let Node::Root(rendered) =
        to_mdast(&table.render_source, &ParseOptions::gfm()).expect("render table parses")
    else {
        panic!("render table should have a root");
    };
    assert!(!contains_activatable_markdown(&rendered.children));
    assert!(visible_node_text(&Node::Root(rendered)).contains("diagram (https://image.example)"));
    assert_eq!(
        table
            .source
            .matches("[guide]: https://example.com/guide")
            .count(),
        1
    );
    assert_eq!(
        prepared.blocks[2],
        ProposalBlock::Markdown("after".to_string())
    );
}

#[test]
fn proposal_tables_receive_recursive_source_order_ordinals() {
    let source = "| root |\n| --- |\n| one |\n\n> | quote |\n> | --- |\n> | two |\n\n- | list |\n  | --- |\n  | three |\n";
    let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
        panic!("nested table description should prepare");
    };
    assert_eq!(prepared.table_count, 3);
    let ProposalBlock::Table(root) = &prepared.blocks[0] else {
        panic!("root table should be first");
    };
    let ProposalBlock::Blockquote(quote) = &prepared.blocks[1] else {
        panic!("quote should remain structural");
    };
    let ProposalBlock::Table(quoted) = &quote.blocks[0] else {
        panic!("quoted table should be second");
    };
    let ProposalBlock::List(list) = &prepared.blocks[2] else {
        panic!("list should remain structural");
    };
    let ProposalBlock::Table(listed) = &list.items[0].blocks[0] else {
        panic!("list table should be third");
    };
    assert_eq!((root.ordinal, quoted.ordinal, listed.ordinal), (0, 1, 2));
    assert_eq!(root.source, "| root |\n| --- |\n| one |");
    for table in [root, quoted, listed] {
        let Node::Root(parsed) =
            to_mdast(&table.source, &ParseOptions::gfm()).expect("prepared table should reparse")
        else {
            panic!("standalone table source should produce a root");
        };
        assert_eq!(parsed.children.len(), 1);
        assert!(matches!(parsed.children.first(), Some(Node::Table(_))));
        assert!(matches!(
            parsed.children.first(),
            Some(Node::Table(table)) if table_fingerprint(table).is_some()
        ));
    }
}

#[test]
fn nested_table_normalization_preserves_inline_content_and_effective_definition() {
    let source = "- > | Name | Details |\n  > | --- | --- |\n  > | *one* | `two` [guide][guide] |\n\n[guide]: https://example.com/guide\n";
    let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(source) else {
        panic!("nested table description should prepare");
    };
    let ProposalBlock::List(list) = &prepared.blocks[0] else {
        panic!("list should remain structural");
    };
    let ProposalBlock::Blockquote(quote) = &list.items[0].blocks[0] else {
        panic!("blockquote should remain structural");
    };
    let ProposalBlock::Table(table) = &quote.blocks[0] else {
        panic!("nested table should remain structural");
    };
    assert_eq!(table.source.matches("[guide]:").count(), 1);
    let Node::Root(rendered) =
        to_mdast(&table.render_source, &ParseOptions::gfm()).expect("render table parses")
    else {
        panic!("render table should have a root");
    };
    assert!(!contains_activatable_markdown(&rendered.children));
    let Node::Root(parsed) =
        to_mdast(&table.source, &ParseOptions::gfm()).expect("normalized table should reparse")
    else {
        panic!("normalized table should produce a root");
    };
    assert!(matches!(parsed.children.first(), Some(Node::Table(_))));
    assert!(matches!(parsed.children.get(1), Some(Node::Definition(_))));
    assert_eq!(parsed.children.len(), 2);
}

#[test]
fn proposal_table_intrinsic_width_tracks_content_and_is_bounded() {
    let long_cell = "W".repeat(100);
    let source = format!("| Value |\n| --- |\n| {long_cell} |\n");
    let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(&source) else {
        panic!("long-wide table should prepare");
    };
    let ProposalBlock::Table(table) = &prepared.blocks[0] else {
        panic!("long-wide table should remain structural");
    };
    let expected_content_width = 100 * usize::from(APP_TEXT_SIZE.ceil());
    let expected_width = expected_content_width + TABLE_COLUMN_CHROME_PX + TABLE_OUTER_BORDER_PX;
    assert!(usize::from(table.render_width_px) >= expected_width);
    assert!(usize::from(table.render_width_px) > usize::from(CONTENT_WIDTH));

    let compact_source = "| Name | Link |\n| --- | --- |\n| one | two |\n";
    let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(compact_source)
    else {
        panic!("compact table should prepare");
    };
    let ProposalBlock::Table(table) = &prepared.blocks[0] else {
        panic!("compact table should remain structural");
    };
    assert!(usize::from(table.render_width_px) < usize::from(CONTENT_WIDTH));

    let multibyte_source = "| Value |\n| --- |\n| 世界🌍 |\n";
    let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(multibyte_source)
    else {
        panic!("multibyte table should prepare");
    };
    let ProposalBlock::Table(table) = &prepared.blocks[0] else {
        panic!("multibyte table should remain structural");
    };
    assert!(usize::from(table.render_width_px) <= MAX_TABLE_RENDER_WIDTH_PX);

    let column_count = 40;
    let headers = (0..column_count)
        .map(|column| format!("h{column}"))
        .collect::<Vec<_>>()
        .join(" | ");
    let separators = (0..column_count)
        .map(|_| "---")
        .collect::<Vec<_>>()
        .join(" | ");
    let pathological_cell = "x".repeat(160);
    let cells = (0..column_count)
        .map(|_| pathological_cell.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    let source = format!("| {headers} |\n| {separators} |\n| {cells} |\n");
    let ProposalPresentation::Prepared(prepared) = prepare_proposal_presentation(&source) else {
        panic!("pathological table should prepare");
    };
    let ProposalBlock::Table(table) = &prepared.blocks[0] else {
        panic!("pathological table should remain structural");
    };
    assert_eq!(
        usize::from(table.render_width_px),
        MAX_TABLE_RENDER_WIDTH_PX
    );
}

#[test]
fn proposal_preparation_enforces_render_complexity_at_limit() {
    let mut budget = PreparationBudget::default();
    for _ in 0..85 {
        budget
            .account_markdown(1)
            .expect("85 markdown chunks should use 255 units");
    }
    budget
        .account_list_item()
        .expect("the 256th unit should be admitted");
    assert_eq!(budget.weighted_complexity, 256);
    assert_eq!(
        budget.account_blockquote(),
        Err(ProposalPreparationFailure::TooComplex)
    );
}

#[test]
fn proposal_preparation_accounts_table_complexity_through_production() {
    let source = (0..86)
        .map(|_| "| value |\n| --- |\n\n")
        .collect::<String>();
    assert!(matches!(
        prepare_proposal_presentation(&source),
        ProposalPresentation::TooComplex
    ));
}

#[test]
fn proposal_preparation_rejects_mdast_node_limit_before_building_leaves() {
    let source = "# \n".repeat(MAX_MDAST_NODES);
    let Node::Root(root) = to_mdast(&source, &ParseOptions::gfm()).expect("markdown parses") else {
        panic!("markdown parser should produce a root");
    };
    assert_eq!(
        ensure_mdast_node_limit(&root.children),
        Err(ProposalPreparationFailure::TooComplex)
    );

    let source = "# \n".repeat(MAX_MDAST_NODES - 1);
    let Node::Root(root) = to_mdast(&source, &ParseOptions::gfm()).expect("markdown parses") else {
        panic!("markdown parser should produce a root");
    };
    assert!(ensure_mdast_node_limit(&root.children).is_ok());
    assert!(matches!(
        prepare_proposal_presentation(&"# \n".repeat(MAX_MDAST_NODES)),
        ProposalPresentation::TooComplex
    ));
}

#[test]
fn proposal_preparation_counts_definition_appendix_bytes_cumulatively() {
    let referenced = ["[guide]: https://example.com/guide"];
    let source_len = MAX_PREPARED_SOURCE_BYTES - 40;
    let prepared_len = prepared_markdown_source_len(source_len, &referenced)
        .expect("source length arithmetic should be checked");
    let mut budget = PreparationBudget::default();
    budget
        .account_markdown(prepared_len)
        .expect("the first prepared leaf should fit");
    assert_eq!(budget.prepared_source_bytes, prepared_len);
    assert_eq!(
        budget.account_markdown(20),
        Err(ProposalPreparationFailure::TooComplex)
    );
}

#[test]
fn proposal_block_parser_prepares_lists_nested_in_blockquotes() {
    let source = "> quoted\n>\n> - outer\n>   - inner\n";
    let blocks = parse_proposal_blocks(source).expect("description should parse");
    let ProposalBlock::Blockquote(quote) = &blocks[0] else {
        panic!("quote should remain a structural block");
    };
    let ProposalBlock::List(list) = &quote.blocks[1] else {
        panic!("quoted list should remain a custom list block");
    };
    let ProposalBlock::List(nested) = &list.items[0].blocks[1] else {
        panic!("nested quoted list should remain a custom list block");
    };
    assert_eq!(nested.items.len(), 1);
}

#[test]
fn proposal_destination_entities_are_counted_in_prepared_source_budget() {
    let destination = "/".repeat(MAX_PREPARED_SOURCE_BYTES / 4);
    let source = format!("[label](<{destination}>)");
    assert!(matches!(
        prepare_proposal_presentation(&source),
        ProposalPresentation::TooComplex
    ));
}

#[test]
fn malformed_proposal_fallback_is_selectable_inert_code() {
    let source = "[not closed\n<a href=\"https://example.com\">";
    let fallback = super::inert_raw_fallback_source(source);
    let Node::Root(root) = to_mdast(&fallback, &ParseOptions::gfm()).expect("fallback parses")
    else {
        panic!("fallback should have a root");
    };
    assert!(!contains_activatable_markdown(&root.children));
    assert!(fallback.contains(source));
}

#[test]
fn table_scroll_handles_are_stable_per_proposal_and_ordinal() {
    let mut state = ProposalsState::new(1);
    let v2 = test_proposal(7, ProposalDocumentState::Pending).identity();
    let mut v1 = v2.clone();
    v1.contract_version = GovernanceContractVersion::V1;
    state.ensure_table_scroll_handles(&v2, 2);
    state.ensure_table_scroll_handles(&v1, 1);
    let first_key = proposal_table_scroll_key(&v2, 0);
    let second_key = proposal_table_scroll_key(&v2, 1);
    let v1_key = proposal_table_scroll_key(&v1, 0);
    assert_ne!(first_key, second_key);
    assert_ne!(first_key, v1_key);
    let first_offset = gpui::point(gpui::px(17.0), gpui::px(-3.0));
    state.table_scroll_handles[&first_key].set_offset(first_offset);
    let first_handle = state.table_scroll_handles[&first_key].clone();
    state.ensure_table_scroll_handles(&v2, 2);
    assert_eq!(state.table_scroll_handles.len(), 3);
    assert_eq!(
        state.table_scroll_handles[&first_key].offset(),
        first_offset
    );
    assert_eq!(
        state.table_scroll_handles[&first_key].offset(),
        first_handle.offset()
    );
}

#[test]
fn calldata_display_preserves_exact_copy_source_and_compact_preview() {
    let calldata = alloy::primitives::Bytes::from(vec![0xab; 40]);
    let exact = action_calldata_hex(&calldata);
    assert_eq!(exact, format!("0x{}", "ab".repeat(40)));
    assert_eq!(
        compact_calldata_display(&exact),
        "0xababababababababab…abababababababab"
    );
}

#[test]
fn proposal_decodes_generic_erc20_calls_to_semantic_fields() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let from = address!("0x3333333333333333333333333333333333333333");
    let amount = U256::from(42);
    assert_eq!(
        decode_proposal_action(
            1,
            target,
            &ProposalErc20::transferCall { recipient, amount }.abi_encode(),
            None,
            None,
        ),
        Some(DecodedProposalAction::Erc20Transfer { recipient, amount })
    );
    assert_eq!(
        decode_proposal_action(
            1,
            target,
            &ProposalErc20::transferFromCall {
                from,
                to: recipient,
                amount,
            }
            .abi_encode(),
            None,
            None,
        ),
        Some(DecodedProposalAction::Erc20TransferFrom {
            from,
            to: recipient,
            amount,
        })
    );
    let spender = address!("0x4444444444444444444444444444444444444444");
    assert_eq!(
        decode_proposal_action(
            1,
            target,
            &ProposalErc20::approveCall { spender, amount }.abi_encode(),
            None,
            None,
        ),
        Some(DecodedProposalAction::Erc20Approve { spender, amount })
    );
}

#[test]
fn proposal_decodes_common_governance_calls_with_target_guards() {
    let arbitrary_target = address!("0x1111111111111111111111111111111111111111");
    let proxy = address!("0x2222222222222222222222222222222222222222");
    let implementation = address!("0x3333333333333333333333333333333333333333");
    let account = address!("0x4444444444444444444444444444444444444444");
    let upgrade = ProposalProxyAdmin::upgradeCall {
        proxy,
        implementation,
    }
    .abi_encode();
    let upgrade_decoded = DecodedProposalAction::ProxyUpgrade {
        proxy,
        implementation,
    };
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &upgrade, None, None),
        Some(upgrade_decoded.clone())
    );
    let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    assert_eq!(
        proposal_action_target_label(
            1,
            arbitrary_target,
            &empty_registry,
            None,
            Some(&upgrade_decoded),
        ),
        Some("Proxy admin".to_owned())
    );

    let pause = ProposalProxyAdmin::pauseCall { proxy }.abi_encode();
    assert_eq!(&pause[..4], &[0x76, 0xa6, 0x7a, 0x51]);
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &pause, None, None),
        Some(DecodedProposalAction::ProxyPause { proxy })
    );
    let proxy_owner = ProposalProxyAdmin::transferProxyOwnershipCall {
        proxy,
        newOwner: account,
    }
    .abi_encode();
    assert_eq!(&proxy_owner[..4], &[0x00, 0x36, 0x1d, 0x55]);
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &proxy_owner, None, None),
        Some(DecodedProposalAction::ProxyTransferOwnership {
            proxy,
            new_owner: account,
        })
    );

    let railgun = address!("0x5555555555555555555555555555555555555555");
    let change_fee = ProposalRailgun::changeFeeCall {
        shieldFee: alloy::primitives::Uint::<120, 2>::from_limbs([10, 0]),
        unshieldFee: alloy::primitives::Uint::<120, 2>::from_limbs([10, 0]),
        nftFee: U256::ZERO,
    }
    .abi_encode();
    assert_eq!(&change_fee[..4], &[0xcc, 0x1f, 0x73, 0xfd]);
    assert_eq!(
        decode_proposal_action(1, railgun, &change_fee, None, Some(railgun)),
        Some(DecodedProposalAction::ChangeFee {
            shield_fee: U256::from(10),
            unshield_fee: U256::from(10),
            nft_fee: U256::ZERO,
        })
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &change_fee, None, Some(railgun)),
        None
    );
    let mut trailing_change_fee = change_fee;
    trailing_change_fee.push(0);
    assert_eq!(
        decode_proposal_action(1, railgun, &trailing_change_fee, None, Some(railgun)),
        None
    );

    let contracts = railgun_ui::governance_contracts(1).expect("Ethereum governance");
    let mint = ProposalGovernanceToken::governanceMintCall {
        account,
        amount: U256::from(42),
    }
    .abi_encode();
    assert_eq!(
        decode_proposal_action(1, contracts.governance_token, &mint, None, None),
        Some(DecodedProposalAction::GovernanceMint {
            account,
            amount: U256::from(42),
        })
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &mint, None, None),
        None
    );

    let interval = ProposalGovernorRewards::setIntervalBPCall {
        newIntervalBP: U256::from(420),
    }
    .abi_encode();
    assert_eq!(
        decode_proposal_action(1, contracts.governor_rewards, &interval, None, None),
        Some(DecodedProposalAction::SetIntervalBP {
            new_interval_bp: U256::from(420),
        })
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &interval, None, None),
        None
    );

    let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let add_tokens = ProposalGovernorRewards::addTokensCall {
        tokens: vec![usdt, usdc],
    }
    .abi_encode();
    assert_eq!(&add_tokens[..4], [0x4a, 0xe0, 0x5c, 0x7d]);
    assert_eq!(
        decode_proposal_action(1, contracts.governor_rewards, &add_tokens, None, None),
        Some(DecodedProposalAction::AddTokens {
            tokens: vec![usdt, usdc],
        })
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &add_tokens, None, None),
        None
    );
    let mut malformed_offset = add_tokens.clone();
    malformed_offset[4..36].fill(0);
    assert_eq!(
        decode_proposal_action(1, contracts.governor_rewards, &malformed_offset, None, None),
        None
    );
    let mut trailing_tokens = add_tokens;
    trailing_tokens.push(0);
    assert_eq!(
        decode_proposal_action(1, contracts.governor_rewards, &trailing_tokens, None, None),
        None
    );

    let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
    let contract_address = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
    let selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
    let permission = ProposalDelegator::setPermissionCall {
        caller,
        contractAddress: contract_address,
        selector,
        permission: true,
    }
    .abi_encode();
    assert_eq!(&permission[..4], [0xe6, 0x46, 0x24, 0xfa]);
    let permission_decoded = DecodedProposalAction::DelegatorSetPermission {
        caller,
        contract_address,
        selector,
        permission: true,
    };
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &permission, None, None),
        Some(permission_decoded)
    );
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &permission, None, None),
        None
    );

    let mut malformed_bool = permission.clone();
    malformed_bool[131] = 2;
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &malformed_bool, None, None),
        None
    );
    let mut malformed_address = permission.clone();
    malformed_address[4] = 1;
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &malformed_address, None, None),
        None
    );
    let mut malformed_padding = permission.clone();
    malformed_padding[72] = 1;
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &malformed_padding, None, None),
        None
    );
    let mut trailing_permission = permission.clone();
    trailing_permission.push(0);
    assert_eq!(
        decode_proposal_action(1, contracts.delegator, &trailing_permission, None, None),
        None
    );
    assert_eq!(
        decode_proposal_action(
            1,
            contracts.delegator,
            &permission[..permission.len() - 1],
            None,
            None
        ),
        None
    );

    let mut trailing = upgrade;
    trailing.push(0);
    assert_eq!(
        decode_proposal_action(1, arbitrary_target, &trailing, None, None),
        None
    );
}

#[test]
fn common_governance_decoded_previews_show_parties_tokens_and_rates() {
    let proxy = address!("0x2222222222222222222222222222222222222222");
    let implementation = address!("0x3333333333333333333333333333333333333333");
    let account = address!("0x4444444444444444444444444444444444444444");
    let token = railgun_ui::governance_contracts(1)
        .expect("Ethereum governance")
        .governance_token;
    let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
    let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([
            (
                (1, token.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: token.to_string(),
                    symbol: "RAIL".to_owned(),
                    decimals: 18,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            ),
            (
                (1, usdt.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: usdt.to_string(),
                    symbol: "USDT".to_owned(),
                    decimals: 6,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            ),
            (
                (1, usdc.to_string().to_ascii_lowercase()),
                wallet_ops::settings::EffectiveTokenInfo {
                    chain_id: 1,
                    token_address: usdc.to_string(),
                    symbol: "USDC".to_owned(),
                    decimals: 6,
                    icon_path: None,
                    price_anchor: None,
                    built_in: false,
                },
            ),
        ]),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let upgrade = proposal_decoded_preview(
        &DecodedProposalAction::ProxyUpgrade {
            proxy,
            implementation,
        },
        address!("0x1111111111111111111111111111111111111111"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(upgrade.verb, "Upgrade proxy");
    assert!(matches!(
        upgrade.details.as_slice(),
        [
            ProposalDecodedDetail::Party { role: proxy_role, address: proxy_address, .. },
            ProposalDecodedDetail::Party {
                role: implementation_role,
                address: implementation_address,
                ..
            },
        ] if proxy_role == "PROXY"
            && *proxy_address == proxy
            && implementation_role == "IMPLEMENTATION"
            && *implementation_address == implementation
    ));

    let ownership = proposal_decoded_preview(
        &DecodedProposalAction::TransferOwnership { new_owner: account },
        address!("0x1111111111111111111111111111111111111111"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(ownership.verb, "Transfer ownership");
    assert!(ownership.hero.is_none());
    assert!(matches!(
        &ownership.details[0],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "NEW OWNER" && *address == account
    ));

    let pause = proposal_decoded_preview(
        &DecodedProposalAction::ProxyPause { proxy },
        address!("0x1111111111111111111111111111111111111111"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(pause.verb, "Pause proxy");
    assert!(matches!(
        &pause.details[0],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "PROXY" && *address == proxy
    ));

    let proxy_ownership = proposal_decoded_preview(
        &DecodedProposalAction::ProxyTransferOwnership {
            proxy,
            new_owner: account,
        },
        address!("0x1111111111111111111111111111111111111111"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(proxy_ownership.verb, "Transfer proxy ownership");
    assert!(matches!(
        proxy_ownership.details.as_slice(),
        [
            ProposalDecodedDetail::Party { role: proxy_role, address: proxy_address, .. },
            ProposalDecodedDetail::Party { role: owner_role, address: owner_address, .. },
        ] if proxy_role == "PROXY"
            && *proxy_address == proxy
            && owner_role == "NEW OWNER"
            && *owner_address == account
    ));

    let fees = proposal_decoded_preview(
        &DecodedProposalAction::ChangeFee {
            shield_fee: U256::from(10),
            unshield_fee: U256::from(10),
            nft_fee: U256::ZERO,
        },
        address!("0x5555555555555555555555555555555555555555"),
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(fees.verb, "Change protocol fees");
    assert!(fees.hero.is_none());
    assert!(matches!(
        fees.details.as_slice(),
        [
            ProposalDecodedDetail::Value { role: shield_role, value: shield_value, copy_value: Some(shield_copy), .. },
            ProposalDecodedDetail::Value { role: unshield_role, value: unshield_value, copy_value: Some(unshield_copy), .. },
            ProposalDecodedDetail::Value { role: nft_role, value: nft_value, copy_value: Some(nft_copy), .. },
        ] if shield_role == "SHIELD FEE"
            && shield_value == "0.10% (10 bp)"
            && shield_copy == "10"
            && unshield_role == "UNSHIELD FEE"
            && unshield_value == "0.10% (10 bp)"
            && unshield_copy == "10"
            && nft_role == "NFT FEE"
            && nft_value == "0 ETH (0 wei)"
            && nft_copy == "0"
    ));

    let mint = proposal_decoded_preview(
        &DecodedProposalAction::GovernanceMint {
            account,
            amount: U256::from(10_000_000_000_000_000_000_000_u128),
        },
        token,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(mint.verb, "Mint governance tokens");
    assert_eq!(
        mint.hero.as_ref().map(|hero| hero.rounded.as_str()),
        Some("10000 RAIL")
    );
    assert!(matches!(
        &mint.details[0],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "TO" && *address == account
    ));

    let interval = proposal_decoded_preview(
        &DecodedProposalAction::SetIntervalBP {
            new_interval_bp: U256::from(420),
        },
        Address::ZERO,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(interval.verb, "Set reward interval rate");
    assert!(matches!(
        &interval.details[0],
        ProposalDecodedDetail::Value {
            role,
            value,
            copy_value: Some(copy_value),
            monospace,
        } if role == "RATE"
            && value == "4.20% (420 bp)"
            && copy_value == "420"
            && !monospace
    ));

    let add_tokens = proposal_decoded_preview(
        &DecodedProposalAction::AddTokens {
            tokens: vec![usdt, usdc],
        },
        railgun_ui::governance_contracts(1)
            .expect("Ethereum governance")
            .governor_rewards,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(add_tokens.verb, "Add reward tokens");
    assert!(add_tokens.hero.is_none());
    assert!(matches!(
        add_tokens.details.as_slice(),
        [
            ProposalDecodedDetail::Party {
                role: first_role,
                address: first_address,
                badge: Some(first_badge),
                ..
            },
            ProposalDecodedDetail::Party {
                role: second_role,
                address: second_address,
                badge: Some(second_badge),
                ..
            },
        ] if first_role == "TOKEN"
            && *first_address == usdt
            && first_badge == "USDT"
            && second_role == "TOKEN"
            && *second_address == usdc
            && second_badge == "USDC"
    ));

    let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
    let contract_address = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
    let known_selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
    let grant = proposal_decoded_preview(
        &DecodedProposalAction::DelegatorSetPermission {
            caller,
            contract_address,
            selector: known_selector,
            permission: true,
        },
        railgun_ui::governance_contracts(1)
            .expect("Ethereum governance")
            .delegator,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(grant.verb, "Grant call permission");
    assert!(grant.hero.is_none());
    assert!(matches!(
        grant.details.as_slice(),
        [
            ProposalDecodedDetail::Party { role: caller_role, address: caller_value, .. },
            ProposalDecodedDetail::Party {
                role: contract_role,
                address: contract_value,
                ..
            },
            ProposalDecodedDetail::Value {
                role: function_role,
                value,
                copy_value: Some(copy_value),
                monospace,
            },
        ] if caller_role == "CALLER"
            && *caller_value == caller
            && contract_role == "CONTRACT"
            && *contract_value == contract_address
            && function_role == "FUNCTION"
            && value == "setVerificationKey · 0x2ec0f359"
            && copy_value == "0x2ec0f359"
            && *monospace
    ));

    let unknown_selector = FixedBytes::from([0xaa, 0xbb, 0xcc, 0xdd]);
    let revoke = proposal_decoded_preview(
        &DecodedProposalAction::DelegatorSetPermission {
            caller,
            contract_address,
            selector: unknown_selector,
            permission: false,
        },
        Address::ZERO,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(revoke.verb, "Revoke call permission");
    assert!(matches!(
        &revoke.details[2],
        ProposalDecodedDetail::Value {
            role,
            value,
            copy_value: Some(copy_value),
            ..
        } if role == "FUNCTION" && value == "0xaabbccdd" && copy_value == "0xaabbccdd"
    ));
}

#[test]
fn delegator_wildcard_preview_details_preserve_specific_counterpart() {
    let caller = address!("0x64DA0892E8E24fECa6Eb5E3D8cbf2D9b6Fbe7598");
    let contract = address!("0xFA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
    let known_selector = FixedBytes::from([0x2e, 0xc0, 0xf3, 0x59]);
    let zero_selector = FixedBytes::from([0; 4]);
    let zero_copy = Address::ZERO.to_checksum(None);
    let contract_copy = contract.to_checksum(None);
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let cases = [
        (
            "zero contract only",
            Address::ZERO,
            known_selector,
            true,
            "Any contract · 0x0000…0000",
            zero_copy.as_str(),
            "setVerificationKey · 0x2ec0f359",
            "0x2ec0f359",
        ),
        (
            "zero selector only",
            contract,
            zero_selector,
            false,
            "",
            contract_copy.as_str(),
            "Any function · 0x00000000",
            "0x00000000",
        ),
        (
            "both zero",
            Address::ZERO,
            zero_selector,
            true,
            "Any contract · 0x0000…0000",
            zero_copy.as_str(),
            "Any function · 0x00000000",
            "0x00000000",
        ),
    ];

    for (
        label,
        contract_address,
        selector,
        wildcard_contract,
        expected_contract_value,
        expected_contract_copy,
        expected_function_value,
        expected_function_copy,
    ) in cases
    {
        let preview = proposal_decoded_preview(
            &DecodedProposalAction::DelegatorSetPermission {
                caller,
                contract_address,
                selector,
                permission: true,
            },
            Address::ZERO,
            U256::ZERO,
            1,
            &anchor_rates,
            &registry,
            None,
            &[],
            None,
        );
        assert!(
            matches!(
                &preview.details[0],
                ProposalDecodedDetail::Party {
                    role,
                    address,
                    copy_value,
                    ..
                } if role == "CALLER"
                    && *address == caller
                    && copy_value == caller.to_checksum(None).as_str()
            ),
            "{label}: caller detail"
        );

        match (&preview.details[1], wildcard_contract) {
            (
                ProposalDecodedDetail::Value {
                    role,
                    value,
                    copy_value: Some(copy_value),
                    monospace,
                },
                true,
            ) if role == "CONTRACT"
                && value == expected_contract_value
                && copy_value == expected_contract_copy
                && !monospace => {}
            (
                ProposalDecodedDetail::Party {
                    role,
                    address,
                    copy_value,
                    ..
                },
                false,
            ) if role == "CONTRACT"
                && *address == contract
                && copy_value == expected_contract_copy => {}
            _ => panic!("{label}: unexpected contract detail"),
        }

        assert!(
            matches!(
                &preview.details[2],
                ProposalDecodedDetail::Value {
                    role,
                    value,
                    copy_value: Some(copy_value),
                    monospace,
                } if role == "FUNCTION"
                    && value == expected_function_value
                    && copy_value == expected_function_copy
                    && *monospace
            ),
            "{label}: function detail"
        );
    }
}

#[test]
fn proposal_decoded_preview_uses_typed_token_and_party_presentation() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, target.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: target.to_string(),
                symbol: "TEST".to_owned(),
                decimals: 6,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let transfer = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(15_109_211_424_u64),
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        Some(&[]),
    );
    let hero = transfer.hero.as_ref().expect("known token hero");
    assert_eq!(hero.rounded, "15109 TEST");
    assert!(hero.icon.is_none(), "custom icon-less symbols have no slot");
    assert!(hero.amount_copy_value.is_none());
    assert!(hero.context_copy_value.is_none());
    assert_eq!(transfer.details.len(), 1);
    assert!(matches!(
        &transfer.details[0],
        ProposalDecodedDetail::Party { role, address, badge, .. }
            if role == "TO" && *address == recipient && badge.is_none()
    ));

    let unknown_token = address!("0x9999999999999999999999999999999999999999");
    let unknown = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(42),
        },
        unknown_token,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    let unknown_hero = unknown.hero.as_ref().expect("unknown token hero");
    assert!(unknown_hero.icon.is_none());
    assert_eq!(unknown_hero.rounded, "42 raw token units");
    assert_eq!(unknown_hero.amount_copy_value.as_deref(), Some("42"));
    let unknown_token_short = railgun_ui::short_address(&unknown_token);
    assert!(
        unknown_hero
            .context
            .as_deref()
            .is_some_and(|context| context.contains(unknown_token_short.as_str()))
    );
    let unknown_token_checksum = unknown_token.to_checksum(None);
    assert_eq!(
        unknown_hero.context_copy_value.as_deref(),
        Some(unknown_token_checksum.as_str())
    );
}

#[test]
fn proposal_decoded_preview_adds_only_nonredundant_cached_usd_context() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, target.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: target.to_string(),
                symbol: "WBTC".to_owned(),
                decimals: 8,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    anchor_rates.store_rate(1, target, U256::from(100_000_000_u64));
    anchor_rates.store_native_usd_rate(1, U256::from(13_194_600_000_u64), 18);
    let priced = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(100_000_000_u64),
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(
        priced
            .hero
            .as_ref()
            .and_then(|hero| hero.context.as_deref()),
        Some("≈ $13,194.60")
    );

    let stable_target = address!("0x3333333333333333333333333333333333333333");
    let stable_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, stable_target.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: stable_target.to_string(),
                symbol: "USDC".to_owned(),
                decimals: 6,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    let stable_rates = TokenAnchorRateCache::new();
    stable_rates.store_rate(1, stable_target, U256::from(1_000_000_000_u64));
    stable_rates.store_native_usd_rate(1, U256::from(1_000_000_000_u64), 18);
    let stable = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(10_000_000_u64),
        },
        stable_target,
        U256::ZERO,
        1,
        &stable_rates,
        &stable_registry,
        None,
        &[],
        None,
    );
    assert!(
        stable
            .hero
            .as_ref()
            .is_some_and(|hero| hero.context.is_none())
    );

    let missing_rates = TokenAnchorRateCache::new();
    let missing = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Transfer {
            recipient,
            amount: U256::from(100_000_000_u64),
        },
        target,
        U256::ZERO,
        1,
        &missing_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(
        missing
            .hero
            .as_ref()
            .is_some_and(|hero| hero.context.is_none())
    );
}

#[test]
fn proposal_decoded_preview_preserves_direction_treasury_badges_and_approval_warning() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let recipient = address!("0x2222222222222222222222222222222222222222");
    let from = address!("0x3333333333333333333333333333333333333333");
    let spender = address!("0x52908400098527886e0f7030069857d2e4169ee7");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::from([(
            (1, target.to_string().to_ascii_lowercase()),
            wallet_ops::settings::EffectiveTokenInfo {
                chain_id: 1,
                token_address: target.to_string(),
                symbol: "TEST".to_owned(),
                decimals: 2,
                icon_path: None,
                price_anchor: None,
                built_in: false,
            },
        )]),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let transfer_from = proposal_decoded_preview(
        &DecodedProposalAction::Erc20TransferFrom {
            from,
            to: recipient,
            amount: U256::from(1),
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(matches!(
        transfer_from.details.as_slice(),
        [
            ProposalDecodedDetail::Party { role: from_role, address: from_address, .. },
            ProposalDecodedDetail::Connector,
            ProposalDecodedDetail::Party { role: to_role, address: to_address, .. },
        ] if from_role == "FROM" && *from_address == from && to_role == "TO" && *to_address == recipient
    ));

    let treasury = railgun_ui::governance_treasury(1).expect("ethereum treasury metadata");
    let treasury_transfer = proposal_decoded_preview(
        &DecodedProposalAction::TreasuryTransferErc20 {
            token: target,
            to: recipient,
            amount: U256::from(1),
        },
        treasury,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(matches!(
        &treasury_transfer.details[0],
        ProposalDecodedDetail::Party { role, address, badge: Some(badge), .. }
            if role == "FROM" && *address == treasury && badge == "Treasury"
    ));

    let bounded = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Approve {
            spender,
            amount: U256::from(1),
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(bounded.unlimited_warning.is_none());
    assert!(!bounded.hero.as_ref().expect("approval hero").danger);

    let unlimited = proposal_decoded_preview(
        &DecodedProposalAction::Erc20Approve {
            spender,
            amount: U256::MAX,
        },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    let unlimited_hero = unlimited.hero.as_ref().expect("approval hero");
    assert_eq!(unlimited_hero.rounded, "Unlimited TEST");
    assert!(unlimited_hero.danger);
    let expected_warning = format!(
        "Unlimited allowance: {} can keep spending this token until the allowance is revoked.",
        spender.to_checksum(None)
    );
    assert_eq!(
        unlimited.unlimited_warning.as_deref(),
        Some(expected_warning.as_str())
    );
    assert!(matches!(
        &unlimited.details[0],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "SPENDER" && *address == spender
    ));
}

#[test]
fn proposal_decoded_preview_covers_wrapping_and_role_shells() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let account = address!("0x2222222222222222222222222222222222222222");
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let wrapped = proposal_decoded_preview(
        &DecodedProposalAction::WrappedDeposit,
        target,
        U256::from(1_000_000_000_000_000_000_u128),
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    let wrapped_hero = wrapped.hero.as_ref().expect("wrap hero");
    assert_eq!(wrapped.verb, "Wrap");
    assert_eq!(wrapped_hero.rounded, "1 ETH");
    assert_eq!(wrapped_hero.context.as_deref(), Some("wrapped into WETH"));

    let role = B256::repeat_byte(7);
    let role_preview = proposal_decoded_preview(
        &DecodedProposalAction::TreasuryGrantRole { role, account },
        target,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert_eq!(role_preview.details.len(), 2);
    assert!(matches!(
        &role_preview.details[0],
        ProposalDecodedDetail::Value { role: label, value, .. }
            if label == "ROLE" && value == &proposal_role_label(role)
    ));
    assert!(matches!(
        &role_preview.details[1],
        ProposalDecodedDetail::Party { role, address, .. }
            if role == "ACCOUNT" && *address == account
    ));
}

#[test]
fn proposal_decoded_preview_retains_exact_address_copy_fidelity() {
    let first = address!("0x123400000000000000000000000000000000abcd");
    let second = address!("0x123411111111111111111111111111111111abcd");
    assert_eq!(
        railgun_ui::short_address(&first),
        railgun_ui::short_address(&second)
    );
    let registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let anchor_rates = TokenAnchorRateCache::new();
    let preview = proposal_decoded_preview(
        &DecodedProposalAction::TreasuryInitialize { owner: first },
        Address::ZERO,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(matches!(
        &preview.details[0],
        ProposalDecodedDetail::Party { address, copy_value, .. }
            if *address == first && copy_value == &first.to_checksum(None)
    ));
    let other = proposal_decoded_preview(
        &DecodedProposalAction::TreasuryInitialize { owner: second },
        Address::ZERO,
        U256::ZERO,
        1,
        &anchor_rates,
        &registry,
        None,
        &[],
        None,
    );
    assert!(matches!(
        &other.details[0],
        ProposalDecodedDetail::Party { address, copy_value, .. }
            if *address == second && copy_value == &second.to_checksum(None)
    ));
}

#[test]
fn proposal_decodes_all_treasury_writes_only_at_known_target() {
    let treasury = railgun_ui::governance_treasury(1).expect("ethereum treasury metadata");
    let other = address!("0x1111111111111111111111111111111111111111");
    let token = address!("0x2222222222222222222222222222222222222222");
    let account = address!("0x3333333333333333333333333333333333333333");
    let role = B256::repeat_byte(7);
    let calls = [
        (
            ProposalTreasury::transferERC20Call {
                token,
                to: account,
                amount: U256::from(1),
            }
            .abi_encode(),
            DecodedProposalAction::TreasuryTransferErc20 {
                token,
                to: account,
                amount: U256::from(1),
            },
        ),
        (
            ProposalTreasury::transferETHCall {
                to: account,
                amount: U256::from(2),
            }
            .abi_encode(),
            DecodedProposalAction::TreasuryTransferEth {
                to: account,
                amount: U256::from(2),
            },
        ),
        (
            ProposalTreasury::initializeTreasuryCall { owner: account }.abi_encode(),
            DecodedProposalAction::TreasuryInitialize { owner: account },
        ),
        (
            ProposalTreasury::grantRoleCall { role, account }.abi_encode(),
            DecodedProposalAction::TreasuryGrantRole { role, account },
        ),
        (
            ProposalTreasury::revokeRoleCall { role, account }.abi_encode(),
            DecodedProposalAction::TreasuryRevokeRole { role, account },
        ),
        (
            ProposalTreasury::renounceRoleCall { role, account }.abi_encode(),
            DecodedProposalAction::TreasuryRenounceRole { role, account },
        ),
    ];
    let arbitrum_treasury =
        railgun_ui::governance_treasury(42161).expect("arbitrum treasury metadata");
    assert_eq!(
        decode_proposal_action(42161, arbitrum_treasury, &calls[0].0, None, None),
        Some(calls[0].1.clone())
    );
    for (calldata, expected) in calls {
        assert_eq!(
            decode_proposal_action(1, treasury, &calldata, None, None),
            Some(expected)
        );
        assert_eq!(
            decode_proposal_action(1, other, &calldata, None, None),
            None
        );
    }
}

#[test]
fn proposal_role_labels_cover_known_treasury_roles() {
    assert_eq!(proposal_role_label(B256::ZERO), "DEFAULT_ADMIN_ROLE");
    assert_eq!(
        proposal_role_label(alloy::primitives::keccak256(b"TRANSFER_ROLE")),
        "TRANSFER_ROLE"
    );
    let unknown = B256::repeat_byte(7);
    assert_eq!(proposal_role_label(unknown), unknown.to_string());
}

#[test]
fn proposal_decodes_sender_writes_at_any_target_and_labels_decoded_actions() {
    let first_target = address!("0x1111111111111111111111111111111111111111");
    let second_target = address!("0x2222222222222222222222222222222222222222");
    let executor = address!("0x3333333333333333333333333333333333333333");
    let empty_registry = wallet_ops::settings::EffectiveTokenRegistry {
        tokens: BTreeMap::new(),
    };
    let ready_task = ProposalOpStackSender::readyTaskCall {
        taskId: U256::from(1),
    }
    .abi_encode();
    let ready_task_decoded = DecodedProposalAction::OpStackReadyTask {
        task_id: U256::from(1),
    };
    assert_eq!(
        decode_proposal_action(1, first_target, &ready_task, None, None),
        Some(ready_task_decoded.clone())
    );
    assert_eq!(
        proposal_action_target_label(
            1,
            first_target,
            &empty_registry,
            None,
            Some(&ready_task_decoded),
        ),
        Some("OpStack sender".to_owned())
    );

    let treasury = railgun_ui::governance_treasury(1).expect("mainnet treasury metadata");
    assert_eq!(
        proposal_action_target_label(
            1,
            treasury,
            &empty_registry,
            None,
            Some(&ready_task_decoded),
        ),
        Some("Treasury".to_owned())
    );

    let delegator = railgun_ui::governance_contracts(1)
        .expect("mainnet governance metadata")
        .delegator;
    assert_eq!(
        proposal_action_target_label(
            1,
            delegator,
            &empty_registry,
            None,
            Some(&ready_task_decoded),
        ),
        Some("Delegator".to_owned())
    );

    let set_executor = ProposalOpStackSender::setExecutorL2Call { executor }.abi_encode();
    let set_executor_decoded = DecodedProposalAction::OpStackSetExecutorL2 { executor };
    assert_eq!(
        decode_proposal_action(42161, second_target, &set_executor, None, None),
        Some(set_executor_decoded.clone())
    );
    assert_eq!(
        proposal_action_target_label(
            42161,
            second_target,
            &empty_registry,
            None,
            Some(&set_executor_decoded),
        ),
        Some("OpStack sender".to_owned())
    );

    let transfer_ownership =
        ProposalOwnable::transferOwnershipCall { newOwner: executor }.abi_encode();
    assert_eq!(&transfer_ownership[..4], &[0xf2, 0xfd, 0xe3, 0x8b]);
    assert_eq!(
        decode_proposal_action(1, first_target, &transfer_ownership, None, None),
        Some(DecodedProposalAction::TransferOwnership {
            new_owner: executor
        })
    );
    assert_eq!(
        proposal_action_target_label(
            1,
            first_target,
            &empty_registry,
            None,
            Some(&DecodedProposalAction::TransferOwnership {
                new_owner: executor
            }),
        ),
        None
    );
    let renounce_ownership = ProposalOwnable::renounceOwnershipCall {}.abi_encode();
    assert_eq!(
        decode_proposal_action(42161, second_target, &renounce_ownership, None, None),
        None
    );
}

#[test]
fn proposal_decodes_wrapped_native_calls_only_at_configured_target() {
    let wrapped = address!("0x1111111111111111111111111111111111111111");
    let other = address!("0x2222222222222222222222222222222222222222");
    let deposit = ProposalWrappedNative::depositCall {}.abi_encode();
    assert_eq!(
        decode_proposal_action(1, wrapped, &deposit, Some(wrapped), None),
        Some(DecodedProposalAction::WrappedDeposit)
    );
    assert_eq!(
        decode_proposal_action(1, other, &deposit, Some(wrapped), None),
        None
    );
    let amount = U256::from(9);
    let withdraw = ProposalWrappedNative::withdrawCall { amount }.abi_encode();
    assert_eq!(
        decode_proposal_action(1, wrapped, &withdraw, Some(wrapped), None),
        Some(DecodedProposalAction::WrappedWithdraw { amount })
    );
}

#[test]
fn proposal_keeps_unknown_malformed_and_trailing_calls_undecoded() {
    let target = address!("0x1111111111111111111111111111111111111111");
    let mut trailing = ProposalErc20::transferCall {
        recipient: target,
        amount: U256::from(1),
    }
    .abi_encode();
    trailing.push(0);
    assert_eq!(
        decode_proposal_action(1, target, &trailing, None, None),
        None
    );
    assert_eq!(
        decode_proposal_action(1, target, &[0xa9, 0x05], None, None),
        None
    );
    assert_eq!(
        decode_proposal_action(1, target, &[0xff; 36], None, None),
        None
    );
}

#[test]
fn action_control_ids_are_unique_per_proposal_ordinal_and_control() {
    let identity = test_proposal(7, resolved_document()).identity();
    let first = proposal_action_id(&identity, 0, "calldata", "clipboard");
    let second = proposal_action_id(&identity, 1, "calldata", "clipboard");
    let display = proposal_action_id(&identity, 0, "calldata", "display");
    assert_ne!(first, second);
    assert_ne!(first, display);
}

#[test]
fn refresh_prunes_action_expansion_only_for_removed_ordinals() {
    let mut state = ProposalsState::new(1);
    let mut source = test_proposal(7, resolved_document());
    source.proposal.actions = vec![
        GovernanceProposalAction {
            call_contract: Address::ZERO,
            calldata: alloy::primitives::Bytes::from(vec![1]),
            value: U256::ZERO,
        },
        GovernanceProposalAction {
            call_contract: Address::ZERO,
            calldata: alloy::primitives::Bytes::from(vec![2]),
            value: U256::ZERO,
        },
    ];
    let identity = source.identity();
    state.pages.insert(
        0,
        ProposalsPage {
            rows: Arc::new(vec![source]),
        },
    );
    state.expanded_calldata.extend([
        ProposalActionIdentity {
            proposal: identity.clone(),
            ordinal: 0,
        },
        ProposalActionIdentity {
            proposal: identity.clone(),
            ordinal: 1,
        },
    ]);
    let mut refreshed = test_proposal(7, ProposalDocumentState::Pending);
    refreshed.proposal.actions = vec![GovernanceProposalAction {
        call_contract: Address::ZERO,
        calldata: alloy::primitives::Bytes::from(vec![1]),
        value: U256::ZERO,
    }];
    state.replace_refreshed_page(0, 0, vec![refreshed]);
    assert!(state.expanded_calldata.contains(&ProposalActionIdentity {
        proposal: identity.clone(),
        ordinal: 0,
    }));
    assert!(!state.expanded_calldata.contains(&ProposalActionIdentity {
        proposal: identity,
        ordinal: 1,
    }));
}

#[test]
fn prefetch_selects_only_the_immediately_next_page() {
    assert_eq!(next_proposal_page(3, 10), Some(4));
    assert_eq!(next_proposal_page(8, 10), Some(9));
    assert_eq!(next_proposal_page(9, 10), None);

    let mut state = ProposalsState::new(1);
    state.current_page = 3;
    state.total_pages = 5;
    state.pages.insert(
        3,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(1, resolved_document())]),
        },
    );
    assert_eq!(state.prefetch_candidate(3), Some(4));

    state.pages.insert(
        3,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(1, ProposalDocumentState::Pending)]),
        },
    );
    assert_eq!(state.prefetch_candidate(3), None);

    state.current_page = 4;
    state.pages.insert(
        4,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(1, resolved_document())]),
        },
    );
    assert_eq!(state.prefetch_candidate(4), None);
}

#[test]
fn rail_amounts_preserve_18_decimal_scaling() {
    let amount = U256::from(1_500_000_000_000_000_000u128);
    assert_eq!(format_compact_rail_amount_with_unit(amount), "1.5 RAIL");

    let rounded = U256::from(12_345_600_000_000_000_000u128);
    assert_eq!(format_compact_rail_amount_with_unit(rounded), "12.35 RAIL");
    assert_eq!(
        format_compact_rail_amount(U256::from(27_000u128) * U256::from(10).pow(U256::from(18))),
        "27K"
    );
    assert_eq!(
        format_compact_rail_amount(U256::from(242_879u128) * U256::from(10).pow(U256::from(18))),
        "242.8K"
    );
    assert_eq!(
        format_compact_rail_amount_with_unit(
            U256::from(2_043_001u128) * U256::from(10).pow(U256::from(18))
        ),
        "2.04M RAIL"
    );
}

#[test]
fn vote_split_uses_raw_base_unit_votes() {
    let point_nine_rail = U256::from(900_000_000_000_000_000u128);
    let point_eight_rail = U256::from(800_000_000_000_000_000u128);
    let one_rail = U256::from(1_000_000_000_000_000_000u128);

    assert_eq!(vote_split(U256::ZERO, U256::ZERO), None);
    assert_eq!(vote_split(point_nine_rail, one_rail), Some(473));
    assert_eq!(vote_split(point_nine_rail, point_eight_rail), Some(529));
}

#[test]
fn chain_time_anchor_current_at_uses_sampled_time() {
    let anchor = super::ChainTimeAnchor {
        chain_time: U256::from(100),
        captured_at: Instant::now(),
    };
    assert_eq!(anchor.current_at(0), U256::from(100));
    assert_eq!(anchor.current_at(60), U256::from(160));
}

#[test]
fn list_deadline_tracks_each_voting_phase() {
    let deadlines = GovernanceProposalDeadlines {
        sponsorship: U256::ZERO,
        voting_start: Some(U256::from(10)),
        yay_end: Some(U256::from(20)),
        nay_end: Some(U256::from(30)),
        execution_start: Some(U256::from(40)),
        execution_end: Some(U256::from(50)),
    };
    let mut status = GovernanceProposalStatus {
        stage: GovernanceProposalStage::VotingOpen,
        deadlines,
        quorum_basis: wallet_ops::GovernanceQuorumBasis::AffirmativeOnly,
        quorum: U256::ZERO,
        quorum_progress: U256::ZERO,
        quorum_met: false,
        majority: wallet_ops::GovernanceMajorityResult::Tie,
    };
    assert_eq!(
        list_voting_deadline(&status),
        Some((U256::from(20), "Yay voting ends in"))
    );
    status.stage = GovernanceProposalStage::NayOnlyVoting;
    assert_eq!(
        list_voting_deadline(&status),
        Some((U256::from(30), "Nay voting ends in"))
    );
}

#[test]
fn timeline_deadline_boundaries_match_protocol_semantics() {
    assert!(timeline_sponsorship_completed(
        true,
        U256::from(9),
        U256::from(10)
    ));
    assert!(!timeline_sponsorship_completed(
        false,
        U256::from(9),
        U256::from(10)
    ));
    assert!(timeline_sponsorship_completed(
        false,
        U256::from(10),
        U256::from(10)
    ));

    for milestone in [
        TimelineDeadline::SponsorshipClose,
        TimelineDeadline::YayVotingEnd,
        TimelineDeadline::NayVotingEnd,
        TimelineDeadline::ExecutionClose,
    ] {
        assert!(timeline_deadline_completed(
            milestone,
            U256::from(10),
            U256::from(10)
        ));
        assert!(!timeline_deadline_completed(
            milestone,
            U256::from(9),
            U256::from(10)
        ));
    }
    for milestone in [
        TimelineDeadline::VotingOpen,
        TimelineDeadline::ExecutionOpen,
    ] {
        assert!(!timeline_deadline_completed(
            milestone,
            U256::from(10),
            U256::from(10)
        ));
        assert!(timeline_deadline_completed(
            milestone,
            U256::from(11),
            U256::from(10)
        ));
    }
}

#[test]
fn canceled_page_work_token_rejects_reused_page_ownership() {
    let mut state = ProposalsState::new(1);
    state.prefetch_page = Some(1);
    let old_prefetch_token = state.prefetch_token;
    assert!(state.owns_page_work(1, true, old_prefetch_token));
    state.cancel_prefetch();
    state.prefetch_page = Some(1);
    assert!(!state.owns_page_work(1, true, old_prefetch_token));
    assert!(state.owns_page_work(1, true, state.prefetch_token));

    state.current_page = 1;
    let old_active_token = state.active_page_token;
    state.cancel_active_page(1);
    assert!(!state.owns_page_work(1, false, old_active_token));
    assert!(state.owns_page_work(1, false, state.active_page_token));
}

#[test]
fn evicting_an_acted_proposal_drops_its_pages_and_page_work_that_may_restore_it() {
    let acted = test_proposal(5, resolved_document()).identity();
    let mut state = ProposalsState::new(1);
    state.pages.insert(
        0,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(5, resolved_document())]),
        },
    );
    state.pages.insert(
        1,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(0, resolved_document())]),
        },
    );
    state.current_page = 1;
    state.prefetch_page = Some(0);
    let active_token = state.active_page_token;
    let prefetch_token = state.prefetch_token;

    // An action on another chain leaves the cache alone.
    assert!(!state.evict_proposal(2, &acted));
    assert_eq!(state.pages.len(), 2);

    // The other proposal's current page stays with its work; the prefetch is cancelled.
    assert!(!state.evict_proposal(1, &acted));
    assert!(state.rows(0).is_none());
    assert!(state.rows(1).is_some());
    assert!(!state.owns_page_work(0, true, prefetch_token));
    assert!(state.owns_page_work(1, false, active_token));

    // A current page still loading may carry the proposal's stale data, so its load cannot
    // land and the page must be requested again.
    state.current_page = 2;
    state.loading_pages.insert(2);
    assert!(state.evict_proposal(1, &acted));
    assert!(!state.owns_page_work(2, false, active_token));
    assert!(state.loading_pages.is_empty());

    // So must a dropped current page.
    state.pages.insert(
        2,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(5, resolved_document())]),
        },
    );
    let active_token = state.active_page_token;
    assert!(state.evict_proposal(1, &acted));
    assert!(state.rows(2).is_none());
    assert!(!state.owns_page_work(2, false, active_token));
}

#[test]
fn take_cleanup_invalidates_work_and_preserves_cached_ui_state() {
    let mut state = ProposalsState::new(42);
    state.generation = 7;
    state.request_generation = 11;
    state.active_page_token = 13;
    state.prefetch_token = 17;
    state.checked = true;
    state.overview = Some(GovernanceOverview {
        chain_id: 42,
        v2: GovernanceContractSummary {
            version: GovernanceContractVersion::V2,
            address: Address::ZERO,
            proposal_count: U256::from(10),
            rules: GovernanceContractRules {
                sponsor_threshold: U256::from(2),
                quorum: U256::from(1),
                sponsor_window: U256::from(1),
                voting_start_offset: U256::from(1),
                voting_yay_end_offset: U256::from(2),
                voting_nay_end_offset: U256::from(3),
                execution_start_offset: U256::from(4),
                execution_end_offset: U256::from(5),
            },
        },
        v1: None,
    });
    state.total_pages = 2;
    state.current_page = 1;
    state.pages.insert(
        1,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(3, resolved_document())]),
        },
    );
    state.loading_pages.extend([1, 2]);
    state.hydrating_pages.insert(1);
    state.prefetch_page = Some(2);
    state.loading = true;
    state.refreshing = true;
    state.error = Some(Arc::from("cached error"));
    let identity = test_proposal(3, resolved_document()).identity();
    state.selected = Some(ProposalSelection {
        page: 1,
        identity,
        tab: ProposalDetailTab::Actions,
        participation_expanded: false,
    });

    let old_generation = state.generation;
    let old_request_generation = state.request_generation;
    let old_active_token = state.active_page_token;
    let old_prefetch_token = state.prefetch_token;
    assert!(state.owns_page_work(1, false, old_active_token));
    assert!(state.owns_page_work(2, true, old_prefetch_token));
    let cached_rows = state.pages[&1].rows.clone();
    let overview = state.overview.clone();
    let selected = state.selected.clone();
    let document_semaphore = Arc::clone(&state.document_semaphore);
    let cleanup = state.take_cleanup();

    assert_eq!(state.generation, old_generation.wrapping_add(1));
    assert_eq!(
        state.request_generation,
        old_request_generation.wrapping_add(1)
    );
    assert_eq!(state.active_page_token, old_active_token.wrapping_add(1));
    assert_eq!(state.prefetch_token, old_prefetch_token.wrapping_add(1));
    assert!(!state.owns_page_work(1, false, old_active_token));
    assert!(!state.owns_page_work(2, true, old_prefetch_token));
    assert!(state.loading_pages.is_empty());
    assert!(state.hydrating_pages.is_empty());
    assert_eq!(state.prefetch_page, None);
    assert!(!state.loading);
    assert!(!state.refreshing);
    assert_eq!(state.chain_id, 42);
    assert!(state.checked);
    assert_eq!(state.overview, overview);
    assert_eq!(state.total_pages, 2);
    assert_eq!(state.current_page, 1);
    assert!(Arc::ptr_eq(&state.pages[&1].rows, &cached_rows));
    assert!(Arc::ptr_eq(&state.document_semaphore, &document_semaphore));
    assert_eq!(state.error.as_deref(), Some("cached error"));
    assert_eq!(
        state.selected.as_ref().map(|selected| selected.page),
        selected.as_ref().map(|selected| selected.page)
    );
    assert_eq!(
        state.selected.as_ref().map(|selected| &selected.identity),
        selected.as_ref().map(|selected| &selected.identity)
    );
    assert!(cleanup.tasks.is_empty());
}

#[test]
fn pending_hydration_selection_keeps_only_pending_rows() {
    let mut state = ProposalsState::new(1);
    state.pages.insert(
        2,
        ProposalsPage {
            rows: Arc::new(vec![
                test_proposal(1, ProposalDocumentState::Pending),
                test_proposal(2, resolved_document()),
            ]),
        },
    );

    let rows = state
        .pending_document_rows(2)
        .expect("cached page should be selectable");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].has_pending_document());
    assert!(state.pending_document_rows(3).is_none());
}

#[test]
fn pending_rows_distinguish_terminal_and_uncached_pages() {
    let mut state = ProposalsState::new(1);
    state.pages.insert(
        2,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(1, ProposalDocumentState::Pending)]),
        },
    );
    state.pages.insert(
        4,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(2, resolved_document())]),
        },
    );

    assert!(!state.page_is_terminal(2));
    assert!(state.page_is_terminal(4));
    assert!(
        state
            .pending_document_rows(4)
            .is_some_and(|rows| rows.is_empty())
    );
    assert!(state.pending_document_rows(6).is_none());
}

#[test]
fn manual_refresh_keeps_active_page_and_retries_only_unavailable_documents() {
    let mut state = ProposalsState::new(1);
    state.current_page = 3;
    state.pages.insert(
        1,
        ProposalsPage {
            rows: Arc::new(vec![test_proposal(1, resolved_document())]),
        },
    );
    state.pages.insert(
        2,
        ProposalsPage {
            rows: Arc::new(vec![
                test_proposal(10, resolved_document_with_title("cached destination")),
                test_proposal(12, resolved_document()),
            ]),
        },
    );
    state.pages.insert(
        3,
        ProposalsPage {
            rows: Arc::new(vec![
                test_proposal(10, resolved_document()),
                test_proposal(
                    11,
                    ProposalDocumentState::Resolved {
                        document: GovernanceDocument {
                            title: "Document unavailable".to_string(),
                            description: String::new(),
                            available: false,
                        },
                        presentation: ProposalPresentation::RawParseFallback(
                            inert_raw_fallback_source(""),
                        ),
                    },
                ),
            ]),
        },
    );
    state.loading_pages.extend([1, 2, 3, 4]);
    state.hydrating_pages.extend([2, 3]);
    let selected_identity = test_proposal(10, resolved_document()).identity();
    state.selected = Some(ProposalSelection {
        page: 3,
        identity: selected_identity.clone(),
        tab: ProposalDetailTab::Actions,
        participation_expanded: false,
    });
    let expanded_identity = ProposalActionIdentity {
        proposal: selected_identity.clone(),
        ordinal: 0,
    };
    state.expanded_calldata.insert(expanded_identity.clone());
    let detail_offset = gpui::point(gpui::px(9.0), gpui::px(-21.0));
    state.detail_scroll_handle.set_offset(detail_offset);

    state.prepare_manual_refresh();
    assert!(state.pages[&3].rows[0].document().is_some());
    assert!(state.pages[&3].rows[1].has_pending_document());

    state.replace_refreshed_page(3, 2, {
        let mut selected_refreshed = test_proposal(10, ProposalDocumentState::Pending);
        selected_refreshed
            .proposal
            .actions
            .push(GovernanceProposalAction {
                call_contract: Address::ZERO,
                calldata: alloy::primitives::Bytes::from(vec![1]),
                value: U256::ZERO,
            });
        vec![
            selected_refreshed,
            test_proposal(11, ProposalDocumentState::Pending),
            test_proposal(12, ProposalDocumentState::Pending),
        ]
    });

    assert_eq!(state.current_page, 2);
    assert_eq!(state.pages.len(), 1);
    assert!(state.pages.contains_key(&2));
    assert!(!state.pages.contains_key(&1));
    assert!(!state.pages.contains_key(&3));
    assert!(state.loading_pages.is_empty());
    assert!(state.hydrating_pages.is_empty());
    assert_eq!(
        state.selected.as_ref().map(|selected| &selected.identity),
        Some(&selected_identity)
    );
    assert_eq!(
        state.selected.as_ref().map(|selected| selected.page),
        Some(2)
    );
    assert_eq!(
        state.selected.as_ref().map(|selected| selected.tab),
        Some(ProposalDetailTab::Actions)
    );
    assert_eq!(state.detail_scroll_handle.offset(), detail_offset);
    assert!(state.expanded_calldata.contains(&expanded_identity));
    assert_eq!(
        state.pages[&2].rows[0]
            .document()
            .map(|document| document.title.as_str()),
        Some("resolved")
    );
    assert!(state.pages[&2].rows[1].has_pending_document());
    assert!(state.pages[&2].rows[2].has_pending_document());
}

#[tokio::test]
async fn proposal_cleanup_aborts_and_awaits_held_workers() {
    struct DropSentinel(Arc<AtomicBool>);
    impl Drop for DropSentinel {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let active_dropped = Arc::new(AtomicBool::new(false));
    let prefetch_dropped = Arc::new(AtomicBool::new(false));
    let lifecycle_dropped = Arc::new(AtomicBool::new(false));
    let active_sentinel = DropSentinel(Arc::clone(&active_dropped));
    let prefetch_sentinel = DropSentinel(Arc::clone(&prefetch_dropped));
    let lifecycle_sentinel = DropSentinel(Arc::clone(&lifecycle_dropped));
    let active_worker = tokio::spawn(async move {
        let _sentinel = active_sentinel;
        std::future::pending::<()>().await;
    });
    let prefetch_worker = tokio::spawn(async move {
        let _sentinel = prefetch_sentinel;
        std::future::pending::<()>().await;
    });
    let lifecycle_worker = tokio::spawn(async move {
        let _sentinel = lifecycle_sentinel;
        std::future::pending::<()>().await;
    });
    let mut state = ProposalsState::new(1);
    state.task_tracker.track(lifecycle_worker);
    state.active_page_task_tracker.track(active_worker);
    state.prefetch_task_tracker.track(prefetch_worker);
    let cleanup = state.take_cleanup();
    cleanup
        .shutdown()
        .await
        .expect("cleanup should cancel normally");
    assert!(active_dropped.load(Ordering::Acquire));
    assert!(prefetch_dropped.load(Ordering::Acquire));
    assert!(lifecycle_dropped.load(Ordering::Acquire));
}

#[tokio::test]
async fn proposal_cleanup_drops_in_flight_document_futures() {
    struct DropSentinel(Arc<AtomicBool>);
    impl Drop for DropSentinel {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let futures = futures_util::stream::FuturesUnordered::new();
    let future_dropped = Arc::clone(&dropped);
    let future_started = Arc::clone(&started);
    let future_notify = Arc::clone(&notify);
    futures.push(async move {
        future_started.store(true, Ordering::Release);
        future_notify.notify_one();
        let _sentinel = DropSentinel(future_dropped);
        std::future::pending::<DocumentCompletion>().await
    });
    let (completion_tx, _completion_rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = tokio::spawn(send_document_completions(futures, completion_tx));
    notify.notified().await;
    assert!(started.load(Ordering::Acquire));
    assert!(!dropped.load(Ordering::Acquire));

    let mut state = ProposalsState::new(1);
    state.active_page_task_tracker.track(worker);
    let cleanup = state.take_cleanup();
    cleanup
        .shutdown()
        .await
        .expect("cleanup should cancel normally");
    assert!(dropped.load(Ordering::Acquire));
}
