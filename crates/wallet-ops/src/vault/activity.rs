//! Encrypted activity journal: one vault record per entry, keyed only by its owner and a random
//! id so that plaintext keys reveal neither network nor time.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock, Mutex};

use alloy::primitives::{Address, B256, FixedBytes, U256};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zeroize::Zeroizing;

use super::{
    DesktopVaultStore, DesktopViewSession, EncryptedRecord, RecordKind, SwapUseId, VaultError,
    ViewUnlock, public_account_metadata_record_key, tron_account_metadata_record_key,
    wallet_view_record_key,
};

// Serializes journal read/modify/write against owner deletion, which holds it while collecting
// keys so a stale writer can't recreate a deleted owner's records. Never hold it across async work.
pub(super) static ACTIVITY_RECORD_LOCK: Mutex<()> = Mutex::new(());
/// Counts journal and marker writes in this process.
static ACTIVITY_CHANGES: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::Sender::new(0));

/// Changes whenever this process writes or deletes a journal entry or a markers record, so a
/// view knows to read the journal again.
#[must_use]
pub fn subscribe_activity_changes() -> watch::Receiver<u64> {
    ACTIVITY_CHANGES.subscribe()
}

fn notify_activity_change() {
    ACTIVITY_CHANGES.send_modify(|revision| *revision = revision.wrapping_add(1));
}

#[derive(Debug, thiserror::Error)]
pub enum ActivityStoreError {
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error(transparent)]
    Db(#[from] local_db::DbError),
    #[error("encode activity record")]
    Encode(#[from] rmp_serde::encode::Error),
    /// Names the record kind only: the serde message can quote decrypted values.
    #[error("decode {} record", kind.as_str())]
    Decode { kind: RecordKind },
    #[error("activity storage is unavailable")]
    Unavailable,
    #[error("activity record identity is invalid")]
    InvalidRecord,
    #[error("activity owner is not this wallet")]
    OwnerMismatch,
    #[error("activity entry not found")]
    NotFound,
}

impl ActivityStoreError {
    /// What failed, safe to log: wrapped vault and database messages can carry record text.
    #[must_use]
    pub fn redacted(&self) -> String {
        match self {
            Self::Vault(VaultError::Decrypt) => "decrypt activity record".to_owned(),
            Self::Vault(_) => "vault operation failed".to_owned(),
            Self::Db(_) => "database operation failed".to_owned(),
            Self::Encode(_) => "encode activity record".to_owned(),
            Self::Decode { .. }
            | Self::Unavailable
            | Self::InvalidRecord
            | Self::OwnerMismatch
            | Self::NotFound => self.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ActivityEntryId(FixedBytes<16>);

impl ActivityEntryId {
    pub fn random() -> Result<Self, ActivityStoreError> {
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|_| ActivityStoreError::Unavailable)?;
        Ok(Self(id.into()))
    }

    #[must_use]
    pub fn opaque_id(self) -> String {
        alloy::hex::encode(self.0)
    }
}

/// The balance an action spends from: a private wallet, or a Public or Tron account by uuid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityOwner {
    Wallet(String),
    Account(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ActivityNetwork {
    Evm(u64),
    Tron,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    PrivateSend,
    Unshield,
    Swap,
    Shield,
    PublicSend,
    ContractCall,
    Governance,
    Refund,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivitySource {
    Wallet,
    WalletConnect { dapp: String },
    Gateway { origin: String },
}

/// Native coin, or an ERC-20 or TRC-20 contract (Tron addresses are the 20 bytes after `0x41`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityAsset {
    Native,
    Token(Address),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityDirection {
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityLeg {
    pub asset: ActivityAsset,
    pub amount: U256,
    pub direction: ActivityDirection,
}

/// The counterparty as entered. Address-book labels are resolved at render time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityCounterparty {
    Private(String),
    Evm(Address),
    Tron(Address),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityFee {
    pub asset: ActivityAsset,
    pub amount: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityInput {
    pub tree: u32,
    pub position: u64,
}

/// What status derivation matches the entry by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityMatch {
    Inputs(Vec<ActivityInput>),
    Hashes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityStep {
    Wrap,
    Approve,
    /// The undelegation an Unlock of an externally delegated stake starts with.
    Undelegate,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityAttemptOutcome {
    Included {
        block: u64,
        timestamp: u64,
        success: bool,
    },
    /// Proven never included.
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityAttempt {
    pub hash: B256,
    pub step: ActivityStep,
    pub outcome: Option<ActivityAttemptOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub id: ActivityEntryId,
    pub owner: ActivityOwner,
    pub network: ActivityNetwork,
    pub kind: ActivityKind,
    pub source: ActivitySource,
    /// Local clock seconds, used only for ordering and age clearing.
    pub created_at: u64,
    pub legs: Vec<ActivityLeg>,
    pub counterparty: Option<ActivityCounterparty>,
    pub fee: Option<ActivityFee>,
    /// Set when gas came from an account other than the owner.
    pub gas_payer: Option<Address>,
    pub match_key: ActivityMatch,
    pub attempts: Vec<ActivityAttempt>,
    /// The broadcaster timed out, so the entry stays unconfirmed until its inputs are seen spent.
    pub relay_timed_out: bool,
    /// A hidden account-owned stub that only suppresses its hashes' notes as received rows.
    pub cleared: bool,
}

/// One per wallet, sealed with its private view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityMarkers {
    /// Block timestamp at or before which received rows are hidden.
    pub received_hidden_before: u64,
    /// Last-seen block number per network.
    pub last_seen: BTreeMap<ActivityNetwork, u64>,
    /// Swap uses whose rows were cleared. Ids are only added.
    pub hidden_swap_uses: BTreeSet<SwapUseId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityClearAge {
    OlderThanDays(u32),
    All,
}

/// A clear-history choice. `received` hides received rows of the same age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityClear {
    pub age: ActivityClearAge,
    pub kinds: Vec<ActivityKind>,
    pub received: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivityClearResult {
    /// Entries deleted, or rewritten as stubs for accounts.
    pub cleared: usize,
    /// Matching entries kept because their work is in flight.
    pub kept_in_flight: usize,
}

impl ActivityClear {
    /// The time at or before which entries and received rows match; `None` matches all entries.
    fn cutoff(&self, now: u64) -> Option<u64> {
        match self.age {
            ActivityClearAge::OlderThanDays(days) => {
                Some(now.saturating_sub(u64::from(days) * 86_400))
            }
            ActivityClearAge::All => None,
        }
    }

    fn matches(&self, entry: &ActivityEntry, now: u64) -> bool {
        !entry.cleared
            && self.kinds.contains(&entry.kind)
            && self
                .cutoff(now)
                .is_none_or(|cutoff| entry.created_at <= cutoff)
    }
}

/// Journal access for one wallet session and the accounts in its scope. Wallet-owned entries
/// and markers are sealed with the wallet's private view; account-owned entries with the vault
/// view DEK, like account records.
#[derive(Clone)]
pub struct ActivityJournal {
    db: Arc<local_db::DbStore>,
    view: Arc<DesktopViewSession>,
}

impl ActivityJournal {
    pub fn new(
        db: Arc<local_db::DbStore>,
        view: Arc<DesktopViewSession>,
    ) -> Result<Self, ActivityStoreError> {
        let journal = Self { db, view };
        journal.require_owner(&ActivityOwner::Wallet(journal.view.wallet_id().to_owned()))?;
        Ok(journal)
    }

    pub fn append(&self, entry: &ActivityEntry) -> Result<(), ActivityStoreError> {
        let _guard = lock()?;
        self.require_owner(&entry.owner)?;
        self.db
            .put_desktop_wallet_vault_records(&[self.seal_entry(entry)?])?;
        notify_activity_change();
        Ok(())
    }

    pub fn update(
        &self,
        owner: &ActivityOwner,
        id: ActivityEntryId,
        update: impl FnOnce(&mut ActivityEntry),
    ) -> Result<ActivityEntry, ActivityStoreError> {
        let _guard = lock()?;
        self.require_owner(owner)?;
        let key = self.entry_key(owner, id)?;
        let payload = self
            .db
            .get_desktop_wallet_vault_record(&key)?
            .ok_or(ActivityStoreError::NotFound)?;
        let mut entry = self.open_entry(owner, &key, &payload)?;
        update(&mut entry);
        if entry.owner != *owner || entry.id != id {
            return Err(ActivityStoreError::InvalidRecord);
        }
        self.db
            .put_desktop_wallet_vault_records(&[self.seal_entry(&entry)?])?;
        notify_activity_change();
        Ok(entry)
    }

    /// Rewrites the entry as a hidden stub keeping its owner, network, kind and attempt hashes.
    pub fn clear_to_stub(
        &self,
        owner: &ActivityOwner,
        id: ActivityEntryId,
    ) -> Result<ActivityEntry, ActivityStoreError> {
        self.update(owner, id, |entry| {
            *entry = ActivityEntry {
                id: entry.id,
                owner: entry.owner.clone(),
                network: entry.network,
                kind: entry.kind,
                source: ActivitySource::Wallet,
                created_at: 0,
                legs: Vec::new(),
                counterparty: None,
                fee: None,
                gas_payer: None,
                match_key: ActivityMatch::Hashes,
                attempts: entry
                    .attempts
                    .iter()
                    .map(|attempt| ActivityAttempt {
                        outcome: None,
                        ..*attempt
                    })
                    .collect(),
                relay_timed_out: false,
                cleared: true,
            };
        })
    }

    pub fn delete(
        &self,
        owner: &ActivityOwner,
        id: ActivityEntryId,
    ) -> Result<(), ActivityStoreError> {
        let key = self.entry_key(owner, id)?;
        self.db.delete_desktop_wallet_vault_record(&key)?;
        notify_activity_change();
        Ok(())
    }

    /// The owner's entries on every network, cleared stubs included, newest first.
    pub fn list(&self, owner: &ActivityOwner) -> Result<Vec<ActivityEntry>, ActivityStoreError> {
        let mut entries = self
            .db
            .list_desktop_wallet_vault_records(&self.owner_prefix(owner)?)?
            .into_iter()
            .map(|stored| self.open_entry(owner, &stored.key, &stored.payload))
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
        Ok(entries)
    }

    pub fn list_for_network(
        &self,
        owner: &ActivityOwner,
        network: ActivityNetwork,
    ) -> Result<Vec<ActivityEntry>, ActivityStoreError> {
        let mut entries = self.list(owner)?;
        entries.retain(|entry| entry.network == network);
        Ok(entries)
    }

    pub fn markers(&self) -> Result<ActivityMarkers, ActivityStoreError> {
        let key = activity_markers_key(self.view.wallet_id());
        self.db
            .get_desktop_wallet_vault_record(&key)?
            .map(|payload| {
                open(
                    &self.view.private_view,
                    RecordKind::ActivityMarkers,
                    &key,
                    &payload,
                )
            })
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub fn update_markers(
        &self,
        update: impl FnOnce(&mut ActivityMarkers),
    ) -> Result<ActivityMarkers, ActivityStoreError> {
        let _guard = lock()?;
        self.require_owner(&ActivityOwner::Wallet(self.view.wallet_id().to_owned()))?;
        let mut markers = self.markers()?;
        update(&mut markers);
        let key = activity_markers_key(self.view.wallet_id());
        let record = seal(
            &self.view.private_view,
            RecordKind::ActivityMarkers,
            key,
            &markers,
        )?;
        self.db.put_desktop_wallet_vault_records(&[record])?;
        notify_activity_change();
        Ok(markers)
    }

    /// This wallet and every Public and Tron account in its scope, hidden ones included, as the
    /// owners whose entries its Activity shows and clears.
    pub fn scope_owners(&self) -> Result<Vec<ActivityOwner>, ActivityStoreError> {
        let store = DesktopVaultStore::from_db(Arc::clone(&self.db));
        let mut owners = vec![ActivityOwner::Wallet(self.view.wallet_id().to_owned())];
        owners.extend(
            store
                .list_public_accounts_for_session(&self.view, true)?
                .into_iter()
                .map(|account| ActivityOwner::Account(account.public_account_uuid)),
        );
        owners.extend(
            store
                .list_tron_accounts_for_session(&self.view, true)?
                .into_iter()
                .map(|account| ActivityOwner::Account(account.tron_account_uuid)),
        );
        Ok(owners)
    }

    /// Clears `selection` relative to `now` on every network for `scope_owners`, skipping entries
    /// `in_flight` reports. Wallet-owned entries are deleted; account-owned ones become stubs so
    /// every wallet keeps suppressing their notes as received rows. Matching `swaps` that aren't
    /// in flight are hidden by the markers. Only journal records change.
    pub fn clear(
        &self,
        selection: &ActivityClear,
        now: u64,
        swaps: &[crate::SwapRow],
        in_flight: impl Fn(&ActivityEntry) -> bool,
    ) -> Result<ActivityClearResult, ActivityStoreError> {
        let mut result = ActivityClearResult::default();
        for owner in self.scope_owners()? {
            for entry in self.list(&owner)? {
                if !selection.matches(&entry, now) {
                    continue;
                }
                if in_flight(&entry) {
                    result.kept_in_flight += 1;
                    continue;
                }
                match &owner {
                    ActivityOwner::Wallet(_) => self.delete(&owner, entry.id)?,
                    ActivityOwner::Account(_) => {
                        self.clear_to_stub(&owner, entry.id)?;
                    }
                }
                result.cleared += 1;
            }
        }
        let mut hidden_swaps = BTreeSet::new();
        if selection.kinds.contains(&ActivityKind::Swap) {
            let markers = self.markers()?;
            for swap in swaps {
                if markers.hidden_swap_uses.contains(&swap.id)
                    || selection
                        .cutoff(now)
                        .is_some_and(|cutoff| swap.started_at > cutoff)
                {
                    continue;
                }
                if swap.in_flight {
                    result.kept_in_flight += 1;
                } else if hidden_swaps.insert(swap.id) {
                    result.cleared += 1;
                }
            }
        }
        let hidden_before = selection
            .received
            .then(|| selection.cutoff(now).unwrap_or(now));
        if hidden_before.is_some() || !hidden_swaps.is_empty() {
            self.update_markers(|markers| {
                if let Some(hidden_before) = hidden_before {
                    markers.received_hidden_before =
                        markers.received_hidden_before.max(hidden_before);
                }
                markers.hidden_swap_uses.append(&mut hidden_swaps);
            })?;
        }
        Ok(result)
    }

    /// Sets `network`'s last-seen block to `tip` when the wallet's first catch-up there
    /// completes, so rows it found aren't unread. A marker already set is kept.
    pub fn init_last_seen(
        &self,
        network: ActivityNetwork,
        tip: u64,
    ) -> Result<(), ActivityStoreError> {
        if !self.markers()?.last_seen.contains_key(&network) {
            self.update_markers(|markers| {
                markers.last_seen.entry(network).or_insert(tip);
            })?;
        }
        Ok(())
    }

    /// Moves `network`'s last-seen block forward to `block`, the newest received row Activity
    /// shows when it opens.
    pub fn advance_last_seen(
        &self,
        network: ActivityNetwork,
        block: u64,
    ) -> Result<(), ActivityStoreError> {
        if self
            .markers()?
            .last_seen
            .get(&network)
            .is_none_or(|seen| *seen < block)
        {
            self.update_markers(|markers| {
                let seen = markers.last_seen.entry(network).or_insert(block);
                *seen = (*seen).max(block);
            })?;
        }
        Ok(())
    }

    /// The owner still exists, and a wallet owner is this session's wallet.
    fn require_owner(&self, owner: &ActivityOwner) -> Result<(), ActivityStoreError> {
        match owner {
            ActivityOwner::Wallet(wallet_id) => {
                if wallet_id != self.view.wallet_id() {
                    return Err(ActivityStoreError::OwnerMismatch);
                }
                if self
                    .db
                    .get_desktop_wallet_vault_record(&wallet_view_record_key(wallet_id))?
                    .is_none()
                {
                    return Err(ActivityStoreError::Unavailable);
                }
                DesktopVaultStore::from_db(self.db.clone()).validate_executor_source(&self.view)?;
            }
            ActivityOwner::Account(account_id) => {
                if self
                    .db
                    .get_desktop_wallet_vault_record(&public_account_metadata_record_key(
                        account_id,
                    ))?
                    .is_none()
                    && self
                        .db
                        .get_desktop_wallet_vault_record(&tron_account_metadata_record_key(
                            account_id,
                        ))?
                        .is_none()
                {
                    return Err(ActivityStoreError::Unavailable);
                }
            }
        }
        Ok(())
    }

    fn owner_prefix(&self, owner: &ActivityOwner) -> Result<String, ActivityStoreError> {
        match owner {
            ActivityOwner::Wallet(wallet_id) if wallet_id == self.view.wallet_id() => {
                Ok(activity_wallet_prefix(wallet_id))
            }
            ActivityOwner::Wallet(_) => Err(ActivityStoreError::OwnerMismatch),
            ActivityOwner::Account(account_id) => Ok(activity_account_prefix(account_id)),
        }
    }

    fn entry_key(
        &self,
        owner: &ActivityOwner,
        id: ActivityEntryId,
    ) -> Result<String, ActivityStoreError> {
        Ok(format!("{}{}", self.owner_prefix(owner)?, id.opaque_id()))
    }

    fn cipher(&self, owner: &ActivityOwner) -> &ViewUnlock {
        match owner {
            ActivityOwner::Wallet(_) => &self.view.private_view,
            ActivityOwner::Account(_) => &self.view.view,
        }
    }

    fn seal_entry(&self, entry: &ActivityEntry) -> Result<(String, Vec<u8>), ActivityStoreError> {
        let key = self.entry_key(&entry.owner, entry.id)?;
        seal(
            self.cipher(&entry.owner),
            RecordKind::ActivityEntry,
            key,
            entry,
        )
    }

    fn open_entry(
        &self,
        owner: &ActivityOwner,
        key: &str,
        payload: &[u8],
    ) -> Result<ActivityEntry, ActivityStoreError> {
        let entry: ActivityEntry =
            open(self.cipher(owner), RecordKind::ActivityEntry, key, payload)?;
        if entry.owner != *owner || self.entry_key(owner, entry.id)? != key {
            return Err(ActivityStoreError::InvalidRecord);
        }
        Ok(entry)
    }
}

// The record key binds owner and id, so it alone is the AAD record id.
fn seal<T: Serialize>(
    cipher: &ViewUnlock,
    kind: RecordKind,
    key: String,
    value: &T,
) -> Result<(String, Vec<u8>), ActivityStoreError> {
    let plaintext = Zeroizing::new(rmp_serde::to_vec_named(value)?);
    Ok(cipher
        .encrypt_record(kind, &key, &plaintext)?
        .to_record_entry(key)?)
}

fn open<T: serde::de::DeserializeOwned>(
    cipher: &ViewUnlock,
    kind: RecordKind,
    key: &str,
    payload: &[u8],
) -> Result<T, ActivityStoreError> {
    let decode = |_: rmp_serde::decode::Error| ActivityStoreError::Decode { kind };
    let record: EncryptedRecord = rmp_serde::from_slice(payload).map_err(decode)?;
    let plaintext = cipher.decrypt_record(kind, key, &record)?;
    rmp_serde::from_slice(&plaintext).map_err(decode)
}

impl DesktopVaultStore {
    /// Keys of an account's journal entries, for deleting them with the account. The caller
    /// holds `ACTIVITY_RECORD_LOCK`.
    pub(super) fn activity_account_keys(
        &self,
        account_id: &str,
    ) -> Result<Vec<String>, VaultError> {
        Ok(self
            .db
            .list_desktop_wallet_vault_records(&activity_account_prefix(account_id))?
            .into_iter()
            .map(|record| record.key)
            .collect())
    }
}

fn lock() -> Result<std::sync::MutexGuard<'static, ()>, ActivityStoreError> {
    ACTIVITY_RECORD_LOCK
        .lock()
        .map_err(|_| ActivityStoreError::Unavailable)
}

pub(super) fn activity_wallet_prefix(wallet_id: &str) -> String {
    format!("activity|w|{}:{wallet_id}|", wallet_id.len())
}
pub(super) fn activity_account_prefix(account_id: &str) -> String {
    format!("activity|a|{}:{account_id}|", account_id.len())
}
pub(super) fn activity_markers_key(wallet_id: &str) -> String {
    format!("activity-markers|{}:{wallet_id}", wallet_id.len())
}
