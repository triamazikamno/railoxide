use super::super::*;
use super::helpers::*;
use crate::{RpcOrigin, WalletRpcOrigin};
use std::fs;

#[test]
fn gateway_permissions_restore_isolate_origins_and_revoke() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view = import_wallet_with_metadata(&store, "gateway-wallet", "Gateway");
    let account = store
        .list_active_public_accounts_for_session(&view)
        .expect("accounts")
        .remove(0);
    assert!(
        store
            .list_gateway_permissions(&view)
            .expect("empty namespace")
            .is_empty()
    );
    let origin =
        RpcOrigin::dapp("peer-a", "https://example.invalid/path?q=one#fragment").expect("origin");
    let grant = store
        .grant_gateway_permission(&view, &origin, &account.public_account_uuid, 1)
        .expect("grant");
    let updated = store
        .grant_gateway_permission(&view, &origin, &account.public_account_uuid, 10)
        .expect("update chain");
    assert_eq!(grant.permission_id, updated.permission_id);
    for document in [
        "https://example.invalid/other?q=one#fragment",
        "https://example.invalid/path?q=two#fragment",
        "https://example.invalid/path?q=one#other",
    ] {
        let same_site = RpcOrigin::dapp("peer-a", document).expect("document");
        let reused = store
            .grant_gateway_permission(&view, &same_site, &account.public_account_uuid, 10)
            .expect("same origin grant");
        assert_eq!(reused.permission_id, grant.permission_id);
    }
    for distinct in [
        RpcOrigin::dapp("peer-b", "https://example.invalid/path?q=one#fragment"),
        RpcOrigin::dapp("peer-a", "http://example.invalid/path"),
        RpcOrigin::dapp("peer-a", "https://other.invalid/path"),
        RpcOrigin::dapp("peer-a", "https://example.invalid:8443/path"),
    ] {
        let independent = store
            .grant_gateway_permission(
                &view,
                &distinct.expect("distinct origin"),
                &account.public_account_uuid,
                1,
            )
            .expect("independent grant");
        assert_ne!(independent.permission_id, grant.permission_id);
    }
    assert!(matches!(
        store.grant_gateway_permission(
            &view,
            &RpcOrigin::dapp("peer-a", "data:text/plain,opaque").unwrap(),
            &account.public_account_uuid,
            1,
        ),
        Err(VaultError::InvalidGatewayPermission)
    ));
    let rows = db
        .list_desktop_wallet_vault_records("gateway-permission|")
        .expect("raw rows");
    assert_eq!(rows.len(), 5);
    for row in rows {
        for private in [
            "example.invalid",
            "peer-a",
            account.public_account_uuid.as_str(),
        ] {
            assert!(!contains_subsequence(&row.payload, private.as_bytes()));
            assert!(!row.key.contains(private));
        }
    }
    drop(view);
    drop(store);
    drop(db);
    let store = DesktopVaultStore::from_db(Arc::new(
        DbStore::open(DbConfig {
            root_dir: root_dir.clone(),
        })
        .expect("reopen db"),
    ));
    let view = store
        .load_view_session(TEST_PASSWORD, "gateway-wallet")
        .expect("unlock");
    let restored = store.list_gateway_permissions(&view).expect("restore");
    assert_eq!(restored.len(), 5);
    assert!(restored.iter().any(|permission| permission == &updated));
    store
        .delete_gateway_permission(&view, &grant.permission_id)
        .expect("revoke");
    assert_eq!(
        store
            .list_gateway_permissions(&view)
            .expect("remaining grants")
            .len(),
        4
    );
    let replacement = store
        .grant_gateway_permission(&view, &origin, &account.public_account_uuid, 1)
        .expect("new grant after revoke");
    assert_ne!(replacement.permission_id, grant.permission_id);
    assert!(matches!(
        store.grant_gateway_permission(
            &view,
            &WalletRpcOrigin::PublicWallet.into(),
            &account.public_account_uuid,
            1
        ),
        Err(VaultError::InvalidGatewayPermission)
    ));
    drop(store);
    fs::remove_dir_all(root_dir).expect("remove db");
}

#[test]
fn gateway_grants_enforce_active_account_scope_and_view_capability() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first = import_wallet_with_metadata(&store, "gateway-a", "Gateway A");
    let second = import_wallet_with_metadata(&store, "gateway-b", "Gateway B");
    let scoped = store
        .import_public_account(TEST_PASSWORD, &first, IMPORT_PRIVATE_KEY_ONE, None, false)
        .expect("scoped account");
    let global = store
        .import_public_account(TEST_PASSWORD, &first, IMPORT_PRIVATE_KEY_TWO, None, true)
        .expect("global account");
    let origin = RpcOrigin::dapp("peer", "https://example.invalid").expect("origin");
    assert!(matches!(
        store.grant_gateway_permission(&second, &origin, &scoped.public_account_uuid, 1),
        Err(VaultError::InvalidPublicAccountOperation)
    ));
    let grant = store
        .grant_gateway_permission(&first, &origin, &scoped.public_account_uuid, 1)
        .expect("owner can grant");
    assert_eq!(
        grant.owning_private_wallet_uuid.as_deref(),
        Some("gateway-a")
    );
    let global_grant = store
        .grant_gateway_permission(&second, &origin, &global.public_account_uuid, 1)
        .expect("global account across wallets");
    assert_eq!(global_grant.permission_id, grant.permission_id);
    assert!(global_grant.owning_private_wallet_uuid.is_none());
    store
        .delete_imported_public_account(&first, &scoped.public_account_uuid)
        .expect("delete account");
    assert!(
        store
            .grant_gateway_permission(&first, &origin, &scoped.public_account_uuid, 1)
            .is_err()
    );
    let derived = store
        .list_active_public_accounts_for_session(&first)
        .expect("accounts")
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .expect("derived");
    store
        .deactivate_derived_public_account(&first, &derived.public_account_uuid)
        .expect("deactivate");
    assert!(matches!(
        store.grant_gateway_permission(&first, &origin, &derived.public_account_uuid, 1),
        Err(VaultError::InvalidPublicAccountOperation)
    ));

    let (other_root, other_db, other_store) = desktop_store_with_vault();
    let wrong_view = import_wallet_with_metadata(&other_store, "gateway-a", "Other vault");
    assert!(matches!(
        store.list_gateway_permissions(&wrong_view),
        Err(VaultError::Decrypt)
    ));
    assert!(matches!(
        store.delete_gateway_permission(&wrong_view, &grant.permission_id),
        Err(VaultError::Decrypt)
    ));
    assert_eq!(
        store
            .list_gateway_permissions(&first)
            .expect("grant retained")
            .len(),
        1
    );
    drop(other_store);
    drop(other_db);
    fs::remove_dir_all(other_root).expect("remove other db");
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove db");
}

#[test]
fn gateway_permission_corruption_fails_closed() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view = import_wallet_with_metadata(&store, "gateway-wallet", "Gateway");
    let account = store
        .list_active_public_accounts_for_session(&view)
        .expect("accounts")
        .remove(0);
    let origin = RpcOrigin::dapp("peer", "https://example.invalid").expect("origin");
    let grant = store
        .grant_gateway_permission(&view, &origin, &account.public_account_uuid, 1)
        .expect("grant");
    let key = format!("gateway-permission|{}", grant.permission_id);
    let raw = db
        .get_desktop_wallet_vault_record(&key)
        .expect("read")
        .expect("record");
    let record: EncryptedRecord = rmp_serde::from_slice(&raw).expect("encrypted record");
    let plaintext = view
        .view
        .decrypt_record(RecordKind::GatewayPermission, &grant.permission_id, &record)
        .expect("decrypt fixture");
    let original: serde_json::Value = rmp_serde::from_slice(&plaintext).expect("fixture payload");
    for (field, value) in [
        ("version", serde_json::json!(2)),
        ("permission_id", serde_json::json!("different-id")),
        ("url", serde_json::json!("invalid URL")),
        ("paired_peer_id", serde_json::json!("")),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        let changed = Zeroizing::new(rmp_serde::to_vec_named(&changed).expect("encode fixture"));
        let encrypted = view
            .view
            .encrypt_record(
                RecordKind::GatewayPermission,
                &grant.permission_id,
                &changed,
            )
            .expect("encrypt fixture");
        db.put_desktop_wallet_vault_record(
            &key,
            &rmp_serde::to_vec_named(&encrypted).expect("encode record"),
        )
        .expect("write fixture");
        assert!(matches!(
            store.list_gateway_permissions(&view),
            Err(VaultError::InvalidGatewayPermission)
        ));
        assert!(matches!(
            store.delete_gateway_permission(&view, &grant.permission_id),
            Err(VaultError::InvalidGatewayPermission)
        ));
    }
    for (kind, id) in [
        (
            RecordKind::WalletConnectSession,
            grant.permission_id.as_str(),
        ),
        (RecordKind::GatewayPermission, "different-id"),
    ] {
        let encrypted = view
            .view
            .encrypt_record(kind, id, &plaintext)
            .expect("encrypt wrong AAD");
        db.put_desktop_wallet_vault_record(
            &key,
            &rmp_serde::to_vec_named(&encrypted).expect("encode record"),
        )
        .expect("write fixture");
        assert!(matches!(
            store.list_gateway_permissions(&view),
            Err(VaultError::Decrypt)
        ));
    }
    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove db");
}
