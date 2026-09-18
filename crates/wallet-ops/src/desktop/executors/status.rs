//! Local presentation of retained history. This never supplies signing admission.
use std::collections::{BTreeMap, BTreeSet};

use alloy::eips::BlockNumHash;
use alloy::primitives::B256;
use eyre::{Result, eyre};

use super::ExecutorOwner;
use crate::vault::{
    ExecutorExecutionResult, ExecutorPayloadPurpose, ExecutorRecord, ExecutorRecordOrigin,
    ExecutorRecoveryStepKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutorAccountOutcome {
    NotSigned,
    HistoryUnknown,
    Unconfirmed,
    Executed,
    Reverted,
    MissingEffects,
    RecoveryPending,
    RecoveryConfirmed,
}

#[derive(Clone, Copy, Debug)]
pub struct ExecutorAccountStatus {
    outcome: ExecutorAccountOutcome,
    rechecked: Option<BlockNumHash>,
    needs_attention: bool,
    unresolved: bool,
}

impl Default for ExecutorAccountStatus {
    fn default() -> Self {
        Self {
            outcome: ExecutorAccountOutcome::HistoryUnknown,
            rechecked: None,
            needs_attention: true,
            unresolved: true,
        }
    }
}

impl ExecutorAccountStatus {
    #[must_use]
    pub const fn outcome(self) -> ExecutorAccountOutcome {
        self.outcome
    }
    #[must_use]
    pub const fn rechecked(self) -> Option<BlockNumHash> {
        self.rechecked
    }
    #[must_use]
    pub const fn needs_attention(self) -> bool {
        self.needs_attention
    }
    #[must_use]
    pub const fn unresolved(self) -> bool {
        self.unresolved
    }
}

#[derive(Clone)]
pub(super) struct HistoryCoverage {
    pub range: std::ops::Range<u64>,
    pub observed: BlockNumHash,
}

impl HistoryCoverage {
    fn is_overdue(
        &self,
        record: &ExecutorRecord,
        submissions: &BTreeMap<B256, Option<u64>>,
    ) -> bool {
        self.range.end > self.observed.number
            && record
                .issued()
                .iter()
                .all(|payload| self.range.start <= payload.context().history_start())
            && record.issued().iter().any(|payload| {
                // observed is the canonical head minus this chain's finality depth.
                // A page behind that head never establishes an overdue operation.
                submissions
                    .get(&payload.hash())
                    .copied()
                    .flatten()
                    .is_some_and(|submitted| submitted < self.observed.number)
                    && !record.issued().iter().any(|other| {
                        other.nonce() == payload.nonce()
                            && other.inclusion().is_some_and(|inclusion| {
                                inclusion.result() == ExecutorExecutionResult::Executed
                            })
                    })
            })
    }
}

impl ExecutorOwner {
    /// Uses encrypted local history and this owner's observation coverage; no RPC.
    pub fn account_status(&self, record: &ExecutorRecord) -> Result<ExecutorAccountStatus> {
        self.ensure_active()?;
        let coverage = self
            .history_coverage
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?;
        let submissions = self
            .submission_blocks
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?;
        let overdue = coverage
            .get(&record.operation())
            .zip(submissions.get(&record.operation()))
            .is_some_and(|(coverage, submissions)| coverage.is_overdue(record, submissions));
        Ok(account_status(record, overdue))
    }
}

fn account_status(record: &ExecutorRecord, overdue: bool) -> ExecutorAccountStatus {
    use ExecutorAccountOutcome as Outcome;
    let mut outcome = match record.origin() {
        ExecutorRecordOrigin::Reserved => Outcome::NotSigned,
        ExecutorRecordOrigin::Discovered => Outcome::HistoryUnknown,
    };
    let mut unresolved = false;
    let mut missing = false;
    let mut reverted = false;
    let mut reserved_revert = false;
    let mut recovery_pending = false;
    let mut unconfirmed = false;
    let mut recovered = false;
    let mut executed = false;
    let mut nonces = BTreeSet::new();
    for payload in record.issued() {
        if !nonces.insert(payload.nonce()) {
            continue;
        }
        let group = || {
            record
                .issued()
                .iter()
                .filter(|other| other.nonce() == payload.nonce())
        };
        if let Some(winner) = group().find(|other| {
            other
                .inclusion()
                .is_some_and(|inclusion| inclusion.result() == ExecutorExecutionResult::Executed)
        }) {
            recovered |= winner.purpose() == ExecutorPayloadPurpose::Recovery;
            executed |= winner.purpose() == ExecutorPayloadPurpose::Operation;
            continue;
        }
        unresolved = true;
        for pending in group() {
            recovery_pending |= pending.purpose() == ExecutorPayloadPurpose::Recovery;
            match pending
                .inclusion()
                .map(crate::vault::ExecutorPayloadInclusion::result)
            {
                Some(ExecutorExecutionResult::MissingEffects) => missing = true,
                Some(ExecutorExecutionResult::Reverted) => {
                    reverted = true;
                    reserved_revert |= !pending.context().inputs().is_empty();
                }
                None => unconfirmed = true,
                Some(ExecutorExecutionResult::Executed) => unreachable!(),
            }
        }
    }
    // A successful approval or wrap does not complete a recovery. Group retries
    // by recovery identity; a canonical shield winner resolves earlier attempts.
    let recoveries = record
        .recovery_transactions()
        .iter()
        .map(crate::vault::IssuedExecutorRecoveryTransaction::recovery)
        .collect::<BTreeSet<_>>();
    for recovery in recoveries {
        let group = || {
            record
                .recovery_transactions()
                .iter()
                .filter(|tx| tx.recovery() == recovery)
        };
        if group().any(|tx| {
            tx.kind() == ExecutorRecoveryStepKind::Shield
                && tx.inclusion().is_some_and(|inclusion| {
                    inclusion.result() == ExecutorExecutionResult::Executed
                })
        }) {
            recovered = true;
            continue;
        }
        // Remove canonical losers before deciding whether this recovery still
        // has work. A replaced earlier attempt cannot override its winner.
        let active = group()
            .filter(|tx| {
                !record.recovery_transactions().iter().any(|other| {
                    other.hash() != tx.hash()
                        && tx.transaction().nonce.is_some()
                        && other.transaction().nonce == tx.transaction().nonce
                        && other.inclusion().is_some()
                })
            })
            .collect::<Vec<_>>();
        let shield_invalidated = group().any(|tx| tx.kind() == ExecutorRecoveryStepKind::Shield)
            && active.iter().all(|tx| {
                tx.kind() != ExecutorRecoveryStepKind::Shield
                    && tx.inclusion().is_some_and(|inclusion| {
                        inclusion.result() == ExecutorExecutionResult::Executed
                    })
            });
        if active.is_empty() || shield_invalidated {
            continue;
        }
        unresolved = true;
        recovery_pending = true;
        for tx in active {
            missing |= tx.inclusion().is_some_and(|inclusion| {
                inclusion.result() == ExecutorExecutionResult::MissingEffects
            });
        }
    }
    if executed {
        outcome = Outcome::Executed;
    }
    if recovered {
        outcome = Outcome::RecoveryConfirmed;
    }
    if unconfirmed {
        outcome = Outcome::Unconfirmed;
    }
    if reverted {
        outcome = Outcome::Reverted;
    }
    if recovery_pending {
        outcome = Outcome::RecoveryPending;
    }
    if missing {
        outcome = Outcome::MissingEffects;
    }
    let rechecked = match (
        record.issued().is_empty(),
        record.recovery_transactions().is_empty(),
    ) {
        (false, true) => record
            .nonce_observation()
            .map(crate::vault::ExecutorNonceObservation::block),
        (true, false) => record.recovery_observation(),
        (false, false) => record
            .nonce_observation()
            .map(crate::vault::ExecutorNonceObservation::block)
            .zip(record.recovery_observation())
            .map(|(payload, recovery)| {
                if payload.number < recovery.number {
                    payload
                } else {
                    recovery
                }
            }),
        (true, true) => None,
    };
    ExecutorAccountStatus {
        outcome,
        rechecked,
        needs_attention: missing
            || recovery_pending
            || reserved_revert
            || (unconfirmed && overdue && rechecked.is_some()),
        unresolved: unresolved || (rechecked.is_none() && !record.reserved_inputs().is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{
        ExecutorNonceObservation, ExecutorOperationId, ExecutorPayloadContext,
        ExecutorPayloadInclusion, IssuedExecutorPayload, IssuedExecutorRecoveryTransaction,
    };
    use alloy::primitives::{Address, B256, Bytes, U256};
    use alloy::rpc::types::TransactionRequest;

    fn record(
        payloads: &[serde_json::Value],
        transactions: &[serde_json::Value],
    ) -> ExecutorRecord {
        serde_json::from_value(serde_json::json!({
            "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
            "operation": ExecutorOperationId::random().unwrap(), "index": 0,
            "address": Address::repeat_byte(1), "delegate": Address::repeat_byte(2),
            "retired": true, "hidden": true, "assets": [],
            "created_at": null, "restored_at": null, "purpose_summary": null,
            "issued": payloads, "recovery_transactions": transactions,
            "nonce_observation": ExecutorNonceObservation::new(BlockNumHash::new(20, B256::repeat_byte(20)), U256::ONE),
            "recovery_observation": BlockNumHash::new(20, B256::repeat_byte(20)),
        })).unwrap()
    }

    fn with_inclusion(
        value: impl serde::Serialize,
        result: Option<ExecutorExecutionResult>,
    ) -> serde_json::Value {
        let mut value = serde_json::to_value(value).unwrap();
        value["inclusion"] = serde_json::to_value(result.map(|result| {
            ExecutorPayloadInclusion::new(
                BlockNumHash::new(12, B256::repeat_byte(12)),
                B256::repeat_byte(3),
                result,
            )
        }))
        .unwrap();
        value
    }

    fn payload(
        purpose: ExecutorPayloadPurpose,
        result: Option<ExecutorExecutionResult>,
    ) -> serde_json::Value {
        with_inclusion(
            IssuedExecutorPayload::new(
                U256::ZERO,
                Address::repeat_byte(2),
                B256::repeat_byte(if purpose == ExecutorPayloadPurpose::Recovery {
                    4
                } else {
                    5
                }),
                purpose,
                ExecutorPayloadContext::new(
                    Bytes::new(),
                    ExecutorNonceObservation::new(
                        BlockNumHash::new(10, B256::repeat_byte(10)),
                        U256::ZERO,
                    ),
                    Vec::new(),
                ),
            ),
            result,
        )
    }

    #[test]
    fn canonical_winner_resolves_competing_attempts_but_restart_keeps_freshness_separate() {
        let saved = record(
            &[
                payload(
                    ExecutorPayloadPurpose::Operation,
                    Some(ExecutorExecutionResult::MissingEffects),
                ),
                payload(
                    ExecutorPayloadPurpose::Recovery,
                    Some(ExecutorExecutionResult::Executed),
                ),
            ],
            &[],
        );
        let status = account_status(&saved, true);
        assert_eq!(status.outcome, ExecutorAccountOutcome::RecoveryConfirmed);
        assert!(!status.needs_attention && !status.unresolved);
        assert!(status.rechecked.is_some());
        let mut restarted = saved;
        restarted.require_reconciliation();
        let status = account_status(&restarted, true);
        assert_eq!(status.outcome, ExecutorAccountOutcome::RecoveryConfirmed);
        assert!(status.rechecked.is_none());
        // Reorg removes the winner; the older signed operation is live again.
        let reorg = record(
            &[
                payload(
                    ExecutorPayloadPurpose::Operation,
                    Some(ExecutorExecutionResult::MissingEffects),
                ),
                payload(ExecutorPayloadPurpose::Recovery, None),
            ],
            &[],
        );
        let status = account_status(&reorg, false);
        assert_eq!(status.outcome, ExecutorAccountOutcome::MissingEffects);
        assert!(status.needs_attention && status.unresolved);
    }

    #[test]
    fn approval_is_pending_until_shield_succeeds_and_a_later_recovery_remains_visible() {
        let recovery = ExecutorOperationId::random().unwrap();
        let tx = |recovery, step, kind, result| {
            with_inclusion(
                IssuedExecutorRecoveryTransaction::new(
                    recovery,
                    step,
                    kind,
                    TransactionRequest::default().nonce(u64::from(step)),
                    B256::repeat_byte(u8::try_from(step + 6).unwrap()),
                    BlockNumHash::new(10, B256::repeat_byte(10)),
                ),
                result,
            )
        };
        let approval = tx(
            recovery,
            0,
            ExecutorRecoveryStepKind::ApproveErc20,
            Some(ExecutorExecutionResult::Executed),
        );
        let failed = tx(
            recovery,
            1,
            ExecutorRecoveryStepKind::Shield,
            Some(ExecutorExecutionResult::Reverted),
        );
        let pending = record(&[], &[approval.clone(), failed]);
        let status = account_status(&pending, false);
        assert_eq!(status.outcome, ExecutorAccountOutcome::RecoveryPending);
        assert!(status.needs_attention && status.unresolved);
        let shield = tx(
            recovery,
            1,
            ExecutorRecoveryStepKind::Shield,
            Some(ExecutorExecutionResult::Executed),
        );
        let complete = record(&[], &[approval.clone(), shield.clone()]);
        assert_eq!(
            account_status(&complete, false).outcome,
            ExecutorAccountOutcome::RecoveryConfirmed
        );
        let mut replaced = tx(
            ExecutorOperationId::random().unwrap(),
            1,
            ExecutorRecoveryStepKind::ApproveErc20,
            None,
        );
        replaced["hash"] = serde_json::json!(B256::repeat_byte(9));
        let with_loser = record(&[], &[approval.clone(), shield.clone(), replaced]);
        assert_eq!(
            account_status(&with_loser, false).outcome,
            ExecutorAccountOutcome::RecoveryConfirmed
        );
        assert!(!account_status(&with_loser, false).needs_attention);
        let next = tx(
            ExecutorOperationId::random().unwrap(),
            2,
            ExecutorRecoveryStepKind::Wrap,
            None,
        );
        let later = record(&[], &[approval, shield, next]);
        assert!(account_status(&later, false).needs_attention);
    }
    #[test]
    fn attention_requires_reserved_reverted_inputs_or_caught_up_confirmation_coverage() {
        let mut reverted = payload(
            ExecutorPayloadPurpose::Operation,
            Some(ExecutorExecutionResult::Reverted),
        );
        reverted["context"]["inputs"] =
            serde_json::json!([{"tree": 0, "position": 1, "commitment": U256::ONE}]);
        let reverted = record(&[reverted], &[]);
        assert!(account_status(&reverted, false).needs_attention);
        let pending = record(&[payload(ExecutorPayloadPurpose::Operation, None)], &[]);
        let coverage = HistoryCoverage {
            range: 10..21,
            observed: BlockNumHash::new(20, B256::repeat_byte(20)),
        };
        let submissions = BTreeMap::from([(B256::repeat_byte(5), Some(10))]);
        assert!(
            !coverage.is_overdue(&pending, &BTreeMap::new()),
            "prefetched state alone cannot date submission"
        );
        assert!(coverage.is_overdue(&pending, &submissions));
        assert!(
            account_status(&pending, coverage.is_overdue(&pending, &submissions)).needs_attention
        );
        let behind = HistoryCoverage {
            range: 10..15,
            ..coverage
        };
        assert!(!behind.is_overdue(&pending, &submissions));
        let mut restarted = pending;
        restarted.require_reconciliation();
        assert!(!account_status(&restarted, true).needs_attention);
        let mut newer = payload(ExecutorPayloadPurpose::Operation, None);
        newer["nonce"] = serde_json::json!(U256::ONE);
        newer["hash"] = serde_json::json!(B256::repeat_byte(6));
        newer["context"]["observed"] =
            serde_json::json!(ExecutorNonceObservation::new(coverage.observed, U256::ONE));
        let mixed = record(
            &[
                payload(
                    ExecutorPayloadPurpose::Operation,
                    Some(ExecutorExecutionResult::Executed),
                ),
                newer,
            ],
            &[],
        );
        assert!(
            !coverage.is_overdue(&mixed, &submissions),
            "a completed older operation cannot age a newly signed one"
        );
    }
}
