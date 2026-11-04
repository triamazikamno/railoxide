//! Journals the wallet's submissions at their handoff, inside wallet-ops, so an entry outlives
//! the view that started it. A journal failure is logged and signalled for a non-blocking
//! notice; it never fails or blocks a submission.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, U256};
use railgun_wallet::Utxo;
use tokio::sync::watch;

use super::PublicBroadcasterResultKind;
use crate::vault::{
    ActivityAsset, ActivityAttempt, ActivityAttemptOutcome, ActivityCounterparty,
    ActivityDirection, ActivityEntry, ActivityEntryId, ActivityFee, ActivityInput, ActivityJournal,
    ActivityKind, ActivityLeg, ActivityMatch, ActivityNetwork, ActivityOwner, ActivitySource,
    ActivityStep, ActivityStoreError, DesktopVaultStore, DesktopViewSession, ExecutorOperationId,
    ExecutorRecord,
};

static RUNNING: Mutex<BTreeSet<ActivityEntryId>> = Mutex::new(BTreeSet::new());
/// Attempts journaled in this process, by hash, whose inclusion the block observer persists.
static OBSERVED: Mutex<BTreeMap<B256, ObservedAttempt>> = Mutex::new(BTreeMap::new());
/// Counts failed journal writes in this process.
static JOURNAL_FAILURES: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::Sender::new(0));

/// Entries whose submission is still running in this process.
#[must_use]
pub fn running_activity_entries() -> BTreeSet<ActivityEntryId> {
    running().clone()
}

/// Changes whenever this process fails to journal a submission, so the UI can say Activity
/// may be incomplete.
#[must_use]
pub fn subscribe_activity_journal_failures() -> watch::Receiver<u64> {
    JOURNAL_FAILURES.subscribe()
}

/// Logs a failed journal `step` without the error, whose text can carry record data, and
/// signals it.
fn journal_failed(step: &'static str, message: &'static str) {
    tracing::warn!(step, "{message}");
    JOURNAL_FAILURES.send_modify(|count| *count = count.wrapping_add(1));
}

fn running() -> MutexGuard<'static, BTreeSet<ActivityEntryId>> {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

fn observed() -> MutexGuard<'static, BTreeMap<B256, ObservedAttempt>> {
    OBSERVED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a journaled attempt lives. The view is held weakly so observation never keeps a
/// closed wallet's keys alive; an attempt seen after that stays unobserved.
#[derive(Clone)]
struct ObservedAttempt {
    db: Arc<local_db::DbStore>,
    view: Weak<DesktopViewSession>,
    owner: ActivityOwner,
    id: ActivityEntryId,
    /// The block of the inclusion this process recorded.
    included: Option<u64>,
}

impl ObservedAttempt {
    fn write(&self, hash: B256, outcome: Option<ActivityAttemptOutcome>) {
        let Some(view) = self.view.upgrade() else {
            return;
        };
        match ActivityJournal::new(Arc::clone(&self.db), view) {
            Ok(journal) => write_attempt_outcome(&journal, &self.owner, self.id, hash, outcome),
            Err(_) => journal_failed("outcome", "activity journal write failed"),
        }
    }
}

/// Sets the outcome of entry `id`'s attempt `hash`; `None` clears a rolled-back inclusion.
fn write_attempt_outcome(
    journal: &ActivityJournal,
    owner: &ActivityOwner,
    id: ActivityEntryId,
    hash: B256,
    outcome: Option<ActivityAttemptOutcome>,
) {
    let result = journal.update(owner, id, |entry| {
        for attempt in &mut entry.attempts {
            if attempt.hash == hash {
                attempt.outcome = outcome;
            }
        }
    });
    match result {
        // History clearing can delete an entry while its submission runs or its attempts are
        // still observed.
        Ok(_) | Err(ActivityStoreError::NotFound) => {}
        Err(_) => journal_failed("outcome", "activity journal write failed"),
    }
}

/// Persists the block observer's receipt of `hash` into its journaled attempt, if any.
pub(crate) fn observe_activity_inclusion(hash: B256, block: u64, timestamp: u64, success: bool) {
    let attempt = {
        let mut observed = observed();
        let Some(attempt) = observed.get_mut(&hash) else {
            return;
        };
        attempt.included = Some(block);
        attempt.clone()
    };
    attempt.write(
        hash,
        Some(ActivityAttemptOutcome::Included {
            block,
            timestamp,
            success,
        }),
    );
}

/// Clears the recorded inclusions of `hashes` above `ancestor`, the block a running observer
/// rolled back to.
pub(crate) fn observe_activity_rollback<'a>(
    hashes: impl IntoIterator<Item = &'a B256>,
    ancestor: u64,
) {
    for hash in hashes {
        let orphaned = {
            let mut observed = observed();
            observed
                .get_mut(hash)
                .filter(|attempt| attempt.included.is_some_and(|block| block > ancestor))
                .map(|attempt| {
                    attempt.included = None;
                    attempt.clone()
                })
        };
        if let Some(attempt) = orphaned {
            attempt.write(*hash, None);
        }
    }
}

/// A public transaction from a Public account as entered, owned by that account. It holds
/// only public account data, never the identity of a wallet it shields into.
pub(crate) struct PublicActivity {
    pub(crate) account_uuid: String,
    pub(crate) chain_id: u64,
    pub(crate) kind: ActivityKind,
    pub(crate) legs: Vec<ActivityLeg>,
    pub(crate) counterparty: Option<ActivityCounterparty>,
}

/// A private send or unshield as entered, owned by the submitting wallet.
pub(crate) struct PrivateActivity {
    pub(crate) chain_id: u64,
    pub(crate) kind: ActivityKind,
    pub(crate) token: Address,
    pub(crate) amount: U256,
    pub(crate) counterparty: ActivityCounterparty,
    pub(crate) fee: Option<ActivityFee>,
    /// The Public account paying self-broadcast gas.
    pub(crate) gas_payer: Option<Address>,
    pub(crate) inputs: Vec<ActivityInput>,
}

pub(crate) fn activity_inputs(utxos: &[Utxo]) -> Vec<ActivityInput> {
    utxos
        .iter()
        .map(|utxo| ActivityInput {
            tree: utxo.tree,
            position: utxo.position,
        })
        .collect()
}

/// Whether an executor operation's step belongs to a swap use. Activity lists swaps from the
/// executor records, so such a step writes no entry.
pub(crate) fn is_swap_step(records: &[ExecutorRecord], operation: ExecutorOperationId) -> bool {
    records
        .iter()
        .any(|record| record.operation() == operation && record.active_swap_use().is_some())
}

enum Target {
    Off,
    /// The action's own entry, appended once `written`.
    Entry {
        entry: Box<ActivityEntry>,
        written: bool,
    },
}

/// One action's journal entry for the life of its submission function. A written entry is in
/// flight until this is dropped.
pub(crate) struct ActivitySubmission {
    journal: Option<ActivityJournal>,
    db: Arc<local_db::DbStore>,
    view: Weak<DesktopViewSession>,
    target: Mutex<Target>,
}

impl ActivitySubmission {
    /// A private send or unshield. A `swap_step` writes nothing, since its swap's row comes from
    /// the executor records.
    pub(crate) fn private(
        vault_store: &DesktopVaultStore,
        view: &Arc<DesktopViewSession>,
        activity: PrivateActivity,
        swap_step: bool,
    ) -> Self {
        if swap_step {
            return Self::new(vault_store, view, Target::Off);
        }
        Self::with_entry(vault_store, view, |id| ActivityEntry {
            fee: activity.fee,
            gas_payer: activity.gas_payer,
            match_key: ActivityMatch::Inputs(activity.inputs),
            ..submission_entry(
                id,
                ActivityOwner::Wallet(view.wallet_id().to_owned()),
                ActivityNetwork::Evm(activity.chain_id),
                activity.kind,
                vec![ActivityLeg {
                    asset: ActivityAsset::Token(activity.token),
                    amount: activity.amount,
                    direction: ActivityDirection::Out,
                }],
                Some(activity.counterparty),
            )
        })
    }

    /// A public transaction, matched by its attempts' hashes. `view` is the submitting
    /// wallet's session, which only opens the journal; the entry belongs to the account.
    pub(crate) fn public(
        vault_store: &DesktopVaultStore,
        view: &Arc<DesktopViewSession>,
        activity: PublicActivity,
    ) -> Self {
        Self::with_entry(vault_store, view, |id| {
            submission_entry(
                id,
                ActivityOwner::Account(activity.account_uuid),
                ActivityNetwork::Evm(activity.chain_id),
                activity.kind,
                activity.legs,
                activity.counterparty,
            )
        })
    }

    /// A Tron send from the Tron account `account_uuid` to `receiver`, matched by its `txid`.
    pub(crate) fn tron(
        vault_store: &DesktopVaultStore,
        view: &Arc<DesktopViewSession>,
        account_uuid: String,
        leg: ActivityLeg,
        receiver: Address,
    ) -> Self {
        Self::with_entry(vault_store, view, |id| {
            submission_entry(
                id,
                ActivityOwner::Account(account_uuid),
                ActivityNetwork::Tron,
                ActivityKind::PublicSend,
                vec![leg],
                Some(ActivityCounterparty::Tron(receiver)),
            )
        })
    }

    /// Tags the action's own entry with where it came from, such as a dapp. Call before the
    /// entry is written.
    pub(crate) fn with_source(self, source: ActivitySource) -> Self {
        if let Target::Entry { entry, .. } = &mut *self.lock() {
            entry.source = source;
        }
        self
    }

    /// Records the Public account paying self-broadcast gas. Call while preparing the
    /// submission, before the entry is first written.
    pub(crate) fn set_gas_payer(&self, payer: Address) {
        if let Target::Entry { entry, .. } = &mut *self.lock() {
            entry.gas_payer = Some(payer);
        }
    }

    /// [`Self::with_source`] for an action of the gateway draft from `origin`, if any.
    pub(crate) fn with_gateway_origin(self, origin: Option<String>) -> Self {
        match origin {
            Some(origin) => self.with_source(ActivitySource::Gateway { origin }),
            None => self,
        }
    }

    fn with_entry(
        vault_store: &DesktopVaultStore,
        view: &Arc<DesktopViewSession>,
        entry: impl FnOnce(ActivityEntryId) -> ActivityEntry,
    ) -> Self {
        let Ok(id) = ActivityEntryId::random() else {
            journal_failed(
                "id",
                "activity journal entry id is unavailable; the submission continues",
            );
            return Self::new(vault_store, view, Target::Off);
        };
        Self::new(
            vault_store,
            view,
            Target::Entry {
                entry: Box::new(entry(id)),
                written: false,
            },
        )
    }

    fn new(
        vault_store: &DesktopVaultStore,
        view: &Arc<DesktopViewSession>,
        target: Target,
    ) -> Self {
        let journal = ActivityJournal::new(vault_store.db(), Arc::clone(view))
            .inspect_err(|_| match &target {
                // Nothing to miss: a swap step writes no entry, and a missing id already signalled.
                Target::Off => {
                    tracing::warn!("activity journal is unavailable; the submission continues");
                }
                Target::Entry { .. } => journal_failed(
                    "open",
                    "activity journal is unavailable; the submission continues",
                ),
            })
            .ok();
        Self {
            journal,
            db: vault_store.db(),
            view: Arc::downgrade(view),
            target: Mutex::new(target),
        }
    }

    /// Writes the entry before a broadcaster publish, whose hash only a `Submitted` response
    /// returns.
    pub(crate) fn begin(&self) {
        self.save("append", |_| false);
    }

    /// Records a signed or relayed attempt's hash. Called before handoff where the wallet signs.
    pub(crate) fn add_attempt(&self, hash: B256) {
        self.add_step_attempt(hash, ActivityStep::Main);
    }

    /// [`Self::add_attempt`] for an attempt of `step`.
    pub(crate) fn add_step_attempt(&self, hash: B256, step: ActivityStep) {
        // A sponsored bundle resubmits one hash every block.
        self.save("attempt", |entry| {
            let new = !entry.attempts.iter().any(|known| known.hash == hash);
            if new {
                entry.attempts.push(ActivityAttempt {
                    hash,
                    step,
                    outcome: None,
                });
            }
            new
        });
        let written = if let Target::Entry {
            entry,
            written: true,
        } = &*self.lock()
        {
            Some((entry.owner.clone(), entry.id))
        } else {
            None
        };
        if let Some((owner, entry)) = written {
            self.observe(owner, entry, hash);
        }
    }

    /// Lets the block observer persist `hash`'s inclusion into the attempt.
    fn observe(&self, owner: ActivityOwner, id: ActivityEntryId, hash: B256) {
        let mut observed = observed();
        observed.retain(|_, attempt| attempt.view.strong_count() > 0);
        observed.entry(hash).or_insert_with(|| ObservedAttempt {
            db: Arc::clone(&self.db),
            view: self.view.clone(),
            owner,
            id,
            included: None,
        });
    }

    /// Settles the entry from a broadcaster submission. `None` is an error before any publish
    /// was attempted. A definite failure deletes it, a timeout keeps it unconfirmed, and
    /// `Submitted` adds the returned hash.
    pub(crate) fn finish_relay(&self, result: Option<&PublicBroadcasterResultKind>) {
        match result {
            Some(PublicBroadcasterResultKind::Submitted { tx_hash }) => {
                if let Ok(hash) = tx_hash.parse() {
                    self.add_attempt(hash);
                }
            }
            Some(PublicBroadcasterResultKind::TimedOut) => {
                self.save("timeout", |entry| {
                    entry.relay_timed_out = true;
                    true
                });
            }
            Some(PublicBroadcasterResultKind::Failed { .. }) | None => self.discard(),
        }
    }

    /// Persists the outcome the submission itself decided for its attempt `hash`, as a Tron send
    /// does from solidified blocks.
    pub(crate) fn record_outcome(&self, hash: B256, outcome: ActivityAttemptOutcome) {
        let target = self.lock();
        let (
            Some(journal),
            Target::Entry {
                entry,
                written: true,
            },
        ) = (&self.journal, &*target)
        else {
            return;
        };
        write_attempt_outcome(journal, &entry.owner, entry.id, hash, Some(outcome));
    }

    fn discard(&self) {
        let mut target = self.lock();
        let (Some(journal), Target::Entry { entry, written }) = (&self.journal, &mut *target)
        else {
            return;
        };
        if !*written {
            return;
        }
        if journal.delete(&entry.owner, entry.id).is_err() {
            journal_failed(
                "delete",
                "activity journal write failed; the submission continues",
            );
        }
        *written = false;
        running().remove(&entry.id);
    }

    /// Applies `change`, which says whether it changed the entry, and persists the entry,
    /// appending it on its first write. Stored attempts and their recorded outcomes are kept.
    fn save(&self, step: &'static str, change: impl FnOnce(&mut ActivityEntry) -> bool) {
        let mut target = self.lock();
        let (Some(journal), Target::Entry { entry, written }) = (&self.journal, &mut *target)
        else {
            return;
        };
        let changed = change(entry);
        let result = if *written {
            if !changed {
                return;
            }
            journal
                .update(&entry.owner, entry.id, |stored| {
                    for attempt in &entry.attempts {
                        if !stored
                            .attempts
                            .iter()
                            .any(|known| known.hash == attempt.hash)
                        {
                            stored.attempts.push(*attempt);
                        }
                    }
                    stored.relay_timed_out |= entry.relay_timed_out;
                })
                .map(drop)
        } else {
            journal.append(entry).inspect(|()| {
                *written = true;
                running().insert(entry.id);
            })
        };
        match result {
            // The user may have cleared history while the submission ran.
            Ok(()) | Err(ActivityStoreError::NotFound) => {}
            Err(_) => journal_failed(
                step,
                "activity journal write failed; the submission continues",
            ),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Target> {
        self.target.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for ActivitySubmission {
    fn drop(&mut self) {
        if let Target::Entry {
            entry,
            written: true,
        } = self
            .target
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
        {
            running().remove(&entry.id);
        }
    }
}

/// A new submission's entry, created now by the wallet and matched by its attempts' hashes.
fn submission_entry(
    id: ActivityEntryId,
    owner: ActivityOwner,
    network: ActivityNetwork,
    kind: ActivityKind,
    legs: Vec<ActivityLeg>,
    counterparty: Option<ActivityCounterparty>,
) -> ActivityEntry {
    ActivityEntry {
        id,
        owner,
        network,
        kind,
        source: ActivitySource::Wallet,
        created_at: now(),
        legs,
        counterparty,
        fee: None,
        gas_payer: None,
        match_key: ActivityMatch::Hashes,
        attempts: Vec::new(),
        relay_timed_out: false,
        cleared: false,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
