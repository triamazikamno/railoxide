use super::super::*;
use super::helpers::*;
use std::fs;

#[test]
fn walletconnect_secret_debug_output_is_redacted() {
    let account = PublicAccountMetadata {
        public_account_uuid: "public-account".to_owned(),
        address: Address::from([0x11; 20]),
        label: None,
        source: PublicAccountSource::Imported,
        scope: PublicAccountScope::Global,
        derivation_index: None,
        hardware_descriptor: None,
        status: PublicAccountStatus::Active,
        display_order: 0,
    };
    let identity = WalletConnectRelayIdentity {
        signing_key: [9u8; KEY_LEN],
        client_id: "relay-client".to_owned(),
    };
    let session = test_walletconnect_session("debug-session", &account, &identity.client_id);

    let identity_debug = format!("{identity:?}");
    let session_debug = format!("{session:?}");

    assert!(identity_debug.contains("<redacted>"));
    assert!(session_debug.contains("<redacted>"));
    for secret_bytes in ["[9, 9", "[1, 1", "[2, 2", "[3, 3"] {
        assert!(!identity_debug.contains(secret_bytes));
        assert!(!session_debug.contains(secret_bytes));
    }
}
#[test]
fn walletconnect_sessions_persist_after_unlock_and_reuse_relay_identity() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "wc-wallet", "WalletConnect");
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("WC Public"),
            false,
        )
        .expect("import public account");

    let identity = store
        .load_or_create_walletconnect_relay_identity(&view_session)
        .expect("create relay identity");
    let reused = store
        .load_or_create_walletconnect_relay_identity(&view_session)
        .expect("reuse relay identity");
    assert_eq!(reused, identity);

    let session = test_walletconnect_session("session-a", &account, &identity.client_id);
    store
        .store_walletconnect_session(&view_session, &session)
        .expect("store WalletConnect session");

    let raw = db
        .get_desktop_wallet_vault_record(&walletconnect_session_record_key("session-a"))
        .expect("read raw session")
        .expect("raw session exists");
    assert!(!String::from_utf8_lossy(&raw).contains("Example Dapp"));

    let unlocked_again = store
        .load_view_session(TEST_PASSWORD, "wc-wallet")
        .expect("unlock again");
    let loaded_identity = store
        .load_walletconnect_relay_identity(&unlocked_again)
        .expect("load relay identity")
        .expect("relay identity exists");
    assert_eq!(loaded_identity, identity);

    let sessions = store
        .list_walletconnect_sessions(&unlocked_again)
        .expect("list sessions after unlock");
    assert_eq!(sessions, vec![session.clone()]);

    store
        .delete_walletconnect_session(&session.session_uuid)
        .expect("delete session");
    assert!(
        store
            .list_walletconnect_sessions(&unlocked_again)
            .expect("list after delete")
            .is_empty()
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn walletconnect_relay_identity_lookup_uses_session_client_id_across_wallets() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first_session = import_wallet_with_metadata(&store, "wc-identity-a", "WC Identity A");
    let second_session = import_wallet_with_metadata(&store, "wc-identity-b", "WC Identity B");
    let first_identity = store
        .load_or_create_walletconnect_relay_identity(&first_session)
        .expect("create first relay identity");
    let second_identity = store
        .load_or_create_walletconnect_relay_identity(&second_session)
        .expect("create second relay identity");

    assert_ne!(first_identity.client_id, second_identity.client_id);
    assert_eq!(
        store
            .load_walletconnect_relay_identity(&second_session)
            .expect("load selected wallet relay identity"),
        Some(second_identity)
    );
    assert_eq!(
        store
            .load_walletconnect_relay_identity_for_client_id(
                &second_session,
                &first_identity.client_id,
            )
            .expect("load relay identity by session client id"),
        Some(first_identity)
    );
    assert!(
        store
            .load_walletconnect_relay_identity_for_client_id(&second_session, "missing-client")
            .expect("missing relay identity lookup")
            .is_none()
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn walletconnect_relay_identity_lookup_survives_origin_wallet_deletion() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first_session = import_wallet_with_metadata(&store, "wc-origin-a", "WC Origin A");
    let second_session = import_wallet_with_metadata(&store, "wc-origin-b", "WC Origin B");
    let first_identity = store
        .load_or_create_walletconnect_relay_identity(&first_session)
        .expect("create first relay identity");
    let global = store
        .import_public_account(
            TEST_PASSWORD,
            &first_session,
            IMPORT_PRIVATE_KEY_TWO,
            Some("Global WC"),
            true,
        )
        .expect("import global account");
    let session =
        test_walletconnect_session("global-origin-session", &global, &first_identity.client_id);
    store
        .store_walletconnect_session(&first_session, &session)
        .expect("store global session");

    store
        .delete_wallet_for_session(&second_session, first_session.wallet_id())
        .expect("delete origin wallet");

    let reconciled = store
        .reconcile_walletconnect_session_account_state(&second_session, &session.session_uuid)
        .expect("global session remains active");
    assert_eq!(
        reconciled.lifecycle_state,
        WalletConnectSessionLifecycleState::Active
    );
    assert_eq!(
        store
            .load_walletconnect_relay_identity_for_client_id(
                &second_session,
                &first_identity.client_id,
            )
            .expect("load relay identity after origin wallet deletion"),
        Some(first_identity)
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn walletconnect_session_account_resolution_handles_scope_pause_and_invalid() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first_session = import_wallet_with_metadata(&store, "wc-wallet-a", "WC A");
    let second_session = import_wallet_with_metadata(&store, "wc-wallet-b", "WC B");
    let scoped = store
        .import_public_account(
            TEST_PASSWORD,
            &first_session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("Scoped WC"),
            false,
        )
        .expect("import scoped account");
    let global = store
        .import_public_account(
            TEST_PASSWORD,
            &first_session,
            IMPORT_PRIVATE_KEY_TWO,
            Some("Global WC"),
            true,
        )
        .expect("import global account");
    let identity = store
        .load_or_create_walletconnect_relay_identity(&first_session)
        .expect("relay identity");

    let scoped_session = test_walletconnect_session("scoped-session", &scoped, &identity.client_id);
    let global_session = test_walletconnect_session("global-session", &global, &identity.client_id);
    store
        .store_walletconnect_session(&first_session, &scoped_session)
        .expect("store scoped session");
    store
        .store_walletconnect_session(&first_session, &global_session)
        .expect("store global session");

    assert!(matches!(
        store
            .resolve_walletconnect_session_account(&first_session, &scoped_session)
            .expect("resolve scoped under owner"),
        WalletConnectSessionAccountResolution::Usable(account)
            if account.public_account_uuid == scoped.public_account_uuid
    ));
    assert!(matches!(
        store
            .resolve_walletconnect_session_account(&second_session, &scoped_session)
            .expect("resolve scoped under other wallet"),
        WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet { owning_wallet_uuid }
            if owning_wallet_uuid == "wc-wallet-a"
    ));
    assert!(matches!(
        store
            .resolve_walletconnect_session_account(&second_session, &global_session)
            .expect("resolve global under other wallet"),
        WalletConnectSessionAccountResolution::Usable(account)
            if account.public_account_uuid == global.public_account_uuid
    ));

    let paused = store
        .reconcile_walletconnect_session_account_state(&second_session, "scoped-session")
        .expect("pause scoped session");
    assert_eq!(
        paused.lifecycle_state,
        WalletConnectSessionLifecycleState::TemporarilyPaused
    );
    let active = store
        .reconcile_walletconnect_session_account_state(&first_session, "scoped-session")
        .expect("resume scoped session");
    assert_eq!(
        active.lifecycle_state,
        WalletConnectSessionLifecycleState::Active
    );

    store
        .delete_imported_public_account(&first_session, &scoped.public_account_uuid)
        .expect("delete scoped public account");
    let invalid = store
        .reconcile_walletconnect_session_account_state(&first_session, "scoped-session")
        .expect("invalidate missing account");
    assert_eq!(
        invalid.lifecycle_state,
        WalletConnectSessionLifecycleState::Invalid
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn walletconnect_session_account_resolution_reads_only_the_selected_record() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "wc-selected-account", "WC Selected");
    let mut account = store
        .list_active_public_accounts_for_session(&view_session)
        .expect("active accounts")
        .into_iter()
        .next()
        .expect("public account");
    let session = test_walletconnect_session("selected-session", &account, "relay-client");
    let selected_uuid = account.public_account_uuid.clone();
    let selected_key = public_account_metadata_record_key(&selected_uuid);
    account.public_account_uuid = "stale-payload-uuid".to_owned();
    let record = view_session
        .view
        .encrypt_public_account_metadata(&selected_uuid, &account)
        .expect("encrypt selected metadata with stale payload UUID");
    let (_, payload) = record
        .to_record_entry(selected_key.clone())
        .expect("encode selected metadata");
    db.put_desktop_wallet_vault_record(&selected_key, &payload)
        .expect("store selected metadata");
    db.put_desktop_wallet_vault_record(
        &public_account_metadata_record_key("unrelated-account"),
        &payload,
    )
    .expect("store unrelated metadata with mismatched authenticated UUID");

    assert!(matches!(
        store
            .resolve_walletconnect_session_account(&view_session, &session)
            .expect("unrelated corrupt metadata does not block selected account"),
        WalletConnectSessionAccountResolution::Usable(resolved)
            if resolved.public_account_uuid == selected_uuid
    ));

    let wrong_record = view_session
        .view
        .encrypt_public_account_metadata("unrelated-account", &account)
        .expect("encrypt metadata for another account");
    let (_, wrong_payload) = wrong_record
        .to_record_entry(selected_key.clone())
        .expect("encode metadata for another account");
    db.put_desktop_wallet_vault_record(&selected_key, &wrong_payload)
        .expect("replace selected metadata with mismatched authenticated UUID");
    assert!(
        store
            .resolve_walletconnect_session_account(&view_session, &session)
            .is_err()
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn walletconnect_invalid_session_does_not_reactivate_with_public_account() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "wc-invalid-account", "WC Invalid");
    let account = store
        .list_active_public_accounts_for_session(&view_session)
        .expect("active accounts")
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .expect("derived account");
    let identity = store
        .load_or_create_walletconnect_relay_identity(&view_session)
        .expect("relay identity");
    let session = test_walletconnect_session("reactivation-session", &account, &identity.client_id);
    store
        .store_walletconnect_session(&view_session, &session)
        .expect("store session");

    store
        .deactivate_derived_public_account(&view_session, &account.public_account_uuid)
        .expect("deactivate account");
    let invalid = store
        .reconcile_walletconnect_session_account_state(&view_session, &session.session_uuid)
        .expect("invalidate session");
    assert_eq!(
        invalid.lifecycle_state,
        WalletConnectSessionLifecycleState::Invalid
    );

    store
        .activate_derived_public_account(&view_session, &account.public_account_uuid)
        .expect("reactivate account");
    let still_invalid = store
        .reconcile_walletconnect_session_account_state(&view_session, &session.session_uuid)
        .expect("reconcile invalid session");
    assert_eq!(
        still_invalid.lifecycle_state,
        WalletConnectSessionLifecycleState::Invalid
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
