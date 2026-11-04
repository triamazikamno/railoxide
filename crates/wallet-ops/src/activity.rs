//! Activity derived at render time from data the wallet already holds: received rows from the
//! UTXO snapshot, journal entry status, and counterparty labels. Nothing here sends a request.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use alloy::primitives::{Address, B256, U256};

use crate::amounts::parse_railgun_recipient;
use crate::bridge::NonEvmNetwork;
use crate::tron::address::parse_tron_address;
use crate::utxos::{ActivityUtxoClassification, UtxoOutput};
use crate::vault::{
    ActivityAsset, ActivityAttemptOutcome, ActivityCounterparty, ActivityDirection, ActivityEntry,
    ActivityEntryId, ActivityInput, ActivityKind, ActivityLeg, ActivityMatch, ActivityNetwork,
    ActivityStep, ExecutorPayloadPurpose, ExecutorRecord, IssuedExecutorPayload,
    NonEvmAddressBookEntry, PrivateAddressBookEntry, PublicAddressBookEntry, PublicSwapRecord,
    SwapDelivery, SwapDestinationOutcome, SwapOrderRecord, SwapUseId, SwapUseRecord, SwapUseRole,
};
use crate::{SwapOrderState, swap_attempt_state};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceivedKind {
    Received,
    ShieldReceived { blocked: bool },
}

/// One incoming transaction that created notes for the wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedRow {
    pub hash: B256,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub kind: ReceivedKind,
    /// One incoming leg per token.
    pub legs: Vec<ActivityLeg>,
}

/// Received rows of one chain's UTXO snapshot (spent notes included), newest first. A
/// transaction is skipped when it spent one of the snapshot's notes (the wallet's own change),
/// when `owned` holds its hash, or when its block timestamp is at or before `hidden_before`.
#[must_use]
pub fn received_rows(
    utxos: &[UtxoOutput],
    owned: &HashSet<B256>,
    hidden_before: u64,
) -> Vec<ReceivedRow> {
    let spent_in: HashSet<B256> = utxos
        .iter()
        .filter_map(|note| parse_hash(note.spent_tx_hash.as_deref()?))
        .collect();
    let mut rows = BTreeMap::new();
    for note in utxos {
        let Some(hash) = parse_hash(&note.source_tx_hash) else {
            continue;
        };
        if note.source_block_timestamp <= hidden_before
            || owned.contains(&hash)
            || spent_in.contains(&hash)
        {
            continue;
        }
        let (Ok(token), Ok(amount)) = (
            note.token.parse::<Address>(),
            U256::from_str_radix(&note.value, 10),
        ) else {
            continue;
        };
        let row = rows.entry(hash).or_insert_with(|| ReceivedRow {
            hash,
            block_number: note.source_block_number,
            block_timestamp: note.source_block_timestamp,
            kind: ReceivedKind::Received,
            legs: Vec::new(),
        });
        match classification(note) {
            Some(ActivityUtxoClassification::BlockedShield) => {
                row.kind = ReceivedKind::ShieldReceived { blocked: true };
            }
            Some(ActivityUtxoClassification::Shield) if row.kind == ReceivedKind::Received => {
                row.kind = ReceivedKind::ShieldReceived { blocked: false };
            }
            _ => {}
        }
        let asset = ActivityAsset::Token(token);
        match row.legs.iter_mut().find(|leg| leg.asset == asset) {
            Some(leg) => leg.amount = leg.amount.saturating_add(amount),
            None => row.legs.push(ActivityLeg {
                asset,
                amount,
                direction: ActivityDirection::In,
            }),
        }
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by_key(|row| Reverse(row.block_number));
    rows
}

/// Whether a shown received row is newer than the network's last-seen block. Without a marker
/// the first catch-up hasn't set one yet, so nothing is unread.
#[must_use]
pub fn has_unread_received(rows: &[ReceivedRow], last_seen: Option<u64>) -> bool {
    last_seen.is_some_and(|seen| rows.iter().any(|row| row.block_number > seen))
}

/// Transaction hashes whose notes belong to the wallet's own actions rather than received rows:
/// every attempt of `entries` (cleared stubs included), and from `records` the transactions of
/// issued payloads, swap destination outcomes and Public-paid swaps.
#[must_use]
pub fn owned_hashes(entries: &[ActivityEntry], records: &[ExecutorRecord]) -> HashSet<B256> {
    let mut owned: HashSet<B256> = entries
        .iter()
        .flat_map(|entry| entry.attempts.iter().map(|attempt| attempt.hash))
        .collect();
    for record in records {
        owned.extend(
            record
                .issued()
                .iter()
                .flat_map(|payload| payload.transaction_hashes().iter().copied()),
        );
        for swap_use in record.swap_uses() {
            let (outcome, swap) = match swap_use.role() {
                SwapUseRole::Destination { outcome, .. } => (outcome, None),
                SwapUseRole::PublicSourceDestination { outcome, swap, .. } => (outcome, Some(swap)),
                SwapUseRole::Source { .. } => continue,
            };
            if let Some(
                SwapDestinationOutcome::Shielded {
                    transaction_hash, ..
                }
                | SwapDestinationOutcome::Held {
                    transaction_hash, ..
                },
            ) = outcome
            {
                owned.insert(*transaction_hash);
            }
            if let Some(swap) = swap {
                owned.extend(
                    swap.transactions()
                        .iter()
                        .map(|transaction| transaction.hash),
                );
            }
        }
    }
    owned
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityConfirmation {
    pub hash: B256,
    pub block: u64,
    /// Unknown when confirmed by spent input notes, whose spend carries no block time.
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Submitted,
    /// A broadcaster timed out; spent inputs will confirm the entry if it landed.
    Unconfirmed,
    /// `None` where the confirming transaction isn't known locally, as for swaps.
    Confirmed(Option<ActivityConfirmation>),
    /// `fee_spent` when any attempt of the entry was included.
    Failed {
        fee_spent: bool,
    },
    Incomplete,
}

/// What status derivation reads besides the entry.
#[derive(Debug, Clone, Copy)]
pub struct StatusInputs<'a> {
    /// The viewing wallet's UTXO snapshot of the entry's chain, spent notes included.
    pub utxos: &'a [UtxoOutput],
    /// Entries whose submission is still running in this session.
    pub running: &'a BTreeSet<ActivityEntryId>,
    /// Transaction hashes of the wallet's own actions, from `owned_hashes`.
    pub owned: &'a HashSet<B256>,
}

/// The entry's status, following its final step only.
#[must_use]
pub fn entry_status(entry: &ActivityEntry, inputs: &StatusInputs<'_>) -> ActivityStatus {
    match &entry.match_key {
        ActivityMatch::Inputs(notes) => private_status(entry, notes, inputs.utxos, inputs.owned),
        ActivityMatch::Hashes => {
            if entry.kind == ActivityKind::Shield
                && let Some(confirmation) = shield_note(entry, inputs.utxos)
            {
                return ActivityStatus::Confirmed(Some(confirmation));
            }
            final_step_status(entry, inputs.running.contains(&entry.id))
        }
    }
}

/// A submission still running in this session.
#[must_use]
pub fn is_in_flight(entry: &ActivityEntry, inputs: &StatusInputs<'_>) -> bool {
    inputs.running.contains(&entry.id)
}

/// Success comes only from the inputs being spent in the snapshot, so a rewind un-confirms it.
/// A spend another local action owns, or a recorded failure contradicted by a foreign spend,
/// doesn't confirm.
fn private_status(
    entry: &ActivityEntry,
    inputs: &[ActivityInput],
    utxos: &[UtxoOutput],
    owned: &HashSet<B256>,
) -> ActivityStatus {
    let spent: Option<Vec<&UtxoOutput>> = inputs
        .iter()
        .map(|input| {
            utxos.iter().find(|note| {
                note.tree == input.tree && note.position == input.position && note.is_spent
            })
        })
        .collect();
    let recorded = final_step_status(entry, true);
    if let Some(spent) = spent.filter(|spent| !spent.is_empty()) {
        let known: Vec<(&UtxoOutput, B256)> = spent
            .into_iter()
            .filter_map(|note| Some((note, note.spent_tx_hash.as_deref().and_then(parse_hash)?)))
            .collect();
        let shared = known.first().copied();
        let consistent = known
            .iter()
            .all(|&(_, hash)| shared.is_some_and(|(_, first)| first == hash));
        let own_main = shared.is_some_and(|(_, hash)| {
            entry
                .attempts
                .iter()
                .any(|attempt| attempt.step == ActivityStep::Main && attempt.hash == hash)
        });
        let unclaimed = !matches!(recorded, ActivityStatus::Failed { .. })
            && shared.is_none_or(|(_, hash)| !owned.contains(&hash));
        if consistent && (own_main || unclaimed) {
            return ActivityStatus::Confirmed(shared.and_then(|(note, hash)| {
                note.spent_block_number.map(|block| ActivityConfirmation {
                    hash,
                    block,
                    timestamp: note.spent_block_timestamp,
                })
            }));
        }
    }
    match recorded {
        failed @ ActivityStatus::Failed { .. } => failed,
        _ if entry.relay_timed_out => ActivityStatus::Unconfirmed,
        _ => ActivityStatus::Submitted,
    }
}

/// A shield note in the viewing wallet created by one of the entry's final-step transactions.
fn shield_note(entry: &ActivityEntry, utxos: &[UtxoOutput]) -> Option<ActivityConfirmation> {
    utxos.iter().find_map(|note| {
        let hash = parse_hash(&note.source_tx_hash)?;
        let shield = matches!(
            classification(note),
            Some(ActivityUtxoClassification::Shield | ActivityUtxoClassification::BlockedShield)
        );
        let main = entry
            .attempts
            .iter()
            .any(|attempt| attempt.step == ActivityStep::Main && attempt.hash == hash);
        (shield && main).then_some(ActivityConfirmation {
            hash,
            block: note.source_block_number,
            timestamp: Some(note.source_block_timestamp),
        })
    })
}

/// Status from the recorded outcomes of the final step's attempts.
fn final_step_status(entry: &ActivityEntry, in_flight: bool) -> ActivityStatus {
    let mut main = entry
        .attempts
        .iter()
        .filter(|attempt| attempt.step == ActivityStep::Main);
    if let Some(confirmation) = main.clone().find_map(|attempt| match attempt.outcome {
        Some(ActivityAttemptOutcome::Included {
            block,
            timestamp,
            success: true,
        }) => Some(ActivityConfirmation {
            hash: attempt.hash,
            block,
            timestamp: Some(timestamp),
        }),
        _ => None,
    }) {
        return ActivityStatus::Confirmed(Some(confirmation));
    }
    match main.next_back().map(|attempt| attempt.outcome) {
        None if !in_flight => ActivityStatus::Incomplete,
        Some(Some(
            ActivityAttemptOutcome::Included { success: false, .. }
            | ActivityAttemptOutcome::Dropped,
        )) => ActivityStatus::Failed {
            fee_spent: fee_spent(entry),
        },
        _ => ActivityStatus::Submitted,
    }
}

fn fee_spent(entry: &ActivityEntry) -> bool {
    entry.attempts.iter().any(|attempt| {
        matches!(
            attempt.outcome,
            Some(ActivityAttemptOutcome::Included { .. })
        )
    })
}

/// One swap use the wallet broadcast something for, derived from its executor records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapRow {
    pub id: SwapUseId,
    /// The network that paid for the swap.
    pub network: ActivityNetwork,
    /// Local start time, for ordering and age clearing.
    pub started_at: u64,
    pub legs: Vec<ActivityLeg>,
    /// The setup's transactions and the settlements of its attempts, or for a Public-paid swap
    /// the transactions the account sent.
    pub hashes: Vec<B256>,
    pub status: ActivityStatus,
    /// The use isn't resolved yet.
    pub in_flight: bool,
    /// Who the swap delivers to. `None` when it lands back in this wallet on the network that
    /// paid.
    pub recipient: Option<SwapRowRecipient>,
}

/// Who a swap row delivers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapRowRecipient {
    /// An EVM address on `chain_id`.
    Evm { address: Address, chain_id: u64 },
    /// A receiver on a non-EVM network, in its canonical form.
    NonEvm {
        network: NonEvmNetwork,
        address: String,
    },
    /// This wallet, on another chain than the one that paid.
    ThisWallet { chain_id: u64 },
}

/// The swap rows of `records`, read from `chain_id`'s store: one per use that broadcast a setup
/// with a transaction hash or has an order. A private Bridge swap's destination use has none,
/// so the swap shows once, on its origin network.
#[must_use]
pub fn swap_rows(chain_id: u64, records: &[ExecutorRecord]) -> Vec<SwapRow> {
    records
        .iter()
        .flat_map(|record| {
            record
                .swap_uses()
                .iter()
                .filter_map(move |swap_use| swap_row(chain_id, record, swap_use))
        })
        .collect()
}

fn swap_row(chain_id: u64, record: &ExecutorRecord, swap_use: &SwapUseRecord) -> Option<SwapRow> {
    let (chain_id, legs, hashes, status, recipient) = match swap_use.role() {
        SwapUseRole::Source { .. } => {
            let orders: Vec<&SwapOrderRecord> = record.swap_use_orders(swap_use.id()).collect();
            let mut hashes: Vec<B256> = setup_payloads(record, swap_use)
                .flat_map(|payload| payload.transaction_hashes().iter().copied())
                .collect();
            if hashes.is_empty() && orders.is_empty() {
                return None;
            }
            hashes.extend(
                orders
                    .iter()
                    .filter_map(|order| order.observations().traded?.transaction_hash),
            );
            let (legs, delivery) = source_legs(record, swap_use, &orders);
            (
                chain_id,
                legs,
                hashes,
                source_swap_status(record, swap_use),
                delivery.and_then(|delivery| swap_recipient(chain_id, &delivery)),
            )
        }
        SwapUseRole::PublicSourceDestination {
            origin_chain, swap, ..
        } => {
            if swap.transactions().is_empty() && swap.order().is_none() {
                return None;
            }
            let approval = swap.approval();
            (
                *origin_chain,
                vec![swap_leg(
                    approval.sell_token,
                    approval.bounds.sell_amount,
                    ActivityDirection::Out,
                )],
                swap.transactions()
                    .iter()
                    .map(|transaction| transaction.hash)
                    .collect(),
                public_swap_status(swap_use, swap),
                // It shields into this wallet on the record's chain.
                (chain_id != *origin_chain).then_some(SwapRowRecipient::ThisWallet { chain_id }),
            )
        }
        SwapUseRole::Destination { .. } => return None,
    };
    Some(SwapRow {
        id: swap_use.id(),
        network: ActivityNetwork::Evm(chain_id),
        started_at: swap_use
            .started_at()
            .or_else(|| record.created_at())
            .unwrap_or(0),
        legs,
        hashes,
        in_flight: status == ActivityStatus::Submitted,
        status,
        recipient,
    })
}

/// The setups the use signed. Setups belong to the use that reserved the account fresh.
fn setup_payloads<'a>(
    record: &'a ExecutorRecord,
    swap_use: &SwapUseRecord,
) -> impl Iterator<Item = &'a IssuedExecutorPayload> {
    let fresh = swap_use.is_fresh();
    record
        .issued()
        .iter()
        .filter(move |payload| fresh && payload.purpose() == ExecutorPayloadPurpose::Operation)
}

/// What the use sells and, for a Reshield, gets back, and its delivery: from its latest order,
/// or before any from its approval. The bought amount is what was shielded, else the approved
/// private minimum.
fn source_legs(
    record: &ExecutorRecord,
    swap_use: &SwapUseRecord,
    orders: &[&SwapOrderRecord],
) -> (Vec<ActivityLeg>, Option<SwapDelivery>) {
    let (tokens, bounds, delivery) = match (orders.last(), record.swap(), swap_use.approval()) {
        (Some(order), Some(swap), _) => {
            let terms = swap.order_terms(order);
            (
                Some((terms.sell_token(), terms.buy_token())),
                order.bounds(),
                order.delivery(),
            )
        }
        (None, _, Some(approval)) => (
            approval.tokens.map(|tokens| (tokens.sell, tokens.buy)),
            &approval.bounds,
            approval.delivery.clone(),
        ),
        _ => return (Vec::new(), None),
    };
    let Some((sell, buy)) = tokens else {
        return (Vec::new(), Some(delivery));
    };
    let mut legs = vec![swap_leg(
        sell,
        bounds.spend_amount(),
        ActivityDirection::Out,
    )];
    // Only a Reshield delivers the bought token to this wallet.
    if matches!(delivery, SwapDelivery::Reshield) {
        let received = orders
            .iter()
            .find_map(|order| order.observations().shielded)
            .map_or(bounds.private_minimum, |shielded| shielded.private_amount);
        legs.push(swap_leg(buy, received, ActivityDirection::In));
    }
    (legs, Some(delivery))
}

/// Who `delivery` pays, for a swap paid on `chain_id`. A Reshield pays this wallet there.
pub(crate) fn swap_recipient(chain_id: u64, delivery: &SwapDelivery) -> Option<SwapRowRecipient> {
    match delivery {
        SwapDelivery::Reshield => None,
        SwapDelivery::External { receiver } => Some(SwapRowRecipient::Evm {
            address: *receiver,
            chain_id,
        }),
        // A private Bridge's receiver is the wallet's own destination account.
        SwapDelivery::Bridge(bridge) if bridge.is_private() => Some(SwapRowRecipient::ThisWallet {
            chain_id: bridge.destination_chain,
        }),
        SwapDelivery::Bridge(bridge) => Some(SwapRowRecipient::Evm {
            address: bridge.receiver,
            chain_id: bridge.destination_chain,
        }),
        SwapDelivery::NonEvmBridge(bridge) => Some(SwapRowRecipient::NonEvm {
            network: bridge.network,
            address: bridge.receiver.clone(),
        }),
    }
}

// Swap approvals name a native asset `Address::ZERO`.
fn swap_leg(token: Address, amount: U256, direction: ActivityDirection) -> ActivityLeg {
    ActivityLeg {
        asset: if token == Address::ZERO {
            ActivityAsset::Native
        } else {
            ActivityAsset::Token(token)
        },
        amount,
        direction,
    }
}

/// A delivered attempt confirms the use. Funds left to recover make it incomplete, an attempt
/// that can still progress keeps it submitted, and attempts that all ended fail it, with the
/// fee spent once a setup's nonce was consumed.
fn source_swap_status(record: &ExecutorRecord, swap_use: &SwapUseRecord) -> ActivityStatus {
    let states: Vec<SwapOrderState> = record
        .swap_use_orders(swap_use.id())
        .map(|order| swap_attempt_state(record, order))
        .collect();
    if states.is_empty() {
        return if swap_use.is_stopped() {
            ActivityStatus::Incomplete
        } else {
            ActivityStatus::Submitted
        };
    }
    if states.contains(&SwapOrderState::Done) {
        return ActivityStatus::Confirmed(None);
    }
    if states.iter().any(|state| {
        matches!(
            state,
            SwapOrderState::NotDelivered
                | SwapOrderState::Refunding
                | SwapOrderState::HeldOnDestination
                | SwapOrderState::NeedsAttention
                | SwapOrderState::PreHookOnly { expired: true }
        )
    }) {
        return ActivityStatus::Incomplete;
    }
    if states.iter().any(|state| {
        matches!(
            state,
            SwapOrderState::Open
                | SwapOrderState::Traded
                | SwapOrderState::Bridging
                | SwapOrderState::PreHookOnly { expired: false }
        )
    }) {
        return ActivityStatus::Submitted;
    }
    ActivityStatus::Failed {
        fee_spent: setup_payloads(record, swap_use)
            .any(|payload| record.nonce_resolved(payload.nonce())),
    }
}

/// A Public-paid swap, read in the order of its presentation stage. Its fee was spent once any
/// of its transactions was included.
fn public_swap_status(swap_use: &SwapUseRecord, swap: &PublicSwapRecord) -> ActivityStatus {
    let observed = swap.observations();
    if observed.bridge_handoff.is_some() {
        return match observed.bridge_outcome {
            None => ActivityStatus::Submitted,
            Some(outcome) if outcome.is_delivered() => ActivityStatus::Confirmed(None),
            Some(_) => ActivityStatus::Incomplete,
        };
    }
    if observed.withdrawn.is_some() || observed.held_by_proxy.is_some() {
        return ActivityStatus::Incomplete;
    }
    if observed.traded.is_none() && (observed.cancelled.is_some() || observed.expired.is_some()) {
        return ActivityStatus::Failed {
            fee_spent: swap
                .transactions()
                .iter()
                .any(|transaction| transaction.inclusion.is_some()),
        };
    }
    if swap.order().is_some() {
        return ActivityStatus::Submitted;
    }
    let mut deposits = swap.live_deposits().peekable();
    if deposits.peek().is_some() {
        return if deposits.all(|deposit| {
            deposit
                .inclusion
                .is_some_and(|included| included.finalized && !included.succeeded)
        }) {
            ActivityStatus::Failed { fee_spent: true }
        } else {
            ActivityStatus::Submitted
        };
    }
    if swap_use.is_stopped() {
        ActivityStatus::Incomplete
    } else {
        ActivityStatus::Submitted
    }
}

/// The address-book label for `counterparty`. Private addresses match by master public key.
#[must_use]
pub fn resolve_counterparty<'a>(
    counterparty: &ActivityCounterparty,
    private: &'a [PrivateAddressBookEntry],
    public: &'a [PublicAddressBookEntry],
    non_evm: &'a [NonEvmAddressBookEntry],
) -> Option<&'a str> {
    match counterparty {
        ActivityCounterparty::Private(address) => {
            resolve_private_mpk(master_public_key(address)?, private)
        }
        ActivityCounterparty::Evm(address) => public
            .iter()
            .find(|entry| entry.address == *address)
            .map(|entry| entry.label.as_str()),
        ActivityCounterparty::Tron(address) => non_evm
            .iter()
            .find(|entry| {
                entry.network == NonEvmNetwork::Tron
                    && parse_tron_address(&entry.address).is_ok_and(|parsed| parsed == *address)
            })
            .map(|entry| entry.label.as_str()),
    }
}

/// Also the lookup for a received note's sender, which yields only its master public key.
fn resolve_private_mpk(mpk: U256, private: &[PrivateAddressBookEntry]) -> Option<&str> {
    private
        .iter()
        .find(|entry| master_public_key(&entry.address) == Some(mpk))
        .map(|entry| entry.label.as_str())
}

fn master_public_key(address: &str) -> Option<U256> {
    parse_railgun_recipient(address)
        .ok()
        .map(|data| data.master_public_key)
}

fn classification(note: &UtxoOutput) -> Option<ActivityUtxoClassification> {
    [
        ActivityUtxoClassification::Shield,
        ActivityUtxoClassification::BlockedShield,
        ActivityUtxoClassification::PrivateOutput,
    ]
    .into_iter()
    .find(|classification| classification.label() == note.activity_classification)
}

fn parse_hash(text: &str) -> Option<B256> {
    text.parse().ok()
}
