use super::*;

#[test]
fn duplicate_seed_imports_keep_distinct_wallet_and_chain_ids() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let first_wallet_id = "duplicate-seed-a";
    let first_metadata = store
        .new_wallet_metadata(
            TEST_PASSWORD,
            first_wallet_id,
            0,
            WalletSource::Imported,
            "Duplicate A",
        )
        .expect("first duplicate metadata");
    store
        .import_wallet_mnemonic_with_metadata(
            TEST_PASSWORD,
            first_wallet_id,
            0,
            "english",
            mnemonic,
            &first_metadata,
        )
        .expect("import first duplicate seed");

    let second_wallet_id = "duplicate-seed-b";
    let second_metadata = store
        .new_wallet_metadata(
            TEST_PASSWORD,
            second_wallet_id,
            0,
            WalletSource::Imported,
            "Duplicate B",
        )
        .expect("second duplicate metadata");
    store
        .import_wallet_mnemonic_with_metadata(
            TEST_PASSWORD,
            second_wallet_id,
            0,
            "english",
            mnemonic,
            &second_metadata,
        )
        .expect("import second duplicate seed");

    let first_session = store
        .load_view_session(TEST_PASSWORD, first_wallet_id)
        .expect("load first session");
    let second_session = store
        .load_view_session(TEST_PASSWORD, second_wallet_id)
        .expect("load second session");
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
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("second chain metadata");

    assert_ne!(first_wallet_id, second_wallet_id);
    assert_ne!(
        first_chain.wallet_chain_uuid,
        second_chain.wallet_chain_uuid
    );
    assert_eq!(first_chain.wallet_uuid, first_wallet_id);
    assert_eq!(second_chain.wallet_uuid, second_wallet_id);
    assert_eq!(
        first_session.scan_keys().master_public_key,
        second_session.scan_keys().master_public_key
    );
    assert_eq!(
        first_session.scan_keys().nullifying_key,
        second_session.scan_keys().nullifying_key
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn opaque_wallet_metadata_keeps_chain_details_encrypted() {
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    store
        .put_metadata(&created.metadata)
        .expect("persist metadata");
    let wallet_uuid = generate_opaque_id().expect("wallet uuid");
    let wallet_chain_uuid = generate_opaque_id().expect("wallet chain uuid");
    let wallet_metadata = WalletMetadataBundle {
        wallet_uuid: wallet_uuid.clone(),
        label: "primary wallet".to_string(),
        derivation_index: 0,
        source: WalletSource::Imported,
        status: WalletStatus::Active,
        display_order: 0,
        hardware_descriptor: None,
        hardware_account: None,
        pending_create_new_chain_ids: BTreeSet::new(),
        software_context: Some(WalletSoftwareContext::standard(wallet_uuid.clone())),
    };
    let chain_metadata = WalletChainMetadataBundle {
        wallet_chain_uuid: wallet_chain_uuid.clone(),
        wallet_uuid: wallet_uuid.clone(),
        chain_type: 0,
        chain_id: 1,
        contract: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
        start_block: 100,
        last_scanned_block: 200,
        last_scanned_block_hash: Some([9u8; KEY_LEN]),
        poi_read_source: None,
    };

    store
        .store_wallet_metadata(TEST_PASSWORD, &wallet_metadata)
        .expect("store wallet metadata");
    store
        .store_wallet_chain_metadata(TEST_PASSWORD, &chain_metadata)
        .expect("store chain metadata");
    let wallet_payload = db
        .get_desktop_wallet_vault_record(&wallet_metadata_record_key(&wallet_uuid))
        .expect("load wallet metadata record")
        .expect("wallet metadata present");
    let chain_payload = db
        .get_desktop_wallet_vault_record(&wallet_chain_metadata_record_key(&wallet_chain_uuid))
        .expect("load chain metadata record")
        .expect("chain metadata present");
    let loaded_wallet = store
        .load_wallet_metadata(TEST_PASSWORD, &wallet_uuid)
        .expect("load wallet metadata");
    let loaded_chain = store
        .load_wallet_chain_metadata(TEST_PASSWORD, &wallet_chain_uuid)
        .expect("load chain metadata");

    assert_eq!(wallet_uuid.len(), 32);
    assert_eq!(wallet_chain_uuid.len(), 32);
    assert_eq!(loaded_wallet.label, "primary wallet");
    assert_eq!(loaded_chain.chain_id, 1);
    assert_eq!(loaded_chain.contract, chain_metadata.contract);
    assert!(!contains_subsequence(&wallet_payload, b"primary wallet"));
    assert!(!contains_subsequence(&chain_payload, b"1234567890abcdef"));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn concurrent_wallet_chain_find_or_create_persists_one_metadata_and_ownership_record() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "concurrent-chain-wallet";
    let session = Arc::new(import_wallet_with_metadata(&store, wallet_id, "Concurrent"));
    let contract = "0x1111111111111111111111111111111111111111";
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();

    for start_block in [100, 200] {
        let db = Arc::clone(&db);
        let session = Arc::clone(&session);
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            let store = DesktopVaultStore::from_db(db);
            barrier.wait();
            store
                .find_or_create_wallet_chain_metadata_for_session(
                    session.as_ref(),
                    0,
                    1,
                    contract,
                    start_block,
                    start_block - 1,
                )
                .expect("find or create chain metadata")
        }));
    }

    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().expect("join metadata creator"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1);
    assert_eq!(
        results[0].0.wallet_chain_uuid,
        results[1].0.wallet_chain_uuid
    );
    assert_eq!(
        db.list_desktop_wallet_vault_records(WALLET_CHAIN_METADATA_PREFIX)
            .expect("list chain metadata")
            .len(),
        1
    );
    assert_eq!(
        db.list_desktop_wallet_vault_records(&wallet_chain_index_prefix(wallet_id))
            .expect("list chain ownership records")
            .len(),
        1
    );

    drop(session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
