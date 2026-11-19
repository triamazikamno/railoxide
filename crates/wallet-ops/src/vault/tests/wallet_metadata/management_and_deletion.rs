use super::*;

#[test]
fn wallet_label_validation_defaults_update_reorder_and_deactivate() {
    let (root_dir, db, store) = desktop_store_with_vault();
    assert_eq!(
        store
            .default_wallet_label(TEST_PASSWORD)
            .expect("default label"),
        PRIMARY_WALLET_LABEL
    );

    let seed = generate_seed_material().expect("generate seed");
    let first_wallet_id = "first-wallet";
    let first_metadata = store
        .new_wallet_metadata(
            TEST_PASSWORD,
            first_wallet_id,
            0,
            WalletSource::Generated,
            "  Primary wallet  ",
        )
        .expect("first wallet metadata");
    assert_eq!(first_metadata.label, PRIMARY_WALLET_LABEL);
    assert_eq!(first_metadata.display_order, 0);
    store
        .store_generated_wallet_with_metadata(
            TEST_PASSWORD,
            first_wallet_id,
            0,
            "english",
            &seed,
            &first_metadata,
        )
        .expect("store first wallet");
    assert_eq!(
        store
            .default_wallet_label(TEST_PASSWORD)
            .expect("second default label"),
        "Wallet 2"
    );
    assert!(matches!(
        store.new_wallet_metadata(
            TEST_PASSWORD,
            "duplicate",
            0,
            WalletSource::Imported,
            "primary wallet",
        ),
        Err(VaultError::DuplicateWalletLabel)
    ));
    assert!(matches!(
        store.new_wallet_metadata(TEST_PASSWORD, "empty", 0, WalletSource::Imported, "   "),
        Err(VaultError::InvalidWalletLabel)
    ));
    assert!(matches!(
        store.preflight_new_wallet_metadata(TEST_PASSWORD, "primary wallet"),
        Err(VaultError::DuplicateWalletLabel)
    ));
    assert!(matches!(
        store.preflight_new_wallet_metadata(TEST_PASSWORD, "   "),
        Err(VaultError::InvalidWalletLabel)
    ));
    assert_eq!(
        store
            .preflight_new_wallet_metadata(TEST_PASSWORD, "  Wallet 2  ")
            .expect("preflight new label"),
        "Wallet 2"
    );

    let second_wallet_id = "second-wallet";
    let second_metadata = store
        .new_wallet_metadata(
            TEST_PASSWORD,
            second_wallet_id,
            0,
            WalletSource::Generated,
            "Wallet 2",
        )
        .expect("second wallet metadata");
    store
        .store_generated_wallet_with_metadata(
            TEST_PASSWORD,
            second_wallet_id,
            0,
            "english",
            &seed,
            &second_metadata,
        )
        .expect("store second wallet");

    let updated = store
        .update_wallet_label(TEST_PASSWORD, first_wallet_id, "  Main  ")
        .expect("update label");
    assert_eq!(updated.label, "Main");
    assert_eq!(updated.wallet_uuid, first_wallet_id);
    assert_eq!(updated.status, WalletStatus::Active);
    assert_eq!(updated.display_order, 0);
    assert!(matches!(
        store.update_wallet_label(TEST_PASSWORD, second_wallet_id, "main"),
        Err(VaultError::DuplicateWalletLabel)
    ));

    let reordered = store
        .reorder_active_wallets(
            TEST_PASSWORD,
            &[second_wallet_id.to_string(), first_wallet_id.to_string()],
        )
        .expect("reorder active wallets");
    assert_eq!(reordered[0].wallet_uuid, second_wallet_id);
    assert_eq!(reordered[0].display_order, 0);
    assert_eq!(reordered[1].wallet_uuid, first_wallet_id);
    assert_eq!(reordered[1].display_order, 1);
    assert!(matches!(
        store.reorder_active_wallets(TEST_PASSWORD, &[first_wallet_id.to_string()]),
        Err(VaultError::InvalidWalletOrder)
    ));

    let deactivated = store
        .deactivate_wallet(TEST_PASSWORD, second_wallet_id)
        .expect("deactivate second wallet");
    assert_eq!(deactivated.status, WalletStatus::Inactive);
    let active = store
        .active_wallet_metadata(TEST_PASSWORD)
        .expect("active metadata");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].wallet_uuid, first_wallet_id);
    assert!(
        store
            .load_view_session(TEST_PASSWORD, second_wallet_id)
            .is_ok()
    );
    assert!(matches!(
        store.deactivate_wallet(TEST_PASSWORD, first_wallet_id),
        Err(VaultError::LastActiveWallet)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn session_wallet_management_renames_hides_shows_reorders_and_guards_last_active() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first_wallet_id = "session-wallet-a";
    let second_wallet_id = "session-wallet-b";
    let third_wallet_id = "session-wallet-c";
    let first_session = import_wallet_with_metadata(&store, first_wallet_id, "Alpha");
    let _second_session = import_wallet_with_metadata(&store, second_wallet_id, "Beta");
    let _third_session = import_wallet_with_metadata(&store, third_wallet_id, "Gamma");

    let metadata = store
        .list_wallet_metadata_for_session(&first_session, true)
        .expect("list all wallet metadata");
    assert_eq!(metadata.len(), 3);
    assert_eq!(
        metadata
            .iter()
            .map(|metadata| metadata.wallet_uuid.as_str())
            .collect::<Vec<_>>(),
        vec![first_wallet_id, second_wallet_id, third_wallet_id]
    );
    assert_eq!(
        store
            .list_wallet_metadata_for_session(&first_session, false)
            .expect("list active wallet metadata")
            .len(),
        3
    );

    let updated = store
        .update_wallet_label_for_session(&first_session, second_wallet_id, "  Main  ")
        .expect("rename wallet");
    assert_eq!(updated.label, "Main");
    assert!(matches!(
        store.update_wallet_label_for_session(&first_session, third_wallet_id, "alpha"),
        Err(VaultError::DuplicateWalletLabel)
    ));
    assert!(matches!(
        store.update_wallet_label_for_session(&first_session, third_wallet_id, "   "),
        Err(VaultError::InvalidWalletLabel)
    ));

    let hidden = store
        .set_wallet_active_for_session(&first_session, second_wallet_id, false)
        .expect("hide wallet");
    assert_eq!(hidden.status, WalletStatus::Inactive);
    let active = store
        .list_wallet_metadata_for_session(&first_session, false)
        .expect("list active after hide");
    assert_eq!(
        active
            .iter()
            .map(|metadata| metadata.wallet_uuid.as_str())
            .collect::<Vec<_>>(),
        vec![first_wallet_id, third_wallet_id]
    );
    assert!(
        store
            .load_view_session(TEST_PASSWORD, second_wallet_id)
            .is_ok()
    );

    let shown = store
        .set_wallet_active_for_session(&first_session, second_wallet_id, true)
        .expect("show wallet");
    assert_eq!(shown.status, WalletStatus::Active);
    let active = store
        .list_wallet_metadata_for_session(&first_session, false)
        .expect("list active after show");
    assert_eq!(
        active
            .iter()
            .map(|metadata| metadata.wallet_uuid.as_str())
            .collect::<Vec<_>>(),
        vec![first_wallet_id, third_wallet_id, second_wallet_id]
    );

    let reordered = store
        .reorder_active_wallets_for_session(
            &first_session,
            &[
                second_wallet_id.to_string(),
                first_wallet_id.to_string(),
                third_wallet_id.to_string(),
            ],
        )
        .expect("reorder active wallets");
    assert_eq!(reordered[0].wallet_uuid, second_wallet_id);
    assert_eq!(reordered[0].display_order, 0);
    assert_eq!(reordered[1].wallet_uuid, first_wallet_id);
    assert_eq!(reordered[1].display_order, 1);
    assert_eq!(reordered[2].wallet_uuid, third_wallet_id);
    assert_eq!(reordered[2].display_order, 2);
    assert!(matches!(
        store.reorder_active_wallets_for_session(&first_session, &[first_wallet_id.to_string()]),
        Err(VaultError::InvalidWalletOrder)
    ));

    store
        .set_wallet_active_for_session(&first_session, first_wallet_id, false)
        .expect("hide first wallet");
    store
        .set_wallet_active_for_session(&first_session, second_wallet_id, false)
        .expect("hide second wallet");
    assert!(matches!(
        store.set_wallet_active_for_session(&first_session, third_wallet_id, false),
        Err(VaultError::LastActiveWallet)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn permanent_wallet_delete_purges_wallet_scoped_records_and_guards_last_active() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first_wallet_id = "delete-wallet-a";
    let second_wallet_id = "64656c6574652d77616c6c65742d6221";
    let third_wallet_id = "delete-wallet-c";
    let first_session = import_wallet_with_metadata(&store, first_wallet_id, "Alpha");
    let second_session = import_wallet_with_metadata(&store, second_wallet_id, "Beta");
    let _third_session = import_wallet_with_metadata(&store, third_wallet_id, "Gamma");
    let first_chain = store
        .wallet_chain_metadata_for_session(
            &first_session,
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("first chain metadata");
    let second_chain = store
        .wallet_chain_metadata_for_session(
            &second_session,
            0,
            1,
            "0x2222222222222222222222222222222222222222",
            100,
        )
        .expect("second chain metadata");
    let second_cache_key = second_chain
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("second wallet cache key");
    db.put_wallet_utxo(
        &second_cache_key,
        "atomic-row",
        b"encrypted atomic cache row",
    )
    .expect("store atomic cache row");
    db.put_wallet_meta(
        &second_cache_key,
        &WalletMeta {
            last_scanned_block: 123,
            updated_at: 1,
            last_scanned_block_hash: None,
        },
    )
    .expect("store atomic cache metadata");
    db.put_wallet_sync_actor_state(&WalletSyncActorStateRecord {
        chain_id: second_chain.chain_id,
        wallet_id: second_cache_key.to_string(),
        highest_accepted_reset_intent: 7,
        pending_reset: None,
        updated_at: 2,
    })
    .expect("store actor state");
    let second_namespace =
        WalletPrivateNamespaceId::new(second_chain.chain_id, second_cache_key.clone());
    let first_cache_key = first_chain
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("first wallet cache key");
    let first_namespace = WalletPrivateNamespaceId::new(first_chain.chain_id, first_cache_key);
    let target_candidate_row = OpaqueWalletPrivateRow {
        row_id: vec![0x21; 32],
        payload: b"target encrypted sender candidate".to_vec(),
    };
    let unrelated_candidate_row = OpaqueWalletPrivateRow {
        row_id: vec![0x22; 32],
        payload: b"unrelated encrypted sender candidate".to_vec(),
    };
    db.put_opaque_wallet_private_row(
        &second_namespace,
        WalletPrivateRecordKind::SenderTransactionCandidate,
        &target_candidate_row,
    )
    .expect("store target sender candidate");
    db.put_opaque_wallet_private_row(
        &first_namespace,
        WalletPrivateRecordKind::SenderTransactionCandidate,
        &unrelated_candidate_row,
    )
    .expect("store unrelated sender candidate");
    let pending_commitment = FixedBytes::from([0x31; 32]);
    db.put_pending_output_poi_context(&PendingOutputPoiContextRecord {
        chain_id: second_chain.chain_id,
        wallet_id: second_cache_key.to_string(),
        txid_version: "V2_PoseidonMerkle".to_string(),
        output_commitment: pending_commitment,
        output_npk: FixedBytes::from([0x32; 32]),
        utxo_tree_in: 1,
        railgun_txid: U256::from(3),
        txid_merkleroot_index: None,
        pre_transaction_pois_per_txid_leaf_per_list: BTreeMap::new(),
        required_poi_list_keys: Vec::new(),
        output_role: PendingOutputPoiRole::Recipient,
        created_at: 3,
        source_operation_id: None,
        observation: None,
        submitted_poi_list_keys: Vec::new(),
        terminal_error: None,
    })
    .expect("store pending output context");
    db.put_pending_output_poi_context(&PendingOutputPoiContextRecord {
        chain_id: second_chain.chain_id,
        wallet_id: second_wallet_id.to_string(),
        txid_version: "V2_PoseidonMerkle".to_string(),
        output_commitment: FixedBytes::from([0x35; 32]),
        output_npk: FixedBytes::from([0x36; 32]),
        utxo_tree_in: 1,
        railgun_txid: U256::from(5),
        txid_merkleroot_index: None,
        pre_transaction_pois_per_txid_leaf_per_list: BTreeMap::new(),
        required_poi_list_keys: Vec::new(),
        output_role: PendingOutputPoiRole::Recipient,
        created_at: 3,
        source_operation_id: None,
        observation: None,
        submitted_poi_list_keys: Vec::new(),
        terminal_error: None,
    })
    .expect("store alpha wallet-identity pending output context");
    let recovery_commitment = FixedBytes::from([0x41; 32]);
    db.put_output_poi_recovery(&OutputPoiRecoveryRecord {
        chain_id: second_chain.chain_id,
        wallet_id: second_cache_key.to_string(),
        output_commitment: recovery_commitment,
        source_tx_hash: FixedBytes::from([0x42; 32]),
        tx_input: None,
        status: OutputPoiRecoveryStatus::Recoverable,
        created_at: 4,
        updated_at: 4,
        last_detection_at: Some(4),
        last_submission_at: None,
        next_retry_at: None,
        attempt_count: 0,
        last_error: None,
    })
    .expect("store output recovery");
    db.put_output_poi_recovery(&OutputPoiRecoveryRecord {
        chain_id: second_chain.chain_id,
        wallet_id: second_wallet_id.to_string(),
        output_commitment: FixedBytes::from([0x45; 32]),
        source_tx_hash: FixedBytes::from([0x46; 32]),
        tx_input: None,
        status: OutputPoiRecoveryStatus::Recoverable,
        created_at: 4,
        updated_at: 4,
        last_detection_at: Some(4),
        last_submission_at: None,
        next_retry_at: None,
        attempt_count: 0,
        last_error: None,
    })
    .expect("store alpha wallet-identity output recovery");
    let private_account = store
        .import_public_account(
            TEST_PASSWORD,
            &second_session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("Private"),
            false,
        )
        .expect("import private scoped account");
    let global_account = store
        .import_public_account(
            TEST_PASSWORD,
            &second_session,
            IMPORT_PRIVATE_KEY_TWO,
            Some("Global"),
            true,
        )
        .expect("import global account");
    let private_account_ids = store
        .list_public_accounts_for_session(&second_session, true)
        .expect("list second wallet public accounts")
        .into_iter()
        .filter_map(|account| match account.scope {
            PublicAccountScope::PrivateWallet { wallet_uuid }
                if wallet_uuid == second_wallet_id =>
            {
                Some(account.public_account_uuid)
            }
            PublicAccountScope::PrivateWallet { .. } | PublicAccountScope::Global => None,
        })
        .collect::<Vec<_>>();
    assert!(private_account_ids.contains(&private_account.public_account_uuid));
    let [private_tron, global_tron] = [
        (IMPORT_PRIVATE_KEY_ONE, false),
        (IMPORT_PRIVATE_KEY_TWO, true),
    ]
    .map(|(private_key, global)| {
        store
            .import_tron_account(TEST_PASSWORD, &second_session, private_key, None, global)
            .expect("import tron account")
            .tron_account_uuid
    });

    db.put_desktop_wallet_vault_record(
        &wallet_chain_metadata_record_key(&second_chain.wallet_chain_uuid),
        b"corrupt wallet-chain metadata",
    )
    .expect("corrupt target chain metadata");
    let second_chain_index_key =
        wallet_chain_index_record_key(second_wallet_id, &second_chain.wallet_chain_uuid);
    db.delete_desktop_wallet_vault_record(&second_chain_index_key)
        .expect("remove target chain ownership index");
    db.delete_desktop_wallet_vault_record(&wallet_chain_index_complete_record_key(
        second_wallet_id,
    ))
    .expect("remove target ownership completeness");
    assert!(matches!(
        store.delete_wallet_for_session(&first_session, second_wallet_id),
        Err(VaultError::WalletChainMetadataUnavailable)
    ));
    assert!(
        db.get_desktop_wallet_vault_record(&wallet_metadata_record_key(second_wallet_id))
            .expect("load retained wallet metadata")
            .is_some()
    );
    assert!(
        db.get_wallet_meta(&second_cache_key)
            .expect("load retained atomic metadata")
            .is_some()
    );
    db.put_desktop_wallet_vault_record(
        &second_chain_index_key,
        &rmp_serde::to_vec_named(&second_chain.chain_id).expect("encode chain id"),
    )
    .expect("restore target chain ownership index");

    let deleted = store
        .delete_wallet_for_session(&first_session, second_wallet_id)
        .expect("delete active wallet");
    assert_eq!(deleted.wallet_uuid, second_wallet_id);
    assert_eq!(deleted.status, WalletStatus::Active);
    assert!(
        store
            .load_view_session(TEST_PASSWORD, second_wallet_id)
            .is_err()
    );

    for key in [
        wallet_metadata_record_key(second_wallet_id),
        wallet_view_record_key(second_wallet_id),
        wallet_spend_record_key(second_wallet_id),
        wallet_chain_metadata_record_key(&second_chain.wallet_chain_uuid),
        second_chain_index_key,
    ] {
        assert!(
            db.get_desktop_wallet_vault_record(&key)
                .expect("load deleted record")
                .is_none(),
            "expected {key} to be deleted"
        );
    }
    assert_eq!(
        db.list_wallet_utxos(&second_cache_key)
            .expect("list deleted atomic cache rows"),
        [] as [local_db::WalletUtxoRecord; 0]
    );
    assert!(
        db.get_wallet_meta(&second_cache_key)
            .expect("load deleted atomic metadata")
            .is_none()
    );
    assert!(
        db.get_wallet_sync_actor_state(second_chain.chain_id, second_cache_key.as_str())
            .expect("load deleted actor state")
            .is_none()
    );
    assert!(
        db.list_pending_output_poi_contexts(second_chain.chain_id, second_cache_key.as_str())
            .expect("list deleted pending output contexts")
            .is_empty()
    );
    assert!(
        db.list_output_poi_recoveries(second_chain.chain_id, second_cache_key.as_str())
            .expect("list deleted output recoveries")
            .is_empty()
    );
    assert_eq!(
        db.list_opaque_wallet_private_rows(
            &second_namespace,
            WalletPrivateRecordKind::SenderTransactionCandidate,
        )
        .expect("list deleted sender candidates"),
        [] as [OpaqueWalletPrivateRow; 0]
    );
    assert_eq!(
        db.list_opaque_wallet_private_rows(
            &first_namespace,
            WalletPrivateRecordKind::SenderTransactionCandidate,
        )
        .expect("list retained unrelated sender candidates"),
        vec![unrelated_candidate_row]
    );
    assert!(
        db.list_pending_output_poi_contexts(second_chain.chain_id, second_wallet_id)
            .expect("list deleted alpha wallet-identity pending contexts")
            .is_empty()
    );
    assert!(
        db.list_output_poi_recoveries(second_chain.chain_id, second_wallet_id)
            .expect("list deleted alpha wallet-identity output recoveries")
            .is_empty()
    );
    for key in [
        wallet_metadata_record_key(first_wallet_id),
        wallet_view_record_key(first_wallet_id),
        wallet_spend_record_key(first_wallet_id),
        wallet_chain_metadata_record_key(&first_chain.wallet_chain_uuid),
    ] {
        assert!(
            db.get_desktop_wallet_vault_record(&key)
                .expect("load retained record")
                .is_some(),
            "expected {key} to be retained"
        );
    }
    for account_id in private_account_ids {
        assert!(
            db.get_desktop_wallet_vault_record(&public_account_metadata_record_key(&account_id))
                .expect("load deleted public account metadata")
                .is_none()
        );
        assert!(
            db.get_desktop_wallet_vault_record(&public_account_secret_record_key(&account_id))
                .expect("load deleted public account secret")
                .is_none()
        );
    }
    assert!(
        db.get_desktop_wallet_vault_record(&public_account_metadata_record_key(
            &global_account.public_account_uuid,
        ))
        .expect("load global metadata")
        .is_some()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&public_account_secret_record_key(
            &global_account.public_account_uuid,
        ))
        .expect("load global secret")
        .is_some()
    );
    for (account_id, retained) in [(private_tron, false), (global_tron, true)] {
        for key in [
            tron_account_metadata_record_key(&account_id),
            tron_account_secret_record_key(&account_id),
        ] {
            assert_eq!(
                db.get_desktop_wallet_vault_record(&key)
                    .expect("load tron account record")
                    .is_some(),
                retained,
                "{key}"
            );
        }
    }

    store
        .set_wallet_active_for_session(&first_session, third_wallet_id, false)
        .expect("hide third wallet");
    let deleted_hidden = store
        .delete_wallet_for_session(&first_session, third_wallet_id)
        .expect("delete hidden wallet");
    assert_eq!(deleted_hidden.status, WalletStatus::Inactive);
    assert!(
        store
            .load_view_session(TEST_PASSWORD, third_wallet_id)
            .is_err()
    );
    assert!(matches!(
        store.delete_wallet_for_session(&first_session, first_wallet_id),
        Err(VaultError::LastActiveWallet)
    ));
    assert_eq!(
        store
            .list_wallet_metadata_for_session(&first_session, true)
            .expect("list remaining metadata")
            .iter()
            .map(|metadata| metadata.wallet_uuid.as_str())
            .collect::<Vec<_>>(),
        vec![first_wallet_id]
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn ownership_complete_wallet_deletion_ignores_unrelated_ambiguous_metadata() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first_session = import_wallet_with_metadata(&store, "complete-first", "First");
    let second_wallet_id = "complete-second";
    let second_session = import_wallet_with_metadata(&store, second_wallet_id, "Second");
    let ambiguous_key = wallet_chain_metadata_record_key("unrelated-legacy-hardware");
    db.put_desktop_wallet_vault_record(&ambiguous_key, b"unreadable legacy metadata")
        .expect("store unrelated ambiguous metadata");
    assert!(
        store
            .find_wallet_chain_metadata_for_session(
                &second_session,
                0,
                1,
                "0x1111111111111111111111111111111111111111",
            )
            .expect("scan certified wallet metadata")
            .is_none()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&wallet_chain_index_complete_record_key(
            second_wallet_id
        ))
        .expect("load retained completeness marker")
        .is_some()
    );

    let deleted = store
        .delete_wallet_for_session(&first_session, second_wallet_id)
        .expect("delete ownership-complete wallet");
    assert_eq!(deleted.wallet_uuid, second_wallet_id);
    assert!(
        db.get_desktop_wallet_vault_record(&ambiguous_key)
            .expect("load unrelated metadata")
            .is_some()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&wallet_chain_index_complete_record_key(
            second_wallet_id
        ))
        .expect("load deleted completeness marker")
        .is_none()
    );

    drop(second_session);
    drop(first_session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn ambiguous_legacy_scan_does_not_certify_ownership_completeness() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "ambiguous-legacy-wallet";
    let session = import_wallet_with_metadata(&store, wallet_id, "Legacy");
    let completeness_key = wallet_chain_index_complete_record_key(wallet_id);
    db.delete_desktop_wallet_vault_record(&completeness_key)
        .expect("remove creation-time completeness");
    db.put_desktop_wallet_vault_record(
        &wallet_chain_metadata_record_key("unindexed-corrupt-chain"),
        b"corrupt chain metadata",
    )
    .expect("store ambiguous chain metadata");

    assert!(
        store
            .find_wallet_chain_metadata_for_session(
                &session,
                0,
                1,
                "0x1111111111111111111111111111111111111111",
            )
            .expect("scan legacy metadata")
            .is_none()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&completeness_key)
            .expect("load completeness marker")
            .is_none()
    );

    drop(session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn authenticated_malformed_legacy_metadata_blocks_certification_and_deletion() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "33333333333333333333333333333333";
    let session = import_wallet_with_metadata(&store, wallet_id, "Malformed");
    import_wallet_with_metadata(&store, "malformed-survivor", "Survivor");
    let wallet_chain_uuid = "44444444444444444444444444444444";
    let record = session
        .clone_vault_view_unlock()
        .encrypt_record(
            RecordKind::WalletChainMetadata,
            wallet_chain_uuid,
            b"invalid wallet chain metadata",
        )
        .expect("encrypt malformed metadata plaintext");
    let chain_key = wallet_chain_metadata_record_key(wallet_chain_uuid);
    db.put_desktop_wallet_vault_record(
        &chain_key,
        &rmp_serde::to_vec_named(&record).expect("encode authenticated envelope"),
    )
    .expect("store authenticated malformed metadata");
    let completeness_key = wallet_chain_index_complete_record_key(wallet_id);
    db.delete_desktop_wallet_vault_record(&completeness_key)
        .expect("remove creation-time completeness");

    assert!(
        store
            .find_wallet_chain_metadata_for_session(
                &session,
                0,
                1,
                "0x1111111111111111111111111111111111111111",
            )
            .expect("scan authenticated malformed metadata")
            .is_none()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&completeness_key)
            .expect("load completeness marker")
            .is_none()
    );
    assert!(matches!(
        store.delete_wallet_for_session(&session, wallet_id),
        Err(VaultError::WalletChainMetadataUnavailable)
    ));
    assert!(
        db.get_desktop_wallet_vault_record(&wallet_metadata_record_key(wallet_id))
            .expect("load retained wallet metadata")
            .is_some()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&chain_key)
            .expect("load retained malformed metadata")
            .is_some()
    );

    drop(session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
