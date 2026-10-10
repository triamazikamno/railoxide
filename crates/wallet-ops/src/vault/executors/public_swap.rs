use alloy::rpc::types::TransactionRequest;
use broadcaster_core::contracts::cow::OrderUid;

use super::swap::{BridgeOutcomeRules, bridge_refund_admitted};
use super::{
    AcrossOrderTerms, Address, B256, BridgeShieldFailure, Bytes, Deserialize, EXECUTOR_RECORD_LOCK,
    ExecutorAsset, ExecutorOperationId, ExecutorRecord, ExecutorStore, ExecutorStoreError,
    FixedBytes, RecordKind, SWAP_DESTINATION_PURPOSE_SUMMARY, Serialize, SwapAccountChoice,
    SwapAccountRole, SwapApprovedAccount, SwapApprovedBounds, SwapBridgeHandoff, SwapBridgeOutcome,
    SwapDestinationOutcome, SwapObservation, SwapSubmission, SwapSubmissionStatus,
    SwapTradeAmounts, SwapUseId, SwapUseRecord, SwapUseRelease, SwapUseRole, U256, local_timestamp,
    next_destination_outcome,
};
use crate::vault::PublicAccountScope;

/// A swap paid from a Public account and delivered to this account's chain, kept with the
/// destination stealth account's use. It names the Public account by address and holds none of
/// its secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapRecord {
    approval: PublicSwapApproval,
    /// What the swap deposits and by which path, known from its claim.
    intent: PublicSwapIntent,
    /// Every transaction the Public account handed to the network for this swap, oldest first.
    #[serde(default)]
    transactions: Vec<PublicSwapTransaction>,
    /// `None` until the Public account signs its deposit or order.
    #[serde(default)]
    path: Option<PublicSwapPath>,
    /// The signed deposit terms, set with the destination shield. `input_amount` and
    /// `output_amount` are minimums for an order; the deposited amounts are in `observations`.
    #[serde(default)]
    bridge: Option<AcrossOrderTerms>,
    #[serde(default)]
    observations: PublicSwapObservations,
}

impl PublicSwapRecord {
    /// The record of an approved swap whose Public account has signed nothing yet.
    #[must_use]
    pub const fn new(approval: PublicSwapApproval, intent: PublicSwapIntent) -> Self {
        Self {
            approval,
            intent,
            transactions: Vec::new(),
            path: None,
            bridge: None,
            observations: PublicSwapObservations {
                traded: None,
                trade_amounts: None,
                expired: None,
                cancelled: None,
                held_by_proxy: None,
                deposit_ruled_out: None,
                withdrawn: None,
                bridge_handoff: None,
                deposited: None,
                bridge_outcome: None,
                bridge_refund: None,
            },
        }
    }
    #[must_use]
    pub const fn approval(&self) -> &PublicSwapApproval {
        &self.approval
    }
    #[must_use]
    pub const fn intent(&self) -> PublicSwapIntent {
        self.intent
    }
    #[must_use]
    pub fn transactions(&self) -> &[PublicSwapTransaction] {
        &self.transactions
    }
    #[must_use]
    pub const fn path(&self) -> Option<&PublicSwapPath> {
        self.path.as_ref()
    }
    /// The order of a swap on the order path.
    #[must_use]
    pub const fn order(&self) -> Option<&PublicSwapOrder> {
        match &self.path {
            Some(PublicSwapPath::Order(order)) => Some(&**order),
            _ => None,
        }
    }
    #[must_use]
    pub const fn bridge(&self) -> Option<&AcrossOrderTerms> {
        self.bridge.as_ref()
    }
    #[must_use]
    pub const fn observations(&self) -> PublicSwapObservations {
        self.observations
    }
    /// Whether a deposit of this swap was handed off to the network.
    fn deposit_handed_off(&self) -> bool {
        self.transactions
            .iter()
            .any(|transaction| transaction.kind == PublicSwapTransactionKind::Deposit)
    }
    /// Whether the Public account has signed nothing that can still move funds: no path is
    /// recorded, or a direct deposit's path whose deposit was never handed off.
    fn unsigned(&self) -> bool {
        match &self.path {
            None => true,
            Some(PublicSwapPath::Deposit) => !self.deposit_handed_off(),
            Some(PublicSwapPath::Order(_)) => false,
        }
    }
    /// Whether this swap's order can still fill at `now`: it placed an order that has not
    /// traded, was not invalidated on chain, and whose `validTo` has not passed.
    #[must_use]
    pub fn order_can_fill(&self, now: u64) -> bool {
        self.order().is_some_and(|order| {
            self.observations.traded.is_none()
                && self.observations.cancelled.is_none()
                && now <= u64::from(order.valid_to())
        })
    }
    /// Whether this swap's hook batch can still run at `now`: it signed one, its deadline has
    /// not passed, and no deposit of it was observed, which is what uses its nonce.
    #[must_use]
    pub fn batch_can_run(&self, now: u64) -> bool {
        self.order().is_some_and(|order| {
            self.observations.bridge_handoff.is_none() && now <= u64::from(order.batch().deadline())
        })
    }
    /// Whether the proxy still holds this swap's proceeds: the settlement paid them to it, no
    /// deposit was observed, and they were not withdrawn.
    #[must_use]
    pub const fn proxy_holds_proceeds(&self) -> bool {
        self.observations.held_by_proxy.is_some()
            && self.observations.bridge_handoff.is_none()
            && self.observations.withdrawn.is_none()
    }
    /// Whether tracking has nothing left to find for this swap at `now`, Unix seconds.
    /// `stopped` is whether its use was stopped. A swap is finished when:
    ///
    /// - its deposit was handed off and the bridge delivered it or left it held in the
    ///   destination stealth account, or is refunding it and the refund was verified;
    /// - its order's proceeds were withdrawn from the proxy and the final deadline query
    ///   ruled out a deposit;
    /// - its order expired or was cancelled without a trade, and its hook batch's deadline has
    ///   passed;
    /// - its use was stopped before the Public account signed anything;
    /// - every deposit it handed off was verified reverted in a canonical finalized block.
    ///
    /// Any other swap is unfinished: one still being prepared, a signed order that was never
    /// submitted, a deposit or an order whose outcome isn't recorded, proceeds the proxy
    /// holds, a hand-off without a bridge outcome, and a refund that isn't verified.
    #[must_use]
    pub fn is_finished(&self, stopped: bool, now: u64) -> bool {
        let observed = &self.observations;
        if observed.bridge_handoff.is_some() {
            return match observed.bridge_outcome {
                Some(
                    SwapBridgeOutcome::DeliveredVerified { .. }
                    | SwapBridgeOutcome::DeliveredReported { .. }
                    | SwapBridgeOutcome::HeldOnDestination { .. },
                ) => true,
                Some(SwapBridgeOutcome::Refunding) => observed.bridge_refund.is_some(),
                Some(SwapBridgeOutcome::NeedsAttention) | None => false,
            };
        }
        if (observed.withdrawn.is_some() && observed.deposit_ruled_out.is_some())
            || (stopped && self.unsigned())
        {
            return true;
        }
        match &self.path {
            None => false,
            Some(PublicSwapPath::Deposit) => {
                let mut deposits = self
                    .transactions
                    .iter()
                    .filter(|transaction| transaction.kind == PublicSwapTransactionKind::Deposit)
                    .peekable();
                deposits.peek().is_some()
                    && deposits.all(|deposit| {
                        deposit
                            .inclusion
                            .is_some_and(|inclusion| inclusion.finalized && !inclusion.succeeded)
                    })
            }
            Some(PublicSwapPath::Order(order)) => {
                observed.traded.is_none()
                    && (observed.expired.is_some() || observed.cancelled.is_some())
                    && now > u64::from(order.batch.deadline)
            }
        }
    }
}

impl ExecutorRecord {
    /// The swaps paid from a Public account that this account's uses deliver and that are not
    /// finished at the local time, as [`PublicSwapRecord::is_finished`] judges them. Their
    /// tracking resumes from this chain's records alone, on this chain's owner. Without a
    /// local time no deadline counts as passed.
    pub fn public_swaps_to_track(&self) -> impl Iterator<Item = (SwapUseId, &PublicSwapRecord)> {
        let now = local_timestamp().unwrap_or(0);
        self.swap_uses.iter().filter_map(move |swap_use| {
            let swap = swap_use.public_swap()?;
            (!swap.is_finished(swap_use.stopped, now)).then_some((swap_use.id, swap))
        })
    }
}

/// What a bridge outcome means for the destination stealth account's shield payload, as
/// `ExecutorRecord::swap_destination_outcome` reads it from a private Bridge order's: a
/// shielded fill ran it, a held fill left its token in the account, and a refund left it
/// unfilled. Across's report, `NeedsAttention` and a fill without the shield say nothing of it.
const fn destination_outcome(outcome: SwapBridgeOutcome) -> Option<SwapDestinationOutcome> {
    match outcome {
        SwapBridgeOutcome::DeliveredVerified {
            shielded: true,
            block,
            transaction_hash,
            ..
        } => Some(SwapDestinationOutcome::Shielded {
            block,
            transaction_hash,
        }),
        SwapBridgeOutcome::HeldOnDestination {
            block,
            transaction_hash,
            ..
        } => Some(SwapDestinationOutcome::Held {
            block,
            transaction_hash,
        }),
        SwapBridgeOutcome::Refunding => Some(SwapDestinationOutcome::Unfilled),
        SwapBridgeOutcome::DeliveredVerified { .. }
        | SwapBridgeOutcome::DeliveredReported { .. }
        | SwapBridgeOutcome::NeedsAttention => None,
    }
}

/// What a Public-paid swap deposits on its origin chain, fixed when it is claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapIntent {
    /// The token the swap deposits: the Sell token for a direct deposit, with a native Sell
    /// asset as its wrapped token, and the order's buy token otherwise.
    pub bridged_token: Address,
    /// Whether the swap places a `CoW` order rather than depositing the Sell token itself.
    pub order: bool,
}

/// What the user approved for a Public-paid swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapApproval {
    pub bounds: SwapApprovedBounds,
    #[serde(default)]
    pub price_verified: Option<bool>,
    pub price_acknowledged: bool,
    /// `Address::ZERO` for the origin chain's native asset.
    pub sell_token: Address,
    pub on_shield_failure: BridgeShieldFailure,
    /// The destination stealth account and whether the swap sets it up.
    pub destination: SwapApprovedAccount,
    /// The Public account's approved maximum gas cost on the origin chain, in wei.
    pub max_gas_cost: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublicSwapTransactionKind {
    ApprovalReset,
    Approval,
    Deposit,
    Invalidation,
    Withdrawal,
}

/// A transaction handed off for the swap, persisted before it is submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapTransaction {
    pub kind: PublicSwapTransactionKind,
    pub transaction: TransactionRequest,
    pub hash: B256,
    /// Where it was included and whether it succeeded. `None` until observed.
    #[serde(default)]
    pub inclusion: Option<PublicSwapInclusion>,
    /// The origin chain's head block number when the transaction was handed off, which bounds
    /// a later search for it.
    #[serde(default)]
    pub submitted_from_block: Option<u64>,
    /// The next finalized block to inspect for an interrupted deposit. Older records resume
    /// from `submitted_from_block`; only checked canonical blocks advance this cursor.
    #[serde(default)]
    pub deposit_scan_from_block: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapInclusion {
    pub observation: SwapObservation,
    pub succeeded: bool,
    /// First action receipts and older records remain provisional until canonical finality
    /// is verified by whole-block observation.
    #[serde(default)]
    pub finalized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublicSwapPath {
    /// The Public account deposits the Sell token itself. Its transactions are in the record.
    Deposit,
    /// A `CoW` order paid to the account's cow-shed proxy, whose post-hook deposits.
    Order(Box<PublicSwapOrder>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapOrder {
    uid: FixedBytes<56>,
    /// The token the order buys and its hook deposits: the Across route's origin token.
    buy_token: Address,
    /// The Public account's cow-shed proxy, the order's receiver.
    proxy: Address,
    batch: PublicSwapHookBatch,
    submission: SwapSubmission,
    #[serde(default)]
    submission_status: SwapSubmissionStatus,
    /// The signed permit the order's pre-hook submits. `None` for an order without one.
    ///
    /// A build from before this field drops it on read, rebuilds the app data without the
    /// pre-hook and gets another UID, so it refuses to resend the order. An order the
    /// orderbook already accepted keeps its app data and can still fill.
    #[serde(default)]
    permit: Option<PublicSwapPermit>,
}

impl PublicSwapOrder {
    /// A signed order whose submission is pending.
    #[must_use]
    pub const fn new(
        uid: OrderUid,
        buy_token: Address,
        proxy: Address,
        batch: PublicSwapHookBatch,
        submission: SwapSubmission,
    ) -> Self {
        Self {
            uid: uid.0,
            buy_token,
            proxy,
            batch,
            submission,
            submission_status: SwapSubmissionStatus::Pending,
            permit: None,
        }
    }
    /// This order with the signed permit its pre-hook submits.
    #[must_use]
    pub const fn with_permit(mut self, permit: Option<PublicSwapPermit>) -> Self {
        self.permit = permit;
        self
    }
    #[must_use]
    pub const fn permit(&self) -> Option<&PublicSwapPermit> {
        self.permit.as_ref()
    }
    #[must_use]
    pub const fn uid(&self) -> OrderUid {
        OrderUid(self.uid)
    }
    /// Persisted as part of the order UID.
    #[must_use]
    pub const fn valid_to(&self) -> u32 {
        self.uid().valid_to()
    }
    #[must_use]
    pub const fn buy_token(&self) -> Address {
        self.buy_token
    }
    #[must_use]
    pub const fn proxy(&self) -> Address {
        self.proxy
    }
    #[must_use]
    pub const fn batch(&self) -> &PublicSwapHookBatch {
        &self.batch
    }
    #[must_use]
    pub const fn submission(&self) -> &SwapSubmission {
        &self.submission
    }
    #[must_use]
    pub const fn submission_status(&self) -> SwapSubmissionStatus {
        self.submission_status
    }
}

/// The EIP-2612 permit an order's pre-hook submits: the Public account's approval of `value`
/// of the sold token to `CoW`'s vault relayer, signed under `nonce` until `deadline`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapPermit {
    nonce: U256,
    /// Unix seconds; the order's `validTo`.
    deadline: u32,
    value: U256,
    signature: FixedBytes<65>,
}

// The signature is not formatted.
impl std::fmt::Debug for PublicSwapPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicSwapPermit")
            .field("nonce", &self.nonce)
            .field("deadline", &self.deadline)
            .field("value", &self.value)
            .finish_non_exhaustive()
    }
}

impl PublicSwapPermit {
    #[must_use]
    pub const fn new(nonce: U256, deadline: u32, value: U256, signature: [u8; 65]) -> Self {
        Self {
            nonce,
            deadline,
            value,
            signature: FixedBytes(signature),
        }
    }
    #[must_use]
    pub const fn nonce(&self) -> U256 {
        self.nonce
    }
    #[must_use]
    pub const fn deadline(&self) -> u32 {
        self.deadline
    }
    #[must_use]
    pub const fn value(&self) -> U256 {
        self.value
    }
    #[must_use]
    pub const fn signature(&self) -> &[u8; 65] {
        &self.signature.0
    }
}

/// The signed cow-shed hook batch in the order's app data. It stays valid until `deadline`
/// or until its nonce is used, whatever becomes of the order.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapHookBatch {
    /// `COWShedFactory.executeHooks` calldata, signature included.
    calldata: Bytes,
    nonce: B256,
    /// Unix seconds; the order's `validTo`.
    deadline: u32,
}

// The calldata carries the batch's signature, so it is not formatted.
impl std::fmt::Debug for PublicSwapHookBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicSwapHookBatch")
            .field("nonce", &self.nonce)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl PublicSwapHookBatch {
    #[must_use]
    pub const fn new(calldata: Bytes, nonce: B256, deadline: u32) -> Self {
        Self {
            calldata,
            nonce,
            deadline,
        }
    }
    #[must_use]
    pub const fn calldata(&self) -> &Bytes {
        &self.calldata
    }
    #[must_use]
    pub const fn nonce(&self) -> B256 {
        self.nonce
    }
    #[must_use]
    pub const fn deadline(&self) -> u32 {
        self.deadline
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapObservations {
    /// The order's trade. Order path only.
    #[serde(default)]
    pub traded: Option<SwapObservation>,
    #[serde(default)]
    pub trade_amounts: Option<SwapTradeAmounts>,
    /// Finalized block past `validTo` at which the order had not filled.
    #[serde(default)]
    pub expired: Option<SwapObservation>,
    /// Finalized settlement state shows the order invalidated and no trade was reported.
    #[serde(default, deserialize_with = "deserialize_finalized_cancellation")]
    pub cancelled: Option<SwapObservation>,
    /// The settlement paid the proxy and held no matching deposit: the amount it paid.
    #[serde(default)]
    pub held_by_proxy: Option<PublicSwapProxyHolding>,
    /// The one bounded depositor query at the batch's deadline found no deposit.
    #[serde(default)]
    pub deposit_ruled_out: Option<SwapObservation>,
    /// The withdrawal from the proxy to the Public account.
    #[serde(default)]
    pub withdrawn: Option<SwapObservation>,
    /// The Across deposit on the origin chain.
    #[serde(default)]
    pub bridge_handoff: Option<SwapBridgeHandoff>,
    /// The amounts in the deposit's `FundsDeposited` event, which the fill is matched on.
    #[serde(default)]
    pub deposited: Option<PublicSwapDeposited>,
    /// The bridge's result on this account's chain.
    #[serde(default)]
    pub bridge_outcome: Option<SwapBridgeOutcome>,
    /// Across's refund to the Public account, verified in finalized receipts.
    #[serde(default)]
    pub bridge_refund: Option<SwapObservation>,
}

fn deserialize_finalized_cancellation<'de, D>(
    deserializer: D,
) -> Result<Option<SwapObservation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Older first-receipt cancellations carried a transaction hash and were not final.
    // Finalized settlement-state observations have always been hashless.
    Ok(Option::<SwapObservation>::deserialize(deserializer)?
        .filter(|observation| observation.transaction_hash.is_none()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapProxyHolding {
    pub observation: SwapObservation,
    pub amount: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicSwapDeposited {
    pub input_amount: U256,
    pub output_amount: U256,
}

/// The claim of a swap paid from a Public account, made on the destination chain's store.
#[derive(Debug, Clone)]
pub struct PublicSwapClaim {
    pub id: SwapUseId,
    pub origin_chain: u64,
    /// The Public account that pays, and how it is scoped.
    pub source: Address,
    pub source_scope: PublicAccountScope,
    /// The destination stealth account on this store's chain.
    pub account: SwapAccountChoice,
    /// This chain's accepted delegate, for a new account.
    pub delegate: Address,
    pub destination_token: Address,
    /// The token the swap buys on the origin chain and deposits: the Sell token itself for a
    /// direct deposit, with a native Sell asset as its wrapped token, and the order's buy token
    /// otherwise.
    pub bridged_token: Address,
    /// Whether the swap places a `CoW` order (true) or deposits the Sell token directly (false).
    pub order: bool,
    pub approval: PublicSwapApproval,
    /// The current time, Unix seconds, for the validity rules.
    pub now: u64,
}

impl PublicSwapClaim {
    /// What the source rules read of this claim.
    fn source_terms(&self) -> PublicSwapSourceTerms {
        PublicSwapSourceTerms {
            id: Some(self.id),
            origin_chain: self.origin_chain,
            source: self.source,
            source_scope: self.source_scope.clone(),
            sell_token: self.approval.sell_token,
            bridged_token: self.bridged_token,
            order: self.order,
            now: self.now,
        }
    }
}

/// What the source rules read of a swap paid from a Public account: of a claim, or of a draft
/// that has chosen no destination account and approved nothing yet.
#[derive(Debug, Clone)]
pub struct PublicSwapSourceTerms {
    /// The swap's own use, which doesn't compete with itself. `None` for a draft without one.
    pub id: Option<SwapUseId>,
    pub origin_chain: u64,
    /// The Public account that pays, and how it is scoped.
    pub source: Address,
    pub source_scope: PublicAccountScope,
    /// `Address::ZERO` for the native asset.
    pub sell_token: Address,
    /// The token the swap buys on the origin chain and deposits.
    pub bridged_token: Address,
    /// Whether the swap places a `CoW` order (true) or deposits the Sell token directly (false).
    pub order: bool,
    /// The current time, Unix seconds, for the validity rules.
    pub now: u64,
}

/// Refuse `claim`, the source terms of a swap, when its Public account is shared between
/// wallets, or while another swap paid from that account on its origin chain can still buy the
/// token it buys or sell the token it sells. `records` are the wallet's records on every chain,
/// each with its chain: a Public account's swaps are kept on the chains they deliver to.
fn admit_public_swap_source(
    claim: &PublicSwapSourceTerms,
    records: &[(u64, ExecutorRecord)],
) -> Result<(), ExecutorStoreError> {
    // Another wallet's swap from a shared account is in records this wallet can't read.
    if claim.source_scope == PublicAccountScope::Global {
        return Err(ExecutorStoreError::PublicSwapSourceShared);
    }
    // Only another swap's order competes: a direct deposit spends what it deposits at once.
    let orders: Vec<_> = records
        .iter()
        .flat_map(|(chain_id, record)| {
            record
                .swap_uses()
                .iter()
                .filter_map(move |swap_use| match &swap_use.role {
                    SwapUseRole::PublicSourceDestination {
                        origin_chain,
                        source,
                        swap,
                        ..
                    } if Some(swap_use.id) != claim.id
                        && *origin_chain == claim.origin_chain
                        && *source == claim.source
                        && swap.intent.order =>
                    {
                        // A claim that has signed nothing can still place its order and run its
                        // batch, until it is stopped.
                        let unsigned = swap.path.is_none() && !swap_use.stopped;
                        Some((*chain_id, swap_use.id, &**swap, unsigned))
                    }
                    _ => None,
                })
        })
        .collect();
    if claim.order {
        for &(chain_id, id, swap, unsigned) in &orders {
            if swap.intent.bridged_token != claim.bridged_token {
                continue;
            }
            let beyond_deadline =
                swap.order_can_fill(claim.now) || swap.proxy_holds_proceeds() || unsigned;
            if beyond_deadline || swap.batch_can_run(claim.now) {
                return Err(ExecutorStoreError::PublicSwapBuysSameToken {
                    swap: id,
                    chain_id,
                    // Only the batch's deadline keeps an order that can no longer fill blocked.
                    available_at: swap
                        .order()
                        .filter(|_| !beyond_deadline)
                        .map(|order| u64::from(order.batch().deadline()) + 1),
                });
            }
        }
    }
    for &(chain_id, id, swap, unsigned) in &orders {
        if swap.approval.sell_token == claim.sell_token
            && (swap.order_can_fill(claim.now) || unsigned)
        {
            return Err(ExecutorStoreError::PublicSwapSellsSameToken {
                swap: id,
                chain_id,
                expires_at: swap.order().map(|order| u64::from(order.valid_to())),
            });
        }
    }
    Ok(())
}

/// The swap of the use `id`, which a Public account pays for and which was not stopped.
fn live_public_swap(
    record: &mut ExecutorRecord,
    id: SwapUseId,
) -> Result<&mut PublicSwapRecord, ExecutorStoreError> {
    match record.swap_use_mut(id) {
        Some(SwapUseRecord {
            stopped: false,
            role: SwapUseRole::PublicSourceDestination { swap, .. },
            ..
        }) => Ok(&mut **swap),
        _ => Err(ExecutorStoreError::OperationMismatch),
    }
}

/// The swap of the use `id`, which a Public account pays for, stopped or not. What the chain
/// shows of a swap is recorded after it is stopped too: a deposit that already left is still
/// tracked.
fn public_swap_mut(
    record: &mut ExecutorRecord,
    id: SwapUseId,
) -> Result<&mut PublicSwapRecord, ExecutorStoreError> {
    match record.swap_use_mut(id) {
        Some(SwapUseRecord {
            role: SwapUseRole::PublicSourceDestination { swap, .. },
            ..
        }) => Ok(&mut **swap),
        _ => Err(ExecutorStoreError::OperationMismatch),
    }
}

impl ExecutorStore {
    /// Claim the destination stealth account of a swap paid from a Public account, on the
    /// destination chain's store. The swap has no source stealth account, so this one use is
    /// its whole claim. The account and the Public account's other swaps on every chain are
    /// checked before anything is written. A claim the account already holds is returned
    /// unchanged.
    pub fn claim_public_swap(
        &self,
        claim: PublicSwapClaim,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        // The Public account pays on another chain than the one it delivers to.
        if claim.origin_chain == self.chain_id {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let operation = claim.account.operation();
        let record = self.record(operation)?;
        if let Some(held) = record.as_ref().and_then(|record| record.swap_use(claim.id)) {
            let bound = matches!(
                &held.role,
                SwapUseRole::PublicSourceDestination {
                    origin_chain,
                    source,
                    destination_token,
                    ..
                } if *origin_chain == claim.origin_chain
                    && *source == claim.source
                    && *destination_token == claim.destination_token
            );
            return record
                .filter(|_| bound)
                .ok_or(ExecutorStoreError::OperationMismatch);
        }

        // An approved new address is checked after derivation, as for a claimed pair.
        let setup = matches!(claim.account, SwapAccountChoice::New(_));
        let approved = claim.approval.destination;
        if !record
            .as_ref()
            .and_then(ExecutorRecord::address)
            .map_or(approved.setup == setup, |address| {
                approved.admits(address, setup)
            })
        {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        super::swap_use::admit_swap_account(
            claim.account,
            record.as_ref(),
            self.chain_id,
            SwapAccountRole::Destination {
                token: claim.destination_token,
            },
            claim.id,
        )?;
        admit_public_swap_source(&claim.source_terms(), &self.wallet_records()?)?;

        let PublicSwapClaim {
            id,
            origin_chain,
            source,
            delegate,
            destination_token,
            bridged_token,
            order,
            approval,
            ..
        } = claim;
        let received = [ExecutorAsset::Erc20(destination_token)];
        let mut updates = Vec::new();
        let record = self.claim_swap_account(
            operation,
            record,
            (
                delegate,
                Some(SWAP_DESTINATION_PURPOSE_SUMMARY),
                received.as_slice(),
            ),
            (
                id,
                SwapUseRole::PublicSourceDestination {
                    origin_chain,
                    source,
                    destination_token,
                    shields: Vec::new(),
                    outcome: None,
                    swap: Box::new(PublicSwapRecord::new(
                        approval,
                        PublicSwapIntent {
                            bridged_token,
                            order,
                        },
                    )),
                },
            ),
            &mut updates,
        )?;
        self.vault.db.put_desktop_wallet_vault_records(&updates)?;
        Ok(record)
    }

    /// The refusal [`Self::claim_public_swap`] would give a swap with the source terms `terms`
    /// for its Public account: that the account is shared between wallets, or the open swap
    /// that still buys or sells one of its tokens. `None` when the source admits the swap. The
    /// same rules run under the record lock, and nothing is written.
    pub fn public_swap_source_conflict(
        &self,
        terms: &PublicSwapSourceTerms,
    ) -> Result<Option<ExecutorStoreError>, ExecutorStoreError> {
        // The Public account pays on another chain than the one it delivers to.
        if terms.origin_chain == self.chain_id {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        Ok(admit_public_swap_source(terms, &self.wallet_records()?).err())
    }

    /// Replace the full approval of a live Public-paid swap before an order was signed or a
    /// deposit handed off. Its identity and destination setup need stay fixed. Issued shields
    /// and transaction history remain recorded; unsigned delivery terms must be signed again.
    pub fn reapprove_public_swap(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        approval: PublicSwapApproval,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            if record.active_swap_use != Some(id) || record.retired || record.swap_setup_stopped {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            let swap = live_public_swap(record, id)?;
            if !swap.unsigned()
                || approval.sell_token != swap.approval.sell_token
                || approval.destination != swap.approval.destination
                || approval.bounds.destination_minimum.is_none()
                || (approval.destination.setup && approval.bounds.destination_setup_fee.is_none())
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            swap.approval = approval;
            swap.path = None;
            swap.bridge = None;
            Ok(())
        })
    }

    /// Cancel the use `id` of a swap paid from a Public account before that account has signed
    /// anything. The use is stopped and its account released as [`SwapUseRelease`] reports. A
    /// direct deposit whose path was recorded but whose deposit was never handed off has signed
    /// nothing either. A swap with a handed-off deposit or a signed order is refused: it ends
    /// through its own flow.
    pub fn cancel_public_swap(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
    ) -> Result<SwapUseRelease, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        let mut record = self
            .record(operation)?
            .ok_or(ExecutorStoreError::OperationMismatch)?;
        if record
            .public_swap_use(id)
            .is_none_or(|(_, swap)| !swap.unsigned())
        {
            return Err(ExecutorStoreError::OperationMismatch);
        }
        let released = record.cancel_swap_use(id);
        self.vault.db.put_desktop_wallet_vault_records(&[self.seal(
            RecordKind::ExecutorOperation,
            self.operation_key(operation),
            &record,
        )?])?;
        Ok(released)
    }

    /// Bind the destination account's `address` into the approval of the live use `id`, once
    /// the account is derived. `address` must be the account's own, and the approval must admit
    /// it with the setup need the use was claimed with. Binding the address it already holds
    /// changes nothing.
    pub fn bind_public_swap_destination(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        address: Address,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let setup = record
                .swap_use(id)
                .ok_or(ExecutorStoreError::OperationMismatch)?
                .is_fresh();
            if record.address() != Some(address) {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            let destination = &mut live_public_swap(record, id)?.approval.destination;
            if !destination.admits(address, setup) {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            destination.address = Some(address);
            Ok(())
        })
    }

    /// The destination setup fee approved with the swap paid from a Public account whose use
    /// claims `operation`'s account. `None` when no such use claims it, or its approval binds
    /// no fee.
    pub(crate) fn public_swap_destination_setup_fee(
        &self,
        operation: ExecutorOperationId,
    ) -> Result<Option<U256>, ExecutorStoreError> {
        let _guard = EXECUTOR_RECORD_LOCK
            .lock()
            .map_err(|_| ExecutorStoreError::Unavailable)?;
        self.require_wallet()?;
        Ok(self.record(operation)?.and_then(|record| {
            record
                .active_use()?
                .public_swap()?
                .approval
                .bounds
                .destination_setup_fee
        }))
    }

    /// Persist the path the Public account signed for the live use `id`, with its deposit
    /// terms. A path is recorded once: the same path and terms again change nothing, and any
    /// other are refused, as is a path of another kind or token than the use claimed. Only a
    /// direct deposit that was never handed off takes other terms, those of a later quote. An
    /// order's path and terms are never replaced.
    pub fn record_public_swap_path(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        path: PublicSwapPath,
        bridge: AcrossOrderTerms,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let swap = live_public_swap(record, id)?;
            let claimed = match &path {
                PublicSwapPath::Deposit => !swap.intent.order,
                PublicSwapPath::Order(order) => {
                    swap.intent.order && order.buy_token == swap.intent.bridged_token
                }
            };
            if !claimed {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            if let Some(recorded) = &swap.path {
                if *recorded == path && swap.bridge == Some(bridge) {
                    return Ok(());
                }
                if path != PublicSwapPath::Deposit || *recorded != path || !swap.unsigned() {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
            }
            swap.path = Some(path);
            swap.bridge = Some(bridge);
            Ok(())
        })
    }

    /// Save what the orderbook answered for the order of the use `id`'s swap, stopped or not.
    /// A late failed request cannot undo an acceptance of the same immutable order.
    pub fn record_public_swap_submission(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        status: SwapSubmissionStatus,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let Some(PublicSwapPath::Order(order)) = &mut public_swap_mut(record, id)?.path else {
                return Err(ExecutorStoreError::OperationMismatch);
            };
            if order.submission_status != SwapSubmissionStatus::Accepted {
                order.submission_status = status;
            }
            Ok(())
        })
    }

    /// Replace what was observed of the use `id`'s swap, stopped or not.
    pub fn record_public_swap_observations(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        observations: PublicSwapObservations,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            public_swap_mut(record, id)?.observations = observations;
            Ok(())
        })
    }

    /// Persist the bridge's outcome for the deposit of the use `id`'s swap, stopped or not,
    /// under the rules a private Bridge order's outcome is recorded by: see
    /// [`ExecutorStore::record_swap_bridge_outcome`]. The delivery is Across's and private, and
    /// a verified one must meet the approved destination minimum.
    ///
    /// The same write sets what the outcome means for this account's shield payload, as
    /// destination reconciliation does for a stealth pair: a fill's outcome is never replaced,
    /// `Unfilled` gives way to a fill, and a fill is not recorded for a use that signed no
    /// shield.
    pub fn record_public_swap_bridge_outcome(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        outcome: SwapBridgeOutcome,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let Some(SwapUseRecord {
                role:
                    SwapUseRole::PublicSourceDestination {
                        shields,
                        outcome: delivered,
                        swap,
                        ..
                    },
                ..
            }) = record.swap_use_mut(id)
            else {
                return Err(ExecutorStoreError::OperationMismatch);
            };
            let observed = swap.observations;
            let rules = BridgeOutcomeRules {
                handed_off: observed.bridge_handoff.is_some(),
                private: true,
                destination_minimum: swap.approval.bounds.destination_minimum,
                across: true,
                known: observed.bridge_outcome,
                refund_verified: observed.bridge_refund.is_some(),
            };
            if !rules.admits(outcome) {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            swap.observations.bridge_outcome = Some(outcome);
            let next = next_destination_outcome(*delivered, destination_outcome(outcome));
            // A fill can only have run, or left funds for, a shield this use signed.
            let filled = matches!(
                next,
                Some(SwapDestinationOutcome::Shielded { .. } | SwapDestinationOutcome::Held { .. })
            );
            if !(filled && shields.is_empty()) {
                *delivered = next;
            }
            Ok(())
        })
    }

    /// Persist Across's refund of the refunding deposit of the use `id`'s swap, stopped or
    /// not, to its Public account: in a finalized block of the chain that account pays on,
    /// after the hand-off. A recorded refund is never replaced, though recording it again is
    /// accepted.
    pub(crate) fn record_public_swap_bridge_refund(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        refund: SwapObservation,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let observations = &mut public_swap_mut(record, id)?.observations;
            if !bridge_refund_admitted(
                true,
                observations.bridge_outcome,
                observations.bridge_handoff,
                observations.bridge_refund,
                refund,
            ) {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            observations.bridge_refund = Some(refund);
            Ok(())
        })
    }

    /// Append a transaction handed off for the use `id`'s swap, stopped or not. An entry with
    /// the same hash, kind and request is already recorded and changes nothing, and another
    /// with that hash is refused. Only one distinct deposit may be handed off. A deposit is
    /// refused until the swap's path is
    /// [`PublicSwapPath::Deposit`], so the path and its signed terms are recorded before a
    /// deposit can be handed off. A withdrawal or an invalidation is refused unless the path
    /// is [`PublicSwapPath::Order`]: only an order has a proxy to withdraw from and an order
    /// to invalidate.
    pub fn record_public_swap_transaction(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        transaction: PublicSwapTransaction,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let swap = public_swap_mut(record, id)?;
            let admitted = match transaction.kind {
                PublicSwapTransactionKind::ApprovalReset | PublicSwapTransactionKind::Approval => {
                    true
                }
                PublicSwapTransactionKind::Deposit => swap.path == Some(PublicSwapPath::Deposit),
                PublicSwapTransactionKind::Invalidation | PublicSwapTransactionKind::Withdrawal => {
                    swap.order().is_some()
                }
            };
            if !admitted {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            if let Some(recorded) = swap
                .transactions
                .iter()
                .find(|recorded| recorded.hash == transaction.hash)
            {
                return if recorded.kind == transaction.kind
                    && recorded.transaction == transaction.transaction
                {
                    Ok(())
                } else {
                    Err(ExecutorStoreError::OperationMismatch)
                };
            }
            if transaction.kind == PublicSwapTransactionKind::Deposit
                && swap
                    .transactions
                    .iter()
                    .any(|recorded| recorded.kind == PublicSwapTransactionKind::Deposit)
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            swap.transactions.push(transaction);
            Ok(())
        })
    }

    /// Record where the transaction `hash` of the use `id`'s swap, stopped or not, was
    /// included and whether it succeeded. A late provisional receipt cannot downgrade a
    /// finalized inclusion. A transaction that was never handed off is refused.
    pub fn record_public_swap_inclusion(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        hash: B256,
        inclusion: PublicSwapInclusion,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let transaction = public_swap_mut(record, id)?
                .transactions
                .iter_mut()
                .find(|transaction| transaction.hash == hash)
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if !transaction
                .inclusion
                .is_some_and(|recorded| recorded.finalized)
                || inclusion.finalized
            {
                transaction.inclusion = Some(inclusion);
            }
            Ok(())
        })
    }

    /// Advance an interrupted deposit's cursor only after inspecting canonical finalized
    /// blocks. Concurrent observations cannot move it backwards.
    pub(crate) fn record_public_swap_deposit_scan(
        &self,
        operation: ExecutorOperationId,
        id: SwapUseId,
        hash: B256,
        next_block: u64,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let transaction = public_swap_mut(record, id)?
                .transactions
                .iter_mut()
                .find(|transaction| {
                    transaction.hash == hash
                        && transaction.kind == PublicSwapTransactionKind::Deposit
                })
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            transaction.deposit_scan_from_block = Some(
                transaction
                    .deposit_scan_from_block
                    .unwrap_or(0)
                    .max(next_block),
            );
            Ok(())
        })
    }
}
