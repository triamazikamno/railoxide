#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn parse(value: &str) -> Option<Mode> {
    let value = value.trim();
    if value == "form" {
        return Some(Mode::Form);
    }
    if value == "route-error" {
        return Some(Mode::RouteError);
    }
    if value == "accounts" {
        return Some(Mode::Accounts);
    }
    if value == "utxos" {
        return Some(Mode::Utxos);
    }
    if value == "activity" {
        return Some(Mode::Activity);
    }
    if value == "logs" {
        return Some(Mode::Logs);
    }
    if value == "first-start:old-notice" {
        return Some(Mode::FirstStartOldNotice);
    }
    if value == "walletconnect-request" {
        return Some(Mode::WalletConnectRequest);
    }
    if value == "unshield" {
        return Some(Mode::Unshield);
    }
    if value == "hardware-derive" {
        return Some(Mode::HardwareDerive);
    }
    if value == "broadcaster-review" {
        return Some(Mode::BroadcasterReview);
    }
    if let Some(state) = value.strip_prefix("update-check:") {
        return Some(Mode::UpdateCheck(match state {
            "behind" => UpdateCheck::Behind,
            "current" => UpdateCheck::Current,
            "ahead" => UpdateCheck::Ahead,
            "failed" => UpdateCheck::Failed,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("max:") {
        return Some(Mode::Max(match state {
            "fragmented" => Max::Fragmented,
            "locked" => Max::Locked,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("broadcaster-progress:") {
        return Some(Mode::BroadcasterProgress(match state {
            "ready" => BroadcasterProgress::Ready,
            "reconnecting" => BroadcasterProgress::Reconnecting,
            "timeout" => BroadcasterProgress::Timeout,
            "refused" => BroadcasterProgress::Refused,
            "direct" => BroadcasterProgress::Direct,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("rotation:") {
        return Some(Mode::Rotation {
            waku_failed: match state {
                "in-progress" => false,
                "waku-failed" => true,
                _ => return None,
            },
        });
    }
    if value == "retry-review" {
        return Some(Mode::RetryReview);
    }
    if value == "quote-error" {
        return Some(Mode::QuoteError);
    }
    if value == "native-buy" {
        return Some(Mode::NativeBuy);
    }
    if value == "speed-up-review" {
        return Some(Mode::SpeedUp(SpeedUp::Review));
    }
    if let Some(state) = value.strip_prefix("speed-up:") {
        return Some(Mode::SpeedUp(match state {
            "open" => SpeedUp::Open,
            "just-placed" => SpeedUp::JustPlaced,
            "two-orders" => SpeedUp::TwoOrders,
            "rejected" => SpeedUp::Rejected,
            "original-bridging" => SpeedUp::OriginalBridging,
            "original-delivered" => SpeedUp::OriginalDelivered,
            "original-private" => SpeedUp::OriginalPrivate,
            "replacement-delivered" => SpeedUp::ReplacementDelivered,
            _ => return None,
        }));
    }
    if let Some(stage) = value.strip_prefix("split:") {
        return Some(Mode::Split(match stage {
            "bridging" => Split::Bridging,
            "delivered" => Split::Delivered,
            "refunding" => Split::Refunding,
            "refund-verified" => Split::RefundVerified,
            "unverified" => Split::Unverified,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("setup:") {
        let under_order = match state {
            "included-order-open" => Some(SetupUnderOrder::Included),
            "dropped-order-open" => Some(SetupUnderOrder::Dropped),
            "dropped-order-expired" => Some(SetupUnderOrder::Expired),
            _ => None,
        };
        if let Some(under_order) = under_order {
            return Some(Mode::SetupUnderOrder(under_order));
        }
        return Some(Mode::Setup(match state {
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
        }));
    }
    if let Some(state) = value.strip_prefix("reported:") {
        return Some(Mode::Reported(match state {
            "fill" => Reported::Fill,
            "handoff" => Reported::Handoff,
            "delivered" => Reported::Delivered,
            "shielded" => Reported::Shielded,
            "held" => Reported::Held,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("non-evm:") {
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
        return Some(Mode::NonEvm(state, network));
    }
    if let Some(state) = value.strip_prefix("tron:") {
        return Some(Mode::Tron(match state {
            "accounts" => Tron::Accounts,
            "new-account" => Tron::NewAccount,
            "waiting" => Tron::Waiting,
            "complete" => Tron::Complete,
            "failed" => Tron::Failed,
            "token-did-not-move" => Tron::TokenDidNotMove,
            "dropped" => Tron::Dropped,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("cancelled:") {
        return Some(Mode::Cancelled(match state {
            "confirmed" => Cancelled::Confirmed,
            "reused" => Cancelled::Reused,
            _ => return None,
        }));
    }
    if let Some(state) = value.strip_prefix("fee:") {
        return Some(Mode::Fee(match state {
            "checking" => Fee::Checking,
            "failed" => Fee::Failed,
            _ => return None,
        }));
    }
    if let Some(case) = value.strip_prefix("refund:") {
        return Some(Mode::Refund(match case {
            "checking" => Refund::Checking,
            "available" => Refund::Available,
            "unknown" => Refund::Unknown,
            "stealth" => Refund::Stealth,
            "inactive" => Refund::Inactive,
            "unresolved" => Refund::Unresolved,
            _ => return None,
        }));
    }
    if let Some(stage) = value.strip_prefix("flow:") {
        return Some(Mode::Flow(match stage {
            "setup-sending" => Flow::SetupSending,
            "setup-waiting" => Flow::SetupWaiting,
            "finishing" => Flow::Finishing,
            "placing" => Flow::Placing,
            "signatures" => Flow::Signatures,
            "changed" => Flow::Changed,
            "error" => Flow::Error,
            _ => return None,
        }));
    }
    Some(Mode::Detail(match value.strip_prefix("detail:")? {
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
    }))
}
