use super::super::*;
use super::helpers::*;
use std::fs;

#[test]
fn desktop_vault_first_run_unlock_wallet_setup_and_spend_prompt_flow() {
    let root_dir = temp_db_root();
    let db = Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("open db"),
    );
    let store = DesktopVaultStore::from_db(Arc::clone(&db));

    assert!(!store.vault_exists().expect("inspect empty vault"));
    let created = store
        .create_vault_with_params(TEST_PASSWORD, test_kdf())
        .expect("create vault");
    assert_eq!(created.metadata.version, current_vault_version());
    assert!(store.vault_exists().expect("inspect created vault"));
    assert!(
        store
            .unlock_first_view_session(TEST_PASSWORD)
            .expect("unlock empty vault")
            .is_none()
    );

    let generated_seed = generate_seed_material().expect("generate wallet");
    store
        .store_generated_wallet(
            TEST_PASSWORD,
            "generated-wallet",
            0,
            "english",
            &generated_seed,
        )
        .expect("store generated wallet");
    let generated_session = store
        .unlock_first_view_session(TEST_PASSWORD)
        .expect("unlock generated wallet")
        .expect("generated session");
    assert_eq!(generated_session.wallet_id(), "generated-wallet");

    let imported_mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    store
        .import_wallet_mnemonic(
            TEST_PASSWORD,
            "imported-wallet",
            0,
            "english",
            imported_mnemonic,
        )
        .expect("import wallet");
    let imported_session = store
        .load_view_session(TEST_PASSWORD, "imported-wallet")
        .expect("load imported wallet");
    assert_eq!(imported_session.wallet_id(), "imported-wallet");
    assert!(matches!(
        store.create_spend_grant("wrong password"),
        Err(VaultError::UnlockFailed)
    ));

    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("fresh spend grant");
    let _signer = store
        .railgun_spend_signer(&mut grant, imported_session.wallet_id())
        .expect("spend signer from grant");
    assert!(!grant.is_valid());
    assert!(matches!(
        store.railgun_spend_signer(&mut grant, imported_session.wallet_id()),
        Err(VaultError::InvalidSpendGrant)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[cfg(unix)]
#[test]
fn desktop_vault_open_makes_existing_profile_private() {
    use std::os::unix::fs::PermissionsExt as _;

    let root_dir = temp_db_root();
    fs::create_dir_all(&root_dir).expect("create profile dir");
    fs::set_permissions(&root_dir, fs::Permissions::from_mode(0o755)).expect("loosen profile");

    let store = DesktopVaultStore::open(root_dir.clone()).expect("open vault store");
    let mode = fs::metadata(&root_dir)
        .expect("stat profile")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);

    drop(store);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
