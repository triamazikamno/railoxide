use super::super::*;
use super::helpers::*;
use alloy::primitives::Bytes;
use broadcaster_core::crypto::railgun::ShareableViewingKey;
use std::fs;

const SDK_VECTOR_MNEMONIC: &str =
    "pause crystal tornado alcohol genre cement fade large song like bag where";
const SDK_VECTOR_ADDRESS: &str = "0zk1qykzjxctynyz4z43pukckpv43jyzhyvy0ehrd5wuc54l5enqf9qfrrv7j6fe3z53la7enqphqvxys9aqyp9xx0km95ehqslx8apmu8l7anc7emau4tvsultrkvd";
const SDK_VECTOR_SHAREABLE_VIEWING_KEY: &str = "82a57670726976d94032643030623234396632646337313236303565336263653364373665376631313931373933363436393365333931666566643963323764303161396262336433a473707562d94030633661376436386331663437303262613764666134613361353236323035303765386637366632393139326363666637653861366231303637393062316165";

fn decode_shareable_viewing_key(key: &str) -> ShareableViewingKey {
    ShareableViewingKey::from(Bytes::from(
        alloy::hex::decode(key).expect("decode key hex"),
    ))
}

#[test]
fn software_wallet_exports_mnemonic_after_password_check() {
    let (root_dir, db, store) = desktop_store_with_vault();
    import_wallet_with_metadata(&store, TEST_WALLET_ID, "Software");

    let mnemonic = store
        .export_wallet_mnemonic(TEST_PASSWORD, TEST_WALLET_ID)
        .expect("export mnemonic");

    assert_eq!(&*mnemonic, TEST_MNEMONIC);
    assert!(matches!(
        store.export_wallet_mnemonic("wrong password", TEST_WALLET_ID),
        Err(VaultError::UnlockFailed)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn software_shareable_viewing_key_matches_sdk_vector() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "sdk-vector-wallet";
    let metadata = store
        .new_wallet_metadata(
            TEST_PASSWORD,
            wallet_id,
            0,
            WalletSource::Imported,
            "SDK vector",
        )
        .expect("wallet metadata");
    store
        .import_wallet_mnemonic_with_metadata(
            TEST_PASSWORD,
            wallet_id,
            0,
            "english",
            SDK_VECTOR_MNEMONIC,
            &metadata,
        )
        .expect("import sdk vector wallet");

    let shareable_key = store
        .export_wallet_shareable_viewing_key(TEST_PASSWORD, wallet_id)
        .expect("export shareable key");
    let view_session = store
        .load_view_session(TEST_PASSWORD, wallet_id)
        .expect("load view session");
    let receive_address = view_session.receive_address().expect("receive address");
    let derived_address = WalletKeys::from_mnemonic(SDK_VECTOR_MNEMONIC, 0)
        .expect("derive wallet")
        .viewing
        .derive_address(None)
        .expect("derive wallet address")
        .to_string();

    assert_eq!(&*shareable_key, SDK_VECTOR_SHAREABLE_VIEWING_KEY);
    assert_eq!(receive_address, SDK_VECTOR_ADDRESS);
    assert_eq!(receive_address, derived_address);
    assert_eq!(
        decode_shareable_viewing_key(&shareable_key)
            .derive_address(None)
            .expect("decode exported wallet address")
            .to_string(),
        receive_address,
    );
    assert!(matches!(
        store.export_wallet_shareable_viewing_key("wrong password", wallet_id),
        Err(VaultError::UnlockFailed)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn shareable_viewing_key_excludes_spend_material() {
    let (root_dir, db, store) = desktop_store_with_vault();
    import_wallet_with_metadata(&store, TEST_WALLET_ID, "Software");
    let shareable_key = store
        .export_wallet_shareable_viewing_key(TEST_PASSWORD, TEST_WALLET_ID)
        .expect("export shareable key");
    let encoded_payload = alloy::hex::decode(&*shareable_key).expect("decode key hex");
    let entropy = bip39_entropy_from_mnemonic(TEST_MNEMONIC).expect("mnemonic entropy");
    let wallet = WalletKeys::from_mnemonic(TEST_MNEMONIC, 0).expect("derive wallet");

    assert!(!contains_subsequence(
        &encoded_payload,
        TEST_MNEMONIC.as_bytes()
    ));
    assert!(!contains_subsequence(&encoded_payload, &entropy));
    assert!(!contains_subsequence(
        &encoded_payload,
        &wallet.spending_private_key
    ));
    assert!(!contains_subsequence(
        shareable_key.as_bytes(),
        TEST_MNEMONIC.as_bytes()
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn hardware_wallet_mnemonic_export_is_unavailable() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "hardware-mnemonic-unavailable";
    let descriptor = test_hardware_descriptor(0);
    let wallet = test_hardware_wallet(descriptor.account_index);
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
            &wallet,
            &metadata,
            &test_hardware_view_access_key(descriptor.account_index),
        )
        .expect("store hardware wallet");

    assert!(matches!(
        store.export_wallet_mnemonic(TEST_PASSWORD, wallet_id),
        Err(VaultError::WalletMnemonicUnavailable)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}

#[test]
fn hardware_shareable_viewing_key_requires_active_matching_session() {
    let (root_dir, db, store) = desktop_store_with_vault();
    let wallet_id = "hardware-shareable-key";
    let descriptor = test_hardware_descriptor(0);
    let wallet = test_hardware_wallet(descriptor.account_index);
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
            &wallet,
            &metadata,
            &test_hardware_view_access_key(descriptor.account_index),
        )
        .expect("store hardware wallet");

    assert!(matches!(
        store.export_hardware_wallet_shareable_viewing_key(TEST_PASSWORD, wallet_id, None),
        Err(VaultError::HardwareWalletViewRequiresDevice)
    ));
    assert!(matches!(
        store.export_wallet_shareable_viewing_key(TEST_PASSWORD, wallet_id),
        Err(VaultError::HardwareWalletViewRequiresDevice)
    ));

    let view_session = load_test_hardware_view_session(&store, wallet_id, &descriptor);
    let shareable_key = store
        .export_hardware_wallet_shareable_viewing_key(TEST_PASSWORD, wallet_id, Some(&view_session))
        .expect("export hardware shareable key");
    let decoded_address = decode_shareable_viewing_key(&shareable_key)
        .derive_address(None)
        .expect("derive decoded address")
        .to_string();

    assert_eq!(
        decoded_address,
        view_session.receive_address().expect("receive address")
    );
    assert!(matches!(
        store.export_hardware_wallet_shareable_viewing_key(
            "wrong password",
            wallet_id,
            Some(&view_session),
        ),
        Err(VaultError::UnlockFailed)
    ));

    drop(store);
    drop(db);
    fs::remove_dir_all(root_dir).expect("remove temp db dir");
}
