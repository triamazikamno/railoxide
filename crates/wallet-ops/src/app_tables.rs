use local_db::{
    AppTable, AppTableWriter, DbError, DbStore, WalletPrivateNamespaceDeletionReport,
    WalletPrivateNamespaceId, WalletPrivateStateBatch,
};
use serde::{Deserialize, Serialize};

// Persisted table names; existing databases depend on these exact strings.
pub const DESKTOP_WALLET_VAULT_TABLE: AppTable = AppTable::new("desktop_wallet_vault_v1");
pub const APP_SETTINGS_TABLE: AppTable = AppTable::new("app_settings_v1");

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesktopWalletVaultRecord {
    pub key: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSettingsRecord {
    pub key: String,
    pub payload: Vec<u8>,
}

/// Typed inputs for permanently deleting a wallet's private and vault state.
pub struct WalletDeletionBatch<'a> {
    pub private_namespaces: &'a [WalletPrivateNamespaceId],
    pub desktop_wallet_vault_delete_keys: &'a [String],
    pub desktop_wallet_vault_put_records: &'a [(String, Vec<u8>)],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalletDeletionReport {
    pub private_namespace_rows: WalletPrivateNamespaceDeletionReport,
    pub desktop_wallet_vault_rows_deleted: u64,
    pub desktop_wallet_vault_rows_put: u64,
}

pub trait AppSettingsDbExt {
    fn get_app_settings_record(&self, key: &str) -> Result<Option<Vec<u8>>, DbError>;
    fn put_app_settings_record(&self, key: &str, payload: &[u8]) -> Result<(), DbError>;
    fn delete_app_settings_record(&self, key: &str) -> Result<(), DbError>;
    fn list_app_settings_records(&self, prefix: &str) -> Result<Vec<AppSettingsRecord>, DbError>;
}

impl AppSettingsDbExt for DbStore {
    fn get_app_settings_record(&self, key: &str) -> Result<Option<Vec<u8>>, DbError> {
        self.read_app_tables(|reader| reader.get(APP_SETTINGS_TABLE, key))
    }

    fn put_app_settings_record(&self, key: &str, payload: &[u8]) -> Result<(), DbError> {
        self.write_app_tables(|writer| writer.insert(APP_SETTINGS_TABLE, key, payload))
    }

    fn delete_app_settings_record(&self, key: &str) -> Result<(), DbError> {
        self.write_app_tables(|writer| {
            writer.remove(APP_SETTINGS_TABLE, key)?;
            Ok(())
        })
    }

    fn list_app_settings_records(&self, prefix: &str) -> Result<Vec<AppSettingsRecord>, DbError> {
        list_records(self, APP_SETTINGS_TABLE, prefix, |key, payload| {
            AppSettingsRecord { key, payload }
        })
    }
}

pub trait DesktopWalletVaultDbExt {
    fn get_desktop_wallet_vault_record(&self, key: &str) -> Result<Option<Vec<u8>>, DbError>;
    fn put_desktop_wallet_vault_record(&self, key: &str, payload: &[u8]) -> Result<(), DbError>;
    fn put_desktop_wallet_vault_record_if_absent(
        &self,
        key: &str,
        payload: &[u8],
    ) -> Result<bool, DbError>;
    fn put_desktop_wallet_vault_records(
        &self,
        records: &[(String, Vec<u8>)],
    ) -> Result<(), DbError>;
    fn update_desktop_wallet_vault_records(
        &self,
        delete_keys: &[String],
        put_records: &[(String, Vec<u8>)],
    ) -> Result<(), DbError>;
    fn delete_desktop_wallet_vault_record(&self, key: &str) -> Result<(), DbError>;
    fn replace_desktop_wallet_vault_prefix_with_records(
        &self,
        prefix: &str,
        records: &[(String, Vec<u8>)],
    ) -> Result<(), DbError>;
    fn list_desktop_wallet_vault_records(
        &self,
        prefix: &str,
    ) -> Result<Vec<DesktopWalletVaultRecord>, DbError>;
    fn batch_commit_wallet_private_state_with_vault_records(
        &self,
        batch: &WalletPrivateStateBatch<'_>,
        vault_records: &[DesktopWalletVaultRecord],
    ) -> Result<(), DbError>;
    /// Applies all namespace and desktop-vault mutations in one write transaction.
    fn delete_wallet(
        &self,
        batch: &WalletDeletionBatch<'_>,
    ) -> Result<WalletDeletionReport, DbError>;
}

impl DesktopWalletVaultDbExt for DbStore {
    fn get_desktop_wallet_vault_record(&self, key: &str) -> Result<Option<Vec<u8>>, DbError> {
        self.read_app_tables(|reader| reader.get(DESKTOP_WALLET_VAULT_TABLE, key))
    }

    fn put_desktop_wallet_vault_record(&self, key: &str, payload: &[u8]) -> Result<(), DbError> {
        self.write_app_tables(|writer| writer.insert(DESKTOP_WALLET_VAULT_TABLE, key, payload))
    }

    fn put_desktop_wallet_vault_record_if_absent(
        &self,
        key: &str,
        payload: &[u8],
    ) -> Result<bool, DbError> {
        self.write_app_tables(|writer| {
            if writer.get(DESKTOP_WALLET_VAULT_TABLE, key)?.is_some() {
                return Ok(false);
            }
            writer.insert(DESKTOP_WALLET_VAULT_TABLE, key, payload)?;
            Ok(true)
        })
    }

    fn put_desktop_wallet_vault_records(
        &self,
        records: &[(String, Vec<u8>)],
    ) -> Result<(), DbError> {
        self.update_desktop_wallet_vault_records(&[], records)
    }

    fn update_desktop_wallet_vault_records(
        &self,
        delete_keys: &[String],
        put_records: &[(String, Vec<u8>)],
    ) -> Result<(), DbError> {
        self.write_app_tables(|writer| update_vault_rows(writer, delete_keys, put_records))
    }

    fn delete_desktop_wallet_vault_record(&self, key: &str) -> Result<(), DbError> {
        self.write_app_tables(|writer| {
            writer.remove(DESKTOP_WALLET_VAULT_TABLE, key)?;
            Ok(())
        })
    }

    fn replace_desktop_wallet_vault_prefix_with_records(
        &self,
        prefix: &str,
        records: &[(String, Vec<u8>)],
    ) -> Result<(), DbError> {
        self.write_app_tables(|writer| {
            let stale_keys: Vec<String> = writer
                .list_prefix(DESKTOP_WALLET_VAULT_TABLE, prefix)?
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            update_vault_rows(writer, &stale_keys, records)
        })
    }

    fn list_desktop_wallet_vault_records(
        &self,
        prefix: &str,
    ) -> Result<Vec<DesktopWalletVaultRecord>, DbError> {
        list_records(self, DESKTOP_WALLET_VAULT_TABLE, prefix, |key, payload| {
            DesktopWalletVaultRecord { key, payload }
        })
    }

    fn batch_commit_wallet_private_state_with_vault_records(
        &self,
        batch: &WalletPrivateStateBatch<'_>,
        vault_records: &[DesktopWalletVaultRecord],
    ) -> Result<(), DbError> {
        self.batch_commit_wallet_private_state_with(batch, |writer| {
            for record in vault_records {
                writer.insert(DESKTOP_WALLET_VAULT_TABLE, &record.key, &record.payload)?;
            }
            Ok(())
        })
    }

    fn delete_wallet(
        &self,
        batch: &WalletDeletionBatch<'_>,
    ) -> Result<WalletDeletionReport, DbError> {
        let (private_namespace_rows, (deleted, put)) =
            self.delete_wallet_namespaces_with(batch.private_namespaces, |writer| {
                let mut deleted = 0_u64;
                for key in batch.desktop_wallet_vault_delete_keys {
                    if writer.remove(DESKTOP_WALLET_VAULT_TABLE, key)? {
                        deleted = deleted.saturating_add(1);
                    }
                }
                let mut put = 0_u64;
                for (key, payload) in batch.desktop_wallet_vault_put_records {
                    writer.insert(DESKTOP_WALLET_VAULT_TABLE, key, payload)?;
                    put = put.saturating_add(1);
                }
                Ok::<_, DbError>((deleted, put))
            })?;
        Ok(WalletDeletionReport {
            private_namespace_rows,
            desktop_wallet_vault_rows_deleted: deleted,
            desktop_wallet_vault_rows_put: put,
        })
    }
}

fn list_records<R>(
    db: &DbStore,
    table: AppTable,
    prefix: &str,
    record: fn(String, Vec<u8>) -> R,
) -> Result<Vec<R>, DbError> {
    let rows = db.read_app_tables(|reader| reader.list_prefix(table, prefix))?;
    Ok(rows
        .into_iter()
        .map(|(key, payload)| record(key, payload))
        .collect())
}

fn update_vault_rows(
    writer: &mut AppTableWriter<'_>,
    delete_keys: &[String],
    put_records: &[(String, Vec<u8>)],
) -> Result<(), DbError> {
    for key in delete_keys {
        writer.remove(DESKTOP_WALLET_VAULT_TABLE, key)?;
    }
    for (key, payload) in put_records {
        writer.insert(DESKTOP_WALLET_VAULT_TABLE, key, payload)?;
    }
    Ok(())
}
