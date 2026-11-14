use super::super::*;
use super::helpers::*;
use crate::DesktopWalletVaultDbExt as _;
use std::fs;

#[test]
fn desktop_vault_store_persists_metadata() {
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
    let loaded = store.metadata().expect("load metadata");
    let unlocked = unlock_view(&loaded, TEST_PASSWORD).expect("unlock loaded metadata");
    let record = unlocked
        .encrypt_record(
            RecordKind::WalletChainMetadata,
            TEST_WALLET_ID,
            b"chain metadata",
        )
        .expect("encrypt");

    assert_eq!(loaded.version, current_vault_version());
    assert_eq!(loaded.kdf, test_kdf());
    assert!(
        unlocked
            .decrypt_record(RecordKind::WalletChainMetadata, TEST_WALLET_ID, &record)
            .is_ok()
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn legacy_v1_vault_metadata_unlocks_and_upgrades_on_store_unlock() {
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    let mut created = create_with_params(TEST_PASSWORD, test_kdf()).expect("create vault");
    created.metadata.version = legacy_vault_version();

    unlock_view(&created.metadata, TEST_PASSWORD).expect("unlock legacy metadata");
    store
        .put_metadata(&created.metadata)
        .expect("persist legacy metadata");
    store.unlock_view(TEST_PASSWORD).expect("store unlock");

    assert_eq!(
        store.metadata().expect("load upgraded metadata").version,
        current_vault_version()
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn create_vault_does_not_overwrite_existing_metadata() {
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));

    let created = store
        .create_vault_with_params(TEST_PASSWORD, test_kdf())
        .expect("create vault");
    assert!(matches!(
        store.create_vault_with_params("different password", test_kdf()),
        Err(VaultError::VaultAlreadyExists)
    ));

    let loaded = store.metadata().expect("load metadata");
    assert_eq!(loaded, created.metadata);
    assert!(store.unlock_view(TEST_PASSWORD).is_ok());

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn reencrypt_vault_changes_password_without_rewriting_wallet_records() {
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    store
        .create_vault_with_params(TEST_PASSWORD, test_kdf())
        .expect("create vault");
    let wallet_id = "password-change-wallet";
    let stored = store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", TEST_MNEMONIC)
        .expect("import wallet");
    let view_payload = db
        .get_desktop_wallet_vault_record(&stored.view_record_key)
        .expect("load view record")
        .expect("view record present");
    let spend_payload = db
        .get_desktop_wallet_vault_record(&stored.spend_record_key)
        .expect("load spend record")
        .expect("spend record present");
    let new_password = "new correct horse battery staple";

    store
        .reencrypt_vault(TEST_PASSWORD, new_password)
        .expect("reencrypt vault");

    assert!(matches!(
        store.unlock_view(TEST_PASSWORD),
        Err(VaultError::UnlockFailed)
    ));
    assert!(matches!(
        store.create_spend_grant(TEST_PASSWORD),
        Err(VaultError::UnlockFailed)
    ));
    assert!(store.unlock_view(new_password).is_ok());
    assert_eq!(
        db.get_desktop_wallet_vault_record(&stored.view_record_key)
            .expect("load view record")
            .expect("view record present"),
        view_payload
    );
    assert_eq!(
        db.get_desktop_wallet_vault_record(&stored.spend_record_key)
            .expect("load spend record")
            .expect("spend record present"),
        spend_payload
    );
    assert!(store.load_view_bundle(new_password, wallet_id).is_ok());
    let mut grant = store
        .create_spend_grant(new_password)
        .expect("create grant with new password");
    assert!(store.load_spend_bundle(&mut grant, wallet_id).is_ok());

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn reencrypt_vault_wrong_current_password_leaves_metadata_unchanged() {
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));
    store
        .create_vault_with_params(TEST_PASSWORD, test_kdf())
        .expect("create vault");
    let metadata = store.metadata().expect("load metadata");

    assert!(matches!(
        store.reencrypt_vault("wrong password", "new password"),
        Err(VaultError::UnlockFailed)
    ));

    assert_eq!(store.metadata().expect("load metadata"), metadata);
    assert!(store.unlock_view(TEST_PASSWORD).is_ok());
    assert!(matches!(
        store.unlock_view("new password"),
        Err(VaultError::UnlockFailed)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn imported_wallet_stores_encrypted_view_and_spend_bundles() {
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
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let wallet_id = "opaque-wallet-id";

    let stored = store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_payload = db
        .get_desktop_wallet_vault_record(&stored.view_record_key)
        .expect("load view record")
        .expect("view record present");
    let spend_payload = db
        .get_desktop_wallet_vault_record(&stored.spend_record_key)
        .expect("load spend record")
        .expect("spend record present");
    let view_bundle = store
        .load_view_bundle(TEST_PASSWORD, wallet_id)
        .expect("load view bundle");
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("create grant");
    let spend_bundle = store
        .load_spend_bundle(&mut grant, wallet_id)
        .expect("load spend bundle");

    assert_eq!(view_bundle.derivation_index, 0);
    assert_eq!(spend_bundle.derivation_index, 0);
    assert_eq!(spend_bundle.bip39_language, "english");
    assert_eq!(
        spend_bundle.bip39_entropy,
        bip39_entropy_from_mnemonic(mnemonic).expect("mnemonic entropy")
    );
    assert!(!contains_subsequence(&view_payload, mnemonic.as_bytes()));
    assert!(!contains_subsequence(&spend_payload, mnemonic.as_bytes()));

    grant.invalidate();
    assert!(matches!(
        store.load_spend_bundle(&mut grant, wallet_id),
        Err(VaultError::InvalidSpendGrant)
    ));
    let mut fresh_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("create fresh grant");
    assert!(store.load_spend_bundle(&mut fresh_grant, wallet_id).is_ok());

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn wallet_with_metadata_stores_records_in_one_batch() {
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
    let seed = generate_seed_material().expect("generate seed");
    let wallet_id = "wallet-with-metadata";
    let metadata = WalletMetadataBundle {
        wallet_uuid: wallet_id.to_string(),
        label: "Primary wallet".to_string(),
        derivation_index: 0,
        source: WalletSource::Generated,
        status: WalletStatus::Active,
        display_order: 0,
        hardware_descriptor: None,
        hardware_account: None,
        pending_create_new_chain_ids: BTreeSet::new(),
        software_context: Some(WalletSoftwareContext::standard(wallet_id)),
    };

    let stored = store
        .store_generated_wallet_with_metadata(
            TEST_PASSWORD,
            wallet_id,
            0,
            "english",
            &seed,
            &metadata,
        )
        .expect("store wallet with metadata");

    assert!(
        db.get_desktop_wallet_vault_record(&stored.view_record_key)
            .expect("load view record")
            .is_some()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&stored.spend_record_key)
            .expect("load spend record")
            .is_some()
    );
    let loaded = store
        .load_wallet_metadata(TEST_PASSWORD, wallet_id)
        .expect("load wallet metadata");
    assert_eq!(loaded.wallet_uuid, wallet_id);
    assert_eq!(loaded.label, "Primary wallet");
    assert_eq!(loaded.derivation_index, 0);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn generated_wallet_seed_material_stores_encrypted_bundles() {
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
    let seed = generate_seed_material().expect("generate seed");
    let wallet_id = "generated-wallet-id";

    let stored = store
        .store_generated_wallet(TEST_PASSWORD, wallet_id, 0, "english", &seed)
        .expect("store generated wallet");
    let view_payload = db
        .get_desktop_wallet_vault_record(&stored.view_record_key)
        .expect("load view record")
        .expect("view record present");
    let spend_payload = db
        .get_desktop_wallet_vault_record(&stored.spend_record_key)
        .expect("load spend record")
        .expect("spend record present");
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("create grant");
    let spend_bundle = store
        .load_spend_bundle(&mut grant, wallet_id)
        .expect("load spend bundle");

    assert_eq!(spend_bundle.bip39_entropy, seed.entropy.as_slice());
    assert!(!contains_subsequence(
        &view_payload,
        seed.mnemonic.as_bytes()
    ));
    assert!(!contains_subsequence(
        &spend_payload,
        seed.mnemonic.as_bytes()
    ));
    assert!(!contains_subsequence(
        &view_payload,
        seed.entropy.as_slice()
    ));
    assert!(!contains_subsequence(
        &spend_payload,
        seed.entropy.as_slice()
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn first_view_session_loads_only_view_material() {
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
    assert!(
        store
            .unlock_first_view_session(TEST_PASSWORD)
            .expect("unlock empty vault")
            .is_none()
    );

    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let wallet_id = "first-view-wallet";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = store
        .unlock_first_view_session(TEST_PASSWORD)
        .expect("unlock first wallet")
        .expect("view session present");
    let wallet = WalletKeys::from_mnemonic(mnemonic, 0).expect("derive wallet");

    assert_eq!(view_session.wallet_id(), wallet_id);
    assert_eq!(view_session.derivation_index(), 0);
    assert_eq!(
        view_session.scan_keys().master_public_key,
        wallet.viewing.master_public_key
    );
    assert_eq!(
        view_session.scan_keys().nullifying_key,
        wallet.viewing.nullifying_key
    );
    assert!(matches!(
        store.unlock_first_view_session("wrong password"),
        Err(VaultError::UnlockFailed)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn view_session_receive_address_uses_all_chains_address() {
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
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let wallet_id = "receive-address-wallet";
    store
        .import_wallet_mnemonic(TEST_PASSWORD, wallet_id, 0, "english", mnemonic)
        .expect("import wallet");
    let view_session = store
        .load_view_session(TEST_PASSWORD, wallet_id)
        .expect("load view session");
    let wallet = WalletKeys::from_mnemonic(mnemonic, 0).expect("derive wallet");
    let all_chains = wallet
        .viewing
        .derive_address(None)
        .expect("derive all-chains address")
        .to_string();
    let ethereum_scoped = wallet
        .viewing
        .derive_address(Some((0, 1)))
        .expect("derive ethereum-scoped address")
        .to_string();
    let bsc_scoped = wallet
        .viewing
        .derive_address(Some((0, 56)))
        .expect("derive bsc-scoped address")
        .to_string();

    assert_eq!(
        view_session.receive_address().expect("receive address"),
        all_chains
    );
    assert_ne!(all_chains, ethereum_scoped);
    assert_ne!(all_chains, bsc_scoped);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
