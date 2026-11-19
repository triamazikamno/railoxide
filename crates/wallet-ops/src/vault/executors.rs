use std::sync::{Arc, Mutex};

use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, FixedBytes, U256};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

mod allocation;
mod observations;
mod public_account;
mod public_swap;
mod record_updates;
mod spare;
mod swap;
mod swap_admission;
mod swap_destinations;
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
use crate::DesktopWalletVaultDbExt as _;

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
    #[error("this order can no longer be sped up; review the swap again")]
    SwapSpeedUpRefused,
    #[error(
        "this swap's earlier attempts were approved for other tokens or delivery terms; start a new swap to change them"
    )]
    SwapUseTermsChanged,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapDestinationRecord {
    pub origin_chain: u64,
    pub origin_operation: ExecutorOperationId,
    pub destination_token: Address,
    /// What became of the account's shield payload, from the origin swap's bridge outcome.
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
/// [`ExecutorRecord::nonce_watermark`] for a record without a stored watermark.
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
    /// The active use's setup was stopped, so the account issues no further swap work. The
    /// stopped use keeps its own flag; a later use admitted onto a cancelled fresh source
    /// clears this one in the write that claims the account.
    swap_setup_stopped: bool,
    /// Issued payloads the user released from input reservation. Their signatures
    /// remain valid; only the wallet's own spending of their inputs changes.
    released_payloads: Vec<B256>,
    /// The swaps this account took part in, in the order they started. A use is removed only
    /// from a retired account that a replacement took its part from.
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
    /// What the account was for, without the recipient: the part of
    /// [`Self::purpose_summary`] before " → ".
    #[must_use]
    pub fn purpose_intent(&self) -> Option<&str> {
        self.purpose_summary().map(|purpose| {
            purpose
                .split_once(" → ")
                .map_or(purpose, |(intent, _)| intent)
        })
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
        let allocation = self.stored_allocation()?;
        let records = self.records()?;
        self.reconcile_allocation(allocation, &records)
    }

    fn stored_allocation(&self) -> Result<Allocation, ExecutorStoreError> {
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
        Ok(allocation)
    }

    fn reconcile_allocation(
        &self,
        mut allocation: Allocation,
        records: &[ExecutorRecord],
    ) -> Result<Allocation, ExecutorStoreError> {
        // Retained operation records establish a lower bound even if the counter was lost.
        for record in records {
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
            updates.push(self.seal(
                RecordKind::ExecutorAllocation,
                self.allocation_key(),
                &allocation,
            )?);
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
