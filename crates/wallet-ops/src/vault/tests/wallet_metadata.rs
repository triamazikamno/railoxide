use super::super::*;
use super::helpers::*;
use alloy::primitives::FixedBytes;
use local_db::{
    OpaqueWalletPrivateRow, OutputPoiRecoveryRecord, OutputPoiRecoveryStatus,
    PendingOutputPoiContextRecord, PendingOutputPoiRole, WalletMeta, WalletPrivateNamespaceId,
    WalletPrivateRecordKind, WalletSyncActorStateRecord,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;

#[derive(Serialize)]
struct ReleasedWalletMetadataWithoutContext {
    wallet_uuid: String,
    label: String,
    derivation_index: u32,
    source: WalletSource,
    status: WalletStatus,
    display_order: u32,
    hardware_descriptor: Option<HardwareDerivationDescriptor>,
    hardware_account: Option<HardwareRailgunAccountMetadata>,
    pending_create_new_chain_ids: BTreeSet<u64>,
}

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
