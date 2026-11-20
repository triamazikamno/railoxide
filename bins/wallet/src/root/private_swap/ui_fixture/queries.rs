#[allow(clippy::wildcard_imports)]
use super::*;

/// Whether the Public form should show its failed route lookup.
pub(in crate::root::private_swap) fn route_error() -> bool {
    matches!(mode(), Some(Mode::RouteError))
}

/// Whether a fixture mode is on. The form of a swap paid from a Public account then works
/// without private funds, and its jobs are scripted. An `update-check:`, `logs`,
/// `first-start:` or `walletconnect-request` mode leaves swaps alone.
pub(in crate::root::private_swap) fn active() -> bool {
    mode().is_some_and(|mode| {
        !matches!(
            mode,
            Mode::UpdateCheck(_)
                | Mode::Logs
                | Mode::FirstStartOldNotice
                | Mode::WalletConnectRequest
        )
    })
}

/// Whether this read of the first-start setup record treats its accepted notice as version 0.
/// Only the first read of a session does, so the notice's Continue can start the wallet.
pub(in crate::root) fn first_start_old_notice() -> bool {
    static USED: AtomicBool = AtomicBool::new(false);
    matches!(mode(), Some(Mode::FirstStartOldNotice)) && !USED.swap(true, Ordering::Relaxed)
}

/// Whether the Buy picker holds a network's setup-fee check, and whether that check failed.
/// `None` in a mode that holds none, where a network takes a new account.
pub(in crate::root::private_swap) fn setup_fee_check_failed() -> Option<bool> {
    match mode()? {
        Mode::Fee(fee) => Some(matches!(fee, Fee::Failed)),
        _ => None,
    }
}

/// Whether the Public account swap records are the fixture's own, which nothing reads again,
/// tracks or acts on. A `flow:` mode has none until its review is approved.
pub(in crate::root::private_swap) fn holds_records() -> bool {
    matches!(mode(), Some(Mode::Detail(_) | Mode::Flow(_)))
}

/// Whether the private swap records are the fixture's own, which nothing reads again, quotes,
/// tracks or acts on. A `retry-review` or `speed-up-review` mode has none until its form is
/// opened, and a `quote-error` mode has none at all: its form holds a failed quote, which no
/// request replaces.
pub(in crate::root::private_swap) fn holds_swaps() -> bool {
    matches!(
        mode(),
        Some(
            Mode::RetryReview
                | Mode::QuoteError
                | Mode::NativeBuy
                | Mode::Split(_)
                | Mode::SpeedUp(_)
                | Mode::Setup(_)
                | Mode::SetupUnderOrder(_)
                | Mode::Reported(_)
                | Mode::Cancelled(_)
                | Mode::NonEvm(..)
        )
    )
}

/// The block a `setup:included-order-open` mode holds its swap's own setup included in, as
/// private sync would show it, and the head that setup's confirmations count from in place of
/// the synced one, which nothing finalizes. `None` in every other mode.
pub(in crate::root::private_swap) fn setup_included() -> Option<(u64, u64)> {
    matches!(mode()?, Mode::SetupUnderOrder(SetupUnderOrder::Included))
        .then_some((SETUP_INCLUDED_BLOCK, SETUP_INCLUDED_BLOCK + 3))
}

/// The head block a `reported:` mode counts its reported settlement's confirmations from, in
/// place of the synced head. `None` in every other mode.
pub(in crate::root::private_swap) fn reported_head() -> Option<u64> {
    let Mode::Reported(reported) = mode()? else {
        return None;
    };
    Some(REPORTED_TRADE_BLOCK + reported.confirmations())
}

/// What approving the review of a `retry-review`, speed-up, `setup:fee-token` or
/// `setup:retired` mode ends in, in place of placing its order or sending its setup. `None` in
/// every other mode.
pub(in crate::root::private_swap) fn review_refusal() -> Option<&'static str> {
    matches!(
        mode(),
        Some(Mode::RetryReview | Mode::SpeedUp(_) | Mode::Setup(Setup::FeeToken | Setup::Retired))
    )
    .then_some("Fixture: this mode stops at the review. Nothing was sent.")
}

/// What the owner's rule would answer about speeding up the synthesized swap of a speed-up or
/// `setup:included-order-open` mode, which no owner holds: `Some(Ok(()))` offers the speed-up.
/// `None` in every other mode.
pub(in crate::root::private_swap) fn speed_up_refusal() -> Option<Result<(), SwapSpeedUpRefusal>> {
    match mode()? {
        Mode::SpeedUp(SpeedUp::TwoOrders | SpeedUp::Rejected) => {
            Some(Err(SwapSpeedUpRefusal::TwoOrdersFillable))
        }
        // The latest read that admitted the order on its included setup admits its speed-up.
        Mode::SpeedUp(SpeedUp::Review | SpeedUp::Open | SpeedUp::JustPlaced)
        | Mode::SetupUnderOrder(SetupUnderOrder::Included) => Some(Ok(())),
        _ => None,
    }
}

/// The setup fee that stands in for a new destination account's estimate, in a token of
/// `decimals`. `None` unless a fixture mode is on.
pub(in crate::root::private_swap) fn setup_fee(decimals: u8) -> Option<U256> {
    active().then(|| ten_thousandths(decimals, 500))
}

/// Whether the staged swap's destination setup counts as confirmed, so its status names what
/// follows it.
pub(in crate::root::private_swap) fn setup_done() -> bool {
    matches!(
        mode(),
        Some(Mode::Flow(
            Flow::Finishing | Flow::Placing | Flow::Signatures
        ))
    )
}

/// `count` ten-thousandths of a token of `decimals`.
pub(super) fn ten_thousandths(decimals: u8, count: u64) -> U256 {
    U256::from(count) * U256::from(10_u8).pow(U256::from(decimals.saturating_sub(4)))
}

/// The scripted answer at `phase`, and how long the job takes to give it: a hold wherever the
/// mode scripts nothing else. `None` unless a fixture mode is on.
pub(in crate::root::private_swap) fn step(phase: Phase) -> Option<(Duration, Step)> {
    use Flow::{Changed, Error, Finishing, Placing, SetupWaiting, Signatures};
    Some(match (mode()?, phase) {
        (
            Mode::Form
            | Mode::RouteError
            | Mode::Fee(_)
            | Mode::Detail(_)
            | Mode::Accounts
            | Mode::Refund(_)
            | Mode::RetryReview
            | Mode::QuoteError
            | Mode::Split(_)
            | Mode::Setup(_)
            | Mode::SetupUnderOrder(_),
            Phase::Approved,
        ) => (
            Duration::ZERO,
            Step::Fail("Fixture: this mode stops at the review. Nothing was sent."),
        ),
        (Mode::Flow(_), Phase::Approved) => (BEAT, Step::Claimed),
        (Mode::Flow(SetupWaiting | Finishing | Placing | Signatures), Phase::Claimed) => {
            (BEAT, Step::Waiting)
        }
        (Mode::Flow(Changed), Phase::Claimed) => (BEAT, Step::Changed(SwapReviewChange::Delivery)),
        (Mode::Flow(Error), Phase::Claimed) => (
            BEAT,
            Step::Fail("Fixture: the broadcaster couldn't submit the destination setup."),
        ),
        (Mode::Flow(Placing), Phase::Setup) => (BEAT, Step::Ready),
        (Mode::Flow(Signatures), Phase::Setup) => (BEAT, Step::Signatures),
        _ => (Duration::ZERO, Step::Hold),
    })
}
