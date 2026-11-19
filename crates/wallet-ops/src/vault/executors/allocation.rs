use super::*;

impl ExecutorStore {
    /// Retained history and explicit discovery can only advance the allocation floor.
    /// This does not establish complete discovery or eligibility of the next address.
    pub fn raise_floor(&self, next_index: u32) -> Result<(), ExecutorStoreError> {
        if next_index > INDEX_LIMIT {
            return Err(ExecutorStoreError::Exhausted);
        }
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut allocation = self.allocation()?;
        allocation.next_index = allocation.next_index.max(next_index);
        let mut updates = Vec::new();
        if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.index < next_index)
        {
            self.retain_spare(&mut allocation, &mut updates)?;
        }
        updates.push(self.seal(
            RecordKind::ExecutorAllocation,
            self.allocation_key(),
            &allocation,
        )?);
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(())
    }

    /// Persists the reservation and advances the floor in the same database transaction.
    /// The caller derives and checks the reserved account before using its address.
    pub fn reserve(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        purpose_summary: Option<&str>,
        assets: &[ExecutorAsset],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.reserve_with_swap_approval(operation, delegate, purpose_summary, assets, None, None)
    }

    /// [`Self::reserve`] that stores `swap_approval` and the link to the swap's destination
    /// stealth account in the write that creates the record, so a new swap's record never
    /// exists without its approved terms. An existing record with the same link is returned
    /// unchanged, with its own approval.
    pub fn reserve_with_swap_approval(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        purpose_summary: Option<&str>,
        assets: &[ExecutorAsset],
        swap_approval: Option<SwapApproval>,
        destination_operation: Option<ExecutorOperationId>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.reserve_record(
            operation,
            delegate,
            purpose_summary,
            assets,
            ReservationLinks {
                swap_approval,
                destination_operation,
                swap_destination: None,
            },
        )
    }

    /// [`Self::reserve`] for a private Bridge swap's destination stealth account on this chain.
    /// The destination token is the account's recoverable asset. An existing record is returned
    /// unchanged only if it serves the same origin swap and token.
    pub fn reserve_swap_destination(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        destination: SwapDestinationRecord,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        if destination.origin_chain == self.chain_id {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        self.reserve_record(
            operation,
            delegate,
            Some(SWAP_DESTINATION_PURPOSE_SUMMARY),
            &[ExecutorAsset::Erc20(destination.destination_token)],
            ReservationLinks {
                swap_approval: None,
                destination_operation: None,
                swap_destination: Some(SwapDestinationRecord {
                    outcome: None,
                    ..destination
                }),
            },
        )
    }

    fn reserve_record(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        purpose_summary: Option<&str>,
        assets: &[ExecutorAsset],
        links: ReservationLinks,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let ReservationLinks {
            swap_approval,
            destination_operation,
            swap_destination,
        } = links;
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        if let Some(existing) = self.record(operation)? {
            // A destination record's outcome changes after its reservation.
            let existing_destination =
                existing
                    .swap_destination()
                    .map(|destination| SwapDestinationRecord {
                        outcome: None,
                        ..destination
                    });
            if existing.origin != ExecutorRecordOrigin::Reserved
                || existing.destination_operation() != destination_operation
                || existing_destination != swap_destination
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            if existing.delegate == delegate {
                return Ok(existing);
            }
            let mut existing = existing;
            self.retarget(&mut existing, delegate)?;
            self.vault.db.put_desktop_wallet_vault_records(&[self.seal(
                RecordKind::ExecutorOperation,
                self.operation_key(operation),
                &existing,
            )?])?;
            return Ok(existing);
        }
        // A swap's reservation is its account's first use. Both accounts of a private Bridge
        // swap take the origin's identity for it.
        let first_use = if let Some(destination) = swap_destination {
            Some((
                SwapUseId::first(destination.origin_operation),
                SwapUseRole::Destination {
                    origin_chain: destination.origin_chain,
                    origin_operation: destination.origin_operation,
                    destination_token: destination.destination_token,
                    shields: Vec::new(),
                    outcome: None,
                },
            ))
        } else if swap_approval.is_some() || destination_operation.is_some() {
            Some((
                SwapUseId::first(operation),
                SwapUseRole::Source {
                    approval: swap_approval.map(Box::new),
                    destination_operation,
                },
            ))
        } else {
            None
        };
        let mut updates = Vec::new();
        let record = self.allocate_record(
            operation,
            delegate,
            purpose_summary,
            assets,
            first_use,
            &mut updates,
        )?;
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(record)
    }

    /// Move `operation`'s account to `delegate`, a listed delegate of its record's capability,
    /// before an authorization targets it. Equal delegates leave the record unchanged.
    pub fn retarget_delegate(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| self.retarget(record, delegate))
    }

    fn retarget(
        &self,
        record: &mut ExecutorRecord,
        delegate: Address,
    ) -> Result<(), ExecutorStoreError> {
        if !self.interchangeable(record.delegate, delegate) {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        record.delegate = delegate;
        Ok(())
    }

    /// An account on `stored` keeps its nonce and payloads on `delegate`.
    fn interchangeable(&self, stored: Address, delegate: Address) -> bool {
        stored == delegate
            || crate::settings::ExecutorProfile::accepted(self.chain_id, stored)
                .and_then(|profile| profile.interchangeable(delegate))
                .is_some()
    }

    /// Allocate a fresh account for `operation`, which has no record yet, and append its
    /// allocation row and record to `updates` without writing them. The caller holds the record
    /// lock and commits `updates` in one write.
    pub(super) fn allocate_record(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        purpose_summary: Option<&str>,
        assets: &[ExecutorAsset],
        first_use: Option<(SwapUseId, SwapUseRole)>,
        updates: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let mut allocation = self.allocation()?;
        if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.delegate != delegate)
        {
            self.retain_spare(&mut allocation, updates)?;
        }
        let (index, address) = if let Some(spare) = allocation.spare.take() {
            (spare.index, spare.address)
        } else {
            let index = ordinary_index(allocation.next_index)?;
            allocation.next_index = index + 1;
            (index, None)
        };
        let mut record = ExecutorRecord {
            version: LEGACY_VERSION,
            derivation: ExecutorDerivationScheme::Railgun7702V1,
            origin: ExecutorRecordOrigin::Reserved,
            operation,
            index,
            address,
            delegate,
            retired: false,
            created_at: local_timestamp(),
            restored_at: None,
            purpose_summary: purpose_summary.map(str::to_owned),
            assets: assets.to_vec(),
            hidden: false,
            use_check: ExecutorUseCheck::default(),
            issued: Vec::new(),
            nonce_observation: None,
            nonce_watermark: None,
            settled_nonce: None,
            public_account_uuid: None,
            swap: None,
            swap_setup_stopped: false,
            released_payloads: Vec::new(),
            swap_uses: Vec::new(),
            active_swap_use: None,
        };
        if let Some((id, role)) = first_use {
            record.begin_first_swap_use(id, role);
        }
        record.version = record.format_version();
        updates.extend([
            self.seal(
                RecordKind::ExecutorAllocation,
                self.allocation_key(),
                &allocation,
            )?,
            self.seal(
                RecordKind::ExecutorOperation,
                self.operation_key(operation),
                &record,
            )?,
        ]);
        Ok(record)
    }

    /// Retain an explicitly derived historical account without assigning it to a new action.
    /// The native owner must derive the address under spend authorization first.
    pub fn restore_index(
        &self,
        index: u32,
        address: Address,
        delegate: Address,
        assets: &[ExecutorAsset],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let mut records = self.restore_indices(&[(index, address)], delegate, assets)?;
        records.pop().ok_or(ExecutorStoreError::InvalidRecord)
    }

    /// Restore each `(index, address)` in order under one lock, as repeated `restore_index`
    /// calls would. The first failure is returned; entries before it stay written.
    pub fn restore_indices(
        &self,
        entries: &[(u32, Address)],
        delegate: Address,
        assets: &[ExecutorAsset],
    ) -> Result<Vec<ExecutorRecord>, ExecutorStoreError> {
        // An out-of-range first entry fails before any lock, read or repair write.
        match entries.first() {
            None => return Ok(Vec::new()),
            Some(&(index, _)) if index >= INDEX_LIMIT => return Err(ExecutorStoreError::Exhausted),
            Some(_) => {}
        }
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        // Load once: each entry's writes are mirrored in memory instead of decrypted again.
        let allocation = self.stored_allocation()?;
        let had_spare = allocation.spare.is_some();
        let mut records = self.records()?;
        let mut allocation = self.reconcile_allocation(allocation, &records)?;
        let mut restored = Vec::with_capacity(entries.len());
        // Reconciliation may have retained the spare as a record that `records` lacks.
        let mut stale = had_spare && allocation.spare.is_none();
        for &(index, address) in entries {
            if index >= INDEX_LIMIT {
                return Err(ExecutorStoreError::Exhausted);
            }
            if std::mem::take(&mut stale) {
                // A displaced spare was written as a record that `records` does not hold.
                records = self.records()?;
            }
            let record =
                if let Some(existing) = records.iter_mut().find(|record| record.index == index) {
                    self.restore_existing(existing, address, delegate, assets)?
                } else {
                    stale = allocation
                        .spare
                        .as_ref()
                        .is_some_and(|spare| spare.index < index);
                    let record =
                        self.restore_new(&mut allocation, (index, address), delegate, assets)?;
                    records.push(record.clone());
                    record
                };
            restored.push(record);
        }
        Ok(restored)
    }

    /// Update `existing` in place and write it.
    fn restore_existing(
        &self,
        existing: &mut ExecutorRecord,
        address: Address,
        delegate: Address,
        assets: &[ExecutorAsset],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        // Restoration authorizes nothing, so an existing record keeps its delegate.
        if existing.address.is_some_and(|known| known != address)
            || !self.interchangeable(existing.delegate, delegate)
        {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        // A reserved account may have survived a crash before address derivation.
        existing.address = Some(address);
        if existing.restored_at.is_none() {
            existing.restored_at = local_timestamp();
        }
        for asset in assets {
            if !existing.assets.contains(asset) {
                existing.assets.push(*asset);
            }
        }
        self.vault.db.put_desktop_wallet_vault_records(&[self.seal(
            RecordKind::ExecutorOperation,
            self.operation_key(existing.operation),
            &*existing,
        )?])?;
        Ok(existing.clone())
    }

    /// Record an unrecorded index and write it with `allocation`, which then equals the stored row.
    fn restore_new(
        &self,
        allocation: &mut Allocation,
        (index, address): (u32, Address),
        mut delegate: Address,
        assets: &[ExecutorAsset],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        allocation.next_index = allocation.next_index.max(index + 1);
        let mut updates = Vec::new();
        if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.index == index)
        {
            let spare = allocation
                .spare
                .take()
                .ok_or(ExecutorStoreError::InvalidRecord)?;
            if spare.address.is_some_and(|known| known != address)
                || !self.interchangeable(spare.delegate, delegate)
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            delegate = spare.delegate;
        } else if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.index < index)
        {
            self.retain_spare(allocation, &mut updates)?;
        }
        let record = ExecutorRecord {
            version: LEGACY_VERSION,
            derivation: ExecutorDerivationScheme::Railgun7702V1,
            origin: ExecutorRecordOrigin::Discovered,
            operation: ExecutorOperationId::random()?,
            index,
            address: Some(address),
            delegate,
            retired: true,
            created_at: None,
            restored_at: local_timestamp(),
            purpose_summary: None,
            assets: assets.to_vec(),
            hidden: false,
            use_check: ExecutorUseCheck::default(),
            issued: Vec::new(),
            nonce_observation: None,
            nonce_watermark: None,
            settled_nonce: None,
            public_account_uuid: None,
            swap: None,
            swap_setup_stopped: false,
            released_payloads: Vec::new(),
            swap_uses: Vec::new(),
            active_swap_use: None,
        };
        updates.extend([
            self.seal(
                RecordKind::ExecutorAllocation,
                self.allocation_key(),
                &allocation,
            )?,
            self.seal(
                RecordKind::ExecutorOperation,
                self.operation_key(record.operation),
                &record,
            )?,
        ]);
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(record)
    }
}
