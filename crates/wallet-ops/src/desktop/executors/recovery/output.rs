use alloy::primitives::{Address, B256, U256};
use broadcaster_core::contracts::railgun::ShieldRequest;
use railgun_wallet::{Utxo, UtxoCommitmentKind};

use crate::desktop::executor_observation::expected_shields;
use crate::desktop::executors::attribution::{
    ExecutorAttributionEvidence, ExecutorPayloadOutcome, payload_calls, payload_outcome,
};
use crate::vault::{ExecutorPayloadPurpose, ExecutorRecord, IssuedExecutorPayload};

/// Where a batch or paid recovery stands, from its nonce, the action attributed there and
/// the shields private sync shows. Derived on each call and never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorRecoveryCompletion {
    /// The nonce is not known to be consumed. The recovery can still execute and keeps its
    /// reservations.
    Pending,
    /// The recovery ran, and private sync does not show every shield it requested yet.
    ShieldPending,
    /// The recovery ran and every shield it requested is received. One that requested none,
    /// as a cancellation, is complete once it is the action that ran.
    Complete,
    /// Another action ran at the recovery's nonce.
    Superseded,
    /// The nonce is consumed and nothing names what ran. The recovery is not complete, and
    /// another stays available for whatever the account's balances show.
    Unattributed,
}

/// The completion of the recovery payload `hash`, or `None` when `record` holds no such
/// recovery. A consumed nonce alone completes nothing: the recovery must be the action
/// attributed at it, and private sync must show every shield it requested.
#[must_use]
pub fn executor_recovery_completion(
    record: &ExecutorRecord,
    hash: B256,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> Option<ExecutorRecoveryCompletion> {
    let payload = record.issued().iter().find(|payload| {
        payload.hash() == hash && payload.purpose() == ExecutorPayloadPurpose::Recovery
    })?;
    Some(match payload_outcome(record, hash, evidence)? {
        ExecutorPayloadOutcome::Pending => ExecutorRecoveryCompletion::Pending,
        ExecutorPayloadOutcome::Superseded => ExecutorRecoveryCompletion::Superseded,
        ExecutorPayloadOutcome::Resolved => ExecutorRecoveryCompletion::Unattributed,
        ExecutorPayloadOutcome::Executed => {
            if received_shields(record, payload, evidence).is_some() {
                ExecutorRecoveryCompletion::Complete
            } else {
                ExecutorRecoveryCompletion::ShieldPending
            }
        }
    })
}

/// Whether a recovery of `record` is the action that ran at its nonce and private sync shows
/// every shield it requested, one of them received at or after `block`. A cancellation
/// shields nothing and never counts.
#[must_use]
pub fn executor_recovered_since(
    record: &ExecutorRecord,
    block: u64,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> bool {
    record
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Recovery)
        .any(|payload| {
            payload_outcome(record, payload.hash(), evidence)
                == Some(ExecutorPayloadOutcome::Executed)
                && received_shields(record, payload, evidence)
                    .is_some_and(|shields| shields.iter().any(|(_, received)| *received >= block))
        })
}

/// The latest received shield of `token` at or after `block` from an attributed recovery
/// whose every requested shield private sync shows. A recovery of another asset or a
/// cancellation does not count.
#[must_use]
pub fn executor_recovered_token_since(
    record: &ExecutorRecord,
    token: Address,
    block: u64,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> Option<u64> {
    record
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Recovery)
        .filter(|payload| {
            payload_outcome(record, payload.hash(), evidence)
                == Some(ExecutorPayloadOutcome::Executed)
        })
        .filter_map(|payload| received_shields(record, payload, evidence))
        .flatten()
        .filter(|(request, received)| {
            request.preimage.token.tokenType == 0
                && request.preimage.token.tokenAddress == token
                && *received >= block
        })
        .map(|(_, received)| received)
        .max()
}

/// Each requested shield and the block where private sync shows it, once it shows them
/// all. Empty for a payload that requested none. Each request is salted, so its note names it.
fn received_shields(
    record: &ExecutorRecord,
    payload: &IssuedExecutorPayload,
    evidence: &ExecutorAttributionEvidence<'_>,
) -> Option<Vec<(ShieldRequest, u64)>> {
    let requests = expected_shields(
        record.address()?,
        evidence.railgun,
        &payload_calls(payload)?,
    )
    .ok()?;
    if requests.is_empty() {
        return Some(Vec::new());
    }
    let utxos = &evidence.sync?.utxos;
    requests
        .into_iter()
        .map(|request| {
            utxos
                .iter()
                .find(|entry| is_shield_of(&entry.utxo, &request))
                .map(|entry| (request, entry.utxo.source.block_number))
        })
        .collect()
}

/// The amount of a token an account still holds: its balance at the latest confirmed read
/// among `observed`, each a balance with the number of the block it was read at. `None`
/// without a read. Anyone can raise a balance, so this says how much is held, where more is
/// safe, and that nothing is left, where zero is exact. Completion never rests on it.
#[must_use]
pub fn executor_recovery_remaining_amount(
    observed: impl IntoIterator<Item = (U256, u64)>,
) -> Option<U256> {
    observed
        .into_iter()
        .max_by_key(|(_, block)| *block)
        .map(|(amount, _)| amount)
}

/// Whether `utxo` is a shield note `request` created. The request's own key identifies it,
/// and the note holds the requested value less the protocol fee.
pub(in crate::desktop::executors) fn is_shield_of(utxo: &Utxo, request: &ShieldRequest) -> bool {
    utxo.poi.commitment_kind == UtxoCommitmentKind::Shield
        && utxo.note.npk == U256::from_be_bytes(request.preimage.npk.0)
        && utxo.note.token_hash == request.preimage.token.id()
        && utxo.note.value <= U256::from(request.preimage.value)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::desktop::executors::attribution::InvalidatedSwapOrders;
    use crate::desktop::executors::attribution::tests::{
        RAILGUN, RESOLVED_AT, SETTLEMENT, execute, note, operation_and_recovery, payload,
        pre_hook_and_cancellation, pre_hook_executed, record, recovery, shield, shield_call,
        shielded, spent, synced,
    };
    use crate::vault::SwapOrderObservations;
    use sync_service::WalletCurrentSnapshot;

    fn evidence(sync: Option<&Arc<WalletCurrentSnapshot>>) -> ExecutorAttributionEvidence<'_> {
        ExecutorAttributionEvidence {
            railgun: RAILGUN,
            sync: sync.map(Arc::as_ref),
            invalidated_orders: Some(InvalidatedSwapOrders {
                settlement: SETTLEMENT,
                orders: &[],
            }),
        }
    }

    fn completion(
        record: &ExecutorRecord,
        sync: Option<&Arc<WalletCurrentSnapshot>>,
        hash: u8,
    ) -> ExecutorRecoveryCompletion {
        executor_recovery_completion(record, B256::repeat_byte(hash), &evidence(sync)).unwrap()
    }

    #[test]
    fn recovery_completes_once_attributed_with_its_shield_received() {
        let request = shield(7);
        // Its nonce is not consumed yet, so it can still execute.
        assert_eq!(
            completion(&record(vec![recovery(2, &request)], 0), None, 2),
            ExecutorRecoveryCompletion::Pending
        );
        // The only action signed at its nonce ran. Its shield is still to be shown.
        let alone = record(vec![recovery(2, &request)], 1);
        assert_eq!(
            completion(&alone, None, 2),
            ExecutorRecoveryCompletion::ShieldPending
        );
        let behind = synced(RESOLVED_AT, Vec::new());
        assert_eq!(
            completion(&alone, Some(&behind), 2),
            ExecutorRecoveryCompletion::ShieldPending
        );
        assert!(!executor_recovered_since(
            &alone,
            0,
            &evidence(Some(&behind))
        ));
        // Against an operation at the same nonce, the shield is what attributes it.
        let input = note(1);
        let contested = operation_and_recovery(&input, &[], &request);
        let sync = synced(RESOLVED_AT, vec![input, shielded(&request)]);
        for saved in [&alone, &contested] {
            assert_eq!(
                completion(saved, Some(&sync), 2),
                ExecutorRecoveryCompletion::Complete
            );
            // The shield arrives in block 15.
            assert!(executor_recovered_since(saved, 15, &evidence(Some(&sync))));
            assert!(!executor_recovered_since(saved, 16, &evidence(Some(&sync))));
        }
    }

    #[test]
    fn token_recovery_requires_every_shield_and_returns_only_matching_erc20_blocks() {
        use broadcaster_core::contracts::railgun::TokenData;

        let token = Address::repeat_byte(5);
        let first = shield(7);
        let later = shield(8);
        let mut other = shield(9);
        other.preimage.token = TokenData::erc20(Address::repeat_byte(6));
        let mut nft = shield(10);
        nft.preimage.token.tokenType = 1;
        nft.preimage.token.tokenSubID = U256::ONE;
        let issued = payload(
            0,
            2,
            ExecutorPayloadPurpose::Recovery,
            &execute(0, [&first, &later, &other, &nft].map(shield_call).into()),
            &[],
            &[],
        );
        let saved = record(vec![issued.clone()], 1);
        let received = |request: &ShieldRequest, block| {
            let mut utxo = shielded(request);
            utxo.utxo.source.block_number = block;
            utxo
        };
        // Seeing both shields of the requested token is insufficient while another request
        // in the recovery remains missing.
        let incomplete = synced(
            RESOLVED_AT + 10,
            vec![
                received(&first, 15),
                received(&later, 20),
                received(&nft, 30),
            ],
        );
        assert_eq!(
            executor_recovered_token_since(&saved, token, 0, &evidence(Some(&incomplete))),
            None
        );
        let complete = synced(
            RESOLVED_AT + 10,
            vec![
                received(&first, 15),
                received(&later, 20),
                received(&other, 25),
                received(&nft, 30),
            ],
        );
        let evidence = evidence(Some(&complete));
        assert_eq!(
            executor_recovered_token_since(&saved, token, 15, &evidence),
            Some(20)
        );
        assert_eq!(
            executor_recovered_token_since(&saved, token, 21, &evidence),
            None
        );
        assert_eq!(
            executor_recovered_token_since(&saved, Address::repeat_byte(6), 15, &evidence),
            Some(25)
        );
        // The account-wide helper retains its existing meaning, including the later NFT.
        assert!(executor_recovered_since(&saved, 30, &evidence));
        let pending = record(vec![issued], 0);
        assert_eq!(
            executor_recovered_token_since(&pending, token, 0, &evidence),
            None
        );
    }

    #[test]
    fn resolved_nonce_with_nothing_attributed_leaves_recovery_incomplete_and_available() {
        // A cancellation whose broadcaster reply was lost, at its pre-hook's nonce. Its fee
        // note is spent in a transaction the wallet never recorded.
        let fee = note(1);
        let saved = pre_hook_and_cancellation(&fee, Vec::new(), &SwapOrderObservations::default());
        let sync = synced(RESOLVED_AT + 10, vec![spent(fee, 31, 15)]);
        assert_eq!(
            completion(&saved, Some(&sync), 4),
            ExecutorRecoveryCompletion::Unattributed
        );
        assert!(!executor_recovered_since(&saved, 0, &evidence(Some(&sync))));
        // Nothing at the consumed nonce is outstanding, so it blocks no further recovery.
        assert!(!saved.has_unresolved_issued_work());
    }

    #[test]
    fn cancellation_that_lost_its_nonce_to_a_recorded_pre_hook_is_superseded() {
        let saved = pre_hook_and_cancellation(&note(1), Vec::new(), &pre_hook_executed());
        // It requested no shield, so only attribution keeps it from counting as complete.
        assert_eq!(
            completion(&saved, None, 4),
            ExecutorRecoveryCompletion::Superseded
        );
    }
}
