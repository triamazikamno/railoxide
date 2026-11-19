#[allow(clippy::wildcard_imports)]
use super::*;

impl PublicSwapStage {
    pub(in crate::root::private_swap) const fn group(self) -> SwapOrderGroup {
        match self {
            Self::HeldByProxy | Self::HeldOnDestination => SwapOrderGroup::NeedsRecovery,
            Self::ApprovalFailed
            | Self::DepositFailed
            | Self::NeedsAttention
            | Self::Preparing(SwapStage::SetupFailed) => SwapOrderGroup::NeedsAttention,
            Self::DeliveredVerified
            | Self::DeliveredReported
            | Self::Refunded
            | Self::SwappedNotBridged
            | Self::Recovered
            | Self::PreparationCancelled
            | Self::Cancelled
            | Self::Expired => SwapOrderGroup::Ended,
            _ => SwapOrderGroup::Open,
        }
    }

    pub(in crate::root::private_swap) const fn label(self) -> &'static str {
        match self {
            Self::Preparing(SwapStage::SetupFailed) => "Destination setup failed",
            // An existing account, or one whose setup confirmed: nothing is being prepared.
            Self::Preparing(SwapStage::Ready | SwapStage::Approved) => "Account ready",
            Self::Preparing(_) => "Preparing destination account",
            Self::Approving => "Approving token",
            Self::ApprovalFailed => "Approval failed",
            Self::OrderOpen => "Order open",
            Self::SubmissionPending => "Order submission pending",
            Self::SubmissionRejected => "Order submission rejected",
            Self::Depositing => "Bridge deposit pending",
            Self::DepositFailed => "Bridge deposit failed",
            Self::Bridging => "Sent to the bridge",
            Self::DeliveredVerified => "Delivered · verified",
            Self::DeliveredReported => "Delivered · reported",
            Self::Refunding => "Refunding to Public account",
            Self::Refunded => "Refunded to Public account",
            Self::HeldByProxy => "Held by CoW proxy",
            Self::Withdrawing => "Withdrawing to Public account",
            Self::WithdrawnPendingCheck => "Withdrawn · checking deposit",
            Self::SwappedNotBridged => "Swapped without bridging",
            Self::HeldOnDestination => "Held on destination",
            Self::Recovered => "Recovered",
            Self::Cancelled => "Order cancelled",
            Self::Expired => "Order expired",
            Self::PreparationCancelled => "Preparation cancelled",
            Self::NeedsAttention => "Needs attention",
        }
    }
}

pub(in crate::root::private_swap) fn public_swap_stage(
    record: &ExecutorRecord,
    claimed: &SwapUseRecord,
    setup: Option<SwapSetupStatus>,
    now: u64,
    attribution: Option<&ExecutorAttribution>,
    destination_balance: Option<(U256, alloy::eips::BlockNumHash)>,
) -> Option<PublicSwapStage> {
    use wallet_ops::vault::{PublicSwapTransactionKind as Kind, SwapBridgeOutcome};
    let swap = claimed.public_swap()?;
    let observed = swap.observations();
    if observed.bridge_handoff.is_some() {
        return Some(match observed.bridge_outcome {
            Some(SwapBridgeOutcome::DeliveredVerified { .. }) => PublicSwapStage::DeliveredVerified,
            Some(SwapBridgeOutcome::DeliveredReported { .. }) => PublicSwapStage::DeliveredReported,
            Some(SwapBridgeOutcome::Refunding) if observed.bridge_refund.is_some() => {
                PublicSwapStage::Refunded
            }
            Some(SwapBridgeOutcome::Refunding) => PublicSwapStage::Refunding,
            Some(SwapBridgeOutcome::HeldOnDestination { block, .. }) => {
                let recovered = match claimed.role() {
                    SwapUseRole::PublicSourceDestination {
                        destination_token, ..
                    } => held_recovered(
                        record,
                        *destination_token,
                        block.number,
                        attribution,
                        destination_balance,
                    ),
                    _ => false,
                };
                if recovered {
                    PublicSwapStage::Recovered
                } else {
                    PublicSwapStage::HeldOnDestination
                }
            }
            Some(SwapBridgeOutcome::NeedsAttention) => PublicSwapStage::NeedsAttention,
            None => PublicSwapStage::Bridging,
        });
    }
    if observed.withdrawn.is_some() {
        // A later batch deposit can still have happened, even after a withdrawal. Its bounded
        // deadline query must rule that out before this swap leaves the status card.
        return Some(
            if observed.deposit_ruled_out.is_some() && !swap.batch_can_run(now) {
                PublicSwapStage::SwappedNotBridged
            } else {
                PublicSwapStage::WithdrawnPendingCheck
            },
        );
    }
    if observed.held_by_proxy.is_some() {
        let withdrawing = swap
            .transactions()
            .iter()
            .rev()
            .find(|tx| tx.kind == Kind::Withdrawal)
            .is_some_and(|tx| tx.inclusion.is_none());
        return Some(if withdrawing {
            PublicSwapStage::Withdrawing
        } else {
            PublicSwapStage::HeldByProxy
        });
    }
    if observed.traded.is_none() {
        if observed.cancelled.is_some() {
            return Some(PublicSwapStage::Cancelled);
        }
        if observed.expired.is_some() {
            return Some(PublicSwapStage::Expired);
        }
    }
    if let Some(order) = swap.order() {
        return Some(match order.submission_status() {
            SwapSubmissionStatus::Accepted => PublicSwapStage::OrderOpen,
            SwapSubmissionStatus::Pending => PublicSwapStage::SubmissionPending,
            SwapSubmissionStatus::Rejected => PublicSwapStage::SubmissionRejected,
        });
    }
    // Deposits that were ruled out leave the swap in preparation.
    let mut deposits = swap.live_deposits().peekable();
    if deposits.peek().is_some() {
        return Some(
            if deposits.all(|deposit| {
                deposit
                    .inclusion
                    .is_some_and(|included| included.finalized && !included.succeeded)
            }) {
                PublicSwapStage::DepositFailed
            } else {
                PublicSwapStage::Depositing
            },
        );
    }
    if claimed.is_stopped() {
        return Some(PublicSwapStage::PreparationCancelled);
    }
    if let Some(approval) = swap
        .transactions()
        .iter()
        .rev()
        .find(|tx| matches!(tx.kind, Kind::ApprovalReset | Kind::Approval))
    {
        if approval.inclusion.is_none() {
            return Some(PublicSwapStage::Approving);
        }
        if approval
            .inclusion
            .is_some_and(|included| !included.succeeded)
        {
            return Some(PublicSwapStage::ApprovalFailed);
        }
    }
    Some(PublicSwapStage::Preparing(if claimed.is_fresh() {
        swap_stage(record, setup, false, attribution)
    } else {
        SwapStage::Ready
    }))
}

/// The latest confirmed balance after the fill, falling back to its dated amount for display.
pub(in crate::root::private_swap) fn public_held_remaining(
    claimed: &SwapUseRecord,
    destination_balance: Option<(U256, alloy::eips::BlockNumHash)>,
) -> Option<U256> {
    let SwapUseRole::PublicSourceDestination { swap, .. } = claimed.role() else {
        return None;
    };
    let wallet_ops::vault::SwapBridgeOutcome::HeldOnDestination { amount, block, .. } =
        swap.observations().bridge_outcome?
    else {
        return None;
    };
    wallet_ops::executor_recovery_remaining_amount(
        std::iter::once((amount, block.number)).chain(
            destination_balance
                .filter(|(_, read_at)| read_at.number >= block.number)
                .map(|(balance, read_at)| (balance, read_at.number)),
        ),
    )
}

pub(in crate::root::private_swap) fn public_swap_steps(
    claimed: &SwapUseRecord,
    stage: PublicSwapStage,
    labels: &PublicSwapLabels,
) -> Vec<SwapStep> {
    use PublicActionStepStatus::{Done, Error, NotStarted, Pending, Stopped, Warning};
    use wallet_ops::vault::{
        PublicSwapTransactionKind as Kind, SwapBridgeOutcome, SwapObservation,
    };
    let Some(swap) = claimed.public_swap() else {
        return Vec::new();
    };
    let observed = swap.observations();
    let block = |observation: Option<SwapObservation>| {
        observation.map(|observation| observation.block.number)
    };
    let hash = |observation: Option<SwapObservation>| {
        observation.and_then(|observation| observation.transaction_hash)
    };
    // The swap's latest transaction of `kind`, which a step waits on until it is observed.
    let latest = |kind: Kind| {
        swap.transactions()
            .iter()
            .rev()
            .find(|tx| tx.kind == kind)
            .map(|tx| tx.hash)
    };
    let setup_status = match stage {
        PublicSwapStage::Preparing(SwapStage::SetupFailed) => Error,
        PublicSwapStage::Preparing(SwapStage::SetupNotSent) => NotStarted,
        PublicSwapStage::Preparing(SwapStage::SetupPending | SwapStage::SetupSubmitting) => Pending,
        PublicSwapStage::PreparationCancelled => Stopped,
        _ => Done,
    };
    // The step claims a setup only once it confirmed. A reused account has none.
    let destination = &labels.destination;
    let setup_title = match setup_status {
        _ if !claimed.is_fresh() => format!("Account on {destination} ready"),
        Error => format!("Account setup on {destination} failed"),
        NotStarted => format!("Set up account on {destination}"),
        Pending => format!("Setting up account on {destination}"),
        Stopped if labels.setup_block.is_none() => {
            format!("Account setup on {destination} stopped")
        }
        _ => format!("Account on {destination} set up"),
    };
    let mut steps = vec![SwapStep::new(
        setup_title,
        labels.account.clone(),
        setup_status,
    )];
    // Existing allowance and native deposits have no approval transaction to display.
    for tx in swap
        .transactions()
        .iter()
        .filter(|tx| matches!(tx.kind, Kind::ApprovalReset | Kind::Approval))
    {
        let (detail, status) = match tx.inclusion {
            None => ("Waiting for confirmation…", Pending),
            Some(inclusion) if inclusion.succeeded => ("", Done),
            Some(_) => ("The approval reverted.", Error),
        };
        steps.push(
            SwapStep::new(
                if tx.kind == Kind::ApprovalReset {
                    format!(
                        "Reset {} approval from {}",
                        labels.sell_symbol, labels.source
                    )
                } else {
                    format!("{} approved from {}", labels.sell_symbol, labels.source)
                },
                detail,
                status,
            )
            .in_block(block(tx.inclusion.map(|inclusion| inclusion.observation)))
            .with_transaction(Some(tx.hash)),
        );
    }
    // A permit is the approval as a signature the order carries, so it has no transaction.
    if swap.order().is_some_and(|order| order.permit().is_some()) {
        steps.push(SwapStep::new(
            format!(
                "{} approval signed by {}",
                labels.sell_symbol, labels.source
            ),
            "",
            Done,
        ));
    }
    if swap.intent().order {
        let (title, status) = if observed.traded.is_some() {
            ("Order filled", Done)
        } else {
            match stage {
                PublicSwapStage::Cancelled => ("Order cancelled", Stopped),
                PublicSwapStage::Expired => ("Order expired", Stopped),
                PublicSwapStage::SubmissionRejected => ("Order submission rejected", Error),
                PublicSwapStage::OrderOpen | PublicSwapStage::SubmissionPending => {
                    (stage.label(), Pending)
                }
                _ => ("Place order", NotStarted),
            }
        };
        let valid_to = || {
            swap.order().map(|order| {
                crate::root::private_swap::local_time_label(u64::from(order.valid_to()))
            })
        };
        let detail = if observed.traded.is_some() {
            labels.traded.clone().unwrap_or_default()
        } else {
            match stage {
                PublicSwapStage::Cancelled => None,
                PublicSwapStage::Expired => valid_to().map(|at| format!("expired {at}")),
                _ => valid_to().map(|at| format!("expires {at}")),
            }
            .unwrap_or_default()
        };
        steps.push(
            SwapStep::new(title, detail, status)
                .in_block(block(observed.traded))
                .with_order(swap.order().map(wallet_ops::vault::PublicSwapOrder::uid))
                .with_transaction(hash(observed.traded)),
        );
    }
    let proxy = matches!(
        stage,
        PublicSwapStage::HeldByProxy
            | PublicSwapStage::Withdrawing
            | PublicSwapStage::WithdrawnPendingCheck
            | PublicSwapStage::SwappedNotBridged
    );
    if proxy {
        let withdrawal = hash(observed.withdrawn).or_else(|| latest(Kind::Withdrawal));
        let step = match stage {
            // The settlement that paid the proxy is the block the proceeds are held since.
            PublicSwapStage::HeldByProxy => SwapStep::new(
                "Held by your CoW proxy",
                format!(
                    "The swap went through, but the bridge deposit didn't run. {} is in {}'s CoW proxy on {}.",
                    labels.held.as_deref().unwrap_or(&labels.bridged_symbol),
                    labels.source,
                    labels.origin
                ),
                Warning,
            )
            .in_block(block(observed.held_by_proxy.map(|held| held.observation))),
            PublicSwapStage::Withdrawing => SwapStep::new(
                format!("Withdrawing to {}", labels.source),
                "Waiting for confirmation…",
                Pending,
            )
            .with_transaction(withdrawal),
            PublicSwapStage::WithdrawnPendingCheck => SwapStep::new(
                format!("Withdrawn to {}", labels.source),
                "The wallet checks for a bridge deposit once the bridge instructions for your CoW proxy expire…",
                Pending,
            )
            .in_block(block(observed.withdrawn))
            .with_transaction(withdrawal),
            _ => SwapStep::new(
                format!("Withdrawn to {}", labels.source),
                "Swapped without bridging",
                Done,
            )
            .in_block(block(observed.withdrawn))
            .with_transaction(withdrawal),
        };
        steps.push(step);
        return steps;
    }
    steps.push(
        SwapStep::new(
            BRIDGE_DEPOSIT,
            labels.deposited.as_ref().map_or_else(
                || {
                    if stage == PublicSwapStage::DepositFailed {
                        "Deposit reverted. Nothing was bridged.".into()
                    } else {
                        String::new()
                    }
                },
                |amount| format!("{amount} to Across"),
            ),
            if observed.bridge_handoff.is_some() {
                Done
            } else if stage == PublicSwapStage::DepositFailed {
                Error
            } else if stage == PublicSwapStage::Depositing {
                Pending
            } else {
                NotStarted
            },
        )
        .in_block(block(
            observed.bridge_handoff.map(|handoff| handoff.observation),
        ))
        .with_transaction(
            // A deposit the settlement made shows its transaction on the order step.
            hash(observed.bridge_handoff.map(|handoff| handoff.observation))
                .filter(|deposit| hash(observed.traded) != Some(*deposit))
                .or_else(|| {
                    matches!(
                        stage,
                        PublicSwapStage::Depositing | PublicSwapStage::DepositFailed
                    )
                    .then(|| latest(Kind::Deposit))
                    .flatten()
                }),
        ),
    );
    let private = format!("Private on {}", labels.destination);
    // The fill on the destination network, when the recorded outcome has one.
    let filled = match observed.bridge_outcome {
        Some(
            SwapBridgeOutcome::DeliveredVerified {
                transaction_hash, ..
            }
            | SwapBridgeOutcome::HeldOnDestination {
                transaction_hash, ..
            },
        ) => Some(transaction_hash),
        Some(SwapBridgeOutcome::DeliveredReported {
            transaction_hash, ..
        }) => transaction_hash,
        _ => None,
    };
    let step = match stage {
        PublicSwapStage::DeliveredVerified => SwapStep::new(
            format!("{private} · verified"),
            labels.received.as_ref().map_or_else(String::new, |amount| {
                format!("{amount} shielded to your private balance")
            }),
            Done,
        )
        .in_block(match observed.bridge_outcome {
            Some(SwapBridgeOutcome::DeliveredVerified { block, .. }) => Some(block.number),
            _ => None,
        })
        .with_transaction(filled),
        PublicSwapStage::DeliveredReported => SwapStep::new(
            format!("Delivered on {} · reported", labels.destination),
            "Across reports delivery. The wallet could not verify the shield in finalized receipts.",
            Warning,
        )
        .with_transaction(filled),
        PublicSwapStage::Refunding => SwapStep::new(
            format!("Refunding to {}", labels.source),
            format!(
                "No relayer delivered the deposit before it expired. Across returns {} to {} on {}, usually within a few hours.",
                labels
                    .deposited
                    .as_deref()
                    .unwrap_or(&labels.bridged_symbol),
                labels.source,
                labels.origin
            ),
            Warning,
        ),
        PublicSwapStage::Refunded => {
            SwapStep::new(format!("Refunded to {}", labels.source), "", Done)
                .in_block(block(observed.bridge_refund))
                .with_transaction(hash(observed.bridge_refund))
        }
        PublicSwapStage::HeldOnDestination => SwapStep::new(
            format!("Held on {}", labels.destination),
            format!(
                "{} is in the destination stealth account. Recover it on this network.",
                labels.held.as_deref().unwrap_or(&labels.buy_symbol)
            ),
            Warning,
        )
        .in_block(match observed.bridge_outcome {
            Some(SwapBridgeOutcome::HeldOnDestination { block, .. }) => Some(block.number),
            _ => None,
        })
        .with_transaction(filled),
        PublicSwapStage::Recovered => {
            SwapStep::new("Recovered", "Destination funds were recovered.", Done)
        }
        PublicSwapStage::NeedsAttention => SwapStep::new(
            "Bridge needs attention",
            "Check the deposit status.",
            Warning,
        ),
        PublicSwapStage::Bridging => SwapStep::new(private, "Waiting for a relayer…", Pending),
        PublicSwapStage::Cancelled
        | PublicSwapStage::Expired
        | PublicSwapStage::PreparationCancelled => {
            SwapStep::new(private, "Nothing traded", Stopped)
        }
        _ => SwapStep::new(private, "", NotStarted),
    };
    steps.push(step);
    steps
}
