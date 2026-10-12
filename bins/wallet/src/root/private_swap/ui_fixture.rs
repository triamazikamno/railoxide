//! A debug-only fixture for looking at the Public account swap screens, at a private swap's
//! retry review, speed-up and split outcome, at the Stealth accounts inspector and at the
//! refund dialog of a blocked Shield: it opens them on a scratch wallet without private funds
//! and holds a chosen state on screen. It is compiled only with `debug_assertions`, and does
//! nothing unless `RAILOXIDE_UI_FIXTURE` is set.
//!
//! The variable names one mode, read once:
//!
//! - `route-error` opens the Public form with a failed Across route lookup and its Retry.
//! - `form` lets the Buy picker pick a network without private funds there, and gives a new
//!   destination account a stand-in setup fee in place of an estimate, so a live quote can
//!   be reviewed. Approving the review ends in an error. The quote is the wallet's own, so
//!   the review signs a permit when the Sell token has one and the account's allowance is
//!   short, as USDC's is on Ethereum, Base and Arbitrum. Such an order asks the account for
//!   no gas, so an account without native balance can review it.
//! - `fee:checking` and `fee:failed` do what `form` does, except that the Buy picker holds
//!   every other private network at its setup-fee check: still running, or failed and being
//!   tried again. A new destination account can't be reviewed there.
//! - `flow:<stage>` does what `form` does, and after the review is approved shows the
//!   detail of the reviewed swap, as a synthesized record, and holds it at `setup-sending`,
//!   `setup-waiting`, `finishing`, `placing` or `error`. `changed` ends on the form again,
//!   with the terms that changed to review. `signatures` opens the review a hardware
//!   account sees before its device prompts, for any account and without a device: two
//!   groups for an order, and three, the signed approval first, for an order whose review
//!   signs a permit. The record is staged once, at the stage the mode
//!   names, and the steps past the first wait start on the view's next observation pass. No
//!   record of the wallet is read, and the detail's actions do nothing.
//! - `detail:<stage>` opens the swap dialog of a Public account on the detail of one
//!   synthesized swap, at `setup-not-sent`, `setup-pending`, `account-ready`, `approving`,
//!   `order-open`, `bridging`, `delivered`, `held-on-destination`,
//!   `held-after-partial-recovery`, `refunded` or `expired`. The partial recovery holds a
//!   confirmed balance at 90% of the original fill without changing its bridge evidence.
//!   `permit-order-open` is an open order whose approval was signed as a permit, with no
//!   approval transaction, and `permit-used-up` is that order with the warning that its
//!   signed approval was used up.
//!   The detail's actions do nothing.
//! - `accounts` shows two synthesized accounts in Stealth accounts in place of the wallet's
//!   own. Account #7 is a swap account whose inspector shows each result tag: an executed
//!   setup, a pre-hook that executed with the cancellation it superseded at the same nonce,
//!   a consumed nonce that names neither of two recoveries, and an unconfirmed recovery.
//!   Account #8 holds one executed operation. The wallet holds neither account, so Check
//!   balances and the other actions fail.
//! - `refund:<case>` opens the refund dialog of a synthesized blocked Shield in place of
//!   Stealth accounts, the first time they are opened. Later openings show Stealth accounts,
//!   so the dialog's own actions lead there. `checking` holds the dialog on its origin
//!   check. `available` is a refund that goes back to a Public account labelled Savings, and
//!   its Refund… only closes the dialog. The other cases can't be refunded: the origin of
//!   `unknown` is an account the wallet doesn't hold, that of `stealth` is account #7 of the
//!   `accounts` mode, whose two accounts Stealth accounts then shows, and that of `inactive`
//!   is an inactive Public account labelled Savings. `unresolved` is a Shield transaction
//!   that couldn't be read, which has no origin.
//! - `retry-review` opens the private swap form in place of a Public account's, on Ethereum
//!   with its private session started: the retry of a synthesized swap that sells 0.05 WETH
//!   for USDC delivered by Across to a public receiver on Arbitrum One. Its first order
//!   expired with an approved delivery of 90 USDC, and the form holds a synthesized quote that
//!   delivers 100, so it shows the earlier order's warning with its checkbox. Ticking it
//!   enables Review…, which opens the review with its Earlier approval row and warning.
//!   Approving the review takes the vault password and ends in an error on the form. The form
//!   asks for no quote and no routes, so another gas share leaves Review… waiting for a bridge
//!   quote until the share is set back.
//! - `split:<stage>` opens the swap dialog, in place of a Public account's form, on the detail
//!   of that swap once its retry traded and the settlement made the expired first order's
//!   deposit: `bridging` has the hand-off and no outcome, `delivered` a verified delivery of 90
//!   USDC under the first order's minimum while the retry approved 100, `refunding` Across's
//!   report of a refund that isn't verified, and `refund-verified` the verified refund with
//!   the deposit found in the stealth account, where Recover… is offered. `unverified` is one
//!   order whose verified trade showed no deposit, for comparison. Copy buttons work, and the
//!   detail's other actions do nothing.
//! - `speed-up-review` opens the private swap form as `retry-review` does, on the speed-up of
//!   that swap's open retry: the first order expired with an approved delivery of 90 USDC, the
//!   retry that delivers 100 is open, and the form holds a synthesized quote that delivers 95
//!   for a lower deposit. Its terms are fixed, and under the Minimum controls it shows the open
//!   order's signed minimum beside the new one. Ticking the earlier order's checkbox enables
//!   Review…, which opens the speed-up's review with both signed minimums, its Earlier approval
//!   row and its warning. Approving the review takes the vault password and ends in an error on
//!   the form. As in `retry-review`, another gas share leaves Review… waiting for a bridge quote.
//! - `speed-up:open` opens the swap dialog, in place of a Public account's form, on the detail
//!   of that swap with its open retry, where Speed up… sits in the order step under the
//!   order's ID and opens the form of `speed-up-review`. `speed-up:just-placed` is that detail
//!   with the retry signed as the mode opens: it shows no Speed up… at first, and the action
//!   appears by itself 30 seconds later. `speed-up:two-orders` is the detail of the swap once a
//!   speed-up was placed and both orders can still fill, where Speed up… is disabled with its
//!   reason and the Orders section lists both orders with their IDs, minimums and expiries.
//!   `speed-up:rejected` is that detail when the orderbook rejected the speed-up: the swap
//!   stays open on the original order, the Orders section reads Open and Rejected, and
//!   Speed up… is disabled with the same reason.
//!   The detail's other actions do nothing.
//! - `speed-up:original-bridging`, `speed-up:original-delivered` and
//!   `speed-up:replacement-delivered` are that swap once one of its two orders traded, as one
//!   swap that follows the traded order: the original order, which approved 100 USDC, with its
//!   own deposit on its way or delivered, or the speed-up, which approved 95, delivered. The
//!   other order reads Not filled in the Orders section, and both IDs keep their copy buttons.
//!   `speed-up:original-private` is the original order's trade for a swap that delivers to the
//!   private balance on Ethereum instead, shielded by its own settlement.
//!   Each opens on the detail. Its Back button opens My orders, which lists the swap once:
//!   under Open while it is on its way, and under Ended once delivered. After the dialog is
//!   closed, the Private tab on Ethereum shows the status card for `speed-up:two-orders`,
//!   `speed-up:rejected` and `speed-up:original-bridging`. A delivered swap has ended and
//!   leaves the card, as any does.
//!
//! In every mode the jobs of a swap are scripted or held back: nothing is claimed, proved,
//! signed, paid or sent, and no record is written. The synthesized records live in memory
//! only, and nothing reads, tracks or acts on them. A `retry-review` or speed-up mode opens the
//! route its orderbook client would use, and sends nothing on it.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use alloy::eips::BlockNumHash;
use alloy::primitives::{Address, B256, Bytes, U256, address, keccak256};
use gpui::{App, Context, Window};
use wallet_ops::cow::OrderUid;
use wallet_ops::settings::SwapProfile;
use wallet_ops::vault::{
    AcrossOrderTerms, BridgeDelivery, BridgeOrderTerms, BridgeProvider, BridgeShieldFailure,
    BridgeSurplus, ExecutorNonceObservation, ExecutorNonceWatermark, ExecutorOperationId,
    ExecutorPayloadContext, ExecutorPayloadPurpose, ExecutorRecord, IssuedExecutorPayload,
    PublicSwapApproval, PublicSwapDeposited, PublicSwapIntent, PublicSwapObservations,
    PublicSwapRecord, SwapApprovedAccount, SwapApprovedBounds, SwapBridgeHandoff,
    SwapBridgeOutcome, SwapDelivery, SwapObservation, SwapOrderObservations, SwapPreHookDeath,
    SwapPreHookDeathCause, SwapProof, SwapRecipient, SwapTerms, SwapTradeAmounts, SwapUseId,
};
use wallet_ops::{
    BlockedShieldRescueBlocker, BlockedShieldRescueInfo, BlockedShieldRescueUtxoId,
    OperationNetworkIsolation, PairAnchorRate, SwapContinuingApproval, SwapReview,
    SwapReviewChange, SwapSpeedUpRefusal,
};

use super::dialog::SwapDialogView;
use super::model::SwapIdentity;
use super::{PrivateSwapsView, now_unix};
use crate::root::WalletRoot;
use crate::root::utxo::BlockedShieldRefundDialogState;

const VARIABLE: &str = "RAILOXIDE_UI_FIXTURE";
/// How long a scripted job takes before it answers.
const BEAT: Duration = Duration::from_secs(2);
/// The blocked Shield a `refund:` mode opens its dialog for. No wallet holds it.
const REFUND_UTXO: BlockedShieldRescueUtxoId = BlockedShieldRescueUtxoId {
    tree: u32::MAX,
    position: u64::MAX,
    commitment: B256::ZERO,
    blinded_commitment: B256::ZERO,
};

#[derive(Clone, Copy, Debug)]
enum Mode {
    Form,
    RouteError,
    Fee(Fee),
    Flow(Flow),
    Detail(Detail),
    Accounts,
    Refund(Refund),
    RetryReview,
    Split(Split),
    SpeedUp(SpeedUp),
}

/// What a speed-up mode shows of the synthesized private swap: the speed-up's form, the detail
/// of the swap with its open retry, the detail with two orders that can still fill, or the
/// detail once one of those two traded and the other can't.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpeedUp {
    Review,
    Open,
    /// The open retry was signed just now, so the detail doesn't offer Speed up yet.
    JustPlaced,
    TwoOrders,
    /// Two orders that can still fill, the speed-up's submission rejected by the orderbook.
    Rejected,
    /// The original order traded and made its own deposit, which Across hasn't filled yet.
    OriginalBridging,
    /// The original order traded and its own deposit was delivered.
    OriginalDelivered,
    /// The original order of a swap to the private balance traded and was shielded.
    OriginalPrivate,
    /// The speed-up traded and its own deposit was delivered.
    ReplacementDelivered,
}

/// What the detail of a `split:` mode's private swap shows of its Across deposit, which the
/// expired first order holds while the retry holds the trade. `Unverified` is one order whose
/// trade showed no deposit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Split {
    Bridging,
    Delivered,
    Refunding,
    RefundVerified,
    Unverified,
}

/// Where the Buy picker holds a network's setup-fee check.
#[derive(Clone, Copy, Debug)]
enum Fee {
    Checking,
    Failed,
}

/// What the refund dialog of a `refund:` mode shows: the origin check, an available refund,
/// or why the blocked Shield can't be refunded.
#[derive(Clone, Copy, Debug)]
enum Refund {
    Checking,
    Available,
    Unknown,
    Stealth,
    Inactive,
    Unresolved,
}

/// Where a reviewed swap is held.
#[derive(Clone, Copy, Debug)]
enum Flow {
    SetupSending,
    SetupWaiting,
    Finishing,
    Placing,
    Signatures,
    Changed,
    Error,
}

/// The stage the synthesized swap's detail shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Detail {
    SetupNotSent,
    SetupPending,
    AccountReady,
    Approving,
    OrderOpen,
    PermitOrderOpen,
    PermitUsedUp,
    Bridging,
    Delivered,
    HeldOnDestination,
    HeldAfterPartialRecovery,
    Refunded,
    Expired,
}

fn parse(value: &str) -> Option<Mode> {
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
    if value == "retry-review" {
        return Some(Mode::RetryReview);
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

/// The mode the variable names. Its first read says so in the log.
fn mode() -> Option<Mode> {
    static MODE: OnceLock<Option<Mode>> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = parse(&std::env::var(VARIABLE).ok()?);
        if let Some(mode) = mode {
            tracing::warn!(
                ?mode,
                "the debug UI fixture is active: swaps paid from a Public account are staged and nothing is sent"
            );
        } else {
            tracing::warn!("{VARIABLE} names no UI fixture mode and is ignored");
        }
        mode
    })
}

/// Whether the Public form should show its failed route lookup.
pub(super) fn route_error() -> bool {
    matches!(mode(), Some(Mode::RouteError))
}

/// Whether a fixture mode is on. The form of a swap paid from a Public account then works
/// without private funds, and its jobs are scripted.
pub(super) fn active() -> bool {
    mode().is_some()
}

/// Whether the Buy picker holds a network's setup-fee check, and whether that check failed.
/// `None` in a mode that holds none, where a network takes a new account.
pub(super) fn setup_fee_check_failed() -> Option<bool> {
    match mode()? {
        Mode::Fee(fee) => Some(matches!(fee, Fee::Failed)),
        _ => None,
    }
}

/// Whether the Public account swap records are the fixture's own, which nothing reads again,
/// tracks or acts on. A `flow:` mode has none until its review is approved.
pub(super) fn holds_records() -> bool {
    matches!(mode(), Some(Mode::Detail(_) | Mode::Flow(_)))
}

/// Whether the private swap records are the fixture's own, which nothing reads again, quotes,
/// tracks or acts on. A `retry-review` or `speed-up-review` mode has none until its form is
/// opened.
pub(super) fn holds_swaps() -> bool {
    matches!(
        mode(),
        Some(Mode::RetryReview | Mode::Split(_) | Mode::SpeedUp(_))
    )
}

/// What approving the review of a `retry-review` or speed-up mode ends in, in place of placing
/// its order. `None` in every other mode.
pub(super) fn review_refusal() -> Option<&'static str> {
    matches!(mode(), Some(Mode::RetryReview | Mode::SpeedUp(_)))
        .then_some("Fixture: this mode stops at the review. Nothing was sent.")
}

/// What the owner's rule would answer about speeding up a speed-up mode's synthesized swap,
/// which no owner holds: `Some(Ok(()))` offers the speed-up. `None` in every other mode.
pub(super) fn speed_up_refusal() -> Option<Result<(), SwapSpeedUpRefusal>> {
    match mode()? {
        Mode::SpeedUp(SpeedUp::TwoOrders | SpeedUp::Rejected) => {
            Some(Err(SwapSpeedUpRefusal::TwoOrdersFillable))
        }
        Mode::SpeedUp(SpeedUp::Review | SpeedUp::Open | SpeedUp::JustPlaced) => Some(Ok(())),
        _ => None,
    }
}

/// The setup fee that stands in for a new destination account's estimate, in a token of
/// `decimals`. `None` unless a fixture mode is on.
pub(super) fn setup_fee(decimals: u8) -> Option<U256> {
    active().then(|| ten_thousandths(decimals, 500))
}

/// Whether the staged swap's destination setup counts as confirmed, so its status names what
/// follows it.
pub(super) fn setup_done() -> bool {
    matches!(
        mode(),
        Some(Mode::Flow(
            Flow::Finishing | Flow::Placing | Flow::Signatures
        ))
    )
}

/// `count` ten-thousandths of a token of `decimals`.
fn ten_thousandths(decimals: u8, count: u64) -> U256 {
    U256::from(count) * U256::from(10_u8).pow(U256::from(decimals.saturating_sub(4)))
}

/// Where a job of a swap paid from a Public account would start its real work.
pub(super) enum Phase {
    /// The review was approved.
    Approved,
    /// The swap is claimed, and its destination is prepared and set up.
    Claimed,
    /// The destination setup is read again, and the swap continues past it once it is confirmed.
    Setup,
    /// The signed order or deposit is sent.
    Submit,
}

/// What a scripted job answers in place of the real one.
pub(super) enum Step {
    /// Never answers, which holds the job's status on screen.
    Hold,
    Claimed,
    Waiting,
    Ready,
    /// The order's signatures are staged for the review a hardware account sees.
    Signatures,
    Changed(SwapReviewChange),
    Fail(&'static str),
}

/// The scripted answer at `phase`, and how long the job takes to give it: a hold wherever the
/// mode scripts nothing else. `None` unless a fixture mode is on.
pub(super) fn step(phase: Phase) -> Option<(Duration, Step)> {
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
            | Mode::Split(_),
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

/// What the synthesized swap trades: `sold` of `sell` from `source` on `origin` for `bought`
/// of `bridged` there, delivered as `received` of `delivered` on the destination network.
/// `order` is false for a deposit of the Sell token itself.
pub(super) struct Staged {
    pub(super) origin: u64,
    pub(super) source: Address,
    pub(super) sell: Address,
    pub(super) bridged: Address,
    pub(super) delivered: Address,
    pub(super) order: bool,
    pub(super) sold: U256,
    pub(super) bought: U256,
    pub(super) received: U256,
}

impl PrivateSwapsView {
    /// In a `detail:` mode, show the detail of one synthesized swap that `source` pays with
    /// `sell`, in place of the form that was just opened.
    pub(super) fn open_ui_fixture_detail(
        &mut self,
        source: Address,
        sell: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(Mode::Detail(stage)) = mode() else {
            return;
        };
        let Some((chain, identity, record)) = self.ui_fixture_record(stage, source, sell, cx)
        else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        if stage == Detail::HeldAfterPartialRecovery
            && let Some(SwapBridgeOutcome::HeldOnDestination { amount, block, .. }) = record
                .public_swap_use(identity.swap_use)
                .and_then(|(_, swap)| swap.observations().bridge_outcome)
        {
            self.public_destination_balances.insert(
                (chain, identity.operation, identity.swap_use),
                (
                    amount * U256::from(9_u8) / U256::from(10_u8),
                    BlockNumHash::new(block.number + 1, block.hash),
                ),
            );
        }
        if stage == Detail::PermitUsedUp {
            self.public_permit_used_up
                .insert((identity.operation, identity.swap_use));
        }
        self.public_records = vec![(chain, record)];
        self.navigate(SwapDialogView::PublicDetail(identity), window, cx);
    }

    /// In a `flow:` mode, stand the record of the reviewed swap `identity`, which trades
    /// `swap` and delivers on `destination`, in for the claim that would have written it.
    pub(super) fn stage_ui_fixture_flow(
        &mut self,
        identity: SwapIdentity,
        destination: u64,
        swap: &Staged,
    ) {
        let Some(Mode::Flow(flow)) = mode() else {
            return;
        };
        let stage = match flow {
            Flow::SetupSending | Flow::Error => Detail::SetupNotSent,
            Flow::SetupWaiting => Detail::SetupPending,
            // Terms change at signing time, after the account is set up.
            Flow::Finishing | Flow::Placing | Flow::Signatures | Flow::Changed => {
                Detail::AccountReady
            }
        };
        let Some(record) = staged_record(stage, identity, swap) else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return;
        };
        self.public_records = vec![(destination, record)];
    }

    /// A swap at `stage` to the first other network with private swaps: an order that buys
    /// the origin's wrapped native token and delivers the destination's, so both are named.
    fn ui_fixture_record(
        &self,
        stage: Detail,
        source: Address,
        sell: Address,
        cx: &App,
    ) -> Option<(u64, SwapIdentity, ExecutorRecord)> {
        let origin = self.origin_chain_id;
        let root = self.root.upgrade()?;
        let (bridged, destination, delivered) = {
            let chains = &root.read(cx).effective_chain_configs;
            let bridged = chains.get(origin)?.wrapped_native_token?;
            let (destination, delivered) = chains.values().find_map(|chain| {
                let token = chain.wrapped_native_token?;
                (chain.chain_id != origin && chain.swap_profile().is_some())
                    .then_some((chain.chain_id, token))
            })?;
            (bridged, destination, delivered)
        };
        let amount = |chain, token, count| {
            let decimals = self
                .chain_token_metadata(chain, token, cx)
                .map_or(18, |metadata| metadata.decimals);
            ten_thousandths(decimals, count)
        };
        let identity = SwapIdentity {
            operation: ExecutorOperationId::random().ok()?,
            swap_use: SwapUseId::random().ok()?,
        };
        let record = staged_record(
            stage,
            identity,
            &Staged {
                origin,
                source,
                sell,
                bridged,
                delivered,
                order: true,
                sold: amount(origin, sell, 1_000_000),
                bought: amount(origin, bridged, 300),
                received: amount(destination, delivered, 299),
            },
        )?;
        Some((destination, identity, record))
    }
}

/// The destination record of `swap` at `stage`, decoded from the JSON a stored record has.
fn staged_record(stage: Detail, identity: SwapIdentity, swap: &Staged) -> Option<ExecutorRecord> {
    const ORIGIN_BLOCK: u64 = 31_204_518;
    const DESTINATION_BLOCK: u64 = 402_118_977;
    let now = now_unix();
    // An open order's validity is still ahead. An expired order's is behind.
    let valid_to = u32::try_from(if stage == Detail::Expired {
        now.saturating_sub(600)
    } else {
        now.saturating_add(1_200)
    })
    .ok()?;
    let hash = B256::repeat_byte(0x7a);
    let at = |number| SwapObservation {
        block: BlockNumHash::new(number, B256::repeat_byte(0xb1)),
        transaction_hash: Some(hash),
    };
    let fill = BlockNumHash::new(DESTINATION_BLOCK + 640, B256::repeat_byte(0xb2));
    let handed_off = |outcome| PublicSwapObservations {
        traded: Some(at(ORIGIN_BLOCK + 21)),
        bridge_handoff: Some(SwapBridgeHandoff {
            observation: at(ORIGIN_BLOCK + 21),
            deposit_id: Some(U256::from(1_279_830_u32)),
        }),
        deposited: Some(PublicSwapDeposited {
            input_amount: swap.bought,
            output_amount: swap.received,
        }),
        bridge_outcome: outcome,
        ..Default::default()
    };
    let observed = match stage {
        Detail::SetupNotSent
        | Detail::SetupPending
        | Detail::AccountReady
        | Detail::Approving
        | Detail::OrderOpen
        | Detail::PermitOrderOpen
        | Detail::PermitUsedUp => PublicSwapObservations::default(),
        Detail::Expired => PublicSwapObservations {
            expired: Some(SwapObservation {
                transaction_hash: None,
                ..at(ORIGIN_BLOCK + 60)
            }),
            ..Default::default()
        },
        Detail::Bridging => handed_off(None),
        Detail::Delivered => handed_off(Some(SwapBridgeOutcome::DeliveredVerified {
            block: fill,
            transaction_hash: hash,
            output_amount: swap.received,
            shielded: true,
        })),
        Detail::HeldOnDestination | Detail::HeldAfterPartialRecovery => {
            handed_off(Some(SwapBridgeOutcome::HeldOnDestination {
                block: fill,
                transaction_hash: hash,
                amount: swap.received,
            }))
        }
        Detail::Refunded => PublicSwapObservations {
            bridge_refund: Some(at(ORIGIN_BLOCK + 900)),
            ..handed_off(Some(SwapBridgeOutcome::Refunding))
        },
    };
    let account = Address::repeat_byte(0x3a);
    let delegate = Address::repeat_byte(0x5d);
    // An approval signed as a permit binds its pre-hook's gas limit and sends no transaction.
    let permit = matches!(stage, Detail::PermitOrderOpen | Detail::PermitUsedUp);
    let pre_hook_gas_limit = if permit { 110_000_u64 } else { 0 };
    let bounds: SwapApprovedBounds = serde_json::from_value(serde_json::json!({
        "sell_amount": swap.sold, "buy_amount": swap.bought,
        "private_minimum": swap.received * U256::from(99_u8) / U256::from(100_u8),
        "shield_fee_bps": "0x0", "slippage_bps": 50,
        "pre_hook_gas_limit": pre_hook_gas_limit, "anchors": []
    }))
    .ok()?;
    let saved = PublicSwapRecord::new(
        PublicSwapApproval {
            bounds,
            price_verified: Some(true),
            price_acknowledged: false,
            sell_token: swap.sell,
            on_shield_failure: if matches!(
                stage,
                Detail::HeldOnDestination | Detail::HeldAfterPartialRecovery
            ) {
                BridgeShieldFailure::KeepOnDestination
            } else {
                BridgeShieldFailure::RefundOnOrigin
            },
            destination: SwapApprovedAccount {
                address: Some(account),
                setup: true,
            },
            max_gas_cost: U256::ZERO,
        },
        PublicSwapIntent {
            bridged_token: swap.bridged,
            order: swap.order,
        },
    );
    let mut saved = serde_json::to_value(saved).ok()?;
    let traded = observed.traded.is_some();
    saved["observations"] = serde_json::to_value(observed).ok()?;
    if traded {
        saved["observations"]["trade_amounts"] = serde_json::json!({
            "sell_amount": swap.sold, "buy_amount": swap.bought, "fee_amount": "0x0"
        });
    }
    // The setup is on its way until the Public account approves, and its order follows.
    let set_up = !matches!(stage, Detail::SetupNotSent | Detail::SetupPending);
    let approves = set_up && stage != Detail::AccountReady;
    if approves && !permit {
        let inclusion = (stage != Detail::Approving).then(|| {
            serde_json::json!({
                "observation": at(ORIGIN_BLOCK), "finalized": true, "succeeded": true
            })
        });
        saved["transactions"] = serde_json::json!([{
            "kind": "Approval", "transaction": {}, "hash": hash, "inclusion": inclusion
        }]);
    }
    if approves && stage != Detail::Approving {
        let mut uid = [0_u8; 56];
        uid[52..].copy_from_slice(&valid_to.to_be_bytes());
        saved["path"] = serde_json::json!({ "Order": {
            "uid": alloy::hex::encode_prefixed(uid),
            "buy_token": swap.bridged, "proxy": Address::repeat_byte(0x4c),
            "batch": { "calldata": "0x", "nonce": B256::ZERO, "deadline": valid_to },
            "submission": {
                "signature": alloy::hex::encode_prefixed([0_u8; 65]), "quote_id": null
            },
            "submission_status": "Accepted"
        }});
        if permit {
            saved["path"]["Order"]["permit"] = serde_json::json!({
                "nonce": U256::ZERO, "deadline": valid_to, "value": swap.sold,
                "signature": alloy::hex::encode_prefixed([0_u8; 65])
            });
        }
    }
    let setup_block = BlockNumHash::new(DESTINATION_BLOCK, B256::repeat_byte(0xb3));
    let setup = serde_json::to_value(IssuedExecutorPayload::new(
        U256::ZERO,
        delegate,
        B256::repeat_byte(0x5e),
        ExecutorPayloadPurpose::Operation,
        ExecutorPayloadContext::new(
            Bytes::new(),
            ExecutorNonceObservation::new(setup_block, U256::ZERO),
            Vec::new(),
        ),
    ))
    .ok()?;
    // A setup that wasn't sent issued no payload.
    let issued = if stage == Detail::SetupNotSent {
        Vec::new()
    } else {
        vec![setup]
    };
    // A confirmed setup is one whose nonce an account read showed consumed.
    let consumed = set_up.then(|| ExecutorNonceWatermark::new(U256::ONE, DESTINATION_BLOCK));
    serde_json::from_value(serde_json::json!({
        "version": 2, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": identity.operation, "index": 3,
        "address": account, "delegate": delegate, "retired": false, "issued": issued,
        "nonce_watermark": consumed,
        "swap_uses": [{ "id": identity.swap_use, "started_at": now.saturating_sub(240),
            "fresh": true,
            "role": { "PublicSourceDestination": { "origin_chain": swap.origin,
                "source": swap.source, "destination_token": swap.delivered, "swap": saved
            }}
        }]
    }))
    .ok()
}

/// The network the private swap of a `retry-review`, `split:` or speed-up mode sells on.
const SWAP_ORIGIN: u64 = 1;
/// The block the synthesized private swap's observations count from.
const SWAP_ORIGIN_BLOCK: u64 = 24_612_480;
const WETH: Address = address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
const USDC: Address = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
/// The swap's stealth account. No wallet holds it.
const SWAP_ACCOUNT: Address = address!("b41d7e0c95a23f6841c7d02e9a5b63f18c4e7a2d");
/// Across delivers the swap's USDC on Arbitrum One to a public receiver no wallet holds.
const SWAP_DELIVERY: BridgeDelivery = BridgeDelivery {
    provider: BridgeProvider::Across,
    destination_chain: 42_161,
    receiver: address!("7f2c9d3a41b8e65c0d9a4b17e3f8a6c25d1b09e4"),
    destination_token: address!("af88d065e77c8cc2239327c5edb3a432268e5831"),
    surplus: BridgeSurplus::Reshield,
    private: None,
};
/// The private spend, 0.05 WETH, and what Railgun's unshield fee leaves of it to sell.
const SWAP_SPEND: u64 = 50_000_000_000_000_000;
const SWAP_SELL: u64 = 49_875_000_000_000_000;
/// The USDC each order deposits with Across and the USDC that deposit delivers: 90 for the
/// first order, 100 for its retry, and 95 for the retry's speed-up.
const FIRST_ORDER: (u64, u64) = (90_420_000, 90_000_000);
const RETRY: (u64, u64) = (100_460_000, 100_000_000);
const SPED_UP: (u64, u64) = (95_440_000, 95_000_000);
/// What the trade bought, 101.31 USDC.
const SWAP_BOUGHT: u64 = 101_312_480;

/// Order `attempt` of the synthesized private swap, as its record stores it: signed at `signed`
/// for 20 minutes, with a deposit that Across has four hours to fill. `hash` tells its ID and
/// its hooks, at nonces 1 and 2, apart from another order's.
fn staged_order(
    attempt: u32,
    signed: u64,
    (deposit, minimum): (u64, u64),
    hash: u8,
    observations: &serde_json::Value,
) -> Option<serde_json::Value> {
    let quoted = u32::try_from(signed).ok()?;
    let uid = OrderUid::new(keccak256([hash]), SWAP_ACCOUNT, quoted.checked_add(1_200)?);
    Some(serde_json::json!({
        "attempt": attempt, "uid": uid.0, "delivery": SwapDelivery::Bridge(SWAP_DELIVERY),
        "bounds": {
            "sell_amount": U256::from(SWAP_SELL), "unshield_amount": U256::from(SWAP_SPEND),
            "unshield_fee_bps": U256::from(25_u8), "buy_amount": U256::from(deposit),
            "private_minimum": U256::from(deposit), "shield_fee_bps": U256::ZERO,
            "slippage_bps": 50, "pre_hook_gas_limit": 1_045_000, "post_hook_gas_limit": 396_000,
            "anchors": [], "destination_minimum": U256::from(minimum), "gas_share_bps": 2_500,
            "gas_estimate": U256::from(492_000_u32), "gas_allowance": U256::from(123_000_u32),
            "gas_price_wei": 120_000_000, "valid_for_secs": 1_200
        },
        "pre_hook": { "nonce": U256::ONE, "payload": B256::repeat_byte(hash) },
        "post_hook": { "nonce": U256::from(2_u8), "payload": B256::repeat_byte(hash + 1) },
        "invalidates": null, "observations": observations, "submission_status": "Accepted",
        "bridge": BridgeOrderTerms::Across(AcrossOrderTerms {
            spoke_pool: address!("5c7bcd6e7de5423a257d81b442095a1a6ced35c5"),
            input_token: USDC,
            output_token: SWAP_DELIVERY.destination_token,
            input_amount: U256::from(deposit),
            output_amount: U256::from(minimum),
            quote_timestamp: quoted,
            fill_deadline: quoted.checked_add(14_400)?,
            exclusive_relayer: Address::ZERO,
            exclusivity_parameter: 0,
            recipient: None,
            message_hash: None,
        }),
    }))
}

/// The record of the synthesized private swap, decoded from the JSON a stored record has.
/// Without `split`, its first order expired and nothing ran. With it, the retry traded and its
/// settlement made the first order's deposit, whose outcome the stage names. `Unverified` is
/// one order instead, whose trade showed no deposit. With `speed_up` and no `split`, the
/// expired first order is followed by its open retry, or for `TwoOrders` the swap holds the
/// retry and its speed-up, both open. `Rejected` is `TwoOrders` with the speed-up's submission
/// rejected. The other speed-up stages hold those two orders once one of them traded, as
/// [`staged_overlap`] stages them.
fn staged_swap(split: Option<Split>, speed_up: Option<SpeedUp>) -> Option<ExecutorRecord> {
    let now = now_unix();
    // A refund follows the deposit's fill deadline.
    let refunds = matches!(split, Some(Split::Refunding | Split::RefundVerified));
    let signed = now.saturating_sub(if refunds { 18_000 } else { 3_000 });
    let at = |block: u64, transaction: Option<&str>| SwapObservation {
        block: BlockNumHash::new(SWAP_ORIGIN_BLOCK + block, keccak256(block.to_be_bytes())),
        transaction_hash: transaction.map(keccak256),
    };
    let value = |observed: SwapOrderObservations| serde_json::to_value(observed).ok();
    // The first order's 20 minutes are 100 blocks, and the retry is signed 25 blocks later.
    let expired = SwapPreHookDeath {
        cause: SwapPreHookDeathCause::Expired,
        observation: at(110, None),
    };
    let single = split == Some(Split::Unverified);
    let settlement = at(if single { 40 } else { 160 }, Some("settlement"));
    let traded = SwapOrderObservations {
        traded: Some(settlement),
        trade_amounts: Some(SwapTradeAmounts {
            sell_amount: U256::from(SWAP_SELL),
            buy_amount: U256::from(SWAP_BOUGHT),
            fee_amount: U256::ZERO,
            settlement_gas_used: Some(1_386_204),
            settlement_effective_gas_price: Some(410_000_000),
            executed_fee: Some(U256::from(11_400_000_000_000_u64)),
            executed_fee_token: Some(WETH),
        }),
        ..Default::default()
    };
    // The orders, and the account's nonce once their hooks ran.
    let (orders, nonce) = match split {
        None => {
            let ended = value(SwapOrderObservations {
                pre_hook_dead: Some(expired),
                expired: Some(expired.observation),
                ..Default::default()
            })?;
            let open = value(SwapOrderObservations::default())?;
            // An open order was signed five minutes ago, and the one before it five earlier.
            // A just-placed one is signed now.
            let orders = match speed_up {
                None => vec![staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?],
                Some(SpeedUp::Review | SpeedUp::Open) => vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?,
                    staged_order(1, now.saturating_sub(300), RETRY, 0xb1, &open)?,
                ],
                Some(SpeedUp::JustPlaced) => vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &ended)?,
                    staged_order(1, now, RETRY, 0xb1, &open)?,
                ],
                Some(SpeedUp::TwoOrders | SpeedUp::Rejected) => {
                    let mut placed =
                        staged_order(1, now.saturating_sub(300), SPED_UP, 0xb1, &open)?;
                    if speed_up == Some(SpeedUp::Rejected) {
                        placed["submission_status"] = serde_json::json!("Rejected");
                    }
                    vec![
                        staged_order(0, now.saturating_sub(600), RETRY, 0xa1, &open)?,
                        placed,
                    ]
                }
                Some(won) => return staged_overlap(won, settlement, &traded, now),
            };
            (orders, 1_u8)
        }
        // The pre-hook ran and the post-hook, which deposits, didn't.
        Some(Split::Unverified) => {
            let traded = value(SwapOrderObservations {
                pre_hook_executed: Some(settlement),
                ..traded
            })?;
            (vec![staged_order(0, signed, RETRY, 0xa1, &traded)?], 2)
        }
        Some(split) => {
            let outcome = match split {
                Split::Delivered => Some(SwapBridgeOutcome::DeliveredVerified {
                    block: BlockNumHash::new(402_118_977, keccak256("fill block")),
                    transaction_hash: keccak256("fill"),
                    output_amount: U256::from(FIRST_ORDER.1),
                    shielded: false,
                }),
                Split::Refunding | Split::RefundVerified => Some(SwapBridgeOutcome::Refunding),
                Split::Bridging | Split::Unverified => None,
            };
            let mut deposit = value(SwapOrderObservations {
                pre_hook_dead: Some(expired),
                delivered: Some(settlement),
                bridge_handoff: Some(SwapBridgeHandoff {
                    observation: settlement,
                    deposit_id: Some(U256::from(3_481_207_u32)),
                }),
                bridge_outcome: outcome,
                bridge_refund: (split == Split::RefundVerified).then(|| at(1_420, Some("refund"))),
                ..Default::default()
            })?;
            // The settlement reshielded what the trade bought beyond the deposit, less the
            // shield fee.
            deposit["settlement_credit"] = serde_json::json!({
                "observation": settlement,
                "private_amount": U256::from(10_865_249_u32), "fee": U256::from(27_231_u32)
            });
            (
                vec![
                    staged_order(0, signed, FIRST_ORDER, 0xa1, &deposit)?,
                    staged_order(1, signed + 1_500, RETRY, 0xb1, &value(traded)?)?,
                ],
                3,
            )
        }
    };
    staged_swap_record(&orders, nonce, signed)
}

/// The synthesized swap's two competing orders once one of them traded, which `won` names:
/// the original, signed ten minutes ago, and its speed-up, signed five minutes ago at the same
/// pre-hook nonce. The traded order holds `traded` at `settlement` with its own delivery, and
/// the other holds nothing, so it reads as not filled.
fn staged_overlap(
    won: SpeedUp,
    settlement: SwapObservation,
    traded: &SwapOrderObservations,
    now: u64,
) -> Option<ExecutorRecord> {
    let private = won == SpeedUp::OriginalPrivate;
    let (terms, delivers) = if won == SpeedUp::ReplacementDelivered {
        (SPED_UP, true)
    } else {
        (RETRY, won == SpeedUp::OriginalDelivered)
    };
    let mut filled = serde_json::to_value(SwapOrderObservations {
        pre_hook_executed: Some(settlement),
        delivered: Some(settlement),
        bridge_handoff: (!private).then_some(SwapBridgeHandoff {
            observation: settlement,
            deposit_id: Some(U256::from(3_481_207_u32)),
        }),
        bridge_outcome: delivers.then(|| SwapBridgeOutcome::DeliveredVerified {
            block: BlockNumHash::new(402_118_977, keccak256("fill block")),
            transaction_hash: keccak256("fill"),
            output_amount: U256::from(terms.1),
            shielded: false,
        }),
        ..*traded
    })
    .ok()?;
    // The settlement shielded everything a private swap bought, and for a Bridge swap what
    // the trade bought beyond the deposit, less the shield fee.
    if private {
        filled["shielded"] = serde_json::json!({
            "observation": settlement,
            "private_amount": U256::from(SWAP_BOUGHT), "fee": U256::ZERO
        });
    } else {
        let surplus = SWAP_BOUGHT.checked_sub(terms.0)?;
        let fee = surplus / 400;
        filled["settlement_credit"] = serde_json::json!({
            "observation": settlement,
            "private_amount": U256::from(surplus - fee), "fee": U256::from(fee)
        });
    }
    let unfilled = serde_json::to_value(SwapOrderObservations::default()).ok()?;
    let (original, speed_up) = if won == SpeedUp::ReplacementDelivered {
        (&unfilled, &filled)
    } else {
        (&filled, &unfilled)
    };
    let mut orders = vec![
        staged_order(0, now.saturating_sub(600), RETRY, 0xa1, original)?,
        staged_order(1, now.saturating_sub(300), SPED_UP, 0xb1, speed_up)?,
    ];
    if private {
        for order in &mut orders {
            order["delivery"] = serde_json::to_value(SwapDelivery::Reshield).ok()?;
            order["bridge"] = serde_json::Value::Null;
            order["bounds"]["destination_minimum"] = serde_json::Value::Null;
        }
    }
    staged_swap_record(&orders, 3, now.saturating_sub(600))
}

/// The synthesized swap's record around `orders`, whose hooks left the account at `nonce`,
/// reserved five minutes before `signed`.
fn staged_swap_record(
    orders: &[serde_json::Value],
    nonce: u8,
    signed: u64,
) -> Option<ExecutorRecord> {
    use ExecutorPayloadPurpose::{Operation, SwapPostHook, SwapPreHook};
    let mut issued = vec![staged_payload(0, 0xa0, Operation, Some(0xc0))];
    for hash in [0xa1, 0xb1].into_iter().take(orders.len()) {
        issued.push(staged_payload(1, hash, SwapPreHook, None));
        issued.push(staged_payload(2, hash + 1, SwapPostHook, None));
    }
    serde_json::from_value(serde_json::json!({
        "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
        "operation": ExecutorOperationId::random().ok()?, "index": 4,
        "address": SWAP_ACCOUNT, "delegate": Address::repeat_byte(0x5d), "retired": false,
        "created_at": signed.saturating_sub(300),
        "issued": issued.into_iter().collect::<Option<Vec<_>>>()?,
        "nonce_watermark": ExecutorNonceWatermark::new(
            U256::from(nonce),
            SWAP_ORIGIN_BLOCK + 1_500,
        ),
        "swap": {
            "terms": SwapTerms::new(
                WETH,
                USDC,
                SwapRecipient::new(U256::ONE, [0; 32]),
                B256::repeat_byte(0xa0),
            ),
            "proof": SwapProof::new(keccak256("proof"), Vec::new()),
            "orders": orders,
        },
    }))
    .ok()
}

/// The record and the review a `retry-review` mode holds its form on: the swap's expired first
/// order, and a quote for its retry that delivers 100 USDC while that order's approval of 90
/// can still decide the delivery. A `speed-up-review`, `speed-up:open` or
/// `speed-up:just-placed` mode holds the swap with that retry open instead, a quote that
/// delivers 95 USDC for a lower deposit, and the retry's ID, the order the form speeds up.
/// `None` in every other mode.
pub(super) fn retry_review(
    profile: &SwapProfile,
    isolation: OperationNetworkIsolation,
) -> Option<(ExecutorRecord, SwapReview, Option<OrderUid>)> {
    let speed_up = match mode()? {
        Mode::RetryReview => None,
        Mode::SpeedUp(SpeedUp::Review | SpeedUp::Open | SpeedUp::JustPlaced) => {
            Some(SpeedUp::Review)
        }
        _ => return None,
    };
    let record = staged_swap(None, speed_up)?;
    let open = speed_up
        .and_then(|_| record.swap()?.orders().last())
        .map(wallet_ops::vault::SwapOrderRecord::uid);
    // What the orderbook quotes, and what the bridge leg delivers of it.
    let (bought, delivered) = if open.is_some() {
        (96_140_000_u32, SPED_UP.1)
    } else {
        (101_180_000_u32, RETRY.1)
    };
    let first = record.swap()?.orders().first()?;
    let BridgeOrderTerms::Across(deposit) = first.bridge()? else {
        return None;
    };
    let earlier = SwapContinuingApproval {
        uid: first.uid(),
        attempt: first.attempt(),
        destination_minimum: deposit.output_amount,
        post_hook_nonce: first.post_hook()?.nonce(),
        quote_timestamp: deposit.quote_timestamp,
    };
    let review = SwapReview::for_ui_fixture(
        profile,
        &record,
        WETH,
        USDC,
        U256::from(SWAP_SPEND),
        U256::from(bought),
        // 2,030 USDC for one ETH, each in base units for one whole native token.
        PairAnchorRate {
            sell_rate: U256::from(1_000_000_000_000_000_000_u64),
            buy_rate: U256::from(2_030_000_000_u32),
        },
        120_000_000,
        SWAP_DELIVERY,
        U256::from(delivered),
        U256::from(delivered),
        vec![earlier],
        isolation,
    );
    match review {
        Ok(review) => Some((record, review, open)),
        Err(error) => {
            tracing::warn!(%error, "the debug UI fixture couldn't synthesize its review");
            None
        }
    }
}

impl PrivateSwapsView {
    /// In a `retry-review`, `split:` or speed-up mode, show the synthesized private swap in
    /// place of the Public account form that was asked for. `true` when such a mode is on.
    pub(super) fn open_ui_fixture_swap(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        // The detail's stage, or none for a mode that opens the form.
        let detail = match mode() {
            Some(Mode::RetryReview | Mode::SpeedUp(SpeedUp::Review)) => None,
            Some(Mode::Split(split)) => Some((Some(split), None)),
            Some(Mode::SpeedUp(speed_up)) => Some((None, Some(speed_up))),
            _ => return false,
        };
        if self.origin_chain_id != SWAP_ORIGIN {
            tracing::warn!("the debug UI fixture's private swap is on Ethereum: select it first");
            return true;
        }
        let Some((split, speed_up)) = detail else {
            self.open_ui_fixture_retry_review(window, cx);
            return true;
        };
        let Some(record) = staged_swap(split, speed_up) else {
            tracing::warn!("the debug UI fixture couldn't synthesize its swap record");
            return true;
        };
        let operation = record.operation();
        // What an explicit check found in the stealth account: the refunded deposit.
        if let Some(refund) = record
            .swap()
            .and_then(|swap| swap.orders().first())
            .and_then(|order| order.observations().bridge_refund)
        {
            self.tracking.entry(operation).or_default().stealth_balance =
                Some((U256::from(FIRST_ORDER.0), refund.block));
        }
        self.records = vec![record];
        self.show_detail(operation, window, cx);
        true
    }
}

/// The stealth accounts an `accounts` or `refund:stealth` mode shows in place of the
/// wallet's own, synthesized once. `None` in every other mode.
pub(in crate::root) fn stealth_accounts() -> Option<Vec<ExecutorRecord>> {
    static ACCOUNTS: OnceLock<Vec<ExecutorRecord>> = OnceLock::new();
    matches!(mode(), Some(Mode::Accounts | Mode::Refund(Refund::Stealth))).then(|| {
        ACCOUNTS
            .get_or_init(|| {
                let accounts = staged_accounts();
                if accounts.len() != 2 {
                    tracing::warn!("the debug UI fixture couldn't synthesize its stealth accounts");
                }
                accounts
            })
            .clone()
    })
}

/// The payload `hash` signed at `nonce`, handed off in `transaction` when it has one. Its
/// calldata does not decode, so each payload is an action of its own.
fn staged_payload(
    nonce: u64,
    hash: u8,
    purpose: ExecutorPayloadPurpose,
    transaction: Option<u8>,
) -> Option<serde_json::Value> {
    let signed = BlockNumHash::new(402_118_900, B256::repeat_byte(0xb4));
    let mut payload = serde_json::to_value(IssuedExecutorPayload::new(
        U256::from(nonce),
        Address::repeat_byte(0x5d),
        B256::repeat_byte(hash),
        purpose,
        ExecutorPayloadContext::new(
            Bytes::new(),
            ExecutorNonceObservation::new(signed, U256::from(nonce)),
            Vec::new(),
        ),
    ))
    .ok()?;
    payload["transaction_hashes"] = serde_json::json!(
        transaction
            .map(B256::repeat_byte)
            .into_iter()
            .collect::<Vec<_>>()
    );
    Some(payload)
}

/// The two accounts of the `accounts` mode, decoded from the JSON a stored record has. An
/// account that fails to decode is left out.
fn staged_accounts() -> Vec<ExecutorRecord> {
    use ExecutorPayloadPurpose::{Operation, Recovery, SwapPreHook};
    const READ_AT: u64 = 402_119_640;
    let now = now_unix();
    let delegate = Address::repeat_byte(0x5d);
    let account = |index: u32,
                   address: u8,
                   consumed: u64,
                   issued: Vec<Option<serde_json::Value>>|
     -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "version": 1, "derivation": "Railgun7702V1", "origin": "Reserved",
            "operation": ExecutorOperationId::random().ok()?, "index": index,
            "address": Address::repeat_byte(address), "delegate": delegate, "retired": false,
            "created_at": now.saturating_sub(3_600),
            "issued": issued.into_iter().collect::<Option<Vec<_>>>()?,
            "nonce_watermark": ExecutorNonceWatermark::new(U256::from(consumed), READ_AT),
        }))
    };
    // The swap's order expired, with its pre-hook recorded as executed at nonce 1.
    let swap = || {
        let mut uid = [0_u8; 56];
        uid[52..].copy_from_slice(&u32::try_from(now.saturating_sub(600)).ok()?.to_be_bytes());
        let mut swap = account(
            7,
            0x3b,
            3,
            vec![
                staged_payload(0, 0xb0, Operation, Some(0xc0)),
                staged_payload(1, 0xb1, SwapPreHook, None),
                staged_payload(1, 0xb2, Recovery, Some(0xc2)),
                staged_payload(2, 0xb3, Recovery, Some(0xc3)),
                staged_payload(2, 0xb4, Recovery, None),
                staged_payload(3, 0xb5, Recovery, Some(0xc5)),
            ],
        )?;
        swap["swap"] = serde_json::json!({
            "terms": SwapTerms::new(
                Address::repeat_byte(0x11),
                Address::ZERO,
                SwapRecipient::new(U256::ONE, [0; 32]),
                B256::repeat_byte(0xb0),
            ),
            "proof": SwapProof::new(B256::repeat_byte(0xb6), Vec::new()),
            "orders": [{
                "attempt": 0, "uid": alloy::hex::encode_prefixed(uid),
                "delivery": SwapDelivery::External { receiver: Address::repeat_byte(0x42) },
                "bounds": {
                    "sell_amount": "0x5af3107a4000", "buy_amount": "0x5af3107a4000",
                    "private_minimum": "0x5af3107a4000", "shield_fee_bps": "0x0",
                    "slippage_bps": 50, "pre_hook_gas_limit": 0, "post_hook_gas_limit": 0,
                    "anchors": []
                },
                "pre_hook": { "nonce": U256::ONE, "payload": B256::repeat_byte(0xb1) },
                "post_hook": null, "invalidates": null,
                "observations": SwapOrderObservations {
                    pre_hook_executed: Some(SwapObservation {
                        block: BlockNumHash::new(READ_AT - 80, B256::repeat_byte(0xb7)),
                        transaction_hash: None,
                    }),
                    ..Default::default()
                },
            }],
        });
        Some(swap)
    };
    let operation = || {
        let mut operation = account(
            8,
            0x3c,
            1,
            vec![staged_payload(0, 0xa0, Operation, Some(0xc8))],
        )?;
        operation["purpose_summary"] = serde_json::json!(format!(
            "Unshield 0.001 WETH → {} (unwrap)",
            Address::repeat_byte(0x42)
        ));
        Some(operation)
    };
    [swap(), operation()]
        .into_iter()
        .flatten()
        .filter_map(|account| serde_json::from_value(account).ok())
        .collect()
}

/// What the refund dialog shows for the blocked Shield of a `refund:` mode. `None` for any
/// other Shield, in every other mode, and in `refund:stealth` without its staged accounts.
pub(in crate::root) fn refund_dialog_state(
    utxo_id: BlockedShieldRescueUtxoId,
) -> Option<BlockedShieldRefundDialogState> {
    let Mode::Refund(refund) = mode()? else {
        return None;
    };
    if utxo_id != REFUND_UTXO {
        return None;
    }
    let unheld = Some(Address::repeat_byte(0x4e));
    // Each reason repeats the sentence wallet-ops gives that case.
    let (reason, blocker, origin) = match refund {
        Refund::Checking => return Some(BlockedShieldRefundDialogState::Checking),
        Refund::Available => {
            return Some(BlockedShieldRefundDialogState::Available(
                BlockedShieldRescueInfo {
                    eligible: true,
                    disabled_reason: None,
                    origin_address: unheld.map(|origin| origin.to_checksum(None)),
                    public_account_uuid: Some("ui-fixture".to_owned()),
                    public_account_label: Some("Savings".to_owned()),
                },
            ));
        }
        Refund::Unknown => (
            "The Shield came from an account that isn't in this wallet.".to_owned(),
            Some(BlockedShieldRescueBlocker::OriginUnknown),
            unheld,
        ),
        Refund::Stealth => {
            // The first account Stealth accounts shows, so the dialog's action finds its row.
            let accounts = stealth_accounts().unwrap_or_default();
            let account = accounts.first()?;
            let index = account.index();
            (
                format!("The Shield came from stealth account #{index}, which isn't in Public."),
                Some(BlockedShieldRescueBlocker::OriginStealth {
                    operation: account.operation(),
                    index,
                }),
                account.address(),
            )
        }
        Refund::Inactive => (
            "The Shield came from Public account \"Savings\", which is inactive.".to_owned(),
            Some(BlockedShieldRescueBlocker::OriginInactive {
                public_account_uuid: "ui-fixture".to_owned(),
                label: Some("Savings".to_owned()),
            }),
            unheld,
        ),
        Refund::Unresolved => (
            "Couldn't read the Shield transaction. Check the connection and try again.".to_owned(),
            None,
            None,
        ),
    };
    Some(BlockedShieldRefundDialogState::Unavailable {
        info: BlockedShieldRescueInfo {
            eligible: false,
            disabled_reason: Some(reason),
            origin_address: origin.map(|origin| origin.to_checksum(None)),
            public_account_uuid: None,
            public_account_label: None,
        },
        blocker,
    })
}

impl WalletRoot {
    /// In a `refund:` mode, open the refund dialog of its blocked Shield in place of Stealth
    /// accounts, the first time they are opened. `true` when the dialog opened.
    pub(in crate::root) fn open_ui_fixture_refund_dialog(
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        static OPENED: AtomicBool = AtomicBool::new(false);
        // Without a state there is no dialog to open: another mode, or `refund:stealth`
        // without its staged accounts.
        if refund_dialog_state(REFUND_UTXO).is_none() || OPENED.swap(true, Ordering::Relaxed) {
            return false;
        }
        Self::open_blocked_shield_refund_dialog(REFUND_UTXO, window, cx);
        true
    }
}
