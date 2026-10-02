use super::*;

#[test]
fn display_rows_reverse_utxo_order() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 3,
        unspent_count: 3,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            utxo_output("0x1111111111111111111111111111111111111111", "1", false),
            utxo_output("0x2222222222222222222222222222222222222222", "2", false),
            utxo_output("0x3333333333333333333333333333333333333333", "3", false),
        ],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", true);
    let amounts: Vec<_> = rows.iter().map(|row| row.amount.as_str()).collect();
    assert_eq!(amounts, ["3", "2", "1"]);
}

#[test]
fn display_rows_include_spent_utxos() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 1,
        spent_count: 1,
        local_pending_spent_count: 0,
        utxos: vec![
            utxo_output("0x1111111111111111111111111111111111111111", "42", true),
            utxo_output("0x2222222222222222222222222222222222222222", "7", false),
        ],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", true);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].amount, "7");
    assert!(!rows[0].is_spent);
    assert_eq!(rows[0].spent_tx_hash, None);
    assert_eq!(rows[1].amount, "42");
    assert!(rows[1].is_spent);
    assert_eq!(
        rows[1].spent_tx_hash.as_deref(),
        Some("0x2222222222222222222222222222222222222222222222222222222222222222")
    );
}

#[test]
fn display_rows_hide_spent_utxos_when_toggle_off() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 1,
        spent_count: 1,
        local_pending_spent_count: 0,
        utxos: vec![
            utxo_output("0x1111111111111111111111111111111111111111", "42", true),
            utxo_output("0x2222222222222222222222222222222222222222", "7", false),
        ],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", false);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].amount, "7");
    assert!(!rows[0].is_spent);
}

#[test]
fn display_rows_keep_pending_spent_visible_when_spent_toggle_off() {
    let mut pending_spent = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    pending_spent.pending_spent = true;
    pending_spent.poi_spendable = false;
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![pending_spent],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", false);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].amount, "42");
    assert!(rows[0].pending_spent);
    assert_eq!(rows[0].poi_status, "Pending spend");
}

#[test]
fn recoverable_poi_candidate_count_only_counts_actionable_transact_outputs() {
    let mut missing = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    missing.poi_spendable = false;
    missing.poi_statuses = BTreeMap::from([(
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
        "Missing".to_string(),
    )]);
    missing.ppoi_state = UtxoPpoiState::Missing;
    let mut shield = missing.clone();
    shield.commitment_kind = "Shield".to_string();
    let mut pending = missing.clone();
    pending.pending_new = true;
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 4,
        unspent_count: 4,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            missing,
            shield,
            pending,
            utxo_output("0x2222222222222222222222222222222222222222", "7", false),
        ],
        totals: Vec::new(),
    };

    assert_eq!(recoverable_poi_candidate_count(&output), 1);
}

#[test]
fn ppoi_row_retry_and_submission_age_visibility_includes_actionable_states_and_excludes_spent() {
    for state in [
        UtxoPpoiState::Missing,
        UtxoPpoiState::Unknown,
        UtxoPpoiState::ProofSubmitted,
    ] {
        let mut output = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
        output.ppoi_state = state;
        output.poi_spendable = false;
        output.ppoi_last_submission_at = Some(1_700_000_000);
        let row = display_rows_from_output(
            &ListUtxosOutput {
                chain_id: 1,
                cache_key: "cache".to_string(),
                utxo_count: 1,
                unspent_count: 1,
                spent_count: 0,
                local_pending_spent_count: 0,
                utxos: vec![output],
                totals: Vec::new(),
            },
            "",
            true,
        )
        .remove(0);
        assert!(should_show_ppoi_retry_action(&row), "state {state:?}");
        assert!(should_show_ppoi_submission_age(&row), "state {state:?}");

        let mut spent = row.clone();
        spent.is_spent = true;
        assert!(!should_show_ppoi_retry_action(&spent));
        assert!(!should_show_ppoi_submission_age(&spent));

        let pending_overlays: [fn(&mut UtxoDisplayRow); 3] = [
            |row: &mut UtxoDisplayRow| row.pending_new = true,
            |row: &mut UtxoDisplayRow| row.pending_spent = true,
            |row: &mut UtxoDisplayRow| row.local_pending_spent = true,
        ];
        for apply_pending in pending_overlays {
            let mut pending = row.clone();
            apply_pending(&mut pending);
            assert!(!should_show_ppoi_retry_action(&pending));
            assert!(!should_show_ppoi_submission_age(&pending));
        }

        if state == UtxoPpoiState::Missing {
            let mut no_timestamp = row.clone();
            no_timestamp.ppoi_last_submission_at = None;
            assert!(!should_show_ppoi_submission_age(&no_timestamp));

            let mut zero_timestamp = row.clone();
            zero_timestamp.ppoi_last_submission_at = Some(0);
            assert!(!should_show_ppoi_submission_age(&zero_timestamp));

            let mut non_private = row.clone();
            non_private.activity_classification = "Shield".to_string();
            assert!(!should_show_ppoi_submission_age(&non_private));
        }
    }

    for state in [UtxoPpoiState::Valid, UtxoPpoiState::ShieldBlocked] {
        let mut row = display_rows_from_output(
            &ListUtxosOutput {
                chain_id: 1,
                cache_key: "cache".to_string(),
                utxo_count: 1,
                unspent_count: 1,
                spent_count: 0,
                local_pending_spent_count: 0,
                utxos: vec![utxo_output(
                    "0x1111111111111111111111111111111111111111",
                    "42",
                    false,
                )],
                totals: Vec::new(),
            },
            "",
            true,
        )
        .remove(0);
        row.ppoi_state = state;
        assert!(!should_show_ppoi_retry_action(&row));
    }
}

#[test]
fn ppoi_state_details_use_concise_user_copy() {
    assert_eq!(
        ppoi_state_detail(UtxoPpoiState::Missing),
        "No proof has been submitted for this output yet. Retrying usually resolves it."
    );
    assert_eq!(
        ppoi_state_detail(UtxoPpoiState::ProofSubmitted),
        "Submitted, awaiting verification."
    );
    assert_eq!(
        ppoi_state_detail(UtxoPpoiState::Unknown),
        "Status not yet checked."
    );
    assert_eq!(
        ppoi_row_state_detail_with_submission(UtxoPpoiState::Missing, false, None),
        "No proof has been submitted for this output yet. Retrying usually resolves it."
    );
    assert_eq!(
        ppoi_row_state_detail_with_submission(UtxoPpoiState::Missing, false, Some(1)),
        "Submission acknowledged; verification is pending."
    );
}

#[test]
fn global_sender_retry_requires_actionable_work() {
    let snapshot = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 0,
        unspent_count: 0,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: Vec::new(),
        totals: Vec::new(),
    };

    assert_eq!(recoverable_poi_candidate_count(&snapshot), 0);
    assert!(!global_poi_retry_available(true, false, 0, 0));
    assert!(!global_poi_retry_available(false, false, 1, 0));
    assert!(global_poi_retry_available(true, false, 1, 0));
    assert!(global_poi_retry_available(true, true, 1, 0));
    assert!(global_poi_retry_available(true, true, 0, 1));
    assert_eq!(poi_retry_button_label(false), "Retry PPOI submissions");
    assert_eq!(poi_retry_button_label(true), "Queue PPOI retry");
    assert_eq!(ppoi_row_retry_label(false), "Retry");
    assert_eq!(ppoi_row_retry_label(true), "Queue retry");
}

#[test]
fn ppoi_workflow_status_copy_tracks_automatic_and_actionable_states() {
    let healthy = wallet_ops::WalletPpoiWorkflowStatus::default();
    assert_eq!(ppoi_workflow_status_title(healthy, false), None);
    assert_eq!(ppoi_workflow_status_title(healthy, true), None);
    assert_eq!(
        ppoi_workflow_status_detail(healthy),
        "Checking proofs from the sending wallet."
    );

    let pending = wallet_ops::WalletPpoiWorkflowStatus {
        awaiting_recovery: 3,
        awaiting_public_txid_data: 0,
        awaiting_poi_data: 0,
        retrying_recovery: 0,
        recovery_needs_attention: 0,
        awaiting_submission: 1,
        awaiting_validation: 2,
        needs_attention: 0,
        validation_revision: 0,
    };
    assert_eq!(
        ppoi_workflow_status_title(pending, false),
        Some("Outgoing proof recovery pending")
    );
    assert_eq!(
        ppoi_workflow_status_title(pending, true),
        Some("Recovering outgoing proofs…")
    );
    assert_eq!(
        ppoi_workflow_status_detail(pending),
        "3 PPOIs awaiting recovery · 1 PPOI awaiting submission · 2 PPOIs awaiting verification"
    );

    let attention = wallet_ops::WalletPpoiWorkflowStatus {
        needs_attention: 1,
        ..pending
    };
    assert_eq!(
        ppoi_workflow_status_title(attention, false),
        Some("PPOI submission needs attention")
    );
    let public_wait = wallet_ops::WalletPpoiWorkflowStatus {
        awaiting_recovery: 2,
        awaiting_public_txid_data: 2,
        ..wallet_ops::WalletPpoiWorkflowStatus::default()
    };
    assert_eq!(
        ppoi_workflow_status_title(public_wait, false),
        Some("Waiting for public transaction proof data")
    );
    assert_eq!(
        ppoi_workflow_status_detail(public_wait),
        "2 PPOIs waiting for public transaction data"
    );
    assert!(ppoi_workflow_status_detail(attention).contains("1 PPOI needs attention"));
}

#[test]
fn spent_valid_ppoi_detail_is_historical_not_spendable() {
    assert_eq!(
        ppoi_row_state_detail_with_submission(UtxoPpoiState::Valid, true, None),
        "Verified — already spent."
    );
    assert!(
        !ppoi_row_state_detail_with_submission(UtxoPpoiState::Valid, true, None)
            .contains("spendable")
    );
    assert!(
        ppoi_row_state_detail_with_submission(UtxoPpoiState::Valid, false, None)
            .contains("spendable")
    );
}

#[test]
fn display_rows_include_activity_classification() {
    let mut blocked_shield = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    blocked_shield.commitment_kind = "Shield".to_string();
    blocked_shield.activity_classification = "Blocked Shield".to_string();
    blocked_shield.poi_statuses = BTreeMap::from([(
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
        "ShieldBlocked".to_string(),
    )]);
    blocked_shield.ppoi_state = UtxoPpoiState::ShieldBlocked;
    blocked_shield.poi_spendable = false;
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![blocked_shield],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", false);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].activity_classification, "Blocked Shield");
    assert_eq!(rows[0].poi_status, "ShieldBlocked");
    assert_eq!(rows[0].source_block_number, 11);
}

#[test]
fn pending_receive_finality_tracks_depth_and_indexing() {
    let mut pending = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    pending.pending_new = true;
    pending.source_block_number = 100;
    let row = display_rows_from_output(
        &ListUtxosOutput {
            chain_id: 1,
            cache_key: "cache".to_string(),
            utxo_count: 1,
            unspent_count: 1,
            spent_count: 0,
            local_pending_spent_count: 0,
            utxos: vec![pending],
            totals: Vec::new(),
        },
        "",
        false,
    )
    .remove(0);

    for (context, expected) in [
        (
            UtxoFinalityContext::new(Some(100), Some(88), Some(12)),
            Some("0/12 blocks"),
        ),
        (
            UtxoFinalityContext::new(Some(105), Some(93), Some(12)),
            Some("5/12 blocks"),
        ),
        (
            UtxoFinalityContext::new(Some(112), Some(100), Some(12)),
            Some("Indexing"),
        ),
        (UtxoFinalityContext::new(Some(99), Some(87), Some(12)), None),
    ] {
        assert_eq!(
            pending_finality_display(&row, context)
                .as_ref()
                .map(|(label, _)| label.as_str()),
            expected
        );
    }
}

#[test]
fn pending_spend_finality_tracks_depth_and_skips_local_locks() {
    let mut pending = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    pending.pending_new = true;
    pending.source_block_number = 20;
    pending.pending_spent = true;
    pending.spent_block_number = Some(100);
    let row = display_rows_from_output(
        &ListUtxosOutput {
            chain_id: 1,
            cache_key: "cache".to_string(),
            utxo_count: 1,
            unspent_count: 1,
            spent_count: 0,
            local_pending_spent_count: 0,
            utxos: vec![pending],
            totals: Vec::new(),
        },
        "",
        false,
    )
    .remove(0);

    for (context, expected) in [
        (
            UtxoFinalityContext::new(Some(100), Some(88), Some(12)),
            Some("0/12 blocks"),
        ),
        (
            UtxoFinalityContext::new(Some(105), Some(93), Some(12)),
            Some("5/12 blocks"),
        ),
        (
            UtxoFinalityContext::new(Some(112), Some(100), Some(12)),
            Some("Indexing"),
        ),
    ] {
        assert_eq!(
            pending_finality_display(&row, context)
                .as_ref()
                .map(|(label, _)| label.as_str()),
            expected
        );
    }
    assert!(
        pending_finality_display(
            &row,
            UtxoFinalityContext::new(Some(105), Some(93), Some(12))
        )
        .is_some_and(|(_, detail)| detail.starts_with("Pending spend:"))
    );

    let mut local = row;
    local.pending_new = false;
    local.pending_spent = false;
    local.local_pending_spent = true;
    local.spent_block_number = None;
    assert_eq!(
        pending_finality_display(
            &local,
            UtxoFinalityContext::new(Some(105), Some(93), Some(12))
        ),
        None
    );
}

#[test]
fn shield_poi_wait_display_counts_down_then_expires() {
    let mut shield = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    shield.activity_classification = "Shield".to_string();
    shield.ppoi_state = UtxoPpoiState::Missing;
    shield.poi_spendable = false;
    shield.source_block_timestamp = 1_000;
    let row = display_rows_from_output(
        &ListUtxosOutput {
            chain_id: 1,
            cache_key: "cache".to_string(),
            utxo_count: 1,
            unspent_count: 1,
            spent_count: 0,
            local_pending_spent_count: 0,
            utxos: vec![shield],
            totals: Vec::new(),
        },
        "",
        false,
    )
    .remove(0);

    assert_eq!(
        shield_poi_wait_display(&row, 1_000).map(|display| display.label),
        Some("~1h".to_string())
    );
    assert_eq!(
        shield_poi_wait_display(&row, 1_061).map(|display| display.label),
        Some("~59m".to_string())
    );
    assert_eq!(
        shield_poi_wait_display(&row, 4_599).map(|display| display.label),
        Some("~1m".to_string())
    );
    assert_eq!(
        shield_poi_wait_display(&row, 4_600).map(|display| display.label),
        Some("Taking longer than usual".to_string())
    );
}

#[test]
fn shield_poi_wait_display_preserves_authoritative_states() {
    let mut shield = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    shield.activity_classification = "Shield".to_string();
    shield.ppoi_state = UtxoPpoiState::Missing;
    shield.poi_spendable = false;
    let row = display_rows_from_output(
        &ListUtxosOutput {
            chain_id: 1,
            cache_key: "cache".to_string(),
            utxo_count: 1,
            unspent_count: 1,
            spent_count: 0,
            local_pending_spent_count: 0,
            utxos: vec![shield],
            totals: Vec::new(),
        },
        "",
        false,
    )
    .remove(0);

    let mut private_output = row.clone();
    private_output.activity_classification = "Private Output".to_string();
    assert!(shield_poi_wait_display(&private_output, row.source_block_timestamp).is_none());

    let mut valid = row.clone();
    valid.ppoi_state = UtxoPpoiState::Valid;
    valid.poi_spendable = true;
    assert!(shield_poi_wait_display(&valid, row.source_block_timestamp).is_none());

    let mut blocked = row.clone();
    blocked.activity_classification = "Blocked Shield".to_string();
    blocked.ppoi_state = UtxoPpoiState::ShieldBlocked;
    assert!(shield_poi_wait_display(&blocked, row.source_block_timestamp).is_none());

    let mut pending = row.clone();
    pending.pending_new = true;
    assert!(shield_poi_wait_display(&pending, row.source_block_timestamp).is_none());
}

#[test]
fn activity_classification_icon_styles_match_kinds() {
    assert_eq!(
        activity_classification_icon_style("Shield"),
        (
            ui::icons::shield_plus_icon_path(),
            ui::theme::SUCCESS,
            "Shield"
        )
    );
    assert_eq!(
        activity_classification_icon_style("Private Output"),
        (
            ui::icons::shield_check_icon_path(),
            ui::theme::TEXT,
            "Private Output",
        )
    );
    assert_eq!(
        activity_classification_icon_style("Blocked Shield"),
        (
            ui::icons::shield_alert_icon_path(),
            ui::theme::DANGER,
            "Blocked Shield",
        )
    );
}

#[test]
fn display_rows_include_blocked_shield_rescue_metadata() {
    let mut blocked_shield = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    blocked_shield.commitment_kind = "Shield".to_string();
    blocked_shield.activity_classification = "Blocked Shield".to_string();
    blocked_shield.poi_statuses = BTreeMap::from([(
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
        "ShieldBlocked".to_string(),
    )]);
    blocked_shield.ppoi_state = UtxoPpoiState::ShieldBlocked;
    blocked_shield.poi_spendable = false;
    blocked_shield.blocked_shield_rescue = Some(BlockedShieldRescueInfo {
        eligible: true,
        disabled_reason: None,
        origin_address: Some("0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa".to_string()),
        public_account_uuid: Some("pub-1".to_string()),
        public_account_label: Some("Origin".to_string()),
    });
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![blocked_shield],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", false);

    assert_eq!(rows.len(), 1);
    assert!(rows[0].utxo_id.is_some());
    let rescue = rows[0]
        .blocked_shield_rescue
        .as_ref()
        .expect("rescue metadata");
    assert!(rescue.eligible);
    assert_eq!(rescue.public_account_uuid.as_deref(), Some("pub-1"));
    assert!(should_show_blocked_shield_refund_action(&rows[0]));

    let mut non_blocked = rows[0].clone();
    non_blocked.poi_status = "Valid".to_string();
    assert!(!should_show_blocked_shield_refund_action(&non_blocked));
}

#[test]
fn blocked_shield_rescue_row_state_tracks_resolution_generation() {
    let disabled = BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some("retry later".to_string()),
        origin_address: None,
        public_account_uuid: None,
        public_account_label: None,
    };
    let disabled_state = BlockedShieldRescueRowState::from_info(disabled);

    assert!(!disabled_state.is_resolving());
    assert!(!disabled_state.accepts_lookup_result(7));

    let eligible = BlockedShieldRescueInfo {
        eligible: true,
        disabled_reason: None,
        origin_address: Some("0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa".to_string()),
        public_account_uuid: Some("pub-1".to_string()),
        public_account_label: None,
    };
    let eligible_state = BlockedShieldRescueRowState::from_info(eligible);

    assert!(!eligible_state.is_resolving());

    let resolving = BlockedShieldRescueRowState::resolving(7);

    assert!(resolving.is_resolving());
    assert!(resolving.accepts_lookup_result(7));
    assert!(!resolving.accepts_lookup_result(8));
}

#[test]
fn resolving_blocked_shield_refund_is_loading_not_unavailable() {
    let mut blocked_shield = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    blocked_shield.commitment_kind = "Shield".to_string();
    blocked_shield.activity_classification = "Blocked Shield".to_string();
    blocked_shield.poi_statuses = BTreeMap::from([(
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
        "ShieldBlocked".to_string(),
    )]);
    blocked_shield.ppoi_state = UtxoPpoiState::ShieldBlocked;
    blocked_shield.poi_spendable = false;
    blocked_shield.blocked_shield_rescue = Some(BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: None,
        origin_address: None,
        public_account_uuid: None,
        public_account_label: None,
    });
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![blocked_shield],
        totals: Vec::new(),
    };

    let mut rows = display_rows_from_output(&output, "", false);
    let utxo_id = rows[0].utxo_id.expect("blocked Shield id");

    apply_blocked_shield_rescue_rows(
        &mut rows,
        &BTreeMap::from([(utxo_id, BlockedShieldRescueRowState::resolving(7))]),
        &BTreeSet::new(),
    );

    assert!(should_show_blocked_shield_refund_action(&rows[0]));
    assert!(blocked_shield_refund_origin_resolving(&rows[0]));
    assert!(!blocked_shield_refund_action_available(&rows[0]));
}

#[test]
fn cached_blocked_shield_rescue_does_not_reenable_spent_row() {
    let mut blocked_shield = utxo_output("0x1111111111111111111111111111111111111111", "42", true);
    blocked_shield.commitment_kind = "Shield".to_string();
    blocked_shield.activity_classification = "Blocked Shield".to_string();
    blocked_shield.poi_statuses = BTreeMap::from([(
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
        "ShieldBlocked".to_string(),
    )]);
    blocked_shield.ppoi_state = UtxoPpoiState::ShieldBlocked;
    blocked_shield.poi_spendable = false;
    blocked_shield.blocked_shield_rescue = Some(BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some("Spent blocked Shield UTXOs cannot be refunded.".to_string()),
        origin_address: None,
        public_account_uuid: None,
        public_account_label: None,
    });
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 0,
        spent_count: 1,
        local_pending_spent_count: 0,
        utxos: vec![blocked_shield],
        totals: Vec::new(),
    };

    let mut rows = display_rows_from_output(&output, "", true);
    let utxo_id = rows[0].utxo_id.expect("blocked Shield id");
    let eligible = BlockedShieldRescueInfo {
        eligible: true,
        disabled_reason: None,
        origin_address: Some("0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa".to_string()),
        public_account_uuid: Some("pub-1".to_string()),
        public_account_label: Some("Origin".to_string()),
    };
    let rescue_rows = BTreeMap::from([(utxo_id, BlockedShieldRescueRowState::from_info(eligible))]);

    apply_blocked_shield_rescue_rows(&mut rows, &rescue_rows, &BTreeSet::new());

    let rescue = rows[0]
        .blocked_shield_rescue
        .as_ref()
        .expect("rescue metadata");
    assert!(!rescue.eligible);
    assert_eq!(
        rescue.disabled_reason.as_deref(),
        Some("Spent blocked Shield UTXOs cannot be refunded.")
    );
}

#[test]
fn in_flight_blocked_shield_refund_disables_cached_action() {
    let mut blocked_shield = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    blocked_shield.commitment_kind = "Shield".to_string();
    blocked_shield.activity_classification = "Blocked Shield".to_string();
    blocked_shield.poi_statuses = BTreeMap::from([(
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
        "ShieldBlocked".to_string(),
    )]);
    blocked_shield.ppoi_state = UtxoPpoiState::ShieldBlocked;
    blocked_shield.poi_spendable = false;
    blocked_shield.blocked_shield_rescue = Some(BlockedShieldRescueInfo {
        eligible: true,
        disabled_reason: None,
        origin_address: Some("0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa".to_string()),
        public_account_uuid: Some("pub-1".to_string()),
        public_account_label: Some("Origin".to_string()),
    });
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![blocked_shield],
        totals: Vec::new(),
    };

    let mut rows = display_rows_from_output(&output, "", false);
    let utxo_id = rows[0].utxo_id.expect("blocked Shield id");

    apply_blocked_shield_rescue_rows(&mut rows, &BTreeMap::new(), &BTreeSet::from([utxo_id]));

    let rescue = rows[0]
        .blocked_shield_rescue
        .as_ref()
        .expect("rescue metadata");
    assert!(!rescue.eligible);
    assert_eq!(
        rescue.disabled_reason.as_deref(),
        Some("Blocked Shield refund submission is already in progress.")
    );
}

#[test]
fn blocked_shield_refund_with_unmatched_origin_is_not_actionable() {
    let mut blocked_shield = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    blocked_shield.commitment_kind = "Shield".to_string();
    blocked_shield.activity_classification = "Blocked Shield".to_string();
    blocked_shield.poi_statuses = BTreeMap::from([(
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
        "ShieldBlocked".to_string(),
    )]);
    blocked_shield.ppoi_state = UtxoPpoiState::ShieldBlocked;
    blocked_shield.poi_spendable = false;
    blocked_shield.blocked_shield_rescue = Some(BlockedShieldRescueInfo {
        eligible: false,
        disabled_reason: Some("origin is not in Public".to_string()),
        origin_address: Some("0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa".to_string()),
        public_account_uuid: None,
        public_account_label: None,
    });
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![blocked_shield],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", false);

    // A resolved origin keeps the disabled reason instead of offering another resolution.
    assert!(should_show_blocked_shield_refund_action(&rows[0]));
    assert!(!blocked_shield_refund_action_available(&rows[0]));
}

#[gpui::test]
fn hardware_blocked_shield_refund_approves_stealth_origin_gas_on_the_device(
    cx: &mut gpui::TestAppContext,
) {
    use crate::root::spend_authorization::SpendAuthorizationIntent;

    let path = temp_wallet_db_root("blocked-shield-refund-route");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let entered = runtime.enter();
    cx.update(gpui_component::init);
    let mut root = None;
    let (host, cx) = cx.add_window_view(|window, cx| {
        let wallet = super::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let operation = wallet_ops::vault::ExecutorOperationId::random().unwrap();
    let utxo_id = |position| BlockedShieldRescueUtxoId {
        tree: 0,
        position,
        commitment: alloy::primitives::FixedBytes::repeat_byte(1),
        blinded_commitment: alloy::primitives::FixedBytes::repeat_byte(2),
    };
    let (stealth_origin, software_origin) = (utxo_id(1), utxo_id(2));
    root.update(cx, |root, _| {
        root.wallet_options = vec![WalletOption {
            wallet_id: Arc::from("preview"),
            source: WalletSource::LedgerDerived,
        }];
        let mut stealth = root.public_accounts[0].clone();
        stealth.public_account_uuid = "stealth".to_string();
        stealth.source = PublicAccountSource::ExecutorDerived(
            serde_json::from_value(serde_json::json!({
                "chain_id": 1,
                "index": 5,
                "operation": operation,
            }))
            .unwrap(),
        );
        root.public_accounts.push(stealth);
        for (utxo_id, uuid) in [(stealth_origin, "stealth"), (software_origin, "account-0")] {
            root.blocked_shield_rescue_rows.insert(
                utxo_id,
                BlockedShieldRescueRowState::from_info(BlockedShieldRescueInfo {
                    eligible: true,
                    disabled_reason: None,
                    origin_address: Some("0xaAaAaAaaAaAaAaaAaAAAAAAAAaaaAaAaAaaAaaAa".to_string()),
                    public_account_uuid: Some(uuid.to_string()),
                    public_account_label: None,
                }),
            );
        }

        // A stealth origin has no vault key on a hardware wallet, so the device approves
        // its gas payment and no password step runs.
        let intent = root.blocked_shield_refund_authorization_intent(stealth_origin);
        assert!(matches!(
            intent,
            SpendAuthorizationIntent::BlockedShieldRefund(id) if id == stealth_origin
        ));
        assert!(matches!(
            intent.hardware_executor_action(root),
            Some(wallet_ops::HardwareExecutorAction::GasPayment { account, operation: approved })
                if account == "stealth" && approved == operation
        ));

        let intent = root.blocked_shield_refund_authorization_intent(software_origin);
        assert!(matches!(
            intent,
            SpendAuthorizationIntent::BlockedShieldRefundGasPassword(id) if id == software_origin
        ));
        assert!(
            SpendAuthorizationIntent::BlockedShieldRefund(software_origin)
                .hardware_executor_action(root)
                .is_none()
        );
    });
    cx.update(|window, _| window.remove_window());
    drop(host);
    drop(root);
    cx.run_until_parked();
    drop(entered);
    drop(runtime);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn display_rows_keep_local_pending_spent_visible_when_spent_toggle_off() {
    let mut local_pending = utxo_output("0x1111111111111111111111111111111111111111", "42", false);
    local_pending.local_pending_spent = true;
    local_pending.poi_spendable = false;
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 1,
        spent_count: 0,
        local_pending_spent_count: 1,
        utxos: vec![local_pending],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "", false);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].amount, "42");
    assert!(rows[0].local_pending_spent);
    assert_eq!(rows[0].poi_status, "Locally locked");
}

#[test]
fn display_rows_search_matches_source_tx_hash() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 2,
        spent_count: 0,
        local_pending_spent_count: 0,
        utxos: vec![
            utxo_output_with_hashes(
                "0x1111111111111111111111111111111111111111",
                "42",
                false,
                "0xaAaA000000000000000000000000000000000000000000000000000000000000",
                None,
            ),
            utxo_output_with_hashes(
                "0x2222222222222222222222222222222222222222",
                "7",
                false,
                "0xbbbb000000000000000000000000000000000000000000000000000000000000",
                None,
            ),
        ],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "aaaa", true);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].amount, "42");
}

#[test]
fn display_rows_search_matches_spent_tx_hash() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 2,
        unspent_count: 1,
        spent_count: 1,
        local_pending_spent_count: 0,
        utxos: vec![
            utxo_output_with_hashes(
                "0x1111111111111111111111111111111111111111",
                "42",
                true,
                "0x3333000000000000000000000000000000000000000000000000000000000000",
                Some("0xdead000000000000000000000000000000000000000000000000000000000000"),
            ),
            utxo_output_with_hashes(
                "0x2222222222222222222222222222222222222222",
                "7",
                false,
                "0x4444000000000000000000000000000000000000000000000000000000000000",
                None,
            ),
        ],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "dead", true);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].amount, "42");
    assert!(rows[0].is_spent);
}

#[test]
fn display_rows_search_ignores_spent_visibility_toggle() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 0,
        spent_count: 1,
        local_pending_spent_count: 0,
        utxos: vec![utxo_output_with_hashes(
            "0x1111111111111111111111111111111111111111",
            "42",
            true,
            "0x3333000000000000000000000000000000000000000000000000000000000000",
            Some("0xdead000000000000000000000000000000000000000000000000000000000000"),
        )],
        totals: Vec::new(),
    };

    let rows = display_rows_from_output(&output, "dead", false);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].is_spent);
}
