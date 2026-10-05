//! Presentation of private swaps, derived from their encrypted executor records.
//!
//! Every stage comes from the durable record plus, at most, this session's last setup
//! observation, so the Private tab card and the Stealth accounts status survive a restart. The
//! orderbook's order status never decides a stage; it only adds a [`SwapFillHint`] to the copy.

use alloy::primitives::{Address, B256, U256, U512};
use wallet_ops::{
    SwapOrderState, SwapSetupStatus, swap_order_state,
    vault::{
        BridgeDelivery, BridgeOrderTerms, BridgeProvider, ExecutorExecutionResult,
        ExecutorOperationId, ExecutorPayloadInclusion, ExecutorPayloadPurpose,
        ExecutorPayloadStatus, ExecutorRecord, ExecutorRecoveryStepKind, SwapApprovedBounds,
        SwapDelivery, SwapOrderObservations, SwapOrderRecord, SwapPreHookDeathCause,
        SwapSubmissionStatus, SwapTradeAmounts, SwapUseId, SwapUseRecord, SwapUseRole,
    },
};

use crate::root::public_action::PublicActionStepStatus;

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
    /// The latest order's canonical state.
    Order(SwapOrderState),
    /// Recovery confirmed after the swap left funds in its stealth account.
    Recovered,
}

impl SwapStage {
    /// The swap can't move on by itself: nothing is pending on chain, and no order of it can
    /// still fill. Ended swaps leave the Private tab by themselves; the user may dismiss the rest.
    pub(in crate::root) const fn is_dismissible(self) -> bool {
        matches!(
            self,
            Self::SetupNotSent
                | Self::SetupRetired
                | Self::SetupFailed
                | Self::Ready
                | Self::Approved
                | Self::Recovered
                | Self::Order(
                    SwapOrderState::Done
                        | SwapOrderState::AttemptEnded(_)
                        | SwapOrderState::NotDelivered
                        | SwapOrderState::Refunding
                        | SwapOrderState::HeldOnDestination
                        | SwapOrderState::PreHookOnly { expired: true }
                )
        )
    }

    /// Nothing more happens to the swap by itself, and nothing is left to recover or continue.
    pub(in crate::root) const fn has_ended(self) -> bool {
        matches!(
            self,
            Self::SetupRetired
                | Self::Recovered
                | Self::Order(SwapOrderState::Done | SwapOrderState::AttemptEnded(_))
        )
    }

    /// Funds sit in the swap's own stealth account and no order can deliver them any more.
    /// Proceeds held by a private Bridge swap's destination account are recovered on that
    /// network instead, as [`Self::is_held_on_destination`] tells.
    pub(in crate::root) const fn needs_recovery(self) -> bool {
        matches!(
            self,
            Self::Order(
                SwapOrderState::NotDelivered
                    | SwapOrderState::Refunding
                    | SwapOrderState::PreHookOnly { expired: true }
            )
        )
    }

    /// A private Bridge swap's fill completed without its shield, so the destination stealth
    /// account holds the proceeds until they are recovered on that network.
    pub(in crate::root) const fn is_held_on_destination(self) -> bool {
        matches!(self, Self::Order(SwapOrderState::HeldOnDestination))
    }

    /// The swap asks the user to act: recover its funds, here or on the destination network, or
    /// have a bridge provider resolve its deposit.
    pub(in crate::root) const fn needs_attention(self) -> bool {
        self.needs_recovery()
            || self.is_held_on_destination()
            || matches!(self, Self::Order(SwapOrderState::NeedsAttention))
    }

    /// Canonical observation can still change this stage.
    pub(in crate::root) const fn is_observed(self) -> bool {
        matches!(
            self,
            Self::SetupPending
                | Self::SubmissionPending
                | Self::SubmissionRejected
                | Self::Order(
                    SwapOrderState::Open
                        | SwapOrderState::Traded
                        | SwapOrderState::PreHookOnly { expired: false }
                )
        )
    }

    /// Ended swaps stay in My orders only. Expired orders may be hidden while their outcome
    /// is unresolved; dismissal never changes their observations or reservations.
    pub(in crate::root) const fn is_shown_on_private_tab(
        self,
        dismissed: bool,
        past_valid_to: bool,
    ) -> bool {
        !self.has_ended() && (!dismissed || !swap_actions(self, past_valid_to).dismiss)
    }
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

pub(in crate::root) const fn swap_form_mode(stage: Option<SwapStage>) -> SwapFormMode {
    match stage {
        None => SwapFormMode::Setup { resume: false },
        Some(SwapStage::SetupNotSent | SwapStage::SetupFailed | SwapStage::SetupPending) => {
            SwapFormMode::Setup { resume: true }
        }
        Some(SwapStage::SetupSubmitting) => SwapFormMode::SettingUp,
        Some(
            SwapStage::Ready
            | SwapStage::Approved
            | SwapStage::Order(SwapOrderState::AttemptEnded(_)),
        ) => SwapFormMode::Order,
        Some(
            SwapStage::SetupRetired
            | SwapStage::Recovered
            | SwapStage::SubmissionPending
            | SwapStage::SubmissionRejected
            | SwapStage::Order(_),
        ) => SwapFormMode::Placed,
    }
}

/// Derive a swap's stage from its record. `setup` is this session's last setup observation;
/// without it the recorded setup outcome is used.
pub(in crate::root) fn swap_stage(
    record: &ExecutorRecord,
    setup: Option<SwapSetupStatus>,
    submitting: bool,
) -> SwapStage {
    if let Some(order) = record.swap().and_then(|swap| swap.orders().last()) {
        return swap_order_stage(record, order);
    }
    if record.is_retired() {
        return SwapStage::SetupRetired;
    }
    if submitting {
        return SwapStage::SetupSubmitting;
    }
    let stage = match setup {
        Some(SwapSetupStatus::Delegated(_)) => SwapStage::Ready,
        Some(SwapSetupStatus::Failed | SwapSetupStatus::MissingDelegation) => {
            SwapStage::SetupFailed
        }
        Some(SwapSetupStatus::Pending) | None => {
            let statuses = record
                .issued()
                .iter()
                .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
                .map(|payload| record.recorded_payload_status(payload.hash()))
                .collect::<Vec<_>>();
            recorded_setup_stage(&statuses, setup.is_some())
        }
    };
    // An approval binds a first order while its swap use claims the account. A reused account
    // that a cancelled preparation released keeps that approval only as history.
    let approved = record.swap_approval().is_some() && record.active_swap_use().is_some();
    if stage == SwapStage::Ready && approved {
        SwapStage::Approved
    } else {
        stage
    }
}

/// A setup's local inclusion is a progress hint, not permission to place its order.
/// Its effects and delegation still need canonical verification.
pub(super) enum SwapSetupConfirmation {
    Included(u64),
    Executed,
}

impl SwapSetupConfirmation {
    pub(super) fn detail(
        self,
        finality: super::super::utxo::UtxoFinalityContext,
    ) -> Option<String> {
        use super::super::utxo::BlockFinalityProgress;
        Some(match self {
            Self::Executed => "Verifying setup…".into(),
            Self::Included(block) => match finality.progress(block)? {
                BlockFinalityProgress::Confirming { elapsed, depth } => {
                    format!("Confirming ({elapsed}/{depth} blocks)")
                }
                BlockFinalityProgress::Safe => "Verifying setup…".into(),
            },
        })
    }
}

pub(super) fn swap_setup_confirmation(
    record: &ExecutorRecord,
    utxos: &[wallet_ops::UtxoOutput],
) -> Option<SwapSetupConfirmation> {
    let mut transactions = std::collections::BTreeSet::new();
    for payload in record
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
    {
        match record.recorded_payload_status(payload.hash()) {
            Some(ExecutorPayloadStatus::Executed) => return Some(SwapSetupConfirmation::Executed),
            Some(ExecutorPayloadStatus::Uncertain) | None => {
                transactions.extend(payload.transaction_hashes().iter().copied());
            }
            _ => {}
        }
    }
    let block = utxos
        .iter()
        .flat_map(|utxo| {
            std::iter::once((utxo.source_tx_hash.as_str(), utxo.source_block_number))
                .chain(utxo.spent_tx_hash.as_deref().zip(utxo.spent_block_number))
        })
        .filter_map(|(hash, block)| {
            (transactions.contains(&hash.parse::<B256>().ok()?) && block > 0).then_some(block)
        })
        .min()?;
    Some(SwapSetupConfirmation::Included(block))
}

/// The stage of one of the record's orders, from its submission and canonical observations.
/// The latest order's is the swap's stage; an earlier order's is where its own swap ended.
pub(in crate::root) fn swap_order_stage(
    record: &ExecutorRecord,
    order: &SwapOrderRecord,
) -> SwapStage {
    let state = swap_order_state(order);
    if state == SwapOrderState::Open {
        match order.submission_status() {
            SwapSubmissionStatus::Pending => return SwapStage::SubmissionPending,
            SwapSubmissionStatus::Rejected => return SwapStage::SubmissionRejected,
            SwapSubmissionStatus::Accepted => {}
        }
    }
    let observed = order.observations();
    // Funds reach the stealth account when the pre-hook runs. A recovery confirmed after
    // that returned them; an earlier cancellation, which is also recovery, didn't. An Across
    // refund reaches it later, and surplus kept there can be recovered before, so only a
    // recovery after the verified refund returned it. Without that refund, none did.
    let stranded_since = match (state, order.bridge()) {
        (SwapOrderState::Refunding, Some(BridgeOrderTerms::Across(_))) => observed
            .bridge_refund
            .map(|refund| refund.block.number.saturating_add(1)),
        _ => observed
            .pre_hook_executed
            .or(observed.traded)
            .map(|observation| observation.block.number),
    };
    let stranded = matches!(
        state,
        SwapOrderState::PreHookOnly { .. }
            | SwapOrderState::NotDelivered
            | SwapOrderState::Refunding
    );
    match stranded_since {
        Some(block) if stranded && recovered_since(record, block) => SwapStage::Recovered,
        _ => SwapStage::Order(state),
    }
}

/// Splits a record's orders, given in order as their sell token, buy token, delivery, and
/// whether each traded, into swaps. A retry keeps the pair and delivery after an attempt that
/// ended without a trade; a different pair or delivery, or any order after a traded one,
/// starts a new swap on the reused account.
pub(in crate::root) fn swap_order_ranges(
    orders: impl IntoIterator<Item = (Address, Address, SwapDelivery, bool)>,
) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut previous: Option<(Address, Address, SwapDelivery, bool)> = None;
    for (index, (sell, buy, delivery, traded)) in orders.into_iter().enumerate() {
        let retry = previous.is_some_and(
            |(previous_sell, previous_buy, previous_delivery, previous_traded)| {
                !previous_traded
                    && previous_sell == sell
                    && previous_buy == buy
                    && previous_delivery == delivery
            },
        );
        if retry && let Some(range) = ranges.last_mut() {
            range.end = index + 1;
        } else {
            ranges.push(index..index + 1);
        }
        previous = Some((sell, buy, delivery, traded));
    }
    ranges
}

/// One swap: the stealth account that places its orders, and the swap use on that account. The
/// account is what derives the address and recovers funds; the use picks the swap among the
/// account's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::root) struct SwapIdentity {
    pub(in crate::root) operation: ExecutorOperationId,
    pub(in crate::root) swap_use: SwapUseId,
}

/// One swap of a record that has orders: its swap use and its orders within the record's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct RecordSwap {
    pub(in crate::root) swap_use: SwapUseId,
    pub(in crate::root) orders: std::ops::Range<usize>,
}

/// The record's swaps that have orders, oldest first. Orders belong to the swap use they were
/// signed for. Orders from before swap uses all belong to their account's first use, so
/// within one use [`swap_order_ranges`] still tells a reused account's earlier swaps apart. A
/// swap that has no order yet isn't listed.
pub(in crate::root) fn record_swaps(record: &ExecutorRecord) -> Vec<RecordSwap> {
    let Some(swap) = record.swap() else {
        return Vec::new();
    };
    let mut swaps = Vec::new();
    let mut start = 0;
    for attempts in swap
        .orders()
        .chunk_by(|earlier, later| earlier.use_id() == later.use_id())
    {
        let swap_use = attempts
            .first()
            .and_then(SwapOrderRecord::use_id)
            .unwrap_or_else(|| SwapUseId::first(record.operation()));
        let ranges = swap_order_ranges(attempts.iter().map(|order| {
            let terms = swap.order_terms(order);
            (
                terms.sell_token(),
                terms.buy_token(),
                order.delivery(),
                order.observations().traded.is_some(),
            )
        }));
        swaps.extend(ranges.into_iter().map(|range| RecordSwap {
            swap_use,
            orders: start + range.start..start + range.end,
        }));
        start += attempts.len();
    }
    swaps
}

/// The swap use the record's own progress is about: its latest order's, or before any order,
/// the source use that claims the account for its first one.
pub(in crate::root) fn record_latest_use(record: &ExecutorRecord) -> Option<SwapUseId> {
    match record.swap().and_then(|swap| swap.orders().last()) {
        Some(order) => order.use_id(),
        None => record
            .swap_uses()
            .last()
            .filter(|swap_use| matches!(swap_use.role(), SwapUseRole::Source { .. }))
            .map(wallet_ops::vault::SwapUseRecord::id),
    }
}

/// The latest order the record holds of `swap_use`.
pub(in crate::root) fn swap_use_last_order(
    record: &ExecutorRecord,
    swap_use: SwapUseId,
) -> Option<&SwapOrderRecord> {
    record.swap_use_orders(swap_use).next_back()
}

/// The destination stealth account's operation the record's source use `swap_use` names, with
/// that use's private Bridge delivery: its latest order's, or before it has an order, the one
/// approved with the use.
pub(in crate::root) fn swap_use_destination(
    record: &ExecutorRecord,
    swap_use: SwapUseId,
) -> Option<(BridgeDelivery, ExecutorOperationId)> {
    let SwapUseRole::Source {
        approval,
        destination_operation,
    } = record.swap_use(swap_use)?.role()
    else {
        return None;
    };
    let delivery = swap_use_last_order(record, swap_use)
        .map(SwapOrderRecord::delivery)
        .or_else(|| approval.as_deref().map(|approval| approval.delivery))?;
    Some((delivery.private_bridge()?, (*destination_operation)?))
}

/// The swap use that claims the record's account to place a swap's orders and has none yet:
/// its accounts are reserved, and nothing but a setup can have been sent for it. A stopped use
/// isn't one, and neither is a use whose order exists, which the order's own progress shows.
pub(in crate::root) fn prepared_swap_use(record: &ExecutorRecord) -> Option<&SwapUseRecord> {
    let swap_use = record.swap_use(record.active_swap_use()?)?;
    (!swap_use.is_stopped()
        && matches!(swap_use.role(), SwapUseRole::Source { .. })
        && !record.has_swap_use_order(swap_use.id()))
    .then_some(swap_use)
}

/// Stopped preparations have no order UID but retain their account and signed-work history.
pub(in crate::root) fn cancelled_swap_uses(
    record: &ExecutorRecord,
) -> impl Iterator<Item = &SwapUseRecord> {
    record.swap_uses().iter().filter(|swap_use| {
        swap_use.is_stopped()
            && swap_use.approval().is_some()
            && !record.has_swap_use_order(swap_use.id())
    })
}

/// A recovery shield confirmed at or after `block`, paid by a broadcaster or by the account.
fn recovered_since(record: &ExecutorRecord, block: u64) -> bool {
    let confirmed = |inclusion: Option<ExecutorPayloadInclusion>| {
        inclusion.is_some_and(|inclusion| {
            inclusion.result() == ExecutorExecutionResult::Executed
                && inclusion.block().number >= block
        })
    };
    record.issued().iter().any(|payload| {
        payload.purpose() == ExecutorPayloadPurpose::Recovery && confirmed(payload.inclusion())
    }) || record.recovery_transactions().iter().any(|transaction| {
        transaction.kind() == ExecutorRecoveryStepKind::Shield && confirmed(transaction.inclusion())
    })
}

/// The setup stage from recorded setup outcomes alone. While this session observes the setup,
/// a recorded execution stays pending until the delegation itself is confirmed.
fn recorded_setup_stage(statuses: &[Option<ExecutorPayloadStatus>], observing: bool) -> SwapStage {
    if statuses.is_empty() {
        return SwapStage::SetupNotSent;
    }
    if statuses.contains(&Some(ExecutorPayloadStatus::Executed)) {
        return if observing {
            SwapStage::SetupPending
        } else {
            SwapStage::Ready
        };
    }
    let failed = statuses.iter().all(|status| {
        matches!(
            status,
            Some(ExecutorPayloadStatus::Reverted | ExecutorPayloadStatus::Invalidated { .. })
        )
    });
    if failed {
        SwapStage::SetupFailed
    } else {
        SwapStage::SetupPending
    }
}

/// The sell and buy tokens of a swap record: from its terms once an order exists, otherwise
/// the pair approved with its setup.
pub(in crate::root) fn swap_tokens(record: &ExecutorRecord) -> Option<(Address, Address)> {
    if let Some(swap) = record.swap() {
        return Some((swap.terms().sell_token(), swap.terms().buy_token()));
    }
    record.swap_approval_tokens()
}

/// The latest order's private spend, or before any order exists, the spend approved with the
/// setup. The order itself sells this less Railgun's unshield fee.
pub(in crate::root) fn swap_sell_amount(record: &ExecutorRecord) -> Option<U256> {
    record
        .swap()
        .and_then(|swap| swap.orders().last())
        .map(|order| order.bounds().spend_amount())
        .or_else(|| {
            record
                .swap_approval()
                .map(|approval| approval.bounds.spend_amount())
        })
}

/// The approved private minimum of the latest order, or before any order exists, the minimum
/// approved with the setup.
pub(in crate::root) fn swap_private_minimum(record: &ExecutorRecord) -> Option<U256> {
    record
        .swap()
        .and_then(|swap| swap.orders().last())
        .map(|order| order.bounds().private_minimum)
        .or_else(|| {
            record
                .swap_approval()
                .map(|approval| approval.bounds.private_minimum)
        })
}

/// Where the latest order delivers, or before any order exists, the delivery approved with the
/// setup. A retry keeps it, as it keeps the pair.
pub(in crate::root) fn swap_delivery(record: &ExecutorRecord) -> SwapDelivery {
    record
        .swap()
        .and_then(|swap| swap.orders().last())
        .map(SwapOrderRecord::delivery)
        .or_else(|| record.swap_approval().map(|approval| approval.delivery))
        .unwrap_or_default()
}

/// The latest order's expiry in Unix seconds.
pub(in crate::root) fn swap_valid_to(record: &ExecutorRecord) -> Option<u64> {
    record
        .swap()
        .and_then(|swap| swap.orders().last())
        .map(|order| u64::from(order.valid_to()))
}

/// The first block a scan must cover to observe the swap's current step.
pub(in crate::root) fn swap_history_start(record: &ExecutorRecord) -> Option<u64> {
    let latest_pre_hook = record
        .swap()
        .and_then(|swap| swap.orders().last())
        .map(|order| order.pre_hook().payload());
    record
        .issued()
        .iter()
        .filter(|payload| match latest_pre_hook {
            Some(pre_hook) => payload.hash() == pre_hook,
            None => payload.purpose() == ExecutorPayloadPurpose::Operation,
        })
        .map(|payload| payload.context().history_start())
        .min()
}

/// Names a swap in one line: "1.00 WETH for USDC", or "WETH for USDC" before an amount is known.
pub(in crate::root) fn swap_pair_label(sell: &str, buy_symbol: &str) -> String {
    format!("{sell} for {buy_symbol}")
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
}

impl SwapBridgeLabels {
    /// Who gets the proceeds on the destination network, as copy names it after "to".
    const fn destination(&self) -> &str {
        if self.private.is_some() {
            "your private balance"
        } else {
            self.receiver.as_str()
        }
    }
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
    /// The block the setup was included in, once recorded.
    pub(in crate::root) block: Option<u64>,
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

/// One account's setup progress from the stage its own record gives.
pub(in crate::root) const fn swap_setup_progress(stage: SwapStage) -> SwapSetupProgress {
    match stage {
        SwapStage::SetupNotSent => SwapSetupProgress::NotSent,
        SwapStage::SetupSubmitting => SwapSetupProgress::Submitting,
        SwapStage::SetupPending => SwapSetupProgress::Pending,
        SwapStage::SetupFailed | SwapStage::SetupRetired => SwapSetupProgress::Failed,
        _ => SwapSetupProgress::Done,
    }
}

/// A private Bridge swap is set up once both stealth accounts are. With the swap's own account
/// set up, the destination account's setup decides the stage shown.
pub(in crate::root) const fn private_bridge_setup_stage(
    stage: SwapStage,
    destination: SwapSetupProgress,
) -> SwapStage {
    match (stage, destination) {
        (SwapStage::Ready | SwapStage::Approved, SwapSetupProgress::NotSent) => {
            SwapStage::SetupNotSent
        }
        (SwapStage::Ready | SwapStage::Approved, SwapSetupProgress::Submitting) => {
            SwapStage::SetupSubmitting
        }
        (
            SwapStage::Ready | SwapStage::Approved,
            SwapSetupProgress::Pending | SwapSetupProgress::NetworkLoading,
        ) => SwapStage::SetupPending,
        (SwapStage::Ready | SwapStage::Approved, SwapSetupProgress::Failed) => {
            SwapStage::SetupFailed
        }
        _ => stage,
    }
}

/// The block a record's executed setup was included in.
pub(in crate::root) fn swap_setup_block(record: &ExecutorRecord) -> Option<u64> {
    record
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
        .find_map(|payload| {
            payload
                .inclusion()
                .filter(|inclusion| inclusion.result() == ExecutorExecutionResult::Executed)
                .map(|inclusion| inclusion.block().number)
        })
}

/// The record's delivery when it shields to the wallet on another network.
pub(in crate::root) fn swap_private_delivery(record: &ExecutorRecord) -> Option<BridgeDelivery> {
    swap_delivery(record).private_bridge()
}

/// What a private Bridge delivery credits to the private balance: `amount`, which the
/// destination stealth account shields, less Railgun's shield fee at the rate the order's
/// approval bound.
pub(in crate::root) fn private_delivery_credit(amount: U256, bounds: &SwapApprovedBounds) -> U256 {
    let fee = bounds
        .destination_shield_fee_bps
        .map_or(U256::ZERO, |fee_bps| {
            amount.saturating_mul(fee_bps) / U256::from(10_000u32)
        });
    amount.saturating_sub(fee)
}

/// Whether a recovery on the destination network returned what its stealth account held since
/// the fill in `held_block`.
pub(in crate::root) fn held_proceeds_recovered(
    destination: &ExecutorRecord,
    held_block: u64,
) -> bool {
    recovered_since(destination, held_block)
}

pub(in crate::root) const fn provider_name(provider: BridgeProvider) -> &'static str {
    match provider {
        BridgeProvider::Across => "Across",
        BridgeProvider::NearIntents => "NEAR Intents",
    }
}

/// What a traded Bridge order handed to its bridge, in bought-token base units: an Across
/// deposit's input amount, or the whole trade paid to a NEAR Intents deposit address. Without
/// an Across deposit, the trade's amount stays in the stealth account.
pub(in crate::root) fn bridge_sent_amount(order: &SwapOrderRecord) -> Option<U256> {
    let observed = order.observations();
    match order.bridge() {
        Some(BridgeOrderTerms::Across(terms)) if observed.bridge_handoff.is_some() => {
            Some(terms.input_amount)
        }
        _ => observed.trade_amounts.map(|trade| trade.buy_amount),
    }
}

/// The orderbook reports an open order filled before canonical observation records the trade.
/// It changes copy and hides cancellation; the stage, other actions, reservations and recovery
/// follow observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapFillHint {
    /// Blocks from the reported trade block to the synced head, when both are known.
    pub(in crate::root) confirmations: Option<u64>,
    /// The chain's finality depth, after which observation reads the trade.
    pub(in crate::root) depth: u64,
}

impl SwapFillHint {
    /// Confirmations first, then the canonical check still needed after the threshold.
    fn progress(self) -> String {
        match self.confirmations {
            Some(confirmations) if confirmations >= self.depth => "Verifying settlement…".into(),
            Some(confirmations) => format!("Confirming ({confirmations}/{})", self.depth),
            None => "Confirming".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::root) struct SwapCardLine {
    pub(in crate::root) title: String,
    pub(in crate::root) detail: String,
    pub(in crate::root) attention: bool,
}

const NOTHING_UNSHIELDED: &str = "Nothing was unshielded. You can retry with a new quote.";

/// One card line for one swap.
pub(in crate::root) fn swap_card_line(stage: SwapStage, labels: &SwapLabels) -> SwapCardLine {
    let pair = &labels.pair;
    let expires = |state: &str| {
        labels
            .expires
            .as_ref()
            .map_or_else(|| state.to_owned(), |at| format!("{state} · expires {at}"))
    };
    let line = |title: String, detail: String| SwapCardLine {
        title,
        detail,
        attention: stage.needs_attention(),
    };
    let bridge = labels.bridge.as_ref();
    // A private Bridge swap sets up one stealth account on each network.
    let private = bridge.is_some_and(|bridge| bridge.private.is_some());
    let set_up = if private {
        "The stealth accounts are set up."
    } else {
        "The stealth account is set up."
    };
    match stage {
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
            if labels.fill_hint.is_some() =>
        {
            line(format!("Swapping {pair}"), "Traded · confirming".into())
        }
        SwapStage::SetupNotSent => line(
            format!("Swap of {pair} isn't set up"),
            "Continue to set up its stealth account, or dismiss the swap.".into(),
        ),
        SwapStage::SetupRetired => line(
            format!("Swap of {pair} needs a new stealth account"),
            "Start a new swap, or recover any funds in the old account.".into(),
        ),
        SwapStage::SetupSubmitting | SwapStage::SetupPending => line(
            format!("Preparing swap of {pair}"),
            if private {
                "Setting up the swap's stealth accounts · waiting for confirmation"
            } else {
                "Setting up the swap's stealth account · waiting for confirmation"
            }
            .into(),
        ),
        SwapStage::SetupFailed => line(
            "Swap setup failed".into(),
            format!("{pair} · Set it up again or dismiss the swap."),
        ),
        SwapStage::Ready => line(
            format!("Swap of {pair} is ready to review"),
            format!("{set_up} Review a quote to place the order."),
        ),
        SwapStage::Approved => line(
            format!("Swap of {pair} is ready to place"),
            format!("{set_up} Confirm to place the order."),
        ),
        SwapStage::SubmissionPending => line(
            format!("Swap of {pair} needs submission"),
            expires("Acceptance unconfirmed · Submit again"),
        ),
        SwapStage::SubmissionRejected => line(
            format!("Swap order for {pair} was rejected"),
            expires("Retry after finalized expiry"),
        ),
        SwapStage::Order(SwapOrderState::Open) if labels.lapsed => {
            line(format!("Swapping {pair}"), "Expired · check status".into())
        }
        SwapStage::Order(SwapOrderState::Open) => {
            line(format!("Swapping {pair}"), expires("Order open"))
        }
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }) if labels.lapsed => line(
            format!("Swapping {pair}"),
            "Unshielded, expired · check status".into(),
        ),
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }) => line(
            format!("Swapping {pair}"),
            expires("Unshielded, waiting for a solver"),
        ),
        SwapStage::Order(SwapOrderState::Traded) => line(
            format!("Swapping {pair}"),
            bridge.map_or_else(
                || "Traded · private return unverified".into(),
                |bridge| {
                    format!(
                        "Traded · hand-off to {} unverified",
                        provider_name(bridge.provider)
                    )
                },
            ),
        ),
        SwapStage::Order(SwapOrderState::Bridging) => line(
            format!("Swapping {pair}"),
            bridge.map_or_else(
                || "Sent to the bridge".into(),
                |bridge| {
                    format!(
                        "Sent to {} · {} to {} on {}",
                        provider_name(bridge.provider),
                        if private { "shielding" } else { "delivering" },
                        bridge.destination(),
                        bridge.network
                    )
                },
            ),
        ),
        SwapStage::Order(SwapOrderState::Done) => line(
            format!("Swapped {pair}"),
            match (bridge, &labels.receiver, &labels.received) {
                (Some(bridge), _, received) => format!(
                    "Delivered {}to {} on {} {} {}",
                    received
                        .as_ref()
                        .map_or_else(String::new, |amount| format!("{amount} ")),
                    bridge.destination(),
                    bridge.network,
                    // An Across delivery the wallet couldn't verify is named as Across's word.
                    if bridge.reported && bridge.provider == BridgeProvider::Across {
                        "· reported by"
                    } else {
                        "via"
                    },
                    provider_name(bridge.provider)
                ),
                (None, Some(receiver), Some(received)) => {
                    format!("Delivered {received} to {receiver}")
                }
                (None, Some(receiver), None) => format!("Delivered to {receiver}"),
                (None, None, Some(received)) => format!("Received {received} privately"),
                (None, None, None) => "Back in your private balance".into(),
            },
        ),
        SwapStage::Order(SwapOrderState::AttemptEnded(cause)) => line(
            match cause {
                SwapPreHookDeathCause::Expired => "Swap order expired",
                SwapPreHookDeathCause::Cancellation => "Swap order cancelled",
                SwapPreHookDeathCause::Recovery
                | SwapPreHookDeathCause::OlderPostHook
                | SwapPreHookDeathCause::Unknown => "Swap order ended",
            }
            .into(),
            NOTHING_UNSHIELDED.into(),
        ),
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: true }) => line(
            "Swap needs attention".into(),
            format!(
                "{} is in the swap's stealth account and wasn't traded",
                labels.sell
            ),
        ),
        SwapStage::Order(SwapOrderState::NotDelivered) => line(
            "Swap needs attention".into(),
            match bridge {
                // The Across post-hook didn't run, so the bought token stayed behind.
                Some(bridge) => format!(
                    "{} is in the swap's stealth account on {} and wasn't sent to the bridge",
                    bridge.sent.as_ref().unwrap_or(&labels.buy_symbol),
                    bridge.origin
                ),
                None => format!(
                    "{} is in the swap's stealth account and wasn't moved to your private balance",
                    labels.buy_symbol
                ),
            },
        ),
        SwapStage::Order(SwapOrderState::Refunding) => line(
            format!("Swap of {pair} is refunding"),
            bridge.map_or_else(
                || {
                    format!(
                        "{} returns to the swap's stealth account.",
                        labels.buy_symbol
                    )
                },
                |bridge| {
                    format!(
                        "{} didn't deliver to {} on {}. {} returns to the stealth account on {}.",
                        provider_name(bridge.provider),
                        bridge.destination(),
                        bridge.network,
                        bridge.sent.as_ref().unwrap_or(&labels.buy_symbol),
                        bridge.origin
                    )
                },
            ),
        ),
        SwapStage::Order(SwapOrderState::NeedsAttention) => line(
            "Swap needs attention".into(),
            bridge.map_or_else(
                || "The bridge provider has to resolve the deposit".into(),
                |bridge| {
                    format!(
                        "{} reported the deposit for {} on {} as failed or incomplete",
                        provider_name(bridge.provider),
                        bridge.destination(),
                        bridge.network
                    )
                },
            ),
        ),
        // The fill completed without its shield, so recovery is on the destination network.
        SwapStage::Order(SwapOrderState::HeldOnDestination) => line(
            format!("Swap of {pair} needs recovery"),
            bridge.map_or_else(
                || "The destination stealth account holds the proceeds. Recover them there.".into(),
                |bridge| {
                    format!(
                        "{} is in the stealth account on {}. Recover it there.",
                        bridge
                            .private
                            .as_ref()
                            .and_then(|private| private.held.as_ref())
                            .unwrap_or(&bridge.token),
                        bridge.network
                    )
                },
            ),
        ),
        SwapStage::Recovered => line(
            "Swap recovered".into(),
            format!("{pair} · Recovery returned the funds to your private balance."),
        ),
    }
}

/// The single Private tab card for every shown swap.
pub(in crate::root) fn swaps_card_line(swaps: &[(SwapStage, SwapLabels)]) -> Option<SwapCardLine> {
    match swaps {
        [] => None,
        [(stage, labels)] => Some(swap_card_line(*stage, labels)),
        swaps => {
            let attention = swaps
                .iter()
                .filter(|(stage, _)| stage.needs_attention())
                .count();
            Some(SwapCardLine {
                title: if attention == 0 {
                    format!("{} swaps in progress", swaps.len())
                } else {
                    format!("{} swaps · {attention} needs attention", swaps.len())
                },
                detail: swaps
                    .iter()
                    .map(|(_, labels)| labels.pair.as_str())
                    .collect::<Vec<_>>()
                    .join(" · "),
                attention: attention > 0,
            })
        }
    }
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
}

impl SwapStep {
    fn new(
        label: impl Into<String>,
        detail: impl Into<String>,
        status: PublicActionStepStatus,
    ) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            status,
            children: Vec::new(),
            account: None,
        }
    }
}

/// A step's status from its sub-steps: failed if any failed, done once all are,
/// and otherwise in progress.
pub(in crate::root) fn parent_step_status(children: &[SwapStep]) -> PublicActionStepStatus {
    use PublicActionStepStatus::{Done, Error, Pending};
    let all = |status| children.iter().all(|child| child.status == status);
    if children.iter().any(|child| child.status == Error) {
        Error
    } else if all(Done) {
        Done
    } else {
        Pending
    }
}

const TRADED: &str = "Traded";
/// What a reused stealth account's sub-step shows in place of a setup's block.
const REUSED_READY: &str = "Reused · ready";
const BACK_IN_PRIVATE_BALANCE: &str = "Back in private balance";
const BRIDGE_DEPOSIT: &str = "Bridge deposit";

/// The progress steps of decision 8: set up, order open, traded, back in private balance. An
/// attempt that ended, or stranded funds before a trade, ends the list at the order step. A
/// fill the orderbook reports shows the trade confirming until observation records it. A Public
/// address swap has no private-balance step, as [`external_steps`] describes, and a Bridge swap
/// hands off to its bridge instead, as [`bridge_steps`] describes.
pub(in crate::root) fn swap_steps(stage: SwapStage, labels: &SwapLabels) -> Vec<SwapStep> {
    if let Some(bridge) = &labels.bridge {
        let mut steps = bridge_steps(stage, labels, bridge);
        if let (Some(private), Some(setup)) = (&bridge.private, steps.first_mut()) {
            *setup = private_setup_step(stage, private, setup);
        }
        return steps;
    }
    let steps = reshield_steps(stage, labels);
    match &labels.receiver {
        Some(receiver) => external_steps(steps, receiver, labels),
        None => steps,
    }
}

fn reshield_steps(stage: SwapStage, labels: &SwapLabels) -> Vec<SwapStep> {
    use PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
    let step = |label: &str, detail: String, status| SwapStep::new(label, detail, status);
    let setup = |status, detail: &str| step("Stealth account set up", detail.to_owned(), status);
    let open = |status, detail: String| step("Order open", detail, status);
    let traded = |status| step(TRADED, String::new(), status);
    let delivered = |status, detail: String| step(BACK_IN_PRIVATE_BALANCE, detail, status);
    let expiry = || {
        labels.expires.as_ref().map_or_else(String::new, |at| {
            if labels.lapsed {
                format!("Expired at {at} · awaiting confirmation")
            } else {
                format!("Expires {at}")
            }
        })
    };
    match stage {
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
            if labels.fill_hint.is_some() =>
        {
            vec![
                setup(Done, ""),
                open(Done, String::new()),
                step(
                    TRADED,
                    labels
                        .fill_hint
                        .map_or_else(String::new, SwapFillHint::progress),
                    Pending,
                ),
                delivered(NotStarted, String::new()),
            ]
        }
        SwapStage::SetupNotSent => vec![
            setup(NotStarted, "Not sent yet"),
            open(NotStarted, String::new()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::SetupRetired => vec![
            setup(
                Warning,
                "This stealth account can no longer be used for the swap.",
            ),
            open(NotStarted, "No order was placed".into()),
        ],
        SwapStage::SetupSubmitting => vec![
            setup(Pending, "Sending the setup through a broadcaster"),
            open(NotStarted, String::new()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::SetupPending => vec![
            setup(Pending, "Waiting for confirmation"),
            open(NotStarted, String::new()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::SetupFailed => vec![
            setup(
                Warning,
                "The setup didn't take effect. Nothing was unshielded.",
            ),
            open(NotStarted, String::new()),
        ],
        SwapStage::Ready => vec![
            setup(Done, ""),
            open(NotStarted, "Review a quote to place the order".into()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::Approved => vec![
            setup(Done, ""),
            open(NotStarted, "Confirm to place the order".into()),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::SubmissionPending | SwapStage::SubmissionRejected => vec![
            setup(Done, ""),
            step(
                "Order submission",
                if stage == SwapStage::SubmissionPending {
                    format!(
                        "Acceptance is unconfirmed. Submit the same order again. {}",
                        expiry()
                    )
                } else {
                    format!(
                        "This order was rejected. Retry after its expiry is final. {}",
                        expiry()
                    )
                },
                Warning,
            ),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::Order(SwapOrderState::Open) => vec![
            setup(Done, ""),
            open(
                Pending,
                [
                    String::from(if labels.lapsed {
                        ""
                    } else {
                        "Waiting for a solver"
                    }),
                    expiry(),
                ]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
            ),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }) => vec![
            setup(Done, ""),
            open(
                Pending,
                format!(
                    "{} is unshielded{}. {}",
                    labels.sell,
                    if labels.lapsed {
                        ""
                    } else {
                        " and waiting for a solver"
                    },
                    expiry()
                )
                .trim_end()
                .to_owned(),
            ),
            traded(NotStarted),
            delivered(NotStarted, String::new()),
        ],
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: true }) => vec![
            setup(Done, ""),
            step(
                "Unshielded, not traded",
                format!(
                    "{} is in the stealth account. The order expired without a fill.",
                    labels.sell
                ),
                Warning,
            ),
        ],
        SwapStage::Order(SwapOrderState::Traded) => vec![
            setup(Done, ""),
            open(Done, String::new()),
            traded(Done),
            delivered(
                Pending,
                "Private return unverified. Check status for retry or recovery.".into(),
            ),
        ],
        SwapStage::Order(SwapOrderState::Done) => vec![
            setup(Done, ""),
            open(Done, String::new()),
            traded(Done),
            delivered(Done, "Finalized".into()),
        ],
        SwapStage::Order(SwapOrderState::NotDelivered) => vec![
            setup(Done, ""),
            open(Done, String::new()),
            traded(Done),
            step(
                "Not delivered",
                format!(
                    "{} is in the stealth account. It wasn't moved to your private balance.",
                    labels.buy_symbol
                ),
                Warning,
            ),
        ],
        // Only Bridge orders reach these states; [`bridge_steps`] adds their hand-off and
        // outcome.
        SwapStage::Order(
            SwapOrderState::Bridging
            | SwapOrderState::Refunding
            | SwapOrderState::NeedsAttention
            | SwapOrderState::HeldOnDestination,
        ) => vec![setup(Done, ""), open(Done, String::new())],
        // An unfilled expiry is a normal result of a tight minimum, so it's a warning with
        // its likely reason, not an error.
        SwapStage::Order(SwapOrderState::AttemptEnded(cause)) => vec![
            setup(Done, ""),
            step(
                match cause {
                    SwapPreHookDeathCause::Expired => "Not filled",
                    SwapPreHookDeathCause::Cancellation => "Order cancelled",
                    SwapPreHookDeathCause::Recovery
                    | SwapPreHookDeathCause::OlderPostHook
                    | SwapPreHookDeathCause::Unknown => "Order ended",
                },
                if cause == SwapPreHookDeathCause::Expired {
                    let at = labels
                        .expires
                        .as_ref()
                        .map_or_else(String::new, |at| format!(" at {at}"));
                    match &labels.uncovered_gas {
                        Some(rest) => format!(
                            "No solver covered {rest} of the gas before the order expired{at}."
                        ),
                        None => format!("No solver filled it before the order expired{at}."),
                    }
                } else {
                    "Nothing was unshielded.".into()
                },
                Warning,
            ),
        ],
        SwapStage::Recovered => vec![
            setup(Done, ""),
            step(
                "Recovered",
                "Recovery returned the funds to your private balance.".into(),
                Done,
            ),
        ],
    }
}

/// A Public address swap's steps. The settlement that trades also pays the receiver, so one
/// step, delivered to the receiver, replaces Traded and Back in private balance: the least the
/// receiver gets while the order can fill, then what it got.
fn external_steps(steps: Vec<SwapStep>, receiver: &str, labels: &SwapLabels) -> Vec<SwapStep> {
    use PublicActionStepStatus::{Done, NotStarted};
    let traded = steps
        .iter()
        .find(|step| step.label == TRADED)
        .map(|step| (step.status, step.detail.clone()));
    steps
        .into_iter()
        .filter(|step| step.label != TRADED)
        .map(|mut step| {
            if step.label != BACK_IN_PRIVATE_BALANCE {
                return step;
            }
            step.label = format!("Delivered to {receiver}");
            // A reported fill confirms here, since no separate trade step shows it.
            if let Some((status, detail)) = traded
                .clone()
                .filter(|(status, _)| step.status == NotStarted && *status != NotStarted)
            {
                step.status = status;
                step.detail = detail;
            }
            step.detail = match (&labels.received, &labels.minimum) {
                (Some(received), _) if step.status == Done => {
                    format!("{} → {received} · Finalized", labels.sell)
                }
                (_, Some(minimum)) if step.status != Done && step.detail.is_empty() => {
                    format!("At least {minimum}")
                }
                _ => step.detail,
            };
            step
        })
        .collect()
}

/// A private Bridge swap's account step with one sub-step per network, each naming its stealth
/// account. An account the swap sets up shows the block of its setup or what it waits for. An
/// account the swap reuses shows that it is reused and ready, with no setup of its own, and
/// never as pending. The step is "Stealth accounts set up", or "Stealth accounts ready" when the
/// swap sets up neither. Its own status follows its sub-steps, as [`parent_step_status`]
/// derives it.
fn private_setup_step(
    stage: SwapStage,
    private: &SwapPrivateBridgeLabels,
    setup: &SwapStep,
) -> SwapStep {
    use PublicActionStepStatus::{Done, Error, NotStarted, Pending};
    let children = private
        .setups
        .iter()
        .map(|account| {
            let (status, detail) = match account.progress {
                SwapSetupProgress::Done if account.reused => (Done, REUSED_READY.to_owned()),
                // Its records are read once its network is loaded. Nothing is on its way.
                SwapSetupProgress::NetworkLoading if account.reused => (
                    NotStarted,
                    format!("Reused · checked once {} loads", account.network),
                ),
                SwapSetupProgress::NotSent => (NotStarted, "Not sent yet".to_owned()),
                SwapSetupProgress::NetworkLoading => {
                    (Pending, format!("Waiting for {} to load…", account.network))
                }
                SwapSetupProgress::Submitting => (
                    Pending,
                    account
                        .detail
                        .clone()
                        .unwrap_or_else(|| "Submitting…".into()),
                ),
                SwapSetupProgress::Pending => (
                    Pending,
                    account
                        .detail
                        .clone()
                        .unwrap_or_else(|| "Waiting for broadcaster…".into()),
                ),
                SwapSetupProgress::Done => (
                    Done,
                    account
                        .block
                        .map_or_else(String::new, |block| format!("block {block}")),
                ),
                SwapSetupProgress::Failed => (Error, "Not confirmed".to_owned()),
            };
            SwapStep {
                account: account.account,
                ..SwapStep::new(account.network.clone(), detail, status)
            }
        })
        .collect::<Vec<_>>();
    let status = parent_step_status(&children);
    let failed = children
        .iter()
        .filter(|child| child.status == Error)
        .map(|child| child.label.as_str())
        .collect::<Vec<_>>();
    let detail = match stage {
        // A retired account keeps the reason its swap can't continue.
        SwapStage::SetupRetired => setup.detail.clone(),
        _ if failed.is_empty() => String::new(),
        _ => format!(
            "The setup on {} wasn't confirmed. Nothing was unshielded.",
            failed.join(" and ")
        ),
    };
    let label = if private.setups.iter().all(|account| account.reused) {
        "Stealth accounts ready"
    } else {
        "Stealth accounts set up"
    };
    SwapStep {
        children,
        ..SwapStep::new(label, detail, status)
    }
}

/// A Bridge swap's steps: set up, order open, sent to the bridge once the hand-off is proven,
/// then delivered on the destination network, labelled by the recorded outcome: verified, or
/// reported by the provider. Refunding and Needs attention replace the delivered step. An Across
/// post-hook that didn't run replaces both with the bought token held on the swap's network. A
/// private Bridge swap's last step is its shield into the private balance there, and proceeds
/// its destination stealth account holds replace that step.
fn bridge_steps(stage: SwapStage, labels: &SwapLabels, bridge: &SwapBridgeLabels) -> Vec<SwapStep> {
    use PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
    let step = |label: String, detail: String, status| SwapStep::new(label, detail, status);
    let provider = provider_name(bridge.provider);
    let sent = bridge.sent.as_ref().unwrap_or(&labels.buy_symbol);
    let handed_off = || {
        step(
            BRIDGE_DEPOSIT.into(),
            bridge
                .sent
                .as_ref()
                .map_or_else(String::new, |sent| match bridge.provider {
                    BridgeProvider::Across => format!("{sent} to Across"),
                    BridgeProvider::NearIntents => format!("{sent} to 1Click"),
                }),
            Done,
        )
    };
    let private = bridge.private.as_ref();
    let delivered = if private.is_some() {
        format!("Private on {}", bridge.network)
    } else {
        format!("Delivered on {}", bridge.network)
    };
    // Across delivers its deposit's exact output; NEAR Intents converts all of it.
    let expected = match (&bridge.minimum, bridge.provider) {
        (Some(minimum), BridgeProvider::Across) => {
            format!("{minimum} to {}", bridge.destination())
        }
        (Some(minimum), BridgeProvider::NearIntents) => {
            format!("At least {minimum} to {}", bridge.destination())
        }
        (None, _) => format!("To {}", bridge.destination()),
    };
    let mut steps = reshield_steps(stage, labels);
    let outcome = match stage {
        SwapStage::Order(SwapOrderState::Traded) => vec![
            step(
                BRIDGE_DEPOSIT.into(),
                "Checking the settlement…".into(),
                Pending,
            ),
            step(delivered, expected, NotStarted),
        ],
        SwapStage::Order(SwapOrderState::Bridging) => vec![
            handed_off(),
            step(
                delivered,
                format!(
                    "{expected} · {}",
                    match bridge.provider {
                        BridgeProvider::Across => "Across usually fills within a few minutes",
                        BridgeProvider::NearIntents => {
                            "NEAR Intents usually delivers within a few minutes"
                        }
                    }
                ),
                Pending,
            ),
        ],
        SwapStage::Order(SwapOrderState::Done) => vec![
            handed_off(),
            step(
                if bridge.reported {
                    format!("{delivered} · reported by {provider}")
                } else {
                    format!("{delivered} · verified")
                },
                match (&labels.received, private) {
                    (Some(received), Some(_)) => {
                        format!("{received} shielded to {}", bridge.destination())
                    }
                    (None, Some(_)) => format!("Shielded to {}", bridge.destination()),
                    (Some(received), None) => format!("{received} to {}", bridge.receiver),
                    (None, None) => format!("To {}", bridge.receiver),
                },
                Done,
            ),
        ],
        SwapStage::Order(SwapOrderState::HeldOnDestination) => vec![
            handed_off(),
            step(
                format!("Held by stealth account on {}", bridge.network),
                {
                    let token = &bridge.token;
                    let account = private
                        .and_then(|private| private.setups[1].account?.index)
                        .map_or_else(
                            || "the stealth account".to_owned(),
                            |index| format!("stealth account #{index}"),
                        );
                    format!(
                        "{provider} delivered {}, but the shield didn't run. The {token} is in {account} on {}.",
                        private
                            .and_then(|private| private.held.as_ref())
                            .unwrap_or(token),
                        bridge.network
                    )
                },
                Warning,
            ),
        ],
        SwapStage::Order(SwapOrderState::Refunding) => vec![
            handed_off(),
            step(
                format!("Refunding on {}", bridge.origin),
                match bridge.provider {
                    BridgeProvider::Across => format!(
                        "No relayer filled the deposit before it expired. Across returns {sent} to the stealth account on {}, usually within a few hours.",
                        bridge.origin
                    ),
                    BridgeProvider::NearIntents => format!(
                        "NEAR Intents refunded the deposit to the stealth account on {}.",
                        bridge.origin
                    ),
                },
                Warning,
            ),
        ],
        SwapStage::Order(SwapOrderState::NeedsAttention) => vec![
            handed_off(),
            step(
                "Needs attention".into(),
                format!(
                    "{provider} reported the deposit as failed or incomplete. The wallet can't recover it. {provider} has to resolve it."
                ),
                Warning,
            ),
        ],
        SwapStage::Order(SwapOrderState::NotDelivered) => vec![step(
            "Not sent to the bridge".into(),
            format!("{sent} is in the stealth account on {}.", bridge.origin),
            Warning,
        )],
        // Before the trade, the hand-off waits for it, including a fill the orderbook reports.
        _ => {
            let traded = steps
                .iter()
                .find(|step| step.label == TRADED)
                .map(|step| (step.status, step.detail.clone()));
            return steps
                .into_iter()
                .filter(|step| step.label != TRADED)
                .flat_map(|step| {
                    if step.label != BACK_IN_PRIVATE_BALANCE {
                        return vec![step];
                    }
                    let (status, detail) = traded
                        .clone()
                        .unwrap_or_else(|| (NotStarted, String::new()));
                    vec![
                        SwapStep::new(BRIDGE_DEPOSIT, detail, status),
                        SwapStep::new(delivered.clone(), expected.clone(), NotStarted),
                    ]
                })
                .collect();
        }
    };
    // Set up and Order open are done once the order traded.
    steps.truncate(2);
    steps.extend(outcome);
    steps
}

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

/// The outcome of an order delivered as `delivery` once its trade is observed.
pub(in crate::root) fn swap_outcome(
    bounds: &SwapApprovedBounds,
    observed: &SwapOrderObservations,
    delivery: SwapDelivery,
) -> Option<SwapOutcome> {
    let traded = observed.traded?;
    let trade = observed.trade_amounts;
    let received_privately = observed
        .settlement_credit
        .or(observed.shielded)
        .map(|shield| (shield.private_amount, shield.fee));
    let received = match delivery {
        SwapDelivery::Reshield => received_privately.map(|(amount, _)| amount),
        // The trade paid the receiver directly.
        SwapDelivery::External { .. } => trade.map(|trade| trade.buy_amount),
        SwapDelivery::Bridge(_) => None,
    };
    let gas = trade.and_then(|trade| {
        Some(SwapOutcomeGas {
            fee: trade.executed_fee?,
            fee_token: trade.executed_fee_token?,
            settlement_cost: U256::from(trade.settlement_gas_used?)
                .saturating_mul(U256::from(trade.settlement_effective_gas_price?)),
        })
    });
    Some(SwapOutcome {
        spent: bounds.spend_amount(),
        unshield_fee: bounds.spend_amount().saturating_sub(bounds.sell_amount),
        limit_sell: bounds.sell_amount,
        minimum: bounds.buy_amount,
        trade,
        private_minimum: bounds.private_minimum,
        received,
        above_minimum: received
            .filter(|received| *received > bounds.private_minimum)
            .map(|received| received - bounds.private_minimum),
        received_privately,
        gas,
        settlement: traded.transaction_hash,
    })
}

/// Whether `order` traded on this network with the settlement's gas cost recorded but not the
/// fee the orderbook charged, the one figure the outcome's fee row still lacks. Trades recorded
/// before the gas cost was kept can't show the row, so their fee isn't asked for.
pub(in crate::root) fn needs_executed_fee(order: &SwapOrderRecord) -> bool {
    let observed = order.observations();
    !matches!(order.delivery(), SwapDelivery::Bridge(_))
        && observed.traded.is_some()
        && observed.trade_amounts.is_some_and(|amounts| {
            amounts.executed_fee.is_none()
                && amounts.settlement_gas_used.is_some()
                && amounts.settlement_effective_gas_price.is_some()
        })
}

/// An action the progress dialog offers. `Err` carries the reason it's disabled.
pub(in crate::root) type SwapActionAvailability = Option<Result<(), &'static str>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::root) struct SwapActions {
    pub(in crate::root) cancel: SwapActionAvailability,
    pub(in crate::root) retry: SwapActionAvailability,
    /// Set up again or continue to the review in the swap form, or place an approved order.
    pub(in crate::root) resume: SwapActionAvailability,
    pub(in crate::root) recover: bool,
    /// Recover what a private Bridge swap's destination stealth account holds, on its network.
    pub(in crate::root) recover_on_destination: bool,
    pub(in crate::root) dismiss: bool,
}

/// What the progress dialog offers for `stage`. `past_valid_to` is local time past the latest
/// order's expiry; canonical expiry still needs a finalized block.
pub(in crate::root) const fn swap_actions(stage: SwapStage, past_valid_to: bool) -> SwapActions {
    let none = SwapActions {
        cancel: None,
        retry: None,
        resume: None,
        recover: false,
        recover_on_destination: false,
        dismiss: false,
    };
    match stage {
        SwapStage::SubmissionPending
        | SwapStage::SubmissionRejected
        | SwapStage::Order(SwapOrderState::Open) => SwapActions {
            cancel: if past_valid_to { None } else { Some(Ok(())) },
            retry: if past_valid_to {
                Some(Err("Retry is available once the expiry is final."))
            } else {
                None
            },
            dismiss: past_valid_to,
            ..none
        },
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }) => SwapActions {
            retry: Some(Err(
                "The unshield already ran, so the order can still fill until it expires.",
            )),
            recover: true,
            dismiss: past_valid_to,
            ..none
        },
        SwapStage::Order(SwapOrderState::AttemptEnded(_)) => SwapActions {
            retry: Some(Ok(())),
            ..none
        },
        SwapStage::Order(
            SwapOrderState::PreHookOnly { expired: true }
            | SwapOrderState::NotDelivered
            | SwapOrderState::Refunding,
        ) => SwapActions {
            recover: true,
            dismiss: true,
            ..none
        },
        SwapStage::SetupNotSent
        | SwapStage::SetupFailed
        | SwapStage::Ready
        | SwapStage::Approved => SwapActions {
            resume: Some(Ok(())),
            dismiss: true,
            ..none
        },
        SwapStage::SetupRetired => SwapActions {
            resume: Some(Ok(())),
            recover: true,
            ..none
        },
        SwapStage::SetupPending => SwapActions {
            resume: Some(Ok(())),
            ..none
        },
        // The swap's own stealth account holds nothing; the destination account does.
        SwapStage::Order(SwapOrderState::HeldOnDestination) => SwapActions {
            recover_on_destination: true,
            dismiss: true,
            ..none
        },
        SwapStage::SetupSubmitting
        | SwapStage::Recovered
        | SwapStage::Order(
            SwapOrderState::Traded
            | SwapOrderState::Bridging
            | SwapOrderState::Done
            | SwapOrderState::NeedsAttention,
        ) => none,
    }
}

/// The status a swap's account shows in Stealth accounts. "Recovery needed" and "Refunding"
/// feed the Needs attention filter.
pub(in crate::root) const fn swap_account_status(stage: SwapStage) -> &'static str {
    match stage {
        SwapStage::SetupNotSent => "Setup not sent",
        SwapStage::SetupRetired => "Account retired",
        SwapStage::SetupSubmitting | SwapStage::SetupPending => "Setting up",
        SwapStage::SetupFailed => "Setup failed",
        SwapStage::Ready | SwapStage::Approved => "Set up",
        SwapStage::SubmissionPending => "Submission unconfirmed",
        SwapStage::SubmissionRejected => "Order rejected",
        SwapStage::Order(SwapOrderState::Open) => "Order open",
        SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }) => {
            "Unshielded, order open"
        }
        SwapStage::Order(SwapOrderState::Traded) => "Traded",
        SwapStage::Order(SwapOrderState::Bridging) => "Sent to the bridge",
        SwapStage::Order(SwapOrderState::Done) => "Delivered",
        SwapStage::Order(
            SwapOrderState::PreHookOnly { expired: true } | SwapOrderState::NotDelivered,
        ) => "Recovery needed",
        SwapStage::Order(SwapOrderState::Refunding) => "Refunding",
        SwapStage::Order(SwapOrderState::NeedsAttention) => "Needs attention",
        // This account holds nothing, so it isn't one that needs recovery.
        SwapStage::Order(SwapOrderState::HeldOnDestination) => "Held on destination",
        SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)) => {
            "Order expired"
        }
        SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)) => {
            "Order cancelled"
        }
        SwapStage::Order(SwapOrderState::AttemptEnded(_)) => "Order ended",
        SwapStage::Recovered => "Recovered",
    }
}

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

/// The My orders group of a swap at `stage`. A stopped setup has ended, and so has a swap that
/// was removed from the Private tab while nothing of it was in flight. Removal never ends a
/// swap that can still move on, nor one that needs recovery or attention.
pub(in crate::root) const fn swap_order_group(
    stage: SwapStage,
    stopped: bool,
    hidden: bool,
) -> SwapOrderGroup {
    if stage.needs_recovery() || stage.is_held_on_destination() {
        return SwapOrderGroup::NeedsRecovery;
    }
    if stage.needs_attention() {
        return SwapOrderGroup::NeedsAttention;
    }
    if stage.has_ended() || stopped || hidden && stage.is_dismissible() {
        SwapOrderGroup::Ended
    } else {
        SwapOrderGroup::Open
    }
}

/// The short status a swap shows in My orders.
pub(in crate::root) fn swap_order_status(
    stage: SwapStage,
    stopped: bool,
    labels: &SwapLabels,
) -> String {
    if stopped {
        return "Stopped".into();
    }
    match stage {
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
            if labels.fill_hint.is_some() =>
        {
            "Traded · confirming".into()
        }
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
            if labels.lapsed =>
        {
            "Expired · confirming".into()
        }
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false }) => {
            labels
                .expires
                .as_ref()
                .map_or_else(|| "Open".into(), |at| format!("Open · {at}"))
        }
        SwapStage::SetupNotSent => "Not set up".into(),
        SwapStage::SetupRetired => "Account retired".into(),
        SwapStage::SetupSubmitting | SwapStage::SetupPending => "Setting up".into(),
        SwapStage::SetupFailed => "Setup failed".into(),
        SwapStage::Ready | SwapStage::Approved => "Set up".into(),
        SwapStage::SubmissionPending => "Submission unconfirmed".into(),
        SwapStage::SubmissionRejected => "Rejected".into(),
        SwapStage::Order(SwapOrderState::Traded) => "Traded".into(),
        SwapStage::Order(SwapOrderState::Bridging) => "Sent to the bridge".into(),
        // An Across delivery the wallet couldn't verify is named as Across's word.
        SwapStage::Order(SwapOrderState::Done)
            if labels.bridge.as_ref().is_some_and(|bridge| {
                bridge.reported && bridge.provider == BridgeProvider::Across
            }) =>
        {
            "Delivered · reported by Across".into()
        }
        // Filled means back in the private balance; a Public address or Bridge swap was
        // delivered.
        SwapStage::Order(SwapOrderState::Done)
            if labels.receiver.is_some() || labels.bridge.is_some() =>
        {
            "Delivered".into()
        }
        SwapStage::Order(SwapOrderState::Done) => "Filled".into(),
        SwapStage::Order(
            SwapOrderState::PreHookOnly { expired: true } | SwapOrderState::NotDelivered,
        ) => "Needs recovery".into(),
        SwapStage::Order(SwapOrderState::Refunding) => "Refunding".into(),
        SwapStage::Order(SwapOrderState::NeedsAttention) => "Needs attention".into(),
        SwapStage::Order(SwapOrderState::HeldOnDestination) => labels.bridge.as_ref().map_or_else(
            || "Held on destination".into(),
            |bridge| format!("Held on {}", bridge.network),
        ),
        SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)) => {
            "Expired".into()
        }
        SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Cancellation)) => {
            "Cancelled".into()
        }
        SwapStage::Order(SwapOrderState::AttemptEnded(_)) => "Ended".into(),
        SwapStage::Recovered => "Recovered".into(),
    }
}

/// The asset a swap's recovery starts with: the sell token before a trade, the buy token after
/// one that paid the stealth account, and a bridge's refund of the bought token. Public address
/// and NEAR Intents orders pay elsewhere, so their trade leaves only the sell token to recover.
pub(in crate::root) const fn swap_recovery_token(
    stage: SwapStage,
    delivery: SwapDelivery,
    sell: Address,
    buy: Address,
) -> Address {
    match stage {
        SwapStage::Order(SwapOrderState::Traded | SwapOrderState::NotDelivered)
            if delivery.pays_executor() =>
        {
            buy
        }
        SwapStage::Order(SwapOrderState::Refunding) => buy,
        _ => sell,
    }
}

/// Next observation page for a swap: at most 64 blocks from `cursor`, ending at the confirmed
/// block. Once caught up it rereads the confirmed block, since expiry and balances are decided
/// from state at that block.
pub(in crate::root) fn swap_observation_range(cursor: u64, confirmed: u64) -> std::ops::Range<u64> {
    const MAX_OBSERVATION_BLOCKS: u64 = 64;
    let start = cursor.min(confirmed);
    start
        ..start
            .saturating_add(MAX_OBSERVATION_BLOCKS)
            .min(confirmed.saturating_add(1))
}

/// Total swap fees in buy-token base units. Convert the sell-token fees at the quote's
/// trading rate, then add the deductions from its output. `quoted_sell` must be nonzero.
pub(super) fn swap_total_cost(
    sell_fees: U256,
    quoted_sell: U256,
    quoted_output: U256,
    received: U256,
) -> U256 {
    let sell_fee_value: U512 = sell_fees.widening_mul(quoted_output);
    let sell_fee_cost = U256::saturating_from(sell_fee_value / U512::from(quoted_sell));
    quoted_output
        .saturating_sub(received)
        .saturating_add(sell_fee_cost)
}

/// Fees as a share of the input's value at the quoted trading rate. Setup is paid separately.
pub(super) fn swap_cost_bps(cost: U256, received: U256) -> u64 {
    if cost.is_zero() {
        return 0;
    }
    let input_value = U512::from(cost) + U512::from(received);
    u64::try_from(U512::from(cost) * U512::from(10_000u32) / input_value)
        .expect("a cost share cannot exceed 10,000 basis points")
}

/// Signed basis points between a quote and the anchor's expected output, positive when the
/// quote pays more. `None` when either amount is zero.
pub(in crate::root) fn quote_anchor_delta_bps(quoted: U256, expected: U256) -> Option<i64> {
    if quoted.is_zero() || expected.is_zero() {
        return None;
    }
    let (difference, better) = if quoted >= expected {
        (quoted - expected, true)
    } else {
        (expected - quoted, false)
    };
    let bps = i64::try_from(difference.saturating_mul(U256::from(10_000u32)) / expected).ok()?;
    Some(if better { bps } else { -bps })
}

/// "0.4%" for 40 basis points, "3%" for 300.
pub(in crate::root) fn format_bps_percent(bps: u64) -> String {
    let whole = bps / 100;
    let fraction = bps % 100;
    if fraction == 0 {
        format!("{whole}%")
    } else if fraction.is_multiple_of(10) {
        format!("{whole}.{}%", fraction / 10)
    } else {
        format!("{whole}.{fraction:02}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> SwapLabels {
        SwapLabels {
            pair: "1.00 WETH for USDC".into(),
            sell: "1.00 WETH".into(),
            buy_symbol: "USDC".into(),
            expires: Some("14:32".into()),
            lapsed: false,
            fill_hint: None,
            received: None,
            receiver: None,
            minimum: None,
            uncovered_gas: None,
            bridge: None,
        }
    }

    const ENDED: [SwapPreHookDeathCause; 5] = [
        SwapPreHookDeathCause::Expired,
        SwapPreHookDeathCause::Cancellation,
        SwapPreHookDeathCause::Recovery,
        SwapPreHookDeathCause::OlderPostHook,
        SwapPreHookDeathCause::Unknown,
    ];

    fn every_stage() -> Vec<SwapStage> {
        let mut stages = vec![
            SwapStage::SetupNotSent,
            SwapStage::SetupRetired,
            SwapStage::SetupSubmitting,
            SwapStage::SetupPending,
            SwapStage::SetupFailed,
            SwapStage::Ready,
            SwapStage::Approved,
            SwapStage::SubmissionPending,
            SwapStage::SubmissionRejected,
            SwapStage::Recovered,
            SwapStage::Order(SwapOrderState::Open),
            SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }),
            SwapStage::Order(SwapOrderState::PreHookOnly { expired: true }),
            SwapStage::Order(SwapOrderState::Traded),
            SwapStage::Order(SwapOrderState::Bridging),
            SwapStage::Order(SwapOrderState::Done),
            SwapStage::Order(SwapOrderState::NotDelivered),
            SwapStage::Order(SwapOrderState::Refunding),
            SwapStage::Order(SwapOrderState::NeedsAttention),
            SwapStage::Order(SwapOrderState::HeldOnDestination),
        ];
        stages.extend(
            ENDED
                .into_iter()
                .map(|cause| SwapStage::Order(SwapOrderState::AttemptEnded(cause))),
        );
        stages
    }

    #[test]
    fn progress_offers_only_the_actions_the_order_state_allows() {
        for stage in every_stage() {
            let actions = swap_actions(stage, false);
            // Cancel only while the order is open and its pre-hook hasn't run.
            assert_eq!(
                actions.cancel.is_some(),
                matches!(
                    stage,
                    SwapStage::Order(SwapOrderState::Open)
                        | SwapStage::SubmissionPending
                        | SwapStage::SubmissionRejected
                ),
                "{stage:?}"
            );
            // Recovery for stranded or refunded funds, or a retired account that may hold funds.
            // A bridge deposit that needs attention isn't in the account to recover.
            assert_eq!(
                actions.recover,
                matches!(
                    stage,
                    SwapStage::SetupRetired
                        | SwapStage::Order(
                            SwapOrderState::PreHookOnly { .. }
                                | SwapOrderState::NotDelivered
                                | SwapOrderState::Refunding
                        )
                ),
                "{stage:?}"
            );
            // Held proceeds are recovered on the destination network, not from this account.
            assert_eq!(
                actions.recover_on_destination,
                stage.is_held_on_destination(),
                "{stage:?}"
            );
            // A retry is admitted only after the attempt ended; an executed pre-hook blocks it
            // with a reason.
            match stage {
                SwapStage::Order(SwapOrderState::AttemptEnded(_)) => {
                    assert_eq!(actions.retry, Some(Ok(())));
                }
                SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }) => {
                    assert!(matches!(actions.retry, Some(Err(_))));
                }
                _ => assert!(actions.retry.is_none(), "{stage:?}"),
            }
            // Only a stalled swap, one that can't move on by itself but hasn't ended, can be
            // dismissed.
            assert_eq!(
                actions.dismiss,
                stage.is_dismissible() && !stage.has_ended(),
                "{stage:?}"
            );
            let ongoing = matches!(
                stage,
                SwapStage::SetupSubmitting
                    | SwapStage::SetupPending
                    | SwapStage::SubmissionPending
                    | SwapStage::SubmissionRejected
                    | SwapStage::Order(
                        SwapOrderState::Open
                            | SwapOrderState::Traded
                            | SwapOrderState::Bridging
                            | SwapOrderState::NeedsAttention
                            | SwapOrderState::PreHookOnly { expired: false }
                    )
            );
            assert_eq!(
                stage.is_shown_on_private_tab(true, false),
                ongoing,
                "{stage:?}"
            );
            // Ended swaps leave the Private tab by themselves; the rest stay until dismissed.
            assert_eq!(
                stage.is_shown_on_private_tab(false, false),
                !stage.has_ended(),
                "{stage:?}"
            );
            let needs_recovery = matches!(
                stage,
                SwapStage::Order(
                    SwapOrderState::NotDelivered
                        | SwapOrderState::Refunding
                        | SwapOrderState::PreHookOnly { expired: true }
                )
            );
            assert_eq!(stage.needs_recovery(), needs_recovery, "{stage:?}");
            // A bridge deposit that needs attention asks the user to act, though not to recover.
            // Held proceeds ask for a recovery on the destination network.
            let needs_attention = needs_recovery
                || matches!(
                    stage,
                    SwapStage::Order(
                        SwapOrderState::NeedsAttention | SwapOrderState::HeldOnDestination
                    )
                );
            assert_eq!(stage.needs_attention(), needs_attention, "{stage:?}");
            assert_eq!(
                swap_card_line(stage, &labels()).attention,
                needs_attention,
                "{stage:?}"
            );
            if matches!(
                stage,
                SwapStage::SubmissionPending | SwapStage::SubmissionRejected
            ) {
                assert_eq!(
                    swap_steps(stage, &labels())[1].status,
                    PublicActionStepStatus::Warning
                );
            }
        }
        assert!(
            swaps_card_line(&[
                (SwapStage::Order(SwapOrderState::Open), labels()),
                (SwapStage::Order(SwapOrderState::NotDelivered), labels()),
            ])
            .unwrap()
            .attention
        );
    }

    #[test]
    fn orders_split_into_swaps_at_a_new_pair_or_after_a_trade() {
        let (weth, usdc, dai) = (
            Address::repeat_byte(1),
            Address::repeat_byte(2),
            Address::repeat_byte(3),
        );
        let reshield = SwapDelivery::Reshield;
        let external = SwapDelivery::External {
            receiver: Address::repeat_byte(4),
        };
        // An order-less record has no ranges; it is one swap without orders.
        assert!(swap_order_ranges(std::iter::empty()).is_empty());
        assert_eq!(
            swap_order_ranges([
                // Expired, then retried with the same pair: one swap.
                (weth, usdc, reshield, false),
                (weth, usdc, reshield, true),
                // After the trade, the same pair on the reused account is a new swap.
                (weth, usdc, reshield, false),
                // A different pair is a new swap even after an attempt without a trade.
                (dai, usdc, reshield, false),
                // So is another delivery for the same pair.
                (dai, usdc, external, false),
            ]),
            [0..2, 2..3, 3..4, 4..5]
        );
    }

    #[test]
    fn my_orders_groups_follow_what_the_swap_can_still_do() {
        use SwapOrderGroup::{Ended, NeedsAttention, NeedsRecovery, Open};
        for (stage, group) in [
            (SwapStage::SetupPending, Open),
            (SwapStage::Approved, Open),
            (SwapStage::SubmissionPending, Open),
            (SwapStage::Order(SwapOrderState::Open), Open),
            (
                SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }),
                Open,
            ),
            // Traded but not yet back in the private balance, or not yet delivered by the bridge.
            (SwapStage::Order(SwapOrderState::Traded), Open),
            (SwapStage::Order(SwapOrderState::Bridging), Open),
            // The provider has to resolve it, so it isn't grouped with recoverable swaps.
            (
                SwapStage::Order(SwapOrderState::NeedsAttention),
                NeedsAttention,
            ),
            (
                SwapStage::Order(SwapOrderState::PreHookOnly { expired: true }),
                NeedsRecovery,
            ),
            (
                SwapStage::Order(SwapOrderState::NotDelivered),
                NeedsRecovery,
            ),
            (SwapStage::Order(SwapOrderState::Refunding), NeedsRecovery),
            // The wallet can recover held proceeds, on the destination network.
            (
                SwapStage::Order(SwapOrderState::HeldOnDestination),
                NeedsRecovery,
            ),
            (SwapStage::Order(SwapOrderState::Done), Ended),
            (
                SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)),
                Ended,
            ),
            (
                SwapStage::Order(SwapOrderState::AttemptEnded(
                    SwapPreHookDeathCause::Cancellation,
                )),
                Ended,
            ),
            (SwapStage::Recovered, Ended),
            (SwapStage::SetupRetired, Ended),
        ] {
            assert_eq!(swap_order_group(stage, false, false), group, "{stage:?}");
        }
        // A stopped setup has ended even while its sent setup may still confirm.
        assert_eq!(
            swap_order_group(SwapStage::SetupPending, true, false),
            Ended
        );
        for stage in every_stage() {
            // Removing a swap from the Private tab ends only swaps that can't move on by
            // themselves. It changes nothing for swaps in flight or needing recovery.
            let shown = swap_order_group(stage, false, false);
            let hidden = swap_order_group(stage, false, true);
            if stage.is_dismissible() && shown != NeedsRecovery {
                assert_eq!(hidden, Ended, "{stage:?}");
            } else {
                assert_eq!(hidden, shown, "{stage:?}");
            }
        }
    }

    #[test]
    fn a_new_swap_is_quoted_before_setup_and_an_approved_one_waits_for_its_setup() {
        // The quote, the price check and the minimum come before any setup is paid, and the
        // review covers the setup. A setup sent again is reviewed the same way.
        assert_eq!(swap_form_mode(None), SwapFormMode::Setup { resume: false });
        for stage in [
            SwapStage::SetupNotSent,
            SwapStage::SetupFailed,
            SwapStage::SetupPending,
        ] {
            assert_eq!(
                swap_form_mode(Some(stage)),
                SwapFormMode::Setup { resume: true }
            );
        }
        // Pending setup can be retried with another broadcaster after the local job ends.
        assert_eq!(
            swap_form_mode(Some(SwapStage::SetupSubmitting)),
            SwapFormMode::SettingUp
        );
        assert_eq!(
            swap_actions(SwapStage::SetupPending, false).resume,
            Some(Ok(()))
        );
        assert!(SwapStage::SetupPending.is_observed());
        // Once the setup is confirmed, the approved order is placed through a confirm-only
        // step, and nothing is left to observe until it exists.
        let approved = swap_actions(SwapStage::Approved, false);
        assert_eq!(approved.resume, Some(Ok(())));
        assert!(approved.retry.is_none() && approved.cancel.is_none());
        assert!(!SwapStage::Approved.is_observed());
        // Changed terms, an amount that no longer fits, and retries go through the form
        // without a second setup.
        for stage in [
            SwapStage::Approved,
            SwapStage::Ready,
            SwapStage::Order(SwapOrderState::AttemptEnded(SwapPreHookDeathCause::Expired)),
        ] {
            assert_eq!(
                swap_form_mode(Some(stage)),
                SwapFormMode::Order,
                "{stage:?}"
            );
        }
        for stage in [
            SwapStage::Order(SwapOrderState::Open),
            SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }),
            SwapStage::Recovered,
        ] {
            assert_eq!(
                swap_form_mode(Some(stage)),
                SwapFormMode::Placed,
                "{stage:?}"
            );
        }
    }

    #[test]
    fn recovery_starts_with_the_token_the_stealth_account_holds() {
        use wallet_ops::vault::{BridgeDelivery, BridgeProvider, BridgeSurplus};
        let (sell, buy) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let bridge = |provider, surplus| {
            SwapDelivery::Bridge(BridgeDelivery {
                provider,
                destination_chain: 42161,
                receiver: Address::repeat_byte(3),
                destination_token: Address::repeat_byte(4),
                surplus,
                private: None,
            })
        };
        let across = bridge(BridgeProvider::Across, BridgeSurplus::KeepInAccount);
        let near = bridge(
            BridgeProvider::NearIntents,
            BridgeSurplus::BridgedByProvider,
        );
        let order = SwapStage::Order;
        for (stage, delivery, token) in [
            (
                order(SwapOrderState::PreHookOnly { expired: true }),
                across,
                sell,
            ),
            // An Across post-hook that didn't run leaves the bought token in the account.
            (order(SwapOrderState::Traded), across, buy),
            (order(SwapOrderState::NotDelivered), across, buy),
            // NEAR Intents' trade pays its deposit address.
            (order(SwapOrderState::Traded), near, sell),
            // Either bridge refunds the bought token to the account.
            (order(SwapOrderState::Refunding), across, buy),
            (order(SwapOrderState::Refunding), near, buy),
        ] {
            assert_eq!(
                swap_recovery_token(stage, delivery, sell, buy),
                token,
                "{stage:?} {delivery:?}"
            );
        }
    }

    #[test]
    fn local_expiry_hides_cancel_and_waits_for_a_final_expiry_before_retry() {
        let actions = swap_actions(SwapStage::Order(SwapOrderState::Open), true);
        assert!(actions.cancel.is_none());
        assert!(matches!(actions.retry, Some(Err(_))));
    }

    #[test]
    fn done_requires_delivery_not_only_the_trade() {
        let traded = swap_steps(SwapStage::Order(SwapOrderState::Traded), &labels());
        assert_eq!(
            traded.last().map(|step| step.status),
            Some(PublicActionStepStatus::Pending)
        );
        let done = swap_steps(SwapStage::Order(SwapOrderState::Done), &labels());
        assert!(
            done.iter()
                .all(|step| step.status == PublicActionStepStatus::Done)
        );
        let undelivered = swap_steps(SwapStage::Order(SwapOrderState::NotDelivered), &labels());
        assert_ne!(
            undelivered.last().map(|step| step.status),
            Some(PublicActionStepStatus::Done)
        );
    }

    #[test]
    fn bridge_steps_follow_the_hand_off_then_the_destination_outcome() {
        use PublicActionStepStatus::{Done, NotStarted, Pending, Warning};
        let bridged = |provider, reported| SwapLabels {
            received: Some("248.71 USDC".into()),
            bridge: Some(SwapBridgeLabels {
                provider,
                network: "Polygon".into(),
                token: "USDC".into(),
                origin: "Arbitrum One".into(),
                receiver: "Treasury".into(),
                sent: Some("248.82 USDC".into()),
                minimum: Some("248.70 USDC".into()),
                private: None,
                reported,
            }),
            ..labels()
        };
        let (across, across_reported, near) = (
            bridged(BridgeProvider::Across, false),
            bridged(BridgeProvider::Across, true),
            bridged(BridgeProvider::NearIntents, true),
        );
        let order = SwapStage::Order;
        for (stage, labels, steps) in [
            // Before the trade, the hand-off and the delivery wait for it.
            (
                order(SwapOrderState::Open),
                &across,
                vec![
                    ("Order open", Pending),
                    ("Bridge deposit", NotStarted),
                    ("Delivered on Polygon", NotStarted),
                ],
            ),
            (
                order(SwapOrderState::Bridging),
                &across,
                vec![
                    ("Order open", Done),
                    ("Bridge deposit", Done),
                    ("Delivered on Polygon", Pending),
                ],
            ),
            (
                order(SwapOrderState::Done),
                &across,
                vec![
                    ("Order open", Done),
                    ("Bridge deposit", Done),
                    ("Delivered on Polygon · verified", Done),
                ],
            ),
            (
                order(SwapOrderState::Done),
                &near,
                vec![
                    ("Order open", Done),
                    ("Bridge deposit", Done),
                    ("Delivered on Polygon · reported by NEAR Intents", Done),
                ],
            ),
            // Refunding and Needs attention replace the delivered step.
            (
                order(SwapOrderState::Refunding),
                &across,
                vec![
                    ("Order open", Done),
                    ("Bridge deposit", Done),
                    ("Refunding on Arbitrum One", Warning),
                ],
            ),
            (
                order(SwapOrderState::NeedsAttention),
                &near,
                vec![
                    ("Order open", Done),
                    ("Bridge deposit", Done),
                    ("Needs attention", Warning),
                ],
            ),
            // An Across post-hook that didn't run left the bought token on the swap's network.
            (
                order(SwapOrderState::NotDelivered),
                &across,
                vec![("Order open", Done), ("Not sent to the bridge", Warning)],
            ),
        ] {
            let shown = swap_steps(stage, labels);
            assert_eq!(
                shown[1..]
                    .iter()
                    .map(|step| (step.label.as_str(), step.status))
                    .collect::<Vec<_>>(),
                steps,
                "{stage:?}"
            );
            // Nothing of a Bridge swap returns to the private balance by itself.
            let card = swap_card_line(stage, labels);
            assert!(
                shown
                    .iter()
                    .flat_map(|step| [&step.label, &step.detail])
                    .chain([&card.title, &card.detail])
                    .all(|text| !text.contains("private balance")),
                "{stage:?}"
            );
        }
        let pending = swap_steps(order(SwapOrderState::Bridging), &near);
        assert_eq!(pending[2].detail, "248.82 USDC to 1Click");
        assert!(
            pending[3]
                .detail
                .starts_with("At least 248.70 USDC to Treasury")
        );
        assert_eq!(
            swap_card_line(order(SwapOrderState::Done), &across).detail,
            "Delivered 248.71 USDC to Treasury on Polygon via Across"
        );
        assert_eq!(
            swap_card_line(order(SwapOrderState::Done), &across_reported).detail,
            "Delivered 248.71 USDC to Treasury on Polygon · reported by Across"
        );
        // A refund returns to the stealth account on the swap's own network.
        assert!(
            swap_card_line(order(SwapOrderState::Refunding), &across)
                .detail
                .contains("248.82 USDC returns to the stealth account on Arbitrum One")
        );
        // Needs attention is its own condition, which the card names, not a recovery.
        let attention = swap_card_line(order(SwapOrderState::NeedsAttention), &near);
        assert!(attention.attention && attention.detail.contains("Treasury on Polygon"));
        assert_eq!(
            swap_order_status(order(SwapOrderState::Done), false, &near),
            "Delivered"
        );
    }

    #[test]
    fn private_bridge_steps_follow_both_setups_then_the_shield_on_the_destination() {
        use PublicActionStepStatus::{Done, Error, NotStarted, Pending, Warning};
        use SwapSetupProgress as Setup;
        const RECEIVER: &str = "0x9a9A…9a9A";
        let destination = Address::repeat_byte(0x9a);
        let private = |origin: Setup, arrival: Setup| {
            let setup = |network: &str, index, address, progress, block| SwapSetupLabels {
                network: network.into(),
                account: Some(SwapStepAccount {
                    index: Some(index),
                    address,
                }),
                progress,
                reused: false,
                block: (progress == Setup::Done).then_some(block),
                detail: None,
            };
            SwapLabels {
                received: Some("990.26 USDC".into()),
                bridge: Some(SwapBridgeLabels {
                    provider: BridgeProvider::Across,
                    network: "Arbitrum One".into(),
                    token: "USDC".into(),
                    origin: "Ethereum".into(),
                    receiver: RECEIVER.into(),
                    sent: Some("993.05 USDC".into()),
                    minimum: Some("990.12 USDC".into()),
                    private: Some(SwapPrivateBridgeLabels {
                        setups: [
                            setup(
                                "Ethereum",
                                191,
                                Address::repeat_byte(0x4e),
                                origin,
                                23_481_902,
                            ),
                            setup("Arbitrum One", 57, destination, arrival, 402_118_977),
                        ],
                        held: Some("992.74 USDC".into()),
                    }),
                    reported: false,
                }),
                ..labels()
            }
        };
        let children = |step: &SwapStep| {
            step.children
                .iter()
                .map(|child| (child.label.clone(), child.status, child.detail.clone()))
                .collect::<Vec<_>>()
        };
        let child =
            |network: &str, status, detail: &str| (network.to_owned(), status, detail.to_owned());

        // One setup confirmed, one on its way: the step is in progress and counts on its
        // sub-steps, each with its own account.
        let pending = swap_steps(
            SwapStage::SetupPending,
            &private(Setup::Done, Setup::Pending),
        );
        assert_eq!(
            pending
                .iter()
                .map(|step| (step.label.as_str(), step.status))
                .collect::<Vec<_>>(),
            [
                ("Stealth accounts set up", Pending),
                ("Order open", NotStarted),
                ("Bridge deposit", NotStarted),
                ("Private on Arbitrum One", NotStarted),
            ]
        );
        assert_eq!(
            children(&pending[0]),
            [
                child("Ethereum", Done, "block 23481902"),
                child("Arbitrum One", Pending, "Waiting for broadcaster…"),
            ]
        );
        assert_eq!(
            pending[0].children[1].account,
            Some(SwapStepAccount {
                index: Some(57),
                address: destination
            })
        );
        // The parent stays in progress while either network still needs its setup.
        let step = |stage, origin, arrival| swap_steps(stage, &private(origin, arrival)).remove(0);
        let loading = step(SwapStage::SetupPending, Setup::Done, Setup::NetworkLoading);
        assert_eq!(loading.status, Pending);
        assert_eq!(
            loading.children[1].detail,
            "Waiting for Arbitrum One to load…"
        );
        assert_eq!(
            step(SwapStage::SetupNotSent, Setup::NotSent, Setup::NotSent).status,
            Pending
        );

        // One failed setup fails the step, whatever the other did, and no order follows.
        let failed = swap_steps(SwapStage::SetupFailed, &private(Setup::Done, Setup::Failed));
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0].status, Error);
        assert!(failed[0].detail.contains("Arbitrum One"));
        assert_eq!(
            children(&failed[0]),
            [
                child("Ethereum", Done, "block 23481902"),
                child("Arbitrum One", Error, "Not confirmed"),
            ]
        );
        assert_eq!(
            step(SwapStage::SetupFailed, Setup::Failed, Setup::Pending).status,
            Error
        );

        // An account the swap reuses is ready without a setup of its own. Only the new
        // account's setup shows as pending, and the step waits for it.
        let reuse = |mut labels: SwapLabels, origin: bool, arrival: bool| {
            let bridge = labels.bridge.as_mut().unwrap();
            let setups = &mut bridge.private.as_mut().unwrap().setups;
            setups[0].reused = origin;
            setups[1].reused = arrival;
            labels
        };
        let mixed = swap_steps(
            SwapStage::SetupPending,
            &reuse(private(Setup::Done, Setup::Pending), true, false),
        );
        assert_eq!(
            (mixed[0].label.as_str(), mixed[0].status),
            ("Stealth accounts set up", Pending)
        );
        assert_eq!(
            children(&mixed[0]),
            [
                child("Ethereum", Done, "Reused · ready"),
                child("Arbitrum One", Pending, "Waiting for broadcaster…"),
            ]
        );
        // With both reused nothing is set up: the step is done and the order is next.
        let both = swap_steps(
            SwapStage::Ready,
            &reuse(private(Setup::Done, Setup::Done), true, true),
        );
        assert_eq!(
            both.iter()
                .take(2)
                .map(|step| (step.label.as_str(), step.status))
                .collect::<Vec<_>>(),
            [("Stealth accounts ready", Done), ("Order open", NotStarted)]
        );
        assert_eq!(
            children(&both[0]),
            [
                child("Ethereum", Done, "Reused · ready"),
                child("Arbitrum One", Done, "Reused · ready"),
            ]
        );
        // A reused account whose network isn't loaded shows no pending marker.
        let unloaded = swap_steps(
            SwapStage::Ready,
            &reuse(private(Setup::Done, Setup::NetworkLoading), true, true),
        );
        assert_eq!(
            children(&unloaded[0])[1],
            child(
                "Arbitrum One",
                NotStarted,
                "Reused · checked once Arbitrum One loads"
            )
        );

        // With both set up, the last step is the shield on the destination network, or what
        // replaced it.
        let set_up = private(Setup::Done, Setup::Done);
        let order = SwapStage::Order;
        for (state, last, detail) in [
            (
                SwapOrderState::Bridging,
                ("Private on Arbitrum One", Pending),
                "990.12 USDC to your private balance",
            ),
            (
                SwapOrderState::Done,
                ("Private on Arbitrum One · verified", Done),
                "990.26 USDC shielded to your private balance",
            ),
            (
                SwapOrderState::HeldOnDestination,
                ("Held by stealth account on Arbitrum One", Warning),
                "Across delivered 992.74 USDC, but the shield didn't run. The USDC is in stealth account #57 on Arbitrum One.",
            ),
            (
                SwapOrderState::Refunding,
                ("Refunding on Ethereum", Warning),
                "No relayer filled the deposit",
            ),
        ] {
            let steps = swap_steps(order(state), &set_up);
            assert_eq!(steps[0].status, Done, "{state:?}");
            assert_eq!(
                children(&steps[0]),
                [
                    child("Ethereum", Done, "block 23481902"),
                    child("Arbitrum One", Done, "block 402118977"),
                ],
                "{state:?}"
            );
            let shown = steps.last().unwrap();
            assert_eq!((shown.label.as_str(), shown.status), last, "{state:?}");
            assert!(shown.detail.starts_with(detail), "{}", shown.detail);
        }
        assert_eq!(
            swap_card_line(order(SwapOrderState::Bridging), &set_up).detail,
            "Sent to Across · shielding to your private balance on Arbitrum One"
        );
        // Held proceeds are the wallet's to recover, on the destination network.
        let held = order(SwapOrderState::HeldOnDestination);
        for hidden in [false, true] {
            assert_eq!(
                swap_order_group(held, false, hidden),
                SwapOrderGroup::NeedsRecovery
            );
        }
        assert_eq!(
            swap_order_status(held, false, &set_up),
            "Held on Arbitrum One"
        );
        let card = swap_card_line(held, &set_up);
        assert!(card.attention && card.detail.contains("992.74 USDC"));

        // The destination stealth account is never named as a receiver.
        for stage in every_stage() {
            let steps = swap_steps(stage, &set_up);
            let card = swap_card_line(stage, &set_up);
            assert!(
                steps
                    .iter()
                    .flat_map(|step| std::iter::once(step).chain(&step.children))
                    .flat_map(|step| [&step.label, &step.detail])
                    .chain([&card.title, &card.detail])
                    .all(|text| !text.contains(RECEIVER)),
                "{stage:?}"
            );
        }
    }

    #[test]
    fn a_reported_fill_shows_the_trade_confirming_until_observation_records_it() {
        use PublicActionStepStatus::{Done, NotStarted, Pending};
        // The orderbook reports the fill 5 blocks after the trade, and even past the local
        // expiry, since a fill can land up to validTo.
        let mut hinted = SwapLabels {
            fill_hint: Some(SwapFillHint {
                confirmations: Some(5),
                depth: 12,
            }),
            lapsed: true,
            ..labels()
        };
        for stage in [
            SwapStage::Order(SwapOrderState::Open),
            SwapStage::Order(SwapOrderState::PreHookOnly { expired: false }),
        ] {
            for (confirmations, detail) in [
                (5, "Confirming (5/12)"),
                (12, "Verifying settlement…"),
                (20, "Verifying settlement…"),
            ] {
                hinted.fill_hint.as_mut().unwrap().confirmations = Some(confirmations);
                let steps = swap_steps(stage, &hinted);
                assert_eq!(
                    steps.iter().map(|step| step.status).collect::<Vec<_>>(),
                    [Done, Done, Pending, NotStarted],
                    "the reported fill stays pending until canonical verification: {stage:?}"
                );
                assert_eq!(
                    (steps[2].label.as_str(), steps[2].detail.as_str()),
                    ("Traded", detail)
                );
            }
            assert_eq!(swap_card_line(stage, &hinted).detail, "Traded · confirming");
        }
    }

    #[test]
    fn a_traded_order_reports_its_outcome_against_the_approved_bounds() {
        use alloy::eips::BlockNumHash;
        use wallet_ops::vault::SwapObservation;
        // The first mainnet fill: 50 USDT spent privately, 49.875 USDT sold after the unshield
        // fee, for 49.4041 DAI against a 42.6174 DAI minimum.
        let bounds = SwapApprovedBounds {
            sell_amount: U256::from(49_875_000u64),
            unshield_amount: Some(U256::from(50_000_000u64)),
            unshield_fee_bps: U256::from(25u8),
            buy_amount: U256::from(42_617_400_000_000_000_000u128),
            private_minimum: U256::from(42_510_856_500_000_000_000u128),
            shield_fee_bps: U256::from(25u8),
            slippage_bps: 50,
            pre_hook_gas_limit: 900_000,
            post_hook_gas_limit: Some(300_000),
            hook_cost: None,
            anchors: Vec::new(),
            destination_minimum: None,
            gas_share_bps: None,
            gas_estimate: None,
            gas_allowance: None,
            gas_price_wei: None,
            valid_for_secs: None,
            destination_shield_fee_bps: None,
            delivery_allowance: None,
            destination_setup_fee: None,
            source_setup_fee: None,
        };
        let settlement = B256::repeat_byte(0x51);
        let observed = SwapOrderObservations {
            traded: Some(SwapObservation {
                block: BlockNumHash::new(26_063_930, B256::repeat_byte(1)),
                transaction_hash: Some(settlement),
            }),
            trade_amounts: Some(SwapTradeAmounts {
                sell_amount: U256::from(49_875_000u64),
                buy_amount: U256::from(49_404_100_000_000_000_000u128),
                fee_amount: U256::ZERO,
                settlement_gas_used: None,
                settlement_effective_gas_price: None,
                executed_fee: None,
                executed_fee_token: None,
            }),
            ..SwapOrderObservations::default()
        };
        let reshield = SwapDelivery::Reshield;
        let outcome = swap_outcome(&bounds, &observed, reshield).expect("traded");
        assert_eq!(
            (outcome.spent, outcome.unshield_fee),
            (U256::from(50_000_000u64), U256::from(125_000u64))
        );
        assert_eq!(outcome.settlement, Some(settlement));
        // Private delivery is measured by its credit, which isn't recorded yet.
        assert_eq!((outcome.received, outcome.above_minimum), (None, None));
        // A Public address receiver got the whole trade: 6.8932435 DAI above its 42.5108565
        // DAI minimum.
        let external = SwapDelivery::External {
            receiver: Address::repeat_byte(7),
        };
        let paid = swap_outcome(&bounds, &observed, external).expect("traded");
        assert_eq!(
            (paid.private_minimum, paid.received, paid.above_minimum),
            (
                bounds.private_minimum,
                Some(U256::from(49_404_100_000_000_000_000u128)),
                Some(U256::from(6_893_243_500_000_000_000u128))
            )
        );
        // The credit after the shield fee, 49.28058975 DAI, against the same minimum.
        let credited = swap_outcome(
            &bounds,
            &SwapOrderObservations {
                shielded: Some(
                    serde_json::from_value(serde_json::json!({
                        "observation": observed.traded.expect("traded"),
                        "private_amount": U256::from(49_280_589_750_000_000_000u128),
                        "fee": U256::from(123_510_250_000_000_000u128),
                    }))
                    .unwrap(),
                ),
                ..observed
            },
            reshield,
        )
        .expect("traded");
        assert_eq!(
            (credited.received, credited.above_minimum),
            (
                Some(U256::from(49_280_589_750_000_000_000u128)),
                Some(U256::from(6_769_733_250_000_000_000u128))
            )
        );
        // The fee row needs the orderbook's fee and the settlement receipt's cost, all four
        // figures; any one missing leaves it out.
        let dai = Address::repeat_byte(0xda);
        let charged = SwapTradeAmounts {
            settlement_gas_used: Some(250_000),
            settlement_effective_gas_price: Some(2_000_000_000),
            executed_fee: Some(U256::from(100_000_000_000_000_000u128)),
            executed_fee_token: Some(dai),
            ..observed.trade_amounts.expect("amounts")
        };
        let gas = |amounts| {
            swap_outcome(
                &bounds,
                &SwapOrderObservations {
                    trade_amounts: Some(amounts),
                    ..observed
                },
                reshield,
            )
            .expect("traded")
            .gas
        };
        assert_eq!(
            gas(charged),
            Some(SwapOutcomeGas {
                fee: U256::from(100_000_000_000_000_000u128),
                fee_token: dai,
                settlement_cost: U256::from(500_000_000_000_000u128),
            })
        );
        for partial in [
            SwapTradeAmounts {
                executed_fee: None,
                ..charged
            },
            SwapTradeAmounts {
                executed_fee_token: None,
                ..charged
            },
            SwapTradeAmounts {
                settlement_gas_used: None,
                ..charged
            },
            SwapTradeAmounts {
                settlement_effective_gas_price: None,
                ..charged
            },
        ] {
            assert_eq!(gas(partial), None);
        }
        // The shield fee is the one its event charged, not derived from the approved rate
        // (997_899 at 25 bps would give 2_501). A shield recorded without one shows no fee.
        let received = |fee: Option<U256>| {
            let shield: wallet_ops::vault::SwapShieldObservation =
                serde_json::from_value(serde_json::json!({
                    "observation": observed.traded.expect("traded"),
                    "private_amount": U256::from(997_899u64),
                    "fee": fee,
                }))
                .unwrap();
            swap_outcome(
                &bounds,
                &SwapOrderObservations {
                    shielded: Some(shield),
                    ..observed
                },
                reshield,
            )
            .expect("traded")
            .received_privately
        };
        assert_eq!(
            received(Some(U256::from(2_500u64))),
            Some((U256::from(997_899u64), Some(U256::from(2_500u64))))
        );
        assert_eq!(received(None), Some((U256::from(997_899u64), None)));
        // A record from before amounts were kept still shows the approved limit.
        let legacy = swap_outcome(
            &bounds,
            &SwapOrderObservations {
                trade_amounts: None,
                ..observed
            },
            external,
        )
        .expect("traded");
        assert_eq!(
            (legacy.trade, legacy.received, legacy.gas),
            (None, None, None)
        );
        assert_eq!(legacy.minimum, bounds.buy_amount);
        assert_eq!(
            swap_outcome(&bounds, &SwapOrderObservations::default(), reshield),
            None
        );
    }

    #[test]
    fn nothing_unshielded_is_claimed_only_for_attempts_whose_pre_hook_never_ran() {
        for stage in every_stage() {
            let claims = swap_card_line(stage, &labels())
                .detail
                .contains("Nothing was unshielded")
                || swap_steps(stage, &labels())
                    .iter()
                    .any(|step| step.detail.contains("Nothing was unshielded"));
            let never_ran = matches!(
                stage,
                SwapStage::SetupFailed | SwapStage::Order(SwapOrderState::AttemptEnded(_))
            );
            assert_eq!(claims, never_ran, "{stage:?}");
        }
    }

    #[test]
    fn a_recorded_setup_outcome_stands_in_for_an_unobserved_session() {
        use alloy::primitives::B256;
        let executed = Some(ExecutorPayloadStatus::Executed);
        let reverted = Some(ExecutorPayloadStatus::Reverted);
        let lost = Some(ExecutorPayloadStatus::Invalidated { winner: B256::ZERO });
        assert_eq!(recorded_setup_stage(&[], false), SwapStage::SetupNotSent);
        assert_eq!(
            recorded_setup_stage(&[reverted, executed], false),
            SwapStage::Ready
        );
        // This session confirms the delegation itself before offering the order.
        assert_eq!(
            recorded_setup_stage(&[executed], true),
            SwapStage::SetupPending
        );
        assert_eq!(
            recorded_setup_stage(&[reverted, lost], false),
            SwapStage::SetupFailed
        );
        assert_eq!(
            recorded_setup_stage(&[reverted, None], false),
            SwapStage::SetupPending
        );
    }

    #[test]
    fn observation_pages_stay_within_the_confirmed_block_and_the_page_limit() {
        assert_eq!(swap_observation_range(100, 1_000), 100..164);
        assert_eq!(swap_observation_range(990, 1_000), 990..1_001);
        // Caught up, or a cursor past a lagging head: reread the confirmed block.
        assert_eq!(swap_observation_range(1_001, 1_000), 1_000..1_001);
        assert_eq!(swap_observation_range(5_000, 1_000), 1_000..1_001);
    }

    #[test]
    fn total_cost_includes_cow_and_railgun_fees_across_token_decimals() {
        // Spend 1 WETH: 0.0025 WETH to unshield and 0.1 WETH to CoW. The remaining
        // 0.8975 WETH trades at 2,500 USDC/WETH, paying 2,243.75 USDC. Subtracting
        // 10 USDC for hooks and 5.584375 USDC to shield leaves 2,228.165625 USDC.
        let received = U256::from(2_228_165_625u64);
        let cost = swap_total_cost(
            U256::from(102_500_000_000_000_000u64),
            U256::from(897_500_000_000_000_000u64),
            U256::from(2_243_750_000u64),
            received,
        );
        assert_eq!(cost, U256::from(271_834_375u64));
        assert_eq!(cost + received, U256::from(2_500_000_000u64));
        assert_eq!(swap_cost_bps(cost, received), 1_087);
        // The percentage is a share of the input, not of the smaller amount received.
        assert_eq!(swap_cost_bps(U256::from(999), U256::from(9_001)), 999);
        assert_eq!(swap_cost_bps(U256::from(1_000), U256::from(9_000)), 1_000);
        // Large token supplies must not overflow while converting the ratio to basis points.
        assert_eq!(swap_cost_bps(U256::MAX, U256::MAX), 5_000);
    }

    #[test]
    fn price_check_delta_is_signed_and_formatted_in_percent() {
        let expected = U256::from(10_000u32);
        assert_eq!(
            quote_anchor_delta_bps(U256::from(9_960u32), expected),
            Some(-40)
        );
        assert_eq!(
            quote_anchor_delta_bps(U256::from(10_125u32), expected),
            Some(125)
        );
        assert_eq!(quote_anchor_delta_bps(U256::ZERO, expected), None);
        assert_eq!(format_bps_percent(40), "0.4%");
        assert_eq!(format_bps_percent(125), "1.25%");
        assert_eq!(format_bps_percent(300), "3%");
    }
}
