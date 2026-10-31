use broadcaster_core::contracts::cow::OrderUid;

use crate::bridge::NonEvmNetwork;

use super::{
    Address, B256, BlockNumHash, Deserialize, ExecutorInputIdentity, ExecutorOperationId,
    ExecutorPayloadPurpose, ExecutorRecord, ExecutorStore, ExecutorStoreError, FixedBytes,
    IssuedExecutorPayload, Serialize, SwapAccountRefusal, SwapDestinationOutcome, SwapSpeedUp,
    SwapUseId, SwapUseRecord, SwapUseRole, U256,
};

/// Public components of the Railgun address that every post-hook of one swap
/// shields to. No key material is persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapRecipient {
    master_public_key: U256,
    viewing_public_key: FixedBytes<32>,
}

impl SwapRecipient {
    #[must_use]
    pub const fn new(master_public_key: U256, viewing_public_key: [u8; 32]) -> Self {
        Self {
            master_public_key,
            viewing_public_key: FixedBytes(viewing_public_key),
        }
    }
    #[must_use]
    pub const fn master_public_key(&self) -> U256 {
        self.master_public_key
    }
    #[must_use]
    pub const fn viewing_public_key(&self) -> [u8; 32] {
        self.viewing_public_key.0
    }
}

/// The token pair, recipient and setup associated with one order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapTerms {
    sell_token: Address,
    buy_token: Address,
    recipient: SwapRecipient,
    /// The latest delegation-only setup issued when the order was signed. Its nonce must be
    /// resolved first; which setup signed at that nonce ran is not recorded.
    setup_payload: B256,
}

impl SwapTerms {
    #[must_use]
    pub const fn new(
        sell_token: Address,
        buy_token: Address,
        recipient: SwapRecipient,
        setup_payload: B256,
    ) -> Self {
        Self {
            sell_token,
            buy_token,
            recipient,
            setup_payload,
        }
    }
    #[must_use]
    pub const fn sell_token(&self) -> Address {
        self.sell_token
    }
    #[must_use]
    pub const fn buy_token(&self) -> Address {
        self.buy_token
    }
    #[must_use]
    pub const fn recipient(&self) -> SwapRecipient {
        self.recipient
    }
    #[must_use]
    pub const fn setup_payload(&self) -> B256 {
        self.setup_payload
    }
}

/// The proved private transactions the latest pre-hook unshields with. A retry
/// may reuse the proof while its inputs and the approved sell amount are unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapProof {
    digest: B256,
    inputs: Vec<ExecutorInputIdentity>,
}

impl SwapProof {
    #[must_use]
    pub const fn new(digest: B256, inputs: Vec<ExecutorInputIdentity>) -> Self {
        Self { digest, inputs }
    }
    #[must_use]
    pub const fn digest(&self) -> B256 {
        self.digest
    }
    #[must_use]
    pub fn inputs(&self) -> &[ExecutorInputIdentity] {
        &self.inputs
    }
}

/// Verified execution of a pre-hook proof's private funding effects for one swap use at one
/// pre-hook nonce. It shows the stealth account was funded with the sell amount, not which of
/// the attempts signed at that nonce ran its pre-hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapFunding {
    pub use_id: SwapUseId,
    pub pre_hook_nonce: U256,
    /// The digest of the proved transactions, as in [`SwapProof::digest`].
    pub proof: B256,
    pub observation: SwapObservation,
}

/// Swap state in the operation's encrypted executor record. Each attempt writes
/// one order; the order list keeps later order kinds additive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapOperationRecord {
    terms: SwapTerms,
    proof: SwapProof,
    orders: Vec<SwapOrderRecord>,
    /// At most one per swap use and pre-hook nonce.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fundings: Vec<SwapFunding>,
}

impl SwapOperationRecord {
    #[must_use]
    pub fn terms(&self) -> &SwapTerms {
        match self.orders.last() {
            Some(order) => self.order_terms(order),
            None => &self.terms,
        }
    }
    /// Older orders inherited the operation's original terms. Keep that fallback immutable.
    #[must_use]
    pub const fn order_terms<'a>(&'a self, order: &'a SwapOrderRecord) -> &'a SwapTerms {
        match &order.terms {
            Some(terms) => terms,
            None => &self.terms,
        }
    }
    #[must_use]
    pub const fn proof(&self) -> &SwapProof {
        &self.proof
    }
    #[must_use]
    pub fn orders(&self) -> &[SwapOrderRecord] {
        &self.orders
    }
    #[must_use]
    pub fn fundings(&self) -> &[SwapFunding] {
        &self.fundings
    }
    /// The verified funding `order` shares: the one of its swap use at its pre-hook nonce,
    /// unless its pre-hook is recorded dead. Within one use, attempts at one nonce that are
    /// not recorded dead share the proof: an ordinary retry at that nonce is admitted only
    /// after the earlier attempt's pre-hook is recorded dead, and a speed-up must keep the
    /// proof. An attempt recorded dead by expiry before the funding could not have been the
    /// one that ran. A funding that another attempt traded away is no longer shared.
    #[must_use]
    pub fn funding(&self, order: &SwapOrderRecord) -> Option<SwapObservation> {
        let use_id = order.use_id?;
        if order.observations.pre_hook_dead.is_some() || self.traded_sibling(order).is_some() {
            return None;
        }
        self.fundings
            .iter()
            .find(|funding| {
                funding.use_id == use_id && funding.pre_hook_nonce == order.pre_hook.nonce
            })
            .map(|funding| funding.observation)
    }
    /// Another attempt of `order`'s swap use, signed at the same pre-hook nonce, that traded.
    /// Attempts of one use at one pre-hook nonce share one funding and one exact sell
    /// allowance, so once one of them traded no other can.
    #[must_use]
    pub fn traded_sibling<'a>(&'a self, order: &'a SwapOrderRecord) -> Option<&'a SwapOrderRecord> {
        let use_id = order.use_id?;
        self.orders.iter().find(|other| {
            other.uid != order.uid
                && other.use_id == Some(use_id)
                && other.pre_hook.nonce == order.pre_hook.nonce
                && other.observations.traded.is_some()
        })
    }
    /// A new attempt may start once each earlier attempt has either ended before its pre-hook
    /// ran or traded and completed delivery, for a Bridge order delivery on the destination
    /// chain. Unrecovered executed pre-hooks block it, as do refunding and unresolved bridges.
    /// An untraded attempt sharing a verified funding blocks it like an executed pre-hook:
    /// such an attempt is never recorded dead, so it has not ended.
    /// An untraded attempt whose sibling at the same pre-hook nonce traded can no longer fill
    /// and admits one, leaving the decision to the traded sibling.
    /// A trade delivered by another attempt of its use completes with that attempt's
    /// destination outcome, and the attempt holding the hand-off waits for the same outcome:
    /// its ended pre-hook does not resolve its bridge.
    #[must_use]
    pub fn admits_attempt(&self) -> bool {
        self.orders
            .iter()
            .all(|order| self.order_admits_attempt(order))
    }

    fn order_admits_attempt(&self, order: &SwapOrderRecord) -> bool {
        if order.observations.traded.is_some() {
            order.has_ended()
                || self
                    .delivery_order(order)
                    .is_some_and(SwapOrderRecord::destination_delivered)
        } else if order.observations.bridge_handoff.is_some() {
            self.trade_order(order).is_some() && order.destination_delivered()
        } else {
            order.has_ended() || self.traded_sibling(order).is_some()
        }
    }

    /// Why a second order can't be signed against the open order `speed_up` names. That order
    /// must be the latest one, of the same use and pre-hook nonce, with nothing recorded that
    /// ends it, and it can't deliver privately through a bridge. The use's other attempts at
    /// that nonce compete with it: they may have ended, but none may have run its pre-hook,
    /// traded or handed off, and no funding may be recorded at that nonce, whichever of them
    /// ran. Every other order must admit an attempt as for a retry.
    pub(super) fn speed_up_refusal(&self, speed_up: &SwapSpeedUp) -> Option<SwapAccountRefusal> {
        let Some(open) = self.orders.last().filter(|order| {
            order.uid == speed_up.order.0
                && order.use_id == Some(speed_up.swap_use)
                && order.pre_hook.nonce == speed_up.pre_hook_nonce
        }) else {
            return Some(SwapAccountRefusal::SpeedUpOrderChanged);
        };
        if open.delivery.private_bridge().is_some() {
            return Some(SwapAccountRefusal::SpeedUpUnsupported);
        }
        let competes = |order: &SwapOrderRecord| {
            order.use_id == open.use_id && order.pre_hook.nonce == open.pre_hook.nonce
        };
        if self
            .orders
            .iter()
            .any(|order| competes(order) && order.observations.pre_hook_executed.is_some())
            || self.fundings.iter().any(|funding| {
                Some(funding.use_id) == open.use_id && funding.pre_hook_nonce == open.pre_hook.nonce
            })
        {
            return Some(SwapAccountRefusal::SpeedUpNonceUsed);
        }
        let admitted = self.orders.iter().all(|order| {
            let observed = &order.observations;
            if order.uid == open.uid {
                observed.traded.is_none()
                    && observed.pre_hook_dead.is_none()
                    && observed.expired.is_none()
                    && observed.bridge_handoff.is_none()
            } else if competes(order) {
                observed.traded.is_none() && observed.bridge_handoff.is_none()
            } else {
                self.order_admits_attempt(order)
            }
        });
        (!admitted).then_some(SwapAccountRefusal::SpeedUpOrderChanged)
    }

    /// Whether a second order may be signed against the open order `speed_up` names.
    pub(super) fn admits_speed_up(&self, speed_up: &SwapSpeedUp) -> bool {
        self.speed_up_refusal(speed_up).is_none()
    }

    /// Whether `order` is part of a trade that completed at or below `cutoff`: the trade, its
    /// delivery on the swap's chain and, for a Bridge order, a delivery on the destination
    /// chain. The trade and the delivery may sit on different attempts of one use, and both
    /// are then settled together. An attempt that holds neither is not a settled order.
    fn order_settled_at(&self, order: &SwapOrderRecord, cutoff: u64) -> bool {
        let within = |event: Option<SwapObservation>| {
            event.is_some_and(|event| event.block.number <= cutoff)
        };
        let (traded, delivery) = if order.observations.traded.is_some() {
            (Some(order), self.delivery_order(order))
        } else if order.observations.bridge_handoff.is_some() {
            (self.trade_order(order), Some(order))
        } else {
            return false;
        };
        traded.is_some_and(|traded| within(traded.observations.traded))
            && delivery.is_some_and(|delivery| {
                within(delivery.observations.delivered) && delivery.destination_delivered()
            })
    }

    /// The attempt whose approved terms delivered the trade of `traded`: the order itself once
    /// it holds its own `delivered`, or the untraded attempt of the same swap use whose
    /// hand-off was read from that trade's receipt. Nothing from another use is paired.
    #[must_use]
    pub fn delivery_order<'a>(
        &'a self,
        traded: &'a SwapOrderRecord,
    ) -> Option<&'a SwapOrderRecord> {
        if traded.observations.delivered.is_some() {
            return Some(traded);
        }
        let trade = traded.observations.traded?;
        let use_id = traded.use_id?;
        self.orders.iter().find(|order| {
            order.use_id == Some(use_id)
                && order.observations.traded.is_none()
                && order
                    .observations
                    .bridge_handoff
                    .is_some_and(|handoff| handoff.observation == trade)
        })
    }

    /// The inverse of [`Self::delivery_order`]: the attempt whose trade `delivery` handed off.
    /// An order holding both its trade and its `delivered` is its own trade order.
    #[must_use]
    pub fn trade_order<'a>(&'a self, delivery: &'a SwapOrderRecord) -> Option<&'a SwapOrderRecord> {
        if delivery.observations.traded.is_some() {
            return delivery.observations.delivered.map(|_| delivery);
        }
        let handoff = delivery.observations.bridge_handoff?;
        let use_id = delivery.use_id?;
        self.orders.iter().find(|order| {
            order.use_id == Some(use_id)
                && order.observations.traded == Some(handoff.observation)
                && order.observations.delivered.is_none()
        })
    }

    pub(super) fn has_use_ids(&self) -> bool {
        self.orders.iter().any(|order| order.use_id.is_some())
    }

    /// Orders from before swap uses all belong to their account's first use.
    pub(super) fn assign_use(&mut self, id: SwapUseId) {
        for order in &mut self.orders {
            order.use_id = Some(id);
        }
    }
}

/// Serialized with serde's external tag, so later kinds such as keeping the
/// output in the executor decode additively. Approvals without one are Reshield.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapDelivery {
    /// The executor receives the bought token and the post-hook shields it.
    #[default]
    Reshield,
    /// The order pays the bought token to `receiver` and carries no post-hook.
    External { receiver: Address },
    /// The order buys an intermediate token on this chain and hands it to a bridge provider,
    /// which delivers the destination token to the receiver on another chain.
    Bridge(BridgeDelivery),
    /// The order pays an intermediate token on this chain to a NEAR Intents deposit address,
    /// which delivers to a receiver on a non-EVM network. Surplus is bridged with it.
    NonEvmBridge(NonEvmDelivery),
}

impl SwapDelivery {
    /// The Bridge delivery of a swap that shields its proceeds on the destination chain.
    #[must_use]
    pub const fn private_bridge(&self) -> Option<BridgeDelivery> {
        match self {
            Self::Bridge(bridge) if bridge.is_private() => Some(*bridge),
            _ => None,
        }
    }

    /// Whether a bridge provider delivers the swap on another chain or network.
    #[must_use]
    pub const fn is_bridge(&self) -> bool {
        matches!(self, Self::Bridge(_) | Self::NonEvmBridge(_))
    }

    /// The bridge provider of a Bridge delivery: always NEAR Intents for a non-EVM one.
    #[must_use]
    pub const fn bridge_provider(&self) -> Option<BridgeProvider> {
        match self {
            Self::Bridge(bridge) => Some(bridge.provider),
            Self::NonEvmBridge(_) => Some(BridgeProvider::NearIntents),
            Self::Reshield | Self::External { .. } => None,
        }
    }

    /// Whether the order carries a post-hook at the pre-hook's nonce plus one: Reshield's
    /// shield, or Across's deposit.
    #[must_use]
    pub const fn has_post_hook(&self) -> bool {
        matches!(
            self,
            Self::Reshield
                | Self::Bridge(BridgeDelivery {
                    provider: BridgeProvider::Across,
                    ..
                })
        )
    }
    /// Whether the order's receiver is the executor, so the bought token lands there first.
    /// The same orders carry a post-hook that moves it on.
    #[must_use]
    pub const fn pays_executor(&self) -> bool {
        self.has_post_hook()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BridgeProvider {
    Across,
    NearIntents,
}

/// What happens to `CoW` surplus above the order's buy amount. Across either reshields it or
/// leaves it in the stealth account; NEAR Intents converts the whole deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BridgeSurplus {
    Reshield,
    KeepInAccount,
    BridgedByProvider,
}

/// What a private Bridge delivery's handler message does when the destination shield fails.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BridgeShieldFailure {
    /// The handler message names no fallback, so a fill whose shield fails can't complete and
    /// Across refunds the deposit on the swap's chain.
    #[default]
    RefundOnOrigin,
    /// The message names the destination stealth account as fallback, so the fill completes and
    /// the account holds the tokens.
    KeepOnDestination,
}

/// The terms of a Bridge delivery that shields to the wallet on the destination chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgePrivateDelivery {
    pub on_shield_failure: BridgeShieldFailure,
}

/// The destination terms of a Bridge order, all bound by its approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeDelivery {
    pub provider: BridgeProvider,
    pub destination_chain: u64,
    pub receiver: Address,
    /// The token delivered on the destination chain. `Address::ZERO` is its native asset,
    /// which only NEAR Intents delivers. Across delivers its route's destination token; WETH
    /// on Ethereum or Arbitrum One reaches receivers without code as ETH.
    pub destination_token: Address,
    pub surplus: BridgeSurplus,
    /// `Some` when the delivery shields to the wallet on the destination chain. `receiver` is
    /// then the destination stealth account. `None` when the bridge pays `receiver` unshielded.
    #[serde(default)]
    pub private: Option<BridgePrivateDelivery>,
}

impl BridgeDelivery {
    #[must_use]
    pub const fn is_private(&self) -> bool {
        self.private.is_some()
    }
    /// Only Across runs a call on delivery, so NEAR Intents has no private delivery.
    #[must_use]
    pub const fn has_valid_private_delivery(&self) -> bool {
        self.private.is_none() || matches!(self.provider, BridgeProvider::Across)
    }
    /// Across reshields or keeps surplus; NEAR Intents always bridges it.
    #[must_use]
    pub const fn has_valid_surplus(&self) -> bool {
        matches!(
            (self.provider, self.surplus),
            (
                BridgeProvider::Across,
                BridgeSurplus::Reshield | BridgeSurplus::KeepInAccount
            ) | (
                BridgeProvider::NearIntents,
                BridgeSurplus::BridgedByProvider
            )
        )
    }
}

/// The destination terms of a NEAR Intents delivery to a non-EVM network, all bound by its
/// approval. Symbol and decimals are kept so the swap is shown without 1Click's token list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonEvmDelivery {
    pub network: NonEvmNetwork,
    /// In the canonical form of [`NonEvmNetwork::parse_receiver`].
    pub receiver: String,
    /// 1Click's `destinationAsset`.
    pub asset_id: String,
    /// The mint or contract as 1Click lists it, `None` for the network's native asset.
    pub contract: Option<String>,
    pub symbol: String,
    pub decimals: u8,
}

/// A Bridge order's provider terms, persisted with the order before it is submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BridgeOrderTerms {
    Across(AcrossOrderTerms),
    NearIntents(NearIntentsOrderTerms),
}

impl BridgeOrderTerms {
    #[must_use]
    pub const fn provider(&self) -> BridgeProvider {
        match self {
            Self::Across(_) => BridgeProvider::Across,
            Self::NearIntents(_) => BridgeProvider::NearIntents,
        }
    }
}

/// The signed `depositV3` arguments besides the depositor (the executor) and the destination
/// chain. A private delivery's deposit pays `recipient`, Across's handler, with a message of
/// hash `message_hash`; the message itself is in the recorded post-hook. Without them, for a
/// delivery that doesn't shield, it pays the delivery's receiver with an empty message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcrossOrderTerms {
    pub spoke_pool: Address,
    pub input_token: Address,
    pub output_token: Address,
    /// The order's buy amount.
    pub input_amount: U256,
    /// The approved destination minimum.
    pub output_amount: U256,
    pub quote_timestamp: u32,
    pub fill_deadline: u32,
    pub exclusive_relayer: Address,
    pub exclusivity_parameter: u32,
    #[serde(default)]
    pub recipient: Option<Address>,
    #[serde(default)]
    pub message_hash: Option<B256>,
}

impl AcrossOrderTerms {
    /// The deposit's signed recipient.
    #[must_use]
    pub fn deposit_recipient(&self, delivery: BridgeDelivery) -> Address {
        self.recipient.unwrap_or(delivery.receiver)
    }
    /// What `FilledRelay.messageHash` holds for the deposit: zero for an empty message, and
    /// otherwise the message's keccak256 hash.
    #[must_use]
    pub fn deposit_message_hash(&self) -> B256 {
        self.message_hash.unwrap_or(B256::ZERO)
    }
    /// Whether a deposit matching these terms for `delivery` also matches `other` for
    /// `other_delivery`. These are the fields the `FundsDeposited` event shows. The event
    /// carries the resolved exclusivity deadline, not the signed parameter, so that is not
    /// compared.
    #[must_use]
    pub fn signs_same_deposit(
        &self,
        delivery: BridgeDelivery,
        other: &Self,
        other_delivery: BridgeDelivery,
    ) -> bool {
        let recipient = self.deposit_recipient(delivery);
        let other_recipient = other.deposit_recipient(other_delivery);
        self.spoke_pool == other.spoke_pool
            && recipient == other_recipient
            && self.message_hash == other.message_hash
            && delivery.destination_chain == other_delivery.destination_chain
            && self.input_token == other.input_token
            && self.output_token == other.output_token
            && self.input_amount == other.input_amount
            && self.output_amount == other.output_amount
            && self.quote_timestamp == other.quote_timestamp
            && self.fill_deadline == other.fill_deadline
            && self.exclusive_relayer == other.exclusive_relayer
    }
}

/// A verified 1Click quote the order pays into.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NearIntentsOrderTerms {
    /// The order's receiver.
    pub deposit_address: Address,
    pub min_amount_out: U256,
    pub amount_out: U256,
    /// The signed quote's `deadline`.
    pub deadline: String,
    /// The exact 1Click response body: request, quote, signature and timestamp.
    pub signed_quote: String,
}

// The deposit address and the signed quote name the swap's receiver and deposit, so neither is
// formatted.
impl std::fmt::Debug for NearIntentsOrderTerms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NearIntentsOrderTerms")
            .field("min_amount_out", &self.min_amount_out)
            .field("amount_out", &self.amount_out)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// One anchor reading approved with an order: a Chainlink aggregator with its
/// round's `updatedAt`, or a Uniswap V3 pool read at the head block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapAnchorObservation {
    pub source: Address,
    pub block: BlockNumHash,
    /// Block timestamp in Unix seconds.
    pub block_timestamp: u64,
    /// Chainlink round update time in Unix seconds.
    pub updated_at: Option<u64>,
}

/// The approved maximum private fee of one stealth account's setup, with the fee token it is
/// counted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapSetupFeeLimit {
    pub token: Address,
    /// In `token` base units.
    pub amount: U256,
}

/// Values the user approved for one order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapApprovedBounds {
    /// The order's `sellAmount`: what the pre-hook's unshield leaves the executor after
    /// Railgun's unshield fee.
    pub sell_amount: U256,
    /// The private spend the pre-hook unshields. `None` in records from before orders sold
    /// the amount after the unshield fee; those orders sold the whole unshield amount.
    #[serde(default)]
    pub unshield_amount: Option<U256>,
    /// Railgun's unshield fee taken from `unshield_amount`. Zero in records from before orders
    /// sold the amount after that fee.
    #[serde(default)]
    pub unshield_fee_bps: U256,
    /// The order's `buyAmount` `B`, also enforced by the post-hook balance guard.
    pub buy_amount: U256,
    /// `M`, the minimum received privately after the shield fee.
    pub private_minimum: U256,
    pub shield_fee_bps: U256,
    /// The price tolerance applied to the quote's best case, in basis points. Records from
    /// before gas shares applied it after deducting all gas; the name is kept for decoding.
    pub slippage_bps: u32,
    pub pre_hook_gas_limit: u64,
    /// `None` for an order without a post-hook.
    #[serde(default)]
    pub post_hook_gas_limit: Option<u64>,
    /// The reviewed gas estimate in buy-token units, the same as `gas_estimate` in records with
    /// a gas share. Earlier records hold the network and hook cost their minimum deducted.
    #[serde(default)]
    pub hook_cost: Option<U256>,
    pub anchors: Vec<SwapAnchorObservation>,
    /// The approved minimum received on a Bridge order's destination chain: Across's output
    /// amount or 1Click's `minAmountOut`. `None` for same-chain delivery.
    #[serde(default)]
    pub destination_minimum: Option<U256>,
    /// The selected share of `gas_estimate` the quote was priced at, in basis points of 10,000.
    /// `None` in records from before gas shares.
    #[serde(default)]
    pub gas_share_bps: Option<u16>,
    /// The gas estimate in buy-token units: the quote's swap gas and the hooks' conservative gas
    /// at the RPC gas price with its 25% cushion, plus any rollup data cost. `None` in records
    /// from before gas shares.
    #[serde(default)]
    pub gas_estimate: Option<U256>,
    /// The allowed gas in buy-token units. For an approval, `gas_share_bps` of `gas_estimate`
    /// rounded up. For an order attempt, the gas the signed minimum leaves room for, which
    /// differs from that share when the order kept an approved minimum. `None` in records from
    /// before gas shares.
    #[serde(default)]
    pub gas_allowance: Option<U256>,
    /// The RPC gas price in wei at quote time, without the cushion. `None` in records from
    /// before gas shares.
    #[serde(default)]
    pub gas_price_wei: Option<u128>,
    /// How long the order is valid after signing, in seconds. `None` in records from before
    /// the validity was chosen, whose orders used the swap profile's window.
    #[serde(default)]
    pub valid_for_secs: Option<u32>,
    /// The destination chain's shield fee rate, in basis points, that a private Bridge
    /// delivery's approval binds. `None` for other deliveries.
    #[serde(default)]
    pub destination_shield_fee_bps: Option<U256>,
    /// The gas allowance deducted from the Across output for a private Bridge delivery's fill,
    /// in destination-token base units. `None` for other deliveries.
    #[serde(default)]
    pub delivery_allowance: Option<U256>,
    /// The approved maximum private setup fee on the destination chain, with its fee token.
    /// `None` when the destination account needs no setup, and for other deliveries.
    #[serde(default)]
    pub destination_setup_fee: Option<SwapSetupFeeLimit>,
    /// The approved maximum private setup fee on the swap's own chain, with its fee token.
    /// `None` when the source account needs no setup because the swap use reuses its account.
    #[serde(default)]
    pub source_setup_fee: Option<SwapSetupFeeLimit>,
}

impl SwapApprovedBounds {
    /// The private spend the pre-hook unshields, also for records without `unshield_amount`.
    #[must_use]
    pub fn spend_amount(&self) -> U256 {
        self.unshield_amount.unwrap_or(self.sell_amount)
    }
}

/// Terms the user approved before the swap's setup, kept with the record so the order can be
/// placed once the setup is confirmed, also after a restart. The order uses these bounds only
/// while a fresh review keeps them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapApproval {
    pub bounds: SwapApprovedBounds,
    /// Whether the reviewed price was checked against anchors, including cached rates with
    /// no block observations. Older approvals infer this from `bounds.anchors`.
    #[serde(default)]
    pub price_verified: Option<bool>,
    /// The user accepted a price that no configured anchor checks.
    pub price_acknowledged: bool,
    /// Older approvals decode as Reshield.
    #[serde(default)]
    pub delivery: SwapDelivery,
    /// `None` in older approvals, whose pair is the record's assets. Read the pair through
    /// [`ExecutorRecord::swap_approval_tokens`].
    #[serde(default)]
    pub tokens: Option<SwapApprovalTokens>,
    /// The stealth accounts the approval binds and whether each needs setup. `None` in
    /// approvals from before accounts were bound, which bind neither.
    #[serde(default)]
    pub accounts: Option<SwapApprovedAccounts>,
}

/// The stealth accounts an approval binds: the swap's own, and for a private Bridge delivery
/// the destination chain's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapApprovedAccounts {
    pub source: SwapApprovedAccount,
    #[serde(default)]
    pub destination: Option<SwapApprovedAccount>,
}

/// One approved stealth account and whether the swap sets it up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapApprovedAccount {
    /// `None` for an approved new account until its derivation binds the address.
    #[serde(default)]
    pub address: Option<Address>,
    /// The approved swap sets the account up. False for an existing account.
    pub setup: bool,
}

impl SwapApprovedAccount {
    /// Whether the account at `address`, with the setup need `setup`, is the approved one. An
    /// approved new account without an address takes the one its derivation binds.
    #[must_use]
    pub fn admits(&self, address: Address, setup: bool) -> bool {
        self.setup == setup && self.address.is_none_or(|approved| approved == address)
    }
}

impl SwapApprovedAccounts {
    /// Whether `source` and `destination`, each an account's address and setup need, are the
    /// approved accounts.
    #[must_use]
    pub fn admits(&self, source: (Address, bool), destination: Option<(Address, bool)>) -> bool {
        self.source.admits(source.0, source.1)
            && match (self.destination, destination) {
                (None, None) => true,
                (Some(approved), Some((address, setup))) => approved.admits(address, setup),
                _ => false,
            }
    }
}

/// The approved sell and buy tokens. A native Buy asset is `Address::ZERO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapApprovalTokens {
    pub sell: Address,
    pub buy: Address,
}

/// Identity of an issued hook payload, also retained in the record's issued list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapHookPayload {
    nonce: U256,
    payload: B256,
}

impl SwapHookPayload {
    const fn of(payload: &IssuedExecutorPayload) -> Self {
        Self {
            nonce: payload.nonce,
            payload: payload.hash,
        }
    }
    #[must_use]
    pub const fn nonce(&self) -> U256 {
        self.nonce
    }
    #[must_use]
    pub const fn payload(&self) -> B256 {
        self.payload
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapObservation {
    pub block: BlockNumHash,
    pub transaction_hash: Option<B256>,
}

/// Canonical post-hook shield evidence and the amount credited privately, after fees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SwapShieldObservation {
    pub observation: SwapObservation,
    pub private_amount: U256,
    /// The shield fee the event charged for the credited commitment. `None` when the event
    /// carried none for it, or the observation predates recording it.
    #[serde(default)]
    pub fee: Option<U256>,
}

/// Executed amounts from the order's canonical settlement `Trade` event, kept with `traded`,
/// and the settlement's gas cost and the fee the orderbook charged when they were read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapTradeAmounts {
    pub sell_amount: U256,
    pub buy_amount: U256,
    /// The order's signed fee, in the sell token. Zero for orders without one.
    pub fee_amount: U256,
    /// `gasUsed` of the settlement transaction's receipt. `None` until it is recorded, and in
    /// older records.
    #[serde(default)]
    pub settlement_gas_used: Option<u64>,
    /// `effectiveGasPrice` in wei of the settlement transaction's receipt. `None` until it is
    /// recorded, and in older records.
    #[serde(default)]
    pub settlement_effective_gas_price: Option<u128>,
    /// The orderbook's `executedFee` for the order, in `executed_fee_token` base units. It is
    /// reported by the orderbook and is not chain evidence. `None` until it is read.
    #[serde(default)]
    pub executed_fee: Option<U256>,
    /// The orderbook's `executedFeeToken`. `None` until it is read.
    #[serde(default)]
    pub executed_fee_token: Option<Address>,
}

/// What consumed the pre-hook's nonce, or `Expired` when `validTo` passed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapPreHookDeathCause {
    Cancellation,
    Recovery,
    OlderPostHook,
    Expired,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapPreHookDeath {
    pub cause: SwapPreHookDeathCause,
    pub observation: SwapObservation,
}

/// Canonical observations, each recorded with its own block evidence. None of
/// them implies another. Observations established from state at a finalized
/// block carry that block and no transaction hash.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapOrderObservations {
    pub pre_hook_executed: Option<SwapObservation>,
    pub traded: Option<SwapObservation>,
    /// The amounts of the trade recorded in `traded`, and only with it. `None` in records from
    /// before amounts were kept; a retained trade isn't read again to fill them in.
    #[serde(default)]
    pub trade_amounts: Option<SwapTradeAmounts>,
    /// Finalized block establishing delivery. For Reshield delivery it establishes the
    /// approved private minimum, and the transaction is the post-hook shield. For External
    /// delivery it is the trade itself, and for Bridge delivery the hand-off to the bridge.
    pub delivered: Option<SwapObservation>,
    /// Retained separately so a later page can establish delivery after a balance change.
    #[serde(default)]
    pub shielded: Option<SwapShieldObservation>,
    /// Private credit verified from a settlement receipt's token transfers and Shield event.
    /// Unlike `shielded`, this does not establish which payload consumed a hook nonce.
    #[serde(default)]
    pub settlement_credit: Option<SwapShieldObservation>,
    pub pre_hook_dead: Option<SwapPreHookDeath>,
    /// Finalized block after the trade at which the executor still held buy tokens.
    #[serde(default)]
    pub undelivered: Option<SwapObservation>,
    /// Finalized block past `validTo` at which the order had not filled.
    #[serde(default)]
    pub expired: Option<SwapObservation>,
    /// A Bridge order's hand-off on this chain: the trade that paid the NEAR Intents deposit
    /// address, or the Across deposit in the settlement that paid the executor.
    #[serde(default)]
    pub bridge_handoff: Option<SwapBridgeHandoff>,
    /// An Across post-hook's deposit, found by explicit reconciliation in a block where the
    /// nonce passed the post-hook's. Like `shielded`, it shows which payload took that nonce.
    #[serde(default)]
    pub post_hook_deposit: Option<SwapObservation>,
    /// The bridge's result on the destination chain. It is not evidence on this chain.
    #[serde(default)]
    pub bridge_outcome: Option<SwapBridgeOutcome>,
    /// Across's refund of a refunding order's deposit to the executor, verified in a finalized
    /// block's receipts by an explicit status check. `None` in older records and until a
    /// refund is verified; it never means the refund was recovered.
    #[serde(default)]
    pub bridge_refund: Option<SwapObservation>,
}

impl SwapOrderObservations {
    /// Blocks on this chain. A bridge outcome's block is on the destination chain.
    fn blocks(&self) -> impl Iterator<Item = BlockNumHash> {
        [
            self.pre_hook_executed,
            self.traded,
            self.delivered,
            self.shielded.map(|shield| shield.observation),
            self.settlement_credit.map(|shield| shield.observation),
            self.pre_hook_dead.map(|death| death.observation),
            self.undelivered,
            self.expired,
            self.bridge_handoff.map(|handoff| handoff.observation),
            self.post_hook_deposit,
            self.bridge_refund,
        ]
        .into_iter()
        .flatten()
        .map(|observation| observation.block)
    }

    /// `amounts` for the trade being recorded. The same trade keeps what was read about it
    /// since it was recorded: the orderbook's executed fee, and the settlement gas from an
    /// earlier receipt.
    fn kept_trade_amounts(&self, amounts: SwapTradeAmounts) -> SwapTradeAmounts {
        match self.trade_amounts {
            Some(known) if self.traded.is_some() => SwapTradeAmounts {
                settlement_gas_used: known.settlement_gas_used.or(amounts.settlement_gas_used),
                settlement_effective_gas_price: known
                    .settlement_effective_gas_price
                    .or(amounts.settlement_effective_gas_price),
                executed_fee: known.executed_fee.or(amounts.executed_fee),
                executed_fee_token: known.executed_fee_token.or(amounts.executed_fee_token),
                ..amounts
            },
            _ => amounts,
        }
    }

    /// Canonical evidence that the order's post-hook ran: its shield, or its Across deposit.
    #[must_use]
    pub const fn post_hook_evidence(&self) -> bool {
        self.shielded.is_some() || self.post_hook_deposit.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapBridgeHandoff {
    pub observation: SwapObservation,
    /// The Across `depositId`. `None` for NEAR Intents, whose persisted deposit address
    /// identifies the transfer.
    pub deposit_id: Option<U256>,
}

/// `DeliveredVerified`, `DeliveredReported`, `Refunding` and `HeldOnDestination` are final.
/// `NeedsAttention` stops automatic polling, but an explicit status check may replace it. An
/// explicit check may also replace an Across `Refunding` with `DeliveredVerified` or
/// `HeldOnDestination` once it verifies the matching fill, and an Across `DeliveredReported`
/// with `DeliveredVerified` once it verifies the fill or with `Refunding` when Across then
/// reports the deposit expired or refunded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapBridgeOutcome {
    /// Across's fill, checked in the destination chain's finalized receipts. `output_amount` is
    /// the fill's executed amount, at least the signed one. `shielded` is set when the fill's
    /// receipt also holds the transfer to the destination stealth account and its shield;
    /// `output_amount` is then the amount the account received, before Railgun's shield fee.
    DeliveredVerified {
        block: BlockNumHash,
        transaction_hash: B256,
        output_amount: U256,
        shielded: bool,
    },
    /// Success as reported by the provider, not checked on the destination chain: 1Click's
    /// report, or Across's when the final fill block holds no fill the wallet can verify or the
    /// destination chain's endpoints serve no whole-block receipts.
    DeliveredReported {
        amount_out: Option<U256>,
        transaction_hash: Option<B256>,
        /// 1Click's destination transaction on a non-EVM network, as it reports it. `None`
        /// for other deliveries, whose transaction is `transaction_hash`.
        #[serde(default)]
        destination_transaction: Option<String>,
    },
    /// The bridge expired or refunded the deposit, which returns to the executor on this chain.
    Refunding,
    /// 1Click reported a failed or incomplete deposit.
    NeedsAttention,
    /// A private delivery's fill completed and the destination stealth account holds `amount`
    /// of the token, with no shield in that receipt.
    HeldOnDestination {
        block: BlockNumHash,
        transaction_hash: B256,
        amount: U256,
    },
}

impl SwapBridgeOutcome {
    #[must_use]
    pub const fn is_final(&self) -> bool {
        !matches!(self, Self::NeedsAttention)
    }
    #[must_use]
    pub const fn is_delivered(&self) -> bool {
        matches!(
            self,
            Self::DeliveredVerified { .. } | Self::DeliveredReported { .. }
        )
    }
}

/// What the rules for recording a bridge outcome read of the delivery it belongs to: a Bridge
/// order's, or that of a swap paid from a Public account.
pub(super) struct BridgeOutcomeRules {
    pub(super) handed_off: bool,
    pub(super) private: bool,
    pub(super) destination_minimum: Option<U256>,
    /// Whether Across bridges the delivery. Only its outcomes are corrected.
    pub(super) across: bool,
    /// The recorded outcome.
    pub(super) known: Option<SwapBridgeOutcome>,
    /// Whether a refund of the deposit was verified, which rules a later fill out.
    pub(super) refund_verified: bool,
}

impl BridgeOutcomeRules {
    /// Whether `outcome` may be recorded. A final outcome is never replaced, though recording
    /// it again is accepted; `NeedsAttention` may be replaced by any outcome. The exceptions
    /// are an Across `Refunding` without a verified refund, which a verified fill replaces, and
    /// an Across `DeliveredReported`, which a verified fill or a refund replaces. A verified
    /// delivery must meet the approved destination minimum. A private delivery's fill is a
    /// shielded delivery or held on the destination chain, and no other delivery is either.
    pub(super) fn admits(&self, outcome: &SwapBridgeOutcome) -> bool {
        let private_fits = match *outcome {
            SwapBridgeOutcome::DeliveredVerified { shielded, .. } => shielded == self.private,
            SwapBridgeOutcome::HeldOnDestination { .. } => self.private,
            _ => true,
        };
        let below_minimum = match *outcome {
            SwapBridgeOutcome::DeliveredVerified { output_amount, .. } => self
                .destination_minimum
                .is_none_or(|minimum| output_amount < minimum),
            _ => false,
        };
        // Across may have filled a deposit it reported expired. A refund verified on the
        // deposit's chain rules that fill out.
        let corrects_refund = self.known == Some(SwapBridgeOutcome::Refunding)
            && matches!(
                outcome,
                SwapBridgeOutcome::DeliveredVerified { .. }
                    | SwapBridgeOutcome::HeldOnDestination { .. }
            )
            && self.across
            && !self.refund_verified;
        // Nothing on the destination chain backs a delivery Across only reported, so a
        // verified fill or a refund replaces it.
        let corrects_report = matches!(
            self.known,
            Some(SwapBridgeOutcome::DeliveredReported { .. })
        ) && matches!(
            outcome,
            SwapBridgeOutcome::DeliveredVerified { .. } | SwapBridgeOutcome::Refunding
        ) && self.across;
        self.handed_off
            && private_fits
            && !below_minimum
            && !self.known.as_ref().is_some_and(|known| {
                known.is_final() && known != outcome && !corrects_refund && !corrects_report
            })
    }
}

/// Whether Across's `refund` of a refunding deposit may be recorded: in a block after the
/// deposit's `handoff`, on the same chain, with its transaction. A recorded refund, `known`, is
/// never replaced, though recording it again is accepted.
pub(super) fn bridge_refund_admitted(
    across: bool,
    outcome: Option<&SwapBridgeOutcome>,
    handoff: Option<SwapBridgeHandoff>,
    known: Option<SwapObservation>,
    refund: SwapObservation,
) -> bool {
    across
        && outcome == Some(&SwapBridgeOutcome::Refunding)
        && refund.transaction_hash.is_some()
        && handoff.is_some_and(|handoff| refund.block.number > handoff.observation.block.number)
        && known.is_none_or(|known| known == refund)
}

/// The original signed request, retained encrypted so submission can resume after restart.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapSubmission {
    signature: FixedBytes<65>,
    quote_id: Option<i64>,
}

impl std::fmt::Debug for SwapSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwapSubmission").finish_non_exhaustive()
    }
}

impl SwapSubmission {
    #[must_use]
    pub const fn new(signature: [u8; 65], quote_id: Option<i64>) -> Self {
        Self {
            signature: FixedBytes(signature),
            quote_id,
        }
    }
    #[must_use]
    pub const fn signature(&self) -> &[u8; 65] {
        &self.signature.0
    }
    #[must_use]
    pub const fn quote_id(&self) -> Option<i64> {
        self.quote_id
    }
}

/// Orderbook acceptance is separate from canonical execution. Pending includes an
/// interrupted request whose outcome is unknown; it never releases input reservations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapSubmissionStatus {
    #[default]
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapOrderRecord {
    #[serde(default)]
    terms: Option<SwapTerms>,
    attempt: u32,
    uid: FixedBytes<56>,
    delivery: SwapDelivery,
    bounds: SwapApprovedBounds,
    pre_hook: SwapHookPayload,
    /// `None` for an order without a post-hook, such as External delivery.
    #[serde(default)]
    post_hook: Option<SwapHookPayload>,
    invalidates: Option<FixedBytes<56>>,
    observations: SwapOrderObservations,
    #[serde(default)]
    submission: Option<SwapSubmission>,
    #[serde(default)]
    submission_status: SwapSubmissionStatus,
    /// Required for Bridge delivery, with the delivery's provider, and absent otherwise.
    #[serde(default)]
    bridge: Option<BridgeOrderTerms>,
    /// The swap use this order is an attempt of. Stored only with the record's uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    use_id: Option<SwapUseId>,
}

impl SwapOrderRecord {
    #[must_use]
    pub const fn use_id(&self) -> Option<SwapUseId> {
        self.use_id
    }

    /// A Bridge order's outcome on the destination chain is a delivery. Other orders have no
    /// destination outcome to wait for.
    pub(super) fn destination_delivered(&self) -> bool {
        !self.delivery.is_bridge()
            || self
                .observations
                .bridge_outcome
                .as_ref()
                .is_some_and(SwapBridgeOutcome::is_delivered)
    }

    /// A Bridge order's provider terms.
    #[must_use]
    pub const fn bridge(&self) -> Option<&BridgeOrderTerms> {
        self.bridge.as_ref()
    }

    #[must_use]
    pub const fn submission(&self) -> Option<&SwapSubmission> {
        self.submission.as_ref()
    }
    #[must_use]
    pub const fn submission_status(&self) -> SwapSubmissionStatus {
        self.submission_status
    }

    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
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
    pub fn delivery(&self) -> SwapDelivery {
        self.delivery.clone()
    }
    #[must_use]
    pub const fn bounds(&self) -> &SwapApprovedBounds {
        &self.bounds
    }
    #[must_use]
    pub const fn pre_hook(&self) -> SwapHookPayload {
        self.pre_hook
    }
    #[must_use]
    pub const fn post_hook(&self) -> Option<SwapHookPayload> {
        self.post_hook
    }
    /// An earlier order of this executor that the pre-hook invalidates.
    #[must_use]
    pub fn invalidates(&self) -> Option<OrderUid> {
        self.invalidates.map(OrderUid)
    }
    #[must_use]
    pub fn observations(&self) -> SwapOrderObservations {
        self.observations.clone()
    }

    /// The attempt has ended once its pre-hook can never run.
    const fn has_ended(&self) -> bool {
        self.observations.pre_hook_dead.is_some() && self.observations.pre_hook_executed.is_none()
    }
}

/// A new attempt's approved order and its signed hooks. The store assigns the
/// attempt index and derives hook identities from the payloads.
#[derive(Debug, Clone)]
pub struct SwapAttempt {
    /// The swap use the order was signed for, which must still claim the account.
    pub use_id: SwapUseId,
    pub terms: SwapTerms,
    pub proof: SwapProof,
    pub uid: OrderUid,
    pub submission: Option<SwapSubmission>,
    pub delivery: SwapDelivery,
    pub bounds: SwapApprovedBounds,
    pub invalidates: Option<OrderUid>,
    pub pre_hook: IssuedExecutorPayload,
    /// Required exactly when [`SwapDelivery::has_post_hook`].
    pub post_hook: Option<IssuedExecutorPayload>,
    /// Required for Bridge delivery, with the delivery's provider, and absent otherwise.
    pub bridge: Option<BridgeOrderTerms>,
    /// The open order this attempt speeds up. `None` for a first order or an ordinary retry.
    pub speed_up: Option<OrderUid>,
}

impl ExecutorRecord {
    /// Completed swaps are durable at the accepted safety cutoff, whichever role the account
    /// had in them: every order traded and delivered, and every destination use that signed a
    /// shield had its fill run one, all at or below the cutoff. Reuse still needs a fresh nonce
    /// and delegation, but no historical RPC evidence for these outcomes. A setup is settled
    /// once its nonce is resolved. For a shield a consumed nonce is not such an outcome: a
    /// signed shield without a shielded delivery leaves the account unsettled, as does any
    /// recovery.
    pub(crate) fn settled_swaps_at(&self, cutoff: u64) -> bool {
        let orders = self
            .swap
            .as_ref()
            .map_or(&[][..], |swap| swap.orders.as_slice());
        let mut delivered_shields = Vec::new();
        for swap_use in &self.swap_uses {
            let (SwapUseRole::Destination {
                shields, outcome, ..
            }
            | SwapUseRole::PublicSourceDestination {
                shields, outcome, ..
            }) = &swap_use.role
            else {
                continue;
            };
            match outcome {
                Some(SwapDestinationOutcome::Shielded { block, .. }) if block.number <= cutoff => {
                    delivered_shields.extend_from_slice(shields);
                }
                _ if shields.is_empty() => {}
                _ => return false,
            }
        }
        self.nonce_observation
            .is_some_and(|observed| observed.block.number <= cutoff)
            && !(orders.is_empty() && delivered_shields.is_empty())
            && self.swap.as_ref().is_none_or(|swap| {
                swap.orders
                    .iter()
                    .all(|order| swap.order_settled_at(order, cutoff))
            })
            && self.issued.iter().all(|payload| match payload.purpose {
                ExecutorPayloadPurpose::SwapPreHook | ExecutorPayloadPurpose::SwapPostHook => {
                    orders.iter().any(|order| {
                        order.pre_hook.payload == payload.hash
                            || order
                                .post_hook
                                .is_some_and(|hook| hook.payload == payload.hash)
                    })
                }
                ExecutorPayloadPurpose::Operation => self.nonce_resolved(payload.nonce),
                ExecutorPayloadPurpose::SwapDestinationShield => {
                    delivered_shields.contains(&payload.hash)
                }
                ExecutorPayloadPurpose::Recovery => false,
            })
    }

    #[must_use]
    pub const fn swap(&self) -> Option<&SwapOperationRecord> {
        self.swap.as_ref()
    }

    /// Bridge orders handed off on this chain without a destination outcome, which their
    /// provider is polled for. `NeedsAttention` waits for an explicit status check.
    pub fn swap_bridges_to_track(&self) -> impl Iterator<Item = &SwapOrderRecord> {
        self.swap
            .iter()
            .flat_map(|swap| &swap.orders)
            .filter(|order| {
                order.delivery.is_bridge()
                    && order.observations.bridge_handoff.is_some()
                    && order.observations.bridge_outcome.is_none()
            })
    }

    /// Whether an attempt of the use `id` with these tokens and `delivery` keeps the fixed
    /// terms of every earlier attempt of that use: the sell token, the buy token (a Bridge
    /// order's intermediate token) and the whole delivery, which for Bridge holds the provider,
    /// destination chain, receiver, destination token and surplus choice. Amounts, deadlines
    /// and other quote-dependent terms may differ between attempts. A private Bridge attempt
    /// is not judged here.
    #[must_use]
    pub fn swap_use_keeps_terms(
        &self,
        id: SwapUseId,
        sell_token: Address,
        buy_token: Address,
        delivery: &SwapDelivery,
    ) -> bool {
        let Some(swap) = &self.swap else {
            return true;
        };
        delivery.private_bridge().is_some()
            || self.swap_use_orders(id).all(|order| {
                let terms = swap.order_terms(order);
                terms.sell_token == sell_token
                    && terms.buy_token == buy_token
                    && order.delivery == *delivery
            })
    }

    /// The terms the latest use approved before setup, while no order records its own.
    #[must_use]
    pub fn swap_approval(&self) -> Option<&SwapApproval> {
        self.swap_uses.last().and_then(SwapUseRecord::approval)
    }

    /// The sell and buy tokens approved with the setup. Approvals that predate the recorded
    /// pair, and records without an approval, use the setup's assets in sell-then-buy order.
    #[must_use]
    pub fn swap_approval_tokens(&self) -> Option<(Address, Address)> {
        if let Some(tokens) = self.swap_approval().and_then(|approval| approval.tokens) {
            return Some((tokens.sell, tokens.buy));
        }
        let mut tokens = self.assets.iter().filter_map(|asset| match asset {
            super::ExecutorAsset::Erc20(token) => Some(*token),
            _ => None,
        });
        Some((tokens.next()?, tokens.next()?))
    }

    /// Stopping setup prevents further swap work without discarding issued payloads.
    /// This is independent of the stealth account's presentation preference.
    #[must_use]
    pub const fn is_swap_setup_stopped(&self) -> bool {
        self.swap_setup_stopped
    }

    /// Only a dead, unexecuted pre-hook releases its notes. Removing that observation on
    /// reorg restores the reservation. Input reservation asks this while the pre-hook's
    /// nonce is unconsumed; at a resolved nonce its notes wait for private sync like any
    /// other payload's.
    pub(super) fn releases_swap_inputs(&self, pre_hook: B256) -> bool {
        self.nonce_observation.is_some()
            && self.swap.as_ref().is_some_and(|swap| {
                swap.orders
                    .iter()
                    .any(|order| order.pre_hook.payload == pre_hook && order.has_ended())
            })
    }

    /// Notes still reserved by this operation's swap pre-hooks. A recovery or early
    /// cancellation competes with the pre-hook for its nonce, and a setup sent again runs
    /// before a pre-hook on its included setup, so neither may spend them.
    #[must_use]
    pub(crate) fn swap_reserved_inputs(&self) -> Vec<ExecutorInputIdentity> {
        self.issued
            .iter()
            .filter(|payload| {
                payload.purpose == ExecutorPayloadPurpose::SwapPreHook
                    && !self.releases_swap_inputs(payload.hash)
            })
            .flat_map(|payload| payload.context.inputs.iter().cloned())
            .collect()
    }

    /// Whether a recovery at execution nonce `current` competes with `payload`. A payload is
    /// outstanding at the nonce it was signed for, with one exception: a swap pre-hook whose
    /// order validity ended at finalized depth while its nonce stayed unused can never run.
    /// A swap post-hook, signed for the nonce after its pre-hook's, is therefore outstanding
    /// only once that nonce is current. A destination shield has no expiry: even a verified
    /// bridge refund leaves it executable if the account is funded at its signed nonce.
    #[must_use]
    pub fn is_outstanding_at(&self, payload: &IssuedExecutorPayload, current: U256) -> bool {
        payload.nonce == current
            && (payload.purpose != ExecutorPayloadPurpose::SwapPreHook
                || !self.swap_pre_hook_expired(payload.hash))
    }

    /// What became of the shield payload of the destination stealth account `receiver`, from
    /// the private Bridge orders to it of this origin record's use `id`. A shield in any of
    /// those orders' fills wins, then a fill that left the token in the account. Otherwise only
    /// the use's latest order counts, since a retry can still be filled.
    pub(super) fn swap_destination_outcome(
        &self,
        id: SwapUseId,
        receiver: Address,
    ) -> Option<SwapDestinationOutcome> {
        let orders = || {
            self.swap
                .iter()
                .flat_map(|swap| &swap.orders)
                .filter(move |order| {
                    order.use_id == Some(id)
                        && matches!(
                            order.delivery,
                            SwapDelivery::Bridge(bridge)
                                if bridge.is_private() && bridge.receiver == receiver
                        )
                })
        };
        orders()
            .find_map(|order| match order.observations.bridge_outcome {
                Some(SwapBridgeOutcome::DeliveredVerified {
                    shielded: true,
                    block,
                    transaction_hash,
                    ..
                }) => Some(SwapDestinationOutcome::Shielded {
                    block,
                    transaction_hash,
                }),
                _ => None,
            })
            .or_else(|| {
                orders().find_map(|order| match order.observations.bridge_outcome {
                    Some(SwapBridgeOutcome::HeldOnDestination {
                        block,
                        transaction_hash,
                        ..
                    }) => Some(SwapDestinationOutcome::Held {
                        block,
                        transaction_hash,
                    }),
                    _ => None,
                })
            })
            .or_else(|| {
                (orders().next_back()?.observations.bridge_outcome
                    == Some(SwapBridgeOutcome::Refunding))
                .then_some(SwapDestinationOutcome::Unfilled)
            })
    }

    /// Whether a recorded payload holds a nonce past `current`, so the record is ahead of
    /// the chain. Only a swap post-hook may hold the next nonce, by the swap exception.
    #[must_use]
    pub fn records_future_nonce(&self, current: U256) -> bool {
        self.issued.iter().any(|payload| {
            payload.nonce > current
                && (payload.purpose != ExecutorPayloadPurpose::SwapPostHook
                    || current.checked_add(U256::ONE) != Some(payload.nonce))
        })
    }

    /// Whether a recovery review warns of a competing payload. A payload competes until the
    /// account's nonce is read past its own. Swap hooks run inside settlements, and a
    /// destination shield inside a relayer's fill, so they compete only while outstanding
    /// at the last nonce read.
    #[must_use]
    pub fn has_competing_payloads(&self) -> bool {
        self.issued.iter().any(|payload| {
            self.nonce_observation
                .is_none_or(|observed| payload.nonce >= observed.nonce)
                && (!matches!(
                    payload.purpose,
                    ExecutorPayloadPurpose::SwapPreHook
                        | ExecutorPayloadPurpose::SwapPostHook
                        | ExecutorPayloadPurpose::SwapDestinationShield
                ) || self
                    .nonce_observation
                    .is_none_or(|observed| self.is_outstanding_at(payload, observed.nonce)))
        })
    }

    /// A swap hook runs inside a settlement, and a destination shield inside a relayer's fill,
    /// where direct-call reconciliation never resolves it. Once the reconciled nonce is past
    /// its own, its signature can no longer execute.
    pub(super) fn swap_hook_nonce_passed(&self, payload: &IssuedExecutorPayload) -> bool {
        matches!(
            payload.purpose,
            ExecutorPayloadPurpose::SwapPreHook
                | ExecutorPayloadPurpose::SwapPostHook
                | ExecutorPayloadPurpose::SwapDestinationShield
        ) && self
            .nonce_observation
            .is_some_and(|observed| observed.nonce > payload.nonce)
    }

    /// Recorded at finalized depth: `validTo` passed while the pre-hook's nonce was unused.
    fn swap_pre_hook_expired(&self, pre_hook: B256) -> bool {
        self.nonce_observation.is_some()
            && self.swap.as_ref().is_some_and(|swap| {
                swap.orders.iter().any(|order| {
                    order.pre_hook.payload == pre_hook
                        && order.observations.pre_hook_executed.is_none()
                        && order
                            .observations
                            .pre_hook_dead
                            .is_some_and(|death| death.cause == SwapPreHookDeathCause::Expired)
                })
            })
    }

    /// A post-hook that runs inside a settlement never becomes a direct-call winner. Its nonce
    /// is resolved by its observed shield or Across deposit and consumed nonce, including when
    /// an older post-hook took another order's pre-hook nonce.
    fn swap_post_hook_took_nonce(&self, nonce: U256) -> bool {
        self.swap.as_ref().is_some_and(|swap| {
            swap.orders.iter().any(|order| {
                (order.post_hook.is_some_and(|hook| hook.nonce == nonce)
                    && order.observations.post_hook_evidence()
                    && self
                        .nonce_observation
                        .is_some_and(|observed| observed.nonce > nonce))
                    || order.pre_hook.nonce == nonce
                        && order.observations.pre_hook_dead.is_some_and(|death| {
                            death.cause == SwapPreHookDeathCause::OlderPostHook
                        })
            })
        })
    }

    /// The swap hook payload that took `nonce`, based on recorded observations: a pre-hook
    /// observed as executed, or the facts of `swap_post_hook_took_nonce`. When an older
    /// post-hook took an order's pre-hook nonce, it is named only if one order holds a
    /// post-hook at that nonce.
    #[must_use]
    pub fn swap_hook_winner(&self, nonce: U256) -> Option<B256> {
        self.swap_hook_at(
            nonce,
            self.nonce_observation.is_some(),
            self.nonce_observation
                .is_some_and(|observed| observed.nonce > nonce),
        )
    }

    /// [`Self::swap_hook_winner`] for a nonce the watermark shows consumed. It reads recorded
    /// observations only, so it holds without a current nonce observation.
    #[must_use]
    pub fn resolved_swap_hook(&self, nonce: U256) -> Option<B256> {
        let resolved = self.nonce_resolved(nonce);
        self.swap_hook_at(nonce, resolved, resolved)
    }

    /// `pre_hook` and `post_hook` say whether an executed pre-hook and a post-hook with
    /// evidence may be named at `nonce`.
    fn swap_hook_at(&self, nonce: U256, pre_hook: bool, post_hook: bool) -> Option<B256> {
        let swap = self.swap.as_ref()?;
        if pre_hook
            && let Some(order) = swap.orders.iter().find(|order| {
                order.pre_hook.nonce == nonce && order.observations.pre_hook_executed.is_some()
            })
        {
            return Some(order.pre_hook.payload);
        }
        if let Some(post_hook) = swap.orders.iter().find_map(|order| {
            order.post_hook.filter(|hook| {
                post_hook && hook.nonce == nonce && order.observations.post_hook_evidence()
            })
        }) {
            return Some(post_hook.payload);
        }
        if !swap.orders.iter().any(|order| {
            order.pre_hook.nonce == nonce
                && order
                    .observations
                    .pre_hook_dead
                    .is_some_and(|death| death.cause == SwapPreHookDeathCause::OlderPostHook)
        }) {
            return None;
        }
        let mut older = swap
            .orders
            .iter()
            .filter_map(|order| order.post_hook)
            .filter(|hook| hook.nonce == nonce);
        match (older.next(), older.next()) {
            (Some(hook), None) => Some(hook.payload),
            _ => None,
        }
    }
}

/// A Bridge order carries its own provider's terms and a surplus that provider supports. Across
/// terms name a recipient and a message hash exactly for a private delivery, which only Across
/// offers. Other orders carry none.
fn bridge_terms_fit(delivery: &SwapDelivery, bridge: Option<&BridgeOrderTerms>) -> bool {
    match (delivery, bridge) {
        (SwapDelivery::Bridge(delivery), Some(terms)) => {
            terms.provider() == delivery.provider
                && delivery.has_valid_surplus()
                && delivery.has_valid_private_delivery()
                && match terms {
                    BridgeOrderTerms::Across(terms) => {
                        terms.recipient.is_some() == delivery.is_private()
                            && terms.message_hash.is_some() == delivery.is_private()
                    }
                    BridgeOrderTerms::NearIntents(_) => true,
                }
        }
        (SwapDelivery::NonEvmBridge(_), Some(terms)) => {
            matches!(terms, BridgeOrderTerms::NearIntents(_))
        }
        (SwapDelivery::Bridge(_) | SwapDelivery::NonEvmBridge(_), None) => false,
        (SwapDelivery::Reshield | SwapDelivery::External { .. }, bridge) => bridge.is_none(),
    }
}

impl ExecutorStore {
    /// Refresh only mutable account state, retaining finalized swap and setup evidence, for
    /// an account settled in either role. A receipt credit does not identify a hook winner:
    /// require every old hook and shield nonce to be behind the freshly read nonce without
    /// inventing historical execution evidence. The nonce alone settles nothing: the record
    /// must already be settled at the observed block.
    pub(crate) fn refresh_settled_swap_nonce(
        &self,
        previous: &ExecutorRecord,
        observed: super::ExecutorNonceObservation,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(previous.operation, |record| {
            if record != previous
                || !record.settled_swaps_at(observed.block.number)
                || record
                    .nonce_observation
                    .is_some_and(|old| observed.nonce < old.nonce)
                || record
                    .issued
                    .iter()
                    .any(|payload| payload.nonce >= observed.nonce)
            {
                return Err(ExecutorStoreError::OutstandingNonce);
            }
            record.nonce_observation = Some(observed);
            record.set_nonce_watermark(observed.into());
            Ok(())
        })
    }

    /// Persist a swap attempt's order and hook payloads in one write, before the
    /// order request exposes them. The pre-hook holds the current nonce `k`. A
    /// Reshield or Across Bridge order's post-hook holds `k + 1`; External and NEAR
    /// Intents Bridge orders have none. No other payload may use a future nonce. A
    /// Bridge order carries its provider's terms. A retry is admitted only after every
    /// earlier attempt ended or completed delivery. The order belongs to the attempt's swap
    /// use, and is refused once another use claims the account. Unless it is a private Bridge
    /// order, it keeps the tokens and delivery of the use's earlier attempts.
    ///
    /// A speed-up is the exception to the retry rule: it is admitted while the use's latest
    /// order is still open, at that order's pre-hook nonce. It must invalidate that order,
    /// keep the recorded proof, the sell amount, the private spend and the delivery, and ask
    /// for a lower buy amount. Earlier orders and the use's setup approval stay as they are.
    ///
    /// The pre-hook never shares a note with a setup whose nonce is unresolved, which could
    /// spend it first.
    pub fn record_swap_attempt(
        &self,
        operation: ExecutorOperationId,
        attempt: SwapAttempt,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.record_swap_attempt_at(operation, attempt, None)
    }

    /// [`Self::record_swap_attempt`] for an order, or its speed-up, on the included setup of
    /// the account its use reserved fresh: a read at the latest block showed the delegation
    /// and the account nonce `included_nonce`, while the record leaves the setup's nonce
    /// unresolved. The setup must sit unresolved at the record's observed nonce `n`, the
    /// pre-hook holds `n + 1`, which must be `included_nonce`, and a post-hook `n + 2`. Only
    /// that use's setups may be unresolved below the pre-hook. The hooks still name the
    /// record's confirmed observation, and nothing from the latest read is recorded.
    pub fn record_swap_attempt_on_included_setup(
        &self,
        operation: ExecutorOperationId,
        attempt: SwapAttempt,
        included_nonce: U256,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.record_swap_attempt_at(operation, attempt, Some(included_nonce))
    }

    fn record_swap_attempt_at(
        &self,
        operation: ExecutorOperationId,
        attempt: SwapAttempt,
        included_nonce: Option<U256>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let SwapAttempt {
            use_id,
            terms,
            proof,
            uid,
            submission,
            delivery,
            bounds,
            invalidates,
            pre_hook,
            post_hook,
            bridge,
            speed_up,
        } = attempt;
        self.update(operation, |record| {
            if record.active_swap_use != Some(use_id) {
                return Err(ExecutorStoreError::SwapUseActive);
            }
            // The order is an attempt of the account's active use, which the latest use is. Only
            // a use that reserved its account fresh has a setup to be included.
            let Some(SwapUseRecord {
                stopped: false,
                fresh,
                role:
                    SwapUseRole::Source {
                        destination_operation,
                        ..
                    },
                ..
            }) = record
                .swap_uses
                .last()
                .filter(|swap_use| swap_use.id == use_id)
            else {
                return Err(ExecutorStoreError::OperationMismatch);
            };
            if included_nonce.is_some() && !*fresh {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            let destination_operation = *destination_operation;
            // A private Bridge order pays the destination account this use names, which must
            // serve this use for the delivery's token. Read under the held record lock.
            if let SwapDelivery::Bridge(bridge) = delivery
                && bridge.is_private()
            {
                let destination = match destination_operation {
                    Some(destination) => self
                        .for_chain(bridge.destination_chain)
                        .record(destination)?,
                    None => None,
                };
                if !destination.is_some_and(|destination| {
                    destination.address == Some(bridge.receiver)
                        && destination.swap_use(use_id).is_some_and(|swap_use| {
                            matches!(
                                swap_use.role,
                                SwapUseRole::Destination {
                                    origin_chain,
                                    origin_operation,
                                    destination_token,
                                    ..
                                } if origin_chain == self.chain_id
                                    && origin_operation == operation
                                    && destination_token == bridge.destination_token
                            )
                        })
                }) {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
            }
            let orders = record
                .swap
                .as_ref()
                .map_or(&[][..], |swap| swap.orders.as_slice());
            if record.retired && record.swap.is_none()
                || record.swap_setup_stopped
                || record.address != Some(uid.owner())
                || record.public_account_uuid.is_some()
                || record
                    .swap
                    .as_ref()
                    .is_some_and(|swap| swap.terms.recipient != terms.recipient)
                || pre_hook.delegate != record.delegate
                || pre_hook.purpose != ExecutorPayloadPurpose::SwapPreHook
                || record
                    .issued
                    .iter()
                    .any(|issued| issued.hash == pre_hook.hash)
                || post_hook.is_some() != delivery.has_post_hook()
                || !bridge_terms_fit(&delivery, bridge.as_ref())
                || post_hook.as_ref().is_some_and(|post_hook| {
                    post_hook.delegate != record.delegate
                        || post_hook.purpose != ExecutorPayloadPurpose::SwapPostHook
                        || pre_hook.hash == post_hook.hash
                        || record
                            .issued
                            .iter()
                            .any(|issued| issued.hash == post_hook.hash)
                        || !post_hook.context.inputs.is_empty()
                })
                || proof.inputs.is_empty()
                || pre_hook.context.inputs != proof.inputs
                || orders.iter().any(|order| order.uid == uid.0)
                || invalidates.is_some_and(|old| orders.iter().all(|order| order.uid != old.0))
                || !record.issued.iter().any(|issued| {
                    issued.hash == terms.setup_payload
                        && issued.purpose == ExecutorPayloadPurpose::Operation
                })
            {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            if !record.swap_use_keeps_terms(use_id, terms.sell_token, terms.buy_token, &delivery) {
                return Err(ExecutorStoreError::SwapUseTermsChanged);
            }
            match speed_up {
                None => {
                    if !record
                        .swap
                        .as_ref()
                        .is_none_or(SwapOperationRecord::admits_attempt)
                    {
                        return Err(ExecutorStoreError::SwapAttemptOutstanding);
                    }
                }
                Some(open) => {
                    let binding = SwapSpeedUp {
                        swap_use: use_id,
                        order: open,
                        pre_hook_nonce: pre_hook.nonce,
                    };
                    // The rule leaves the open order as the latest one. Both orders fund from
                    // the same proof and sell the same amount, so only one of them can trade.
                    let admitted = record.swap.as_ref().is_some_and(|swap| {
                        swap.admits_speed_up(&binding)
                            && invalidates == Some(open)
                            && proof == swap.proof
                            && swap.orders.last().is_some_and(|latest| {
                                bounds.sell_amount == latest.bounds.sell_amount
                                    && bounds.spend_amount() == latest.bounds.spend_amount()
                                    && bounds.buy_amount < latest.bounds.buy_amount
                                    && delivery == latest.delivery
                            })
                    });
                    if !admitted {
                        return Err(ExecutorStoreError::SwapSpeedUpRefused);
                    }
                }
            }
            // This check shares the record mutation lock with every store handle.
            if self.records()?.iter().any(|other| {
                other.operation != operation
                    && other
                        .reserved_inputs()
                        .iter()
                        .any(|input| proof.inputs.contains(input))
            }) {
                return Err(ExecutorStoreError::InputReserved);
            }
            // A setup whose nonce is unresolved can still run before the pre-hook and spend
            // these notes as its fee, whichever attempt of it runs.
            if record.issued.iter().any(|issued| {
                issued.purpose == ExecutorPayloadPurpose::Operation
                    && !record.nonce_resolved(issued.nonce)
                    && issued
                        .context
                        .inputs
                        .iter()
                        .any(|input| proof.inputs.contains(input))
            }) {
                return Err(ExecutorStoreError::InputReserved);
            }
            let observed = pre_hook.context.observed;
            let nonce = pre_hook.nonce;
            // Without included evidence the setup's nonce must be resolved, whichever setup
            // signed at it ran. Every nonce below the current one is resolved with it, which
            // makes the old signatures of earlier hooks and shields unusable. On an included
            // setup the pre-hook takes the nonce after the setup's, and only the setup's
            // attempts may be unresolved below it.
            let setup_admitted = match included_nonce {
                None => {
                    observed.nonce == nonce
                        && record.issued.iter().any(|issued| {
                            issued.hash == terms.setup_payload
                                && record.nonce_resolved(issued.nonce)
                        })
                }
                Some(included) => {
                    record.unresolved_swap_setup_nonce(use_id) == Some(observed.nonce)
                        && observed.nonce.checked_add(U256::ONE) == Some(nonce)
                        && included == nonce
                        && record
                            .issued
                            .iter()
                            .find(|issued| issued.hash == terms.setup_payload)
                            .is_some_and(|setup| setup.nonce == observed.nonce)
                }
            };
            let included_setup_below = |issued: &IssuedExecutorPayload| {
                included_nonce.is_some()
                    && issued.purpose == ExecutorPayloadPurpose::Operation
                    && issued.nonce == observed.nonce
            };
            if record.nonce_observation != Some(observed)
                || !setup_admitted
                || pre_hook.context.calldata.is_empty()
                || post_hook.as_ref().is_some_and(|post_hook| {
                    post_hook.context.observed != observed
                        || nonce.checked_add(U256::ONE) != Some(post_hook.nonce)
                        || post_hook.context.calldata.is_empty()
                })
                || record.issued.iter().any(|issued| {
                    issued.nonce < nonce
                        && !record.nonce_resolved(issued.nonce)
                        && !included_setup_below(issued)
                        && !(issued.purpose == ExecutorPayloadPurpose::SwapPostHook
                            && (record.swap_post_hook_took_nonce(issued.nonce)
                                || record.swap.as_ref().is_some_and(|swap| {
                                    swap.orders.iter().any(|order| {
                                        order
                                            .post_hook
                                            .is_some_and(|hook| hook.payload == issued.hash)
                                            && swap.order_settled_at(order, observed.block.number)
                                    })
                                })))
                        && !(issued.purpose == ExecutorPayloadPurpose::SwapDestinationShield
                            && record.swap_shield_delivered(issued.hash))
                        && !orders
                            .iter()
                            .any(|order| order.pre_hook.payload == issued.hash)
                })
            {
                return Err(ExecutorStoreError::OutstandingNonce);
            }
            // A Reshield order pays the bought token to the executor. A Bridge order's bought
            // token can end up there too: a skipped Across post-hook leaves it, and a bridge
            // refunds to the executor. An External order pays its receiver. Earlier attempts'
            // assets stay. The wallet's native marker is not an ERC-20 asset.
            let bought = (matches!(delivery, SwapDelivery::Reshield) || delivery.is_bridge())
                .then_some(terms.buy_token);
            let order = SwapOrderRecord {
                terms: Some(terms),
                attempt: orders.last().map_or(0, |order| order.attempt + 1),
                uid: uid.0,
                delivery,
                bounds,
                pre_hook: SwapHookPayload::of(&pre_hook),
                post_hook: post_hook.as_ref().map(SwapHookPayload::of),
                invalidates: invalidates.map(|old| old.0),
                observations: SwapOrderObservations::default(),
                submission,
                submission_status: SwapSubmissionStatus::Pending,
                bridge,
                use_id: Some(use_id),
            };
            if let Some(swap) = &mut record.swap {
                swap.proof = proof;
                swap.orders.push(order);
            } else {
                record.swap = Some(SwapOperationRecord {
                    terms,
                    proof,
                    orders: vec![order],
                    fundings: Vec::new(),
                });
            }
            record.issued.push(pre_hook);
            record.issued.extend(post_hook);
            for token in std::iter::once(terms.sell_token).chain(bought) {
                if token == Address::ZERO {
                    continue;
                }
                let asset = super::ExecutorAsset::Erc20(token);
                if !record.assets.contains(&asset) {
                    record.assets.push(asset);
                }
            }
            record.hidden = false;
            Ok(())
        })
    }

    /// Save acceptance independently of execution. A late failed request cannot undo
    /// a successful submission of the same immutable order. Sending an order reserves
    /// its hooks' inputs again, even after the user released them.
    pub fn record_swap_submission(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        status: SwapSubmissionStatus,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if order.submission_status != SwapSubmissionStatus::Accepted {
                order.submission_status = status;
            }
            let hooks = [
                Some(order.pre_hook.payload),
                order.post_hook.map(|hook| hook.payload),
            ];
            if matches!(
                status,
                SwapSubmissionStatus::Pending | SwapSubmissionStatus::Accepted
            ) {
                record
                    .released_payloads
                    .retain(|hash| !hooks.contains(&Some(*hash)));
            }
            Ok(())
        })
    }

    /// Persist the terms approved with a swap's setup, replacing an earlier approval. Only the
    /// expected latest use takes one, as an active source use without orders that was not
    /// stopped; an order records its own bounds.
    pub fn record_swap_approval(
        &self,
        operation: ExecutorOperationId,
        expected_use: SwapUseId,
        approval: SwapApproval,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            if record.address.is_none() || record.swap_setup_stopped {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            if record.active_swap_use != Some(expected_use) {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            let has_orders = record
                .swap_uses
                .last()
                .is_some_and(|swap_use| record.has_swap_use_order(swap_use.id));
            // A setup sent again beside an order placed on it while it was included may raise
            // its own fee limit. Nothing else the order was approved with changes.
            let setup_fee_only = has_orders
                && record.unresolved_swap_setup_nonce(expected_use).is_some()
                && record
                    .swap_uses
                    .last()
                    .and_then(SwapUseRecord::approval)
                    .is_some_and(|known| {
                        let mut unchanged = approval.clone();
                        unchanged.bounds.source_setup_fee = known.bounds.source_setup_fee;
                        unchanged == *known
                    });
            match record.swap_uses.last_mut() {
                Some(SwapUseRecord {
                    id,
                    stopped: false,
                    role:
                        SwapUseRole::Source {
                            approval: known,
                            destination_operation,
                        },
                    ..
                }) if *id == expected_use && (!has_orders || setup_fee_only) => {
                    // Cancellation resolves the linked destination through this delivery's chain.
                    if destination_operation.is_some()
                        && !matches!(
                            (known.as_deref().map(|known| &known.delivery), &approval.delivery),
                            (Some(SwapDelivery::Bridge(previous)), SwapDelivery::Bridge(replacement))
                                if previous.is_private()
                                    && replacement.is_private()
                                    && previous.destination_chain == replacement.destination_chain
                        )
                    {
                        return Err(ExecutorStoreError::OperationMismatch);
                    }
                    *known = Some(Box::new(approval));
                }
                _ => return Err(ExecutorStoreError::OperationMismatch),
            }
            Ok(())
        })
    }

    /// Replace an order's canonical observations, including when a reorg removes
    /// one. Record pre-hook death only at finalized depth, since it admits a retry.
    /// Every observation is at or below the record's reconciled confirmed block, and trade
    /// amounts come only with their trade.
    pub fn record_swap_observations(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        observations: SwapOrderObservations,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let confirmed = record
                .nonce_observation
                .ok_or(ExecutorStoreError::OutstandingNonce)?
                .block;
            if observations
                .blocks()
                .any(|block| block.number > confirmed.number)
                || observations.trade_amounts.is_some() && observations.traded.is_none()
            {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            order.observations = observations;
            Ok(())
        })
    }

    /// Replace the swap's verified fundings, including when a reorg removes one. Each is at or
    /// below the record's reconciled confirmed block, names the transaction that ran the
    /// proof, and belongs to a recorded order's swap use and pre-hook nonce, one per pair.
    pub fn record_swap_fundings(
        &self,
        operation: ExecutorOperationId,
        fundings: Vec<SwapFunding>,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let confirmed = record
                .nonce_observation
                .ok_or(ExecutorStoreError::OutstandingNonce)?
                .block;
            let same = |one: &SwapFunding, other: &SwapFunding| {
                one.use_id == other.use_id && one.pre_hook_nonce == other.pre_hook_nonce
            };
            if fundings.iter().enumerate().any(|(index, funding)| {
                funding.observation.block.number > confirmed.number
                    || funding.observation.transaction_hash.is_none()
                    || fundings[..index]
                        .iter()
                        .any(|earlier| same(earlier, funding))
            }) {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            let swap = record
                .swap
                .as_mut()
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if !fundings.iter().all(|funding| {
                swap.orders.iter().any(|order| {
                    order.use_id == Some(funding.use_id)
                        && order.pre_hook.nonce == funding.pre_hook_nonce
                })
            }) {
                return Err(ExecutorStoreError::OperationMismatch);
            }
            swap.fundings = fundings;
            Ok(())
        })
    }

    /// Persist receipt evidence checked at head minus finality depth by the owner.
    /// This deliberately leaves account nonce reconciliation and input reservations alone.
    /// A Reshield order is delivered only with a private credit. An External order is
    /// delivered by its trade and never carries a credit. A Bridge order is delivered on this
    /// chain by its hand-off, in the trade's own transaction: for NEAR Intents the trade, for
    /// Across the deposit that names its id, optionally with the surplus credit of a post-hook
    /// that reshields it. Same-chain orders take no hand-off.
    ///
    /// `uid` is the traded order. Usually it is also the evidence's delivery order and takes
    /// all of it. A public-receiver Across deposit may instead match another attempt of the
    /// same swap use. The traded order then keeps the trade and its amounts, and that attempt
    /// takes the hand-off, `delivered` and any surplus credit, in the same write.
    pub(crate) fn record_swap_settlement(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        evidence: &SwapSettlementEvidence,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        let SwapSettlementEvidence {
            traded,
            amounts,
            credit,
            handoff,
            delivery,
        } = *evidence;
        self.update(operation, |record| {
            if delivery != uid {
                return record_split_settlement(record.swap.as_mut(), uid, evidence);
            }
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            let (credit_fits, handoff_fits, delivered) = match order.delivery {
                SwapDelivery::Reshield => (
                    credit
                        .is_none_or(|credit| credit.private_amount >= order.bounds.private_minimum),
                    handoff.is_none(),
                    credit.is_some(),
                ),
                SwapDelivery::External { .. } => (credit.is_none(), handoff.is_none(), true),
                SwapDelivery::Bridge(bridge) => {
                    let handoff_fits = handoff.is_none_or(|handoff| {
                        handoff.observation == traded
                            && handoff.deposit_id.is_some()
                                == (bridge.provider == BridgeProvider::Across)
                    });
                    // Only an Across post-hook that reshields surplus shields anything, and
                    // only after its deposit. The surplus has no minimum.
                    let credit_fits = credit.is_none()
                        || bridge.surplus == BridgeSurplus::Reshield
                            && bridge.provider == BridgeProvider::Across
                            && handoff.is_some();
                    (credit_fits, handoff_fits, handoff.is_some())
                }
                // Like a NEAR Intents Bridge order: the trade pays the deposit address.
                SwapDelivery::NonEvmBridge(_) => (
                    credit.is_none(),
                    handoff.is_none_or(|handoff| {
                        handoff.observation == traded && handoff.deposit_id.is_none()
                    }),
                    handoff.is_some(),
                ),
            };
            if traded.transaction_hash.is_none()
                || !credit_fits
                || !handoff_fits
                || credit.is_some_and(|credit| credit.observation != traded)
                || order
                    .observations
                    .traded
                    .is_some_and(|known| known != traded)
                || handoff.is_some_and(|handoff| {
                    order
                        .observations
                        .bridge_handoff
                        .is_some_and(|known| known != handoff)
                })
            {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            let amounts = order.observations.kept_trade_amounts(amounts);
            order.observations.traded = Some(traded);
            order.observations.trade_amounts = Some(amounts);
            if let Some(credit) = credit {
                order.observations.settlement_credit = Some(credit);
            }
            if let Some(handoff) = handoff {
                order.observations.bridge_handoff = Some(handoff);
            }
            if delivered {
                order.observations.delivered = Some(traded);
                order.observations.undelivered = None;
            }
            Ok(())
        })
    }

    /// Persist the orderbook's executed fee with the amounts of an order's recorded trade. The
    /// fee is the orderbook's report, not chain evidence. A recorded fee is never replaced,
    /// though recording one again is accepted.
    pub(crate) fn record_swap_executed_fee(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        fee: U256,
        token: Address,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            if order.observations.traded.is_none() {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            let amounts = order
                .observations
                .trade_amounts
                .as_mut()
                .ok_or(ExecutorStoreError::InvalidRecord)?;
            if amounts.executed_fee.is_none() {
                amounts.executed_fee = Some(fee);
                amounts.executed_fee_token = Some(token);
            }
            Ok(())
        })
    }

    /// Persist a Bridge order's destination outcome after its hand-off. A final outcome is
    /// never replaced, though recording it again is accepted; `NeedsAttention` may be
    /// replaced by any outcome. The exceptions are an Across `Refunding` without a verified
    /// refund, which a verified fill replaces, and an Across `DeliveredReported`, which a
    /// verified fill or a refund replaces. A verified delivery must meet the approved
    /// destination minimum. A private delivery's fill is recorded as a shielded delivery or as
    /// held on the destination chain, and no other delivery takes either.
    pub fn record_swap_bridge_outcome(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        outcome: SwapBridgeOutcome,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            let private = match &order.delivery {
                SwapDelivery::Bridge(delivery) => delivery.is_private(),
                SwapDelivery::NonEvmBridge(_) => false,
                SwapDelivery::Reshield | SwapDelivery::External { .. } => {
                    return Err(ExecutorStoreError::OperationMismatch);
                }
            };
            let rules = BridgeOutcomeRules {
                handed_off: order.observations.bridge_handoff.is_some(),
                private,
                destination_minimum: order.bounds.destination_minimum,
                across: matches!(order.bridge, Some(BridgeOrderTerms::Across(_))),
                known: order.observations.bridge_outcome.clone(),
                refund_verified: order.observations.bridge_refund.is_some(),
            };
            if !rules.admits(&outcome) {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            order.observations.bridge_outcome = Some(outcome);
            Ok(())
        })
    }

    /// Persist Across's refund of a refunding order's deposit, in a finalized block after the
    /// hand-off. A recorded refund is never replaced, though recording it again is accepted.
    pub(crate) fn record_swap_bridge_refund(
        &self,
        operation: ExecutorOperationId,
        uid: OrderUid,
        refund: SwapObservation,
    ) -> Result<ExecutorRecord, ExecutorStoreError> {
        self.update(operation, |record| {
            let order = record
                .swap
                .as_mut()
                .and_then(|swap| swap.orders.iter_mut().find(|order| order.uid == uid.0))
                .ok_or(ExecutorStoreError::OperationMismatch)?;
            let observed = &order.observations;
            if !bridge_refund_admitted(
                matches!(order.bridge, Some(BridgeOrderTerms::Across(_))),
                observed.bridge_outcome.as_ref(),
                observed.bridge_handoff,
                observed.bridge_refund,
                refund,
            ) {
                return Err(ExecutorStoreError::InvalidRecord);
            }
            order.observations.bridge_refund = Some(refund);
            Ok(())
        })
    }
}

/// Receipt evidence of one settlement, checked by the owner at head minus finality depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SwapSettlementEvidence {
    pub traded: SwapObservation,
    pub amounts: SwapTradeAmounts,
    pub credit: Option<SwapShieldObservation>,
    pub handoff: Option<SwapBridgeHandoff>,
    /// The attempt whose approved terms the hand-off and credit match: the traded order, or
    /// for a public-receiver Across deposit another attempt of the same use.
    pub delivery: OrderUid,
}

/// Record a settlement whose Across deposit matches another attempt than the traded order
/// `uid`. Both are public-receiver Across attempts of one swap use, and the hand-off is in the
/// trade's own transaction. The delivery order takes no trade, and the traded order no
/// hand-off, `delivered` or credit.
fn record_split_settlement(
    swap: Option<&mut SwapOperationRecord>,
    uid: OrderUid,
    evidence: &SwapSettlementEvidence,
) -> Result<(), ExecutorStoreError> {
    let SwapSettlementEvidence {
        traded,
        amounts,
        credit,
        handoff,
        delivery,
    } = *evidence;
    let orders = &mut swap.ok_or(ExecutorStoreError::OperationMismatch)?.orders;
    let position = |uid: OrderUid| orders.iter().position(|order| order.uid == uid.0);
    let (Some(trade_index), Some(delivery_index)) = (position(uid), position(delivery)) else {
        return Err(ExecutorStoreError::OperationMismatch);
    };
    let (trade_order, delivery_order) = (&orders[trade_index], &orders[delivery_index]);
    let public_across = |order: &SwapOrderRecord| {
        matches!(
            order.delivery,
            SwapDelivery::Bridge(bridge)
                if bridge.provider == BridgeProvider::Across && !bridge.is_private()
        ) && matches!(order.bridge, Some(BridgeOrderTerms::Across(_)))
    };
    // A split without a hand-off records nothing the traded order could not hold itself.
    let Some(handoff) = handoff else {
        return Err(ExecutorStoreError::InvalidRecord);
    };
    let credit_fits = credit.is_none_or(|credit| {
        credit.observation == traded
            && matches!(
                delivery_order.delivery,
                SwapDelivery::Bridge(bridge) if bridge.surplus == BridgeSurplus::Reshield
            )
    });
    if trade_order.use_id.is_none()
        || trade_order.use_id != delivery_order.use_id
        || !public_across(trade_order)
        || !public_across(delivery_order)
        || traded.transaction_hash.is_none()
        || handoff.deposit_id.is_none()
        || handoff.observation != traded
        || !credit_fits
        || trade_order
            .observations
            .traded
            .is_some_and(|known| known != traded)
        || trade_order.observations.bridge_handoff.is_some()
        || trade_order.observations.delivered.is_some()
        || delivery_order.observations.traded.is_some()
        || delivery_order
            .observations
            .bridge_handoff
            .is_some_and(|known| known != handoff)
    {
        return Err(ExecutorStoreError::InvalidRecord);
    }
    let observed = &mut orders[trade_index].observations;
    observed.trade_amounts = Some(observed.kept_trade_amounts(amounts));
    observed.traded = Some(traded);
    observed.undelivered = None;
    let observed = &mut orders[delivery_index].observations;
    observed.bridge_handoff = Some(handoff);
    observed.delivered = Some(traded);
    if let Some(credit) = credit {
        observed.settlement_credit = Some(credit);
    }
    observed.undelivered = None;
    Ok(())
}
