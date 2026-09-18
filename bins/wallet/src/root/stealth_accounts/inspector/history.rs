use std::collections::BTreeMap;

use alloy::eips::BlockNumHash;
use wallet_ops::{
    PublicActionProgressStep, executor_payload_recovery_steps,
    vault::{
        ExecutorExecutionResult, ExecutorPayloadPurpose, ExecutorRecord, ExecutorRecoveryStepKind,
        IssuedExecutorPayload,
    },
};

pub(super) fn payload_rows(record: &ExecutorRecord) -> Vec<&IssuedExecutorPayload> {
    let mut nonces = BTreeMap::new();
    for payload in record.issued() {
        let rank = |payload: &IssuedExecutorPayload| {
            payload.inclusion().map_or((false, false, 0), |inclusion| {
                (
                    inclusion.result() == ExecutorExecutionResult::Executed,
                    true,
                    inclusion.block().number,
                )
            })
        };
        let current = nonces.entry(payload.nonce()).or_insert(payload);
        // Successful execution consumes the nonce. Otherwise prefer the latest
        // mined attempt, or the most recently signed payload if none was mined.
        if rank(payload) >= rank(current) {
            *current = payload;
        }
    }
    nonces.into_values().collect()
}

pub(super) fn payload_purpose(payload: &IssuedExecutorPayload) -> String {
    if payload.purpose() == ExecutorPayloadPurpose::Operation {
        return "Operation".into();
    }
    let steps = executor_payload_recovery_steps(payload);
    let mut label = "Recovery".to_owned();
    for step in steps {
        label.push_str(match step {
            PublicActionProgressStep::Wrap => " · wrap",
            PublicActionProgressStep::Approve => " · approve",
            PublicActionProgressStep::Shield => " · shield",
            _ => continue,
        });
    }
    label
}

pub(super) fn block_label(number: u64) -> String {
    let number = number.to_string();
    let mut label = String::new();
    for (index, digit) in number.chars().enumerate() {
        if index > 0 && (number.len() - index).is_multiple_of(3) {
            label.push(',');
        }
        label.push(digit);
    }
    label
}

pub(super) fn status_as_of(record: &ExecutorRecord, rechecked: Option<BlockNumHash>) -> String {
    let block = rechecked.map(|block| block.number).or_else(|| {
        record
            .issued()
            .iter()
            .filter_map(IssuedExecutorPayload::inclusion)
            .chain(
                record
                    .recovery_transactions()
                    .iter()
                    .filter_map(wallet_ops::vault::IssuedExecutorRecoveryTransaction::inclusion),
            )
            .map(|inclusion| inclusion.block().number)
            .max()
    });
    let mut label = block.map_or_else(
        || "Status not yet confirmed".into(),
        |block| format!("Status as of #{}", block_label(block)),
    );
    if rechecked.is_none() {
        label.push_str(" · not rechecked since restart");
    }
    label
}

pub(super) fn recorded_outcomes(record: &ExecutorRecord) -> Vec<String> {
    let mut outcomes = Vec::new();
    for purpose in [
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadPurpose::Recovery,
    ] {
        for result in [
            ExecutorExecutionResult::Executed,
            ExecutorExecutionResult::Reverted,
            ExecutorExecutionResult::MissingEffects,
        ] {
            let mut transactions = BTreeMap::new();
            for payload in record
                .issued()
                .iter()
                .filter(|payload| payload.purpose() == purpose)
            {
                if let Some(inclusion) = payload
                    .inclusion()
                    .filter(|inclusion| inclusion.result() == result)
                {
                    let shield = purpose == ExecutorPayloadPurpose::Recovery
                        && executor_payload_recovery_steps(payload)
                            .contains(&PublicActionProgressStep::Shield);
                    transactions.insert(
                        inclusion.transaction_hash(),
                        (inclusion.block().number, shield),
                    );
                }
            }
            if purpose == ExecutorPayloadPurpose::Recovery {
                for tx in record.recovery_transactions() {
                    if let Some(inclusion) = tx
                        .inclusion()
                        .filter(|inclusion| inclusion.result() == result)
                    {
                        transactions.insert(
                            inclusion.transaction_hash(),
                            (
                                inclusion.block().number,
                                tx.kind() == ExecutorRecoveryStepKind::Shield,
                            ),
                        );
                    }
                }
            }
            let Some(last) = transactions.values().map(|(block, _)| *block).max() else {
                continue;
            };
            let recovered = transactions
                .values()
                .any(|(block, shield)| *block == last && *shield);
            let label = match (purpose, result) {
                (ExecutorPayloadPurpose::Operation, ExecutorExecutionResult::Executed) => {
                    "Executed"
                }
                (ExecutorPayloadPurpose::Operation, ExecutorExecutionResult::Reverted) => {
                    "Operation reverted"
                }
                (ExecutorPayloadPurpose::Operation, ExecutorExecutionResult::MissingEffects) => {
                    "Operation effects missing"
                }
                (ExecutorPayloadPurpose::Recovery, ExecutorExecutionResult::Executed)
                    if recovered =>
                {
                    "Leftover balance recovered to your private balance"
                }
                (ExecutorPayloadPurpose::Recovery, ExecutorExecutionResult::Executed) => {
                    "Recovery steps executed"
                }
                (ExecutorPayloadPurpose::Recovery, ExecutorExecutionResult::Reverted) => {
                    "Recovery reverted"
                }
                (ExecutorPayloadPurpose::Recovery, ExecutorExecutionResult::MissingEffects) => {
                    "Recovery effects missing"
                }
            };
            outcomes.push(if transactions.len() == 1 {
                format!("{label} in block #{}.", block_label(last))
            } else {
                format!(
                    "{label} in {} transactions, the last in block #{}.",
                    transactions.len(),
                    block_label(last)
                )
            });
        }
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, Bytes, U256};
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorOperationId, ExecutorPayloadContext,
        ExecutorPayloadInclusion,
    };

    fn payload(nonce: u64, hash: u8, result: Option<ExecutorExecutionResult>) -> serde_json::Value {
        let block = BlockNumHash::new(25_990_899, B256::repeat_byte(10));
        let mut value = serde_json::to_value(IssuedExecutorPayload::new(
            U256::from(nonce),
            Address::ZERO,
            B256::repeat_byte(hash),
            ExecutorPayloadPurpose::Operation,
            ExecutorPayloadContext::new(
                Bytes::new(),
                ExecutorNonceObservation::new(block, U256::ZERO),
                Vec::new(),
            ),
        ))
        .unwrap();
        value["inclusion"] =
            serde_json::to_value(result.map(|result| {
                ExecutorPayloadInclusion::new(block, B256::repeat_byte(hash), result)
            }))
            .unwrap();
        value
    }

    fn record(payloads: &[serde_json::Value], recovery: &[serde_json::Value]) -> ExecutorRecord {
        serde_json::from_value(serde_json::json!({
            "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
            "operation": ExecutorOperationId::random().unwrap(), "index": 0,
            "address": Address::ZERO, "delegate": Address::ZERO, "retired": true,
            "issued": payloads, "recovery_transactions": recovery,
        }))
        .unwrap()
    }

    #[test]
    fn nonce_rows_prefer_execution_then_latest_mined_or_signed_attempt() {
        use ExecutorExecutionResult::{Executed, Reverted};
        let record = record(
            &[
                payload(0, 1, None),
                payload(0, 2, Some(Reverted)),
                payload(0, 3, Some(Executed)),
                payload(0, 4, None),
                payload(1, 5, Some(Reverted)),
                payload(1, 6, None),
                payload(2, 7, None),
                payload(2, 8, None),
            ],
            &[],
        );
        let hashes = payload_rows(&record)
            .into_iter()
            .map(IssuedExecutorPayload::hash)
            .collect::<Vec<_>>();
        assert_eq!(hashes, [3, 5, 8].map(B256::repeat_byte));
    }

    #[test]
    fn recovery_summary_requires_a_completed_shield_and_counts_transactions_once() {
        use ExecutorExecutionResult::Executed;
        let recovery = ExecutorOperationId::random().unwrap();
        let steps = [
            ExecutorRecoveryStepKind::Wrap,
            ExecutorRecoveryStepKind::ApproveErc20,
            ExecutorRecoveryStepKind::Shield,
        ];
        let mut transactions = Vec::new();
        for (step, kind) in steps.into_iter().enumerate() {
            let block = BlockNumHash::new(25_999_879 + step as u64, B256::repeat_byte(20));
            let hash = B256::repeat_byte(20 + step as u8);
            transactions.push(serde_json::json!({
                "recovery": recovery, "step": step, "kind": kind, "hash": hash,
                "observed": block, "transaction": { "nonce": format!("0x{step:x}") },
                "inclusion": ExecutorPayloadInclusion::new(block, hash, Executed),
            }));
        }
        let partial = record(&[payload(0, 1, Some(Executed))], &transactions[..2]);
        assert!(
            !recorded_outcomes(&partial)
                .iter()
                .any(|outcome| outcome.contains("recovered to"))
        );
        let complete = record(
            &[payload(0, 1, Some(Executed)), payload(0, 2, None)],
            &transactions,
        );
        let outcomes = recorded_outcomes(&complete);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes[0].contains("25,990,899"));
        assert!(outcomes[1].contains("recovered to your private balance in 3 transactions"));
        assert!(outcomes[1].contains("25,999,881"));
    }
}
