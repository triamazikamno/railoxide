//! Each authentication method has an independent, device-bound sealed password.
//! The existing Touch ID key and record format remain readable. Apple Watch
//! enrollments use a different database key, so enabling or revoking either
//! method cannot widen the other key's access policy.

use serde::{Deserialize, Serialize};

use super::{DesktopVaultStore, SALT_LEN, VaultError, Zeroizing};
use crate::DesktopWalletVaultDbExt as _;
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
mod tests;
