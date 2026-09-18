use super::super::*;
use super::helpers::*;
use local_db::{
    OpaqueWalletPrivateRow, WalletMeta, WalletPrivateNamespaceId, WalletPrivateRecordKind,
};
use std::fs;
use std::sync::Arc;
use zeroize::Zeroizing;

const PASSPHRASE: &str = "TREZOR";
const CONTEXT_WALLET_ID: &str = "70617373706872617373652d636f6e74";

fn add_passphrase_context(
    store: &DesktopVaultStore,
    db: &DbStore,
    base_profile_uuid: &str,
) -> WalletMetadataBundle {
    add_passphrase_context_with_ids(
        store,
        db,
        base_profile_uuid,
        CONTEXT_WALLET_ID,
        "context-public-account",
        "Passphrase context",
    )
}

fn add_passphrase_context_with_ids(
    store: &DesktopVaultStore,
    db: &DbStore,
    base_profile_uuid: &str,
    context_wallet_id: &str,
    public_account_id: &str,
    label: &str,
) -> WalletMetadataBundle {
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let metadata = store
        .new_passphrase_context_metadata_with_view_unlock(
            &view,
            base_profile_uuid,
            context_wallet_id,
            3,
            label,
        )
        .expect("context metadata");
    let wallet =
        wallet_keys_from_mnemonic(TEST_MNEMONIC, PASSPHRASE, 3).expect("context wallet keys");
    let view_bundle = WalletViewBundle::from_wallet_keys(3, &wallet);
    store
        .store_passphrase_context_with_view_bundle_with_view_unlock(
            &view,
            base_profile_uuid,
            context_wallet_id,
            &view_bundle,
            &metadata,
        )
        .expect("store context");

    let account = PublicAccountMetadata {
        public_account_uuid: public_account_id.to_owned(),
        address: derive_public_evm_address_from_mnemonic_with_passphrase(
            TEST_MNEMONIC,
            PASSPHRASE,
            0,
        )
        .expect("context EVM address"),
        label: Some("Context account".to_owned()),
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: context_wallet_id.to_owned(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let account_record = view
        .encrypt_public_account_metadata(&account.public_account_uuid, &account)
        .expect("encrypt context account");
    let (key, data) = account_record
        .to_record_entry(public_account_metadata_record_key(
            &account.public_account_uuid,
        ))
        .expect("encode context account");
    db.put_desktop_wallet_vault_record(&key, &data)
        .expect("persist context account");
    metadata
}

#[test]
fn protected_seed_session_binds_context_and_vault_session_and_is_one_use() {
    let created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    let seed = bip39_seed_from_mnemonic(TEST_MNEMONIC, PASSPHRASE).expect("derive seed");
    let first_binding = SoftwareSeedSessionBinding::new(
        "base-profile",
        "context-wallet",
        VaultSessionId::from_bytes([1; 16]),
    );
    let session = created
        .spend
        .seal_software_seed_session(first_binding.clone(), &*seed)
        .expect("seal seed");

    let wrong_context = SoftwareSeedSessionBinding::new(
        "base-profile",
        "other-context",
        VaultSessionId::from_bytes([1; 16]),
    );
    let wrong_base = SoftwareSeedSessionBinding::new(
        "other-base",
        "context-wallet",
        VaultSessionId::from_bytes([1; 16]),
    );
    let wrong_session = SoftwareSeedSessionBinding::new(
        "base-profile",
        "context-wallet",
        VaultSessionId::from_bytes([2; 16]),
    );
    assert!(
        created
            .spend
            .decrypt_record(
                RecordKind::SoftwareContextSeed,
                &wrong_base.record_id(),
                session.encrypted_record(),
            )
            .is_err()
    );
    assert!(
        created
            .spend
            .decrypt_record(
                RecordKind::SoftwareContextSeed,
                &wrong_context.record_id(),
                session.encrypted_record(),
            )
            .is_err()
    );
    assert!(
        created
            .spend
            .decrypt_record(
                RecordKind::SoftwareContextSeed,
                &wrong_session.record_id(),
                session.encrypted_record(),
            )
            .is_err()
    );
    let mut grant = create_spend_grant(&created.metadata, TEST_PASSWORD).expect("grant");
    assert!(matches!(
        session.open(&mut grant, &wrong_context),
        Err(VaultError::SoftwareSeedSessionBindingMismatch)
    ));
    assert!(grant.is_valid());
    assert!(matches!(
        session.open(&mut grant, &wrong_base),
        Err(VaultError::SoftwareSeedSessionBindingMismatch)
    ));
    assert!(matches!(
        session.open(&mut grant, &wrong_session),
        Err(VaultError::SoftwareSeedSessionBindingMismatch)
    ));

    assert!(
        created
            .view
            .decrypt_record(
                RecordKind::SoftwareContextSeed,
                &first_binding.record_id(),
                session.encrypted_record(),
            )
            .is_err()
    );

    let mut open_grant = create_spend_grant(&created.metadata, TEST_PASSWORD).expect("open grant");
    let opened = session
        .open(&mut open_grant, &first_binding)
        .expect("open seed");
    assert_eq!(&*opened, &*seed);
    assert!(!open_grant.is_valid());
    assert!(matches!(
        session.open(&mut open_grant, &first_binding),
        Err(VaultError::InvalidSpendGrant)
    ));
}

#[test]
fn matching_known_context_returns_existing_metadata_and_protected_session() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "base-profile", "Base");
    let base_metadata = store
        .load_wallet_metadata(TEST_PASSWORD, base.wallet_id())
        .expect("base metadata");
    let context_metadata = add_passphrase_context(&store, &db, base.wallet_id());
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("matching grant");
    let session_id = VaultSessionId::from_bytes([3; 16]);

    let result = store
        .match_software_context(
            &view,
            &base_metadata,
            &mut grant,
            Zeroizing::new(PASSPHRASE.to_owned()),
            session_id,
        )
        .expect("match context");
    let SoftwareContextMatch::Known { metadata, session } = result else {
        panic!("known context was not returned");
    };
    assert_eq!(*metadata, context_metadata);
    assert_eq!(session.binding().base_profile_uuid(), base.wallet_id());
    assert_eq!(session.binding().context_wallet_uuid(), CONTEXT_WALLET_ID);
    assert_eq!(session.binding().vault_session_id(), session_id);
    assert!(!grant.is_valid());

    let retry_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("retry grant");
    let spend_unlock = retry_grant.spend_unlock().expect("borrowed spend unlock");
    assert!(matches!(
        store
            .match_software_context_with_spend_unlock_ref(
                &view,
                &base_metadata,
                spend_unlock,
                PASSPHRASE,
                session_id,
            )
            .expect("retry matching"),
        SoftwareContextMatch::Known { .. }
    ));
    assert!(retry_grant.is_valid());

    drop(session);
    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn standard_base_delete_is_blocked_while_passphrase_context_exists() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "delete-profile-base", "Base");
    add_passphrase_context(&store, &db, base.wallet_id());
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");

    assert!(matches!(
        store.delete_wallet_with_view_unlock(&view, base.wallet_id()),
        Err(VaultError::StandardWalletHasPassphraseChildren)
    ));

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db");
}

#[test]
fn whole_profile_delete_removes_child_records_and_keeps_an_outside_wallet() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base_id = generate_opaque_id().expect("base wallet id");
    let second_context_id = generate_opaque_id().expect("second context wallet id");
    let base = import_wallet_with_metadata(&store, &base_id, "Base");
    let _survivor = import_wallet_with_metadata(&store, "delete-whole-survivor", "Survivor");
    add_passphrase_context(&store, &db, base.wallet_id());
    add_passphrase_context_with_ids(
        &store,
        &db,
        base.wallet_id(),
        &second_context_id,
        "second-context-public-account",
        "Second passphrase context",
    );
    let base_account_id = store
        .list_active_public_accounts_for_session(&base)
        .expect("base public account")
        .into_iter()
        .next()
        .expect("base derived account")
        .public_account_uuid;
    let child_session = store
        .load_view_session(TEST_PASSWORD, CONTEXT_WALLET_ID)
        .expect("child session");
    let second_child_session = store
        .load_view_session(TEST_PASSWORD, &second_context_id)
        .expect("second child session");
    let base_chain = store
        .wallet_chain_metadata_for_session(
            &base,
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("base chain metadata");
    let child_chain = store
        .wallet_chain_metadata_for_session(
            &child_session,
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("child chain metadata");
    let second_child_chain = store
        .wallet_chain_metadata_for_session(
            &second_child_session,
            0,
            1,
            "0x1111111111111111111111111111111111111111",
            100,
        )
        .expect("second child chain metadata");
    let child_cache_key = child_chain
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("child cache key");
    let second_child_cache_key = second_child_chain
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("second child cache key");
    let base_cache_key = base_chain
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("base cache key");
    for (cache_key, row_id) in [
        (&base_cache_key, "base-private-row"),
        (&child_cache_key, "child-private-row"),
        (&second_child_cache_key, "second-child-private-row"),
    ] {
        db.put_wallet_utxo(cache_key, row_id, b"encrypted private row")
            .expect("store wallet cache row");
        db.put_wallet_meta(
            cache_key,
            &WalletMeta {
                last_scanned_block: 123,
                updated_at: 456,
                last_scanned_block_hash: None,
            },
        )
        .expect("store wallet checkpoint");
        let namespace = WalletPrivateNamespaceId::new(1, (*cache_key).clone());
        db.put_opaque_wallet_private_row(
            &namespace,
            WalletPrivateRecordKind::SenderTransactionCandidate,
            &OpaqueWalletPrivateRow {
                row_id: vec![0x61; 32],
                payload: b"encrypted private namespace row".to_vec(),
            },
        )
        .expect("store wallet private namespace row");
    }
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");

    let deleted = store
        .delete_software_profile_with_view_unlock(&view, base.wallet_id())
        .expect("delete whole software profile");
    assert_eq!(
        deleted
            .iter()
            .map(|metadata| metadata.wallet_uuid.as_str())
            .collect::<Vec<_>>(),
        vec![
            CONTEXT_WALLET_ID,
            second_context_id.as_str(),
            base.wallet_id()
        ]
    );

    let remaining = store
        .list_wallet_metadata(TEST_PASSWORD)
        .expect("list remaining metadata");
    assert_eq!(
        remaining
            .iter()
            .map(|metadata| metadata.wallet_uuid.as_str())
            .collect::<Vec<_>>(),
        vec!["delete-whole-survivor"]
    );
    assert!(
        store
            .load_view_session(TEST_PASSWORD, base.wallet_id())
            .is_err()
    );
    for (wallet_id, chain, cache_key, account_id) in [
        (
            base.wallet_id(),
            &base_chain,
            &base_cache_key,
            base_account_id.as_str(),
        ),
        (
            CONTEXT_WALLET_ID,
            &child_chain,
            &child_cache_key,
            "context-public-account",
        ),
        (
            second_context_id.as_str(),
            &second_child_chain,
            &second_child_cache_key,
            "second-context-public-account",
        ),
    ] {
        assert!(store.load_view_session(TEST_PASSWORD, wallet_id).is_err());
        for key in [
            wallet_metadata_record_key(wallet_id),
            wallet_view_record_key(wallet_id),
            wallet_spend_record_key(wallet_id),
            wallet_chain_metadata_record_key(&chain.wallet_chain_uuid),
            wallet_chain_index_record_key(wallet_id, &chain.wallet_chain_uuid),
            wallet_chain_index_complete_record_key(wallet_id),
            public_account_metadata_record_key(account_id),
        ] {
            assert!(
                db.get_desktop_wallet_vault_record(&key)
                    .expect("load deleted profile record")
                    .is_none(),
                "expected {key} to be deleted"
            );
        }
        assert!(
            db.list_wallet_utxos(cache_key)
                .expect("list deleted wallet cache rows")
                .is_empty()
        );
        assert!(
            db.get_wallet_meta(cache_key)
                .expect("load deleted checkpoint")
                .is_none()
        );
        let namespace = WalletPrivateNamespaceId::new(1, (*cache_key).clone());
        assert!(
            db.list_opaque_wallet_private_rows(
                &namespace,
                WalletPrivateRecordKind::SenderTransactionCandidate,
            )
            .expect("list deleted private rows")
            .is_empty()
        );
    }
    assert!(
        store
            .load_view_session(TEST_PASSWORD, "delete-whole-survivor")
            .is_ok()
    );

    drop(view);
    drop(child_session);
    drop(second_child_session);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db");
}

#[test]
fn whole_profile_delete_reenumerates_children_after_partial_progress() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base_id = generate_opaque_id().expect("base wallet id");
    let remaining_child_id = generate_opaque_id().expect("remaining child id");
    let base = import_wallet_with_metadata(&store, &base_id, "Retry base");
    let _survivor = import_wallet_with_metadata(&store, "retry-survivor", "Survivor");
    add_passphrase_context(&store, &db, base.wallet_id());
    add_passphrase_context_with_ids(
        &store,
        &db,
        base.wallet_id(),
        &remaining_child_id,
        "retry-remaining-account",
        "Remaining passphrase context",
    );
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");

    store
        .delete_wallet_with_view_unlock(&view, CONTEXT_WALLET_ID)
        .expect("delete first child before retry");
    assert!(
        store
            .load_view_session(TEST_PASSWORD, CONTEXT_WALLET_ID)
            .is_err()
    );

    let deleted = store
        .delete_software_profile_with_view_unlock(&view, base.wallet_id())
        .expect("retry whole profile deletion");
    assert_eq!(
        deleted
            .iter()
            .map(|metadata| metadata.wallet_uuid.as_str())
            .collect::<Vec<_>>(),
        vec![remaining_child_id.as_str(), base.wallet_id()]
    );
    assert!(
        store
            .load_view_session(TEST_PASSWORD, &remaining_child_id)
            .is_err()
    );
    assert!(
        store
            .load_view_session(TEST_PASSWORD, base.wallet_id())
            .is_err()
    );
    assert!(
        store
            .load_view_session(TEST_PASSWORD, "retry-survivor")
            .is_ok()
    );

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db");
}

#[test]
fn context_export_maps_mnemonic_to_base_and_view_key_to_child() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "export-context-base", "Base");
    add_passphrase_context(&store, &db, base.wallet_id());

    let mnemonic = store
        .export_wallet_mnemonic_for_context(TEST_PASSWORD, CONTEXT_WALLET_ID)
        .expect("export base mnemonic from child");
    let standard_mnemonic = store
        .export_wallet_mnemonic_for_context(TEST_PASSWORD, base.wallet_id())
        .expect("export standard mnemonic");
    assert_eq!(&*mnemonic, &*standard_mnemonic);
    assert_eq!(&*mnemonic, TEST_MNEMONIC);

    let child_view_key = store
        .export_wallet_shareable_viewing_key_for_context(TEST_PASSWORD, CONTEXT_WALLET_ID)
        .expect("export child view key");
    let base_view_key = store
        .export_wallet_shareable_viewing_key_for_context(TEST_PASSWORD, base.wallet_id())
        .expect("export base view key");
    assert_ne!(&*child_view_key, &*base_view_key);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db");
}

#[test]
fn unknown_exact_passphrase_variant_does_not_write_storage() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "unknown-base", "Base");
    let base_metadata = store
        .load_wallet_metadata(TEST_PASSWORD, base.wallet_id())
        .expect("base metadata");
    add_passphrase_context(&store, &db, base.wallet_id());
    let before = db
        .list_desktop_wallet_vault_records("")
        .expect("snapshot records");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("matching grant");

    let result = store
        .match_software_context(
            &view,
            &base_metadata,
            &mut grant,
            Zeroizing::new("TREZOR ".to_owned()),
            VaultSessionId::from_bytes([4; 16]),
        )
        .expect("unknown passphrase");
    assert!(matches!(result, SoftwareContextMatch::Unknown));
    assert_eq!(
        db.list_desktop_wallet_vault_records("")
            .expect("records after unknown"),
        before
    );

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn duplicate_context_public_identity_is_rejected_without_persisting_secret_fields() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "duplicate-base", "Base");
    let base_metadata = store
        .load_wallet_metadata(TEST_PASSWORD, base.wallet_id())
        .expect("base metadata");
    let context_metadata = add_passphrase_context(&store, &db, base.wallet_id());
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let duplicate = PublicAccountMetadata {
        public_account_uuid: "context-public-account-duplicate".to_owned(),
        address: derive_public_evm_address_from_mnemonic_with_passphrase(
            TEST_MNEMONIC,
            PASSPHRASE,
            0,
        )
        .expect("context EVM address"),
        label: None,
        source: PublicAccountSource::Derived,
        scope: PublicAccountScope::PrivateWallet {
            wallet_uuid: context_metadata.wallet_uuid.clone(),
        },
        derivation_index: Some(0),
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 1,
    };
    let record = view
        .encrypt_public_account_metadata(&duplicate.public_account_uuid, &duplicate)
        .expect("encrypt duplicate account");
    let (key, data) = record
        .to_record_entry(public_account_metadata_record_key(
            &duplicate.public_account_uuid,
        ))
        .expect("encode duplicate account");
    db.put_desktop_wallet_vault_record(&key, &data)
        .expect("persist duplicate account");
    let serialized = serde_json::to_string(&context_metadata).expect("serialize metadata");
    assert!(!serialized.contains(PASSPHRASE));
    assert!(!serialized.contains("seed"));

    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("matching grant");
    assert!(matches!(
        store.match_software_context(
            &view,
            &base_metadata,
            &mut grant,
            Zeroizing::new(PASSPHRASE.to_owned()),
            VaultSessionId::from_bytes([5; 16]),
        ),
        Err(VaultError::DuplicateSoftwareContextIdentity)
    ));

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn malformed_context_identity_is_rejected() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "malformed-base", "Base");
    let base_metadata = store
        .load_wallet_metadata(TEST_PASSWORD, base.wallet_id())
        .expect("base metadata");
    add_passphrase_context(&store, &db, base.wallet_id());
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let wallet =
        wallet_keys_from_mnemonic(TEST_MNEMONIC, PASSPHRASE, 3).expect("context wallet keys");
    let malformed_bundle = WalletViewBundle::from_wallet_keys(99, &wallet);
    let record = view
        .encrypt_view_bundle(CONTEXT_WALLET_ID, &malformed_bundle)
        .expect("encrypt malformed view bundle");
    let (key, data) = record
        .to_record_entry(wallet_view_record_key(CONTEXT_WALLET_ID))
        .expect("encode malformed view bundle");
    db.put_desktop_wallet_vault_record(&key, &data)
        .expect("persist malformed view bundle");

    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("matching grant");
    assert!(matches!(
        store.match_software_context(
            &view,
            &base_metadata,
            &mut grant,
            Zeroizing::new(PASSPHRASE.to_owned()),
            VaultSessionId::from_bytes([6; 16]),
        ),
        Err(VaultError::InvalidSoftwareContextIdentity)
    ));

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn create_software_context_requires_exact_confirmation_without_writing() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "creation-base", "Base");
    let before = db
        .list_desktop_wallet_vault_records("")
        .expect("snapshot records");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("spend grant");
    let result = store.create_software_context(
        &view,
        &mut grant,
        base.wallet_id(),
        "creation-child",
        7,
        "Child",
        Zeroizing::new("exact passphrase".to_owned()),
        Zeroizing::new("different passphrase".to_owned()),
        SoftwareContextSyncIntent::CreateNew,
        &[],
        VaultSessionId::from_bytes([21; 16]),
    );
    assert!(matches!(
        result,
        Err(VaultError::PassphraseConfirmationMismatch)
    ));
    assert_eq!(
        db.list_desktop_wallet_vault_records("")
            .expect("records after mismatch"),
        before
    );
    assert!(grant.is_valid());

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn create_software_context_rejects_exact_empty_passphrase_without_writing() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "empty-passphrase-base", "Base");
    let before = db
        .list_desktop_wallet_vault_records("")
        .expect("snapshot records");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("spend grant");
    let result = store.create_software_context(
        &view,
        &mut grant,
        base.wallet_id(),
        "empty-passphrase-child",
        0,
        "Empty passphrase child",
        Zeroizing::new(String::new()),
        Zeroizing::new(String::new()),
        SoftwareContextSyncIntent::RecoverExisting,
        &[],
        VaultSessionId::from_bytes([29; 16]),
    );
    assert!(matches!(
        result,
        Err(VaultError::EmptySoftwareContextPassphrase)
    ));
    assert!(grant.is_valid());
    assert_eq!(
        db.list_desktop_wallet_vault_records("")
            .expect("records after empty passphrase"),
        before
    );

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn create_software_context_rejects_duplicate_uuid_and_label_without_writing() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "duplicate-create-base", "Base");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    for (wallet_uuid, label, expected) in [
        (
            base.wallet_id(),
            "New label",
            VaultError::DuplicateWalletUuid,
        ),
        (
            "duplicate-create-child",
            "Base",
            VaultError::DuplicateWalletLabel,
        ),
    ] {
        let before = db
            .list_desktop_wallet_vault_records("")
            .expect("snapshot records");
        let mut grant = store
            .create_spend_grant(TEST_PASSWORD)
            .expect("spend grant");
        let result = store.create_software_context(
            &view,
            &mut grant,
            base.wallet_id(),
            wallet_uuid,
            0,
            label,
            Zeroizing::new(format!("unique-{wallet_uuid}")),
            Zeroizing::new(format!("unique-{wallet_uuid}")),
            SoftwareContextSyncIntent::RecoverExisting,
            &[],
            VaultSessionId::from_bytes([28; 16]),
        );
        let Err(actual) = result else {
            panic!("duplicate context input unexpectedly succeeded");
        };
        assert_eq!(actual.to_string(), expected.to_string());
        assert_eq!(
            db.list_desktop_wallet_vault_records("")
                .expect("records after duplicate failure"),
            before
        );
    }

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn create_software_context_is_atomic_and_uses_independent_indexes() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "atomic-base", "Base");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("spend grant");
    let result = store
        .create_software_context(
            &view,
            &mut grant,
            base.wallet_id(),
            "atomic-child",
            7,
            "Child",
            Zeroizing::new(PASSPHRASE.to_owned()),
            Zeroizing::new(PASSPHRASE.to_owned()),
            SoftwareContextSyncIntent::CreateNew,
            &[SoftwareContextChainInput {
                chain_type: 0,
                chain_id: 1,
                contract: "0xcontract".to_owned(),
                deployment_block: 10,
                current_safe_head: Some(42),
            }],
            VaultSessionId::from_bytes([22; 16]),
        )
        .expect("create context");
    let CreateSoftwareContextResult::Created {
        metadata,
        public_account,
        chain_metadata,
        protected_seed_session,
    } = result
    else {
        panic!("expected created context");
    };
    assert_eq!(metadata.wallet_uuid, "atomic-child");
    assert_eq!(metadata.derivation_index, 7);
    assert_eq!(public_account.derivation_index, Some(0));
    assert_eq!(chain_metadata[0].start_block, 43);
    assert_eq!(chain_metadata[0].last_scanned_block, 42);
    assert_eq!(
        protected_seed_session.binding().context_wallet_uuid(),
        "atomic-child"
    );
    assert!(!grant.is_valid());
    assert!(
        db.get_desktop_wallet_vault_record("wallet-spend|atomic-child")
            .expect("child spend record lookup")
            .is_none()
    );
    assert!(
        db.get_desktop_wallet_vault_record("wallet-view|atomic-child")
            .expect("child view record lookup")
            .is_some()
    );
    let context_view = store
        .load_view_session(TEST_PASSWORD, "atomic-child")
        .expect("load context view");
    let accounts = store
        .list_public_accounts_for_session(&context_view, false)
        .expect("context accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        accounts[0].public_account_uuid,
        public_account.public_account_uuid
    );
    assert_eq!(accounts[0].derivation_index, Some(0));
    assert_eq!(
        accounts[0].address,
        derive_public_evm_address_from_mnemonic_with_passphrase(TEST_MNEMONIC, PASSPHRASE, 0)
            .expect("context address")
    );
    let stored_view = store
        .load_view_bundle(TEST_PASSWORD, "atomic-child")
        .expect("stored context view");
    let expected_wallet =
        wallet_keys_from_mnemonic(TEST_MNEMONIC, PASSPHRASE, 7).expect("context railgun keys");
    assert_eq!(
        stored_view.spending_public_key,
        expected_wallet
            .spending_public_key
            .map(|key| key.to_be_bytes())
    );
    let context_seed =
        bip39_seed_from_mnemonic(TEST_MNEMONIC, PASSPHRASE).expect("context BIP39 seed");
    for record in db
        .list_desktop_wallet_vault_records("")
        .expect("enumerate vault records")
    {
        assert!(!contains_subsequence(
            &record.payload,
            PASSPHRASE.as_bytes()
        ));
        assert!(!contains_subsequence(&record.payload, &context_seed[..]));
    }
    if let Some(settings) = db
        .get_app_settings_record("wallet-settings")
        .expect("load wallet settings")
    {
        assert!(!contains_subsequence(&settings, PASSPHRASE.as_bytes()));
        assert!(!contains_subsequence(&settings, &context_seed[..]));
    }

    drop(context_view);
    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn create_software_context_recover_baseline_and_known_reopen_are_no_write_duplicates() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "recover-base", "Base");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("spend grant");
    let result = store
        .create_software_context(
            &view,
            &mut grant,
            base.wallet_id(),
            "recover-child",
            2,
            "Recovered",
            Zeroizing::new(PASSPHRASE.to_owned()),
            Zeroizing::new(PASSPHRASE.to_owned()),
            SoftwareContextSyncIntent::RecoverExisting,
            &[SoftwareContextChainInput {
                chain_type: 0,
                chain_id: 1,
                contract: "0xcontract".to_owned(),
                deployment_block: 17,
                current_safe_head: None,
            }],
            VaultSessionId::from_bytes([23; 16]),
        )
        .expect("recover context");
    let CreateSoftwareContextResult::Created { chain_metadata, .. } = result else {
        panic!("expected recovered context");
    };
    assert_eq!(chain_metadata[0].start_block, 17);
    assert_eq!(chain_metadata[0].last_scanned_block, 16);

    let context_session = store
        .load_view_session(TEST_PASSWORD, "recover-child")
        .expect("recovered context session");
    let chain = chain_metadata[0].clone();
    let cache_key = chain
        .wallet_chain_uuid
        .parse::<WalletCacheKey>()
        .expect("recovered cache key");
    let cache_meta = WalletMeta {
        last_scanned_block: 123,
        updated_at: 456,
        last_scanned_block_hash: None,
    };
    db.put_wallet_utxo(&cache_key, "recovered-cache-row", b"encrypted cache data")
        .expect("store recovered cache row");
    db.put_wallet_meta(&cache_key, &cache_meta)
        .expect("store recovered cache metadata");
    let cache_meta_before = rmp_serde::to_vec_named(
        &db.get_wallet_meta(&cache_key)
            .expect("load recovered cache metadata")
            .expect("recovered cache metadata"),
    )
    .expect("encode recovered cache metadata");
    let private_namespace = WalletPrivateNamespaceId::new(1, cache_key.clone());
    let private_row = OpaqueWalletPrivateRow {
        row_id: vec![0x51; 32],
        payload: b"encrypted private workflow data".to_vec(),
    };
    db.put_opaque_wallet_private_row(
        &private_namespace,
        WalletPrivateRecordKind::SenderTransactionCandidate,
        &private_row,
    )
    .expect("store recovered private row");
    let cache_rows = db
        .list_wallet_utxos(&cache_key)
        .expect("snapshot recovered cache rows");

    let before = db
        .list_desktop_wallet_vault_records("")
        .expect("snapshot records");
    let mut reopen_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("reopen grant");
    let reopened = store
        .create_software_context(
            &view,
            &mut reopen_grant,
            base.wallet_id(),
            "another-child",
            99,
            "Another label",
            Zeroizing::new(PASSPHRASE.to_owned()),
            Zeroizing::new(PASSPHRASE.to_owned()),
            SoftwareContextSyncIntent::CreateNew,
            &[],
            VaultSessionId::from_bytes([24; 16]),
        )
        .expect("known context reopen");
    let CreateSoftwareContextResult::ExistingContext { metadata, .. } = reopened else {
        panic!("known context reopen unexpectedly created a context");
    };
    assert_eq!(metadata.wallet_uuid, "recover-child");
    assert_eq!(
        db.list_desktop_wallet_vault_records("")
            .expect("records after reopen"),
        before
    );
    let reloaded_chain = store
        .wallet_chain_metadata_for_session(&context_session, 0, 1, "0xcontract", 17)
        .expect("reloaded recovered chain metadata");
    assert_eq!(reloaded_chain.wallet_chain_uuid, chain.wallet_chain_uuid);
    assert_eq!(reloaded_chain.wallet_uuid, chain.wallet_uuid);
    assert_eq!(reloaded_chain.chain_type, chain.chain_type);
    assert_eq!(reloaded_chain.chain_id, chain.chain_id);
    assert_eq!(reloaded_chain.contract, chain.contract);
    assert_eq!(reloaded_chain.start_block, chain.start_block);
    assert_eq!(reloaded_chain.last_scanned_block, chain.last_scanned_block);
    assert_eq!(
        db.list_wallet_utxos(&cache_key)
            .expect("reloaded recovered cache rows"),
        cache_rows
    );
    let reloaded_cache_meta = db
        .get_wallet_meta(&cache_key)
        .expect("load reloaded recovered cache metadata")
        .expect("reloaded recovered cache metadata");
    assert_eq!(
        rmp_serde::to_vec_named(&reloaded_cache_meta)
            .expect("encode reloaded recovered cache metadata"),
        cache_meta_before
    );
    assert_eq!(
        db.list_opaque_wallet_private_rows(
            &private_namespace,
            WalletPrivateRecordKind::SenderTransactionCandidate,
        )
        .expect("reloaded recovered private rows"),
        vec![private_row]
    );

    drop(context_session);
    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn create_new_context_requires_available_non_overflowing_safe_heads() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "safe-head-base", "Base");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    for (wallet_uuid, passphrase, safe_head, error) in [
        (
            "missing-safe-head",
            "missing safe head",
            None,
            VaultError::SoftwareContextSafeHeadUnavailable,
        ),
        (
            "overflow-safe-head",
            "overflow safe head",
            Some(u64::MAX),
            VaultError::SoftwareContextSafeHeadOverflow,
        ),
    ] {
        let before = db
            .list_desktop_wallet_vault_records("")
            .expect("snapshot records");
        let mut grant = store
            .create_spend_grant(TEST_PASSWORD)
            .expect("spend grant");
        let result = store.create_software_context(
            &view,
            &mut grant,
            base.wallet_id(),
            wallet_uuid,
            0,
            wallet_uuid,
            Zeroizing::new(passphrase.to_owned()),
            Zeroizing::new(passphrase.to_owned()),
            SoftwareContextSyncIntent::CreateNew,
            &[SoftwareContextChainInput {
                chain_type: 0,
                chain_id: 1,
                contract: "0xcontract".to_owned(),
                deployment_block: 0,
                current_safe_head: safe_head,
            }],
            VaultSessionId::from_bytes([25; 16]),
        );
        let Err(actual) = result else {
            panic!("safe-head failure unexpectedly succeeded");
        };
        assert_eq!(actual.to_string(), error.to_string());
        assert_eq!(
            db.list_desktop_wallet_vault_records("")
                .expect("records after safe-head failure"),
            before
        );
    }

    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn passphrase_signers_and_derived_accounts_require_the_bound_session() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let base = import_wallet_with_metadata(&store, "signer-base", "Base");
    let view = store.unlock_view(TEST_PASSWORD).expect("unlock view");
    let mut creation_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("creation grant");
    let CreateSoftwareContextResult::Created {
        protected_seed_session,
        ..
    } = store
        .create_software_context(
            &view,
            &mut creation_grant,
            base.wallet_id(),
            "signer-child",
            4,
            "Signer child",
            Zeroizing::new(PASSPHRASE.to_owned()),
            Zeroizing::new(PASSPHRASE.to_owned()),
            SoftwareContextSyncIntent::RecoverExisting,
            &[],
            VaultSessionId::from_bytes([26; 16]),
        )
        .expect("create signer context")
    else {
        panic!("expected created signer context");
    };
    let context_session = store
        .load_view_session(TEST_PASSWORD, "signer-child")
        .expect("context view session");
    let protected_seed_session = Arc::new(protected_seed_session);

    let mut missing_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("missing-session grant");
    assert!(matches!(
        store.railgun_spend_signer_for_session(&mut missing_grant, &context_session, None),
        Err(VaultError::SoftwareSeedSessionRequired)
    ));

    let seed = bip39_seed_from_mnemonic(TEST_MNEMONIC, PASSPHRASE).expect("context seed");
    let wrong_binding = SoftwareSeedSessionBinding::new(
        base.wallet_id(),
        "wrong-context",
        VaultSessionId::from_bytes([27; 16]),
    );
    let wrong_session = {
        let wrong_grant = store
            .create_spend_grant(TEST_PASSWORD)
            .expect("wrong-session grant");
        wrong_grant
            .spend_unlock()
            .expect("wrong-session spend unlock")
            .seal_software_seed_session(wrong_binding, seed.as_ref())
            .expect("wrong session")
    };
    let mut wrong_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("wrong-binding grant");
    assert!(matches!(
        store.railgun_spend_signer_for_session(
            &mut wrong_grant,
            &context_session,
            Some(&wrong_session),
        ),
        Err(VaultError::SoftwareSeedSessionBindingMismatch)
    ));

    let mut signer_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("signer grant");
    let signer = store
        .railgun_spend_signer_for_session(
            &mut signer_grant,
            &context_session,
            Some(protected_seed_session.as_ref()),
        )
        .expect("passphrase signer");
    assert_eq!(
        signer.spending_public_key(),
        context_session.spending_public_key()
    );
    drop(signer);

    let mut executor_grant = store.create_spend_grant(TEST_PASSWORD).unwrap();
    let (private_signer, executor_signer) = store
        .executor_spend_signers_for_session(
            &mut executor_grant,
            &context_session,
            Some(protected_seed_session.as_ref()),
            137,
            9,
        )
        .unwrap();
    let mut discovery_grant = store.create_spend_grant(TEST_PASSWORD).unwrap();
    let addresses = store
        .executor_addresses_for_session(
            &mut discovery_grant,
            &context_session,
            Some(protected_seed_session.as_ref()),
            137,
            9..11,
        )
        .unwrap();
    assert_eq!(addresses[0], (9, executor_signer.address()));
    assert_ne!(addresses[0].1, addresses[1].1);
    assert_eq!(
        private_signer.spending_public_key(),
        context_session.spending_public_key()
    );
    assert_eq!(
        executor_signer.address(),
        railgun_wallet::keys::derive_executor_signer(&seed, 4, 137, 9)
            .unwrap()
            .address()
    );
    assert!(matches!(
        store.executor_spend_signers_for_session(
            &mut executor_grant,
            &context_session,
            Some(protected_seed_session.as_ref()),
            137,
            9,
        ),
        Err(ExecutorStoreError::Vault(VaultError::InvalidSpendGrant))
    ));
    for session in [None, Some(&wrong_session)] {
        let mut grant = store.create_spend_grant(TEST_PASSWORD).unwrap();
        assert!(matches!(
            store
                .executor_spend_signers_for_session(&mut grant, &context_session, session, 137, 9,),
            Err(ExecutorStoreError::Vault(
                VaultError::SoftwareSeedSessionRequired
                    | VaultError::SoftwareSeedSessionBindingMismatch
            ))
        ));
    }

    let initial_account = store
        .list_active_public_accounts_for_session(&context_session)
        .expect("initial context account")
        .into_iter()
        .find(|account| account.derivation_index == Some(0))
        .expect("index zero context account");
    let mut missing_public_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("missing-public-session grant");
    assert!(matches!(
        store.public_account_signing_key_with_session(
            &mut missing_public_grant,
            &context_session,
            &initial_account.public_account_uuid,
            None,
        ),
        Err(VaultError::SoftwareSeedSessionRequired)
    ));
    let mut wrong_public_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("wrong-public-session grant");
    assert!(matches!(
        store.public_account_signing_key_with_session(
            &mut wrong_public_grant,
            &context_session,
            &initial_account.public_account_uuid,
            Some(&wrong_session),
        ),
        Err(VaultError::SoftwareSeedSessionBindingMismatch)
    ));
    let mut public_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("public signer grant");
    let private_key = store
        .public_account_signing_key_with_session(
            &mut public_grant,
            &context_session,
            &initial_account.public_account_uuid,
            Some(protected_seed_session.as_ref()),
        )
        .expect("passphrase public signer");
    assert_eq!(
        public_evm_address_from_private_key(&private_key).expect("public signer address"),
        initial_account.address
    );

    let before = db
        .list_desktop_wallet_vault_records("")
        .expect("snapshot account records");
    assert!(matches!(
        store.add_derived_public_account_with_session(
            TEST_PASSWORD,
            &context_session,
            Some("blocked account"),
            None,
        ),
        Err(VaultError::SoftwareSeedSessionRequired)
    ));
    assert_eq!(
        db.list_desktop_wallet_vault_records("")
            .expect("records after blocked account"),
        before
    );
    let before_wrong_account = db
        .list_desktop_wallet_vault_records("")
        .expect("snapshot before wrong-session account");
    assert!(matches!(
        store.add_derived_public_account_with_session(
            TEST_PASSWORD,
            &context_session,
            Some("wrong session account"),
            Some(&wrong_session),
        ),
        Err(VaultError::SoftwareSeedSessionBindingMismatch)
    ));
    assert_eq!(
        db.list_desktop_wallet_vault_records("")
            .expect("records after wrong-session account"),
        before_wrong_account
    );
    let account = store
        .add_derived_public_account_with_session(
            TEST_PASSWORD,
            &context_session,
            Some("derived account"),
            Some(protected_seed_session.as_ref()),
        )
        .expect("derived account");
    assert_eq!(account.derivation_index, Some(1));
    assert_eq!(
        account.address,
        derive_public_evm_address_from_mnemonic_with_passphrase(TEST_MNEMONIC, PASSPHRASE, 1)
            .expect("derived account index one address")
    );

    drop(context_session);
    drop(view);
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
