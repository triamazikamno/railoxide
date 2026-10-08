use super::super::*;
use super::helpers::*;
use crate::device_auth::{DeviceAuthMethod, device_auth_available};
use std::fs;

#[test]
fn device_auth_is_disabled_until_enabled_and_rejects_wrong_passwords() {
    for method in DeviceAuthMethod::ALL {
        let (root_dir, _db, store) = desktop_store_with_vault();

        assert_eq!(
            store.device_auth_status(method).expect("status"),
            DeviceAuthStatus::Disabled
        );
        assert!(matches!(
            store.device_auth_vault_password(method, "test"),
            Err(VaultError::DeviceAuthDisabled)
        ));
        assert!(matches!(
            store.enable_device_auth(method, "wrong password"),
            Err(VaultError::UnlockFailed)
        ));
        // Renewing never opts into either method.
        store
            .renew_device_auth(method, TEST_PASSWORD)
            .expect("renew");
        assert_eq!(
            store.device_auth_status(method).expect("status"),
            DeviceAuthStatus::Disabled
        );

        fs::remove_dir_all(root_dir).expect("cleanup");
    }
}

/// Sealing uses the real Secure Enclave but never prompts; only opening does.
#[test]
fn device_auth_survives_password_changes_until_disabled() {
    for method in DeviceAuthMethod::ALL {
        let (root_dir, _db, store) = desktop_store_with_vault();
        let result = store.enable_device_auth(method, TEST_PASSWORD);
        if !device_auth_available(method) {
            assert!(matches!(
                result,
                Err(VaultError::DeviceAuth(
                    crate::device_auth::DeviceAuthError::Unavailable
                        | crate::device_auth::DeviceAuthError::LockedOut
                ))
            ));
            fs::remove_dir_all(root_dir).expect("cleanup");
            continue;
        }
        result.expect("enable device authentication");
        assert_eq!(
            store.device_auth_status(method).expect("status"),
            DeviceAuthStatus::Enabled
        );

        store
            .reencrypt_vault(TEST_PASSWORD, "new password")
            .expect("change password");
        assert_eq!(
            store.device_auth_status(method).expect("status"),
            DeviceAuthStatus::Enabled,
            "a password change reseals the new password"
        );

        let created = create_with_params("replaced vault", test_kdf()).expect("create vault");
        store
            .put_metadata(&created.metadata)
            .expect("replace vault");
        assert_eq!(
            store.device_auth_status(method).expect("status"),
            DeviceAuthStatus::NeedsReenrollment,
            "a replaced vault invalidates the sealed password"
        );
        store
            .renew_device_auth(method, "replaced vault")
            .expect("renew");
        assert_eq!(
            store.device_auth_status(method).expect("status"),
            DeviceAuthStatus::Enabled
        );

        store.disable_device_auth(method).expect("disable");
        assert_eq!(
            store.device_auth_status(method).expect("status"),
            DeviceAuthStatus::Disabled
        );

        fs::remove_dir_all(root_dir).expect("cleanup");
    }
}
