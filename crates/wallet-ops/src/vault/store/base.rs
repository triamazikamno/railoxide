use super::{
    Arc, CreatedVault, DbConfig, DbStore, DesktopVaultStore, KdfParams, PathBuf, SpendGrant,
    VAULT_METADATA_KEY, VaultError, VaultMetadata, ViewUnlock, create_spend_grant,
    create_with_params, current_vault_version, reencrypt_metadata, unlock_view,
};

const DESKTOP_DB_CACHE_SIZE_BYTES: usize = 128 * 1024 * 1024;

impl DesktopVaultStore {
    pub fn open(db_path: PathBuf) -> Result<Self, VaultError> {
        let db = DbStore::open_with_cache_size_bytes(
            DbConfig { root_dir: db_path },
            DESKTOP_DB_CACHE_SIZE_BYTES,
        )?;
        Ok(Self { db: Arc::new(db) })
    }

    #[must_use]
    pub const fn from_db(db: Arc<DbStore>) -> Self {
        Self { db }
    }

    #[must_use]
    pub fn db(&self) -> Arc<DbStore> {
        Arc::clone(&self.db)
    }

    pub fn create_vault(&self, password: &str) -> Result<CreatedVault, VaultError> {
        self.create_vault_with_params(password, KdfParams::default())
    }

    pub fn create_vault_with_params(
        &self,
        password: &str,
        kdf: KdfParams,
    ) -> Result<CreatedVault, VaultError> {
        let created = create_with_params(password, kdf)?;
        let data = rmp_serde::to_vec_named(&created.metadata)?;
        if !self
            .db
            .put_desktop_wallet_vault_record_if_absent(VAULT_METADATA_KEY, &data)?
        {
            return Err(VaultError::VaultAlreadyExists);
        }
        Ok(created)
    }

    pub fn metadata(&self) -> Result<VaultMetadata, VaultError> {
        let data = self
            .db
            .get_desktop_wallet_vault_record(VAULT_METADATA_KEY)?
            .ok_or(VaultError::VaultNotFound)?;
        Ok(rmp_serde::from_slice(&data)?)
    }

    pub fn vault_exists(&self) -> Result<bool, VaultError> {
        Ok(self
            .db
            .get_desktop_wallet_vault_record(VAULT_METADATA_KEY)?
            .is_some())
    }

    pub fn put_metadata(&self, metadata: &VaultMetadata) -> Result<(), VaultError> {
        let data = rmp_serde::to_vec_named(metadata)?;
        self.db
            .put_desktop_wallet_vault_record(VAULT_METADATA_KEY, &data)?;
        Ok(())
    }

    pub fn unlock_view(&self, password: &str) -> Result<ViewUnlock, VaultError> {
        let mut metadata = self.metadata()?;
        let view = unlock_view(&metadata, password)?;
        self.upgrade_vault_metadata_version_if_legacy(&mut metadata)?;
        Ok(view)
    }

    pub fn create_spend_grant(&self, password: &str) -> Result<SpendGrant, VaultError> {
        let mut metadata = self.metadata()?;
        let grant = create_spend_grant(&metadata, password)?;
        self.upgrade_vault_metadata_version_if_legacy(&mut metadata)?;
        Ok(grant)
    }

    pub fn reencrypt_vault(
        &self,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), VaultError> {
        let metadata = self.metadata()?;
        let metadata = reencrypt_metadata(&metadata, current_password, new_password)?;
        self.put_metadata(&metadata)?;
        self.reseal_device_auth_after_password_change(new_password);
        Ok(())
    }

    fn upgrade_vault_metadata_version_if_legacy(
        &self,
        metadata: &mut VaultMetadata,
    ) -> Result<(), VaultError> {
        let current_version = current_vault_version();
        if metadata.version == current_version {
            return Ok(());
        }
        metadata.version = current_version;
        self.put_metadata(metadata)
    }
}
