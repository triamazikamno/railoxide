use super::*;

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
    let anchor = crate::root::proposals::ChainTimeAnchor {
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
