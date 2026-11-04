use std::collections::{BTreeSet, HashSet};

use alloy::primitives::B256;

use super::helpers::*;
use crate::bridge::NonEvmNetwork;
use crate::tron::address::encode_tron_address;
use crate::vault::{
    ActivityAsset, ActivityAttempt, ActivityAttemptOutcome, ActivityCounterparty,
    ActivityDirection, ActivityEntry, ActivityEntryId, ActivityInput, ActivityKind, ActivityLeg,
    ActivityMatch, ActivityNetwork, ActivityOwner, ActivitySource, ActivityStep, BridgeDelivery,
    BridgePrivateDelivery, BridgeProvider, BridgeShieldFailure, BridgeSurplus,
    NonEvmAddressBookEntry, PrivateAddressBookEntry, SwapDelivery,
};
use crate::{
    ActivityConfirmation, ActivityStatus, ActivityUtxoClassification, ReceivedKind, StatusInputs,
    SwapRowRecipient, UtxoOutput, UtxoPpoiState, entry_status, is_in_flight, received_rows,
    resolve_counterparty,
};

pub(crate) fn activity_entry(
    kind: ActivityKind,
    match_key: ActivityMatch,
    attempts: Vec<ActivityAttempt>,
) -> ActivityEntry {
    ActivityEntry {
        id: ActivityEntryId::random().unwrap(),
        owner: ActivityOwner::Wallet("wallet".to_owned()),
        network: ActivityNetwork::Evm(1),
        kind,
        source: ActivitySource::Wallet,
        created_at: 1,
        legs: Vec::new(),
        counterparty: None,
        fee: None,
        gas_payer: None,
        match_key,
        attempts,
        relay_timed_out: false,
        cleared: false,
    }
}

pub(crate) const fn activity_attempt(
    hash: u8,
    step: ActivityStep,
    outcome: Option<ActivityAttemptOutcome>,
) -> ActivityAttempt {
    ActivityAttempt {
        hash: B256::repeat_byte(hash),
        step,
        outcome,
    }
}

const fn included(block: u64, success: bool) -> ActivityAttemptOutcome {
    ActivityAttemptOutcome::Included {
        block,
        timestamp: block * 10,
        success,
    }
}

/// A note of 5 of token `0x11…` created in transaction `source` at block and time `timestamp`.
pub(crate) fn received_note(
    position: u64,
    source: u8,
    timestamp: u64,
    classification: ActivityUtxoClassification,
) -> UtxoOutput {
    UtxoOutput {
        tree: 0,
        position,
        token: Address::repeat_byte(0x11).to_checksum(None),
        value: "5".to_owned(),
        commitment_kind: if classification == ActivityUtxoClassification::PrivateOutput {
            "Transact"
        } else {
            "Shield"
        }
        .to_owned(),
        activity_classification: classification.label().to_owned(),
        blocked_shield_rescue: None,
        commitment: String::new(),
        npk: String::new(),
        blinded_commitment: String::new(),
        poi_statuses: BTreeMap::new(),
        ppoi_state: UtxoPpoiState::Unknown,
        ppoi_last_submission_at: None,
        poi_spendable: false,
        source_tx_hash: hex::encode_prefixed(B256::repeat_byte(source)),
        source_block_number: timestamp,
        source_block_timestamp: timestamp,
        is_spent: false,
        pending_new: false,
        pending_spent: false,
        local_pending_spent: false,
        spent_tx_hash: None,
        spent_block_number: None,
        spent_block_timestamp: None,
    }
}

fn spend(note: &mut UtxoOutput, hash: u8, block: u64) {
    note.is_spent = true;
    note.spent_tx_hash = Some(hex::encode_prefixed(B256::repeat_byte(hash)));
    note.spent_block_number = Some(block);
}

#[test]
fn received_rows_group_third_party_transactions_and_skip_own_and_hidden_ones() {
    use ActivityUtxoClassification::{BlockedShield, PrivateOutput, Shield};
    // Received at the hide marker, then spent in 0x02, which returned change.
    let mut hidden = received_note(0, 0x01, 100, PrivateOutput);
    spend(&mut hidden, 0x02, 200);
    let mut second_token = received_note(3, 0x03, 300, PrivateOutput);
    second_token.token = Address::repeat_byte(0x22).to_checksum(None);
    let utxos = [
        hidden,
        received_note(1, 0x02, 200, PrivateOutput),
        received_note(2, 0x03, 300, PrivateOutput),
        second_token,
        received_note(4, 0x04, 400, BlockedShield),
        received_note(5, 0x05, 500, Shield),
    ];
    let leg = |token: u8| ActivityLeg {
        asset: ActivityAsset::Token(Address::repeat_byte(token)),
        amount: U256::from(5),
        direction: ActivityDirection::In,
    };

    let rows = received_rows(&utxos, &HashSet::new(), 100);
    assert_eq!(
        rows.into_iter()
            .map(|row| (row.hash, row.kind, row.legs))
            .collect::<Vec<_>>(),
        vec![
            (
                B256::repeat_byte(0x05),
                ReceivedKind::ShieldReceived { blocked: false },
                vec![leg(0x11)],
            ),
            (
                B256::repeat_byte(0x04),
                ReceivedKind::ShieldReceived { blocked: true },
                vec![leg(0x11)],
            ),
            (
                B256::repeat_byte(0x03),
                ReceivedKind::Received,
                vec![leg(0x11), leg(0x22)],
            ),
        ]
    );
}

#[test]
fn private_entries_confirm_only_while_their_inputs_are_spent() {
    let input = ActivityInput {
        tree: 0,
        position: 7,
    };
    let running = BTreeSet::new();
    // 0xcc belongs to another local action.
    let owned = HashSet::from([B256::repeat_byte(0xcc)]);
    let status = |entry: &ActivityEntry, utxos: &[UtxoOutput]| {
        entry_status(
            entry,
            &StatusInputs {
                utxos,
                running: &running,
                owned: &owned,
            },
        )
    };
    let unspent = received_note(7, 0x01, 100, ActivityUtxoClassification::PrivateOutput);
    let mut spent = unspent.clone();
    spend(&mut spent, 0xaa, 20);
    // An inclusion recorded in session doesn't confirm a private entry by itself.
    let sent = activity_entry(
        ActivityKind::PrivateSend,
        ActivityMatch::Inputs(vec![input]),
        vec![activity_attempt(
            0xaa,
            ActivityStep::Main,
            Some(included(20, true)),
        )],
    );
    assert_eq!(
        status(&sent, std::slice::from_ref(&spent)),
        ActivityStatus::Confirmed(Some(ActivityConfirmation {
            hash: B256::repeat_byte(0xaa),
            block: 20,
            timestamp: None,
        }))
    );
    assert_eq!(
        status(&sent, std::slice::from_ref(&unspent)),
        ActivityStatus::Submitted
    );

    let reverted = activity_entry(
        ActivityKind::PrivateSend,
        ActivityMatch::Inputs(vec![input]),
        vec![activity_attempt(
            0xab,
            ActivityStep::Main,
            Some(included(21, false)),
        )],
    );
    assert_eq!(
        status(&reverted, std::slice::from_ref(&unspent)),
        ActivityStatus::Failed { fee_spent: true }
    );
    // A later foreign spend doesn't overturn the recorded revert.
    let mut spent_elsewhere = unspent.clone();
    spend(&mut spent_elsewhere, 0xcd, 22);
    assert_eq!(
        status(&reverted, std::slice::from_ref(&spent_elsewhere)),
        ActivityStatus::Failed { fee_spent: true }
    );

    // Another local action's spend doesn't confirm a timed-out relay.
    let mut timed_out = activity_entry(
        ActivityKind::PrivateSend,
        ActivityMatch::Inputs(vec![input]),
        Vec::new(),
    );
    timed_out.relay_timed_out = true;
    let mut spent_by_owned = unspent;
    spend(&mut spent_by_owned, 0xcc, 23);
    assert_eq!(
        status(&timed_out, std::slice::from_ref(&spent_by_owned)),
        ActivityStatus::Unconfirmed
    );

    // Inputs spent by different transactions weren't spent by one action.
    let second = ActivityInput {
        tree: 0,
        position: 8,
    };
    let split = activity_entry(
        ActivityKind::PrivateSend,
        ActivityMatch::Inputs(vec![input, second]),
        vec![activity_attempt(
            0xaa,
            ActivityStep::Main,
            Some(included(20, true)),
        )],
    );
    let mut other = received_note(8, 0x01, 100, ActivityUtxoClassification::PrivateOutput);
    spend(&mut other, 0xcd, 22);
    assert_eq!(status(&split, &[spent, other]), ActivityStatus::Submitted);
}

#[test]
fn hash_matched_entries_follow_their_final_step_only() {
    let mut running = BTreeSet::new();
    let status = |entry: &ActivityEntry, running: &BTreeSet<ActivityEntryId>| {
        let inputs = StatusInputs {
            utxos: &[],
            running,
            owned: &HashSet::new(),
        };
        (entry_status(entry, &inputs), is_in_flight(entry, &inputs))
    };

    // A successful approval never confirms a shield, which is incomplete once not running.
    let approved = activity_entry(
        ActivityKind::Shield,
        ActivityMatch::Hashes,
        vec![activity_attempt(
            0x01,
            ActivityStep::Approve,
            Some(included(10, true)),
        )],
    );
    running.insert(approved.id);
    assert_eq!(
        status(&approved, &running),
        (ActivityStatus::Submitted, true)
    );
    running.clear();
    assert_eq!(
        status(&approved, &running),
        (ActivityStatus::Incomplete, false)
    );

    let retried = activity_entry(
        ActivityKind::PublicSend,
        ActivityMatch::Hashes,
        vec![
            activity_attempt(0x02, ActivityStep::Main, Some(included(11, false))),
            activity_attempt(0x03, ActivityStep::Main, Some(included(12, true))),
        ],
    );
    assert_eq!(
        status(&retried, &running).0,
        ActivityStatus::Confirmed(Some(ActivityConfirmation {
            hash: B256::repeat_byte(0x03),
            block: 12,
            timestamp: Some(120),
        }))
    );

    let dropped = activity_entry(
        ActivityKind::PublicSend,
        ActivityMatch::Hashes,
        vec![activity_attempt(
            0x04,
            ActivityStep::Main,
            Some(ActivityAttemptOutcome::Dropped),
        )],
    );
    assert_eq!(
        status(&dropped, &running).0,
        ActivityStatus::Failed { fee_spent: false }
    );

    let unobserved = activity_entry(
        ActivityKind::PublicSend,
        ActivityMatch::Hashes,
        vec![activity_attempt(0x05, ActivityStep::Main, None)],
    );
    assert_eq!(
        status(&unobserved, &running),
        (ActivityStatus::Submitted, false)
    );
}

#[test]
fn a_shield_confirms_from_its_note_in_the_viewing_wallet() {
    let shield = activity_entry(
        ActivityKind::Shield,
        ActivityMatch::Hashes,
        vec![
            activity_attempt(0x01, ActivityStep::Approve, Some(included(10, true))),
            activity_attempt(0x02, ActivityStep::Main, None),
        ],
    );
    let running = BTreeSet::new();
    let utxos = [received_note(
        0,
        0x02,
        11,
        ActivityUtxoClassification::Shield,
    )];
    assert_eq!(
        entry_status(
            &shield,
            &StatusInputs {
                utxos: &utxos,
                running: &running,
                owned: &HashSet::new(),
            },
        ),
        ActivityStatus::Confirmed(Some(ActivityConfirmation {
            hash: B256::repeat_byte(0x02),
            block: 11,
            timestamp: Some(11),
        }))
    );
}

#[test]
fn counterparties_resolve_to_their_current_address_book_label() {
    let viewing = ViewingKeyData::from_spending_public_key([1; 32], [U256::from(1), U256::from(2)]);
    let saved = viewing.derive_address(None).unwrap().to_string();
    // Entered with a chain, it still names the saved recipient's master public key.
    let entered = viewing.derive_address(Some((0, 1))).unwrap().to_string();
    let mut private = vec![PrivateAddressBookEntry {
        entry_uuid: "alice".to_owned(),
        label: "Alice".to_owned(),
        address: saved,
        display_order: 0,
    }];
    let tron = Address::repeat_byte(0x33);
    let non_evm = [NonEvmAddressBookEntry {
        entry_uuid: "tron".to_owned(),
        label: "Exchange".to_owned(),
        network: NonEvmNetwork::Tron,
        address: encode_tron_address(tron),
        display_order: 0,
    }];
    let resolve = |counterparty: &ActivityCounterparty, private: &[PrivateAddressBookEntry]| {
        resolve_counterparty(counterparty, private, &[], &non_evm).map(str::to_owned)
    };
    let recipient = ActivityCounterparty::Private(entered);

    assert_eq!(resolve(&recipient, &private).as_deref(), Some("Alice"));
    private[0].label = "Alicia".to_owned();
    assert_eq!(resolve(&recipient, &private).as_deref(), Some("Alicia"));
    assert_eq!(
        resolve(
            &ActivityCounterparty::Private(sample_railgun_address(2)),
            &private
        ),
        None
    );
    assert_eq!(
        resolve(&ActivityCounterparty::Tron(tron), &private).as_deref(),
        Some("Exchange")
    );
}

#[test]
fn a_swap_names_its_external_receiver_and_a_private_bridge_names_this_wallet() {
    use crate::activity::swap_recipient;

    let receiver = Address::repeat_byte(0x44);
    let bridge = |private| {
        SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: 8453,
            receiver,
            destination_token: Address::repeat_byte(10),
            surplus: BridgeSurplus::KeepInAccount,
            private,
        })
    };
    assert_eq!(swap_recipient(1, &SwapDelivery::Reshield), None);
    assert_eq!(
        swap_recipient(1, &SwapDelivery::External { receiver }),
        Some(SwapRowRecipient::Evm {
            address: receiver,
            chain_id: 1,
        })
    );
    assert_eq!(
        swap_recipient(1, &bridge(None)),
        Some(SwapRowRecipient::Evm {
            address: receiver,
            chain_id: 8453,
        })
    );
    // A private Bridge's receiver is the wallet's own destination account, never shown.
    assert_eq!(
        swap_recipient(
            1,
            &bridge(Some(BridgePrivateDelivery {
                on_shield_failure: BridgeShieldFailure::KeepOnDestination,
            }))
        ),
        Some(SwapRowRecipient::ThisWallet { chain_id: 8453 })
    );
}
