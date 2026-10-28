use super::super::*;
use super::helpers::*;
use std::fs;

#[test]
fn hardware_public_account_stores_descriptor_without_secret_at_unused_indexes() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "hardware-public-wallet";
    let wallet_descriptor = test_hardware_descriptor(0);
    let wallet = test_hardware_wallet(wallet_descriptor.account_index);
    let wallet_metadata = store
        .new_hardware_wallet_metadata(
            TEST_PASSWORD,
            wallet_id,
            "Ledger wallet",
            wallet_descriptor.clone(),
        )
        .expect("hardware metadata");
    store
        .store_hardware_derived_wallet_with_metadata(
            TEST_PASSWORD,
            wallet_id,
            wallet_descriptor.account_index,
            &wallet,
            &wallet_metadata,
            &test_hardware_view_access_key(wallet_descriptor.account_index),
        )
        .expect("store hardware wallet");
    let view_session = load_test_hardware_view_session(&store, wallet_id, &wallet_descriptor);
    let descriptor =
        HardwarePublicAccountDescriptor::for_device_index(HardwareDeviceKind::Ledger, 0)
            .expect("hardware public descriptor");
    let address = Address::from([0x44; 20]);

    let confirmed =
        crate::hardware::ConfirmedHardwarePublicAccount::new_for_tests(descriptor.clone(), address);
    let account = store
        .add_hardware_public_account(&view_session, &confirmed, Some("Ledger 1"))
        .expect("add hardware public account");

    assert_eq!(account.source, PublicAccountSource::HardwareDerived);
    assert_eq!(account.derivation_index, Some(0));
    assert_eq!(account.hardware_descriptor.as_ref(), Some(&descriptor));
    assert_eq!(descriptor.path_display(), "m/44'/60'/0'/0/0",);
    assert!(
        db.get_desktop_wallet_vault_record(&public_account_secret_record_key(
            &account.public_account_uuid,
        ))
        .expect("load public secret record")
        .is_none()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&wallet_spend_record_key(wallet_id))
            .expect("load wallet spend record")
            .is_none()
    );
    assert_eq!(
        store
            .next_derived_public_account_index_for_session(&view_session)
            .expect("next hardware public index"),
        1,
    );

    let index_500 = HardwarePublicAccountDescriptor::for_wallet_public_index(
        HardwareDeviceKind::Ledger,
        0,
        500,
    )
    .expect("index 500 descriptor");
    let account_500 = store
        .add_hardware_public_account(
            &view_session,
            &crate::hardware::ConfirmedHardwarePublicAccount::new_for_tests(
                index_500.clone(),
                Address::from([0x45; 20]),
            ),
            None,
        )
        .expect("add index 500 after index 0");
    assert_eq!(account_500.derivation_index, Some(500));
    assert_eq!(
        store
            .next_derived_public_account_index_for_session(&view_session)
            .expect("next hardware public index after 500"),
        501,
    );
    store
        .deactivate_derived_public_account(&view_session, &account_500.public_account_uuid)
        .expect("hide index 500");
    // Index 0 is active and index 500 hidden; both stay taken.
    for used in [descriptor, index_500] {
        let confirmed = crate::hardware::ConfirmedHardwarePublicAccount::new_for_tests(
            used,
            Address::from([0x46; 20]),
        );
        assert!(matches!(
            store.add_hardware_public_account(&view_session, &confirmed, None),
            Err(VaultError::PublicAccountIndexAlreadyAdded)
        ));
    }

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn hardware_public_account_requires_hardware_view_session() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "software-wallet", "Software wallet");
    let public_index = store
        .next_derived_public_account_index_for_session(&view_session)
        .expect("next public index");
    let descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
        HardwareDeviceKind::Ledger,
        view_session.derivation_index(),
        public_index,
    )
    .expect("hardware public descriptor");
    let address = Address::from([0x45; 20]);
    let confirmed =
        crate::hardware::ConfirmedHardwarePublicAccount::new_for_tests(descriptor, address);

    assert!(matches!(
        store.add_hardware_public_account(&view_session, &confirmed, Some("Ledger bypass")),
        Err(VaultError::HardwareWalletViewRequiresDevice)
    ));
    assert!(
        store
            .list_public_accounts_for_session(&view_session, true)
            .expect("public accounts")
            .into_iter()
            .all(|account| account.source != PublicAccountSource::HardwareDerived)
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn hardware_public_account_paths_partition_by_private_wallet_index() {
    let (root_dir, db, store) = desktop_store_with_vault();

    for account_index in 0..=1 {
        let wallet_id = format!("hardware-public-wallet-{account_index}");
        let wallet_descriptor = test_hardware_descriptor(account_index);
        let wallet = test_hardware_wallet(wallet_descriptor.account_index);
        let wallet_metadata = store
            .new_hardware_wallet_metadata(
                TEST_PASSWORD,
                &wallet_id,
                &format!("Ledger wallet {account_index}"),
                wallet_descriptor.clone(),
            )
            .expect("hardware metadata");
        store
            .store_hardware_derived_wallet_with_metadata(
                TEST_PASSWORD,
                &wallet_id,
                wallet_descriptor.account_index,
                &wallet,
                &wallet_metadata,
                &test_hardware_view_access_key(wallet_descriptor.account_index),
            )
            .expect("store hardware wallet");
        let view_session = load_test_hardware_view_session(&store, &wallet_id, &wallet_descriptor);
        let descriptor = HardwarePublicAccountDescriptor::for_wallet_public_index(
            HardwareDeviceKind::Ledger,
            view_session.derivation_index(),
            0,
        )
        .expect("hardware public descriptor");
        let address = Address::from([0x44 + u8::try_from(account_index).expect("index fits"); 20]);

        let confirmed = crate::hardware::ConfirmedHardwarePublicAccount::new_for_tests(
            descriptor.clone(),
            address,
        );
        let account = store
            .add_hardware_public_account(&view_session, &confirmed, None)
            .expect("add hardware public account");

        assert_eq!(account.derivation_index, Some(0));
        assert_eq!(account.hardware_descriptor.as_ref(), Some(&descriptor));
        assert_eq!(descriptor.wallet_account_index, account_index);
        assert_eq!(descriptor.public_account_index, 0);
    }

    let first_descriptor = test_hardware_descriptor(0);
    let second_descriptor = test_hardware_descriptor(1);
    let first_session =
        load_test_hardware_view_session(&store, "hardware-public-wallet-0", &first_descriptor);
    let second_session =
        load_test_hardware_view_session(&store, "hardware-public-wallet-1", &second_descriptor);
    let first = store
        .list_public_accounts_for_session(&first_session, true)
        .expect("first public accounts");
    let second = store
        .list_public_accounts_for_session(&second_session, true)
        .expect("second public accounts");

    assert_eq!(
        first[0]
            .hardware_descriptor
            .as_ref()
            .expect("first hardware descriptor")
            .path_display(),
        "m/44'/60'/0'/0/0",
    );
    assert_eq!(
        second[0]
            .hardware_descriptor
            .as_ref()
            .expect("second hardware descriptor")
            .path_display(),
        "m/44'/60'/1'/0/0",
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn public_account_import_encrypts_secret_and_delete_removes_records() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "public-secret-wallet", "Public A");
    let account = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("  Hot account  "),
            false,
        )
        .expect("import public account");
    let private_key = parse_public_evm_private_key(IMPORT_PRIVATE_KEY_ONE).expect("private key");
    let metadata_payload = db
        .get_desktop_wallet_vault_record(&public_account_metadata_record_key(
            &account.public_account_uuid,
        ))
        .expect("load public metadata record")
        .expect("public metadata record present");
    let secret_payload = db
        .get_desktop_wallet_vault_record(&public_account_secret_record_key(
            &account.public_account_uuid,
        ))
        .expect("load public secret record")
        .expect("public secret record present");
    let secret_record: EncryptedRecord =
        rmp_serde::from_slice(&secret_payload).expect("decode secret record");

    assert_eq!(account.label.as_deref(), Some("Hot account"));
    assert!(!contains_subsequence(&metadata_payload, b"Hot account"));
    assert!(!contains_subsequence(&metadata_payload, &*private_key));
    assert!(!contains_subsequence(&secret_payload, &*private_key));
    assert!(
        view_session
            .view
            .decrypt_record(
                RecordKind::PublicAccountSecret,
                &account.public_account_uuid,
                &secret_record,
            )
            .is_err()
    );

    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("create spend grant");
    let signing_key = store
        .public_account_signing_key(&mut grant, &view_session, &account.public_account_uuid)
        .expect("imported signing key");
    assert_eq!(&*signing_key, &*private_key);

    let deleted = store
        .delete_imported_public_account(&view_session, &account.public_account_uuid)
        .expect("delete imported account");
    assert_eq!(deleted.public_account_uuid, account.public_account_uuid);
    assert!(
        db.get_desktop_wallet_vault_record(&public_account_metadata_record_key(
            &account.public_account_uuid,
        ))
        .expect("load deleted metadata")
        .is_none()
    );
    assert!(
        db.get_desktop_wallet_vault_record(&public_account_secret_record_key(
            &account.public_account_uuid,
        ))
        .expect("load deleted secret")
        .is_none()
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn public_account_visibility_scope_duplicates_and_next_index() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let first_session = import_wallet_with_metadata(&store, "public-wallet-a", "Public A");
    let second_session = import_wallet_with_metadata(&store, "public-wallet-b", "Public B");
    let scoped = store
        .import_public_account(
            TEST_PASSWORD,
            &first_session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("Scoped"),
            false,
        )
        .expect("import scoped account");
    let global = store
        .import_public_account(
            TEST_PASSWORD,
            &first_session,
            IMPORT_PRIVATE_KEY_TWO,
            Some("Global"),
            true,
        )
        .expect("import global account");

    let first_active = store
        .list_active_public_accounts_for_session(&first_session)
        .expect("first active accounts");
    let second_active = store
        .list_active_public_accounts_for_session(&second_session)
        .expect("second active accounts");
    assert!(
        first_active
            .iter()
            .any(|account| account.source == PublicAccountSource::Derived)
    );
    assert!(
        first_active
            .iter()
            .any(|account| account.public_account_uuid == scoped.public_account_uuid)
    );
    assert!(
        first_active
            .iter()
            .any(|account| account.public_account_uuid == global.public_account_uuid)
    );
    assert!(
        !second_active
            .iter()
            .any(|account| account.public_account_uuid == scoped.public_account_uuid)
    );
    assert!(
        second_active
            .iter()
            .any(|account| account.public_account_uuid == global.public_account_uuid)
    );

    assert!(matches!(
        store.import_public_account(
            TEST_PASSWORD,
            &first_session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("Duplicate scoped"),
            false,
        ),
        Err(VaultError::DuplicatePublicAccountAddress)
    ));
    assert!(matches!(
        store.import_public_account(
            TEST_PASSWORD,
            &first_session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("Duplicate global"),
            true,
        ),
        Err(VaultError::DuplicatePublicAccountAddress)
    ));
    assert!(matches!(
        store.import_public_account(
            TEST_PASSWORD,
            &second_session,
            IMPORT_PRIVATE_KEY_TWO,
            Some("Duplicate active global"),
            false,
        ),
        Err(VaultError::DuplicatePublicAccountAddress)
    ));

    let derived = store
        .add_derived_public_account(TEST_PASSWORD, &first_session, Some("Derived 1"))
        .expect("add derived account");
    assert_eq!(derived.derivation_index, Some(1));
    store
        .deactivate_derived_public_account(&first_session, &derived.public_account_uuid)
        .expect("deactivate derived account");
    let all_first_accounts = store
        .list_public_accounts_for_session(&first_session, true)
        .expect("first accounts including inactive");
    assert!(all_first_accounts.iter().any(|account| {
        account.public_account_uuid == derived.public_account_uuid
            && account.status == PublicAccountStatus::Inactive
    }));
    let relabeled_inactive = store
        .update_public_account_label(
            &first_session,
            &derived.public_account_uuid,
            Some("Inactive derived"),
        )
        .expect("edit inactive derived label");
    assert_eq!(relabeled_inactive.status, PublicAccountStatus::Inactive);
    assert_eq!(
        relabeled_inactive.label.as_deref(),
        Some("Inactive derived")
    );
    assert_eq!(
        store
            .next_derived_public_account_index_for_session(&first_session)
            .expect("next derived index"),
        2
    );
    let next = store
        .add_derived_public_account(TEST_PASSWORD, &first_session, Some("Derived 2"))
        .expect("add next derived account");
    assert_eq!(next.derivation_index, Some(2));
    assert!(
        !store
            .list_active_public_accounts_for_session(&first_session)
            .expect("active after deactivate")
            .iter()
            .any(|account| account.public_account_uuid == derived.public_account_uuid)
    );
    let activated = store
        .activate_derived_public_account(&first_session, &derived.public_account_uuid)
        .expect("activate derived account");
    assert_eq!(activated.status, PublicAccountStatus::Active);
    assert!(
        store
            .list_active_public_accounts_for_session(&first_session)
            .expect("active after activate")
            .iter()
            .any(|account| account.public_account_uuid == derived.public_account_uuid)
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn inactive_derived_account_activate_rejects_active_duplicate() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "activate-duplicate-wallet", "Public A");
    let derived = store
        .add_derived_public_account(TEST_PASSWORD, &view_session, Some("Derived 1"))
        .expect("add derived account");
    store
        .deactivate_derived_public_account(&view_session, &derived.public_account_uuid)
        .expect("deactivate derived account");
    let derived_private_key =
        derive_public_evm_private_key_from_mnemonic(TEST_MNEMONIC, 1).expect("derived key");
    let derived_private_key_hex = format!("0x{}", alloy::hex::encode(derived_private_key));
    store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            &derived_private_key_hex,
            Some("Imported duplicate"),
            false,
        )
        .expect("import inactive derived duplicate");

    assert!(matches!(
        store.activate_derived_public_account(&view_session, &derived.public_account_uuid),
        Err(VaultError::DuplicatePublicAccountAddress)
    ));
    let inactive_accounts = store
        .list_public_accounts_for_session(&view_session, true)
        .expect("accounts including inactive");
    assert!(inactive_accounts.iter().any(|account| {
        account.public_account_uuid == derived.public_account_uuid
            && account.status == PublicAccountStatus::Inactive
    }));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn public_account_status_reads_legacy_hidden_as_inactive() {
    let status: PublicAccountStatus =
        serde_json::from_str("\"Hidden\"").expect("legacy hidden status");

    assert_eq!(status, PublicAccountStatus::Inactive);
    assert_eq!(
        serde_json::to_string(&status).expect("serialize inactive status"),
        "\"Inactive\"",
    );
}
#[test]
fn derived_duplicate_address_is_rejected() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "derived-duplicate-wallet", "Public A");
    let derived_private_key =
        derive_public_evm_private_key_from_mnemonic(TEST_MNEMONIC, 1).expect("derived key");
    let derived_private_key_hex = format!("0x{}", alloy::hex::encode(derived_private_key));
    store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            &derived_private_key_hex,
            Some("Imported index 1"),
            false,
        )
        .expect("import duplicate derived address");

    assert!(matches!(
        store.add_derived_public_account(TEST_PASSWORD, &view_session, Some("Derived 1")),
        Err(VaultError::DuplicatePublicAccountAddress)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn derived_public_account_preview_writes_nothing_and_adds_selected_indexes() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "preview-wallet", "Public A");
    let other_session = import_wallet_with_metadata(&store, "preview-other-wallet", "Public B");
    let hidden = store
        .add_derived_public_account(TEST_PASSWORD, &view_session, None)
        .expect("add index one");
    store
        .deactivate_derived_public_account(&view_session, &hidden.public_account_uuid)
        .expect("hide index one");
    let index_three_key =
        derive_public_evm_private_key_from_mnemonic(TEST_MNEMONIC, 3).expect("index three key");
    store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            &format!("0x{}", alloy::hex::encode(index_three_key)),
            None,
            false,
        )
        .expect("import index three address");
    let before = db
        .list_desktop_wallet_vault_records("")
        .expect("records before preview");

    assert!(matches!(
        store.derived_public_account_preview("wrong password", &view_session, None),
        Err(VaultError::UnlockFailed)
    ));
    let preview = store
        .derived_public_account_preview(TEST_PASSWORD, &view_session, None)
        .expect("preview");
    let page = preview.page(0).expect("first page");
    assert_eq!(page.len(), 20);
    for (index, address) in page {
        assert_eq!(
            address,
            derive_public_evm_address_from_mnemonic_with_passphrase(TEST_MNEMONIC, "", index)
                .expect("single-account address")
        );
    }
    let last_page = preview
        .page(DerivedPublicAccountPreview::INDEX_LIMIT - 1)
        .expect("last page");
    assert_eq!(last_page.len(), 1);
    assert!(
        preview
            .address(DerivedPublicAccountPreview::INDEX_LIMIT)
            .is_err()
    );
    assert!(matches!(
        store.add_previewed_public_accounts(&other_session, &preview, &[4]),
        Err(VaultError::PublicAccountPreviewWalletMismatch)
    ));
    assert!(matches!(
        store.add_previewed_public_accounts(
            &view_session,
            &preview,
            &(0..101).collect::<Vec<u32>>()
        ),
        Err(VaultError::InvalidPublicAccountSelection)
    ));
    assert!(matches!(
        store.previewed_public_account_skip(&other_session, &preview, 4),
        Err(VaultError::PublicAccountPreviewWalletMismatch)
    ));
    for (index, skip) in [
        (1, Some(PreviewedPublicAccountSkip::AlreadyAdded)),
        (3, Some(PreviewedPublicAccountSkip::DuplicateAddress)),
        (4, None),
    ] {
        assert_eq!(
            store
                .previewed_public_account_skip(&view_session, &preview, index)
                .expect("skip check"),
            skip
        );
    }
    assert_eq!(
        db.list_desktop_wallet_vault_records("")
            .expect("records after preview and rejected adds"),
        before
    );

    let result = store
        .add_previewed_public_accounts(&view_session, &preview, &[0, 1, 3, 4, 4, 23])
        .expect("add previewed accounts");
    assert_eq!(
        result.skipped,
        vec![
            (0, PreviewedPublicAccountSkip::AlreadyAdded),
            (1, PreviewedPublicAccountSkip::AlreadyAdded),
            (3, PreviewedPublicAccountSkip::DuplicateAddress),
        ]
    );
    let stored = store
        .list_active_public_accounts_for_session(&view_session)
        .expect("accounts after add");
    let mut display_orders = Vec::new();
    for (account, index) in result.added.iter().zip([4, 23]) {
        assert_eq!(account.derivation_index, Some(index));
        assert_eq!(account.address, preview.address(index).expect("address"));
        assert_eq!(account.source, PublicAccountSource::Derived);
        assert_eq!(account.label, None);
        assert!(account.is_active_for_wallet("preview-wallet") && !account.is_global());
        assert!(stored.contains(account));
        display_orders.push(account.display_order);
    }
    assert_eq!(result.added.len(), 2);
    assert_eq!(display_orders[1], display_orders[0] + 1);

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
#[test]
fn hardware_account_key_preview_matches_mnemonic_derivation() {
    use crate::hardware::{TEST_ACCOUNT_CHAIN_CODE, TEST_ACCOUNT_PUBLIC_KEY};
    use coins_bip32::prelude::{Parent, XKeyInfo, XPriv};

    let seed = bip39_seed_from_mnemonic(TEST_MNEMONIC, "").expect("seed");
    let account_key = XPriv::root_from_seed(&seed[..], None)
        .and_then(|key| key.derive_child(coins_bip32::BIP32_HARDEN + 44))
        .and_then(|key| key.derive_child(coins_bip32::BIP32_HARDEN + 60))
        .and_then(|key| key.derive_child(coins_bip32::BIP32_HARDEN))
        .and_then(|key| key.derive_child(0))
        .expect("account key")
        .verify_key();
    let verifying_key: &coins_bip32::ecdsa::VerifyingKey = account_key.as_ref();
    let info: &XKeyInfo = account_key.as_ref();
    assert_eq!(
        verifying_key.to_encoded_point(false).as_bytes(),
        TEST_ACCOUNT_PUBLIC_KEY
    );
    assert_eq!(info.chain_code.0, TEST_ACCOUNT_CHAIN_CODE);

    let compressed = verifying_key.to_encoded_point(true);
    for public_key in [&TEST_ACCOUNT_PUBLIC_KEY[..], compressed.as_bytes()] {
        let preview = DerivedPublicAccountPreview::from_account_public_key(
            "hardware-wallet",
            public_key,
            TEST_ACCOUNT_CHAIN_CODE,
        )
        .expect("preview from account key");
        for index in [0, 1, 500] {
            assert_eq!(
                preview.address(index).expect("preview address"),
                derive_public_evm_address_from_mnemonic_with_passphrase(TEST_MNEMONIC, "", index)
                    .expect("mnemonic address")
            );
        }
    }
    assert!(matches!(
        DerivedPublicAccountPreview::from_account_public_key(
            "hardware-wallet",
            &[0x04; 65],
            TEST_ACCOUNT_CHAIN_CODE,
        ),
        Err(VaultError::PublicEvmKeyDerivation)
    ));
}
#[test]
fn public_account_signing_key_resolves_derived_and_imported_accounts() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let view_session = import_wallet_with_metadata(&store, "public-signing-wallet", "Public A");
    let derived = store
        .list_active_public_accounts_for_session(&view_session)
        .expect("active accounts")
        .into_iter()
        .find(|account| account.source == PublicAccountSource::Derived)
        .expect("derived account");
    assert!(
        db.get_desktop_wallet_vault_record(&public_account_secret_record_key(
            &derived.public_account_uuid,
        ))
        .expect("load derived secret record")
        .is_none()
    );

    let mut derived_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("derived spend grant");
    let derived_key = store
        .public_account_signing_key(
            &mut derived_grant,
            &view_session,
            &derived.public_account_uuid,
        )
        .expect("derived signing key");
    assert_eq!(
        public_evm_address_from_private_key(&derived_key).expect("derived address"),
        derived.address
    );
    assert!(!derived_grant.is_valid());

    let inactive = store
        .deactivate_derived_public_account(&view_session, &derived.public_account_uuid)
        .expect("deactivate derived account");
    let mut inactive_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("inactive derived spend grant");
    let inactive_key = store
        .public_account_signing_key(
            &mut inactive_grant,
            &view_session,
            &inactive.public_account_uuid,
        )
        .expect("inactive derived signing key");
    assert_eq!(
        public_evm_address_from_private_key(&inactive_key).expect("inactive derived address"),
        inactive.address
    );
    assert!(!inactive_grant.is_valid());

    let imported = store
        .import_public_account(
            TEST_PASSWORD,
            &view_session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("Imported"),
            false,
        )
        .expect("import account");
    let expected_private_key =
        parse_public_evm_private_key(IMPORT_PRIVATE_KEY_ONE).expect("private key");
    let mut imported_grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("imported spend grant");
    let imported_key = store
        .public_account_signing_key(
            &mut imported_grant,
            &view_session,
            &imported.public_account_uuid,
        )
        .expect("imported signing key");
    assert_eq!(&*imported_key, &*expected_private_key);
    assert!(!imported_grant.is_valid());

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
