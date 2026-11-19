#[allow(clippy::wildcard_imports)]
use super::*;

impl ExecutorStore {
    /// Persist what private sync's snapshot, scanned to `last_scanned`, shows about a
    /// record, in one write: see [`ExecutorRecord::apply_synced`]. It grants no current
    /// nonce or signing admission.
    pub(crate) fn record_synced(
        &self,
        operation: ExecutorOperationId,
        resolved: Option<ExecutorNonceWatermark>,
        last_scanned: u64,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.apply_synced(resolved, last_scanned);
            Ok(())
        })
    }

    /// [`Self::record_synced`] of a snapshot scanned to `last_scanned` that shows no payload
    /// executed: it settles the nonces resolved at a block sync has reached. UI tests use it,
    /// since their sessions never sync.
    #[cfg(feature = "test-support")]
    pub fn record_synced_for_tests(
        &self,
        operation: ExecutorOperationId,
        last_scanned: u64,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.record_synced(operation, None, last_scanned)
    }

    /// Persist a confirmed read of the account's execution nonce: the nonce observation
    /// and the watermark. Stored inclusions are left as they are and no longer consulted.
    /// The read is applied to the record as it is now. One taken at a block below a read
    /// or a resolution already recorded says nothing newer: it leaves the watermark alone,
    /// and stands as the nonce observation only while it agrees with the watermark, as a
    /// signing inspection at a slightly older block does.
    pub fn record_account_read(
        &self,
        operation: ExecutorOperationId,
        observation: ExecutorNonceObservation,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let watermark = record.nonce_watermark();
            let recorded = record
                .nonce_observation
                .map(|observed| observed.block.number)
                .max(watermark.map(|watermark| watermark.block));
            if recorded.is_none_or(|recorded| recorded <= observation.block.number) {
                record.nonce_observation = Some(observation);
                record.set_nonce_watermark(observation.into());
            } else if record.nonce_observation.is_none()
                && watermark.is_some_and(|watermark| watermark.nonce == observation.nonce)
            {
                record.nonce_observation = Some(observation);
            }
            Ok(())
        })
    }

    /// Require fresh evidence for a new reconciliation attempt or after an unavailable
    /// chain read. Restart alone retains the confirmed observation.
    pub fn invalidate_observation(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.require_reconciliation();
            Ok(())
        })
    }

    /// The user's explicit release of issued payloads' input reservations. The signed
    /// payloads can still execute; whichever transaction spends the notes first wins.
    /// Payloads the wallet issues or sends again afterwards reserve their inputs again.
    pub(crate) fn release_payloads(
        &self,
        operation: ExecutorOperationId,
        payloads: &[B256],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            for hash in payloads {
                if !record.issued.iter().any(|payload| payload.hash == *hash) {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
                if !record.released_payloads.contains(hash) {
                    record.released_payloads.push(*hash);
                }
            }
            Ok(())
        })
    }

    /// The wallet is sending a released payload again, so it reserves its inputs again.
    /// Refused while another operation reserves any of those inputs.
    pub(crate) fn reserve_released(
        &self,
        operation: ExecutorOperationId,
        payload: B256,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let inputs = record
                .issued
                .iter()
                .find(|issued| issued.hash == payload)
                .map(|issued| issued.context.inputs.clone())
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if !record.released_payloads.contains(&payload) {
                return Ok(());
            }
            // Same predicate as `record_issued`, under the same record mutation lock.
            if self.records()?.iter().any(|other| {
                other.operation != operation
                    && other
                        .reserved_inputs()
                        .iter()
                        .any(|input| inputs.contains(input))
            }) {
                return Err(ExecutorStoreError::InputReserved);
            }
            record.released_payloads.retain(|hash| *hash != payload);
            Ok(())
        })
    }

    /// A send by this wallet reserves the payload's inputs again, even when another
    /// operation selected them after the release.
    pub fn record_submission(
        &self,
        operation: ExecutorOperationId,
        payload_hash: B256,
        transaction_hash: B256,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let payload = record
                .issued
                .iter_mut()
                .find(|payload| payload.hash == payload_hash)
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if !payload.transaction_hashes.contains(&transaction_hash) {
                payload.transaction_hashes.push(transaction_hash);
            }
            record
                .released_payloads
                .retain(|hash| *hash != payload_hash);
            Ok(())
        })
    }
}
