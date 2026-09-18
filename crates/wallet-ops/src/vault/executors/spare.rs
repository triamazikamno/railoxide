use alloy::primitives::Address;
use serde::{Deserialize, Serialize};

use super::{
    Allocation, EXECUTOR_RECORD_LOCK, ExecutorDerivationScheme, ExecutorOperationId,
    ExecutorRecord, ExecutorRecordOrigin, ExecutorStore, ExecutorStoreError, ExecutorUseCheck,
    RecordKind, VERSION, ordinary_index,
};

/// A durably allocated account that has not yet been claimed by an operation.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ExecutorSpare {
    pub(super) derivation: ExecutorDerivationScheme,
    pub(super) index: u32,
    pub(super) address: Option<Address>,
    pub(super) delegate: Address,
}

impl ExecutorSpare {
    pub(crate) const fn index(&self) -> u32 {
        self.index
    }

    pub(crate) const fn address(&self) -> Option<Address> {
        self.address
    }
}

impl ExecutorStore {
    #[cfg(test)]
    pub(crate) fn spare(&self) -> Result<Option<ExecutorSpare>, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        Ok(self.allocation()?.spare)
    }

    /// Reserve before derivation or RPC exposure. Repeated replenishment keeps one spare.
    pub(crate) fn reserve_spare(
        &self,
        delegate: Address,
    ) -> Result<ExecutorSpare, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut allocation = self.allocation()?;
        if let Some(spare) = &allocation.spare
            && spare.delegate == delegate
        {
            return Ok(spare.clone());
        }
        let mut updates = Vec::new();
        self.retain_spare(&mut allocation, &mut updates)?;
        let index = ordinary_index(allocation.next_index)?;
        allocation.next_index = index + 1;
        let spare = ExecutorSpare {
            derivation: ExecutorDerivationScheme::Railgun7702V1,
            index,
            address: None,
            delegate,
        };
        allocation.spare = Some(spare.clone());
        updates.push(self.seal(
            RecordKind::ExecutorAllocation,
            self.allocation_key(),
            &allocation,
        )?);
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(spare)
    }

    pub(crate) fn bind_spare_address(
        &self,
        index: u32,
        address: Address,
    ) -> Result<(), ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut allocation = self.allocation()?;
        let spare = allocation
            .spare
            .as_mut()
            .filter(|spare| spare.index == index)
            .ok_or(ExecutorStoreError::OperationMismatch)?;
        if spare.address.is_some_and(|known| known != address) {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        spare.address = Some(address);
        self.vault.db.put_desktop_wallet_vault_records(&[self.seal(
            RecordKind::ExecutorAllocation,
            self.allocation_key(),
            &allocation,
        )?])?;
        Ok(())
    }

    // Retain displaced spares for recovery in the same write as the new floor/profile.
    pub(super) fn retain_spare(
        &self,
        allocation: &mut Allocation,
        updates: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<(), ExecutorStoreError> {
        let Some(spare) = allocation.spare.take() else {
            return Ok(());
        };
        let record = ExecutorRecord {
            version: VERSION,
            derivation: spare.derivation,
            origin: ExecutorRecordOrigin::Discovered,
            operation: ExecutorOperationId::random()?,
            index: spare.index,
            address: spare.address,
            delegate: spare.delegate,
            retired: true,
            created_at: None,
            restored_at: None,
            purpose_summary: None,
            assets: Vec::new(),
            hidden: false,
            use_check: ExecutorUseCheck::default(),
            issued: Vec::new(),
            nonce_observation: None,
            recovery_transactions: Vec::new(),
            recovery_observation: None,
            public_account_uuid: None,
        };
        updates.push(self.seal(
            RecordKind::ExecutorOperation,
            self.operation_key(record.operation),
            &record,
        )?);
        Ok(())
    }
}
