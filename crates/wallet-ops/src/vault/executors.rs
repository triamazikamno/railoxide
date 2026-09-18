use std::sync::{Arc, Mutex};

use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, FixedBytes, U256};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

mod public_account;
mod recovery;
mod spare;
pub use recovery::*;
pub(crate) use spare::ExecutorSpare;

use super::{
    DesktopVaultStore, DesktopViewSession, EncryptedRecord, RecordKind, VaultError,
    wallet_view_record_key,
};

const VERSION: u32 = 1;
const ALLOCATION_VERSION: u32 = 2;
const INDEX_LIMIT: u32 = 1 << 31;
const POSITION_START: u32 = 1_000_000;
const POSITION_END: u32 = 1_000_063;
pub const MAX_EXECUTOR_DISCOVERY_RANGE: u32 = 64;

// Like wallet-chain metadata creation, short vault read/modify/write operations
// are serialized across store handles. Never hold this guard across async work.
pub(super) static EXECUTOR_RECORD_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, thiserror::Error)]
pub enum ExecutorStoreError {
    #[error(transparent)]
    Vault(#[from] VaultError),
    #[error(transparent)]
    Db(#[from] local_db::DbError),
    #[error("encode executor record")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode executor record")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("executor storage is unavailable")]
    Unavailable,
    #[error("executor record version or identity is invalid")]
    InvalidRecord,
    #[error("software wallet access is required for executors")]
    SoftwareWalletRequired,
    #[error("executor index space is exhausted")]
    Exhausted,
    #[error("executor discovery requires a nonempty bounded range of hardened indices")]
    DiscoveryRange,
    #[error("executor operation does not match its reservation")]
    OperationMismatch,
    #[error("executor payload requires current reconciled execution nonce state")]
    OutstandingNonce,
    #[error("private inputs are reserved by another executor operation")]
    InputReserved,
}

/// Stable native operation identity, retained across retries and presentation cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExecutorOperationId(FixedBytes<16>);

impl ExecutorOperationId {
    pub fn random() -> Result<Self, ExecutorStoreError> {
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|_| ExecutorStoreError::Unavailable)?;
        Ok(Self(id.into()))
    }

    #[must_use]
    pub fn opaque_id(self) -> String {
        alloy::hex::encode(self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutorPayloadPurpose {
    Operation,
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutorDerivationScheme {
    Railgun7702V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutorRecordOrigin {
    Reserved,
    Discovered,
}

/// Assets associated with the approved operation, also used for explicit balance checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ExecutorAsset {
    Native,
    Erc20(Address),
    Erc721 { collection: Address, token_id: U256 },
}

fn local_timestamp() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// A private input remains identifiable after cache reconstruction or a reorg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorInputIdentity {
    tree: u32,
    position: u64,
    commitment: U256,
}

impl ExecutorInputIdentity {
    #[must_use]
    pub fn from_utxo(utxo: &railgun_wallet::Utxo) -> Self {
        Self {
            tree: utxo.tree,
            position: utxo.position,
            commitment: utxo.note.commitment(),
        }
    }

    #[must_use]
    pub fn matches(&self, utxo: &railgun_wallet::Utxo) -> bool {
        self.tree == utxo.tree
            && self.position == utxo.position
            && self.commitment == utxo.note.commitment()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorNonceObservation {
    block: BlockNumHash,
    nonce: U256,
}

impl ExecutorNonceObservation {
    #[must_use]
    pub const fn new(block: BlockNumHash, nonce: U256) -> Self {
        Self { block, nonce }
    }
    #[must_use]
    pub const fn block(self) -> BlockNumHash {
        self.block
    }
    #[must_use]
    pub const fn nonce(self) -> U256 {
        self.nonce
    }
}

/// The full issued call retains expected private transactions and recovery actions.
/// It stays encrypted, and permits block-scoped discovery if handoff returned no hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorPayloadContext {
    calldata: Bytes,
    observed: ExecutorNonceObservation,
    inputs: Vec<ExecutorInputIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_start: Option<u64>,
}

impl ExecutorPayloadContext {
    #[must_use]
    pub const fn new(
        calldata: Bytes,
        observed: ExecutorNonceObservation,
        inputs: Vec<ExecutorInputIdentity>,
    ) -> Self {
        Self {
            calldata,
            observed,
            inputs,
            history_start: None,
        }
    }
    #[must_use]
    pub const fn calldata(&self) -> &Bytes {
        &self.calldata
    }
    #[must_use]
    pub const fn observed(&self) -> ExecutorNonceObservation {
        self.observed
    }
    #[must_use]
    pub fn inputs(&self) -> &[ExecutorInputIdentity] {
        &self.inputs
    }

    /// The checked nonce may predate signing when an unused spare was prefetched.
    pub(crate) const fn with_history_start(mut self, block: u64) -> Self {
        self.history_start = Some(block);
        self
    }

    pub(crate) const fn history_start(&self) -> u64 {
        match self.history_start {
            Some(block) => block,
            None => self.observed.block().number,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutorExecutionResult {
    Reverted,
    MissingEffects,
    Executed,
}

/// Supplied by canonical observation after checking the issued call and its effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorPayloadInclusion {
    block: BlockNumHash,
    transaction_hash: B256,
    result: ExecutorExecutionResult,
    /// Present only when the verified outer sender is this executor. Older rows
    /// omit this evidence and acquire it on canonical reobservation.
    #[serde(default)]
    executor_account_nonce: Option<u64>,
}

impl ExecutorPayloadInclusion {
    #[must_use]
    pub const fn new(
        block: BlockNumHash,
        transaction_hash: B256,
        result: ExecutorExecutionResult,
    ) -> Self {
        Self {
            block,
            transaction_hash,
            result,
            executor_account_nonce: None,
        }
    }
    pub(crate) const fn with_executor_account_nonce(mut self, nonce: Option<u64>) -> Self {
        self.executor_account_nonce = nonce;
        self
    }
    #[must_use]
    pub const fn block(self) -> BlockNumHash {
        self.block
    }
    #[must_use]
    pub const fn transaction_hash(self) -> B256 {
        self.transaction_hash
    }
    #[must_use]
    pub const fn result(self) -> ExecutorExecutionResult {
        self.result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorPayloadStatus {
    Uncertain,
    Reverted,
    MissingEffects,
    Executed,
    Invalidated { winner: B256 },
}

/// Issued signatures remain relevant even after a reverted transaction or local stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedExecutorPayload {
    capability: crate::settings::ExecutorCapability,
    nonce: U256,
    delegate: Address,
    hash: B256,
    purpose: ExecutorPayloadPurpose,
    transaction_hashes: Vec<B256>,
    context: ExecutorPayloadContext,
    inclusion: Option<ExecutorPayloadInclusion>,
}

impl IssuedExecutorPayload {
    #[must_use]
    pub const fn new(
        nonce: U256,
        delegate: Address,
        hash: B256,
        purpose: ExecutorPayloadPurpose,
        context: ExecutorPayloadContext,
    ) -> Self {
        Self {
            capability: crate::settings::ExecutorCapability::NonceBearingV1,
            nonce,
            delegate,
            hash,
            purpose,
            transaction_hashes: Vec::new(),
            context,
            inclusion: None,
        }
    }

    #[must_use]
    pub const fn nonce(&self) -> U256 {
        self.nonce
    }
    #[must_use]
    pub const fn capability(&self) -> crate::settings::ExecutorCapability {
        self.capability
    }
    #[must_use]
    pub const fn delegate(&self) -> Address {
        self.delegate
    }
    #[must_use]
    pub const fn hash(&self) -> B256 {
        self.hash
    }
    #[must_use]
    pub const fn purpose(&self) -> ExecutorPayloadPurpose {
        self.purpose
    }
    #[must_use]
    pub fn transaction_hashes(&self) -> &[B256] {
        &self.transaction_hashes
    }
    #[must_use]
    pub const fn context(&self) -> &ExecutorPayloadContext {
        &self.context
    }
    #[must_use]
    pub const fn inclusion(&self) -> Option<ExecutorPayloadInclusion> {
        self.inclusion
    }
}

/// A successful bounded use check, independent of transaction reconciliation.
/// An empty return means undelegated; a decoded nonce, including zero, means used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorUseObservation {
    block: BlockNumHash,
    checked_at: u64,
    nonce: Option<U256>,
}

impl ExecutorUseObservation {
    pub(crate) const fn new(block: BlockNumHash, checked_at: u64, nonce: Option<U256>) -> Self {
        Self {
            block,
            checked_at,
            nonce,
        }
    }

    #[must_use]
    pub const fn block(&self) -> BlockNumHash {
        self.block
    }
    #[must_use]
    pub const fn checked_at(&self) -> u64 {
        self.checked_at
    }
    #[must_use]
    pub const fn was_used(&self) -> bool {
        self.nonce.is_some()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorUseCheck {
    observation: Option<ExecutorUseObservation>,
    unavailable: bool,
}

impl ExecutorUseCheck {
    #[must_use]
    pub const fn observation(&self) -> Option<ExecutorUseObservation> {
        self.observation
    }
    #[must_use]
    pub const fn is_unavailable(&self) -> bool {
        self.unavailable
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorRecord {
    version: u32,
    derivation: ExecutorDerivationScheme,
    origin: ExecutorRecordOrigin,
    operation: ExecutorOperationId,
    index: u32,
    address: Option<Address>,
    delegate: Address,
    retired: bool,
    created_at: Option<u64>,
    restored_at: Option<u64>,
    purpose_summary: Option<String>,
    #[serde(default)]
    assets: Vec<ExecutorAsset>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    use_check: ExecutorUseCheck,
    issued: Vec<IssuedExecutorPayload>,
    #[serde(default)]
    nonce_observation: Option<ExecutorNonceObservation>,
    #[serde(default)]
    recovery_transactions: Vec<IssuedExecutorRecoveryTransaction>,
    #[serde(default)]
    recovery_observation: Option<BlockNumHash>,
    #[serde(default)]
    public_account_uuid: Option<String>,
}

impl ExecutorRecord {
    /// Ordinary Public signing requires a freshly reconciled record. A reverted
    /// executor call still has a replayable execution signature; an ordinary
    /// recovery transaction consumes its account nonce even when it reverts.
    #[must_use]
    pub fn has_unresolved_issued_work(&self) -> bool {
        self.issued().iter().any(|payload| {
            !matches!(
                self.payload_status(payload.hash()),
                Some(ExecutorPayloadStatus::Executed | ExecutorPayloadStatus::Invalidated { .. })
            )
        }) || self.recovery_transactions().iter().any(|transaction| {
            !matches!(
                self.recovery_transaction_status(transaction.hash()),
                Some(
                    ExecutorPayloadStatus::Executed
                        | ExecutorPayloadStatus::Reverted
                        | ExecutorPayloadStatus::Invalidated { .. }
                )
            )
        })
    }

    #[must_use]
    pub fn public_account_uuid(&self) -> Option<&str> {
        self.public_account_uuid.as_deref()
    }

    #[must_use]
    pub const fn use_check(&self) -> ExecutorUseCheck {
        self.use_check
    }

    /// Local reservation time, in Unix seconds; unknown for restored accounts.
    #[must_use]
    pub const fn created_at(&self) -> Option<u64> {
        self.created_at
    }
    /// First explicit restoration time, independent of operation creation.
    #[must_use]
    pub const fn restored_at(&self) -> Option<u64> {
        self.restored_at
    }
    #[must_use]
    pub fn purpose_summary(&self) -> Option<&str> {
        self.purpose_summary.as_deref()
    }
    #[must_use]
    pub fn assets(&self) -> &[ExecutorAsset] {
        &self.assets
    }
    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        self.hidden
    }

    #[must_use]
    pub const fn origin(&self) -> ExecutorRecordOrigin {
        self.origin
    }
    #[must_use]
    pub const fn derivation(&self) -> ExecutorDerivationScheme {
        self.derivation
    }
    #[must_use]
    pub const fn operation(&self) -> ExecutorOperationId {
        self.operation
    }
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }
    #[must_use]
    pub const fn address(&self) -> Option<Address> {
        self.address
    }
    #[must_use]
    pub const fn delegate(&self) -> Address {
        self.delegate
    }
    #[must_use]
    pub const fn is_retired(&self) -> bool {
        self.retired
    }
    #[must_use]
    pub fn issued(&self) -> &[IssuedExecutorPayload] {
        &self.issued
    }
    #[must_use]
    pub const fn nonce_observation(&self) -> Option<ExecutorNonceObservation> {
        self.nonce_observation
    }

    pub(crate) const fn require_reconciliation(&mut self) {
        self.nonce_observation = None;
        self.recovery_observation = None;
    }

    fn winner(&self, nonce: U256) -> Option<B256> {
        self.nonce_observation?;
        self.recorded_winner(nonce)
    }

    fn recorded_winner(&self, nonce: U256) -> Option<B256> {
        self.issued
            .iter()
            .find(|payload| {
                payload.nonce == nonce
                    && payload.inclusion.is_some_and(|inclusion| {
                        inclusion.result == ExecutorExecutionResult::Executed
                    })
            })
            .map(|payload| payload.hash)
    }

    #[must_use]
    pub fn payload_status(&self, hash: B256) -> Option<ExecutorPayloadStatus> {
        let recorded = self.recorded_payload_status(hash)?;
        if self.nonce_observation.is_none() {
            return Some(ExecutorPayloadStatus::Uncertain);
        }
        Some(recorded)
    }

    /// Last recorded outcome for history display, without current reconciliation.
    /// This does not establish current signing or recovery eligibility.
    #[must_use]
    pub fn recorded_payload_status(&self, hash: B256) -> Option<ExecutorPayloadStatus> {
        let payload = self.issued.iter().find(|payload| payload.hash == hash)?;
        if let Some(winner) = self.recorded_winner(payload.nonce) {
            return Some(if winner == hash {
                ExecutorPayloadStatus::Executed
            } else {
                ExecutorPayloadStatus::Invalidated { winner }
            });
        }
        Some(match payload.inclusion.map(|inclusion| inclusion.result) {
            Some(ExecutorExecutionResult::Reverted) => ExecutorPayloadStatus::Reverted,
            Some(ExecutorExecutionResult::MissingEffects) => ExecutorPayloadStatus::MissingEffects,
            Some(ExecutorExecutionResult::Executed) => ExecutorPayloadStatus::Executed,
            None => ExecutorPayloadStatus::Uncertain,
        })
    }

    /// A reverted attempt does not revoke its signed payload or free its private inputs.
    /// Keep the winning payload's spent inputs protected while private sync catches up;
    /// only a losing, invalidated payload releases its otherwise unspent inputs.
    #[must_use]
    pub fn reserved_inputs(&self) -> Vec<ExecutorInputIdentity> {
        self.issued
            .iter()
            .filter(|payload| {
                self.winner(payload.nonce)
                    .is_none_or(|winner| winner == payload.hash)
            })
            .flat_map(|payload| payload.context.inputs.iter().cloned())
            .fold(Vec::new(), |mut inputs, input| {
                if !inputs.contains(&input) {
                    inputs.push(input);
                }
                inputs
            })
    }
}

#[derive(Serialize, Deserialize)]
struct Allocation {
    version: u32,
    next_index: u32,
    #[serde(default)]
    spare: Option<ExecutorSpare>,
}

impl Default for Allocation {
    fn default() -> Self {
        Self {
            version: ALLOCATION_VERSION,
            next_index: 0,
            spare: None,
        }
    }
}

/// Encrypted executor persistence for one canonical wallet-chain namespace.
/// Native operation owners provide lifecycle, chain-state and signing admission.
pub struct ExecutorStore {
    vault: DesktopVaultStore,
    view: Arc<DesktopViewSession>,
    chain_id: u64,
}

impl ExecutorStore {
    pub fn new(
        db: Arc<local_db::DbStore>,
        view: Arc<DesktopViewSession>,
        chain_id: u64,
    ) -> Result<Self, ExecutorStoreError> {
        if view.hardware_profile_session().is_some() {
            return Err(ExecutorStoreError::SoftwareWalletRequired);
        }
        let store = Self {
            vault: DesktopVaultStore::from_db(db),
            view,
            chain_id,
        };
        store.require_wallet()?;
        Ok(store)
    }

    pub fn records(&self) -> Result<Vec<ExecutorRecord>, ExecutorStoreError> {
        let mut records = self
            .vault
            .db
            .list_desktop_wallet_vault_records(&executor_operation_prefix(
                self.view.wallet_id(),
                self.chain_id,
            ))?
            .into_iter()
            .map(|stored| {
                let record: ExecutorRecord =
                    self.open(RecordKind::ExecutorOperation, &stored.key, &stored.payload)?;
                if record.version != VERSION
                    || record.index >= INDEX_LIMIT
                    || stored.key != self.operation_key(record.operation)
                {
                    return Err(ExecutorStoreError::InvalidRecord);
                }
                Ok(record)
            })
            .collect::<Result<Vec<_>, _>>()?;
        records.sort_by_key(ExecutorRecord::index);
        Ok(records)
    }

    /// Missing allocation data is not evidence of an unused address family.
    pub fn next_index(&self) -> Result<u32, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        Ok(self.allocation()?.next_index)
    }

    /// Retained history and explicit discovery can only advance the allocation floor.
    /// This does not establish complete discovery or eligibility of the next address.
    pub fn raise_floor(&self, next_index: u32) -> Result<(), ExecutorStoreError> {
        if next_index > INDEX_LIMIT {
            return Err(ExecutorStoreError::Exhausted);
        }
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut allocation = self.allocation()?;
        allocation.next_index = allocation.next_index.max(next_index);
        let mut updates = Vec::new();
        if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.index < next_index)
        {
            self.retain_spare(&mut allocation, &mut updates)?;
        }
        updates.push(self.seal(
            RecordKind::ExecutorAllocation,
            self.allocation_key(),
            &allocation,
        )?);
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(())
    }

    /// Persists the reservation and advances the floor in the same database transaction.
    /// The caller derives and checks the reserved account before using its address.
    pub fn reserve(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        purpose_summary: Option<&str>,
        assets: &[ExecutorAsset],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        if let Some(existing) = self.record(operation)? {
            return if existing.delegate == delegate
                && existing.origin == ExecutorRecordOrigin::Reserved
            {
                Ok(existing)
            } else {
                Err(ExecutorStoreError::OperationMismatch)
            };
        }
        let mut allocation = self.allocation()?;
        let mut updates = Vec::new();
        if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.delegate != delegate)
        {
            self.retain_spare(&mut allocation, &mut updates)?;
        }
        let (index, address) = if let Some(spare) = allocation.spare.take() {
            (spare.index, spare.address)
        } else {
            let index = ordinary_index(allocation.next_index)?;
            allocation.next_index = index + 1;
            (index, None)
        };
        let record = ExecutorRecord {
            version: VERSION,
            derivation: ExecutorDerivationScheme::Railgun7702V1,
            origin: ExecutorRecordOrigin::Reserved,
            operation,
            index,
            address,
            delegate,
            retired: false,
            created_at: local_timestamp(),
            restored_at: None,
            purpose_summary: purpose_summary.map(str::to_owned),
            assets: assets.to_vec(),
            hidden: false,
            use_check: ExecutorUseCheck::default(),
            issued: Vec::new(),
            nonce_observation: None,
            recovery_transactions: Vec::new(),
            recovery_observation: None,
            public_account_uuid: None,
        };
        updates.extend([
            self.seal(
                RecordKind::ExecutorAllocation,
                self.allocation_key(),
                &allocation,
            )?,
            self.seal(
                RecordKind::ExecutorOperation,
                self.operation_key(operation),
                &record,
            )?,
        ]);
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(record)
    }

    /// Retain an explicitly derived historical account without assigning it to a new action.
    /// The native owner must derive the address under spend authorization first.
    pub fn restore_index(
        &self,
        index: u32,
        address: Address,
        delegate: Address,
        assets: &[ExecutorAsset],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        if index >= INDEX_LIMIT {
            return Err(ExecutorStoreError::Exhausted);
        }
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut allocation = self.allocation()?;
        if let Some(existing) = self
            .records()?
            .into_iter()
            .find(|record| record.index == index)
        {
            if existing.address.is_some_and(|known| known != address)
                || existing.delegate != delegate
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            // A reserved account may have survived a crash before address derivation.
            let mut existing = existing;
            existing.address = Some(address);
            if existing.restored_at.is_none() {
                existing.restored_at = local_timestamp();
            }
            for asset in assets {
                if !existing.assets.contains(asset) {
                    existing.assets.push(*asset);
                }
            }
            self.vault.db.put_desktop_wallet_vault_records(&[self.seal(
                RecordKind::ExecutorOperation,
                self.operation_key(existing.operation),
                &existing,
            )?])?;
            return Ok(existing);
        }
        allocation.next_index = allocation.next_index.max(index + 1);
        let mut updates = Vec::new();
        if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.index == index)
        {
            let spare = allocation
                .spare
                .take()
                .ok_or(ExecutorStoreError::InvalidRecord)?;
            if spare.address.is_some_and(|known| known != address) || spare.delegate != delegate {
                return Err(ExecutorStoreError::OperationMismatch);
            }
        } else if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.index < index)
        {
            self.retain_spare(&mut allocation, &mut updates)?;
        }
        let record = ExecutorRecord {
            version: VERSION,
            derivation: ExecutorDerivationScheme::Railgun7702V1,
            origin: ExecutorRecordOrigin::Discovered,
            operation: ExecutorOperationId::random()?,
            index,
            address: Some(address),
            delegate,
            retired: true,
            created_at: None,
            restored_at: local_timestamp(),
            purpose_summary: None,
            assets: assets.to_vec(),
            hidden: false,
            use_check: ExecutorUseCheck::default(),
            issued: Vec::new(),
            nonce_observation: None,
            recovery_transactions: Vec::new(),
            recovery_observation: None,
            public_account_uuid: None,
        };
        updates.extend([
            self.seal(
                RecordKind::ExecutorAllocation,
                self.allocation_key(),
                &allocation,
            )?,
            self.seal(
                RecordKind::ExecutorOperation,
                self.operation_key(record.operation),
                &record,
            )?,
        ]);
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(record)
    }

    /// Keep the last successful observation when a later check fails.
    pub(crate) fn record_use_check(
        &self,
        operation: ExecutorOperationId,
        observation: Option<ExecutorUseObservation>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            if let Some(observation) = observation {
                record.use_check.observation = Some(observation);
            }
            record.use_check.unavailable = observation.is_none();
            Ok(())
        })
    }

    pub fn bind_address(
        &self,
        operation: ExecutorOperationId,
        address: Address,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            if record.address.is_some_and(|existing| existing != address) {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            record.address = Some(address);
            Ok(())
        })
    }

    /// Presentation preference only; hiding cannot release or reallocate an account.
    pub fn set_hidden(
        &self,
        operation: ExecutorOperationId,
        hidden: bool,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.hidden = hidden;
            Ok(())
        })
    }

    pub fn retire(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.retired = true;
            Ok(())
        })
    }

    /// Persist before releasing signed data. A competing recovery retains both identities.
    pub fn record_issued(
        &self,
        operation: ExecutorOperationId,
        payload: IssuedExecutorPayload,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            if record.address.is_none() || payload.delegate != record.delegate {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            if record.public_account_uuid.is_some()
                && payload.purpose == ExecutorPayloadPurpose::Operation
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            // This check shares the record mutation lock with every store handle.
            // Concurrent proofs may have selected the same previously free notes.
            if self.records()?.iter().any(|other| {
                other.operation != operation
                    && other
                        .reserved_inputs()
                        .iter()
                        .any(|input| payload.context.inputs.contains(input))
            }) {
                return Err(ExecutorStoreError::InputReserved);
            }
            if record.nonce_observation != Some(payload.context.observed)
                || payload.context.observed.nonce != payload.nonce
                || payload.context.calldata.is_empty()
                || payload.purpose == ExecutorPayloadPurpose::Operation
                    && record.issued.iter().any(|issued| {
                        issued.nonce < payload.nonce && record.winner(issued.nonce).is_none()
                    })
            {
                return Err(ExecutorStoreError::OutstandingNonce);
            }
            if let Some(existing) = record
                .issued
                .iter()
                .find(|issued| issued.hash == payload.hash)
            {
                if existing.nonce != payload.nonce
                    || existing.delegate != payload.delegate
                    || existing.purpose != payload.purpose
                    || existing.context.calldata != payload.context.calldata
                    || existing.context.inputs != payload.context.inputs
                {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
            } else {
                if payload.purpose == ExecutorPayloadPurpose::Recovery {
                    record.retired = true;
                }
                record.issued.push(payload);
            }
            Ok(())
        })
    }

    /// Replace the last canonical observation, including when a reorg removes a
    /// previous winner. Missing payloads become uncertain; issued identities and
    /// transaction hashes survive. Callers verify receipts and expected effects.
    pub fn reconcile(
        &self,
        operation: ExecutorOperationId,
        observation: ExecutorNonceObservation,
        inclusions: &[(B256, ExecutorPayloadInclusion)],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.reconcile_inclusions(operation, observation.block, Some(observation), inclusions)
    }

    /// Persist verified history without granting current nonce/signing admission.
    /// The caller must revalidate all retained inclusions, as for reconciliation.
    pub(crate) fn record_history(
        &self,
        operation: ExecutorOperationId,
        block: BlockNumHash,
        inclusions: &[(B256, ExecutorPayloadInclusion)],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.reconcile_inclusions(operation, block, None, inclusions)
    }

    fn reconcile_inclusions(
        &self,
        operation: ExecutorOperationId,
        block: BlockNumHash,
        observation: Option<ExecutorNonceObservation>,
        inclusions: &[(B256, ExecutorPayloadInclusion)],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            for payload in &mut record.issued {
                payload.inclusion = None;
            }
            let mut seen = std::collections::BTreeSet::new();
            let mut winners = std::collections::BTreeSet::new();
            for (hash, inclusion) in inclusions {
                let payload = record
                    .issued
                    .iter_mut()
                    .find(|payload| payload.hash == *hash)
                    .ok_or(ExecutorStoreError::OperationMismatch)?;
                if !seen.insert(*hash) || inclusion.block.number > block.number {
                    return Err(ExecutorStoreError::InvalidRecord);
                }
                if inclusion.result == ExecutorExecutionResult::Executed
                    && (observation.is_some_and(|observed| payload.nonce >= observed.nonce)
                        || !winners.insert(payload.nonce))
                {
                    return Err(ExecutorStoreError::InvalidRecord);
                }
                payload.inclusion = Some(*inclusion);
                if !payload
                    .transaction_hashes
                    .contains(&inclusion.transaction_hash)
                {
                    payload.transaction_hashes.push(inclusion.transaction_hash);
                }
            }
            record.nonce_observation = observation;
            Ok(())
        })
    }

    /// Require fresh evidence before relying on persisted outcomes after restart,
    /// an unavailable chain read, or a new reconciliation attempt.
    pub fn invalidate_observation(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.require_reconciliation();
            Ok(())
        })
    }

    pub fn record_submission(
        &self,
        operation: ExecutorOperationId,
        payload_hash: B256,
        transaction_hash: B256,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let payload = record
                .issued
                .iter_mut()
                .find(|payload| payload.hash == payload_hash)
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if !payload.transaction_hashes.contains(&transaction_hash) {
                payload.transaction_hashes.push(transaction_hash);
            }
            Ok(())
        })
    }

    fn update(
        &self,
        operation: ExecutorOperationId,
        update: impl FnOnce(&mut ExecutorRecord) -> Result<(), ExecutorStoreError>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut record = self
            .record(operation)?
            .ok_or(ExecutorStoreError::OperationMismatch)?;
        update(&mut record)?;
        self.vault.db.put_desktop_wallet_vault_records(&[self.seal(
            RecordKind::ExecutorOperation,
            self.operation_key(operation),
            &record,
        )?])?;
        Ok(record)
    }

    fn record(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<Option<ExecutorRecord>, ExecutorStoreError> {
        let key = self.operation_key(operation);
        self.vault
            .db
            .get_desktop_wallet_vault_record(&key)?
            .map(|payload| {
                let record: ExecutorRecord =
                    self.open(RecordKind::ExecutorOperation, &key, &payload)?;
                if record.version != VERSION
                    || record.operation != operation
                    || record.index >= INDEX_LIMIT
                {
                    return Err(ExecutorStoreError::InvalidRecord);
                }
                Ok(record)
            })
            .transpose()
    }

    fn allocation(&self) -> Result<Allocation, ExecutorStoreError> {
        let key = self.allocation_key();
        let mut allocation: Allocation = self
            .vault
            .db
            .get_desktop_wallet_vault_record(&key)?
            .map(|payload| self.open(RecordKind::ExecutorAllocation, &key, &payload))
            .transpose()?
            .unwrap_or_default();
        if !(VERSION..=ALLOCATION_VERSION).contains(&allocation.version)
            || allocation.next_index > INDEX_LIMIT
            || allocation.spare.as_ref().is_some_and(|spare| {
                spare.index >= allocation.next_index
                    || !ordinary_index(spare.index).is_ok_and(|index| index == spare.index)
            })
        {
            return Err(ExecutorStoreError::InvalidRecord);
        }
        allocation.version = ALLOCATION_VERSION;
        // Retained operation records establish a lower bound even if the counter was lost.
        let records = self.records()?;
        for record in &records {
            allocation.next_index = allocation.next_index.max(record.index + 1);
        }
        // Restoring an older allocation row must not make an already claimed
        // spare reusable when newer operation records survived.
        if let Some(spare) = &allocation.spare
            && records.iter().any(|record| record.index >= spare.index)
        {
            let mut updates = Vec::new();
            if records.iter().any(|record| record.index == spare.index) {
                allocation.spare = None;
            } else {
                self.retain_spare(&mut allocation, &mut updates)?;
            }
            updates.push(self.seal(RecordKind::ExecutorAllocation, key, &allocation)?);
            self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        }
        Ok(allocation)
    }

    fn require_wallet(&self) -> Result<(), ExecutorStoreError> {
        if self
            .vault
            .db
            .get_desktop_wallet_vault_record(&wallet_view_record_key(self.view.wallet_id()))?
            .is_none()
        {
            return Err(ExecutorStoreError::Unavailable);
        }
        Ok(())
    }

    fn allocation_key(&self) -> String {
        executor_allocation_key(self.view.wallet_id(), self.chain_id)
    }
    fn operation_key(&self, operation: ExecutorOperationId) -> String {
        format!(
            "{}{}",
            executor_operation_prefix(self.view.wallet_id(), self.chain_id),
            operation.opaque_id()
        )
    }
    fn aad_id(&self, key: &str) -> String {
        format!(
            "{}:{}:{}:{}",
            self.view.wallet_id().len(),
            self.view.wallet_id(),
            self.chain_id,
            key
        )
    }
    fn seal<T: Serialize>(
        &self,
        kind: RecordKind,
        key: String,
        value: &T,
    ) -> Result<(String, Vec<u8>), ExecutorStoreError> {
        let plaintext = Zeroizing::new(rmp_serde::to_vec_named(value)?);
        Ok(self
            .view
            .private_view
            .encrypt_record(kind, &self.aad_id(&key), &plaintext)?
            .to_record_entry(key)?)
    }
    fn open<T: serde::de::DeserializeOwned>(
        &self,
        kind: RecordKind,
        key: &str,
        payload: &[u8],
    ) -> Result<T, ExecutorStoreError> {
        let record: EncryptedRecord = rmp_serde::from_slice(payload)?;
        let plaintext = self
            .view
            .private_view
            .decrypt_record(kind, &self.aad_id(key), &record)?;
        Ok(rmp_serde::from_slice(&plaintext)?)
    }
}

// The derivation family is wallet + chain, independent of private-sync contract/cache settings.
pub(super) fn executor_wallet_prefix(wallet_id: &str) -> String {
    format!("executor|{}:{wallet_id}|", wallet_id.len())
}
pub(super) fn executor_allocation_key(wallet_id: &str, chain_id: u64) -> String {
    format!("{}{chain_id}|allocation", executor_wallet_prefix(wallet_id))
}
pub(super) fn executor_operation_prefix(wallet_id: &str, chain_id: u64) -> String {
    format!("{}{chain_id}|operation|", executor_wallet_prefix(wallet_id))
}

fn ordinary_index(index: u32) -> Result<u32, ExecutorStoreError> {
    let index = if (POSITION_START..=POSITION_END).contains(&index) {
        POSITION_END + 1
    } else {
        index
    };
    if index >= INDEX_LIMIT {
        Err(ExecutorStoreError::Exhausted)
    } else {
        Ok(index)
    }
}
