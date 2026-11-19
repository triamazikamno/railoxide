#[allow(clippy::wildcard_imports)]
use super::*;

impl SwapBridgeLabels {
    /// The token the swap is named after: "USDT on Tron" for a non-EVM destination, whose
    /// symbol can match the sold token's, and "POL" otherwise.
    pub(in crate::root) fn named_token(&self) -> String {
        if self.non_evm {
            format!("{} on {}", self.token, self.network)
        } else {
            self.token.clone()
        }
    }

    /// The shield of a private delivery the provider reports made before the wallet verified
    /// the settlement. `None` for a public delivery and without such a report.
    pub(super) const fn reported_shield(&self) -> Option<&SwapShieldSeen> {
        match (&self.private, &self.report) {
            (Some(_), Some(SwapBridgeReported::Delivered { shield, .. })) => Some(shield),
            _ => None,
        }
    }

    /// Whether the swap reads as delivered before the wallet verified the settlement: the
    /// provider reports the delivery to a public receiver, or the fill block of a private one
    /// shows its shield.
    pub(super) const fn delivery_reported(&self) -> bool {
        matches!(
            (self.reported_shield(), &self.report),
            (Some(SwapShieldSeen::Detected(_)), _)
                | (None, Some(SwapBridgeReported::Delivered { .. }))
        )
    }

    /// Whether the fill block of a reported private delivery shows the proceeds held by the
    /// destination stealth account.
    const fn delivery_held(&self) -> bool {
        matches!(self.reported_shield(), Some(SwapShieldSeen::Held(_)))
    }

    /// Who gets the proceeds on the destination network, as copy names it after "to".
    pub(super) const fn destination(&self) -> &str {
        if self.private.is_some() {
            "your private balance"
        } else {
            self.receiver.as_str()
        }
    }
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

/// The block that showed a record's setup nonce consumed. `None` while no setup is resolved.
pub(in crate::root) fn swap_setup_block(record: &ExecutorRecord) -> Option<u64> {
    record
        .issued()
        .iter()
        .any(|payload| {
            payload.purpose() == ExecutorPayloadPurpose::Operation
                && record.nonce_resolved(payload.nonce())
        })
        .then(|| {
            record
                .nonce_watermark()
                .map(wallet_ops::vault::ExecutorNonceWatermark::block)
        })
        .flatten()
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

/// Whether a recovery on the destination network returned the delivered `token` its stealth
/// account held since the fill in `held_block`, as an explicit check's `balance` confirms.
pub(in crate::root) fn held_proceeds_recovered(
    destination: &ExecutorRecord,
    token: Address,
    held_block: u64,
    attribution: Option<&ExecutorAttribution>,
    balance: Option<(U256, alloy::eips::BlockNumHash)>,
) -> bool {
    held_recovered(destination, token, held_block, attribution, balance)
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

impl SwapFillHint {
    /// Confirmations first, then the canonical check still needed after the threshold.
    fn progress(&self) -> String {
        match self.confirmations {
            Some(confirmations) if confirmations >= self.depth => "Verifying settlement…".into(),
            Some(confirmations) => format!("Confirming ({confirmations}/{})", self.depth),
            None => "Confirming".into(),
        }
    }

    /// The Traded step's detail: the progress, then the bought amount when there is one, as
    /// the orderbook's report until the reported block shows the trade. The amount is what the
    /// order bought, which isn't what the receiver got.
    pub(super) fn traded(&self) -> String {
        let progress = self.progress();
        match &self.bought {
            Some(bought) if self.detected => format!("{progress} · {bought} bought"),
            Some(bought) => format!("{progress} · CoW reports {bought} bought"),
            None => progress,
        }
    }

    /// The private return's detail: whether the reported trade block shows the shield.
    pub(super) fn shielded(&self) -> String {
        match &self.shield {
            SwapSeen::Looking => "Looking for the shield…".into(),
            SwapSeen::Detected(Some(amount)) => format!("Shield detected · {amount}"),
            SwapSeen::Detected(None) => "Shield detected".into(),
            SwapSeen::NotFound => "No shield detected yet".into(),
        }
    }
}

/// The note under the steps of a swap whose fill is reported and not verified: nothing is left
/// for the user to do, and the wallet acts on a reported refund or failure, or on a private
/// delivery its fill block shows held, only once the settlement is verified. `None` without a
/// reported fill.
pub(in crate::root) fn reported_fill_note(stage: SwapStage, labels: &SwapLabels) -> Option<String> {
    if !matches!(
        stage,
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
    ) {
        return None;
    }
    let SwapFillHint { depth, origin, .. } = labels.fill_hint.as_ref()?;
    let problem = labels.bridge.as_ref().is_some_and(|bridge| {
        bridge.delivery_held()
            || matches!(
                bridge.report,
                Some(SwapBridgeReported::Refunded | SwapBridgeReported::Failed)
            )
    });
    Some(if problem {
        format!(
            "The wallet acts on this once the result is confirmed, after {depth} blocks on {origin}."
        )
    } else {
        format!("Nothing more to do. The result is confirmed after {depth} blocks on {origin}.")
    })
}
