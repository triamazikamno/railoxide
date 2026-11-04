use super::helpers::*;

#[test]
fn poi_verified_unspent_utxos_filter_planner_inputs() {
    let token = address(0x11);
    let valid = utxo(token, 5, 0, 1);
    let mut unknown = utxo(token, 100, 0, 2);
    unknown.utxo.poi.statuses.clear();
    let mut blocked = utxo(token, 7, 0, 3);
    blocked
        .utxo
        .poi
        .statuses
        .insert(default_active_poi_list_keys()[0], PoiStatus::ShieldBlocked);
    let spent = spent_utxo(token, 9, 0, 4);

    let selected = crate::poi_verified_unspent_utxos_from_records(
        &[valid, unknown, blocked, spent],
        &WalletPendingOverlay::default(),
    );

    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].note.value, uint!(5_U256));
}

#[test]
fn pending_spent_utxos_filter_planner_inputs() {
    let token = address(0x11);
    let valid = utxo(token, 5, 0, 1);
    let pending = WalletPendingOverlay {
        pending_spent: vec![WalletPendingSpent {
            tree: 0,
            position: 1,
            stable_identity: None,
            tx_hash: Some(FixedBytes::from([0x99; 32])),
            block_number: Some(20),
            block_timestamp: Some(1_700_000_020),
        }],
        ..WalletPendingOverlay::default()
    };

    let selected = crate::poi_verified_unspent_utxos_from_records(&[valid], &pending);

    assert!(selected.is_empty());
}

#[test]
fn pending_overlay_rows_are_not_spendable() {
    let token = address(0x11);
    let confirmed = utxo(token, 5, 0, 1);
    let pending_new = utxo(token, 7, 0, 2);
    let mut pending_spent_overlay = WalletPendingOverlay {
        pending_spent: vec![WalletPendingSpent {
            tree: 0,
            position: 1,
            stable_identity: None,
            tx_hash: Some(FixedBytes::from([0x99; 32])),
            block_number: Some(20),
            block_timestamp: Some(1_700_000_020),
        }],
        ..WalletPendingOverlay::default()
    };
    pending_spent_overlay.new_utxos.push(pending_new);
    let (mut outputs, _) = utxo_outputs_from_utxos(vec![confirmed.clone()]);

    apply_pending_overlay_to_outputs(&[confirmed], pending_spent_overlay, &mut outputs);

    assert_eq!(outputs.len(), 2);
    assert!(outputs.iter().any(|output| output.pending_spent));
    assert!(
        outputs
            .iter()
            .any(|output| output.pending_new && output.ppoi_state == UtxoPpoiState::Unknown)
    );
    assert_eq!(max_send_amount_from_outputs(&outputs, token), U256::ZERO);
    assert_eq!(
        max_unshield_amount_from_outputs(&outputs, token),
        U256::ZERO
    );
}

#[test]
fn local_pending_spent_rows_are_not_spendable() {
    let token = address(0x11);
    let confirmed = utxo(token, 5, 0, 1);
    let local_pending_overlay = WalletPendingOverlay {
        local_pending_spent: vec![WalletPendingSpent {
            tree: 0,
            position: 1,
            stable_identity: Some(railgun_wallet::wallet_cache::wallet_utxo_stable_identity(
                &confirmed,
            )),
            tx_hash: Some(FixedBytes::from([0x99; 32])),
            block_number: None,
            block_timestamp: Some(1_700_000_020),
        }],
        ..WalletPendingOverlay::default()
    };
    let (mut outputs, _) = utxo_outputs_from_utxos(vec![confirmed.clone()]);

    apply_pending_overlay_to_outputs(&[confirmed], local_pending_overlay, &mut outputs);

    assert_eq!(outputs.len(), 1);
    assert!(outputs[0].local_pending_spent);
    assert!(!outputs[0].pending_spent);
    assert_eq!(max_send_amount_from_outputs(&outputs, token), U256::ZERO);
    assert_eq!(
        max_unshield_amount_from_outputs(&outputs, token),
        U256::ZERO
    );
}

#[test]
fn retained_local_lock_matches_identical_replay_but_not_reorg_replacement() {
    let token = address(0x11);
    let original = utxo(token, 5, 0, 1);
    let replacement = utxo(token, 7, 0, 1);
    let retained_lock = WalletPendingSpent {
        tree: 0,
        position: 1,
        stable_identity: Some(railgun_wallet::wallet_cache::wallet_utxo_stable_identity(
            &original,
        )),
        tx_hash: Some(FixedBytes::from([0x99; 32])),
        block_number: None,
        block_timestamp: Some(1_700_000_020),
    };
    let overlay = WalletPendingOverlay {
        local_pending_spent: vec![retained_lock],
        ..WalletPendingOverlay::default()
    };

    assert!(
        crate::poi_verified_unspent_utxos_from_records(std::slice::from_ref(&original), &overlay,)
            .is_empty(),
        "the identical input remains locked when it replays"
    );
    let selected = crate::poi_verified_unspent_utxos_from_records(
        std::slice::from_ref(&replacement),
        &overlay,
    );
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].note.value, uint!(7_U256));

    let (mut outputs, _) = utxo_outputs_from_utxos(vec![replacement.clone()]);
    apply_pending_overlay_to_outputs(&[replacement], overlay, &mut outputs);
    assert!(!outputs[0].local_pending_spent);
    assert_eq!(max_send_amount_from_outputs(&outputs, token), uint!(7_U256));
}

#[test]
fn same_position_local_intents_match_current_identity_in_either_order() {
    let token = address(0x11);
    let current = utxo(token, 5, 0, 1);
    let replacement = utxo(token, 7, 0, 1);
    let lock_for = |utxo: &WalletUtxo, marker| WalletPendingSpent {
        tree: utxo.utxo.tree,
        position: utxo.utxo.position,
        stable_identity: Some(railgun_wallet::wallet_cache::wallet_utxo_stable_identity(
            utxo,
        )),
        tx_hash: Some(FixedBytes::from([marker; 32])),
        block_number: None,
        block_timestamp: Some(1_700_000_020),
    };
    let current_lock = lock_for(&current, 0x91);
    let replacement_lock = lock_for(&replacement, 0x92);

    for local_pending_spent in [
        vec![current_lock.clone(), replacement_lock.clone()],
        vec![replacement_lock, current_lock],
    ] {
        let (mut outputs, _) = utxo_outputs_from_utxos(vec![current.clone()]);
        apply_pending_overlay_to_outputs(
            std::slice::from_ref(&current),
            WalletPendingOverlay {
                local_pending_spent,
                ..WalletPendingOverlay::default()
            },
            &mut outputs,
        );
        assert!(outputs[0].local_pending_spent);
        assert_eq!(max_send_amount_from_outputs(&outputs, token), U256::ZERO);
    }
}

#[test]
fn utxo_outputs_are_sorted_by_tree_then_position() {
    let token = address(0x11);
    let (outputs, _) = utxo_outputs_from_utxos(vec![
        utxo(token, 1, 2, 1),
        utxo(token, 1, 1, 2),
        utxo(token, 1, 1, 1),
    ]);

    let positions: Vec<(u32, u64)> = outputs
        .into_iter()
        .map(|output| (output.tree, output.position))
        .collect();
    assert_eq!(positions, vec![(1, 1), (1, 2), (2, 1)]);
}

#[test]
fn fee_token_amount_from_outputs_matches_single_fee_transaction_limit() {
    let token = address(0x12);
    let (outputs, _) = utxo_outputs_from_utxos(
        (0..20)
            .map(|position| utxo(token, 1, 0, position))
            .collect(),
    );

    assert_eq!(
        max_broadcaster_fee_token_amount_from_outputs(&outputs, token),
        uint!(13_U256)
    );
}

#[test]
fn token_totals_are_accumulated_by_token_address() {
    let token_a = address(0x11);
    let token_b = address(0x22);
    let (_, totals) = utxo_outputs_from_utxos(vec![
        utxo(token_b, 7, 0, 0),
        utxo(token_a, 3, 0, 1),
        utxo(token_a, 4, 0, 2),
        spent_utxo(token_a, 100, 0, 3),
    ]);

    assert_eq!(
        totals,
        vec![
            TokenTotal {
                token: token_a.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
            TokenTotal {
                token: token_b.to_checksum(None),
                total: "7".to_string(),
                poi_verified_total: "7".to_string(),
            },
        ]
    );
}

#[test]
fn token_totals_include_poi_verified_balance() {
    let token = address(0x11);
    let active_list_key = default_active_poi_list_keys()[0];
    let mut valid = utxo(token, 5, 0, 1);
    valid
        .utxo
        .poi
        .statuses
        .insert(active_list_key, PoiStatus::Valid);
    let mut missing = utxo(token, 7, 0, 2);
    missing.utxo.poi.statuses.clear();

    let (outputs, totals) = utxo_outputs_from_utxos(vec![valid, missing]);

    assert!(outputs[0].poi_spendable);
    assert_eq!(
        outputs[0].poi_statuses[&hex::encode(active_list_key)],
        "Valid"
    );
    assert!(!outputs[1].poi_spendable);
    assert_eq!(
        outputs[1].poi_statuses[&hex::encode(active_list_key)],
        "Unknown"
    );
    assert_eq!(outputs[1].ppoi_state, UtxoPpoiState::Unknown);
    assert_eq!(totals[0].total, "12");
    assert_eq!(totals[0].poi_verified_total, "5");
}

#[test]
fn utxo_ppoi_state_is_derived_from_active_list_statuses() {
    let token = address(0x11);
    for (status, expected, retry_eligible) in [
        (PoiStatus::Valid, UtxoPpoiState::Valid, false),
        (PoiStatus::Missing, UtxoPpoiState::Missing, true),
        (
            PoiStatus::ProofSubmitted,
            UtxoPpoiState::ProofSubmitted,
            true,
        ),
        (PoiStatus::Unknown, UtxoPpoiState::Unknown, true),
        (
            PoiStatus::ShieldBlocked,
            UtxoPpoiState::ShieldBlocked,
            false,
        ),
    ] {
        let mut wallet_utxo = utxo(token, 5, 0, 1);
        for list_key in default_active_poi_list_keys() {
            wallet_utxo.utxo.poi.statuses.insert(list_key, status);
        }

        let (outputs, _) = utxo_outputs_from_utxos(vec![wallet_utxo]);

        assert_eq!(outputs[0].ppoi_state, expected);
        assert_eq!(outputs[0].is_ppoi_retry_eligible(), retry_eligible);
    }
}

#[test]
fn mixed_active_list_statuses_are_typed_and_shield_blocked_takes_priority() {
    let mut wallet_utxo = utxo(address(0x11), 5, 0, 1);
    let active_keys = [
        FixedBytes::from([0x11; 32]),
        FixedBytes::from([0x22; 32]),
        FixedBytes::from([0x33; 32]),
    ];
    wallet_utxo.utxo.poi.statuses.clear();
    wallet_utxo
        .utxo
        .poi
        .statuses
        .insert(active_keys[0], PoiStatus::Valid);
    wallet_utxo
        .utxo
        .poi
        .statuses
        .insert(active_keys[1], PoiStatus::Missing);

    assert_eq!(
        crate::utxos::utxo_ppoi_state(&wallet_utxo.utxo.poi, &active_keys),
        UtxoPpoiState::Mixed
    );

    wallet_utxo
        .utxo
        .poi
        .statuses
        .insert(active_keys[2], PoiStatus::ShieldBlocked);
    assert_eq!(
        crate::utxos::utxo_ppoi_state(&wallet_utxo.utxo.poi, &active_keys),
        UtxoPpoiState::ShieldBlocked
    );
}

#[test]
fn ppoi_retry_eligibility_excludes_spent_and_pending_outputs() {
    let token = address(0x11);
    let mut missing = utxo(token, 5, 0, 1);
    for list_key in default_active_poi_list_keys() {
        missing
            .utxo
            .poi
            .statuses
            .insert(list_key, PoiStatus::Missing);
    }
    let (mut outputs, _) = utxo_outputs_from_utxos(vec![missing]);
    assert!(outputs[0].is_ppoi_retry_eligible());

    outputs[0].is_spent = true;
    assert!(!outputs[0].is_ppoi_retry_eligible());
    outputs[0].is_spent = false;
    outputs[0].pending_new = true;
    assert!(!outputs[0].is_ppoi_retry_eligible());
    outputs[0].pending_new = false;
    outputs[0].pending_spent = true;
    assert!(!outputs[0].is_ppoi_retry_eligible());
    outputs[0].pending_spent = false;
    outputs[0].local_pending_spent = true;
    assert!(!outputs[0].is_ppoi_retry_eligible());
    outputs[0].local_pending_spent = false;
    outputs[0].commitment_kind = "Shield".to_string();
    assert!(!outputs[0].is_ppoi_retry_eligible());
}

#[test]
fn utxo_outputs_classify_activity_rows() {
    let token = address(0x11);
    let active_list_key = default_active_poi_list_keys()[0];
    let shield = utxo_with_kind(token, 5, 0, 1, UtxoCommitmentKind::Shield);
    let mut blocked_shield = utxo_with_kind(token, 7, 0, 2, UtxoCommitmentKind::Shield);
    blocked_shield
        .utxo
        .poi
        .statuses
        .insert(active_list_key, PoiStatus::ShieldBlocked);
    let transact = utxo(token, 9, 0, 3);

    let (outputs, _) = utxo_outputs_from_utxos(vec![shield, blocked_shield, transact]);

    assert_eq!(outputs[0].activity_classification, "Shield");
    assert!(outputs[0].blocked_shield_rescue.is_none());
    assert_eq!(outputs[1].activity_classification, "Blocked Shield");
    assert!(!outputs[1].poi_spendable);
    assert_eq!(
        outputs[1]
            .blocked_shield_rescue
            .as_ref()
            .and_then(|rescue| rescue.disabled_reason.as_deref()),
        Some("Source transaction origin has not been resolved yet.")
    );
    assert_eq!(outputs[2].activity_classification, "Private Output");
    assert!(outputs[2].blocked_shield_rescue.is_none());
}

#[test]
fn max_amount_from_outputs_uses_planner_batched_selection() {
    let token = address(0x11);
    let other = address(0x22);
    let mut wallet_utxos = (0..20)
        .map(|position| utxo(token, 1, 0, position))
        .collect::<Vec<_>>();
    wallet_utxos.extend((0..5).map(|position| utxo(token, 3, 1, position)));
    wallet_utxos.push(utxo(other, 100, 1, 99));
    wallet_utxos.push(spent_utxo(token, 100, 2, 0));
    let (outputs, _) = utxo_outputs_from_utxos(wallet_utxos);

    assert_eq!(
        max_unshield_amount_from_outputs(&outputs, token),
        uint!(35_U256)
    );
    assert_eq!(
        max_send_amount_from_outputs(&outputs, token),
        uint!(35_U256)
    );
}

#[test]
fn max_amount_from_outputs_does_not_reconstruct_display_commitments() {
    let token = address(0x11);
    let (mut outputs, _) = utxo_outputs_from_utxos(vec![utxo(token, 5, 0, 1)]);
    outputs[0].commitment = "not-a-commitment".to_string();
    outputs[0].npk = "not-an-npk".to_string();
    outputs[0].blinded_commitment = "not-a-blinded-commitment".to_string();

    assert_eq!(max_send_amount_from_outputs(&outputs, token), uint!(5_U256));
    assert_eq!(
        max_unshield_amount_from_outputs(&outputs, token),
        uint!(5_U256)
    );
}

#[test]
fn max_amount_from_outputs_excludes_non_poi_verified_utxos() {
    let token = address(0x11);
    let mut valid = utxo(token, 5, 0, 1);
    let mut unknown = utxo(token, 100, 0, 2);
    unknown.utxo.poi.statuses.clear();
    let (outputs, _) = utxo_outputs_from_utxos(vec![valid.clone(), unknown]);

    assert_eq!(max_send_amount_from_outputs(&outputs, token), uint!(5_U256));
    assert_eq!(
        max_unshield_amount_from_outputs(&outputs, token),
        uint!(5_U256)
    );

    valid.spent = Some(source(9));
    let (outputs, _) = utxo_outputs_from_utxos(vec![valid]);
    assert_eq!(max_send_amount_from_outputs(&outputs, token), U256::ZERO);
    assert_eq!(
        max_unshield_amount_from_outputs(&outputs, token),
        U256::ZERO
    );
}

#[test]
fn utxo_outputs_include_generation_timestamp() {
    let token = address(0x11);
    let (outputs, _) = utxo_outputs_from_utxos(vec![utxo(token, 1, 0, 7)]);

    assert_eq!(outputs[0].source_block_timestamp, 1_700_000_008);
}

#[test]
fn list_utxos_output_serializes_existing_field_names() {
    let output = ListUtxosOutput {
        chain_id: 1,
        cache_key: "cache".to_string(),
        utxo_count: 1,
        unspent_count: 0,
        spent_count: 1,
        local_pending_spent_count: 0,
        utxos: vec![UtxoOutput {
            tree: 2,
            position: 3,
            token: "0x0000000000000000000000000000000000000001".to_string(),
            value: "4".to_string(),
            commitment_kind: "Transact".to_string(),
            activity_classification: "Private Output".to_string(),
            blocked_shield_rescue: None,
            commitment: "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            npk: "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            blinded_commitment:
                "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_string(),
            poi_statuses: BTreeMap::from([(
                "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_string(),
                "Unknown".to_string(),
            )]),
            ppoi_state: UtxoPpoiState::Unknown,
            ppoi_last_submission_at: None,
            poi_spendable: false,
            source_tx_hash: "0x1111111111111111111111111111111111111111111111111111111111111111"
                .to_string(),
            source_block_number: 11,
            source_block_timestamp: 1_700_000_011,
            is_spent: true,
            pending_new: false,
            pending_spent: false,
            local_pending_spent: false,
            spent_tx_hash: Some(
                "0x2222222222222222222222222222222222222222222222222222222222222222".to_string(),
            ),
            spent_block_number: Some(21),
            spent_block_timestamp: None,
        }],
        totals: vec![TokenTotal {
            token: "0x0000000000000000000000000000000000000001".to_string(),
            total: "4".to_string(),
            poi_verified_total: "0".to_string(),
        }],
    };

    assert_eq!(
        serde_json::to_value(output).expect("serialize output"),
        json!({
            "chain_id": 1,
            "cache_key": "cache",
            "utxo_count": 1,
            "unspent_count": 0,
            "spent_count": 1,
            "local_pending_spent_count": 0,
            "utxos": [{
                "tree": 2,
                "position": 3,
                "token": "0x0000000000000000000000000000000000000001",
                "value": "4",
                "commitment_kind": "Transact",
                "activity_classification": "Private Output",
                "blocked_shield_rescue": null,
                "commitment": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "npk": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "blinded_commitment": "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "poi_statuses": {
                    "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd": "Unknown",
                },
                "ppoi_state": "Unknown",
                "ppoi_last_submission_at": null,
                "poi_spendable": false,
                "source_tx_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                "source_block_number": 11,
                "source_block_timestamp": 1_700_000_011,
                "is_spent": true,
                "pending_new": false,
                "pending_spent": false,
                "local_pending_spent": false,
                "spent_tx_hash": "0x2222222222222222222222222222222222222222222222222222222222222222",
                "spent_block_number": 21,
                "spent_block_timestamp": null,
            }],
            "totals": [{
                "token": "0x0000000000000000000000000000000000000001",
                "total": "4",
                "poi_verified_total": "0",
            }],
        })
    );
}
