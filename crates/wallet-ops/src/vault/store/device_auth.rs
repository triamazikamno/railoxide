//! Each authentication method has an independent, device-bound sealed password.
//! The existing Touch ID key and record format remain readable. Apple Watch
//! enrollments use a different database key, so enabling or revoking either
//! method cannot widen the other key's access policy.

use serde::{Deserialize, Serialize};

use super::{DesktopVaultStore, SALT_LEN, VaultError, Zeroizing};
use crate::device_auth::{self, DeviceAuthError, DeviceAuthMethod, SealedSecret};

const BIOMETRIC_UNLOCK_KEY: &str = "biometric-unlock|vault-password";
const BIOMETRIC_UNLOCK_VERSION: u32 = 1;
const PASSWORD_LENGTH_PREFIX: usize = 4;
/// Sealed passwords are padded so the ciphertext does not reveal their length.
const PASSWORD_PADDING_BLOCK: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceAuthStatus {
    /// This authentication method was never enabled or was explicitly disabled.
    Disabled,
    /// A current enrollment exists, regardless of temporary unavailability.
    Enabled,
    /// The sealed password no longer matches the vault or enrollment.
    /// The next verified password reseals it.
    NeedsReenrollment,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct StoredDeviceAuth {
    version: u32,
    vault_salt: [u8; SALT_LEN],
    domain_state: Option<Vec<u8>>,
    key_handle: Vec<u8>,
    ciphertext: Vec<u8>,
    stale: bool,
}

impl DesktopVaultStore {
    pub fn device_auth_status(
        &self,
        method: DeviceAuthMethod,
    ) -> Result<DeviceAuthStatus, VaultError> {
        let Some(record) = self.device_auth_record(method)? else {
            return Ok(DeviceAuthStatus::Disabled);
        };
        // Enrollment is a saved setting, independent of temporary macOS lockout.
        Ok(if self.device_auth_is_current(method, &record)? {
            DeviceAuthStatus::Enabled
        } else {
            DeviceAuthStatus::NeedsReenrollment
        })
    }

    /// Verifies the vault password and enables only the selected method.
    pub fn enable_device_auth(
        &self,
        method: DeviceAuthMethod,
        password: &str,
    ) -> Result<(), VaultError> {
        self.unlock_view(password)?;
        self.seal_device_auth(method, password)
    }

    pub fn disable_device_auth(&self, method: DeviceAuthMethod) -> Result<(), VaultError> {
        self.db
            .delete_desktop_wallet_vault_record(record_key(method))?;
        Ok(())
    }

    /// Reseals a stale record with a password the caller has already verified.
    /// Does nothing unless this method was explicitly enabled.
    pub fn renew_device_auth(
        &self,
        method: DeviceAuthMethod,
        password: &str,
    ) -> Result<(), VaultError> {
        if self.device_auth_status(method)? == DeviceAuthStatus::NeedsReenrollment {
            self.seal_device_auth(method, password)?;
        }
        Ok(())
    }

    /// Requests the selected method and returns the sealed vault password.
    ///
    /// Blocks until the prompt is answered. The password is not checked here:
    /// callers pass it to the same vault operations as a typed password.
    pub fn device_auth_vault_password(
        &self,
        method: DeviceAuthMethod,
        reason: &str,
    ) -> Result<Zeroizing<String>, VaultError> {
        self.open_device_auth_password(method, |sealed| {
            device_auth::open_secret(method, sealed, reason)
        })
    }

    fn open_device_auth_password(
        &self,
        method: DeviceAuthMethod,
        open: impl FnOnce(&SealedSecret) -> Result<Zeroizing<Vec<u8>>, DeviceAuthError>,
    ) -> Result<Zeroizing<String>, VaultError> {
        let Some(mut record) = self.device_auth_record(method)? else {
            return Err(VaultError::DeviceAuthDisabled);
        };
        if !self.device_auth_is_current(method, &record)? {
            return Err(VaultError::DeviceAuthDisabled);
        }
        let sealed = SealedSecret {
            key_handle: record.key_handle.clone(),
            ciphertext: record.ciphertext.clone(),
        };
        match open(&sealed) {
            Ok(padded) => {
                // Disabling, replacing, or resealing while the native prompt is
                // open revokes the pending request as well.
                if self.device_auth_record(method)?.as_ref() != Some(&record)
                    || !self.device_auth_is_current(method, &record)?
                {
                    return Err(VaultError::DeviceAuthDisabled);
                }
                unpad_password(&padded)
            }
            Err(DeviceAuthError::Failed(reason)) => {
                // The Secure Enclave key may be invalidated for good, so fall
                // back to the password once and reseal a fresh key after it.
                if self.device_auth_record(method)?.as_ref() == Some(&record) {
                    record.stale = true;
                    self.put_device_auth_record(method, &record)?;
                }
                Err(DeviceAuthError::Failed(reason).into())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Keeps each opted-in method working after a password change. A missing
    /// Watch must not prevent Touch ID renewal, or the password change itself.
    pub(super) fn reseal_device_auth_after_password_change(&self, new_password: &str) {
        for method in DeviceAuthMethod::ALL {
            match self.device_auth_record(method) {
                Ok(Some(_)) => {
                    if let Err(error) = self.seal_device_auth(method, new_password) {
                        // The old vault salt makes this enrollment stale until
                        // a later verified password can reseal it.
                        tracing::warn!(%error, ?method, "failed to reseal device authentication");
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, ?method, "failed to read device authentication enrollment");
                }
            }
        }
    }

    fn seal_device_auth(&self, method: DeviceAuthMethod, password: &str) -> Result<(), VaultError> {
        let metadata = self.metadata()?;
        let sealed = device_auth::seal_secret(method, &pad_password(password))?;
        self.put_device_auth_record(
            method,
            &StoredDeviceAuth {
                version: BIOMETRIC_UNLOCK_VERSION,
                vault_salt: metadata.salt,
                domain_state: device_auth::device_auth_domain_state(method),
                key_handle: sealed.key_handle,
                ciphertext: sealed.ciphertext,
                stale: false,
            },
        )
    }

    fn device_auth_is_current(
        &self,
        method: DeviceAuthMethod,
        record: &StoredDeviceAuth,
    ) -> Result<bool, VaultError> {
        if record.stale
            || record.version != BIOMETRIC_UNLOCK_VERSION
            || record.vault_salt != self.metadata()?.salt
        {
            return Ok(false);
        }
        // Skip a prompt after fingerprint or paired-Watch enrollment changed.
        // Temporary unavailability must not erase a saved enrollment.
        Ok(
            match (
                &record.domain_state,
                device_auth::device_auth_domain_state(method),
            ) {
                (Some(sealed), Some(current)) => *sealed == current,
                _ => true,
            },
        )
    }

    fn device_auth_record(
        &self,
        method: DeviceAuthMethod,
    ) -> Result<Option<StoredDeviceAuth>, VaultError> {
        self.db
            .get_desktop_wallet_vault_record(record_key(method))?
            .map(|data| rmp_serde::from_slice(&data).map_err(VaultError::from))
            .transpose()
    }

    fn put_device_auth_record(
        &self,
        method: DeviceAuthMethod,
        record: &StoredDeviceAuth,
    ) -> Result<(), VaultError> {
        let data = rmp_serde::to_vec_named(record)?;
        self.db
            .put_desktop_wallet_vault_record(record_key(method), &data)?;
        Ok(())
    }
}

const fn record_key(method: DeviceAuthMethod) -> &'static str {
    match method {
        // Persisted compatibility boundary: never rename the Touch ID key.
        DeviceAuthMethod::TouchId => BIOMETRIC_UNLOCK_KEY,
        DeviceAuthMethod::AppleWatch => "apple-watch-unlock|vault-password",
    }
}

fn pad_password(password: &str) -> Zeroizing<Vec<u8>> {
    let bytes = password.as_bytes();
    let padded_len = (PASSWORD_LENGTH_PREFIX + bytes.len()).div_ceil(PASSWORD_PADDING_BLOCK)
        * PASSWORD_PADDING_BLOCK;
    // Reserve the final size up front so no unzeroized reallocation is left behind.
    let mut padded = Zeroizing::new(Vec::with_capacity(padded_len));
    padded.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    padded.extend_from_slice(bytes);
    padded.resize(padded_len, 0);
    padded
}

fn unpad_password(padded: &[u8]) -> Result<Zeroizing<String>, VaultError> {
    let (length, rest) = padded
        .split_first_chunk::<PASSWORD_LENGTH_PREFIX>()
        .ok_or(VaultError::DeviceAuthCorrupt)?;
    let bytes = rest
        .get(..u32::from_be_bytes(*length) as usize)
        .ok_or(VaultError::DeviceAuthCorrupt)?;
    let password = std::str::from_utf8(bytes).map_err(|_| VaultError::DeviceAuthCorrupt)?;
    Ok(Zeroizing::new(password.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{
        BIOMETRIC_UNLOCK_VERSION, DesktopVaultStore, DeviceAuthStatus, PASSWORD_PADDING_BLOCK,
        StoredDeviceAuth, VaultError, pad_password, unpad_password,
    };
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
}
