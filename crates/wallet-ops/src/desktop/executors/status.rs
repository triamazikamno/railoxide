//! Local presentation of retained history. This never supplies signing admission.
use std::collections::BTreeMap;

use alloy::eips::BlockNumHash;
use alloy::primitives::B256;
use eyre::{Result, eyre};

use super::attribution::{ExecutorAttributionEvidence, ExecutorPayloadOutcome, signed_actions};
use super::{ExecutorOwner, ExecutorRecoveryCompletion, executor_recovery_completion};
use crate::vault::{
    ExecutorNonceObservation, ExecutorPayloadPurpose, ExecutorRecord, ExecutorRecordOrigin,
};

/// What an account's signed work came to. Work that can still execute decides first, then
/// the actions that ran at the account's resolved nonces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutorAccountOutcome {
    NotSigned,
    HistoryUnknown,
    /// A signed payload's nonce is not known to be consumed, so it can still execute.
    Unconfirmed,
    /// An operation ran at its nonce.
    Executed,
    /// An action the account signed lost its nonce to another, and no operation or recovery
    /// of the account ran. A swap hook that took the nonce leaves this.
    Superseded,
    /// A nonce is consumed and nothing names the action that ran at it.
    Resolved,
    /// A recovery can still execute, or it ran and private sync does not show its shield.
    RecoveryPending,
    /// A recovery ran and private sync shows every shield it requested.
    RecoveryConfirmed,
}

#[derive(Clone, Copy, Debug)]
pub struct ExecutorAccountStatus {
    outcome: ExecutorAccountOutcome,
    read: Option<BlockNumHash>,
    overdue: bool,
    needs_attention: bool,
    unresolved: bool,
}

impl Default for ExecutorAccountStatus {
    fn default() -> Self {
        Self {
            outcome: ExecutorAccountOutcome::HistoryUnknown,
            read: None,
            overdue: false,
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
    /// The block of the latest read of the account's execution nonce in this session.
    /// `None` when the account has not been read since the session started.
    #[must_use]
    pub const fn read_this_session(self) -> Option<BlockNumHash> {
        self.read
    }
    /// Whether an account read of this session shows a submitted payload's nonce unconsumed
    /// at a confirmed block past the block its submission is dated to. Without such a read
    /// nothing is asserted.
    #[must_use]
    pub const fn overdue(self) -> bool {
        self.overdue
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

/// Whether `read`, an account read of this session, shows a submitted payload's nonce
/// unconsumed at a confirmed block past the block the submission is dated to. `submissions`
/// holds that block for each payload handed off in this session, once a read dated it.
fn is_overdue(
    record: &ExecutorRecord,
    read: ExecutorNonceObservation,
    submissions: &BTreeMap<B256, Option<u64>>,
) -> bool {
    record.issued().iter().any(|payload| {
        payload.nonce() >= read.nonce()
            && !record.nonce_resolved(payload.nonce())
            && submissions
                .get(&payload.hash())
                .copied()
                .flatten()
                .is_some_and(|submitted| submitted < read.block().number)
    })
}

impl ExecutorOwner {
    /// Uses encrypted local history, private sync's snapshot and this session's account
    /// reads; no RPC.
    pub fn account_status(&self, record: &ExecutorRecord) -> Result<ExecutorAccountStatus> {
        self.ensure_active()?;
        let read = self
            .account_reads
            .lock()
            .map_err(|_| eyre!("executor observations are unavailable"))?
            .get(&record.operation())
            .copied();
        let overdue = match read {
            Some(read) => self
                .submission_blocks
                .lock()
                .map_err(|_| eyre!("executor observations are unavailable"))?
                .get(&record.operation())
                .is_some_and(|submissions| is_overdue(record, read, submissions)),
            None => false,
        };
        let attribution = self.attribution(record);
        let evidence = match &attribution {
            Some(attribution) => attribution.evidence(),
            None => ExecutorAttributionEvidence::record_only(),
        };
        Ok(account_status(record, &evidence, read, overdue))
    }
}

fn account_status(
    record: &ExecutorRecord,
    evidence: &ExecutorAttributionEvidence<'_>,
    read: Option<ExecutorNonceObservation>,
    overdue: bool,
) -> ExecutorAccountStatus {
    use ExecutorAccountOutcome as Outcome;
    let mut unconfirmed = false;
    let mut recovery_pending = false;
    let mut recovered = false;
    let mut executed = false;
    let mut unnamed = false;
    let mut superseded = false;
    for action in signed_actions(record, evidence) {
        let recovery = action.purpose() == ExecutorPayloadPurpose::Recovery;
        match action.outcome() {
            ExecutorPayloadOutcome::Pending if recovery => recovery_pending = true,
            ExecutorPayloadOutcome::Pending => unconfirmed = true,
            ExecutorPayloadOutcome::Executed if recovery => {
                // Fee rounds share their calls, so any payload of the action tells.
                let complete = action
                    .payloads()
                    .first()
                    .and_then(|hash| executor_recovery_completion(record, *hash, evidence))
                    == Some(ExecutorRecoveryCompletion::Complete);
                recovered |= complete;
                recovery_pending |= !complete;
            }
            // A swap hook that ran settles its nonce and is no outcome of the account.
            ExecutorPayloadOutcome::Executed => {
                executed |= action.purpose() == ExecutorPayloadPurpose::Operation;
            }
            ExecutorPayloadOutcome::Superseded => superseded = true,
            ExecutorPayloadOutcome::Resolved => unnamed = true,
        }
    }
    let outcome = if recovery_pending {
        Outcome::RecoveryPending
    } else if unconfirmed {
        Outcome::Unconfirmed
    } else if recovered {
        Outcome::RecoveryConfirmed
    } else if unnamed {
        Outcome::Resolved
    } else if executed {
        Outcome::Executed
    } else if superseded {
        Outcome::Superseded
    } else {
        match record.origin() {
            ExecutorRecordOrigin::Reserved => Outcome::NotSigned,
            ExecutorRecordOrigin::Discovered => Outcome::HistoryUnknown,
        }
    };
    ExecutorAccountStatus {
        outcome,
        read: read.map(ExecutorNonceObservation::block),
        overdue,
        needs_attention: recovery_pending || (unconfirmed && overdue),
        unresolved: unconfirmed || recovery_pending,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::primitives::U256;
    use sync_service::WalletCurrentSnapshot;

    use super::ExecutorAccountOutcome as Outcome;
    use super::*;
    use crate::desktop::executors::attribution::tests::{
        RAILGUN, RESOLVED_AT, execute, note, operation_and_recovery, payload,
        pre_hook_and_cancellation, pre_hook_executed, record, recovery, shield, shielded, synced,
    };
    use crate::vault::ExecutorPayloadPurpose::Operation;

    fn status(
        record: &ExecutorRecord,
        sync: Option<&Arc<WalletCurrentSnapshot>>,
        read: Option<ExecutorNonceObservation>,
        overdue: bool,
    ) -> ExecutorAccountStatus {
        let evidence = ExecutorAttributionEvidence {
            railgun: RAILGUN,
            sync: sync.map(Arc::as_ref),
            invalidated_orders: None,
        };
        account_status(record, &evidence, read, overdue)
    }

    /// An operation at nonce 0, payload 1, with no private inputs.
    fn operation() -> serde_json::Value {
        payload(0, 1, Operation, &execute(0, Vec::new()), &[], &[])
    }

    /// An account read at `block` that shows `nonce` as the next execution nonce.
    fn read(block: u64, nonce: u64) -> ExecutorNonceObservation {
        ExecutorNonceObservation::new(
            BlockNumHash::new(block, B256::repeat_byte(9)),
            U256::from(nonce),
        )
    }

    #[test]
    fn account_outcome_follows_pending_work_and_the_action_that_ran_at_each_nonce() {
        let outcome = |record: &ExecutorRecord, sync| status(record, sync, None, false).outcome;
        assert_eq!(outcome(&record(Vec::new(), 0), None), Outcome::NotSigned);
        assert_eq!(
            outcome(&record(vec![operation()], 0), None),
            Outcome::Unconfirmed
        );
        // The only action signed at a consumed nonce is the one that ran.
        assert_eq!(
            outcome(&record(vec![operation()], 1), None),
            Outcome::Executed
        );
        // A recorded pre-hook took the nonce its order's cancellation was signed at.
        let lost = pre_hook_and_cancellation(&note(1), Vec::new(), &pre_hook_executed());
        assert_eq!(outcome(&lost, None), Outcome::Superseded);
        assert!(!status(&lost, None, Some(read(RESOLVED_AT, 2)), false).unresolved);
        // An operation and a recovery share a consumed nonce and neither left evidence.
        let input = note(1);
        let request = shield(7);
        let shared = operation_and_recovery(&input, &[], &request);
        let behind = synced(RESOLVED_AT, vec![input.clone()]);
        assert_eq!(outcome(&shared, Some(&behind)), Outcome::Resolved);
        // The recovery's shield names it, and completes it.
        let arrived = synced(RESOLVED_AT, vec![input, shielded(&request)]);
        assert_eq!(outcome(&shared, Some(&arrived)), Outcome::RecoveryConfirmed);
        let pending = status(&record(vec![recovery(2, &request)], 0), None, None, false);
        assert_eq!(pending.outcome, Outcome::RecoveryPending);
        assert!(pending.needs_attention && pending.unresolved);
    }

    #[test]
    fn payload_recorded_as_reverted_by_an_earlier_version_is_unconfirmed() {
        for result in ["Reverted", "MissingEffects"] {
            let mut stored = operation();
            stored["inclusion"] = serde_json::json!({
                "block": BlockNumHash::new(12, B256::repeat_byte(12)),
                "transaction_hash": B256::repeat_byte(3),
                "result": result,
            });
            // Its nonce is unconsumed, so its signature can still execute.
            let status = status(&record(vec![stored], 0), None, None, false);
            assert_eq!(status.outcome, Outcome::Unconfirmed, "{result}");
            assert!(status.unresolved && !status.needs_attention, "{result}");
        }
    }

    #[test]
    fn sole_shielding_recovery_is_pending_until_private_sync_shows_its_shield() {
        let request = shield(7);
        // The recovery is the only action at its consumed nonce, so it ran.
        let saved = record(vec![recovery(2, &request)], 1);
        let read = Some(read(RESOLVED_AT, 1));
        let behind = synced(RESOLVED_AT, Vec::new());
        for sync in [None, Some(&behind)] {
            let status = status(&saved, sync, read, false);
            assert_eq!(status.outcome, Outcome::RecoveryPending);
            assert!(status.needs_attention && status.unresolved);
        }
        let arrived = synced(RESOLVED_AT, vec![shielded(&request)]);
        let status = status(&saved, Some(&arrived), read, false);
        assert_eq!(status.outcome, Outcome::RecoveryConfirmed);
        assert!(!status.needs_attention && !status.unresolved);
    }

    #[test]
    fn overdue_needs_an_account_read_past_the_submissions_dated_block() {
        let saved = record(vec![operation()], 0);
        let hash = B256::repeat_byte(1);
        // The first read after handoff dated the submission to block 12.
        let dated = BTreeMap::from([(hash, Some(12))]);
        assert!(
            !is_overdue(&saved, read(12, 0), &dated),
            "the read that dated the submission says nothing yet"
        );
        assert!(is_overdue(&saved, read(13, 0), &dated));
        assert!(
            !is_overdue(&saved, read(13, 0), &BTreeMap::from([(hash, None)])),
            "a submission no read has dated has no known age"
        );
        assert!(
            !is_overdue(&saved, read(13, 0), &BTreeMap::new()),
            "a payload handed off before this session has no known age"
        );
        assert!(
            !is_overdue(&record(vec![operation()], 1), read(13, 1), &dated),
            "a consumed nonce is not overdue"
        );
        // Attention follows the assertion, never the passing of time alone.
        assert!(status(&saved, None, Some(read(13, 0)), true).needs_attention);
        let unread = status(&saved, None, None, false);
        assert!(!unread.needs_attention && unread.read_this_session().is_none());
    }
}
