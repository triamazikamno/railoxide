use alloy::primitives::{B256, U256};
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::{RelayAdapt7702, ShieldRequest, shieldCall};
use eyre::{Result, eyre};
use railgun_wallet::{UtxoCommitmentKind, WalletUtxo};

use super::ExecutorOwner;
use crate::WalletSession;
use crate::desktop::executor_observation::expected_shields;
use crate::vault::{
    ExecutorOperationId, ExecutorPayloadInclusion, ExecutorPayloadPurpose, ExecutorPayloadStatus,
    ExecutorRecoveryStepKind,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorRecoveryOutputId {
    OrdinaryTransaction(B256),
    ExecutionPayload(B256),
}

/// Current observation only, recomputed from the canonical record and private actor.
/// Receiving the shield does not assert that its POIs are ready for another spend.
pub struct ExecutorRecoveryOutputStatus {
    pub id: ExecutorRecoveryOutputId,
    pub execution: ExecutorPayloadStatus,
    pub transaction_hash: Option<B256>,
    pub expected_outputs: usize,
    /// None while the private snapshot is unavailable or execution is unconfirmed.
    pub observed_private_outputs: Option<usize>,
}

impl ExecutorRecoveryOutputStatus {
    #[must_use]
    pub fn is_received(&self) -> bool {
        self.execution == ExecutorPayloadStatus::Executed
            && self.expected_outputs != 0
            && self.observed_private_outputs == Some(self.expected_outputs)
    }
}

impl ExecutorOwner {
    /// Local-only progress. No RPC query or private projection mutation is performed.
    pub fn recovery_output_statuses(
        &self,
        session: &WalletSession,
        operation: ExecutorOperationId,
    ) -> Result<Vec<ExecutorRecoveryOutputStatus>> {
        self.ensure_active()?;
        if session
            .executor_owner
            .as_ref()
            .is_none_or(|owner| !std::ptr::eq(owner.as_ref(), self))
        {
            return Err(eyre!(
                "recovery outputs belong to a different wallet session"
            ));
        }
        // History display can use verified inclusions without a fresh signing
        // nonce. Receipt matching still requires the actor's current projection.
        let record = self
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
            .ok_or_else(|| eyre!("historical executor record is unavailable"))?;
        let source = record
            .address()
            .ok_or_else(|| eyre!("executor address is unavailable"))?;
        let railgun = self.chain.require_railgun()?.deployment.contract;
        let snapshot = session.handle.current_snapshot();
        let utxos = snapshot.as_ref().map(|snapshot| snapshot.utxos.as_ref());
        let mut statuses = Vec::new();
        for transaction in record
            .recovery_transactions()
            .iter()
            .filter(|transaction| transaction.kind() == ExecutorRecoveryStepKind::Shield)
        {
            let input = transaction
                .transaction()
                .input
                .input()
                .ok_or_else(|| eyre!("retained shield call is unavailable"))?;
            let requests = shieldCall::abi_decode(input)?._shieldRequests;
            statuses.push(output_status(
                ExecutorRecoveryOutputId::OrdinaryTransaction(transaction.hash()),
                record
                    .recovery_transaction_status(transaction.hash())
                    .unwrap_or(ExecutorPayloadStatus::Uncertain),
                transaction.inclusion(),
                &requests,
                utxos,
            ));
        }
        for payload in record
            .issued()
            .iter()
            .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Recovery)
        {
            let calls = if let Ok(call) =
                RelayAdapt7702::executeCall::abi_decode(payload.context().calldata())
            {
                call._actionData.calls
            } else {
                RelayAdapt7702::multicallCall::abi_decode(payload.context().calldata())?._calls
            };
            let requests = expected_shields(source, railgun, &calls)?;
            statuses.push(output_status(
                ExecutorRecoveryOutputId::ExecutionPayload(payload.hash()),
                record
                    .recorded_payload_status(payload.hash())
                    .unwrap_or(ExecutorPayloadStatus::Uncertain),
                payload.inclusion(),
                &requests,
                utxos,
            ));
        }
        Ok(statuses)
    }
}

fn output_status(
    id: ExecutorRecoveryOutputId,
    execution: ExecutorPayloadStatus,
    inclusion: Option<ExecutorPayloadInclusion>,
    requests: &[ShieldRequest],
    utxos: Option<&[WalletUtxo]>,
) -> ExecutorRecoveryOutputStatus {
    let observed_private_outputs = if execution == ExecutorPayloadStatus::Executed {
        inclusion.zip(utxos).map(|(inclusion, utxos)| {
            let mut matched = std::collections::BTreeSet::new();
            requests
                .iter()
                .filter(|request| {
                    let token = request.preimage.token.id();
                    utxos
                        .iter()
                        .enumerate()
                        .find(|(index, wallet_utxo)| {
                            let utxo = &wallet_utxo.utxo;
                            // The canonical receipt verified gross = net + protocol fee.
                            // Include a matching shield note even if it was spent later.
                            !matched.contains(index)
                                && utxo.poi.commitment_kind == UtxoCommitmentKind::Shield
                                && utxo.source.tx_hash == inclusion.transaction_hash()
                                && utxo.source.block_number == inclusion.block().number
                                && utxo.note.npk == U256::from_be_bytes(request.preimage.npk.0)
                                && utxo.note.token_hash == token
                                && utxo.note.value <= U256::from(request.preimage.value)
                        })
                        .is_some_and(|(index, _)| matched.insert(index))
                })
                .count()
        })
    } else {
        None
    };
    ExecutorRecoveryOutputStatus {
        id,
        execution,
        transaction_hash: inclusion.map(ExecutorPayloadInclusion::transaction_hash),
        expected_outputs: requests.len(),
        observed_private_outputs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::ExecutorExecutionResult;
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{Address, Uint};
    use broadcaster_core::contracts::railgun::{CommitmentPreimage, ShieldCiphertext, TokenData};
    use railgun_wallet::{Utxo, UtxoSource};

    #[test]
    fn executor_recovery_output_waits_for_matching_private_shield_and_reopens_after_reorg() {
        let request = ShieldRequest {
            preimage: CommitmentPreimage {
                npk: B256::repeat_byte(1),
                token: TokenData::erc20(Address::repeat_byte(2)),
                value: Uint::from(100),
            },
            ciphertext: ShieldCiphertext {
                encryptedBundle: [B256::ZERO; 3],
                shieldKey: B256::ZERO,
            },
        };
        let hash = B256::repeat_byte(3);
        let id = ExecutorRecoveryOutputId::ExecutionPayload(B256::repeat_byte(4));
        let inclusion = Some(ExecutorPayloadInclusion::new(
            BlockNumHash::new(10, B256::repeat_byte(10)),
            hash,
            ExecutorExecutionResult::Executed,
        ));
        let mut note = request.preimage.note_with_random([0; 16]);
        note.value = U256::from(99);
        let mut output = WalletUtxo::new(Utxo::new(
            note,
            0,
            0,
            UtxoSource {
                tx_hash: hash,
                block_number: 10,
                block_timestamp: 0,
            },
            UtxoCommitmentKind::Shield,
        ));
        let requests = [request];
        let status = |execution, outputs: Option<&[WalletUtxo]>| {
            output_status(id, execution, inclusion, &requests, outputs)
        };
        assert!(!status(ExecutorPayloadStatus::Executed, None).is_received());
        assert!(!status(ExecutorPayloadStatus::Executed, Some(&[])).is_received());
        output.utxo.source.tx_hash = B256::repeat_byte(5);
        assert!(
            !status(
                ExecutorPayloadStatus::Executed,
                Some(std::slice::from_ref(&output))
            )
            .is_received()
        );
        output.utxo.source.tx_hash = hash;
        output.utxo.source.block_number = 9;
        assert!(
            !status(
                ExecutorPayloadStatus::Executed,
                Some(std::slice::from_ref(&output))
            )
            .is_received()
        );
        output.utxo.source.block_number = 10;
        output.spent = Some(UtxoSource {
            tx_hash: B256::repeat_byte(6),
            block_number: 11,
            block_timestamp: 0,
        });
        assert!(
            status(
                ExecutorPayloadStatus::Executed,
                Some(std::slice::from_ref(&output))
            )
            .is_received()
        );
        assert!(
            !status(
                ExecutorPayloadStatus::MissingEffects,
                Some(std::slice::from_ref(&output))
            )
            .is_received()
        );
        assert!(
            !status(
                ExecutorPayloadStatus::Uncertain,
                Some(std::slice::from_ref(&output))
            )
            .is_received()
        );
        output.utxo.note.npk += U256::ONE;
        assert!(
            !status(
                ExecutorPayloadStatus::Executed,
                Some(std::slice::from_ref(&output))
            )
            .is_received()
        );
    }
}
