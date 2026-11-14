use super::{DesktopVaultStore, EncryptedRecord, VaultError};
use crate::DesktopWalletVaultDbExt as _;

impl DesktopVaultStore {
    pub(super) fn encrypted_record(&self, key: &str) -> Result<EncryptedRecord, VaultError> {
        let data = self
            .db
            .get_desktop_wallet_vault_record(key)?
            .ok_or(VaultError::VaultNotFound)?;
        Ok(rmp_serde::from_slice(&data)?)
    }

    pub(super) fn encrypted_record_optional(
        &self,
        key: &str,
    ) -> Result<Option<EncryptedRecord>, VaultError> {
        self.db
            .get_desktop_wallet_vault_record(key)?
            .map(|data| rmp_serde::from_slice(&data).map_err(VaultError::from))
            .transpose()
    }
}
