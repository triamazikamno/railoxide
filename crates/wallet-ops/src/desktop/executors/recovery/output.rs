use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::{RelayAdapt7702, ShieldRequest, TokenData, shieldCall};
use eyre::{Result, eyre};
use railgun_wallet::{UtxoCommitmentKind, WalletUtxo};

use super::ExecutorOwner;
use crate::WalletSession;
use crate::desktop::executor_observation::expected_shields;
use crate::vault::{
    ExecutorOperationId, ExecutorPayloadInclusion, ExecutorPayloadPurpose, ExecutorPayloadStatus,
    ExecutorRecord, ExecutorRecoveryStepKind,
};
use crate::walletconnect::WrappedNative;

/// The original held amount still needing recovery, from verified record history only.
/// Same-block recoveries have no ordering evidence and leave the amount unchanged.
/// Newly wrapped native funds are excluded even when they shield the same ERC-20.
#[must_use]
pub fn executor_recovery_remaining_amount(
    record: &ExecutorRecord,
    token: Address,
    gross_amount: U256,
    held_block: u64,
    railgun: Address,
) -> U256 {
    let Some(source) = record.address() else {
        return gross_amount;
    };
    let token = TokenData::erc20(token);
    let mut seen = BTreeSet::new();
    let mut recovered = U256::ZERO;
    for payload in record
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Recovery)
    {
        let Some(inclusion) = payload.inclusion() else {
            continue;
        };
        if record.recorded_payload_status(payload.hash()) != Some(ExecutorPayloadStatus::Executed)
            || inclusion.block().number <= held_block
            || !seen.insert(inclusion.transaction_hash())
        {
            continue;
        }
        let data = payload.context().calldata();
        let calls = if let Ok(call) = RelayAdapt7702::executeCall::abi_decode(data) {
            if call._nonce != payload.nonce() || !call._actionData.requireSuccess {
                continue;
            }
            call._actionData.calls
        } else if let Ok(call) = RelayAdapt7702::multicallCall::abi_decode(data) {
            if call._nonce != payload.nonce() || !call._requireSuccess {
                continue;
            }
            call._calls
        } else {
            continue;
        };
        let Ok(requests) = expected_shields(source, railgun, &calls) else {
            continue;
        };
        let Some(wrapped) = calls
            .iter()
            .filter(|call| {
                call.to == token.tokenAddress
                    && call.data.starts_with(&WrappedNative::depositCall::SELECTOR)
            })
            .try_fold(U256::ZERO, |amount, call| {
                WrappedNative::depositCall::abi_decode(&call.data)
                    .ok()
                    .map(|_| amount.saturating_add(call.value))
            })
        else {
            continue;
        };
        recovered =
            recovered.saturating_add(shielded_amount(&requests, &token).saturating_sub(wrapped));
    }

    // Ordinary native recovery wraps and shields in separate transactions belonging
    // to the same reviewed recovery. Deduct the wrap once across that group's shields.
    let mut wrapped_by_recovery = BTreeMap::new();
    let mut seen_wraps = seen.clone();
    for transaction in record
        .recovery_transactions()
        .iter()
        .filter(|transaction| transaction.kind() == ExecutorRecoveryStepKind::Wrap)
    {
        if record.recorded_recovery_transaction_status(transaction.hash())
            != Some(ExecutorPayloadStatus::Executed)
            || !seen_wraps.insert(transaction.hash())
        {
            continue;
        }
        let request = transaction.transaction();
        let wrapped = wrapped_by_recovery
            .entry(transaction.recovery())
            .or_insert(Some(U256::ZERO));
        if request.from != Some(source) || request.to.as_ref().and_then(|to| to.to()).is_none() {
            *wrapped = None;
        } else if request.to.as_ref().and_then(|to| to.to()) == Some(&token.tokenAddress) {
            if request
                .input
                .input()
                .is_none_or(|input| WrappedNative::depositCall::abi_decode(input).is_err())
            {
                *wrapped = None;
            } else if let Some(amount) = wrapped {
                *amount = amount.saturating_add(request.value.unwrap_or_default());
            }
        }
    }
    for transaction in record
        .recovery_transactions()
        .iter()
        .filter(|transaction| transaction.kind() == ExecutorRecoveryStepKind::Shield)
    {
        let Some(inclusion) = transaction.inclusion() else {
            continue;
        };
        if record.recorded_recovery_transaction_status(transaction.hash())
            != Some(ExecutorPayloadStatus::Executed)
            || inclusion.block().number <= held_block
            || !seen.insert(inclusion.transaction_hash())
        {
            continue;
        }
        let request = transaction.transaction();
        if request.from != Some(source)
            || request.to.as_ref().and_then(|to| to.to()) != Some(&railgun)
        {
            continue;
        }
        let Some(input) = request.input.input() else {
            continue;
        };
        let Ok(call) = shieldCall::abi_decode(input) else {
            continue;
        };
        let mut amount = shielded_amount(&call._shieldRequests, &token);
        if let Some(wrapped) = wrapped_by_recovery.get_mut(&transaction.recovery()) {
            let Some(wrapped) = wrapped else {
                continue;
            };
            let newly_wrapped = amount.min(*wrapped);
            amount -= newly_wrapped;
            *wrapped -= newly_wrapped;
        }
        recovered = recovered.saturating_add(amount);
    }
    gross_amount.saturating_sub(recovered)
}

fn shielded_amount(requests: &[ShieldRequest], token: &TokenData) -> U256 {
    requests
        .iter()
        .filter(|request| {
            let shield_token = &request.preimage.token;
            shield_token.tokenType == token.tokenType
                && shield_token.tokenAddress == token.tokenAddress
                && shield_token.tokenSubID == token.tokenSubID
        })
        .fold(U256::ZERO, |amount, request| {
            amount.saturating_add(U256::from(request.preimage.value))
        })
}

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
    use crate::vault::{
        ExecutorExecutionResult, ExecutorNonceObservation, ExecutorPayloadContext,
        IssuedExecutorPayload, IssuedExecutorRecoveryTransaction,
    };
    use alloy::eips::BlockNumHash;
    use alloy::primitives::{Address, Bytes, Uint};
    use alloy::rpc::types::TransactionRequest;
    use broadcaster_core::contracts::railgun::{Call, CommitmentPreimage, ShieldCiphertext};
    use railgun_wallet::{Utxo, UtxoSource};

    fn recovery_shield(token: &TokenData, amount: u64) -> ShieldRequest {
        ShieldRequest {
            preimage: CommitmentPreimage {
                npk: B256::repeat_byte(1),
                token: token.clone(),
                value: Uint::from(amount),
            },
            ciphertext: ShieldCiphertext {
                encryptedBundle: [B256::ZERO; 3],
                shieldKey: B256::ZERO,
            },
        }
    }

    fn recovery_record(
        issued: Vec<serde_json::Value>,
        transactions: Vec<serde_json::Value>,
    ) -> ExecutorRecord {
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "derivation": "Railgun7702V1",
            "origin": "Discovered",
            "operation": ExecutorOperationId::random().unwrap(),
            "index": 1,
            "address": Address::repeat_byte(1),
            "delegate": Address::repeat_byte(2),
            "retired": true,
            "issued": serde_json::Value::Array(issued),
            "recovery_transactions": serde_json::Value::Array(transactions),
        }))
        .unwrap()
    }

    fn recovery_batch(nonce: u64, outer: u8, calls: Vec<Call>) -> serde_json::Value {
        let calldata = RelayAdapt7702::multicallCall {
            _requireSuccess: true,
            _calls: calls,
            _nonce: U256::from(nonce),
            _signature: Bytes::default(),
        }
        .abi_encode();
        let observed = BlockNumHash::new(10, B256::repeat_byte(10));
        let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
            U256::from(nonce),
            Address::repeat_byte(2),
            B256::repeat_byte(outer),
            ExecutorPayloadPurpose::Recovery,
            ExecutorPayloadContext::new(
                calldata.into(),
                ExecutorNonceObservation::new(observed, U256::from(nonce)),
                Vec::new(),
            ),
        ))
        .unwrap();
        payload["inclusion"] = serde_json::to_value(ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(outer),
            ExecutorExecutionResult::Executed,
        ))
        .unwrap();
        payload
    }

    fn ordinary_recovery(
        recovery: ExecutorOperationId,
        step: u32,
        outer: u8,
        kind: ExecutorRecoveryStepKind,
        call: Call,
    ) -> serde_json::Value {
        let request = TransactionRequest::default()
            .from(Address::repeat_byte(1))
            .to(call.to)
            .value(call.value)
            .input(call.data.into());
        let mut transaction = serde_json::to_value(IssuedExecutorRecoveryTransaction::new(
            recovery,
            step,
            kind,
            request,
            B256::repeat_byte(outer),
            BlockNumHash::new(10, B256::repeat_byte(10)),
        ))
        .unwrap();
        transaction["inclusion"] = serde_json::to_value(ExecutorPayloadInclusion::new(
            BlockNumHash::new(11, B256::repeat_byte(11)),
            B256::repeat_byte(outer),
            ExecutorExecutionResult::Executed,
        ))
        .unwrap();
        transaction
    }

    fn shield_call(to: Address, token: &TokenData, amount: u64) -> Call {
        Call {
            to,
            data: shieldCall {
                _shieldRequests: vec![recovery_shield(token, amount)],
            }
            .abi_encode()
            .into(),
            value: U256::ZERO,
        }
    }

    #[test]
    fn held_recovery_amount_accumulates_exact_asset_winners_and_reopens_after_reorg() {
        let railgun = Address::repeat_byte(3);
        let source = Address::repeat_byte(1);
        let token = TokenData::erc20(Address::repeat_byte(4));
        let recovery = ExecutorOperationId::random().unwrap();
        let ordinary = ordinary_recovery(
            recovery,
            0,
            20,
            ExecutorRecoveryStepKind::Shield,
            shield_call(railgun, &token, 10),
        );
        let partial = recovery_record(Vec::new(), vec![ordinary.clone()]);
        let remaining = |record: &ExecutorRecord| {
            executor_recovery_remaining_amount(
                record,
                token.tokenAddress,
                U256::from(100),
                10,
                railgun,
            )
        };
        assert_eq!(remaining(&partial), U256::from(90));

        let batch = recovery_batch(1, 21, vec![shield_call(source, &token, 90)]);
        let unrelated = recovery_batch(
            2,
            22,
            vec![shield_call(
                source,
                &TokenData::erc20(Address::repeat_byte(5)),
                100,
            )],
        );
        let mut pending = recovery_batch(3, 23, vec![shield_call(source, &token, 100)]);
        pending["inclusion"] = serde_json::Value::Null;
        let mut reverted = recovery_batch(4, 24, vec![shield_call(source, &token, 100)]);
        reverted["inclusion"]["result"] = serde_json::json!("Reverted");
        let mut same_block = recovery_batch(5, 25, vec![shield_call(source, &token, 100)]);
        same_block["inclusion"]["block"]["number"] = serde_json::json!(10);
        let ignored = vec![unrelated, pending, reverted, same_block];
        assert_eq!(
            remaining(&recovery_record(ignored.clone(), vec![ordinary.clone()])),
            U256::from(90)
        );
        let mut completed = ignored.clone();
        completed.push(batch.clone());
        assert_eq!(
            remaining(&recovery_record(completed, vec![ordinary.clone()])),
            U256::ZERO
        );
        let mut reorged = batch;
        reorged["inclusion"] = serde_json::Value::Null;
        let mut evidence = ignored;
        evidence.push(reorged);
        assert_eq!(
            remaining(&recovery_record(evidence, vec![ordinary])),
            U256::from(90)
        );
    }

    #[test]
    fn held_weth_recovery_excludes_new_wraps_and_counts_each_outer_transaction_once() {
        let railgun = Address::repeat_byte(3);
        let source = Address::repeat_byte(1);
        let token = TokenData::erc20(Address::repeat_byte(4));
        let wrap = |amount| Call {
            to: token.tokenAddress,
            data: WrappedNative::depositCall {}.abi_encode().into(),
            value: U256::from(amount),
        };
        let batch = recovery_batch(1, 20, vec![wrap(80_u64), shield_call(source, &token, 90)]);
        let recovery = ExecutorOperationId::random().unwrap();
        let ordinary = vec![
            ordinary_recovery(
                recovery,
                0,
                21,
                ExecutorRecoveryStepKind::Wrap,
                wrap(30_u64),
            ),
            ordinary_recovery(
                recovery,
                1,
                22,
                ExecutorRecoveryStepKind::Shield,
                shield_call(railgun, &token, 40),
            ),
        ];
        let remaining = |record: &ExecutorRecord| {
            executor_recovery_remaining_amount(
                record,
                token.tokenAddress,
                U256::from(100),
                10,
                railgun,
            )
        };
        assert_eq!(
            remaining(&recovery_record(vec![batch.clone()], ordinary.clone())),
            U256::from(80)
        );
        let mut duplicate = ordinary;
        duplicate.push(ordinary_recovery(
            ExecutorOperationId::random().unwrap(),
            0,
            20,
            ExecutorRecoveryStepKind::Shield,
            shield_call(railgun, &token, 90),
        ));
        assert_eq!(
            remaining(&recovery_record(vec![batch], duplicate)),
            U256::from(80)
        );
    }

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
