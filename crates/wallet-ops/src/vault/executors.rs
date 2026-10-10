use std::sync::{Arc, Mutex};

use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, FixedBytes, U256};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

mod public_account;
mod public_swap;
mod spare;
mod swap;
mod swap_admission;
mod swap_use;
pub use public_swap::*;
pub(crate) use spare::ExecutorSpare;
pub use swap::*;
pub use swap_admission::*;
pub use swap_use::*;

use super::{
    DesktopVaultStore, DesktopViewSession, EncryptedRecord, RecordKind, VaultError,
    wallet_view_record_key,
};

/// Records without swap-use metadata keep this version, which earlier builds read.
const LEGACY_VERSION: u32 = 1;
/// A record with swap-use metadata. Earlier builds refuse it rather than ignore its claim.
const VERSION: u32 = 2;
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
    #[error("a previous swap attempt's pre-hook can still execute")]
    SwapAttemptOutstanding,
    #[error("this account is reserved by another swap")]
    SwapUseActive,
    #[error("a Public account shared between Private wallets can't pay for a swap")]
    PublicSwapSourceShared,
    /// Another swap paid from the same Public account on the same chain can still buy the
    /// token. `chain_id` is the chain it delivers to, whose store holds it. `available_at` is
    /// set, in Unix seconds, when only its hook batch's deadline keeps it open.
    #[error("another swap from this Public account can still buy the same token")]
    PublicSwapBuysSameToken {
        swap: SwapUseId,
        chain_id: u64,
        available_at: Option<u64>,
    },
    /// Another swap's order from the same Public account on the same chain can still sell the
    /// token. `expires_at` is that order's `validTo`, once it is placed.
    #[error("another swap's order from this Public account can still sell the same token")]
    PublicSwapSellsSameToken {
        swap: SwapUseId,
        chain_id: u64,
        expires_at: Option<u64>,
    },
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
    /// Also covers swap cancellation, which is recovery with no assets.
    Recovery,
    /// A swap order's pre-hook at the current nonce, recorded only with its order.
    SwapPreHook,
    /// A swap order's post-hook at the pre-hook's nonce plus one.
    SwapPostHook,
    /// A destination stealth account's guarded shield at its current nonce, run by the Across
    /// handler inside the fill.
    SwapDestinationShield,
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

/// Purpose summary of a private Bridge swap's destination stealth account.
pub const SWAP_DESTINATION_PURPOSE_SUMMARY: &str = "Private swap destination";

/// What a destination stealth account serves, kept in its own record on the destination chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapDestinationRecord {
    pub origin_chain: u64,
    pub origin_operation: ExecutorOperationId,
    pub destination_token: Address,
    /// What became of the account's shield payload, from the origin swap's bridge outcome.
    #[serde(default)]
    pub outcome: Option<SwapDestinationOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapDestinationOutcome {
    /// The fill ran the shield payload.
    Shielded {
        block: BlockNumHash,
        transaction_hash: B256,
    },
    /// The fill completed without the shield. The account holds the token and the payload can
    /// still run.
    Held {
        block: BlockNumHash,
        transaction_hash: B256,
    },
    /// The provider reported expiry or refund. A later verified fill can correct this;
    /// this status does not revoke the destination shield's execution signature.
    Unfilled,
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

/// The execution nonce below which every nonce is consumed, with the number of the block
/// that showed it. Private sync reports a spend's block number without its hash, so no
/// hash is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorNonceWatermark {
    nonce: U256,
    block: u64,
}

impl ExecutorNonceWatermark {
    #[must_use]
    pub const fn new(nonce: U256, block: u64) -> Self {
        Self { nonce, block }
    }
    #[must_use]
    pub const fn nonce(self) -> U256 {
        self.nonce
    }
    #[must_use]
    pub const fn block(self) -> u64 {
        self.block
    }
}

impl From<ExecutorNonceObservation> for ExecutorNonceWatermark {
    fn from(observed: ExecutorNonceObservation) -> Self {
        Self::new(observed.nonce, observed.block.number)
    }
}

/// Whether a signed payload can still execute, from [`ExecutorRecord::nonce_watermark`]
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorPayloadState {
    /// The payload's nonce is not known to be consumed.
    Pending,
    /// The execution nonce is past the payload's, so its signature can no longer execute.
    Resolved,
}

/// The full issued call retains expected private transactions and recovery actions.
/// It stays encrypted. A context stored by an earlier version may carry a `history_start`
/// key, which is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorPayloadContext {
    calldata: Bytes,
    observed: ExecutorNonceObservation,
    inputs: Vec<ExecutorInputIdentity>,
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
}

/// What an earlier version's block scan recorded for a payload. Nothing writes one any
/// more; see [`ExecutorPayloadInclusion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ExecutorExecutionResult {
    Reverted,
    MissingEffects,
    Executed,
}

/// A payload's inclusion as an earlier version stored it. It is compatibility data: kept
/// as stored, never written, and read only where an `Executed` one places
/// [`ExecutorRecord::nonce_watermark`] for a record without a stored watermark and names
/// the payload that ran at its nonce for a stopped swap destination's delivery guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ExecutorPayloadInclusion {
    block: BlockNumHash,
    transaction_hash: B256,
    result: ExecutorExecutionResult,
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

/// Stored through `ExecutorRecordWire`, which keeps the field layout earlier builds wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "swap_use::ExecutorRecordWire",
    into = "swap_use::ExecutorRecordWire"
)]
pub struct ExecutorRecord {
    /// The format this record is stored in, which follows from its swap-use metadata.
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
    assets: Vec<ExecutorAsset>,
    hidden: bool,
    use_check: ExecutorUseCheck,
    issued: Vec<IssuedExecutorPayload>,
    nonce_observation: Option<ExecutorNonceObservation>,
    /// The last confirmed nonce read under a readable layout, with its block number. Unlike
    /// `nonce_observation` it survives [`Self::require_reconciliation`]. Records from
    /// earlier builds have none; see [`Self::nonce_watermark`].
    nonce_watermark: Option<ExecutorNonceWatermark>,
    /// The nonce below which private sync has scanned past the block that resolved each
    /// nonce, so payloads there stop reserving their notes. It only rises, except that a
    /// read lowering the watermark below it lowers it too. With none, nothing is settled.
    settled_nonce: Option<U256>,
    public_account_uuid: Option<String>,
    swap: Option<SwapOperationRecord>,
    swap_setup_stopped: bool,
    /// Issued payloads the user released from input reservation. Their signatures
    /// remain valid; only the wallet's own spending of their inputs changes.
    released_payloads: Vec<B256>,
    /// The swaps this account took part in, in the order they started. A use is never removed.
    swap_uses: Vec<SwapUseRecord>,
    /// The one swap use that claims this account.
    active_swap_use: Option<SwapUseId>,
}

impl ExecutorRecord {
    /// Whether a signed payload can still execute. An issued payload is pending until the
    /// watermark passes its nonce: a reverted executor call still has a replayable execution
    /// signature, and a bridge refund does not revoke a destination shield's signature.
    #[must_use]
    pub fn has_unresolved_issued_work(&self) -> bool {
        self.issued().iter().any(|payload| {
            !self.nonce_resolved(payload.nonce) && !self.swap_hook_nonce_passed(payload)
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

    /// The nonce below which every execution nonce is consumed, with the block that
    /// showed it. A record without the stored field takes the highest of its nonce
    /// observation and one past each payload with an executed inclusion, at that
    /// inclusion's block.
    #[must_use]
    pub fn nonce_watermark(&self) -> Option<ExecutorNonceWatermark> {
        self.nonce_watermark.or_else(|| {
            self.issued
                .iter()
                .filter_map(|payload| {
                    let inclusion = payload.inclusion?;
                    (inclusion.result == ExecutorExecutionResult::Executed).then(|| {
                        ExecutorNonceWatermark::new(
                            payload.nonce.saturating_add(U256::ONE),
                            inclusion.block.number,
                        )
                    })
                })
                .chain(self.nonce_observation.map(ExecutorNonceWatermark::from))
                .max_by_key(|watermark| watermark.nonce)
        })
    }

    /// Whether `nonce` lies below [`Self::nonce_watermark`], so that no payload signed
    /// at it can still execute.
    #[must_use]
    pub fn nonce_resolved(&self, nonce: U256) -> bool {
        self.nonce_watermark()
            .is_some_and(|watermark| nonce < watermark.nonce)
    }

    /// Whether the issued payload `hash` is pending or resolved. This reads the watermark
    /// only, so it holds without a current nonce observation and names no winner.
    #[must_use]
    pub fn payload_state(&self, hash: B256) -> Option<ExecutorPayloadState> {
        let payload = self.issued.iter().find(|payload| payload.hash == hash)?;
        Some(if self.nonce_resolved(payload.nonce) {
            ExecutorPayloadState::Resolved
        } else {
            ExecutorPayloadState::Pending
        })
    }

    /// The nonce below which notes follow private sync alone. `None` when nothing is settled.
    #[must_use]
    pub const fn settled_nonce(&self) -> Option<U256> {
        self.settled_nonce
    }

    /// Whether `nonce` is resolved and private sync has scanned past the block that
    /// resolved it. Payloads signed at it no longer reserve their notes: private sync's
    /// spent status is the only record of which were spent.
    #[must_use]
    pub fn nonce_settled(&self, nonce: U256) -> bool {
        self.nonce_resolved(nonce) && self.settled_nonce.is_some_and(|settled| nonce < settled)
    }

    /// Store a confirmed nonce read. A read below the settled nonce shows those nonces
    /// unconsumed again, so the settled nonce falls with it.
    fn set_nonce_watermark(&mut self, watermark: ExecutorNonceWatermark) {
        self.nonce_watermark = Some(watermark);
        if self
            .settled_nonce
            .is_some_and(|settled| watermark.nonce < settled)
        {
            self.settled_nonce = Some(watermark.nonce);
        }
    }

    /// Take private sync's evidence from a snapshot scanned to `last_scanned`. `resolved`
    /// is one past the nonce of a pending payload that sync itself showed executed, with
    /// that block: it only ever raises the watermark, and is settled at once, since sync has
    /// scanned that block. Every nonce the watermark resolved at a block sync has reached is
    /// settled too. This is not an account read, so `nonce_observation` stays as it is.
    pub(crate) fn apply_synced(
        &mut self,
        resolved: Option<ExecutorNonceWatermark>,
        last_scanned: u64,
    ) {
        let mut settled = self.settled_nonce;
        if let Some(resolved) = resolved
            && self
                .nonce_watermark()
                .is_none_or(|watermark| watermark.nonce < resolved.nonce)
        {
            self.nonce_watermark = Some(resolved);
            settled = settled.max(Some(resolved.nonce));
        }
        if let Some(watermark) = self.nonce_watermark()
            && watermark.block <= last_scanned
        {
            settled = settled.max(Some(watermark.nonce));
        }
        self.settled_nonce = settled;
    }

    pub(crate) fn require_reconciliation(&mut self) {
        self.nonce_watermark = self.nonce_watermark();
        self.nonce_observation = None;
    }

    /// The payload an earlier version's block scan stored as executed at `nonce`. Nothing
    /// stores one any more, so a record written by this version has none.
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

    /// A reverted attempt does not revoke its signed payload or free its private inputs.
    /// Every payload at a resolved nonce, whichever of them ran, keeps its inputs
    /// protected until [`Self::nonce_settled`]; from then on private sync's spent status
    /// rules. A swap pre-hook whose order ended while its nonce stayed unconsumed can never
    /// run and releases its inputs. A payload the user explicitly released no longer
    /// reserves its inputs. This needs no private sync snapshot.
    #[must_use]
    pub fn reserved_inputs(&self) -> Vec<ExecutorInputIdentity> {
        self.reserving_payloads()
            .flat_map(|payload| payload.context.inputs.iter().cloned())
            .fold(Vec::new(), |mut inputs, input| {
                if !inputs.contains(&input) {
                    inputs.push(input);
                }
                inputs
            })
    }

    /// Issued payloads whose inputs [`Self::reserved_inputs`] reserves. A payload the
    /// user released reserves again once the wallet issues or sends it again.
    pub fn reserving_payloads(&self) -> impl Iterator<Item = &IssuedExecutorPayload> {
        self.reserving_payloads_before_release()
            .filter(|payload| !self.released_payloads.contains(&payload.hash))
    }

    fn reserving_payloads_before_release(&self) -> impl Iterator<Item = &IssuedExecutorPayload> {
        self.issued.iter().filter(|payload| {
            if self.nonce_resolved(payload.nonce) {
                return !self.nonce_settled(payload.nonce);
            }
            payload.purpose != ExecutorPayloadPurpose::SwapPreHook
                || !self.releases_swap_inputs(payload.hash)
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

/// An exact expectation includes an account with no active use; unchecked issuance does not.
#[derive(Clone, Copy)]
enum SwapUseExpectation {
    Unchecked,
    Exact(Option<SwapUseId>),
}

impl SwapUseExpectation {
    fn matches(&self, active: Option<SwapUseId>) -> bool {
        match self {
            Self::Unchecked => true,
            Self::Exact(expected) => *expected == active,
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
                if !(LEGACY_VERSION..=VERSION).contains(&record.version)
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
        self.reserve_with_swap_approval(operation, delegate, purpose_summary, assets, None, None)
    }

    /// [`Self::reserve`] that stores `swap_approval` and the link to the swap's destination
    /// stealth account in the write that creates the record, so a new swap's record never
    /// exists without its approved terms. An existing record with the same link is returned
    /// unchanged, with its own approval.
    pub fn reserve_with_swap_approval(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        purpose_summary: Option<&str>,
        assets: &[ExecutorAsset],
        swap_approval: Option<SwapApproval>,
        destination_operation: Option<ExecutorOperationId>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.reserve_record(
            operation,
            delegate,
            purpose_summary,
            assets,
            ReservationLinks {
                swap_approval,
                destination_operation,
                swap_destination: None,
            },
        )
    }

    /// [`Self::reserve`] for a private Bridge swap's destination stealth account on this chain.
    /// The destination token is the account's recoverable asset. An existing record is returned
    /// unchanged only if it serves the same origin swap and token.
    pub fn reserve_swap_destination(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        destination: SwapDestinationRecord,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        if destination.origin_chain == self.chain_id {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        self.reserve_record(
            operation,
            delegate,
            Some(SWAP_DESTINATION_PURPOSE_SUMMARY),
            &[ExecutorAsset::Erc20(destination.destination_token)],
            ReservationLinks {
                swap_approval: None,
                destination_operation: None,
                swap_destination: Some(SwapDestinationRecord {
                    outcome: None,
                    ..destination
                }),
            },
        )
    }

    fn reserve_record(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        purpose_summary: Option<&str>,
        assets: &[ExecutorAsset],
        links: ReservationLinks,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let ReservationLinks {
            swap_approval,
            destination_operation,
            swap_destination,
        } = links;
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        if let Some(existing) = self.record(operation)? {
            // A destination record's outcome changes after its reservation.
            let existing_destination =
                existing
                    .swap_destination()
                    .map(|destination| SwapDestinationRecord {
                        outcome: None,
                        ..destination
                    });
            return if existing.delegate == delegate
                && existing.origin == ExecutorRecordOrigin::Reserved
                && existing.destination_operation() == destination_operation
                && existing_destination == swap_destination
            {
                Ok(existing)
            } else {
                Err(ExecutorStoreError::OperationMismatch)
            };
        }
        // A swap's reservation is its account's first use. Both accounts of a private Bridge
        // swap take the origin's identity for it.
        let first_use = if let Some(destination) = swap_destination {
            Some((
                SwapUseId::first(destination.origin_operation),
                SwapUseRole::Destination {
                    origin_chain: destination.origin_chain,
                    origin_operation: destination.origin_operation,
                    destination_token: destination.destination_token,
                    shields: Vec::new(),
                    outcome: None,
                },
            ))
        } else if swap_approval.is_some() || destination_operation.is_some() {
            Some((
                SwapUseId::first(operation),
                SwapUseRole::Source {
                    approval: swap_approval.map(Box::new),
                    destination_operation,
                },
            ))
        } else {
            None
        };
        let mut updates = Vec::new();
        let record = self.allocate_record(
            operation,
            delegate,
            purpose_summary,
            assets,
            first_use,
            &mut updates,
        )?;
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(record)
    }

    /// Allocate a fresh account for `operation`, which has no record yet, and append its
    /// allocation row and record to `updates` without writing them. The caller holds the record
    /// lock and commits `updates` in one write.
    fn allocate_record(
        &self,
        operation: ExecutorOperationId,
        delegate: Address,
        purpose_summary: Option<&str>,
        assets: &[ExecutorAsset],
        first_use: Option<(SwapUseId, SwapUseRole)>,
        updates: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let mut allocation = self.allocation()?;
        if allocation
            .spare
            .as_ref()
            .is_some_and(|spare| spare.delegate != delegate)
        {
            self.retain_spare(&mut allocation, updates)?;
        }
        let (index, address) = if let Some(spare) = allocation.spare.take() {
            (spare.index, spare.address)
        } else {
            let index = ordinary_index(allocation.next_index)?;
            allocation.next_index = index + 1;
            (index, None)
        };
        let mut record = ExecutorRecord {
            version: LEGACY_VERSION,
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
            nonce_watermark: None,
            settled_nonce: None,
            public_account_uuid: None,
            swap: None,
            swap_setup_stopped: false,
            released_payloads: Vec::new(),
            swap_uses: Vec::new(),
            active_swap_use: None,
        };
        if let Some((id, role)) = first_use {
            record.begin_first_swap_use(id, role);
        }
        record.version = record.format_version();
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
            version: LEGACY_VERSION,
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
            nonce_watermark: None,
            settled_nonce: None,
            public_account_uuid: None,
            swap: None,
            swap_setup_stopped: false,
            released_payloads: Vec::new(),
            swap_uses: Vec::new(),
            active_swap_use: None,
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
        self.record_issued_for(operation, SwapUseExpectation::Unchecked, payload)
    }

    /// [`Self::record_issued`] for a destination stealth account's shield, which is signed for
    /// the swap use `use_id`. It is refused unless that use still claims the account, checked in
    /// the write that persists the payload.
    pub fn record_swap_destination_shield(
        &self,
        operation: ExecutorOperationId,
        use_id: SwapUseId,
        payload: IssuedExecutorPayload,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        if payload.purpose != ExecutorPayloadPurpose::SwapDestinationShield {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        self.record_issued_for(operation, SwapUseExpectation::Exact(Some(use_id)), payload)
    }

    /// Persist a reviewed recovery only while its captured swap claim still matches.
    pub(crate) fn record_recovery_issued(
        &self,
        operation: ExecutorOperationId,
        expected_active_use: Option<SwapUseId>,
        payload: IssuedExecutorPayload,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        if payload.purpose != ExecutorPayloadPurpose::Recovery {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        self.record_issued_for(
            operation,
            SwapUseExpectation::Exact(expected_active_use),
            payload,
        )
    }

    fn record_issued_for(
        &self,
        operation: ExecutorOperationId,
        expected_active_use: SwapUseExpectation,
        payload: IssuedExecutorPayload,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let (purpose, hash) = (payload.purpose, payload.hash);
        self.update(operation, |record| {
            if !expected_active_use.matches(record.active_swap_use) {
                return Err(ExecutorStoreError::SwapUseActive);
            }
            if record.address.is_none() || payload.delegate != record.delegate {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            // A destination shield belongs to the account's active destination use, whose
            // origin swap on the other chain must name this account for the same use. A swap
            // paid from a Public account has no origin record to name it.
            let shield_use = if purpose == ExecutorPayloadPurpose::SwapDestinationShield {
                let Some(SwapUseRecord {
                    id,
                    stopped: false,
                    role,
                    ..
                }) = record.active_use()
                else {
                    return Err(ExecutorStoreError::OperationMismatch);
                };
                match role {
                    SwapUseRole::Destination {
                        origin_chain,
                        origin_operation,
                        ..
                    } => {
                        if !self
                            .for_chain(*origin_chain)
                            .record(*origin_operation)?
                            .is_some_and(|origin| origin.links_swap_destination(*id, operation))
                        {
                            return Err(ExecutorStoreError::OperationMismatch);
                        }
                    }
                    SwapUseRole::PublicSourceDestination { .. } => {}
                    SwapUseRole::Source { .. } => {
                        return Err(ExecutorStoreError::OperationMismatch);
                    }
                }
                Some(*id)
            } else {
                None
            };
            // A setup belongs to the swap use that reserved its account fresh, until that use
            // is stopped. An account a later use claims is already set up.
            let setup_closed = record
                .active_use()
                .is_some_and(|swap_use| swap_use.is_stopped() || !swap_use.is_fresh());
            // Swap hooks are recorded only together with their order. A destination shield
            // belongs to a live destination account and spends no private inputs.
            if (record.public_account_uuid.is_some() || record.swap_setup_stopped || setup_closed)
                && payload.purpose == ExecutorPayloadPurpose::Operation
                || matches!(
                    payload.purpose,
                    ExecutorPayloadPurpose::SwapPreHook | ExecutorPayloadPurpose::SwapPostHook
                )
                || payload.purpose == ExecutorPayloadPurpose::SwapDestinationShield
                    && (record.retired
                        || record.swap_setup_stopped
                        || !payload.context.inputs.is_empty())
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
            // Recovery, including early cancellation, competes with this swap's own pre-hook.
            if payload.purpose == ExecutorPayloadPurpose::Recovery
                && record
                    .swap_reserved_inputs()
                    .iter()
                    .any(|input| payload.context.inputs.contains(input))
            {
                return Err(ExecutorStoreError::InputReserved);
            }
            if record.nonce_observation != Some(payload.context.observed)
                || payload.context.observed.nonce != payload.nonce
                || payload.context.calldata.is_empty()
                || payload.purpose == ExecutorPayloadPurpose::Operation
                    && record.issued.iter().any(|issued| {
                        issued.nonce < payload.nonce && !record.nonce_resolved(issued.nonce)
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
                // Recording a released payload again means it is being sent again.
                record
                    .released_payloads
                    .retain(|hash| *hash != payload.hash);
            } else {
                if payload.purpose == ExecutorPayloadPurpose::Recovery {
                    record.retired = true;
                }
                record.issued.push(payload);
            }
            if let Some(id) = shield_use
                && let Some(SwapUseRecord {
                    role:
                        SwapUseRole::Destination {
                            shields, outcome, ..
                        }
                        | SwapUseRole::PublicSourceDestination {
                            shields, outcome, ..
                        },
                    ..
                }) = record.swap_use_mut(id)
            {
                if !shields.contains(&hash) {
                    shields.push(hash);
                }
                // A newly signed order's fill can fund the shield again.
                if *outcome == Some(SwapDestinationOutcome::Unfilled) {
                    *outcome = None;
                }
            }
            Ok(())
        })
    }

    /// Bring this chain's destination stealth accounts in line with their origin swaps on
    /// other chains, in one write. Unlinked reservations may still be in preparation and are
    /// left alone. A destination whose swap setup was stopped before any outcome is stopped
    /// and retired too, when that swap reserved it fresh; an account the swap reused only has
    /// that use stopped. Otherwise the origin's bridge outcomes decide what became of the
    /// shield payload, for each destination use from the orders of that use alone. A stopped,
    /// orderless origin releases unsigned reused claims through cancellation. Returns whether
    /// any record changed.
    pub fn reconcile_swap_destinations(&self) -> Result<bool, ExecutorStoreError> {
        self.reconcile_swap_destinations_inner(false)
    }

    /// Reconcile before the chain's owner admits work. An unlinked reservation left by a
    /// crash between the two reservations can be retired here while it has issued nothing.
    pub(crate) fn reconcile_swap_destinations_on_load(&self) -> Result<bool, ExecutorStoreError> {
        self.reconcile_swap_destinations_inner(true)
    }

    fn reconcile_swap_destinations_inner(
        &self,
        retire_orphans: bool,
    ) -> Result<bool, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut updates = Vec::new();
        let mut origins = std::collections::BTreeMap::new();
        for mut record in self.records()? {
            let previous = record.clone();
            // Every destination use is matched to its own origin use, so an earlier swap's fill
            // is never read as a later swap's.
            let uses: Vec<_> = record
                .swap_uses()
                .iter()
                .filter_map(destination_link)
                .collect();
            for (id, origin_chain, origin_operation, known) in uses {
                // Only the use that reserved an account fresh, while it still claims the
                // account, can retire it or stop its setup.
                let fresh = record.active_swap_use == Some(id)
                    && record.swap_use(id).is_some_and(SwapUseRecord::is_fresh);
                let key = (origin_chain, origin_operation);
                if let std::collections::btree_map::Entry::Vacant(entry) = origins.entry(key) {
                    entry.insert(self.for_chain(origin_chain).record(origin_operation)?);
                }
                let origin = origins
                    .get_mut(&key)
                    .and_then(Option::as_mut)
                    .filter(|origin| origin.links_swap_destination(id, record.operation));
                let next = match origin {
                    None => {
                        record.retired |= retire_orphans && fresh && record.issued.is_empty();
                        None
                    }
                    Some(origin) => {
                        let setup_stopped = origin.swap_setup_stopped && known.is_none();
                        let stopped = setup_stopped
                            || origin.swap_use(id).is_some_and(SwapUseRecord::is_stopped);
                        if stopped && !origin.has_swap_use_order(id) {
                            // Both facts come from the exact linked source while the mutation
                            // lock excludes any late order or signed-data handoff.
                            origin.cancel_swap_use(id);
                            record.cancel_swap_use(id);
                        } else if setup_stopped && fresh {
                            record.swap_setup_stopped = true;
                            record.retired = true;
                        }
                        let derived = record
                            .address
                            .and_then(|address| origin.swap_destination_outcome(id, address));
                        Some((stopped, next_destination_outcome(known, derived)))
                    }
                };
                if let Some((stop, next)) = next
                    && let Some(SwapUseRecord {
                        stopped,
                        role:
                            SwapUseRole::Destination {
                                shields, outcome, ..
                            },
                        ..
                    }) = record.swap_use_mut(id)
                {
                    *stopped |= stop;
                    // A fill can only have run, or left funds for, a shield this use signed.
                    let filled = matches!(
                        next,
                        Some(
                            SwapDestinationOutcome::Shielded { .. }
                                | SwapDestinationOutcome::Held { .. }
                        )
                    );
                    if !(filled && shields.is_empty()) {
                        *outcome = next;
                    }
                }
            }
            if record != previous {
                updates.push(self.seal(
                    RecordKind::ExecutorOperation,
                    self.operation_key(record.operation),
                    &record,
                )?);
            }
        }
        for ((chain_id, operation), origin) in origins {
            let Some(origin) = origin else {
                continue;
            };
            let store = self.for_chain(chain_id);
            if store.record(operation)?.as_ref() != Some(&origin) {
                updates.push(store.seal(
                    RecordKind::ExecutorOperation,
                    store.operation_key(operation),
                    &origin,
                )?);
            }
        }
        if updates.is_empty() {
            return Ok(false);
        }
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(true)
    }

    /// The swap record on its own chain that names this chain's destination stealth account
    /// `operation` as the destination of the use that claims it. `None` when no destination use
    /// claims `operation` or no swap references it.
    pub(crate) fn swap_destination_origin(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<Option<ExecutorRecord>, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let Some((id, origin_chain, origin_operation, _)) = self
            .record(operation)?
            .and_then(|record| record.active_use().and_then(destination_link))
        else {
            return Ok(None);
        };
        Ok(self
            .for_chain(origin_chain)
            .record(origin_operation)?
            .filter(|origin| origin.links_swap_destination(id, operation)))
    }

    /// The same wallet's store for another chain. It takes no lock: read through it with
    /// `record` or `records` while the caller holds the record lock.
    fn for_chain(&self, chain_id: u64) -> Self {
        Self {
            vault: DesktopVaultStore::from_db(self.vault.db()),
            view: Arc::clone(&self.view),
            chain_id,
        }
    }

    /// Every record of this wallet with its chain, on every chain that holds executor data. It
    /// takes no lock, like `for_chain`. Each chain's records are read as `records` reads them,
    /// so one that fails to decode fails the listing.
    fn wallet_records(&self) -> Result<Vec<(u64, ExecutorRecord)>, ExecutorStoreError> {
        let prefix = executor_wallet_prefix(self.view.wallet_id());
        let mut chains = std::collections::BTreeSet::new();
        for stored in self.vault.db.list_desktop_wallet_vault_records(&prefix)? {
            // The chain follows the wallet prefix in every executor key.
            let chain_id = stored
                .key
                .strip_prefix(&prefix)
                .and_then(|key| key.split_once('|'))
                .and_then(|(chain_id, _)| chain_id.parse::<u64>().ok())
                .ok_or(ExecutorStoreError::InvalidRecord)?;
            chains.insert(chain_id);
        }
        let mut records = Vec::new();
        for chain_id in chains {
            let chain_records = self.for_chain(chain_id).records()?;
            records.extend(chain_records.into_iter().map(|record| (chain_id, record)));
        }
        Ok(records)
    }

    /// Persist what private sync's snapshot, scanned to `last_scanned`, shows about a
    /// record, in one write: see [`ExecutorRecord::apply_synced`]. It grants no current
    /// nonce or signing admission.
    pub(crate) fn record_synced(
        &self,
        operation: ExecutorOperationId,
        resolved: Option<ExecutorNonceWatermark>,
        last_scanned: u64,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.apply_synced(resolved, last_scanned);
            Ok(())
        })
    }

    /// Persist a confirmed read of the account's execution nonce: the nonce observation
    /// and the watermark. Stored inclusions are left as they are and no longer consulted.
    /// The read is applied to the record as it is now. One taken at a block below a read
    /// or a resolution already recorded says nothing newer: it leaves the watermark alone,
    /// and stands as the nonce observation only while it agrees with the watermark, as a
    /// signing inspection at a slightly older block does.
    pub fn record_account_read(
        &self,
        operation: ExecutorOperationId,
        observation: ExecutorNonceObservation,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let watermark = record.nonce_watermark();
            let recorded = record
                .nonce_observation
                .map(|observed| observed.block.number)
                .max(watermark.map(|watermark| watermark.block));
            if recorded.is_none_or(|recorded| recorded <= observation.block.number) {
                record.nonce_observation = Some(observation);
                record.set_nonce_watermark(observation.into());
            } else if record.nonce_observation.is_none()
                && watermark.is_some_and(|watermark| watermark.nonce == observation.nonce)
            {
                record.nonce_observation = Some(observation);
            }
            Ok(())
        })
    }

    /// Require fresh evidence for a new reconciliation attempt or after an unavailable
    /// chain read. Restart alone retains the confirmed observation.
    pub fn invalidate_observation(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            record.require_reconciliation();
            Ok(())
        })
    }

    /// The user's explicit release of issued payloads' input reservations. The signed
    /// payloads can still execute; whichever transaction spends the notes first wins.
    /// Payloads the wallet issues or sends again afterwards reserve their inputs again.
    pub(crate) fn release_payloads(
        &self,
        operation: ExecutorOperationId,
        payloads: &[B256],
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            for hash in payloads {
                if !record.issued.iter().any(|payload| payload.hash == *hash) {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
                if !record.released_payloads.contains(hash) {
                    record.released_payloads.push(*hash);
                }
            }
            Ok(())
        })
    }

    /// The wallet is sending a released payload again, so it reserves its inputs again.
    /// Refused while another operation reserves any of those inputs.
    pub(crate) fn reserve_released(
        &self,
        operation: ExecutorOperationId,
        payload: B256,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let inputs = record
                .issued
                .iter()
                .find(|issued| issued.hash == payload)
                .map(|issued| issued.context.inputs.clone())
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if !record.released_payloads.contains(&payload) {
                return Ok(());
            }
            // Same predicate as `record_issued`, under the same record mutation lock.
            if self.records()?.iter().any(|other| {
                other.operation != operation
                    && other
                        .reserved_inputs()
                        .iter()
                        .any(|input| inputs.contains(input))
            }) {
                return Err(ExecutorStoreError::InputReserved);
            }
            record.released_payloads.retain(|hash| *hash != payload);
            Ok(())
        })
    }

    /// A send by this wallet reserves the payload's inputs again, even when another
    /// operation selected them after the release.
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
            record
                .released_payloads
                .retain(|hash| *hash != payload_hash);
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
        // A record that took its first swap use is stored, and compared, in the new format.
        record.version = record.format_version();
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
                if !(LEGACY_VERSION..=VERSION).contains(&record.version)
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
        if !(LEGACY_VERSION..=ALLOCATION_VERSION).contains(&allocation.version)
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
        self.vault.validate_executor_source(&self.view)?;
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

/// What a reservation stores beside the account's purpose and assets.
struct ReservationLinks {
    swap_approval: Option<SwapApproval>,
    destination_operation: Option<ExecutorOperationId>,
    swap_destination: Option<SwapDestinationRecord>,
}

/// A destination use's identity, its origin swap's chain and operation, and its outcome. A use
/// paid from a Public account has no origin swap.
const fn destination_link(
    swap_use: &SwapUseRecord,
) -> Option<(
    SwapUseId,
    u64,
    ExecutorOperationId,
    Option<SwapDestinationOutcome>,
)> {
    match swap_use.role() {
        SwapUseRole::Destination {
            origin_chain,
            origin_operation,
            outcome,
            ..
        } => Some((swap_use.id(), *origin_chain, *origin_operation, *outcome)),
        SwapUseRole::Source { .. } | SwapUseRole::PublicSourceDestination { .. } => None,
    }
}

/// A fill's outcome is never replaced. `Unfilled` gives way to a fill, since Across can fill a
/// deposit it reported expired, and is cleared once a retry issues a new order.
const fn next_destination_outcome(
    known: Option<SwapDestinationOutcome>,
    derived: Option<SwapDestinationOutcome>,
) -> Option<SwapDestinationOutcome> {
    match known {
        Some(SwapDestinationOutcome::Shielded { .. } | SwapDestinationOutcome::Held { .. }) => {
            known
        }
        Some(SwapDestinationOutcome::Unfilled) | None => derived,
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
