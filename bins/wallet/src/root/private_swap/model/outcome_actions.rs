#[allow(clippy::wildcard_imports)]
use super::*;

/// The outcome of an order delivered as `delivery` once its trade is observed.
pub(in crate::root) fn swap_outcome(
    bounds: &SwapApprovedBounds,
    observed: &SwapOrderObservations,
    delivery: &SwapDelivery,
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
        SwapDelivery::Bridge(_) | SwapDelivery::NonEvmBridge(_) => None,
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
    !order.delivery().is_bridge()
        && observed.traded.is_some()
        && observed.trade_amounts.is_some_and(|amounts| {
            amounts.executed_fee.is_none()
                && amounts.settlement_gas_used.is_some()
                && amounts.settlement_effective_gas_price.is_some()
        })
}

/// The unix time from which the detail shows Speed up for the latest order `order`:
/// [`SPEED_UP_AFTER`] past its signing, which is its expiry less its approved validity. `None`
/// for an order with no recorded validity, which has no delay.
pub(in crate::root) fn speed_up_shown_at(order: &SwapOrderRecord) -> Option<u64> {
    let valid_for = order.bounds().valid_for_secs?;
    Some(
        u64::from(order.valid_to())
            .saturating_sub(u64::from(valid_for))
            .saturating_add(SPEED_UP_AFTER.as_secs()),
    )
}

/// Whether the detail shows Speed up, enabled or disabled, for the latest order `order` at
/// `now` in unix seconds.
pub(in crate::root) fn speed_up_shown(order: &SwapOrderRecord, now: u64) -> bool {
    speed_up_shown_at(order).is_none_or(|shown_at| now >= shown_at)
}

impl SwapActions {
    /// These actions with Speed up as the executor owner's rule answered for the latest order:
    /// `None` offers it, two fillable orders show it disabled with the reason, and an order
    /// that can't be sped up has none. It is offered only beside an available Cancel order,
    /// and only once `shown`, as [`speed_up_shown`] answers for that order.
    pub(in crate::root) const fn with_speed_up(
        mut self,
        refusal: Option<SwapSpeedUpRefusal>,
        shown: bool,
    ) -> Self {
        self.speed_up = match (self.cancel, refusal) {
            (Some(Ok(())), None) if shown => Some(Ok(())),
            (Some(Ok(())), Some(SwapSpeedUpRefusal::TwoOrdersFillable)) if shown => {
                Some(Err(SPEED_UP_TWO_ORDERS))
            }
            _ => None,
        };
        self
    }
}

/// What the progress dialog offers for `stage`. `past_valid_to` is local time past the latest
/// order's expiry; canonical expiry still needs a finalized block.
pub(in crate::root) const fn swap_actions(stage: SwapStage, past_valid_to: bool) -> SwapActions {
    let none = SwapActions {
        cancel: None,
        speed_up: None,
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
        // A setup on its way can be sent again. A retired account never held this swap's funds,
        // so the swap is retried with a new one, and what the old one holds from before is
        // recovered in Stealth accounts.
        SwapStage::SetupRetired | SwapStage::SetupPending => SwapActions {
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
            | SwapOrderState::NeedsAttention
            | SwapOrderState::Superseded,
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
        SwapStage::Order(SwapOrderState::Superseded) => "Not filled",
        SwapStage::Recovered => "Recovered",
    }
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
            if labels
                .bridge
                .as_ref()
                .is_some_and(SwapBridgeLabels::delivery_reported)
            {
                "Delivered · confirming"
            } else {
                "Traded · confirming"
            }
            .into()
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
        SwapStage::Order(SwapOrderState::Superseded) => "Not filled".into(),
        SwapStage::Recovered => "Recovered".into(),
    }
}

/// The asset a swap's recovery starts with: the sell token before a trade, the buy token after
/// one that paid the stealth account, and a bridge's refund of the bought token. Public address
/// and NEAR Intents orders pay elsewhere, so their trade leaves only the sell token to recover.
pub(in crate::root) const fn swap_recovery_token(
    stage: SwapStage,
    delivery: &SwapDelivery,
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
pub(in crate::root::private_swap) fn swap_total_cost(
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
pub(in crate::root::private_swap) fn swap_cost_bps(cost: U256, received: U256) -> u64 {
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
