use super::{
    Address, EXECUTOR_RECORD_LOCK, ExecutorOperationId, ExecutorStore, ExecutorStoreError,
    RecordKind, VaultError,
};
use crate::vault::{
    ExecutorPublicAccountSource, PublicAccountMetadata, PublicAccountScope, PublicAccountSource,
    PublicAccountStatus, generate_opaque_id, next_public_account_display_order,
    public_account_metadata_record_entry,
};

impl ExecutorStore {
    /// The native owner holds activity admission and verifies the derived address
    /// before atomically linking both encrypted records.
    pub(crate) fn register_public_account(
        &self,
        operation: ExecutorOperationId,
        address: Address,
    ) -> Result<PublicAccountMetadata, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut record = self
            .record(operation)?
            .ok_or(ExecutorStoreError::OperationMismatch)?;
        if record.address != Some(address) {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        let source = PublicAccountSource::ExecutorDerived(ExecutorPublicAccountSource {
            chain_id: self.chain_id,
            index: record.index,
            operation,
        });
        let scope = PublicAccountScope::PrivateWallet {
            wallet_uuid: self.view.wallet_id().to_owned(),
        };
        let accounts = self
            .vault
            .list_public_account_metadata_with_view(&self.view.view)?;
        let mut matching = None;
        for account in &accounts {
            if account.address == address {
                if account.source != source || account.scope != scope || matching.is_some() {
                    return Err(VaultError::DuplicatePublicAccountAddress.into());
                }
                matching = Some(account.clone());
            }
        }
        let mut account = if let Some(account) = matching {
            account
        } else {
            if record.public_account_uuid.is_some() {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            PublicAccountMetadata {
                public_account_uuid: generate_opaque_id()?,
                address,
                label: Some(format!("Stealth account {}", record.index)),
                source,
                scope,
                derivation_index: None,
                hardware_descriptor: None,
                status: PublicAccountStatus::Active,
                display_order: next_public_account_display_order(&accounts)?,
            }
        };
        if record
            .public_account_uuid
            .as_ref()
            .is_some_and(|id| id != &account.public_account_uuid)
        {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        account.status = PublicAccountStatus::Active;
        record.public_account_uuid = Some(account.public_account_uuid.clone());
        record.retired = true;
        self.vault.db.put_desktop_wallet_vault_records(&[
            public_account_metadata_record_entry(&self.view.view, &account)?,
            self.seal(
                RecordKind::ExecutorOperation,
                self.operation_key(operation),
                &record,
            )?,
        ])?;
        Ok(account)
    }
}
