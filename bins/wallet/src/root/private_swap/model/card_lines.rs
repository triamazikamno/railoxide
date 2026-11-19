#[allow(clippy::wildcard_imports)]
use super::*;

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
            line(
                format!("Swapped {pair}"),
                match bridge.map(|bridge| (bridge, bridge.reported_shield())) {
                    Some((bridge, Some(SwapShieldSeen::Detected(_)))) => format!(
                        "Shielded to {} on {} · confirming",
                        bridge.destination(),
                        bridge.network
                    ),
                    // The provider delivered to the destination stealth account, whose fill
                    // block wasn't read yet.
                    Some((bridge, Some(SwapShieldSeen::Looking))) => {
                        format!("Delivered on {} · confirming", bridge.network)
                    }
                    Some((bridge, None)) if bridge.delivery_reported() => format!(
                        "Delivered to {} on {} · confirming",
                        bridge.destination(),
                        bridge.network
                    ),
                    // Held proceeds weren't delivered to the private balance.
                    _ => "Traded · confirming".into(),
                },
            )
        }
        SwapStage::SetupNotSent => line(
            format!("Swap of {pair} isn't set up"),
            "Continue to set up its stealth account, or dismiss the swap.".into(),
        ),
        SwapStage::SetupRetired => line(
            format!("Swap of {pair} needs a new stealth account"),
            "Retry with a new account. The old one stays in Stealth accounts.".into(),
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
        SwapStage::Order(SwapOrderState::Superseded) => line(
            "Swap order not filled".into(),
            "Another order of this swap filled.".into(),
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

/// The single Private tab card for the wallet's private swaps and the swaps paid from Public
/// accounts. Either kind alone shows its own card. Together they read as several private swaps
/// do: how many there are and how many need attention, then each private swap's pair and the
/// title of the Public-paid ones.
pub(in crate::root::private_swap) fn card_line(
    swaps: &[(SwapStage, SwapLabels)],
    public: Option<PublicCardLine>,
) -> Option<SwapCardLine> {
    let Some(public) = public else {
        return swaps_card_line(swaps);
    };
    if swaps.is_empty() {
        return Some(SwapCardLine {
            title: public.title,
            detail: public.detail,
            attention: public.attention > 0,
        });
    }
    let count = swaps.len() + public.count;
    let attention = swaps
        .iter()
        .filter(|(stage, _)| stage.needs_attention())
        .count()
        + public.attention;
    Some(SwapCardLine {
        title: if attention == 0 {
            format!("{count} swaps in progress")
        } else {
            format!("{count} swaps · {attention} needs attention")
        },
        detail: swaps
            .iter()
            .map(|(_, labels)| labels.pair.as_str())
            .chain([public.title.as_str()])
            .collect::<Vec<_>>()
            .join(" · "),
        attention: attention > 0,
    })
}
