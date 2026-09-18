use alloy::rpc::types::TransactionRequest;

use super::{
    B256, BlockNumHash, Deserialize, ExecutorExecutionResult, ExecutorOperationId,
    ExecutorPayloadInclusion, ExecutorPayloadStatus, ExecutorRecord, ExecutorStore,
    ExecutorStoreError, Serialize,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutorRecoveryStepKind {
    Wrap,
    ApproveErc20,
    ApproveErc721,
    Shield,
}

/// Ordinary EOA recovery has no contract execution nonce. Its signed outer identity
/// and expected call remain encrypted independently of delegated execution payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedExecutorRecoveryTransaction {
    recovery: ExecutorOperationId,
    step: u32,
    kind: ExecutorRecoveryStepKind,
    transaction: TransactionRequest,
    hash: B256,
    observed: BlockNumHash,
    #[serde(default)]
    remaining_gas_limit: u64,
    inclusion: Option<ExecutorPayloadInclusion>,
}

impl IssuedExecutorRecoveryTransaction {
    pub(crate) const fn new(
        recovery: ExecutorOperationId,
        step: u32,
        kind: ExecutorRecoveryStepKind,
        transaction: TransactionRequest,
        hash: B256,
        observed: BlockNumHash,
    ) -> Self {
        let remaining_gas_limit = match transaction.gas {
            Some(gas) => gas,
            None => 0,
        };
        Self {
            recovery,
            step,
            kind,
            transaction,
            hash,
            observed,
            remaining_gas_limit,
            inclusion: None,
        }
    }
    pub(crate) const fn with_remaining_gas_limit(mut self, remaining_gas_limit: u64) -> Self {
        self.remaining_gas_limit = remaining_gas_limit;
        self
    }
    #[must_use]
    pub const fn remaining_gas_limit(&self) -> u64 {
        self.remaining_gas_limit
    }
    #[must_use]
    pub const fn recovery(&self) -> ExecutorOperationId {
        self.recovery
    }
    #[must_use]
    pub const fn step(&self) -> u32 {
        self.step
    }
    #[must_use]
    pub const fn kind(&self) -> ExecutorRecoveryStepKind {
        self.kind
    }
    #[must_use]
    pub const fn transaction(&self) -> &TransactionRequest {
        &self.transaction
    }
    #[must_use]
    pub const fn hash(&self) -> B256 {
        self.hash
    }
    #[must_use]
    pub const fn observed(&self) -> BlockNumHash {
        self.observed
    }
    #[must_use]
    pub const fn inclusion(&self) -> Option<ExecutorPayloadInclusion> {
        self.inclusion
    }
}

impl ExecutorRecord {
    #[must_use]
    pub const fn recovery_observation(&self) -> Option<BlockNumHash> {
        self.recovery_observation
    }

    #[must_use]
    pub fn recovery_transactions(&self) -> &[IssuedExecutorRecoveryTransaction] {
        &self.recovery_transactions
    }

    #[must_use]
    pub fn recovery_transaction_status(&self, hash: B256) -> Option<ExecutorPayloadStatus> {
        let recorded = self.recorded_recovery_transaction_status(hash)?;
        if self.recovery_observation.is_none() {
            return Some(ExecutorPayloadStatus::Uncertain);
        }
        if let ExecutorPayloadStatus::Invalidated { winner } = recorded
            && self.nonce_observation.is_none()
            && self.issued.iter().any(|payload| {
                payload
                    .inclusion
                    .is_some_and(|inclusion| inclusion.transaction_hash == winner)
            })
        {
            return Some(ExecutorPayloadStatus::Uncertain);
        }
        Some(recorded)
    }

    /// Last recorded outcome for history display, without current reconciliation.
    /// This does not establish current signing or retry eligibility.
    #[must_use]
    pub fn recorded_recovery_transaction_status(
        &self,
        hash: B256,
    ) -> Option<ExecutorPayloadStatus> {
        let transaction = self
            .recovery_transactions
            .iter()
            .find(|transaction| transaction.hash == hash)?;
        // A canonical ordinary receipt consumes its Ethereum account nonce even
        // when the call reverts. Contract execution payloads have separate rules.
        if let Some(winner) = self.recovery_transactions.iter().find(|other| {
            other.hash != hash
                && transaction.transaction.nonce.is_some()
                && other.transaction.nonce == transaction.transaction.nonce
                && other.inclusion.is_some()
        }) {
            return Some(ExecutorPayloadStatus::Invalidated {
                winner: winner.hash,
            });
        }
        // An atomic batch sent by this account can replace a retained ordinary
        // attempt. A broadcaster's sender nonce must never supersede this account's.
        if let Some(winner) = self
            .issued
            .iter()
            .filter_map(|payload| payload.inclusion)
            .find(|inclusion| {
                transaction.transaction.nonce.is_some()
                    && inclusion.executor_account_nonce == transaction.transaction.nonce
            })
        {
            return Some(ExecutorPayloadStatus::Invalidated {
                winner: winner.transaction_hash,
            });
        }
        Some(
            match transaction.inclusion.map(ExecutorPayloadInclusion::result) {
                Some(ExecutorExecutionResult::Executed) => ExecutorPayloadStatus::Executed,
                Some(ExecutorExecutionResult::Reverted) => ExecutorPayloadStatus::Reverted,
                Some(ExecutorExecutionResult::MissingEffects) => {
                    ExecutorPayloadStatus::MissingEffects
                }
                None => ExecutorPayloadStatus::Uncertain,
            },
        )
    }
}

impl ExecutorStore {
    pub(crate) fn record_recovery_transaction(
        &self,
        operation: ExecutorOperationId,
        issued: IssuedExecutorRecoveryTransaction,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let transaction = &issued.transaction;
            if record.address.is_none()
                || transaction.from != record.address
                || transaction.chain_id != Some(self.chain_id)
                || transaction.to.is_none()
                || transaction.nonce.is_none()
                || transaction.gas.is_none()
                || transaction.max_fee_per_gas.is_none()
                || transaction.max_priority_fee_per_gas.is_none()
                || transaction.authorization_list.is_some()
                || issued.remaining_gas_limit < transaction.gas.unwrap_or_default()
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            // An ordinary transaction cannot invalidate an outstanding execution signature.
            if !record.issued.is_empty()
                && record.nonce_observation.is_none_or(|observation| {
                    record
                        .issued
                        .iter()
                        .any(|payload| payload.nonce >= observation.nonce)
                })
            {
                return Err(ExecutorStoreError::OutstandingNonce);
            }
            for previous in &record.recovery_transactions {
                if previous.hash == issued.hash {
                    return if previous.transaction == issued.transaction
                        && previous.recovery == issued.recovery
                        && previous.step == issued.step
                        && previous.kind == issued.kind
                        && previous.remaining_gas_limit == issued.remaining_gas_limit
                    {
                        Ok(())
                    } else {
                        Err(ExecutorStoreError::OperationMismatch)
                    };
                }
                if previous.recovery == issued.recovery && previous.step == issued.step {
                    let prior = &previous.transaction;
                    if prior.nonce != transaction.nonce
                        || prior.to != transaction.to
                        || prior.value != transaction.value
                        || prior.input != transaction.input
                        || previous.kind != issued.kind
                    {
                        return Err(ExecutorStoreError::OperationMismatch);
                    }
                }
            }
            record.retired = true;
            record.recovery_transactions.push(issued);
            Ok(())
        })
    }

    pub(crate) fn reconcile_recovery(
        &self,
        operation: ExecutorOperationId,
        block: BlockNumHash,
        inclusions: &[(B256, ExecutorPayloadInclusion)],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            for transaction in &mut record.recovery_transactions {
                transaction.inclusion = None;
            }
            let mut seen = std::collections::BTreeSet::new();
            let mut nonces = std::collections::BTreeSet::new();
            for (hash, inclusion) in inclusions {
                let transaction = record
                    .recovery_transactions
                    .iter_mut()
                    .find(|transaction| transaction.hash == *hash)
                    .ok_or(ExecutorStoreError::OperationMismatch)?;
                if !seen.insert(*hash)
                    || inclusion.transaction_hash != *hash
                    || inclusion.block.number > block.number
                    || !nonces.insert(
                        transaction
                            .transaction
                            .nonce
                            .ok_or(ExecutorStoreError::InvalidRecord)?,
                    )
                {
                    return Err(ExecutorStoreError::InvalidRecord);
                }
                transaction.inclusion = Some(*inclusion);
            }
            record.recovery_observation = Some(block);
            Ok(())
        })
    }
}
