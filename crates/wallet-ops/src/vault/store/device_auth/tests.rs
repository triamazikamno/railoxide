use super::{
    BIOMETRIC_UNLOCK_VERSION, DesktopVaultStore, DeviceAuthStatus, PASSWORD_PADDING_BLOCK,
    StoredDeviceAuth, VaultError, pad_password, unpad_password,
};
use crate::DesktopWalletVaultDbExt as _;
use crate::device_auth::{DeviceAuthError, DeviceAuthMethod, device_auth_available};
use crate::vault::KdfParams;

#[test]
fn device_auth_enrollments_preserve_legacy_touch_id_and_recover_independently() {
    let directory =
        std::env::temp_dir().join(format!("railoxide-device-auth-{}", rand::random::<u64>()));
    let store = DesktopVaultStore::open(directory.clone()).unwrap();
    let created = store
        .create_vault_with_params("test password", KdfParams::new(1024, 1, 1))
        .unwrap();
    // The persisted v1 representation predates Apple Watch. In particular,
    // it has no method field and uses the original database key.
    let legacy = serde_json::json!({
        "version": 1, "vault_salt": created.metadata.salt,
        "domain_state": null, "key_handle": [1], "ciphertext": [2], "stale": false,
    });
    store
        .db
        .put_desktop_wallet_vault_record(
            "biometric-unlock|vault-password",
            &rmp_serde::to_vec_named(&legacy).unwrap(),
        )
        .unwrap();
    assert_eq!(
        store.device_auth_status(DeviceAuthMethod::TouchId).unwrap(),
        DeviceAuthStatus::Enabled
    );
    assert_eq!(
        store
            .device_auth_status(DeviceAuthMethod::AppleWatch)
            .unwrap(),
        DeviceAuthStatus::Disabled
    );
    let mut record = store
        .device_auth_record(DeviceAuthMethod::TouchId)
        .unwrap()
        .unwrap();
    store
        .put_device_auth_record(DeviceAuthMethod::AppleWatch, &record)
        .unwrap();

    for method in DeviceAuthMethod::ALL {
        assert_eq!(
            store.device_auth_status(method).unwrap(),
            DeviceAuthStatus::Enabled
        );
        record.stale = true;
        store.put_device_auth_record(method, &record).unwrap();
        if !device_auth_available(method) {
            assert!(matches!(
                store.renew_device_auth(method, "test password"),
                Err(VaultError::DeviceAuth(
                    DeviceAuthError::Unavailable | DeviceAuthError::LockedOut
                ))
            ));
        }
        assert_eq!(
            store.device_auth_status(method).unwrap(),
            DeviceAuthStatus::NeedsReenrollment
        );
    }
    store
        .disable_device_auth(DeviceAuthMethod::AppleWatch)
        .unwrap();
    assert_eq!(
        store.device_auth_status(DeviceAuthMethod::TouchId).unwrap(),
        DeviceAuthStatus::NeedsReenrollment
    );
    assert_eq!(
        store
            .device_auth_status(DeviceAuthMethod::AppleWatch)
            .unwrap(),
        DeviceAuthStatus::Disabled
    );
    store
        .put_device_auth_record(DeviceAuthMethod::AppleWatch, &record)
        .unwrap();
    store
        .disable_device_auth(DeviceAuthMethod::TouchId)
        .unwrap();
    assert_eq!(
        store
            .device_auth_status(DeviceAuthMethod::AppleWatch)
            .unwrap(),
        DeviceAuthStatus::NeedsReenrollment
    );
    assert_eq!(
        store.device_auth_status(DeviceAuthMethod::TouchId).unwrap(),
        DeviceAuthStatus::Disabled
    );
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn device_auth_rejects_revoked_or_replaced_enrollment_after_approval() {
    let directory = std::env::temp_dir().join(format!(
        "railoxide-device-auth-pending-{}",
        rand::random::<u64>()
    ));
    let store = DesktopVaultStore::open(directory.clone()).unwrap();
    let created = store
        .create_vault_with_params("test password", KdfParams::new(1024, 1, 1))
        .unwrap();
    let record = StoredDeviceAuth {
        version: BIOMETRIC_UNLOCK_VERSION,
        vault_salt: created.metadata.salt,
        domain_state: None,
        key_handle: vec![1],
        ciphertext: vec![2],
        stale: false,
    };
    for method in DeviceAuthMethod::ALL {
        for replacement in [false, true] {
            store.put_device_auth_record(method, &record).unwrap();
            let result = store.open_device_auth_password(method, |_| {
                if replacement {
                    let mut next = store.device_auth_record(method).unwrap().unwrap();
                    next.key_handle = vec![3];
                    store.put_device_auth_record(method, &next).unwrap();
                } else {
                    store.disable_device_auth(method).unwrap();
                }
                Ok(pad_password("test password"))
            });
            assert!(matches!(result, Err(VaultError::DeviceAuthDisabled)));
        }
        store.put_device_auth_record(method, &record).unwrap();
        let result = store.open_device_auth_password(method, |_| {
            store.disable_device_auth(method).unwrap();
            Err(DeviceAuthError::Failed("key invalidated".into()))
        });
        assert!(result.is_err());
        assert_eq!(
            store.device_auth_status(method).unwrap(),
            DeviceAuthStatus::Disabled,
            "a late native failure must not restore a disabled enrollment"
        );
        store.put_device_auth_record(method, &record).unwrap();
        assert_eq!(
            store
                .open_device_auth_password(method, |_| Ok(pad_password("test password")))
                .unwrap()
                .as_str(),
            "test password"
        );
    }
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn padded_password_round_trips_and_hides_its_length() {
    for password in ["", "a", "correct horse battery staple", "пароль 密码 🔑"] {
        let padded = pad_password(password);
        assert_eq!(padded.len() % PASSWORD_PADDING_BLOCK, 0);
        assert_eq!(unpad_password(&padded).unwrap().as_str(), password);
    }
    assert_eq!(pad_password("a").len(), pad_password(&"a".repeat(60)).len());
    assert_eq!(
        pad_password(&"a".repeat(61)).len(),
        2 * PASSWORD_PADDING_BLOCK
    );
}

#[test]
fn corrupt_padding_is_rejected() {
    assert!(unpad_password(&[0, 0]).is_err());
    assert!(unpad_password(&[0, 0, 0, 9, b'a']).is_err());
    assert!(unpad_password(&[0, 0, 0, 1, 0xff]).is_err());
}
