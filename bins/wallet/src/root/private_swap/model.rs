//! Presentation of private swaps, derived from their encrypted executor records.
//!
//! Every stage comes from the durable record plus, at most, this session's last setup
//! observation, so the Private tab card and the Stealth accounts status survive a restart. The
//! orderbook's order status never decides a stage; it only adds a [`SwapFillHint`] to the copy.

use std::time::Duration;

use alloy::primitives::{Address, B256, U256, U512};
use wallet_ops::{
    ExecutorAttribution, SwapOrderState, SwapSetupStatus, SwapSpeedUpRefusal,
    cow::OrderUid,
    swap_attempt_state,
    vault::{
        BridgeDelivery, BridgeOrderTerms, BridgeProvider, ExecutorOperationId,
        ExecutorPayloadPurpose, ExecutorPayloadState, ExecutorRecord, SwapApprovedBounds,
        SwapDelivery, SwapOrderObservations, SwapOrderRecord, SwapPreHookDeathCause,
        SwapSubmissionStatus, SwapTradeAmounts, SwapUseId, SwapUseRecord, SwapUseRole,
    },
};

use crate::root::public_action::PublicActionStepStatus;

mod card_lines;
mod labels;
mod outcome_actions;
mod public_swaps;
mod record_queries;
mod stages;
mod steps;

pub(in crate::root) use card_lines::*;
pub(in crate::root) use labels::*;
pub(in crate::root) use outcome_actions::*;
pub(super) use public_swaps::*;
pub(in crate::root) use record_queries::*;
pub(in crate::root) use stages::*;
pub(in crate::root) use steps::*;

/// Where one swap stands for presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum SwapStage {
    /// The stealth account is reserved but no setup was sent.
    SetupNotSent,
    /// The account was retired before an order was placed and cannot be set up again.
    SetupRetired,
    /// The setup is being proved and handed to a broadcaster in this session.
    SetupSubmitting,
    /// A setup was sent and its delegation isn't confirmed yet.
    SetupPending,
    /// Every setup reverted, lost its nonce, or left the account undelegated.
    SetupFailed,
    /// The stealth account is set up and no order was placed yet. No terms were approved with
    /// the setup, so the order is quoted and reviewed now.
    Ready,
    /// The stealth account is set up and its order, approved with the setup, isn't placed yet.
    /// The terms are checked again before the order is signed.
    Approved,
    /// A durable order whose orderbook acceptance is unknown.
    SubmissionPending,
    /// The orderbook rejected the durable attempt; reservations await canonical expiry.
    SubmissionRejected,
    /// The canonical state of the order the swap follows, as [`swap_outcome_order`] picks it.
    Order(SwapOrderState),
    /// Recovery confirmed after the swap left funds in its stealth account.
    Recovered,
}

/// What the swap form does for a swap at `stage`. `None` is a new swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum SwapFormMode {
    /// Quote before any setup is paid, then review the swap together with its setup. With
    /// `resume`, the setup is sent again for the stealth account already reserved.
    Setup { resume: bool },
    /// The setup is on its way; the approved order is placed once it's confirmed.
    SettingUp,
    /// The stealth account is set up: quote, review, and place the order without a setup.
    Order,
    /// An order of this attempt exists; its progress lives on the Private tab.
    Placed,
}

/// What private sync and the record show of a setup that this session has not confirmed:
/// a progress hint, not permission to place its order. Its delegation still needs an
/// account read.
pub(super) enum SwapSetupConfirmation {
    /// Private sync shows a note the setup spends or creates in a transaction recorded for
    /// it, in this block, while its nonce is not read as consumed.
    Included(u64),
    /// A setup's nonce is recorded as consumed.
    Executed,
}

/// What ties attempts to one funding: the swap use an order was signed for and its pre-hook
/// nonce. Attempts with the same key compete: they share one proof and one exact sell
/// allowance, so at most one of them trades, whichever was signed first.
pub(in crate::root) type SwapFundingKey = (SwapUseId, U256);

/// One order as [`swap_order_ranges`] reads it: its sell token, buy token and delivery,
/// whether it traded, and its funding key.
pub(in crate::root) type SwapRangeOrder =
    (Address, Address, SwapDelivery, bool, Option<SwapFundingKey>);

/// One swap: the stealth account that places its orders, and the swap use on that account. The
/// account is what derives the address and recovers funds; the use picks the swap among the
/// account's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::root) struct SwapIdentity {
    pub(in crate::root) operation: ExecutorOperationId,
    pub(in crate::root) swap_use: SwapUseId,
}

/// Presentation of a Public-paid swap, derived from its destination use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PublicSwapStage {
    Preparing(SwapStage),
    Approving,
    ApprovalFailed,
    OrderOpen,
    SubmissionPending,
    SubmissionRejected,
    Depositing,
    DepositFailed,
    Bridging,
    DeliveredVerified,
    DeliveredReported,
    Refunding,
    Refunded,
    HeldByProxy,
    Withdrawing,
    WithdrawnPendingCheck,
    SwappedNotBridged,
    HeldOnDestination,
    Recovered,
    Cancelled,
    Expired,
    PreparationCancelled,
    NeedsAttention,
}

/// Values on the origin and destination networks, formatted independently.
pub(super) struct PublicSwapLabels {
    pub(super) source: String,
    pub(super) origin: String,
    pub(super) destination: String,
    pub(super) sell: String,
    pub(super) sell_symbol: String,
    pub(super) buy_symbol: String,
    /// The token the bridge takes on the origin network, which a refund and the proxy hold.
    pub(super) bridged_symbol: String,
    pub(super) traded: Option<String>,
    pub(super) deposited: Option<String>,
    pub(super) received: Option<String>,
    pub(super) held: Option<String>,
    /// The wallet knows the held token, so `held` is a formatted amount a card title can show.
    pub(super) held_named: bool,
    pub(super) minimum: String,
    /// "#57 · 0x9a3D…41e0", with " · reused" for an account the swap didn't set up.
    pub(super) account: String,
    /// The block that showed the destination account's setup nonce consumed, which the
    /// steps don't show. `None` for a reused account and while the setup is unresolved.
    pub(super) setup_block: Option<u64>,
}

/// One swap of a record that has orders: its swap use and its orders within the record's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct RecordSwap {
    pub(in crate::root) swap_use: SwapUseId,
    pub(in crate::root) orders: std::ops::Range<usize>,
}

/// The attempts of one swap and those that decide its outcome. A retry can hold the trade
/// while an earlier attempt of the same swap use, whose signed terms the settlement's deposit
/// matched, holds the bridge hand-off and everything that follows it. Of two orders that
/// compete for one funding, either can hold the trade.
#[derive(Clone, Copy, Debug)]
pub(in crate::root) struct SwapAttempts<'a> {
    /// Every attempt of the swap, oldest first.
    pub(in crate::root) attempts: &'a [SwapOrderRecord],
    /// The attempt the swap follows, as [`swap_outcome_order`] picks it: the traded one, or
    /// while nothing traded, the one whose submission and open-order state the swap shows.
    /// That is an accepted attempt that can still fill when the latest one was rejected.
    pub(in crate::root) outcome: &'a SwapOrderRecord,
    /// The attempt whose trade is verified.
    pub(in crate::root) traded: Option<&'a SwapOrderRecord>,
    /// The attempt holding the delivery evidence and the terms the delivery follows.
    pub(in crate::root) delivery: Option<&'a SwapOrderRecord>,
}

/// Display strings the card and progress copy need for one swap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::root) struct SwapLabels {
    /// "1.00 WETH for USDC".
    pub(in crate::root) pair: String,
    /// "1.00 WETH", or the sell symbol when no amount is recorded.
    pub(in crate::root) sell: String,
    pub(in crate::root) buy_symbol: String,
    /// Local expiry time of the latest order, such as "14:32".
    pub(in crate::root) expires: Option<String>,
    /// The latest order's validity has passed by the local clock, but no observation has
    /// recorded its end yet.
    pub(in crate::root) lapsed: bool,
    /// The orderbook reports the open order filled; canonical observation doesn't show it yet.
    pub(in crate::root) fill_hint: Option<SwapFillHint>,
    /// "49.28 DAI", the amount the post-hook credited privately once the swap is done, or for
    /// a Public address swap, the amount its receiver got. A Bridge swap's is the amount
    /// delivered on its destination network, verified or reported.
    pub(in crate::root) received: Option<String>,
    /// A Public address swap's receiver: its Public account or address-book label, or its
    /// short address. `None` for a swap back to the private balance or to another network.
    pub(in crate::root) receiver: Option<String>,
    /// "0.3787 ETH", the least the receiver gets while the order can fill.
    pub(in crate::root) minimum: Option<String>,
    /// "$4.42", the latest order's gas estimate beyond the gas it allowed, which no solver
    /// covered if it expired unfilled. `None` without a recorded gas share or when the share
    /// covered all of it.
    pub(in crate::root) uncovered_gas: Option<String>,
    /// The latest order's `CoW` order ID, which the order step shows.
    pub(in crate::root) order: Option<OrderUid>,
    /// A swap back to the private balance: the transaction of its post-hook shield, once
    /// observation verified the delivery. `None` for any other delivery.
    pub(in crate::root) shield_transaction: Option<B256>,
    /// The settlement transaction of the traded order, once observation recorded the trade.
    pub(in crate::root) settlement_transaction: Option<B256>,
    /// A Bridge swap's destination. `None` for delivery on the swap's own network.
    pub(in crate::root) bridge: Option<SwapBridgeLabels>,
}

/// Display strings for a Bridge swap's destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapBridgeLabels {
    pub(in crate::root) provider: BridgeProvider,
    /// The destination network, such as "Polygon".
    pub(in crate::root) network: String,
    /// The token the receiver gets there, such as "POL". The swap is named after it.
    pub(in crate::root) token: String,
    /// The destination is a non-EVM network, which the swap's name gives after `token`.
    pub(in crate::root) non_evm: bool,
    /// The swap's own network, where a refund reaches the stealth account.
    pub(in crate::root) origin: String,
    /// The receiver's Public account or address-book label, or its short address.
    pub(in crate::root) receiver: String,
    /// "248.82 USDC" of the bought token, once traded: what the settlement handed to the
    /// bridge, or what an Across post-hook that didn't run left in the stealth account.
    pub(in crate::root) sent: Option<String>,
    /// "248.71 USDC", the approved minimum on the destination network. For private delivery,
    /// what the private balance gets after that network's shield fee.
    pub(in crate::root) minimum: Option<String>,
    /// Set when the swap delivers to the wallet's private balance on the destination network.
    /// `receiver` is then the destination stealth account, which no copy names as a receiver.
    pub(in crate::root) private: Option<SwapPrivateBridgeLabels>,
    /// The recorded delivery is the provider's report, which the wallet didn't verify.
    pub(in crate::root) reported: bool,
    /// The transaction of the recorded delivery on the destination network, when it is known.
    pub(in crate::root) delivered_transaction: Option<B256>,
    /// A non-EVM delivery's transaction there, as 1Click reports it. `None` for any other.
    pub(in crate::root) destination_transaction: Option<String>,
    /// The hand-off's transaction on the swap's network, once observation recorded it.
    pub(in crate::root) handoff_transaction: Option<B256>,
    /// The transaction of Across's refund on the swap's network, once a refund is verified.
    pub(in crate::root) refund_transaction: Option<B256>,
    /// The deposit in the reported trade block, while the swap's fill is reported and its
    /// settlement isn't verified, with "248.82 USDC" of the bought token. `Looking` otherwise.
    pub(in crate::root) deposit: SwapSeen,
    /// What the provider reports about the delivery while the swap's fill is reported and its
    /// settlement isn't verified. `None` without such a report, and once a hand-off is recorded.
    pub(in crate::root) report: Option<SwapBridgeReported>,
}

/// A bridge provider's answer about a deposit the wallet hasn't verified. It changes copy only:
/// the stage, the actions and the outcome follow what observation records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) enum SwapBridgeReported {
    /// Delivered, with "248.71 USDC" when the provider gives the amount. A private delivery's
    /// amount is before the destination network's shield fee, and `shield` tells what its fill
    /// block shows. A public delivery has no shield, and nothing reads the field. `transaction`
    /// is the provider's transaction on the destination network, when it gives one.
    Delivered {
        amount: Option<String>,
        shield: SwapShieldSeen,
        transaction: Option<B256>,
    },
    Pending,
    Refunded,
    Failed,
}

/// What the fill block of a reported private delivery shows on the destination network, before
/// the wallet verified the settlement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) enum SwapShieldSeen {
    /// The fill block wasn't read yet.
    Looking,
    /// The destination stealth account shielded the delivery, with "248.09 USDC", the private
    /// credit after the shield fee.
    Detected(String),
    /// The destination stealth account holds the delivery, "248.71 USDC", and its shield
    /// didn't run.
    Held(String),
}

/// Display strings for a private Bridge swap's two stealth accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapPrivateBridgeLabels {
    /// Each network's stealth account setup, the swap's own network first.
    pub(in crate::root) setups: [SwapSetupLabels; 2],
    /// "992.74 USDC", what the destination stealth account holds after a fill without its
    /// shield.
    pub(in crate::root) held: Option<String>,
}

/// One network's stealth account and how far its setup is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapSetupLabels {
    /// The account's network, such as "Arbitrum One".
    pub(in crate::root) network: String,
    /// `None` until the account's address is derived.
    pub(in crate::root) account: Option<SwapStepAccount>,
    pub(in crate::root) progress: SwapSetupProgress,
    /// The swap reuses the account, which was set up before it, so it sends no setup for it.
    pub(in crate::root) reused: bool,
    /// What a setup on its way waits for, when this session knows it.
    pub(in crate::root) detail: Option<String>,
}

/// The stealth account a setup sub-step names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapStepAccount {
    /// The account's number. `None` for a destination account whose network isn't loaded.
    pub(in crate::root) index: Option<u32>,
    pub(in crate::root) address: Address,
}

/// How far one stealth account's setup is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum SwapSetupProgress {
    NotSent,
    /// The account's network isn't loaded in this session, so its records can't be read.
    NetworkLoading,
    Submitting,
    /// Sent, and its delegation isn't confirmed yet.
    Pending,
    Done,
    Failed,
}

/// A part of a reported settlement, as the reported trade block shows it before finality depth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) enum SwapSeen {
    /// The block isn't known or wasn't read yet.
    Looking,
    /// The block holds it, with "49.28 DAI" where it gives an amount.
    Detected(Option<String>),
    /// The block was read and doesn't show it.
    NotFound,
}

/// The orderbook reports an open order filled before canonical observation records the trade.
/// It changes copy and hides cancellation; the stage, other actions, reservations and recovery
/// follow observation. Every step the settlement performs shows as confirming, with the count
/// and the bought amount on the trade, and each other step tells whether the reported trade
/// block shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapFillHint {
    /// Blocks from the reported trade block to the synced head, when both are known.
    pub(in crate::root) confirmations: Option<u64>,
    /// The chain's finality depth, after which observation reads the trade.
    pub(in crate::root) depth: u64,
    /// "49.40 DAI", the bought amount before any shield fee: the trade's in the reported block
    /// once that was read, the orderbook's before.
    pub(in crate::root) bought: Option<String>,
    /// The reported trade block was read and holds the trade.
    pub(in crate::root) detected: bool,
    /// A swap back to the private balance: its shield in the reported trade block, with the
    /// private credit after the shield fee. `Looking` for any other delivery.
    pub(in crate::root) shield: SwapSeen,
    /// The transaction of that shield, when the reported trade block shows it. `None`
    /// otherwise.
    pub(in crate::root) shield_transaction: Option<B256>,
    /// The swap's own network, such as "Ethereum", whose blocks the count is of.
    pub(in crate::root) origin: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapCardLine {
    pub(in crate::root) title: String,
    pub(in crate::root) detail: String,
    pub(in crate::root) attention: bool,
}

const NOTHING_UNSHIELDED: &str = "Nothing was unshielded. You can retry with a new quote.";

/// What the Private tab card shows of the swaps paid from Public accounts: one swap's own title
/// and subtitle, or how many are incoming.
pub(super) struct PublicCardLine {
    pub(super) title: String,
    pub(super) detail: String,
    /// How many swaps the line stands for.
    pub(super) count: usize,
    /// How many of them ask the user to act.
    pub(super) attention: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapStep {
    pub(in crate::root) label: String,
    pub(in crate::root) detail: String,
    pub(in crate::root) status: PublicActionStepStatus,
    /// Sub-steps shown indented under the step, each with its own status. Only a private
    /// Bridge swap's setup has them, one per network.
    pub(in crate::root) children: Vec<Self>,
    /// A sub-step's stealth account. `None` for every other step.
    pub(in crate::root) account: Option<SwapStepAccount>,
    /// The `CoW` order ID of the order step, once the swap has an order.
    pub(in crate::root) order: Option<OrderUid>,
    /// The transaction that completed the step, or a Public-paid swap's own transaction the
    /// step waits on, once the wallet knows its hash: such as the settlement, a Bridge swap's
    /// hand-off or its fill on the destination network, or the shield of a swap back to the
    /// private balance, once the reported trade block or verified delivery shows it.
    pub(in crate::root) transaction: Option<B256>,
    /// A non-EVM delivery's transaction on its network, as 1Click reports it, which no EVM
    /// hash can hold. Only that delivery's step carries one.
    pub(in crate::root) destination_transaction: Option<String>,
    /// The block the step completed in, shown at its trailing edge. Only a Public-paid swap's
    /// steps carry one.
    pub(in crate::root) block: Option<u64>,
}

const TRADED: &str = "Traded";
/// What a reused stealth account's sub-step shows in place of a setup's block.
const REUSED_READY: &str = "Reused · ready";
const BACK_IN_PRIVATE_BALANCE: &str = "Back in private balance";
const BRIDGE_DEPOSIT: &str = "Bridge deposit";

/// A traded order's outcome, from its approved bounds and canonical observations. Amounts are
/// base units of the sell token for `spent`, `unshield_fee` and `limit_sell`, and of the buy
/// token otherwise, except `gas`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapOutcome {
    /// The private spend, and Railgun's unshield fee taken from it.
    pub(in crate::root) spent: U256,
    pub(in crate::root) unshield_fee: U256,
    /// The order's `sellAmount` and `buyAmount`: its limit price and minimum.
    pub(in crate::root) limit_sell: U256,
    pub(in crate::root) minimum: U256,
    /// Executed amounts from the settlement's Trade event. `None` in records from before they
    /// were kept.
    pub(in crate::root) trade: Option<SwapTradeAmounts>,
    /// The approved least the receiver gets: after the shield fee for Private delivery.
    pub(in crate::root) private_minimum: U256,
    /// What was delivered: the private credit for Private delivery, or the trade's buy amount
    /// for a Public address. `None` until it's recorded, and for Bridge delivery.
    pub(in crate::root) received: Option<U256>,
    /// `received` above `private_minimum`. `None` when it isn't above, or isn't recorded.
    pub(in crate::root) above_minimum: Option<U256>,
    /// The amount the post-hook's shield credited privately, and the shield fee its event
    /// charged. The fee is `None` when it wasn't recorded.
    pub(in crate::root) received_privately: Option<(U256, Option<U256>)>,
    /// The fee the orderbook charged the order, beside the settlement's gas cost. `None`
    /// unless all of its figures are recorded.
    pub(in crate::root) gas: Option<SwapOutcomeGas>,
    pub(in crate::root) settlement: Option<B256>,
}

/// The fee a filled order was charged, and the settlement transaction's own gas cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapOutcomeGas {
    /// The orderbook's executed fee, in `fee_token` base units: everything the order was
    /// charged, network and protocol fees together.
    pub(in crate::root) fee: U256,
    pub(in crate::root) fee_token: Address,
    /// The settlement's gas used times its effective gas price, in wei of the native token.
    /// The settlement batches every trade it settles, so this is its whole cost.
    pub(in crate::root) settlement_cost: U256,
}

/// An action the progress dialog offers. `Err` carries the reason it's disabled.
pub(in crate::root) type SwapActionAvailability = Option<Result<(), &'static str>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::root) struct SwapActions {
    pub(in crate::root) cancel: SwapActionAvailability,
    /// Sign a second order with a lower minimum while the open one stays live. Set by
    /// [`Self::with_speed_up`]; `swap_actions` offers none.
    pub(in crate::root) speed_up: SwapActionAvailability,
    pub(in crate::root) retry: SwapActionAvailability,
    /// Set up again or continue to the review in the swap form, or place an approved order.
    pub(in crate::root) resume: SwapActionAvailability,
    pub(in crate::root) recover: bool,
    /// Recover what a private Bridge swap's destination stealth account holds, on its network.
    pub(in crate::root) recover_on_destination: bool,
    pub(in crate::root) dismiss: bool,
}

/// Why Speed up is disabled while the swap's earlier order still counts as fillable. It names
/// the limit only: the latest order may expire before the earlier one stops counting.
const SPEED_UP_TWO_ORDERS: &str =
    "Two orders of this swap can still fill, and a new order can replace only one.";

/// How long after the latest order was signed the detail first shows Speed up. An early
/// speed-up signs a lower minimum that either order can fill at, and uses up the one speed-up
/// the two-order limit allows until an order drops out. The transaction Speed up actions have
/// no delay.
const SPEED_UP_AFTER: Duration = Duration::from_secs(30);

/// Where a swap belongs in My orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) enum SwapOrderGroup {
    /// The swap can still set up, place, fill, or deliver its order.
    Open,
    /// Funds sit in the stealth account and no order can deliver them any more, or in a
    /// private Bridge swap's destination stealth account after a fill without its shield.
    NeedsRecovery,
    /// A bridge provider has to resolve the deposit. The wallet can't recover it.
    NeedsAttention,
    /// Nothing more happens to the swap by itself.
    Ended,
}

#[cfg(test)]
pub(super) mod tests;
