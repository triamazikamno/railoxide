#[allow(clippy::wildcard_imports)]
use super::*;

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
                swap_funding_key(order),
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

impl<'a> SwapAttempts<'a> {
    /// The swap of `record` whose last order is `latest`, as [`record_swaps`] groups the
    /// record's orders. An order that ends none of those swaps is a swap of its own.
    pub(in crate::root) fn ending_at(
        record: &'a ExecutorRecord,
        latest: &'a SwapOrderRecord,
    ) -> Self {
        let swap = record.swap();
        let attempts = swap
            .and_then(|swap| {
                record_swaps(record).into_iter().find_map(|range| {
                    let orders = swap.orders().get(range.orders)?;
                    orders
                        .last()
                        .is_some_and(|last| last.uid() == latest.uid())
                        .then_some(orders)
                })
            })
            .unwrap_or_else(|| std::slice::from_ref(latest));
        let traded = attempts
            .iter()
            .rev()
            .find(|order| order.observations().traded.is_some());
        let delivery = traded.and_then(|traded| swap?.delivery_order(traded));
        Self {
            attempts,
            outcome: swap_outcome_order(attempts, latest),
            traded,
            delivery,
        }
    }

    /// The attempt whose bridge evidence and signed delivery terms the swap shows: the one
    /// that delivered, or the one the swap follows while nothing is delivered.
    pub(in crate::root) fn bridge_evidence(&self) -> &'a SwapOrderRecord {
        self.delivery.unwrap_or(self.outcome)
    }

    /// When the swap's order expires, in Unix seconds: the expiry of the attempt it follows,
    /// or while competing attempts can still trade, the last of theirs.
    pub(in crate::root) fn valid_to(&self) -> u64 {
        self.attempts
            .last()
            .into_iter()
            .flat_map(|latest| tracked_among(self.attempts, latest))
            .map(|order| u64::from(order.valid_to()))
            .max()
            .unwrap_or_else(|| u64::from(self.outcome.valid_to()))
    }

    /// The stage of the swap: that of the attempt it follows.
    pub(in crate::root) fn stage(
        &self,
        record: &ExecutorRecord,
        attribution: Option<&ExecutorAttribution>,
    ) -> SwapStage {
        swap_order_stage(record, self.outcome, attribution)
    }

    /// Whether `order` ended without contributing to the swap: it isn't the attempt the swap
    /// follows, its trade or its delivery, and it no longer competes with the followed one.
    /// An attempt that still competes, or that the trade superseded, has its own status.
    pub(in crate::root) fn ended_earlier(&self, order: &SwapOrderRecord) -> bool {
        let is = |other: &SwapOrderRecord| other.uid() == order.uid();
        !(is(self.outcome)
            || self.traded.is_some_and(is)
            || self.delivery.is_some_and(is)
            || competes(self.outcome, order) && order.observations().pre_hook_dead.is_none())
    }

    /// What `order` contributed to the swap's outcome: its trade, or the bridge deposit of a
    /// trade another attempt holds. `None` for an attempt that contributed neither.
    pub(in crate::root) fn role(&self, order: &SwapOrderRecord) -> Option<&'static str> {
        let is =
            |other: Option<&SwapOrderRecord>| other.is_some_and(|other| other.uid() == order.uid());
        if is(self.traded) {
            Some(TRADED)
        } else if is(self.delivery) {
            Some(BRIDGE_DEPOSIT)
        } else {
            None
        }
    }

    /// The delivery attempt's approved destination minimum and the traded attempt's higher
    /// one, in that order, when the trade was delivered under another attempt's terms. The
    /// higher minimum did not bind the delivery.
    pub(in crate::root) fn superseded_minimum(&self) -> Option<(U256, U256)> {
        let (traded, delivery) = (self.traded?, self.delivery?);
        let approved = delivery.bounds().destination_minimum?;
        let unmet = traded.bounds().destination_minimum?;
        (traded.uid() != delivery.uid() && unmet > approved).then_some((approved, unmet))
    }
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
        .or_else(|| {
            approval
                .as_deref()
                .map(|approval| approval.delivery.clone())
        })?;
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

/// The swap use whose own setup can be sent again beside its orders: the use that reserved the
/// record's account fresh, with an order, open or ended, placed on that setup while the record
/// still leaves the setup unresolved. Its saved approval keeps the retry's fee limit.
pub(in crate::root) fn order_setup_use(record: &ExecutorRecord) -> Option<&SwapUseRecord> {
    wallet_ops::own_pending_swap_setup(record)?;
    let swap_use = record.swap_use(record.active_swap_use()?)?;
    (swap_use.is_fresh()
        && !swap_use.is_stopped()
        && record.has_swap_use_order(swap_use.id())
        && swap_use.approval().is_some())
    .then_some(swap_use)
}

/// Whether a setup of `record`'s account below `order`'s pre-hook nonce is unresolved, as the
/// setup below an order placed on it while included is until it is confirmed.
pub(in crate::root) fn setup_unresolved_below(
    record: &ExecutorRecord,
    order: &SwapOrderRecord,
) -> bool {
    let nonce = order.pre_hook().nonce();
    record.issued().iter().any(|payload| {
        payload.purpose() == ExecutorPayloadPurpose::Operation
            && payload.nonce() < nonce
            && !record.nonce_resolved(payload.nonce())
    })
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

/// Whether `record` holds only cancelled preparations: no order of any of its swaps, no
/// prepared swap, and at least one cancelled preparation. This only decides what is shown;
/// it neither means the work they signed is resolved nor that the account can be reused.
pub(in crate::root) fn holds_only_cancelled_preparations(record: &ExecutorRecord) -> bool {
    record.swap().is_none_or(|swap| swap.orders().is_empty())
        && prepared_swap_use(record).is_none()
        && cancelled_swap_uses(record).next().is_some()
}

/// A recovery returned funds at or after `block`: a batch, paid by a broadcaster or by the
/// account, that is the action attributed at its nonce with its shield received since then.
/// Without `attribution` no batch counts.
pub(super) fn recovered_since(
    record: &ExecutorRecord,
    block: u64,
    attribution: Option<&ExecutorAttribution>,
) -> bool {
    attribution.is_some_and(|attribution| {
        wallet_ops::executor_recovered_since(record, block, &attribution.evidence())
    })
}

/// Proceeds of `token` held since `held_block` are recovered: a recovery of that token is
/// confirmed since then, and `balance` reads none of it at a confirmed block no earlier than
/// the hold and that recovery. Without `attribution` or `balance` they stay held.
pub(super) fn held_recovered(
    record: &ExecutorRecord,
    token: Address,
    held_block: u64,
    attribution: Option<&ExecutorAttribution>,
    balance: Option<(U256, alloy::eips::BlockNumHash)>,
) -> bool {
    attribution
        .and_then(|attribution| {
            wallet_ops::executor_recovered_token_since(
                record,
                token,
                held_block,
                &attribution.evidence(),
            )
        })
        .is_some_and(|recovered_at| {
            balance.is_some_and(|(amount, read_at)| {
                amount.is_zero() && read_at.number >= held_block && read_at.number >= recovered_at
            })
        })
}

/// The setup stage from the recorded state of each signed setup alone. A setup whose nonce
/// is not consumed stays pending and can be retried, whatever became of a transaction that
/// carried it. While this session observes the setup, a consumed nonce stays pending until
/// the delegation itself is confirmed.
pub(super) fn recorded_setup_stage(setups: &[ExecutorPayloadState], observing: bool) -> SwapStage {
    if setups.is_empty() {
        SwapStage::SetupNotSent
    } else if setups.contains(&ExecutorPayloadState::Resolved) && !observing {
        SwapStage::Ready
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

/// The approved private minimum of the order the latest swap follows, or before any order
/// exists, the minimum approved with the setup.
pub(in crate::root) fn swap_private_minimum(record: &ExecutorRecord) -> Option<U256> {
    record_outcome_order(record)
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
        .or_else(|| {
            record
                .swap_approval()
                .map(|approval| approval.delivery.clone())
        })
        .unwrap_or_default()
}

/// When the latest swap's order expires, in Unix seconds: the expiry of the order it
/// follows, or while competing orders can still trade, the last of theirs.
pub(in crate::root) fn swap_valid_to(record: &ExecutorRecord) -> Option<u64> {
    tracked_orders(record)
        .into_iter()
        .map(|order| u64::from(order.valid_to()))
        .max()
}

/// The first block the swap's evidence pages must cover to observe its current step: the
/// block the step's payload was signed against.
pub(in crate::root) fn swap_observation_start(record: &ExecutorRecord) -> Option<u64> {
    // Competing orders can each trade, so the pages start at the earliest of their pre-hooks.
    let pre_hooks = tracked_orders(record)
        .into_iter()
        .map(|order| order.pre_hook().payload())
        .collect::<Vec<_>>();
    record
        .issued()
        .iter()
        .filter(|payload| {
            if pre_hooks.is_empty() {
                payload.purpose() == ExecutorPayloadPurpose::Operation
            } else {
                pre_hooks.contains(&payload.hash())
            }
        })
        .map(|payload| payload.context().observed().block().number)
        .min()
}

/// Names a swap in one line: "1.00 WETH for USDC", or "WETH for USDC" before an amount is known.
pub(in crate::root) fn swap_pair_label(sell: &str, buy_symbol: &str) -> String {
    format!("{sell} for {buy_symbol}")
}
