//! Notes that stealth-account operations keep out of private spending, and the user's
//! explicit check or release of one operation's reservation.
//!
//! Listing locks reads only local records. A check reads the executor account on chain,
//! so it runs only when the user asks for it, one operation at a time.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, U256};
use eyre::{Result, eyre};
use railgun_wallet::Utxo;

use super::ExecutorOwner;
use crate::WalletSession;
use crate::vault::{
    ExecutorOperationId, ExecutorPayloadPurpose, ExecutorPayloadStatus, ExecutorRecord,
    IssuedExecutorPayload,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorInputLockKind {
    SwapSetup,
    SwapOrder,
    Recovery,
    Operation,
}

/// Why an operation still reserves notes, in the order a lock reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorInputLockReason {
    /// No usable confirmed observation is retained, or an explicit check failed.
    NeedsChainCheck,
    /// A swap order's pre-hook can still run while the order is open. `valid_to` is in Unix
    /// seconds.
    OrderOpen { valid_to: u32 },
    /// No inclusion was observed, so the signed payload can still execute.
    SignedNotConfirmed,
    /// The payload executed, and private sync has not yet recorded its notes as spent.
    SpentAwaitingSync,
}

impl ExecutorInputLockReason {
    const fn rank(self) -> u8 {
        match self {
            Self::NeedsChainCheck => 0,
            Self::OrderOpen { .. } => 1,
            Self::SignedNotConfirmed => 2,
            Self::SpentAwaitingSync => 3,
        }
    }
}

/// A set of notes by count and per-token total, without the notes themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockedNotes {
    count: usize,
    amounts: BTreeMap<Address, U256>,
}

impl LockedNotes {
    fn of<'a>(notes: impl IntoIterator<Item = &'a Utxo>) -> Self {
        let mut locked = Self::default();
        for note in notes {
            locked.count += 1;
            let total = locked.amounts.entry(note.token_address()).or_default();
            *total = total.saturating_add(note.note.value);
        }
        locked
    }

    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Total value per token.
    pub fn amounts(&self) -> impl Iterator<Item = (Address, U256)> {
        self.amounts.iter().map(|(token, amount)| (*token, *amount))
    }
}

/// One operation's reservation of notes that are otherwise spendable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorInputLock {
    operation: ExecutorOperationId,
    address: Option<Address>,
    kind: ExecutorInputLockKind,
    reason: ExecutorInputLockReason,
    notes: LockedNotes,
}

impl ExecutorInputLock {
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
    #[must_use]
    pub const fn address(&self) -> Option<Address> {
        self.address
    }
    #[must_use]
    pub const fn kind(&self) -> ExecutorInputLockKind {
        self.kind
    }
    #[must_use]
    pub const fn reason(&self) -> ExecutorInputLockReason {
        self.reason
    }
    #[must_use]
    pub const fn notes(&self) -> &LockedNotes {
        &self.notes
    }
}

/// Every lock that keeps POI-valid, unspent notes out of private spending.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalletNoteLocks {
    executor: Vec<ExecutorInputLock>,
    local_pending: LockedNotes,
    chain_pending: LockedNotes,
}

impl WalletNoteLocks {
    /// Stealth-account operation reservations, one per operation.
    #[must_use]
    pub fn executor(&self) -> &[ExecutorInputLock] {
        &self.executor
    }
    /// Notes this wallet protects while its own submission is pending. They can be cleared.
    #[must_use]
    pub const fn local_pending(&self) -> &LockedNotes {
        &self.local_pending
    }
    /// Notes a pending chain transaction spends.
    #[must_use]
    pub const fn chain_pending(&self) -> &LockedNotes {
        &self.chain_pending
    }
    #[must_use]
    pub fn executor_note_count(&self) -> usize {
        self.executor.iter().map(|lock| lock.notes.count).sum()
    }
}

impl ExecutorOwner {
    /// Locks on `notes`, the POI-valid notes left after pending-spend filtering. Only
    /// unreleased reserving payloads count, and an operation without a matching note has
    /// no lock. This reads local records only.
    pub fn input_locks(&self, notes: &[Utxo]) -> Result<Vec<ExecutorInputLock>> {
        Ok(self
            .records()?
            .iter()
            .filter_map(|record| input_lock(record, notes))
            .collect())
    }

    /// The user's explicit release of every payload that currently reserves notes for
    /// `operation`. The signed payloads can still execute. Payloads the wallet issues or
    /// sends again afterwards reserve their inputs again.
    pub fn release_input_lock(&self, operation: ExecutorOperationId) -> Result<()> {
        let record = self
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
            .ok_or_else(|| eyre!("stealth account is unavailable"))?;
        let payloads = record
            .reserving_payloads()
            .map(IssuedExecutorPayload::hash)
            .collect::<Vec<_>>();
        if payloads.is_empty() {
            return Ok(());
        }
        self.store.release_payloads(operation, &payloads)?;
        self.notify_change();
        Ok(())
    }

    /// Check one operation's executor account at the `confirmed` block, as the user asked,
    /// and return its lock on `notes` afterwards. A swap's orders are observed too, so an
    /// expired order's pre-hook can stop reserving its notes.
    pub async fn check_input_lock(
        &self,
        operation: ExecutorOperationId,
        confirmed: u64,
        notes: &[Utxo],
    ) -> Result<Option<ExecutorInputLock>> {
        let record = self
            .records()?
            .into_iter()
            .find(|record| record.operation() == operation)
            .ok_or_else(|| eyre!("stealth account is unavailable"))?;
        let range = confirmed..confirmed.saturating_add(1);
        let report = if record.swap().is_some() {
            self.observe_swap(operation, range).await?
        } else {
            self.reconcile_history(operation, range).await?
        };
        Ok(input_lock(report.record(), notes))
    }
}

impl WalletSession {
    /// Executor, local pending-submission and chain pending-spend locks on this wallet's
    /// POI-valid, unspent notes. This reads local state only.
    pub fn note_locks(&self) -> Result<WalletNoteLocks> {
        let snapshot = self
            .handle
            .current_snapshot()
            .ok_or_else(|| eyre!("private wallet snapshot is unavailable"))?;
        let overlay = &snapshot.pending_overlay;
        let chain_pending = super::super::public_broadcaster::chain_pending_spent_keys(overlay);
        let poi_lists = poi::poi::default_active_poi_list_keys();
        let mut local_pending = Vec::new();
        let mut chain_pending_notes = Vec::new();
        for entry in snapshot
            .utxos
            .iter()
            .filter(|entry| !entry.is_spent() && entry.utxo.poi.is_valid_for_lists(&poi_lists))
        {
            if overlay
                .local_pending_spent
                .iter()
                .any(|spent| spent.matches_local_utxo(entry))
            {
                local_pending.push(&entry.utxo);
            } else if chain_pending.contains(&(entry.utxo.tree, entry.utxo.position)) {
                chain_pending_notes.push(&entry.utxo);
            }
        }
        let executor = match self.executor_owner() {
            Some(owner) => owner.input_locks(&crate::poi_verified_unspent_utxos_from_records(
                &snapshot.utxos,
                overlay,
            ))?,
            None => Vec::new(),
        };
        Ok(WalletNoteLocks {
            executor,
            local_pending: LockedNotes::of(local_pending),
            chain_pending: LockedNotes::of(chain_pending_notes),
        })
    }

    /// Total value of `token`'s POI-valid, unspent notes that any lock keeps out of
    /// private spending.
    #[must_use]
    pub fn locked_note_value(&self, token: Address) -> U256 {
        let Some(snapshot) = self.handle.current_snapshot() else {
            return U256::ZERO;
        };
        let inputs = crate::poi_verified_unspent_utxos_from_records(
            &snapshot.utxos,
            &snapshot.pending_overlay,
        );
        // As for spending, unavailable reservations leave nothing spendable.
        let available = match self.executor_owner() {
            Some(owner) => owner.available_inputs(inputs).unwrap_or_default(),
            None => inputs,
        };
        let available = available
            .iter()
            .map(|note| (note.tree, note.position))
            .collect::<BTreeSet<_>>();
        let poi_lists = poi::poi::default_active_poi_list_keys();
        snapshot
            .utxos
            .iter()
            .filter(|entry| {
                !entry.is_spent()
                    && entry.utxo.token_address() == token
                    && entry.utxo.poi.is_valid_for_lists(&poi_lists)
                    && !available.contains(&(entry.utxo.tree, entry.utxo.position))
            })
            .fold(U256::ZERO, |total, entry| {
                total.saturating_add(entry.utxo.note.value)
            })
    }

    /// Check one operation's reservation against the chain, as the user asked. See
    /// [`ExecutorOwner::check_input_lock`].
    pub async fn check_input_lock(
        &self,
        operation: ExecutorOperationId,
        confirmed: u64,
    ) -> Result<Option<ExecutorInputLock>> {
        let owner = self
            .executor_owner()
            .ok_or_else(|| eyre!("executor wallet ownership is unavailable"))?;
        let notes = self
            .handle
            .current_snapshot()
            .map_or_else(Vec::new, |snapshot| {
                crate::poi_verified_unspent_utxos_from_records(
                    &snapshot.utxos,
                    &snapshot.pending_overlay,
                )
            });
        owner.check_input_lock(operation, confirmed, &notes).await
    }
}

fn input_lock(record: &ExecutorRecord, notes: &[Utxo]) -> Option<ExecutorInputLock> {
    let mut chosen: Option<(ExecutorInputLockReason, ExecutorInputLockKind)> = None;
    let mut matched: Vec<&Utxo> = Vec::new();
    for payload in record.reserving_payloads() {
        let mut matches_payload = false;
        for note in notes.iter().filter(|note| {
            payload
                .context()
                .inputs()
                .iter()
                .any(|input| input.matches(note))
        }) {
            matches_payload = true;
            if !matched
                .iter()
                .any(|seen| (seen.tree, seen.position) == (note.tree, note.position))
            {
                matched.push(note);
            }
        }
        if !matches_payload {
            continue;
        }
        let reason = payload_reason(record, payload);
        if chosen.is_none_or(|(current, _)| reason.rank() < current.rank()) {
            chosen = Some((reason, payload_kind(record, payload)));
        }
    }
    let (reason, kind) = chosen?;
    Some(ExecutorInputLock {
        operation: record.operation(),
        address: record.address(),
        kind,
        reason,
        notes: LockedNotes::of(matched),
    })
}

fn payload_reason(
    record: &ExecutorRecord,
    payload: &IssuedExecutorPayload,
) -> ExecutorInputLockReason {
    if record.nonce_observation().is_none() {
        return ExecutorInputLockReason::NeedsChainCheck;
    }
    // A reserving pre-hook's order has not ended; it runs inside a settlement.
    let order = (payload.purpose() == ExecutorPayloadPurpose::SwapPreHook)
        .then(|| {
            record.swap().and_then(|swap| {
                swap.orders()
                    .iter()
                    .find(|order| order.pre_hook().payload() == payload.hash())
            })
        })
        .flatten();
    if let Some(order) = order {
        return if order.observations().pre_hook_executed.is_some() {
            ExecutorInputLockReason::SpentAwaitingSync
        } else {
            ExecutorInputLockReason::OrderOpen {
                valid_to: order.valid_to(),
            }
        };
    }
    if record.payload_status(payload.hash()) == Some(ExecutorPayloadStatus::Executed) {
        ExecutorInputLockReason::SpentAwaitingSync
    } else {
        ExecutorInputLockReason::SignedNotConfirmed
    }
}

fn payload_kind(record: &ExecutorRecord, payload: &IssuedExecutorPayload) -> ExecutorInputLockKind {
    match payload.purpose() {
        ExecutorPayloadPurpose::SwapPreHook
        | ExecutorPayloadPurpose::SwapPostHook
        | ExecutorPayloadPurpose::SwapDestinationShield => ExecutorInputLockKind::SwapOrder,
        ExecutorPayloadPurpose::Recovery => ExecutorInputLockKind::Recovery,
        ExecutorPayloadPurpose::Operation if super::is_swap_record(record) => {
            ExecutorInputLockKind::SwapSetup
        }
        ExecutorPayloadPurpose::Operation => ExecutorInputLockKind::Operation,
    }
}
