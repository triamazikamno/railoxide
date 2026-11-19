use super::*;

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
    let fallback = crate::root::proposals::inert_raw_fallback_source(source);
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
