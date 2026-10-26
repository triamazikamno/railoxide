use super::super::*;
use super::helpers::*;
use std::fs;

#[test]
fn address_book_private_duplicate_scan_skips_locked_hardware_wallets() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "address-book-software", "Software");
    let descriptor = test_hardware_descriptor(0);
    let wallet = test_hardware_wallet(0);
    let metadata = store
        .new_hardware_wallet_metadata(
            TEST_PASSWORD,
            "address-book-hardware",
            "Hardware",
            descriptor.clone(),
        )
        .expect("hardware metadata");
    store
        .store_hardware_derived_wallet_with_metadata(
            TEST_PASSWORD,
            "address-book-hardware",
            descriptor.account_index,
            &wallet,
            &metadata,
            &test_hardware_view_access_key(descriptor.account_index),
        )
        .expect("store hardware wallet");

    let entry = store
        .add_private_address_book_entry_for_session(
            &view_session,
            "Private",
            &railgun_recipient_for_derivation(14),
        )
        .expect("add private address while hardware wallet is active");
    let updated = store
        .update_private_address_book_entry_for_session(
            &view_session,
            &entry.entry_uuid,
            "Private updated",
            &railgun_recipient_for_derivation(15),
        )
        .expect("update private address while hardware wallet is active");
    assert_eq!(updated.label, "Private updated");

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn address_books_persist_encrypted_and_load_without_spend_unlock() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "address-book-wallet", "Address Book");
    let private_address = railgun_recipient_for_derivation(7);
    let public_address = "0x1111111111111111111111111111111111111111";

    assert_eq!(
        store
            .list_private_address_book_entries_for_session(&view_session)
            .expect("empty private address book"),
        [] as [PrivateAddressBookEntry; 0]
    );
    assert_eq!(
        store
            .list_public_address_book_entries_for_session(&view_session)
            .expect("empty public address book"),
        [] as [PublicAddressBookEntry; 0]
    );

    let private_entry = store
        .add_private_address_book_entry_for_session(
            &view_session,
            "  Private friend  ",
            &private_address,
        )
        .expect("save private address book entry");
    let public_entry = store
        .add_public_address_book_entry_for_session(
            &view_session,
            "  Public friend  ",
            public_address,
        )
        .expect("save public address book entry");
    let private_payload = db
        .get_desktop_wallet_vault_record(&private_address_book_record_key(
            &private_entry.entry_uuid,
        ))
        .expect("load private address book record")
        .expect("private address book record present");
    let public_payload = db
        .get_desktop_wallet_vault_record(&public_address_book_record_key(&public_entry.entry_uuid))
        .expect("load public address book record")
        .expect("public address book record present");

    assert_eq!(private_entry.label, "Private friend");
    assert_eq!(private_entry.address, private_address);
    assert_eq!(private_entry.display_order, 0);
    assert_eq!(public_entry.label, "Public friend");
    assert_eq!(public_entry.address.to_checksum(None), public_address);
    assert_eq!(public_entry.display_order, 0);
    assert!(!contains_subsequence(&private_payload, b"Private friend"));
    assert!(!contains_subsequence(
        &private_payload,
        private_address.as_bytes()
    ));
    assert!(!contains_subsequence(&public_payload, b"Public friend"));
    assert!(!contains_subsequence(&public_payload, b"1111111111111111"));

    drop(store);
    drop(db);

    let reopened = DesktopVaultStore::open(root_dir.clone()).expect("reopen vault store");
    let reloaded_session = reopened
        .load_view_session(TEST_PASSWORD, view_session.wallet_id())
        .expect("reload view session");
    let private_entries = reopened
        .list_private_address_book_entries_for_session(&reloaded_session)
        .expect("reload private address book without spend unlock");
    let public_entries = reopened
        .list_public_address_book_entries_for_session(&reloaded_session)
        .expect("reload public address book without spend unlock");

    assert_eq!(private_entries, vec![private_entry]);
    assert_eq!(public_entries, vec![public_entry]);

    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn address_book_validation_rejects_invalid_labels_addresses_and_duplicates() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session =
        import_wallet_with_metadata(&store, "address-book-validation", "Address Book");
    let private_address = railgun_recipient_for_derivation(8);
    let public_address = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    assert!(matches!(
        store.add_private_address_book_entry_for_session(&view_session, "   ", &private_address),
        Err(VaultError::InvalidAddressBookLabel)
    ));
    assert!(matches!(
        store.add_private_address_book_entry_for_session(
            &view_session,
            "Invalid private",
            "not-a-0zk-address",
        ),
        Err(VaultError::InvalidPrivateAddressBookAddress)
    ));
    let private_entry = store
        .add_private_address_book_entry_for_session(&view_session, "Private", &private_address)
        .expect("save private address book entry");
    assert!(matches!(
        store.add_private_address_book_entry_for_session(
            &view_session,
            "Private duplicate",
            &private_entry.address,
        ),
        Err(VaultError::DuplicatePrivateAddressBookAddress)
    ));
    assert!(matches!(
        store.add_private_address_book_entry_for_session(
            &view_session,
            "Active private wallet",
            &view_session.receive_address().expect("receive address"),
        ),
        Err(VaultError::DuplicatePrivateAddressBookAddress)
    ));

    assert!(matches!(
        store.add_public_address_book_entry_for_session(&view_session, "   ", public_address),
        Err(VaultError::InvalidAddressBookLabel)
    ));
    assert!(matches!(
        store.add_public_address_book_entry_for_session(
            &view_session,
            "Invalid public",
            "not-an-address",
        ),
        Err(VaultError::InvalidPublicAddressBookAddress)
    ));
    let public_entry = store
        .add_public_address_book_entry_for_session(&view_session, "Public", public_address)
        .expect("save public address book entry");
    assert!(matches!(
        store.add_public_address_book_entry_for_session(
            &view_session,
            "Public duplicate",
            &public_entry.address.to_checksum(None).to_ascii_uppercase(),
        ),
        Err(VaultError::DuplicatePublicAddressBookAddress)
    ));
    let active_public_address = store
        .list_active_public_accounts_for_session(&view_session)
        .expect("active public accounts")[0]
        .address
        .to_checksum(None);
    assert!(matches!(
        store.add_public_address_book_entry_for_session(
            &view_session,
            "Active public account",
            &active_public_address,
        ),
        Err(VaultError::DuplicatePublicAddressBookAddress)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn address_book_entries_update_delete_and_allow_same_entry_address() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "address-book-update", "Address Book");
    let private_address = railgun_recipient_for_derivation(9);
    let other_private_address = railgun_recipient_for_derivation(10);
    let updated_private_address = railgun_recipient_for_derivation(11);
    let public_address = "0x1111111111111111111111111111111111111111";
    let other_public_address = "0x2222222222222222222222222222222222222222";
    let updated_public_address = "0x3333333333333333333333333333333333333333";

    let private_entry = store
        .add_private_address_book_entry_for_session(&view_session, "Private", &private_address)
        .expect("save private entry");
    let other_private_entry = store
        .add_private_address_book_entry_for_session(
            &view_session,
            "Other private",
            &other_private_address,
        )
        .expect("save other private entry");
    let public_entry = store
        .add_public_address_book_entry_for_session(&view_session, "Public", public_address)
        .expect("save public entry");
    let other_public_entry = store
        .add_public_address_book_entry_for_session(
            &view_session,
            "Other public",
            other_public_address,
        )
        .expect("save other public entry");

    let private_self_update = store
        .update_private_address_book_entry_for_session(
            &view_session,
            &private_entry.entry_uuid,
            "  Private renamed  ",
            &private_entry.address,
        )
        .expect("self update private entry");
    assert_eq!(private_self_update.label, "Private renamed");
    assert_eq!(private_self_update.address, private_entry.address);
    assert_eq!(
        private_self_update.display_order,
        private_entry.display_order
    );
    assert!(matches!(
        store.update_private_address_book_entry_for_session(
            &view_session,
            &private_entry.entry_uuid,
            "Duplicate private",
            &other_private_entry.address,
        ),
        Err(VaultError::DuplicatePrivateAddressBookAddress)
    ));
    assert!(matches!(
        store.update_private_address_book_entry_for_session(
            &view_session,
            &private_entry.entry_uuid,
            "Active private wallet",
            &view_session.receive_address().expect("receive address"),
        ),
        Err(VaultError::DuplicatePrivateAddressBookAddress)
    ));
    let private_updated = store
        .update_private_address_book_entry_for_session(
            &view_session,
            &private_entry.entry_uuid,
            "Private updated",
            &updated_private_address,
        )
        .expect("update private address");
    assert_eq!(private_updated.address, updated_private_address);
    assert!(matches!(
        store.update_private_address_book_entry_for_session(
            &view_session,
            "missing-private-entry",
            "Missing",
            &updated_private_address,
        ),
        Err(VaultError::PrivateAddressBookEntryNotFound)
    ));

    let public_self_update = store
        .update_public_address_book_entry_for_session(
            &view_session,
            &public_entry.entry_uuid,
            "  Public renamed  ",
            &public_entry.address.to_checksum(None),
        )
        .expect("self update public entry");
    assert_eq!(public_self_update.label, "Public renamed");
    assert_eq!(public_self_update.address, public_entry.address);
    assert_eq!(public_self_update.display_order, public_entry.display_order);
    assert!(matches!(
        store.update_public_address_book_entry_for_session(
            &view_session,
            &public_entry.entry_uuid,
            "Duplicate public",
            &other_public_entry.address.to_checksum(None),
        ),
        Err(VaultError::DuplicatePublicAddressBookAddress)
    ));
    let active_public_address = store
        .list_active_public_accounts_for_session(&view_session)
        .expect("active public accounts")[0]
        .address
        .to_checksum(None);
    assert!(matches!(
        store.update_public_address_book_entry_for_session(
            &view_session,
            &public_entry.entry_uuid,
            "Active public account",
            &active_public_address,
        ),
        Err(VaultError::DuplicatePublicAddressBookAddress)
    ));
    let public_updated = store
        .update_public_address_book_entry_for_session(
            &view_session,
            &public_entry.entry_uuid,
            "Public updated",
            updated_public_address,
        )
        .expect("update public address");
    assert_eq!(
        public_updated.address.to_checksum(None),
        updated_public_address
    );
    assert!(matches!(
        store.update_public_address_book_entry_for_session(
            &view_session,
            "missing-public-entry",
            "Missing",
            updated_public_address,
        ),
        Err(VaultError::PublicAddressBookEntryNotFound)
    ));

    let deleted_private = store
        .delete_private_address_book_entry_for_session(&view_session, &private_entry.entry_uuid)
        .expect("delete private entry");
    assert_eq!(deleted_private.entry_uuid, private_entry.entry_uuid);
    assert!(
        db.get_desktop_wallet_vault_record(&private_address_book_record_key(
            &private_entry.entry_uuid,
        ))
        .expect("load deleted private record")
        .is_none()
    );
    assert!(matches!(
        store.delete_private_address_book_entry_for_session(
            &view_session,
            &private_entry.entry_uuid
        ),
        Err(VaultError::PrivateAddressBookEntryNotFound)
    ));

    let deleted_public = store
        .delete_public_address_book_entry_for_session(&view_session, &public_entry.entry_uuid)
        .expect("delete public entry");
    assert_eq!(deleted_public.entry_uuid, public_entry.entry_uuid);
    assert!(
        db.get_desktop_wallet_vault_record(&public_address_book_record_key(
            &public_entry.entry_uuid
        ))
        .expect("load deleted public record")
        .is_none()
    );
    assert!(matches!(
        store.delete_public_address_book_entry_for_session(&view_session, &public_entry.entry_uuid),
        Err(VaultError::PublicAddressBookEntryNotFound)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
