#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn parse(value: &str) -> Option<Mode> {
    let value = value.trim();
    match value.split_once(':') {
        Some((family, state)) => parse_family(family, state),
        None => parse_keyword(value),
    }
}

fn parse_keyword(value: &str) -> Option<Mode> {
    Some(match value {
        "form" => Mode::Form,
        "route-error" => Mode::RouteError,
        "accounts" => Mode::Accounts,
        "utxos" => Mode::Utxos,
        "activity" => Mode::Activity,
        "logs" => Mode::Logs,
        "walletconnect-request" => Mode::WalletConnectRequest,
        "unshield" => Mode::Unshield,
        "hardware-derive" => Mode::HardwareDerive,
        "broadcaster-review" => Mode::BroadcasterReview,
        "retry-review" => Mode::RetryReview,
        "quote-error" => Mode::QuoteError,
        "native-buy" => Mode::NativeBuy,
        "speed-up-review" => Mode::SpeedUp(SpeedUp::Review),
        _ => return None,
    })
}

fn parse_family(family: &str, state: &str) -> Option<Mode> {
    match family {
        "first-start" => (state == "old-notice").then_some(Mode::FirstStartOldNotice),
        "update-check" => update_check_state(state).map(Mode::UpdateCheck),
        "max" => max_state(state).map(Mode::Max),
        "broadcaster-progress" => broadcaster_progress_state(state).map(Mode::BroadcasterProgress),
        "delivery" => delivery_state(state).map(Mode::Delivery),
        "rotation" => rotation_waku_failed(state).map(|waku_failed| Mode::Rotation { waku_failed }),
        "speed-up" => speed_up_state(state).map(Mode::SpeedUp),
        "split" => split_stage(state).map(Mode::Split),
        "setup" => setup_under_order_state(state)
            .map(Mode::SetupUnderOrder)
            .or_else(|| setup_state(state).map(Mode::Setup)),
        "reported" => reported_state(state).map(Mode::Reported),
        "non-evm" => non_evm_mode(state),
        "tron" => tron_state(state).map(Mode::Tron),
        "cancelled" => cancelled_state(state).map(Mode::Cancelled),
        "fee" => fee_state(state).map(Mode::Fee),
        "refund" => refund_case(state).map(Mode::Refund),
        "flow" => flow_stage(state).map(Mode::Flow),
        "detail" => detail_stage(state).map(Mode::Detail),
        _ => None,
    }
}

fn update_check_state(state: &str) -> Option<UpdateCheck> {
    Some(match state {
        "behind" => UpdateCheck::Behind,
        "current" => UpdateCheck::Current,
        "ahead" => UpdateCheck::Ahead,
        "failed" => UpdateCheck::Failed,
        _ => return None,
    })
}

fn max_state(state: &str) -> Option<Max> {
    Some(match state {
        "fragmented" => Max::Fragmented,
        "locked" => Max::Locked,
        _ => return None,
    })
}

fn broadcaster_progress_state(state: &str) -> Option<BroadcasterProgress> {
    Some(match state {
        "ready" => BroadcasterProgress::Ready,
        "reconnecting" => BroadcasterProgress::Reconnecting,
        "timeout" => BroadcasterProgress::Timeout,
        "refused" => BroadcasterProgress::Refused,
        "direct" => BroadcasterProgress::Direct,
        _ => return None,
    })
}

fn delivery_state(state: &str) -> Option<Delivery> {
    Some(match state {
        "estimate" => Delivery::Estimate,
        "refreshing" => Delivery::Refreshing,
        "custom-fee" => Delivery::CustomFee,
        "retry" => Delivery::Retry,
        "pin-conflict" => Delivery::PinConflict,
        "sponsored" => Delivery::Sponsored,
        "submitted" => Delivery::Submitted,
        _ => return None,
    })
}

fn rotation_waku_failed(state: &str) -> Option<bool> {
    Some(match state {
        "in-progress" => false,
        "waku-failed" => true,
        _ => return None,
    })
}

fn speed_up_state(state: &str) -> Option<SpeedUp> {
    Some(match state {
        "open" => SpeedUp::Open,
        "just-placed" => SpeedUp::JustPlaced,
        "two-orders" => SpeedUp::TwoOrders,
        "rejected" => SpeedUp::Rejected,
        "original-bridging" => SpeedUp::OriginalBridging,
        "original-delivered" => SpeedUp::OriginalDelivered,
        "original-private" => SpeedUp::OriginalPrivate,
        "replacement-delivered" => SpeedUp::ReplacementDelivered,
        _ => return None,
    })
}

fn split_stage(stage: &str) -> Option<Split> {
    Some(match stage {
        "bridging" => Split::Bridging,
        "delivered" => Split::Delivered,
        "refunding" => Split::Refunding,
        "refund-verified" => Split::RefundVerified,
        "unverified" => Split::Unverified,
        _ => return None,
    })
}

fn setup_under_order_state(state: &str) -> Option<SetupUnderOrder> {
    Some(match state {
        "included-order-open" => SetupUnderOrder::Included,
        "dropped-order-open" => SetupUnderOrder::Dropped,
        "dropped-order-expired" => SetupUnderOrder::Expired,
        _ => return None,
    })
}

fn setup_state(state: &str) -> Option<Setup> {
    Some(match state {
        "running" => Setup::Running,
        "waiting" => Setup::Waiting,
        "failed" => Setup::Failed,
        "broadcaster-error" => Setup::BroadcasterError,
        "timeout" => Setup::Timeout,
        "restart" => Setup::Restart,
        "ready" => Setup::Ready,
        "retired" => Setup::Retired,
        "replaced" => Setup::Replaced,
        "fee-token" => Setup::FeeToken,
        "two-accounts" => Setup::TwoAccounts,
        "two-waiting" => Setup::TwoWaiting,
        "reconnecting" => Setup::Reconnecting,
        "destination-failed" => Setup::DestinationFailed,
        _ => return None,
    })
}

fn reported_state(state: &str) -> Option<Reported> {
    Some(match state {
        "fill" => Reported::Fill,
        "handoff" => Reported::Handoff,
        "delivered" => Reported::Delivered,
        "shielded" => Reported::Shielded,
        "held" => Reported::Held,
        _ => return None,
    })
}

/// `non-evm:` takes an optional `zcash:` network before its state; Solana otherwise.
fn non_evm_mode(state: &str) -> Option<Mode> {
    let (network, state) = state
        .strip_prefix("zcash:")
        .map_or((NonEvmNetwork::Solana, state), |state| {
            (NonEvmNetwork::Zcash, state)
        });
    let state = match state {
        "review" => NonEvm::Review,
        "in-progress" => NonEvm::InProgress,
        "delivered" => NonEvm::Delivered,
        "needs-attention" => NonEvm::NeedsAttention,
        _ => return None,
    };
    Some(Mode::NonEvm(state, network))
}

fn tron_state(state: &str) -> Option<Tron> {
    Some(match state {
        "accounts" => Tron::Accounts,
        "new-account" => Tron::NewAccount,
        "waiting" => Tron::Waiting,
        "complete" => Tron::Complete,
        "failed" => Tron::Failed,
        "token-did-not-move" => Tron::TokenDidNotMove,
        "dropped" => Tron::Dropped,
        _ => return None,
    })
}

fn cancelled_state(state: &str) -> Option<Cancelled> {
    Some(match state {
        "confirmed" => Cancelled::Confirmed,
        "reused" => Cancelled::Reused,
        _ => return None,
    })
}

fn fee_state(state: &str) -> Option<Fee> {
    Some(match state {
        "checking" => Fee::Checking,
        "failed" => Fee::Failed,
        _ => return None,
    })
}

fn refund_case(case: &str) -> Option<Refund> {
    Some(match case {
        "checking" => Refund::Checking,
        "available" => Refund::Available,
        "unknown" => Refund::Unknown,
        "stealth" => Refund::Stealth,
        "inactive" => Refund::Inactive,
        "unresolved" => Refund::Unresolved,
        _ => return None,
    })
}

fn flow_stage(stage: &str) -> Option<Flow> {
    Some(match stage {
        "setup-sending" => Flow::SetupSending,
        "setup-waiting" => Flow::SetupWaiting,
        "finishing" => Flow::Finishing,
        "placing" => Flow::Placing,
        "signatures" => Flow::Signatures,
        "changed" => Flow::Changed,
        "error" => Flow::Error,
        _ => return None,
    })
}

fn detail_stage(stage: &str) -> Option<Detail> {
    Some(match stage {
        "setup-not-sent" => Detail::SetupNotSent,
        "setup-pending" => Detail::SetupPending,
        "account-ready" => Detail::AccountReady,
        "approving" => Detail::Approving,
        "order-open" => Detail::OrderOpen,
        "permit-order-open" => Detail::PermitOrderOpen,
        "permit-used-up" => Detail::PermitUsedUp,
        "bridging" => Detail::Bridging,
        "delivered" => Detail::Delivered,
        "held-on-destination" => Detail::HeldOnDestination,
        "held-after-partial-recovery" => Detail::HeldAfterPartialRecovery,
        "refunded" => Detail::Refunded,
        "expired" => Detail::Expired,
        _ => return None,
    })
}
