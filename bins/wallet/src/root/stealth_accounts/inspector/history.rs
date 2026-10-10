use alloy::eips::BlockNumHash;
use wallet_ops::{
    ExecutorAttributionEvidence, ExecutorPayloadOutcome, ExecutorRecoveryCompletion,
    ExecutorSignedAction, PublicActionProgressStep, executor_payload_recovery_steps,
    executor_recovery_completion,
    vault::{ExecutorPayloadPurpose, ExecutorRecord, IssuedExecutorPayload},
};

/// The payload a signed action's row shows: its fee round with the latest submitted
/// transaction, or the last one signed when none was submitted.
pub(super) fn action_payload<'a>(
    record: &'a ExecutorRecord,
    action: &ExecutorSignedAction,
) -> Option<&'a IssuedExecutorPayload> {
    let latest_first = action
        .payloads()
        .iter()
        .rev()
        .filter_map(|hash| {
            record
                .issued()
                .iter()
                .find(|payload| payload.hash() == *hash)
        })
        .collect::<Vec<_>>();
    latest_first
        .iter()
        .find(|payload| !payload.transaction_hashes().is_empty())
        .or_else(|| latest_first.first())
        .copied()
}

pub(super) fn payload_purpose(payload: &IssuedExecutorPayload) -> String {
    match payload.purpose() {
        ExecutorPayloadPurpose::Operation => return "Operation".into(),
        ExecutorPayloadPurpose::SwapPreHook => return "Swap pre-hook".into(),
        ExecutorPayloadPurpose::SwapPostHook => return "Swap post-hook".into(),
        ExecutorPayloadPurpose::SwapDestinationShield => return "Swap shield".into(),
        ExecutorPayloadPurpose::Recovery => {}
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

/// The block the recorded results stand at: this session's account read, or the block
/// that last resolved a nonce for an account not read since the wallet started.
pub(super) fn status_as_of(record: &ExecutorRecord, read: Option<BlockNumHash>) -> String {
    let block = read.map(|block| block.number).or_else(|| {
        record
            .nonce_watermark()
            .map(wallet_ops::vault::ExecutorNonceWatermark::block)
    });
    let mut label = block.map_or_else(
        || "No account read is recorded".into(),
        |block| format!("Result as of #{}", block_label(block)),
    );
    if read.is_none() {
        label.push_str(" · account not read since restart");
    }
    label
}

/// What became of each operation and recovery signed for the account. Swap hooks are left
/// to the swap's own status.
pub(super) fn recorded_outcomes(
    record: &ExecutorRecord,
    actions: &[ExecutorSignedAction],
    evidence: &ExecutorAttributionEvidence<'_>,
) -> Vec<String> {
    let mut outcomes = Vec::new();
    for action in actions {
        let subject = match action.purpose() {
            ExecutorPayloadPurpose::Operation => "operation",
            ExecutorPayloadPurpose::Recovery => "recovery",
            ExecutorPayloadPurpose::SwapPreHook
            | ExecutorPayloadPurpose::SwapPostHook
            | ExecutorPayloadPurpose::SwapDestinationShield => continue,
        };
        let recovery = action.purpose() == ExecutorPayloadPurpose::Recovery;
        let outcome = match action.outcome() {
            ExecutorPayloadOutcome::Pending => {
                format!("The signed {subject} is not confirmed and can still execute.")
            }
            ExecutorPayloadOutcome::Superseded => {
                format!("Another signed action ran at the {subject}'s nonce.")
            }
            ExecutorPayloadOutcome::Resolved => format!(
                "Nonce {} was used, and nothing recorded shows which signed action ran.",
                action.nonce()
            ),
            ExecutorPayloadOutcome::Executed if recovery => {
                recovery_outcome(record, action, evidence).to_owned()
            }
            ExecutorPayloadOutcome::Executed => "Executed.".to_owned(),
        };
        let outcome = match action.spend_block() {
            Some(block) => format!(
                "{outcome} Private sync shows its spend in block #{}.",
                block_label(block)
            ),
            None => outcome,
        };
        if !outcomes.contains(&outcome) {
            outcomes.push(outcome);
        }
    }
    outcomes
}

/// A recovery that ran is complete once private sync shows every shield it requested.
fn recovery_outcome(
    record: &ExecutorRecord,
    action: &ExecutorSignedAction,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> &'static str {
    // Fee rounds share their calls, so any payload of the action tells.
    let shields = action_payload(record, action).is_some_and(|payload| {
        executor_payload_recovery_steps(payload).contains(&PublicActionProgressStep::Shield)
    });
    let completion = action
        .payloads()
        .first()
        .and_then(|hash| executor_recovery_completion(record, *hash, evidence));
    match completion {
        Some(ExecutorRecoveryCompletion::Complete) if shields => {
            "Leftover balance recovered to your private balance."
        }
        Some(ExecutorRecoveryCompletion::Complete) => "Recovery executed.",
        _ => "Recovery executed. Its shield has not reached your private balance yet.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, B256, Bytes, U256};
    use alloy::sol_types::SolCall;
    use broadcaster_core::contracts::railgun::RelayAdapt7702;
    use wallet_ops::signed_actions;
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorNonceWatermark, ExecutorOperationId,
        ExecutorPayloadContext,
    };

    /// An operation payload. Calldata that does not decode makes it an action of its own.
    fn payload(nonce: u64, hash: u8, calldata: Bytes, submitted: bool) -> serde_json::Value {
        let block = BlockNumHash::new(25_990_899, B256::repeat_byte(10));
        let mut value = serde_json::to_value(IssuedExecutorPayload::new(
            U256::from(nonce),
            Address::ZERO,
            B256::repeat_byte(hash),
            ExecutorPayloadPurpose::Operation,
            ExecutorPayloadContext::new(
                calldata,
                ExecutorNonceObservation::new(block, U256::ZERO),
                Vec::new(),
            ),
        ))
        .unwrap();
        if submitted {
            value["transaction_hashes"] = serde_json::json!([B256::repeat_byte(20)]);
        }
        value
    }

    /// One fee round of an action with no calls. Rounds differ only in their signature.
    fn fee_round(signature: u8) -> Bytes {
        RelayAdapt7702::multicallCall {
            _requireSuccess: true,
            _calls: Vec::new(),
            _nonce: U256::ZERO,
            _signature: Bytes::from(vec![signature]),
        }
        .abi_encode()
        .into()
    }

    fn record(payloads: &[serde_json::Value], consumed: u64) -> ExecutorRecord {
        serde_json::from_value(serde_json::json!({
            "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
            "operation": ExecutorOperationId::random().unwrap(), "index": 0,
            "address": Address::ZERO, "delegate": Address::ZERO, "retired": true,
            "issued": payloads,
            "nonce_watermark": ExecutorNonceWatermark::new(U256::from(consumed), 25_990_900),
        }))
        .unwrap()
    }

    #[test]
    fn each_signed_action_has_a_row_with_its_recorded_result() {
        use ExecutorPayloadOutcome::{Executed, Pending, Resolved};
        // Nonce 0 holds one action signed in two fee rounds, of which only the first was
        // submitted. Nonce 1 holds two actions that nothing tells apart, and nonce 2 is not
        // consumed.
        let record = record(
            &[
                payload(0, 1, fee_round(1), true),
                payload(0, 2, fee_round(2), false),
                payload(1, 3, Bytes::new(), false),
                payload(1, 4, Bytes::new(), false),
                payload(2, 5, Bytes::new(), false),
            ],
            2,
        );
        let evidence = ExecutorAttributionEvidence::record_only();
        let actions = signed_actions(&record, &evidence);
        // The first row shows the submitted round over the one signed after it.
        assert_eq!(
            actions
                .iter()
                .map(|action| (
                    action_payload(&record, action).unwrap().hash(),
                    action.outcome()
                ))
                .collect::<Vec<_>>(),
            [(1, Executed), (3, Resolved), (4, Resolved), (5, Pending)]
                .map(|(hash, outcome)| (B256::repeat_byte(hash), outcome))
        );
        // Both actions at nonce 1 read the same, and are told once.
        assert_eq!(recorded_outcomes(&record, &actions, &evidence).len(), 3);
    }
}
