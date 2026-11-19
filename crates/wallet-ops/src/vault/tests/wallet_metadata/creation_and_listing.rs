use super::*;

#[test]
fn wallet_metadata_flows_auto_create_initial_public_account() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let generated_seed = generate_seed_material().expect("generate seed");
    let generated_wallet_id = "generated-public-wallet";
    let generated_metadata = store
        .new_wallet_metadata(
            TEST_PASSWORD,
            generated_wallet_id,
            0,
            WalletSource::Generated,
            "Generated",
        )
        .expect("generated wallet metadata");
    store
        .store_generated_wallet_with_metadata(
            TEST_PASSWORD,
            generated_wallet_id,
            0,
            "english",
            &generated_seed,
            &generated_metadata,
        )
        .expect("store generated wallet with metadata");
    assert!(
        db.get_desktop_wallet_vault_record(&wallet_chain_index_complete_record_key(
            generated_wallet_id,
        ))
        .expect("load generated ownership completeness")
        .is_some()
    );
    let generated_session = store
        .load_view_session(TEST_PASSWORD, generated_wallet_id)
        .expect("generated view session");
    let generated_accounts = store
        .list_active_public_accounts_for_session(&generated_session)
        .expect("generated public accounts");
    assert_eq!(generated_accounts.len(), 1);
    assert_eq!(generated_accounts[0].source, PublicAccountSource::Derived);
    assert_eq!(generated_accounts[0].label.as_deref(), Some("Account #1"));
    assert_eq!(generated_accounts[0].derivation_index, Some(0));
    assert_eq!(
        generated_accounts[0].address,
        derive_public_evm_address_from_entropy(generated_seed.entropy.as_slice(), 0)
            .expect("generated public address")
    );

    let imported_wallet_id = "imported-public-wallet";
    let imported_metadata = store
        .new_wallet_metadata(
            TEST_PASSWORD,
            imported_wallet_id,
            0,
            WalletSource::Imported,
            "Imported",
        )
        .expect("imported wallet metadata");
    store
        .import_wallet_mnemonic_with_metadata(
            TEST_PASSWORD,
            imported_wallet_id,
            0,
            "english",
            TEST_MNEMONIC,
            &imported_metadata,
        )
        .expect("import wallet with metadata");
    let imported_session = store
        .load_view_session(TEST_PASSWORD, imported_wallet_id)
        .expect("imported view session");
    let imported_accounts = store
        .list_active_public_accounts_for_session(&imported_session)
        .expect("imported public accounts");
    let imported_entropy = bip39_entropy_from_mnemonic(TEST_MNEMONIC).expect("mnemonic entropy");
    assert_eq!(imported_accounts.len(), 1);
    assert_eq!(imported_accounts[0].source, PublicAccountSource::Derived);
    assert_eq!(imported_accounts[0].label.as_deref(), Some("Account #1"));
    assert_eq!(imported_accounts[0].derivation_index, Some(0));
    assert_eq!(
        imported_accounts[0].address,
        derive_public_evm_address_from_entropy(&imported_entropy, 0)
            .expect("imported public address")
    );

    let legacy_wallet_id = "metadata-less-public-wallet";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, legacy_wallet_id, 0, "english", TEST_MNEMONIC)
        .expect("import metadata-less wallet");
    let legacy_session = store
        .load_view_session(TEST_PASSWORD, legacy_wallet_id)
        .expect("legacy view session");
    assert_eq!(
        store
            .list_active_public_accounts_for_session(&legacy_session)
            .expect("legacy public accounts"),
        [] as [PublicAccountMetadata; 0]
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn wallet_metadata_listing_defaults_and_synthesizes_records() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let legacy_wallet_id = "legacy-wallet";
    let missing_wallet_id = "missing-metadata-wallet";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, legacy_wallet_id, 0, "english", mnemonic)
        .expect("import legacy wallet");
    store
        .import_wallet_mnemonic(TEST_PASSWORD, missing_wallet_id, 1, "english", mnemonic)
        .expect("import metadata-less wallet");

    let legacy = LegacyWalletMetadataBundle {
        wallet_uuid: legacy_wallet_id.to_string(),
        label: "Legacy wallet".to_string(),
        derivation_index: 0,
    };
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let record = encrypt_serialized(
        view.view_dek(),
        RecordKind::WalletMetadata,
        legacy_wallet_id,
        &legacy,
    )
    .expect("encrypt legacy metadata");
    let (key, payload) = record
        .to_record_entry(wallet_metadata_record_key(legacy_wallet_id))
        .expect("encode legacy metadata");
    db.put_desktop_wallet_vault_records(&[(key, payload)])
        .expect("store legacy metadata");

    let metadata = store
        .list_wallet_metadata(TEST_PASSWORD)
        .expect("list wallet metadata");
    let legacy = metadata
        .iter()
        .find(|metadata| metadata.wallet_uuid == legacy_wallet_id)
        .expect("legacy metadata");
    let synthesized = metadata
        .iter()
        .find(|metadata| metadata.wallet_uuid == missing_wallet_id)
        .expect("synthesized metadata");

    assert_eq!(metadata.len(), 2);
    assert_eq!(legacy.status, WalletStatus::Active);
    assert_eq!(legacy.display_order, 0);
    assert!(legacy.pending_create_new_chain_ids.is_empty());
    assert_eq!(synthesized.label, "Wallet 2");
    assert_eq!(synthesized.derivation_index, 1);
    assert_eq!(synthesized.status, WalletStatus::Active);
    assert_eq!(synthesized.display_order, 1);
    assert!(synthesized.pending_create_new_chain_ids.is_empty());

    let persisted_legacy = store
        .load_wallet_metadata(TEST_PASSWORD, legacy_wallet_id)
        .expect("load persisted legacy metadata");
    let persisted_synthesized = store
        .load_wallet_metadata(TEST_PASSWORD, missing_wallet_id)
        .expect("load synthesized metadata");
    assert_eq!(persisted_legacy, legacy.clone());
    assert_eq!(persisted_synthesized, synthesized.clone());

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn standard_context_auto_open_preference_is_additive_and_encrypted() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "standard-auto-open-wallet";
    let session = import_wallet_with_metadata(&store, wallet_id, "Standard");
    let metadata = store
        .load_wallet_metadata_for_session(&session)
        .expect("load standard metadata");
    assert!(
        !metadata
            .software_context
            .as_ref()
            .expect("standard context")
            .auto_open_standard_context
    );

    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let updated = store
        .set_standard_context_auto_open_preference_with_view_unlock(&view, wallet_id, true)
        .expect("set standard auto-open preference");
    assert!(
        updated
            .software_context
            .as_ref()
            .expect("standard context")
            .auto_open_standard_context
    );
    let persisted = store
        .load_wallet_metadata(TEST_PASSWORD, wallet_id)
        .expect("load persisted metadata");
    assert!(
        persisted
            .software_context
            .as_ref()
            .expect("standard context")
            .auto_open_standard_context
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn metadata_normalization_preserves_encrypted_records_and_cache_namespace() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "legacy-contextless-wallet";
    let other_wallet_id = "legacy-contextless-other";
    let session = import_wallet_with_metadata(&store, wallet_id, "Legacy target");
    let _other_session = import_wallet_with_metadata(&store, other_wallet_id, "Other");
    let chain = store
        .wallet_chain_metadata_for_session(
            &session,
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("wallet chain metadata");
    let cache_key = chain
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("wallet cache key");
    db.put_wallet_utxo(&cache_key, "migration-row", b"encrypted cache row")
        .expect("store cache row");
    let cache_meta = WalletMeta {
        last_scanned_block: 123,
        updated_at: 456,
        last_scanned_block_hash: None,
    };
    db.put_wallet_meta(&cache_key, &cache_meta)
        .expect("store cache metadata");
    let cache_meta_before = rmp_serde::to_vec_named(
        &db.get_wallet_meta(&cache_key)
            .expect("snapshot cache metadata"),
    )
    .expect("encode cache metadata");

    let reordered = store
        .reorder_active_wallets_for_session(
            &session,
            &[other_wallet_id.to_owned(), wallet_id.to_owned()],
        )
        .expect("reorder wallets");
    assert_eq!(reordered[1].wallet_uuid, wallet_id);
    let hidden = store
        .set_wallet_active_for_session(&session, wallet_id, false)
        .expect("hide target wallet");
    assert_eq!(hidden.status, WalletStatus::Inactive);

    let before = store
        .load_wallet_metadata(TEST_PASSWORD, wallet_id)
        .expect("load metadata before migration");
    let view_payload = db
        .get_desktop_wallet_vault_record(&wallet_view_record_key(wallet_id))
        .expect("load view payload before migration")
        .expect("view payload");
    let spend_payload = db
        .get_desktop_wallet_vault_record(&wallet_spend_record_key(wallet_id))
        .expect("load spend payload before migration")
        .expect("spend payload");
    let account_before = store
        .list_active_public_accounts_for_session(&session)
        .expect("list public accounts")
        .into_iter()
        .next()
        .expect("derived public account");
    let account_id = account_before.public_account_uuid.clone();
    let account_payload = db
        .get_desktop_wallet_vault_record(&public_account_metadata_record_key(&account_id))
        .expect("load public account payload before migration")
        .expect("public account payload");
    let chain_payload = db
        .get_desktop_wallet_vault_record(&wallet_chain_metadata_record_key(
            &chain.wallet_chain_uuid,
        ))
        .expect("load chain payload before migration")
        .expect("chain payload");
    let cache_rows_before = db
        .list_wallet_utxos(&cache_key)
        .expect("load cache rows before migration");

    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let legacy = ReleasedWalletMetadataWithoutContext {
        wallet_uuid: before.wallet_uuid.clone(),
        label: before.label.clone(),
        derivation_index: before.derivation_index,
        source: before.source,
        status: before.status,
        display_order: before.display_order,
        hardware_descriptor: before.hardware_descriptor.clone(),
        hardware_account: before.hardware_account.clone(),
        pending_create_new_chain_ids: before.pending_create_new_chain_ids.clone(),
    };
    let legacy_record = encrypt_serialized(
        view.view_dek(),
        RecordKind::WalletMetadata,
        wallet_id,
        &legacy,
    )
    .expect("encrypt contextless metadata");
    let (legacy_key, legacy_payload) = legacy_record
        .to_record_entry(wallet_metadata_record_key(wallet_id))
        .expect("encode contextless metadata");
    db.put_desktop_wallet_vault_records(&[(legacy_key, legacy_payload)])
        .expect("store contextless metadata");

    let migrated = store
        .list_wallet_metadata(TEST_PASSWORD)
        .expect("normalize wallet metadata")
        .into_iter()
        .find(|metadata| metadata.wallet_uuid == wallet_id)
        .expect("migrated metadata");
    assert_eq!(migrated.wallet_uuid, before.wallet_uuid);
    assert_eq!(migrated.label, before.label);
    assert_eq!(migrated.status, before.status);
    assert_eq!(migrated.display_order, before.display_order);
    assert_eq!(
        migrated
            .software_context
            .expect("standard software context"),
        WalletSoftwareContext::standard(wallet_id)
    );
    let expected_wallet =
        wallet_keys_from_mnemonic(TEST_MNEMONIC, "", 0).expect("derive empty-passphrase identity");
    let reloaded_session = store
        .load_view_session(TEST_PASSWORD, wallet_id)
        .expect("reload migrated view session");
    assert_eq!(
        reloaded_session.scan_keys().master_public_key,
        expected_wallet.viewing.master_public_key
    );
    assert_eq!(
        reloaded_session.scan_keys().nullifying_key,
        expected_wallet.viewing.nullifying_key
    );
    let account_after = store
        .list_active_public_accounts_for_session(&reloaded_session)
        .expect("list migrated public accounts")
        .into_iter()
        .find(|account| account.public_account_uuid == account_id)
        .expect("migrated public account");
    assert_eq!(account_after.address, account_before.address);
    assert_eq!(
        db.get_desktop_wallet_vault_record(&wallet_view_record_key(wallet_id))
            .expect("reload view payload")
            .expect("view payload"),
        view_payload
    );
    assert_eq!(
        db.get_desktop_wallet_vault_record(&wallet_spend_record_key(wallet_id))
            .expect("reload spend payload")
            .expect("spend payload"),
        spend_payload
    );
    assert_eq!(
        db.get_desktop_wallet_vault_record(&public_account_metadata_record_key(&account_id))
            .expect("reload public account payload")
            .expect("public account payload"),
        account_payload
    );
    assert_eq!(
        db.get_desktop_wallet_vault_record(&wallet_chain_metadata_record_key(
            &chain.wallet_chain_uuid
        ))
        .expect("reload chain payload")
        .expect("chain payload"),
        chain_payload
    );
    assert_eq!(
        db.list_wallet_utxos(&cache_key).expect("reload cache rows"),
        cache_rows_before
    );
    assert_eq!(
        rmp_serde::to_vec_named(
            &db.get_wallet_meta(&cache_key)
                .expect("reload cache metadata")
        )
        .expect("encode reloaded cache metadata"),
        cache_meta_before
    );

    drop(session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn duplicate_mnemonic_legacy_wallets_normalize_to_independent_standard_profiles() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first_id = "duplicate-legacy-profile-a";
    let second_id = "duplicate-legacy-profile-b";
    let first = import_wallet_with_metadata(&store, first_id, "Duplicate A");
    let second = import_wallet_with_metadata(&store, second_id, "Duplicate B");
    let first_view_payload = db
        .get_desktop_wallet_vault_record(&wallet_view_record_key(first_id))
        .expect("load first view payload")
        .expect("first view payload");
    let second_view_payload = db
        .get_desktop_wallet_vault_record(&wallet_view_record_key(second_id))
        .expect("load second view payload")
        .expect("second view payload");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    for (wallet_id, label) in [(first_id, "Duplicate A"), (second_id, "Duplicate B")] {
        let legacy = LegacyWalletMetadataBundle {
            wallet_uuid: wallet_id.to_owned(),
            label: label.to_owned(),
            derivation_index: 0,
        };
        let record = encrypt_serialized(
            view.view_dek(),
            RecordKind::WalletMetadata,
            wallet_id,
            &legacy,
        )
        .expect("encrypt legacy metadata");
        let (key, payload) = record
            .to_record_entry(wallet_metadata_record_key(wallet_id))
            .expect("encode legacy metadata");
        db.put_desktop_wallet_vault_records(&[(key, payload)])
            .expect("store legacy metadata");
    }

    let metadata = store
        .list_wallet_metadata(TEST_PASSWORD)
        .expect("normalize duplicate metadata");
    for wallet_id in [first_id, second_id] {
        let context = metadata
            .iter()
            .find(|metadata| metadata.wallet_uuid == wallet_id)
            .and_then(|metadata| metadata.software_context.as_ref())
            .expect("independent standard context");
        assert_eq!(context.kind, WalletSoftwareContextKind::Standard);
        assert_eq!(context.base_profile_uuid, wallet_id);
    }
    assert_ne!(first_id, second_id);
    assert_eq!(
        db.get_desktop_wallet_vault_record(&wallet_view_record_key(first_id))
            .expect("reload first view payload")
            .expect("first view payload"),
        first_view_payload
    );
    assert_eq!(
        db.get_desktop_wallet_vault_record(&wallet_view_record_key(second_id))
            .expect("reload second view payload")
            .expect("second view payload"),
        second_view_payload
    );
    drop(first);
    drop(second);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn software_wallet_creation_persists_pending_chains_only_for_generated_wallets() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let pending_chain_ids = BTreeSet::from([1, 56, 137]);
    let generated_id = "generated-pending-wallet";
    let generated = store
        .new_wallet_metadata_with_pending_create_new_chain_ids(
            TEST_PASSWORD,
            generated_id,
            0,
            WalletSource::Generated,
            "Generated pending",
            pending_chain_ids.clone(),
        )
        .expect("create generated metadata");
    let seed = generate_seed_material().expect("generate seed");
    store
        .store_generated_wallet_with_metadata(
            TEST_PASSWORD,
            generated_id,
            0,
            "english",
            &seed,
            &generated,
        )
        .expect("atomically store generated wallet and metadata");
    assert_eq!(
        store
            .load_wallet_metadata(TEST_PASSWORD, generated_id)
            .expect("reload generated metadata")
            .pending_create_new_chain_ids,
        pending_chain_ids
    );

    let imported = store
        .new_wallet_metadata_with_pending_create_new_chain_ids(
            TEST_PASSWORD,
            "imported-no-pending-wallet",
            0,
            WalletSource::Imported,
            "Imported no pending",
            BTreeSet::from([1, 137]),
        )
        .expect("create imported metadata");
    assert!(imported.pending_create_new_chain_ids.is_empty());

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
