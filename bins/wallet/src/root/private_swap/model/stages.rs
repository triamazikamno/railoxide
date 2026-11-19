#[allow(clippy::wildcard_imports)]
use super::*;

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
                        | SwapOrderState::Superseded
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
                | Self::Order(
                    SwapOrderState::Done
                        | SwapOrderState::AttemptEnded(_)
                        | SwapOrderState::Superseded
                )
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
/// without it the recorded setup outcome is used. `attribution` is the owner's evidence for
/// the record, which decides whether a recovery returned stranded funds.
pub(in crate::root) fn swap_stage(
    record: &ExecutorRecord,
    setup: Option<SwapSetupStatus>,
    submitting: bool,
    attribution: Option<&ExecutorAttribution>,
) -> SwapStage {
    if let Some(order) = record_outcome_order(record) {
        return swap_order_stage(record, order, attribution);
    }
    if record.is_retired() {
        return SwapStage::SetupRetired;
    }
    if submitting {
        return SwapStage::SetupSubmitting;
    }
    let stage = match setup {
        Some(SwapSetupStatus::Delegated(_)) => SwapStage::Ready,
        Some(SwapSetupStatus::MissingDelegation) => SwapStage::SetupFailed,
        Some(SwapSetupStatus::Pending) | None => {
            let setups = record
                .issued()
                .iter()
                .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
                .filter_map(|payload| record.payload_state(payload.hash()))
                .collect::<Vec<_>>();
            recorded_setup_stage(&setups, setup.is_some())
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

impl SwapSetupConfirmation {
    pub(in crate::root::private_swap) fn detail(
        self,
        finality: crate::root::utxo::UtxoFinalityContext,
    ) -> Option<String> {
        use crate::root::utxo::BlockFinalityProgress;
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

pub(in crate::root::private_swap) fn swap_setup_confirmation(
    record: &ExecutorRecord,
    utxos: &[wallet_ops::UtxoOutput],
) -> Option<SwapSetupConfirmation> {
    let mut transactions = std::collections::BTreeSet::new();
    for payload in record
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == ExecutorPayloadPurpose::Operation)
    {
        if record.nonce_resolved(payload.nonce()) {
            return Some(SwapSetupConfirmation::Executed);
        }
        transactions.extend(payload.transaction_hashes().iter().copied());
    }
    if transactions.is_empty() {
        return None;
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

/// The funding key of `order`. `None` for an order from before swap uses, which competes
/// with no other.
pub(in crate::root) fn swap_funding_key(order: &SwapOrderRecord) -> Option<SwapFundingKey> {
    order
        .use_id()
        .map(|use_id| (use_id, order.pre_hook().nonce()))
}

/// Whether `other` is `order` or competes with it for the same funding.
pub(super) fn competes(order: &SwapOrderRecord, other: &SwapOrderRecord) -> bool {
    other.uid() == order.uid()
        || swap_funding_key(order).is_some_and(|key| swap_funding_key(other) == Some(key))
}

/// The order a swap follows, among `orders` of its record and given the swap's last order
/// `latest`. With one order at `latest`'s funding key that is `latest`. Among competing
/// attempts it is the one whose trade is verified, whichever was signed first. While none
/// traded it is the latest of them not recorded dead, whose submission and open-order state
/// the swap shows, and once all are dead, `latest`. When the orderbook rejected that
/// attempt, the swap follows the latest accepted attempt not recorded dead instead, if there
/// is one, since that order can still fill.
pub(in crate::root) fn swap_outcome_order<'a>(
    orders: &'a [SwapOrderRecord],
    latest: &'a SwapOrderRecord,
) -> &'a SwapOrderRecord {
    let competing = || {
        orders
            .iter()
            .rev()
            .filter(move |order| competes(latest, order))
    };
    let alive = || competing().filter(|order| order.observations().pre_hook_dead.is_none());
    competing()
        .find(|order| order.observations().traded.is_some())
        .or_else(|| {
            let followed = alive().next()?;
            Some(match followed.submission_status() {
                SwapSubmissionStatus::Rejected => alive()
                    .find(|order| order.submission_status() == SwapSubmissionStatus::Accepted)
                    .unwrap_or(followed),
                SwapSubmissionStatus::Pending | SwapSubmissionStatus::Accepted => followed,
            })
        })
        .unwrap_or(latest)
}

/// [`swap_outcome_order`] of the record's latest swap. `None` before any order.
pub(in crate::root) fn record_outcome_order(record: &ExecutorRecord) -> Option<&SwapOrderRecord> {
    let orders = record.swap()?.orders();
    Some(swap_outcome_order(orders, orders.last()?))
}

/// The orders of the record's latest swap that routine tracking follows, oldest first: the
/// order the swap follows and, while none of them traded, every attempt competing with it
/// that can still be the one that trades, which is one without a recorded hand-off or death.
/// Once one traded the others can't, and only it is followed. A swap with one order lists
/// that order alone. Empty before any order.
pub(in crate::root) fn tracked_orders(record: &ExecutorRecord) -> Vec<&SwapOrderRecord> {
    record
        .swap()
        .and_then(|swap| Some((swap.orders(), swap.orders().last()?)))
        .map_or_else(Vec::new, |(orders, latest)| tracked_among(orders, latest))
}

/// [`tracked_orders`] of the swap among `orders` whose last order is `latest`.
pub(super) fn tracked_among<'a>(
    orders: &'a [SwapOrderRecord],
    latest: &'a SwapOrderRecord,
) -> Vec<&'a SwapOrderRecord> {
    let outcome = swap_outcome_order(orders, latest);
    if outcome.observations().traded.is_some() {
        return vec![outcome];
    }
    orders
        .iter()
        .filter(|order| competes(latest, order))
        .filter(|order| {
            let observed = order.observations();
            order.uid() == outcome.uid()
                || (observed.bridge_handoff.is_none() && observed.pre_hook_dead.is_none())
        })
        .collect()
}

/// The stage of one of the record's orders, from its submission and canonical observations.
/// The swap's stage is that of the order it follows; another attempt's is its own status.
pub(in crate::root) fn swap_order_stage(
    record: &ExecutorRecord,
    order: &SwapOrderRecord,
    attribution: Option<&ExecutorAttribution>,
) -> SwapStage {
    let state = swap_attempt_state(record, order);
    if state == SwapOrderState::Open {
        match order.submission_status() {
            SwapSubmissionStatus::Pending => return SwapStage::SubmissionPending,
            SwapSubmissionStatus::Rejected => return SwapStage::SubmissionRejected,
            SwapSubmissionStatus::Accepted => {}
        }
    }
    let observed = order.observations();
    // Funds reach the stealth account when the pre-hook runs, or when a pre-hook sharing its
    // proof and nonce does, which the swap's funding records. A recovery whose shield arrived
    // after that returned them; a cancellation, which is also recovery, shields nothing. An
    // Across refund reaches it later, and surplus kept there can be recovered before, so only
    // a recovery after the verified refund returned it. Without that refund, none did. A trade
    // delivered by another attempt of its use takes that attempt's refund.
    let delivery = record
        .swap()
        .and_then(|swap| swap.delivery_order(order))
        .unwrap_or(order);
    let stranded_since = match (state, delivery.bridge()) {
        (SwapOrderState::Refunding, Some(BridgeOrderTerms::Across(_))) => delivery
            .observations()
            .bridge_refund
            .map(|refund| refund.block.number.saturating_add(1)),
        _ => observed
            .pre_hook_executed
            .or_else(|| record.swap().and_then(|swap| swap.funding(order)))
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
        Some(block) if stranded && recovered_since(record, block, attribution) => {
            SwapStage::Recovered
        }
        _ => SwapStage::Order(state),
    }
}

/// Splits a record's orders, given in order, into swaps. A retry keeps the pair and delivery
/// while no attempt of the swap traded. An order with the funding key of the one before it
/// competes with it for one trade and stays in its swap, whichever of them traded. A
/// different pair or delivery, or any other order after the swap traded, starts a new swap on
/// the reused account.
pub(in crate::root) fn swap_order_ranges(
    orders: impl IntoIterator<Item = SwapRangeOrder>,
) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    let mut previous: Option<(Address, Address, SwapDelivery, Option<SwapFundingKey>)> = None;
    // Whether an order of the current swap traded.
    let mut swap_traded = false;
    for (index, (sell, buy, delivery, traded, funding)) in orders.into_iter().enumerate() {
        let joins = previous.is_some_and(
            |(previous_sell, previous_buy, previous_delivery, previous_funding)| {
                (funding.is_some() && previous_funding == funding)
                    || (!swap_traded
                        && previous_sell == sell
                        && previous_buy == buy
                        && previous_delivery == delivery)
            },
        );
        if joins && let Some(range) = ranges.last_mut() {
            range.end = index + 1;
            swap_traded |= traded;
        } else {
            ranges.push(index..index + 1);
            swap_traded = traded;
        }
        previous = Some((sell, buy, delivery, funding));
    }
    ranges
}
