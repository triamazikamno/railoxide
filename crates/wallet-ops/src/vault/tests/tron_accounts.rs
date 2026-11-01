use super::super::*;
use super::helpers::*;
use crate::tron::address::encode_tron_address;
use alloy::primitives::address;
use std::fs;

/// `TEST_MNEMONIC` with an empty passphrase at `m/44'/195'/0'/0/{index}`. The hex addresses come
/// from Foundry `cast wallet address --mnemonic-derivation-path "m/44'/195'/0'/0/{i}"`, and the
/// base58 forms from an independent Python base58check encoding of `0x41 ‖ address`.
const TRON_DERIVED: [(u32, Address, &str); 2] = [
    (
        0,
        address!("0xc8599111f29c1e1e061265b4af93ea1f274ad78a"),
        "TUEZSdKsoDHQMeZwihtdoBiN46zxhGWYdH",
    ),
    (
        1,
        address!("0xb6e708a39781c96bd399c7657780ff9fe9f052a8"),
        "TSeJkUh4Qv67VNFwY8LaAxERygNdy6NQZK",
    ),
];

#[test]
fn tron_derivation_matches_other_wallets() {
    let seed = bip39_seed_from_mnemonic(TEST_MNEMONIC, "").expect("seed");
    for (index, expected, base58) in TRON_DERIVED {
        let private_key = derive_tron_private_key_from_seed(&seed, index).expect("derive");
        let address = public_evm_address_from_private_key(&private_key).expect("address");
        assert_eq!(address, expected, "index {index}");
        assert_eq!(encode_tron_address(address), base58, "index {index}");
    }
}

#[test]
fn tron_accounts_stay_apart_from_public_accounts_and_refuse_visible_duplicates() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let session = import_wallet_with_metadata(&store, "tron-wallet", "Tron A");
    let public = store
        .import_public_account(TEST_PASSWORD, &session, IMPORT_PRIVATE_KEY_ONE, None, false)
        .expect("import public account");
    // The same key on Tron is a separate account and leaves the Public account unchanged.
    let imported = store
        .import_tron_account(
            TEST_PASSWORD,
            &session,
            IMPORT_PRIVATE_KEY_ONE,
            Some("Hot"),
            false,
        )
        .expect("import tron account");
    assert_eq!(imported.address, public.address);
    let first = store
        .add_derived_tron_account(TEST_PASSWORD, &session, None, None)
        .expect("add derived tron account");
    assert_eq!(first.source, TronAccountSource::Derived { index: 0 });
    assert_eq!(first.address, TRON_DERIVED[0].1);

    let public_ids: Vec<_> = store
        .list_public_accounts_for_session(&session, true)
        .expect("public accounts")
        .into_iter()
        .map(|account| account.public_account_uuid)
        .collect();
    assert!(public_ids.contains(&public.public_account_uuid));
    assert!(!public_ids.contains(&imported.tron_account_uuid));
    assert!(!public_ids.contains(&first.tron_account_uuid));
    let tron_ids: Vec<_> = store
        .list_tron_accounts_for_session(&session, true)
        .expect("tron accounts")
        .into_iter()
        .map(|account| account.tron_account_uuid)
        .collect();
    assert_eq!(
        tron_ids,
        [imported.tron_account_uuid, first.tron_account_uuid.clone()]
    );

    for global in [false, true] {
        assert!(matches!(
            store.import_tron_account(
                TEST_PASSWORD,
                &session,
                IMPORT_PRIVATE_KEY_ONE,
                None,
                global,
            ),
            Err(VaultError::DuplicatePublicAccountAddress)
        ));
    }

    // Hiding keeps the index, and the hidden address may be imported, but then not shown again.
    store
        .set_derived_tron_account_active(&session, &first.tron_account_uuid, false)
        .expect("hide derived");
    let second = store
        .add_derived_tron_account(TEST_PASSWORD, &session, None, None)
        .expect("add second derived");
    assert_eq!(second.source, TronAccountSource::Derived { index: 1 });
    assert_eq!(second.address, TRON_DERIVED[1].1);
    let seed = bip39_seed_from_mnemonic(TEST_MNEMONIC, "").expect("seed");
    let first_key = derive_tron_private_key_from_seed(&seed, 0).expect("derive");
    store
        .import_tron_account(
            TEST_PASSWORD,
            &session,
            &alloy::hex::encode(*first_key),
            None,
            false,
        )
        .expect("import hidden derived key");
    assert!(matches!(
        store.set_derived_tron_account_active(&session, &first.tron_account_uuid, true),
        Err(VaultError::DuplicatePublicAccountAddress)
    ));

    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("create spend grant");
    let signing_key = store
        .tron_account_signing_key(&mut grant, &session, &second.tron_account_uuid, None)
        .expect("derived signing key");
    assert_eq!(
        public_evm_address_from_private_key(&signing_key).expect("address"),
        second.address
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn hardware_derived_tron_accounts_need_the_device_for_each_key() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "hardware-tron-wallet";
    let descriptor = test_hardware_descriptor(0);
    let metadata = store
        .new_hardware_wallet_metadata(
            TEST_PASSWORD,
            wallet_id,
            "Ledger wallet",
            descriptor.clone(),
        )
        .expect("hardware metadata");
    store
        .store_hardware_derived_wallet_with_metadata(
            TEST_PASSWORD,
            wallet_id,
            descriptor.account_index,
            &test_hardware_wallet(descriptor.account_index),
            &metadata,
            &test_hardware_view_access_key(descriptor.account_index),
        )
        .expect("store hardware wallet");
    let session = load_test_hardware_view_session(&store, wallet_id, &descriptor);
    // `test_hardware_wallet` stands for the device output `[42; 32]`.
    let entropy = [42u8; 32];
    let mnemonic = bip39_mnemonic_from_entropy(&entropy).expect("mnemonic");
    let seed = bip39_seed_from_mnemonic(&mnemonic, "").expect("seed");

    for index in [0, 1] {
        let account = store
            .add_hardware_derived_tron_account(&session, &descriptor, &entropy, None)
            .expect("add hardware-derived tron account");
        assert_eq!(account.source, TronAccountSource::HardwareDerived { index });
        let expected = derive_tron_private_key_from_seed(&seed, index).expect("derive");
        assert_eq!(
            account.address,
            public_evm_address_from_private_key(&expected).expect("address")
        );
        // Each signature takes the device output again; nothing from the add is reused.
        for _ in 0..2 {
            let key = store
                .hardware_derived_tron_account_signing_key(
                    &session,
                    &account.tron_account_uuid,
                    &descriptor,
                    &entropy,
                )
                .expect("hardware-derived signing key");
            assert_eq!(&*key, &*expected);
        }
        assert!(matches!(
            store.hardware_derived_tron_account_signing_key(
                &session,
                &account.tron_account_uuid,
                &descriptor,
                &[43u8; 32],
            ),
            Err(VaultError::HardwareWalletIdentityMismatch)
        ));
        let mut grant = store
            .create_spend_grant(TEST_PASSWORD)
            .expect("create spend grant");
        assert!(matches!(
            store.tron_account_signing_key(&mut grant, &session, &account.tron_account_uuid, None),
            Err(VaultError::InvalidPublicAccountOperation)
        ));
    }

    // An imported key under a hardware-derived wallet is an ordinary import: Tron only, and it
    // signs without the device.
    let imported = store
        .import_tron_account(TEST_PASSWORD, &session, IMPORT_PRIVATE_KEY_ONE, None, false)
        .expect("import tron account");
    assert_eq!(imported.source, TronAccountSource::Imported);
    assert!(
        store
            .list_public_accounts_for_session(&session, true)
            .expect("public accounts")
            .iter()
            .all(|account| account.address != imported.address)
    );
    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("create spend grant");
    let key = store
        .tron_account_signing_key(&mut grant, &session, &imported.tron_account_uuid, None)
        .expect("imported signing key");
    assert_eq!(
        &*key,
        &*parse_public_evm_private_key(IMPORT_PRIVATE_KEY_ONE).expect("private key")
    );

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn tron_import_secret_round_trips_and_delete_removes_records() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let session = import_wallet_with_metadata(&store, "tron-secret-wallet", "Tron A");
    let account = store
        .import_tron_account(TEST_PASSWORD, &session, IMPORT_PRIVATE_KEY_TWO, None, false)
        .expect("import tron account");
    let private_key = parse_public_evm_private_key(IMPORT_PRIVATE_KEY_TWO).expect("private key");
    let metadata_key = tron_account_metadata_record_key(&account.tron_account_uuid);
    let secret_key = tron_account_secret_record_key(&account.tron_account_uuid);
    let metadata_payload = db
        .get_desktop_wallet_vault_record(&metadata_key)
        .expect("load metadata record")
        .expect("metadata record present");
    let secret_payload = db
        .get_desktop_wallet_vault_record(&secret_key)
        .expect("load secret record")
        .expect("secret record present");
    assert!(!contains_subsequence(&secret_payload, &*private_key));
    // The record kind is bound into the AAD, so a Tron record never opens as a Public one.
    let metadata_record: EncryptedRecord =
        rmp_serde::from_slice(&metadata_payload).expect("decode metadata record");
    assert!(
        session
            .view
            .decrypt_record(
                RecordKind::PublicAccountMetadata,
                &account.tron_account_uuid,
                &metadata_record,
            )
            .is_err()
    );

    let mut grant = store
        .create_spend_grant(TEST_PASSWORD)
        .expect("create spend grant");
    let signing_key = store
        .tron_account_signing_key(&mut grant, &session, &account.tron_account_uuid, None)
        .expect("imported signing key");
    assert_eq!(&*signing_key, &*private_key);

    store
        .delete_imported_tron_account(&session, &account.tron_account_uuid)
        .expect("delete imported account");
    for key in [metadata_key, secret_key] {
        assert!(
            db.get_desktop_wallet_vault_record(&key)
                .expect("load deleted record")
                .is_none()
        );
    }

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
