use super::super::*;
use super::helpers::*;
use std::fs;

#[test]
fn broadcaster_preferences_default_empty_persist_and_reload() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "broadcaster-pref-wallet", "Prefs");
    let favorite_address = railgun_recipient_for_derivation(12);
    let banned_address = railgun_recipient_for_derivation(13);

    let empty = store
        .list_broadcaster_preferences_for_session(&view_session)
        .expect("empty broadcaster preferences");
    assert_eq!(empty.favorites, [] as [BroadcasterPreferenceEntry; 0]);
    assert_eq!(empty.banned, [] as [BroadcasterPreferenceEntry; 0]);

    let favorite = store
        .add_favorite_broadcaster_for_session(&view_session, &favorite_address)
        .expect("add favorite broadcaster");
    let banned = store
        .add_banned_broadcaster_for_session(&view_session, &banned_address)
        .expect("add banned broadcaster");
    assert_eq!(favorite.address, favorite_address);
    assert_eq!(banned.address, banned_address);

    drop(store);
    drop(db);

    let reopened = DesktopVaultStore::open(root_dir.clone()).expect("reopen vault store");
    let reloaded_session = reopened
        .load_view_session(TEST_PASSWORD, view_session.wallet_id())
        .expect("reload view session");
    let preferences = reopened
        .list_broadcaster_preferences_for_session(&reloaded_session)
        .expect("reload broadcaster preferences");

    assert_eq!(preferences.favorites, vec![favorite]);
    assert_eq!(preferences.banned, vec![banned]);

    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn broadcaster_preferences_validate_dedupe_remove_and_exclude() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "broadcaster-pref-validation", "Prefs");
    let address = railgun_recipient_for_derivation(14);

    assert!(matches!(
        store.add_favorite_broadcaster_for_session(&view_session, "not-a-0zk-address"),
        Err(VaultError::InvalidBroadcasterPreferenceAddress)
    ));
    assert!(matches!(
        store.add_banned_broadcaster_for_session(&view_session, "not-a-0zk-address"),
        Err(VaultError::InvalidBroadcasterPreferenceAddress)
    ));

    let favorite = store
        .add_favorite_broadcaster_for_session(&view_session, &address)
        .expect("add favorite broadcaster");
    let duplicate = store
        .add_favorite_broadcaster_for_session(&view_session, &address)
        .expect("add duplicate favorite broadcaster");
    assert_eq!(duplicate, favorite);
    let preferences = store
        .list_broadcaster_preferences_for_session(&view_session)
        .expect("preferences after duplicate");
    assert_eq!(preferences.favorites, vec![favorite]);
    assert_eq!(preferences.banned, [] as [BroadcasterPreferenceEntry; 0]);

    let banned = store
        .add_banned_broadcaster_for_session(&view_session, &address)
        .expect("ban favorite broadcaster");
    let preferences = store
        .list_broadcaster_preferences_for_session(&view_session)
        .expect("preferences after ban");
    assert_eq!(preferences.favorites, [] as [BroadcasterPreferenceEntry; 0]);
    assert_eq!(preferences.banned, vec![banned.clone()]);

    assert_eq!(
        store
            .remove_banned_broadcaster_for_session(&view_session, &address)
            .expect("remove banned broadcaster"),
        Some(banned)
    );
    assert!(
        store
            .remove_banned_broadcaster_for_session(&view_session, &address)
            .expect("remove missing banned broadcaster")
            .is_none()
    );
    let favorite = store
        .add_favorite_broadcaster_for_session(&view_session, &address)
        .expect("favorite after unban");
    assert_eq!(
        store
            .remove_favorite_broadcaster_for_session(&view_session, &address)
            .expect("remove favorite broadcaster"),
        Some(favorite)
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn broadcaster_preferences_banned_wins_and_invalid_persisted_entries_are_ignored() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session =
        import_wallet_with_metadata(&store, "broadcaster-pref-inconsistent", "Prefs");
    let address = railgun_recipient_for_derivation(15);
    let entry = BroadcasterPreferenceEntry { address };
    let favorite_uuid = generate_opaque_id().expect("favorite id");
    let banned_uuid = generate_opaque_id().expect("banned id");
    let invalid_uuid = generate_opaque_id().expect("invalid id");
    let favorite_record =
        broadcaster_favorite_record_entry(&view_session.view, &favorite_uuid, &entry)
            .expect("favorite record");
    let banned_record = broadcaster_banned_record_entry(&view_session.view, &banned_uuid, &entry)
        .expect("banned record");
    let invalid_record = broadcaster_favorite_record_entry(
        &view_session.view,
        &invalid_uuid,
        &BroadcasterPreferenceEntry {
            address: "not-a-0zk-address".to_string(),
        },
    )
    .expect("invalid favorite record");
    db.put_desktop_wallet_vault_records(&[favorite_record, banned_record, invalid_record])
        .expect("store inconsistent preferences");

    let preferences = store
        .list_broadcaster_preferences_for_session(&view_session)
        .expect("list inconsistent preferences");

    assert_eq!(preferences.favorites, [] as [BroadcasterPreferenceEntry; 0]);
    assert_eq!(preferences.banned, vec![entry]);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
