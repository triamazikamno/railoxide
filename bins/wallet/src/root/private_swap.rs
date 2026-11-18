//! Private swaps on the Private tab: the entry points, the swap dialog with its form, My orders
//! and order detail views, the review, the status card, cancellation, retry, and the hand-off to
//! Stealth accounts recovery.
//!
//! `PrivateSwapsView` owns this session's presentation state. Each swap's lifecycle is read
//! from its encrypted executor record, so the card and its progress survive a restart. The
//! wallet-ops executor owner performs every chain, orderbook, signing, and submission step.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, U256};
use gpui::{AppContext as _, Context, Entity, Task, WeakEntity, Window};
use gpui_component::WindowExt as _;
use rand::RngExt as _;
use tokio::sync::watch;
use wallet_ops::cow::OrderUid;
use wallet_ops::{
    DesktopPrivateSpendAuthorization, ExecutorOwner, HardwareExecutorAction,
    PublicBroadcasterCandidate, PublicBroadcasterResultKind, PublicBroadcasterWaku, SwapOrderState,
    SwapPlacementReadiness, SwapReviewChange, SwapSetupInclusion, SwapSetupObservation,
    SwapSetupStatus, SwapSpeedUpRefusal, TransactionGenerationStage, WalletSession,
    cow::{CowOrderStatusHint, CowOrderStatusReport, CowOrderbookClient},
    is_swap_record,
    settings::{
        EffectiveChainConfig, EffectiveTokenRegistry, ExecutorProfile, SwapProfile,
        SwapTokenEligibility,
    },
    swap_setup_recorded_executed,
    vault::{
        BridgeDelivery, BridgeProvider, ExecutorNonceObservation, ExecutorOperationId,
        ExecutorPayloadPurpose, ExecutorRecord, NonEvmDelivery, SwapApproval, SwapApprovedBounds,
        SwapBridgeOutcome, SwapDelivery, SwapOrderRecord, SwapUseId, SwapUseRecord,
    },
};

use super::WalletRoot;
use super::spend_authorization::{SpendAuthorizationIntent, SpendAuthorizationSummary};
use super::ui_helpers::{day_label, now_unix};

mod destinations;
mod dialog;
mod disclaimer;
mod form;
mod jobs;
mod model;
mod observations;
mod progress;
mod public_progress;
mod setup_progress;
mod swap_labels;
mod swap_records;
mod swap_state;
#[cfg(debug_assertions)]
mod ui_fixture;
pub(super) use ui_fixture::activity as ui_fixture_activity;
#[cfg(debug_assertions)]
pub(super) use ui_fixture::fill_logs as ui_fixture_fill_logs;
pub(super) use ui_fixture::first_start_old_notice as ui_fixture_first_start_old_notice;
pub(super) use ui_fixture::hardware_derive as ui_fixture_hardware_derive;
#[cfg(debug_assertions)]
pub(super) use ui_fixture::holds_send_review as ui_fixture_holds_send_review;
pub(super) use ui_fixture::refund_dialog_state as ui_fixture_refund_dialog_state;
pub(super) use ui_fixture::send_asset as ui_fixture_send_asset;
pub(super) use ui_fixture::setup_swap_token as ui_fixture_setup_swap_token;
pub(super) use ui_fixture::shows_utxos as ui_fixture_shows_utxos;
pub(super) use ui_fixture::stealth_accounts as ui_fixture_stealth_accounts;
pub(super) use ui_fixture::tor_rotation as ui_fixture_tor_rotation;
pub(super) use ui_fixture::unshield_asset as ui_fixture_unshield_asset;
#[cfg(debug_assertions)]
pub(super) use ui_fixture::update_check as ui_fixture_update_check;
pub(super) use ui_fixture::utxo_finality_context as ui_fixture_utxo_finality_context;
pub(super) use ui_fixture::utxo_rows as ui_fixture_utxo_rows;
#[cfg(debug_assertions)]
pub(super) use ui_fixture::{Tron as UiFixtureTron, tron as ui_fixture_tron};
/// The debug UI fixture's queries in a release build, which has no fixture.
#[cfg(not(debug_assertions))]
mod ui_fixture {
    pub(in crate::root) const fn stealth_accounts() -> Option<Vec<super::ExecutorRecord>> {
        None
    }

    pub(in crate::root) const fn refund_dialog_state(
        _utxo_id: wallet_ops::BlockedShieldRescueUtxoId,
    ) -> Option<crate::root::utxo::BlockedShieldRefundDialogState> {
        None
    }

    pub(in crate::root) const fn shows_utxos() -> bool {
        false
    }

    pub(in crate::root) const fn activity(
        _chain_id: u64,
    ) -> Option<crate::root::wallet_activity::ActivityStandIn> {
        None
    }

    pub(in crate::root) const fn hardware_derive() -> bool {
        false
    }

    pub(in crate::root) const fn first_start_old_notice() -> bool {
        false
    }

    pub(in crate::root) const fn tor_rotation() -> Option<bool> {
        None
    }

    pub(in crate::root) const fn utxo_finality_context()
    -> Option<crate::root::utxo::UtxoFinalityContext> {
        None
    }

    pub(in crate::root) const fn utxo_rows(
        _chain_id: u64,
    ) -> Option<Vec<crate::root::utxo::UtxoDisplayRow>> {
        None
    }

    pub(in crate::root) const fn unshield_asset(
        _chain_id: u64,
        _wrapped_native: alloy::primitives::Address,
    ) -> Option<crate::root::UnshieldAsset> {
        None
    }

    pub(in crate::root) const fn send_asset(
        _chain_id: u64,
        _wrapped_native: alloy::primitives::Address,
    ) -> Option<crate::root::UnshieldAsset> {
        None
    }

    pub(in crate::root) const fn setup_swap_token(
        _wrapped_native: alloy::primitives::Address,
    ) -> Option<alloy::primitives::Address> {
        None
    }

    pub(super) const fn max_asset(
        _chain_id: u64,
        _wrapped_native: alloy::primitives::Address,
    ) -> Option<crate::root::UnshieldAsset> {
        None
    }

    pub(super) const fn locked_sell_value(
        _token: alloy::primitives::Address,
        _wrapped_native: Option<alloy::primitives::Address>,
    ) -> Option<alloy::primitives::U256> {
        None
    }

    pub(super) const fn active() -> bool {
        false
    }

    pub(super) const fn setup_fee_check_failed() -> Option<bool> {
        None
    }

    pub(super) const fn holds_records() -> bool {
        false
    }

    pub(super) const fn holds_swaps() -> bool {
        false
    }

    pub(super) const fn setup_fee(_decimals: u8) -> Option<alloy::primitives::U256> {
        None
    }

    pub(super) const fn setup_done() -> bool {
        false
    }
}
pub(super) use form::SHARED_ACCOUNT_REASON;
pub(super) use form::public_source::PublicSwapAuthorization;

use form::{SetupSide, SwapForm};
use model::{
    SwapBridgeLabels, SwapBridgeReported, SwapFillHint, SwapIdentity, SwapLabels,
    SwapPrivateBridgeLabels, SwapSeen, SwapSetupLabels, SwapSetupProgress, SwapShieldSeen,
    SwapStepAccount, bridge_sent_amount, swap_observation_range, swap_observation_start,
    swap_private_delivery,
};
pub(super) use model::{
    SwapStage, swap_account_status, swap_delivery, swap_pair_label, swap_recovery_token,
    swap_sell_amount, swap_stage, swap_tokens,
};
use setup_progress::SetupProgress;

/// The longest wait between passes of a polling loop, which notice newly tracked swaps and
/// continue waiting work, and the first back-off of a bridge provider poll. Each swap's own
/// polls follow its [`SwapSchedule`].
const SWAP_OBSERVATION_INTERVAL: Duration = Duration::from_secs(12);
/// The shortest wait before a swap's next poll, and the polling loops' first wait, so swaps
/// loaded with the view are first polled 9-15 s after it opens.
const MIN_SWAP_POLL_INTERVAL: Duration = Duration::from_secs(9);
/// Longest wait between routine polls of a bridge provider about one order.
const MAX_BRIDGE_POLL_INTERVAL: Duration = Duration::from_mins(5);
/// The order validity a prepared swap's draft restores from an approval saved without one. The
/// form shows it, and the user can change it before the review.
const DRAFT_VALIDITY: Duration = Duration::from_mins(30);
const SWAP_BROADCASTER_RESPONSE_TIMEOUT: Duration = Duration::from_mins(2);
const SWAP_BROADCASTER_REPUBLISH_INTERVAL: Duration = Duration::from_secs(5);
/// The re-sends after which a setup still waiting for a broadcaster's answer offers to be sent
/// again, with another broadcaster.
const SWAP_SETUP_RETRY_RESENDS: u32 = 5;
/// Why a setup that private sync shows included isn't sent again.
const SETUP_ALREADY_INCLUDED: &str =
    "The setup is already included. Wait for confirmation and verification.";

/// Whether a setup's row offers to send that setup again. `None` shows no action, and an error
/// shows it disabled with the reason.
type SetupRetryAvailability = Option<Result<(), gpui::SharedString>>;

pub(super) struct PrivateSwapsPanel {
    origin_chain_id: u64,
    session: Option<Arc<WalletSession>>,
    view: Entity<PrivateSwapsView>,
}

/// A spend the user authorizes for a swap: its setup with the approved order, one account's
/// setup sent again, the order, or an early cancellation.
pub(super) struct SwapAuthorization {
    session: Arc<WalletSession>,
    destination_session: Option<Arc<WalletSession>>,
    action: SwapAction,
    /// A private Bridge swap's destination network and its stealth account there, when the
    /// action also signs for that account. One approval then gives an authorization for each
    /// network.
    destination: Option<(u64, ExecutorOperationId)>,
}

#[derive(Clone)]
enum SwapAction {
    Setup(Box<form::SetupApproval>),
    /// Only one stealth account's setup, sent again: the swap's own, or a private Bridge
    /// swap's destination account's on its network.
    SetupRetry(Box<form::SetupRetry>),
    Order(Box<form::OrderApproval>),
    Cancel(Box<progress::CancelApproval>),
}

impl SwapAction {
    /// The swap the action belongs to.
    fn operation(&self) -> ExecutorOperationId {
        match self {
            Self::Setup(approval) => approval.operation,
            Self::SetupRetry(approval) => approval.operation,
            Self::Order(approval) => approval.operation,
            Self::Cancel(approval) => approval.operation,
        }
    }
}

impl SwapAuthorization {
    pub(super) fn hardware_executor_action(&self) -> HardwareExecutorAction {
        match &self.action {
            SwapAction::Setup(approval) => HardwareExecutorAction::Execute(approval.operation),
            SwapAction::SetupRetry(approval) => HardwareExecutorAction::Execute(approval.account),
            SwapAction::Order(approval) => HardwareExecutorAction::Execute(approval.operation),
            SwapAction::Cancel(approval) => HardwareExecutorAction::Recover(approval.operation),
        }
    }

    /// The network whose executor owner approves [`Self::hardware_executor_action`]: the
    /// swap's own, or the destination network for a setup sent there by itself.
    #[cfg(feature = "hardware")]
    pub(super) fn hardware_executor_chain(&self) -> u64 {
        match &self.action {
            SwapAction::SetupRetry(approval) => approval.chain_id,
            _ => self.session.chain_id,
        }
    }

    /// The destination network and the executor action a hardware wallet approves there in the
    /// same device session, for a private Bridge swap's setup or order.
    #[cfg(feature = "hardware")]
    pub(super) fn hardware_destination_action(&self) -> Option<(u64, HardwareExecutorAction)> {
        self.destination
            .map(|(chain_id, operation)| (chain_id, HardwareExecutorAction::Execute(operation)))
    }

    /// Every session used for review must still be installed when approval completes.
    pub(super) fn sessions_are_current(&self, root: &WalletRoot) -> bool {
        use super::chain_load::ChainUtxoState;
        root.stealth_session_is_current(&self.session)
            && self.destination_session.as_ref().is_none_or(|expected| {
                matches!(
                    root.chain_states.get(&expected.chain_id),
                    Some(ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. })
                        if Arc::ptr_eq(session, expected)
                )
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwapJobKind {
    Setup,
    /// One account's setup is estimated again before its retry is reviewed. Nothing is
    /// signed.
    SetupQuote,
    /// The approved terms are quoted again once the setup is confirmed. Nothing is signed.
    Requote,
    Order,
    CancelQuote,
    Cancel,
    Check,
}

struct SwapJob {
    operation: ExecutorOperationId,
    kind: SwapJobKind,
    abort: tokio::task::AbortHandle,
}

/// Approved display terms while a new order is being prepared, before it has a durable UID.
#[derive(Clone)]
struct PendingSwapOrder {
    previous_order: Option<OrderUid>,
    sell: Address,
    buy: Address,
    delivery: SwapDelivery,
    amount: U256,
    private_minimum: U256,
    slippage_bps: u32,
    gas_share_bps: u16,
    valid_for: Duration,
    reuse_account: bool,
    /// The swap use the draft's order is signed for: the one that claims the account, or the
    /// new one a reused account's draft claims it for. Restored with the draft.
    swap_use: SwapUseId,
    started_at: u64,
    /// The draft is a speed-up of `previous_order`, which stays open. It stands in for that
    /// order only while its own order is being placed: see [`PrivateSwapsView::pending_order`].
    speeds_up: bool,
}

impl PendingSwapOrder {
    /// The draft of the swap a reused account is claimed for and has no order of yet, from the
    /// approval saved with that claim. A restart keeps the claim and drops this session's
    /// draft, so the records give it again: the swap stays listed, and reopens with its
    /// accounts and terms. An account the use reserved fresh shows its own setup instead.
    fn prepared(record: &ExecutorRecord) -> Option<Self> {
        let swap_use = model::prepared_swap_use(record).filter(|swap_use| !swap_use.is_fresh())?;
        let approval = swap_use.approval()?;
        let tokens = approval.tokens?;
        let bounds = &approval.bounds;
        Some(Self {
            previous_order: None,
            sell: tokens.sell,
            buy: tokens.buy,
            delivery: approval.delivery.clone(),
            amount: bounds.spend_amount(),
            private_minimum: bounds.private_minimum,
            slippage_bps: bounds.slippage_bps,
            gas_share_bps: bounds
                .gas_share_bps
                .unwrap_or(wallet_ops::cow::GAS_SHARE_BALANCED_BPS),
            valid_for: bounds
                .valid_for_secs
                .map_or(DRAFT_VALIDITY, |secs| Duration::from_secs(secs.into())),
            reuse_account: true,
            swap_use: swap_use.id(),
            started_at: swap_use.started_at().unwrap_or_default(),
            speeds_up: false,
        })
    }

    /// Whether the draft is another swap than the record's latest order belongs to.
    fn starts_new_swap(&self, record: &ExecutorRecord) -> bool {
        record
            .swap()
            .and_then(|swap| swap.orders().last())
            .is_some_and(|order| order.use_id() != Some(self.swap_use))
    }

    /// When the draft's swap started. A reused account's new swap starts when its use claims
    /// the account, which a retry of the draft keeps.
    fn started(&self, record: &ExecutorRecord) -> u64 {
        record
            .swap_use(self.swap_use)
            .filter(|swap_use| !swap_use.is_fresh())
            .and_then(SwapUseRecord::started_at)
            .unwrap_or(self.started_at)
    }
}

/// This session's knowledge about one swap that its record doesn't hold.
#[derive(Default)]
struct SwapTracking {
    pending_order: Option<PendingSwapOrder>,
    /// The last setup observation, including the delegated executor once confirmed.
    setup: Option<SwapSetupStatus>,
    /// The last setup observation's read at the latest block, when it showed the swap's own
    /// setup included while the record leaves that setup unresolved. Never recorded. Each
    /// setup observation replaces it, and a signing that finds the setup gone drops it.
    inclusion: Option<SwapSetupInclusion>,
    /// The next block of an order's history to observe. A setup keeps none: each pass reads
    /// its account at the confirmed block.
    cursor: Option<u64>,
    /// The last observation or submission problem, shown in the swap's progress.
    error: Option<String>,
    /// The latest successful explicit status check, scoped to the order the swap followed
    /// then and its completion time.
    status_checked: Option<(OrderUid, u64)>,
    /// The sell amount entered before the first order records it.
    amount: Option<U256>,
    slippage_bps: Option<u32>,
    /// The gas share and order validity entered before the first order records them.
    gas_share_bps: Option<u16>,
    valid_for: Option<Duration>,
    /// The swap's own orderbook route, kept for its quotes, its order submission, its order
    /// reports, and the RPC reads of its accounts.
    orderbook: Option<CowOrderbookClient>,
    /// The orderbook's last report on each order asked about, by its UID, so a report on one
    /// competing order never replaces another's. Display and location only; never an outcome.
    order_hints: HashMap<OrderUid, SwapOrderHint>,
    /// What was learned early about each order whose fill is reported and whose settlement
    /// isn't recorded, by its UID. Display only; never persisted and never an outcome.
    early_hints: HashMap<OrderUid, SwapEarlyHint>,
    /// The app-data budget after the orderbook rejected an order's size.
    byte_budget: Option<usize>,
    /// Routine bridge provider polls of handed-off orders without an outcome, which back off,
    /// and the early requests of orders in `early_hints`.
    bridge_polls: HashMap<OrderUid, BridgePoll>,
    /// Bridge provider clients on the `orderbook` route, kept with it for status checks.
    bridge_clients: Option<wallet_ops::SwapBridgeClients>,
    /// The route a polling loop opens for the swap while it keeps no `orderbook`, shared by
    /// both loops until the opened client is kept there.
    opening: Arc<tokio::sync::OnceCell<CowOrderbookClient>>,
    /// Traded orders whose executed fee this session already asked the orderbook for, whatever
    /// the answer, so a failed read isn't repeated until the next session.
    fee_asked: HashSet<OrderUid>,
    /// The bought token in the stealth account, and the block it was read at, from this
    /// session's last explicit check of a Bridge swap's refund or undelivered deposit. Recovery
    /// of a refund waits for it, and opens with it.
    stealth_balance: Option<(U256, alloy::eips::BlockNumHash)>,
    /// The stages of the swap's own setup handed to a broadcaster in this session, kept after
    /// its job ended until a new attempt starts or the swap leaves its setup.
    setup_progress: Option<SetupProgress>,
    /// The same for the setup of a private Bridge swap's destination stealth account.
    destination_setup_progress: Option<SetupProgress>,
    /// The last setup observation of a private Bridge swap's destination stealth account, read
    /// on its own network.
    destination_setup: Option<SwapSetupStatus>,
    /// The delivered token in the destination stealth account, and the confirmed block it was
    /// read at, from this session's last explicit check of held proceeds, with the order
    /// checked. Only [`PrivateSwapsView::destination_balance`] reads it. Recovery there opens
    /// with it.
    destination_balance: Option<(OrderUid, U256, alloy::eips::BlockNumHash)>,
    /// An early cancellation was handed to a broadcaster; the observation decides the winner.
    cancelling: bool,
    /// The user approved this swap's setup in this session: once the setup is confirmed, the
    /// wallet checks the approved terms again and asks to place the order without waiting
    /// for the user to return. Cleared after the first attempt, so failures don't repeat.
    auto_place: bool,
    /// The automatic attempt waits for the setup's change: the approved spend doesn't fit the
    /// spendable notes yet, and the setup can still make it fit. As the owner last answered.
    waits_for_change: bool,
    /// The delivery allowance earlier signing quotes of this swap showed its previews to lack,
    /// added to each later review until an order is submitted.
    delivery_shortfall: Option<U256>,
}

impl SwapTracking {
    const fn setup_progress(&self, side: SetupSide) -> Option<&SetupProgress> {
        match side {
            SetupSide::Origin => self.setup_progress.as_ref(),
            SetupSide::Destination => self.destination_setup_progress.as_ref(),
        }
    }

    const fn setup_progress_mut(&mut self, side: SetupSide) -> &mut Option<SetupProgress> {
        match side {
            SetupSide::Origin => &mut self.setup_progress,
            SetupSide::Destination => &mut self.destination_setup_progress,
        }
    }

    fn clear_setup_progress(&mut self) {
        self.setup_progress = None;
        self.destination_setup_progress = None;
    }

    /// Replaces the swap's route, or drops it with `None`. A read still opening a route through
    /// the previous `opening` never installs it afterwards.
    fn set_route(&mut self, client: Option<CowOrderbookClient>) {
        self.orderbook = client;
        self.opening = Arc::default();
    }

    /// The route cell a read of the swap goes over: a fresh one holding the kept route, or the
    /// shared `opening` while the swap keeps none.
    fn route_cell(&self) -> Arc<tokio::sync::OnceCell<CowOrderbookClient>> {
        match &self.orderbook {
            Some(orderbook) => Arc::new(orderbook.clone().into()),
            None => Arc::clone(&self.opening),
        }
    }

    /// Keeps the route a background read opened through the swap's current `opening`. A read
    /// over a kept route, or over an opening that a later route change replaced, keeps nothing.
    fn keep_route(&mut self, cell: &Arc<tokio::sync::OnceCell<CowOrderbookClient>>) {
        if Arc::ptr_eq(cell, &self.opening)
            && let Some(client) = cell.get()
        {
            self.orderbook.get_or_insert_with(|| client.clone());
            self.opening = Arc::default();
        }
    }

    /// Makes the next bridge provider poll of `uid` wait longer, after one without an outcome.
    fn back_off_bridge_poll(&mut self, uid: OrderUid, now: Instant) {
        let attempts = self.bridge_polls.get(&uid).map_or(0, |poll| poll.attempts);
        self.bridge_polls.insert(
            uid,
            BridgePoll {
                attempts: attempts.saturating_add(1),
                next_at: now + bridge_poll_interval(attempts),
            },
        );
    }
}

/// The summary `action` is authorized with. A speed-up signs a second order beside a live one,
/// so it always takes its explicit review, which a remembered spend authorization can't skip,
/// whatever its summary was built from.
fn authorization_summary(
    action: &SwapAction,
    summary: SpendAuthorizationSummary,
) -> SpendAuthorizationSummary {
    match action {
        SwapAction::Order(approval) if approval.speeds_up() => summary.requiring_explicit_review(),
        _ => summary,
    }
}

/// A swap's name, after the token its receiver gets on a Bridge swap's destination network.
fn bridge_pair_label(labels: &SwapLabels, bridge: Option<&SwapBridgeLabels>) -> String {
    swap_pair_label(
        &labels.sell,
        &bridge.map_or_else(|| labels.buy_symbol.clone(), SwapBridgeLabels::named_token),
    )
}

/// Consecutive bridge provider polls of one order that returned no outcome, and when the next
/// is due.
#[derive(Clone, Copy)]
struct BridgePoll {
    attempts: u32,
    next_at: Instant,
}

/// The wait after `attempts` consecutive bridge polls without an outcome: the observation
/// interval, doubled after each, up to [`MAX_BRIDGE_POLL_INTERVAL`].
fn bridge_poll_interval(attempts: u32) -> Duration {
    SWAP_OBSERVATION_INTERVAL
        .saturating_mul(2_u32.saturating_pow(attempts))
        .min(MAX_BRIDGE_POLL_INTERVAL)
}

/// A random 9-15 s wait before a swap's next poll, chosen afresh for each poll.
fn swap_poll_interval() -> Duration {
    Duration::from_millis(rand::rng().random_range(9_000..=15_000))
}

/// When each tracked swap is next polled. Every swap keeps its own due time, a fresh
/// interval after its previous poll completed, so swaps are polled out of step rather than on
/// one shared tick.
struct SwapSchedule<K> {
    due: BTreeMap<K, Instant>,
    /// Swaps woken since their last poll was dispatched, which its completion leaves due.
    woken: BTreeSet<K>,
    interval: fn() -> Duration,
    /// When the view started, until the first pass.
    start: Option<Instant>,
}

impl<K: Ord + Copy> SwapSchedule<K> {
    fn new() -> Self {
        Self::with_interval(swap_poll_interval, Instant::now())
    }

    const fn with_interval(interval: fn() -> Duration, start: Instant) -> Self {
        Self {
            due: BTreeMap::new(),
            woken: BTreeSet::new(),
            interval,
            start: Some(start),
        }
    }

    /// Keeps the `items` of the swaps due at `now`, and makes each of those swaps next due an
    /// interval from `now` while its poll runs. The swaps of `items` are the tracked ones: a swap
    /// first seen is due an interval from the view's start on the first pass and from `now`
    /// after it, and one absent from `items` is forgotten.
    fn poll_due<T>(&mut self, mut items: Vec<T>, key: impl Fn(&T) -> K, now: Instant) -> Vec<T> {
        let interval = self.interval;
        let seen = self.start.take().unwrap_or(now);
        let tracked = items.iter().map(&key).collect::<BTreeSet<_>>();
        self.due.retain(|swap, _| tracked.contains(swap));
        self.woken.retain(|swap| tracked.contains(swap));
        let mut due = BTreeSet::new();
        for swap in tracked {
            let at = self.due.entry(swap).or_insert_with(|| seen + interval());
            if *at <= now {
                *at = now + interval();
                self.woken.remove(&swap);
                due.insert(swap);
            }
        }
        items.retain(|item| due.contains(&key(item)));
        items
    }

    /// Makes each still tracked swap of `swaps`, whose poll completed at `now`, next due a fresh
    /// interval from `now`, so a poll slower than its interval isn't repeated at once. A swap
    /// woken since its poll was dispatched stays due.
    fn polled(&mut self, swaps: &BTreeSet<K>, now: Instant) {
        for swap in swaps {
            if !self.woken.contains(swap)
                && let Some(at) = self.due.get_mut(swap)
            {
                *at = now + (self.interval)();
            }
        }
    }

    /// Makes `swap` due at `now`.
    fn wake(&mut self, swap: K, now: Instant) {
        self.due.insert(swap, now);
        self.woken.insert(swap);
    }

    fn next_due(&self) -> Option<Instant> {
        self.due.values().min().copied()
    }
}

/// How long a polling loop waits for the next swap due at `next`: at most
/// [`SWAP_OBSERVATION_INTERVAL`].
fn poll_wait(next: Option<Instant>) -> Duration {
    next.map_or(SWAP_OBSERVATION_INTERVAL, |at| {
        at.saturating_duration_since(Instant::now())
    })
    .min(SWAP_OBSERVATION_INTERVAL)
}

/// What the orderbook last reported about one order.
#[derive(Clone, Copy)]
struct SwapOrderHint {
    report: CowOrderStatusReport,
    /// The reported trade's block, read once the order is reported filled.
    trade_block: Option<u64>,
}

/// What this session learned about an order from the block its fill is reported in, before the
/// settlement reached finality depth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwapEarlyHint {
    /// The reported trade block all of it belongs to.
    trade_block: Option<u64>,
    /// Whether that block was read, so it is read once.
    block_read: bool,
    /// What that block holds of the order's settlement. `None` when it holds no trade of the
    /// order or can't be read as a whole block.
    settlement: Option<wallet_ops::SwapSettlementHint>,
    /// A Bridge order's provider's last report about the delivery.
    report: Option<wallet_ops::SwapBridgeReport>,
    /// Whether the order's fill block is also read on its destination chain: a private Bridge
    /// delivery to a chain enabled here, as the pass that asks finds it.
    reads_fill: bool,
}

impl SwapEarlyHint {
    /// Whether the reported trade block is known and still to be read.
    const fn unread(&self) -> bool {
        self.trade_block.is_some() && !self.block_read
    }

    /// Whether the provider's report ends the early polling of the reported block: a refund, a
    /// failure, or a delivery. The delivery of an order whose fill block is also read ends it
    /// only once that block was read.
    const fn ends_polling(&self) -> bool {
        use wallet_ops::SwapBridgeReport::{Delivered, Failed, Pending, Refunded};
        match self.report {
            Some(Delivered { seen, .. }) => seen.is_some() || !self.reads_fill,
            Some(Refunded | Failed) => true,
            Some(Pending) | None => false,
        }
    }

    /// How a part of the reported settlement shows, with `amount` picking it from the read
    /// block: looked for until the block is read, then detected with its amount, or not found.
    fn seen(
        hint: Option<&Self>,
        amount: impl FnOnce(wallet_ops::SwapSettlementHint) -> Option<String>,
    ) -> SwapSeen {
        match hint {
            Some(hint) if hint.block_read => hint
                .settlement
                .and_then(amount)
                .map_or(SwapSeen::NotFound, |amount| {
                    SwapSeen::Detected(Some(amount))
                }),
            _ => SwapSeen::Looking,
        }
    }
}

/// The delivery of `order` while its reported settlement is read early, and the reported
/// trade's block: the orderbook reports the order filled, `stage` still shows its swap open,
/// and no settlement is recorded for it. For a Bridge order that is its hand-off, which
/// `swap_bridges_to_track` tracks once recorded.
fn early_order(
    stage: SwapStage,
    order: &SwapOrderRecord,
    hints: &HashMap<OrderUid, SwapOrderHint>,
) -> Option<(SwapDelivery, Option<u64>)> {
    if !matches!(
        stage,
        SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
    ) {
        return None;
    }
    let hint = hints
        .get(&order.uid())
        .filter(|hint| hint.report.status == CowOrderStatusHint::Fulfilled)?;
    let delivery = order.delivery();
    let observed = order.observations();
    let recorded = match delivery {
        SwapDelivery::Bridge(_) | SwapDelivery::NonEvmBridge(_) => {
            observed.bridge_handoff.is_some()
        }
        SwapDelivery::Reshield | SwapDelivery::External { .. } => observed.traded.is_some(),
    };
    (!recorded).then_some((delivery, hint.trade_block))
}

/// The early request `order` is due in this pass, with its bridge provider and what is known
/// for its reported block. Every order reads that block once it is known. A Bridge order is
/// then asked about until its provider reports an end: NEAR Intents by its deposit address,
/// Across with the deposit the block's hand-off holds. With `reads_fill`, the order's fill block
/// is read on its destination chain too, and a reported delivery is asked about until that
/// block was read. `None` when neither is left to do, and while a request backs off.
fn early_request(
    stage: SwapStage,
    order: &SwapOrderRecord,
    tracking: &SwapTracking,
    reads_fill: bool,
    now: Instant,
) -> Option<(Option<BridgeProvider>, SwapEarlyHint)> {
    let (delivery, trade_block) = early_order(stage, order, &tracking.order_hints)?;
    let uid = order.uid();
    if tracking
        .bridge_polls
        .get(&uid)
        .is_some_and(|poll| poll.next_at > now)
    {
        return None;
    }
    let hint = SwapEarlyHint {
        reads_fill,
        ..tracking
            .early_hints
            .get(&uid)
            .copied()
            .filter(|hint| hint.trade_block == trade_block)
            .unwrap_or(SwapEarlyHint {
                trade_block,
                block_read: false,
                settlement: None,
                report: None,
                reads_fill,
            })
    };
    let provider = delivery.bridge_provider();
    let asked = !hint.ends_polling()
        && match provider {
            Some(BridgeProvider::NearIntents) => true,
            Some(BridgeProvider::Across) => hint
                .settlement
                .is_some_and(|settlement| settlement.handoff.is_some()),
            None => false,
        };
    (hint.unread() || asked).then_some((provider, hint))
}

pub(super) struct PrivateSwapsView {
    root: WeakEntity<WalletRoot>,
    origin_chain_id: u64,
    session: Option<Arc<WalletSession>>,
    owner: Option<Arc<ExecutorOwner>>,
    public_records: Vec<(u64, ExecutorRecord)>,
    public_tracking: BTreeSet<(u64, ExecutorOperationId, SwapUseId)>,
    /// When each Public account swap's status is next checked in the background.
    public_schedule: SwapSchedule<(u64, ExecutorOperationId, SwapUseId)>,
    /// Confirmed destination token balances from explicit held-proceeds checks this session.
    public_destination_balances:
        BTreeMap<(u64, ExecutorOperationId, SwapUseId), (U256, alloy::eips::BlockNumHash)>,
    /// The Railgun contract of each chain in `public_records`, read with them. The Private
    /// tab asks for the shown swaps while the root renders, when the root can't be read.
    public_railgun: BTreeMap<u64, Address>,
    /// The networks whose private sync has been ready in this session. Such a network syncs
    /// again from time to time, which doesn't put a quote for a delivery there on hold.
    public_synced: BTreeSet<u64>,
    /// Background status checks of a Public account swap that failed in a row.
    public_tracking_failures: BTreeMap<(ExecutorOperationId, SwapUseId), u32>,
    /// The open orders whose signed approval the latest status check found used up.
    public_permit_used_up: BTreeSet<(ExecutorOperationId, SwapUseId)>,
    public_authorization: Option<Arc<PublicSwapAuthorization>>,
    public_execution: Option<form::public_source::PublicSwapExecution>,
    public_job: Option<Task<()>>,
    /// The swap `public_job` works on, which its detail shows at work. `None` for any other
    /// job, such as a withdrawal's.
    public_running: Option<SwapIdentity>,
    /// What a Public account swap is doing between its review and its order or deposit.
    public_status: Option<gpui::SharedString>,
    runtime: tokio::runtime::Handle,
    active: bool,
    /// Swap executor records only, newest first.
    records: Vec<ExecutorRecord>,
    /// Accounts whose pending setup private sync showed in a transaction recorded for it, as
    /// of the last reload; `None` before the first.
    setup_inclusions: Option<BTreeSet<ExecutorOperationId>>,
    /// Counts setups private sync newly showed. The observation loop starts its next pass
    /// when it changes, so wakes during a pass coalesce into one follow-up pass.
    observation_wake: watch::Sender<u64>,
    /// When each swap's pages are next observed, by the swap's operation.
    observation_schedule: SwapSchedule<ExecutorOperationId>,
    tracking: BTreeMap<ExecutorOperationId, SwapTracking>,
    /// Each private Bridge swap's destination stealth account record, by the swap's operation,
    /// as last read from the destination network's own executor records. An entry exists once
    /// that network's session is loaded, and holds `None` when no such record is there.
    destinations: BTreeMap<SwapIdentity, Option<ExecutorRecord>>,
    /// Each loaded account's attribution evidence, from its own network's owner, as read when
    /// its record was last loaded. It decides whether a recovery returned stranded funds.
    attributions: BTreeMap<ExecutorOperationId, wallet_ops::ExecutorAttribution>,
    /// The owner's answer on speeding up each account's latest order, for the orders that
    /// hadn't locally expired when the records were last read. An order without an entry
    /// can't be sped up.
    speed_up_refusals: BTreeMap<ExecutorOperationId, (OrderUid, Option<SwapSpeedUpRefusal>)>,
    /// The wait for the open detail's Speed up delay to end, and the unix time it ends at.
    speed_up_reveal: Option<(u64, Task<()>)>,
    /// My orders' open swaps, counted again whenever the view changes rather than on every
    /// frame.
    open_orders: usize,
    /// The open swap dialog's view, and its focus, which tells it apart from unrelated modal
    /// work.
    dialog: Option<dialog::SwapDialog>,
    /// My orders' filter; `None` shows every swap.
    orders_filter: Option<model::SwapOrderGroup>,
    orders_list: Option<Entity<gpui_component::list::ListState<dialog::SwapOrdersDelegate>>>,
    form: Option<SwapForm>,
    /// The open retry of one account's setup, over the swap's detail.
    setup_retry: Option<form::SetupRetryForm>,
    cancel: Option<progress::CancelQuote>,
    pending_authorization: Option<Arc<SwapAuthorization>>,
    /// The order's terms changed before signing. Nothing was signed; the swap is quoted again
    /// and its review reopens with the change named.
    reapproval: Option<(ExecutorOperationId, SwapReviewChange)>,
    /// What cancelling a swap's preparation in this session left of its claims, which the
    /// swap's detail reports until the dialog shows something else.
    cancelled: Option<progress::CancelledPreparation>,
    job: Option<SwapJob>,
    job_revision: u64,
    error: Option<String>,
    origin_changes: Task<()>,
    _polling: Task<()>,
    _hint_polling: Task<()>,
}

impl Drop for PrivateSwapsView {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.abort.abort();
        }
    }
}

impl WalletRoot {
    pub(super) fn ensure_private_swaps(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let origin_chain_id = self.last_evm_chain;
        let Some(chain) = self.effective_chain_configs.get(origin_chain_id) else {
            self.clear_private_swaps(cx);
            return;
        };
        if self.view_session.is_none()
            || (chain.swap_profile().is_none() && chain.bridge_origin_profile().is_none())
        {
            self.clear_private_swaps(cx);
            return;
        }
        let session = self.stealth_session().filter(|session| {
            session.chain_id == origin_chain_id && chain.swap_profile().is_some()
        });
        // Public drafts and destination execution do not depend on the origin's private
        // session. Attach one when it arrives without replacing their view or capabilities.
        if let Some(panel) = self
            .private_swaps
            .as_mut()
            .filter(|panel| panel.origin_chain_id == origin_chain_id && panel.session.is_none())
            && let Some(session) = session.as_ref()
        {
            let owner = session.executor_owner();
            panel.session = Some(session.clone());
            panel.view.update(cx, |view, cx| {
                view.session = Some(session.clone());
                view.owner = owner;
                view.origin_changes = PrivateSwapsView::watch_origin_changes(
                    view.session.as_ref(),
                    view.owner.as_ref(),
                    cx,
                );
                view.reload_records();
                cx.notify();
            });
            return;
        }
        if self.private_swaps.as_ref().is_some_and(|panel| {
            panel.origin_chain_id == origin_chain_id
                && match (&panel.session, &session) {
                    (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
                    (None, None) => true,
                    _ => false,
                }
        }) {
            return;
        }
        self.clear_private_swaps(cx);
        let owner = session
            .as_ref()
            .and_then(|session| session.executor_owner());
        let root = cx.entity().downgrade();
        let runtime = self.runtime.clone();
        let view = cx.new(|cx| {
            PrivateSwapsView::new_for_origin(
                root,
                origin_chain_id,
                session.clone(),
                owner,
                runtime,
                window,
                cx,
            )
        });
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        let refresh = view.downgrade();
        cx.defer(move |cx| {
            let _ = refresh.update(cx, |view, cx| {
                view.refresh_public_swap_records(cx);
                cx.notify();
            });
        });
        self.private_swaps = Some(PrivateSwapsPanel {
            origin_chain_id,
            session,
            view,
        });
    }

    pub(super) fn clear_private_swaps(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(panel) = self.private_swaps.take() {
            panel.view.update(cx, |view, cx| {
                view.active = false;
                if let Some(job) = view.job.take() {
                    job.abort.abort();
                }
                view.job_revision = view.job_revision.wrapping_add(1);
                view.dialog = None;
                view.form = None;
                view.setup_retry = None;
                view.cancel = None;
                view.pending_authorization = None;
                view.reapproval = None;
                view.cancelled = None;
                view.tracking.clear();
                view.destinations.clear();
                view.attributions.clear();
                view.speed_up_refusals.clear();
                view.records.clear();
                view.public_records.clear();
                view.public_tracking.clear();
                view.public_destination_balances.clear();
                view.public_tracking_failures.clear();
                view.public_permit_used_up.clear();
                view.public_authorization = None;
                view.public_execution = None;
                view.public_job = None;
                view.public_running = None;
                cx.notify();
            });
        }
    }

    /// The swap view of the current executor session.
    pub(super) fn private_swaps_view(&self) -> Option<Entity<PrivateSwapsView>> {
        self.private_swaps
            .as_ref()
            .filter(|panel| {
                Some(panel.origin_chain_id) == self.selected_evm_chain()
                    && self.view_session.is_some()
                    && panel
                        .session
                        .as_ref()
                        .is_none_or(|session| self.stealth_session_is_current(session))
            })
            .map(|panel| panel.view.clone())
    }

    /// Read the swap form's balances again once a private snapshot of `chain_id` replaced the
    /// root's. The swap view hears of the same observation on its own and may read the root
    /// before this update, so it reads again after it.
    pub(super) fn refresh_private_swap_assets(&self, chain_id: u64, cx: &mut Context<'_, Self>) {
        if self
            .private_swaps
            .as_ref()
            .is_none_or(|panel| panel.origin_chain_id != chain_id)
        {
            return;
        }
        let Some(view) = self.private_swaps_view() else {
            return;
        };
        let view = view.downgrade();
        cx.defer(move |cx| {
            let _ = view.update(cx, |view, cx| {
                if view.form.is_some() {
                    view.refresh_form_assets(cx);
                    cx.notify();
                }
            });
        });
    }

    /// The selected chain's swap profile, when this wallet can run executor operations there.
    pub(super) fn private_swap_profile(&self) -> Option<SwapProfile> {
        self.private_swaps_view()?;
        self.effective_chain_configs
            .get(self.last_evm_chain)?
            .swap_profile()
    }

    pub(super) fn open_public_swap_form(
        &mut self,
        public_account_uuid: &str,
        sell: Option<Address>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self
            .public_accounts
            .iter()
            .find(|account| {
                account.public_account_uuid == public_account_uuid
                    && account.status == wallet_ops::vault::PublicAccountStatus::Active
                    && account.scope != wallet_ops::vault::PublicAccountScope::Global
                    && !matches!(
                        account.source,
                        wallet_ops::vault::PublicAccountSource::ExecutorDerived(_)
                    )
            })
            .cloned()
        else {
            return;
        };
        self.ensure_private_swaps(window, cx);
        let Some(view) = self.private_swaps_view() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            view.update(cx, |view, cx| {
                view.open_public_form(account, sell.unwrap_or(Address::ZERO), window, cx);
            });
        });
    }

    pub(super) fn open_private_swap_form(
        &self,
        sell: Address,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(view) = self.private_swaps_view() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            view.update(cx, |view, cx| view.open_new_form(sell, window, cx));
        });
    }
}

/// Whether a private asset row, or the header, can start a swap of `token`. `None` hides the
/// action: the chain has no swap profile. `Err` disables it with the reason.
pub(super) fn swap_entry_availability(
    profile: Option<&SwapProfile>,
    token: Option<Address>,
    spendable: bool,
    actions_available: bool,
) -> Option<Result<(), &'static str>> {
    use wallet_ops::settings::{SwapIneligibility, SwapTokenRole};
    let profile = profile?;
    Some(if actions_available {
        match token.map(|token| profile.token_eligibility(token, &SwapTokenRole::Sell)) {
            None | Some(SwapTokenEligibility::Ineligible(SwapIneligibility::NativeAsset)) => {
                Err("Native assets can't be swapped privately")
            }
            Some(SwapTokenEligibility::Ineligible(_)) => {
                Err("This token isn't supported for private swaps")
            }
            Some(SwapTokenEligibility::Eligible) if !spendable => {
                Err("No spendable private balance for this token")
            }
            Some(SwapTokenEligibility::Eligible) => Ok(()),
        }
    } else {
        Err("Available after wallet session starts")
    })
}

/// Whether a source use's saved approval has the swap set its destination account up, as
/// recorded by the destination account the approval binds.
fn sets_up_destination(swap_use: &SwapUseRecord) -> bool {
    swap_use.approval().is_some_and(|approval| {
        approval
            .accounts
            .and_then(|accounts| accounts.destination)
            .is_some_and(|account| account.setup)
    })
}

/// The destination stealth accounts on `chain_id` whose shield a verified fill of the swap
/// account `operation` ran while their recorded execution nonce hasn't passed it. Read from
/// both owners' records, without a network request.
fn delivered_destinations(
    origin: &ExecutorOwner,
    destination: &ExecutorOwner,
    operation: ExecutorOperationId,
    chain_id: u64,
) -> Vec<ExecutorOperationId> {
    let record = origin.records().ok().and_then(|records| {
        records
            .into_iter()
            .find(|record| record.operation() == operation)
    });
    let (Some(record), Ok(accounts)) = (record, destination.records()) else {
        return Vec::new();
    };
    let orders = record
        .swap()
        .map_or(&[][..], wallet_ops::vault::SwapOperationRecord::orders);
    orders
        .iter()
        .filter(|order| {
            matches!(
                order.observations().bridge_outcome,
                Some(SwapBridgeOutcome::DeliveredVerified { shielded: true, .. })
            ) && matches!(
                order.delivery(),
                SwapDelivery::Bridge(delivery) if delivery.destination_chain == chain_id
            )
        })
        .filter_map(|order| {
            let swap_use = order
                .use_id()
                .unwrap_or_else(|| SwapUseId::first(operation));
            let (_, account) = model::swap_use_destination(&record, swap_use)?;
            accounts
                .iter()
                .find(|candidate| candidate.operation() == account)
                .filter(|candidate| has_unpassed_shield(candidate))
                .map(ExecutorRecord::operation)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Whether a destination shield `record`'s account signed sits at or above its recorded
/// execution nonce, or the record has none, so the records can't tell that the shield ran.
fn has_unpassed_shield(record: &ExecutorRecord) -> bool {
    let observed = record
        .nonce_observation()
        .map(ExecutorNonceObservation::nonce);
    record.issued().iter().any(|payload| {
        payload.purpose() == ExecutorPayloadPurpose::SwapDestinationShield
            && observed.is_none_or(|nonce| nonce <= payload.nonce())
    })
}

/// Whether `record`'s setup is recorded as executed for the accepted delegate of `chain_id`,
/// the record's own network.
fn setup_recorded_executed_on(chain_id: u64, record: &ExecutorRecord) -> bool {
    ExecutorProfile::accepted(chain_id, record.delegate())
        .is_some_and(|profile| swap_setup_recorded_executed(record, profile))
}

/// The destination record and the freshness reported by its matching claim and saved
/// source approval. Callers choose which record is available and how missing claims fall back.
struct DestinationAccountMetadata<'a> {
    record: Option<&'a ExecutorRecord>,
    claimed_fresh: Option<bool>,
    approved_fresh: Option<bool>,
}

impl DestinationAccountMetadata<'_> {
    /// A loaded record is authoritative for setup and review; without its claim, treat the
    /// account as fresh. Only an unloaded record falls back to the saved approval.
    fn fresh_from_record_or_approval(&self) -> bool {
        if self.record.is_some() {
            self.claimed_fresh.unwrap_or(true)
        } else {
            self.approved_fresh.unwrap_or(true)
        }
    }

    /// Cancellation retains the approval's account choice when the loaded record has no
    /// matching claim, so its explanation can still distinguish an existing account.
    fn fresh_from_claim_or_approval(&self) -> bool {
        self.claimed_fresh.or(self.approved_fresh).unwrap_or(true)
    }
}

fn destination_account_metadata<'a>(
    record: Option<&'a ExecutorRecord>,
    swap_use: Option<SwapUseId>,
    approval: Option<&SwapApproval>,
) -> DestinationAccountMetadata<'a> {
    DestinationAccountMetadata {
        record,
        claimed_fresh: swap_use
            .and_then(|id| record?.swap_use(id))
            .map(SwapUseRecord::is_fresh),
        approved_fresh: approval
            .and_then(|approval| approval.accounts?.destination)
            .map(|account| account.setup),
    }
}

/// The stage `record` gives on `chain_id`, its own network, with `setup` this session's last
/// observation of its setup and `attribution` its owner's evidence for it.
fn account_stage(
    record: &ExecutorRecord,
    chain_id: u64,
    setup: Option<SwapSetupStatus>,
    submitting: bool,
    attribution: Option<&wallet_ops::ExecutorAttribution>,
) -> SwapStage {
    swap_stage(
        record,
        match setup {
            // A setup recorded as executed for the accepted delegate is ready without
            // this session reading the account, so `swap_stage` uses the recorded
            // outcome. That readiness can outlast a reorg until reconciliation; order
            // preparation checks the delegation afresh before anything is signed.
            None | Some(SwapSetupStatus::Pending)
                if setup_recorded_executed_on(chain_id, record) =>
            {
                None
            }
            // Any other stored execution waits for this session's account check, and a
            // failure that check reported stands.
            setup => setup.or(Some(SwapSetupStatus::Pending)),
        },
        submitting,
        attribution,
    )
}

/// How far one stealth account's setup is, apart from the other account of a private Bridge
/// swap. A setup job sends both setups, so only an account that isn't set up shows as
/// submitting.
fn account_setup_progress(
    record: &ExecutorRecord,
    chain_id: u64,
    setup: Option<SwapSetupStatus>,
    submitting: bool,
) -> SwapSetupProgress {
    // No setup stage depends on what ran at a later nonce.
    let stage = account_stage(record, chain_id, setup, false, None);
    if submitting
        && matches!(
            stage,
            SwapStage::SetupNotSent | SwapStage::SetupPending | SwapStage::SetupFailed
        )
    {
        SwapSetupProgress::Submitting
    } else {
        model::swap_setup_progress(stage)
    }
}

/// The accounts among `records` whose pending setup private sync's `utxos` show in a
/// transaction recorded for it: a swap's setup before its order, or the swap's own setup an
/// order was placed on while the record leaves that setup unresolved.
fn synced_setup_inclusions(
    records: &[ExecutorRecord],
    utxos: &[wallet_ops::UtxoOutput],
) -> BTreeSet<ExecutorOperationId> {
    records
        .iter()
        .filter(|record| {
            (record.swap().is_none() || wallet_ops::own_pending_swap_setup(record).is_some())
                && matches!(
                    model::swap_setup_confirmation(record, utxos),
                    Some(model::SwapSetupConfirmation::Included(_))
                )
        })
        .map(ExecutorRecord::operation)
        .collect()
}

/// Whether a setup was signed for the record's account, so its outcome can be read.
fn has_issued_setup(record: &ExecutorRecord) -> bool {
    record
        .issued()
        .iter()
        .any(|payload| payload.purpose() == wallet_ops::vault::ExecutorPayloadPurpose::Operation)
}

struct ObservationPage {
    operation: ExecutorOperationId,
    /// An orderbook hint or a previously verified trade locates a whole-block receipt read.
    settlement: Option<(OrderUid, u64)>,
    /// The blocks of an order's history to read. `None` while no order exists yet: the setup
    /// is what can change, and it is read from the account at the confirmed block.
    range: Option<std::ops::Range<u64>>,
    /// Set when `operation` is a private Bridge swap's destination stealth account, which
    /// its own network's owner observes.
    destination: Option<DestinationPage>,
    /// The swap's route, which the page is read over.
    route: SwapRoute,
}

impl ObservationPage {
    /// The swap the page is observed for, whose schedule it follows: a destination account's
    /// page follows its swap's.
    fn swap(&self) -> ExecutorOperationId {
        self.destination
            .as_ref()
            .map_or(self.operation, |destination| destination.origin)
    }
}

/// A swap's own network route, as a page or hint request carries it to the runtime. Its
/// orderbook client's HTTP client carries the swap's RPC reads too, so a route replaced by an
/// explicit status check moves both. A swap that keeps none opens one here, once for both
/// polling loops; a route that can't open fails the read, which never falls back to a shared
/// route.
#[derive(Clone)]
struct SwapRoute {
    http: wallet_ops::HttpContext,
    orderbook: Arc<tokio::sync::OnceCell<CowOrderbookClient>>,
}

impl SwapRoute {
    /// The swap's orderbook client, opened through `owner`, the swap's own network's.
    async fn orderbook(&self, owner: &ExecutorOwner) -> eyre::Result<CowOrderbookClient> {
        self.orderbook
            .get_or_try_init(|| async {
                owner.swap_orderbook_client(&self.http.operation_http_client().await?)
            })
            .await
            .cloned()
    }
}

struct DestinationPage {
    owner: Arc<ExecutorOwner>,
    /// The swap the destination account serves, on this view's network.
    origin: ExecutorOperationId,
}

struct ObservationResult {
    operation: ExecutorOperationId,
    /// The end of the page read. `None` for a setup, which reads no page.
    range_end: Option<u64>,
    /// A setup's observation; `None` for an order's page.
    outcome: eyre::Result<Option<SwapSetupObservation>>,
    /// A destination account's swap.
    destination: Option<ExecutorOperationId>,
    /// The cell of the swap's route the page was read over.
    route: Arc<tokio::sync::OnceCell<CowOrderbookClient>>,
}

struct HintRequest {
    operation: ExecutorOperationId,
    uid: OrderUid,
    /// Whether to ask the orderbook about `uid`.
    hint: bool,
    /// Whether to read the traded order's executed fee, once per session.
    fee: bool,
    /// The swap's own route.
    route: SwapRoute,
    /// Handed-off Bridge orders to poll, with their destination chain. A non-EVM network has
    /// none.
    bridges: Vec<(OrderUid, Option<EffectiveChainConfig>)>,
    /// Set when `uid` is due an early request: a Bridge order's provider, and what is known
    /// for its reported trade block.
    early: Option<(Option<BridgeProvider>, SwapEarlyHint)>,
    /// The destination chain of a private Bridge order due an early request, when it is
    /// enabled here, where the order's fill block is read.
    destination: Option<Box<EffectiveChainConfig>>,
}

struct HintResult {
    operation: ExecutorOperationId,
    uid: OrderUid,
    /// The cell of the swap's route the request went over.
    route: Arc<tokio::sync::OnceCell<CowOrderbookClient>>,
    /// The executed fee was asked for, whether or not the read succeeded.
    fee: bool,
    /// Each polled Bridge order, and whether it has an outcome now.
    bridges: Vec<(OrderUid, bool)>,
    /// The report and trade block, or `None` when the orderbook couldn't be asked.
    report: Option<(CowOrderStatusReport, Option<u64>)>,
    /// What is known early about `uid` after this pass, for the trade block that was asked
    /// about. A failed read or request leaves what the request held.
    early: Option<SwapEarlyHint>,
}

/// Ask the orderbook about each order through its swap's own route, sending only the order
/// UID. A failed request leaves the last report; the client logs failures without URLs. A
/// traded order's executed fee is read once, and the owner persists it; a failed read is
/// dropped.
/// Bridge providers are polled on the same route, and the owner persists any outcome. The
/// caller backs off after a poll without an outcome, including a failed one.
/// An early request reads the reported trade block's settlement as whole-block data, for an
/// order that hasn't read it, and asks a Bridge order's provider on the same route until it
/// reports an end: NEAR Intents always, Across only with a hand-off. With a destination chain,
/// the owner also reads a reported delivery's fill block there. Nothing of an early request is
/// persisted.
async fn fetch_order_hints(
    owner: Arc<ExecutorOwner>,
    requests: Vec<HintRequest>,
) -> Vec<HintResult> {
    let mut results = Vec::with_capacity(requests.len());
    for request in requests {
        let client = request.route.orderbook(&owner).await.ok();
        let report = if request.hint
            && let Some(client) = &client
            && let Ok(status) = client.order_status_hint(&request.uid).await
        {
            let trade_block = if status.status == CowOrderStatusHint::Fulfilled {
                client.order_trade_block(&request.uid).await.ok().flatten()
            } else {
                None
            };
            Some((status, trade_block))
        } else {
            None
        };
        if request.fee
            && let Some(client) = &client
        {
            let _ =
                Box::pin(owner.observe_swap_executed_fee(request.operation, request.uid, client))
                    .await;
        }
        // Without clients, every poll counts as one without an outcome. The swap's RPC reads go
        // over the same route.
        let early_provider = request.early.and_then(|(provider, _)| provider);
        let clients = if request.bridges.is_empty() && early_provider.is_none() {
            None
        } else {
            client
                .as_ref()
                .and_then(|client| Some((owner.swap_bridge_clients(client).ok()?, client.http())))
        };
        let mut bridges = Vec::with_capacity(request.bridges.len());
        for (uid, destination) in &request.bridges {
            let outcome = if let Some((clients, rpc)) = &clients {
                matches!(
                    Box::pin(owner.observe_swap_bridge(
                        request.operation,
                        *uid,
                        clients,
                        destination.as_ref(),
                        rpc,
                    ))
                    .await,
                    Ok(Some(_))
                )
            } else {
                false
            };
            bridges.push((*uid, outcome));
        }
        let early = match request.early {
            Some((provider, mut hint)) => {
                if !hint.block_read
                    && let Some(block) = hint.trade_block
                    && let Some(client) = &client
                    && let Ok(settlement) = Box::pin(owner.read_swap_settlement_hint(
                        request.operation,
                        request.uid,
                        block,
                        client.http(),
                    ))
                    .await
                {
                    hint.block_read = true;
                    hint.settlement = settlement;
                }
                let handoff = hint.settlement.and_then(|settlement| settlement.handoff);
                if let Some(provider) = provider
                    && !hint.ends_polling()
                    && (provider == BridgeProvider::NearIntents || handoff.is_some())
                    && let Some((clients, rpc)) = &clients
                    && let Ok(report) = Box::pin(owner.swap_bridge_report(
                        request.operation,
                        request.uid,
                        handoff,
                        clients,
                        request.destination.as_deref(),
                        rpc,
                    ))
                    .await
                {
                    hint.report = Some(report);
                }
                Some(hint)
            }
            None => None,
        };
        results.push(HintResult {
            operation: request.operation,
            uid: request.uid,
            route: request.route.orderbook,
            fee: request.fee,
            bridges,
            report,
            early,
        });
    }
    results
}

/// A setup reads its account once. A user-requested cancellation check reconciles its
/// account. Routine order confirmation reads only the settlement block, matching its receipts
/// locally.
async fn observe_pages(
    owner: Arc<ExecutorOwner>,
    pages: Vec<ObservationPage>,
) -> Vec<ObservationResult> {
    let mut results = Vec::with_capacity(pages.len());
    for page in pages {
        // The swap's route is opened on its own network, also for a destination page.
        let route = page.route.orderbook(&owner).await;
        let owner = page
            .destination
            .as_ref()
            .map_or(&owner, |destination| &destination.owner);
        let outcome = match route {
            Ok(orderbook) => {
                let client = orderbook.http();
                if let Some((uid, block)) = page.settlement {
                    Box::pin(owner.observe_swap_settlement(page.operation, uid, block, client))
                        .await
                        .map(|()| None)
                } else if let Some(range) = page.range.clone() {
                    Box::pin(owner.observe_swap(page.operation, range, client))
                        .await
                        .map(|_| None)
                } else {
                    Box::pin(owner.observe_swap_setup(page.operation, client))
                        .await
                        .map(Some)
                }
            }
            Err(error) => Err(error),
        };
        results.push(ObservationResult {
            operation: page.operation,
            range_end: page.range.map(|range| range.end),
            outcome,
            destination: page.destination.map(|destination| destination.origin),
            route: page.route.orderbook,
        });
    }
    results
}

/// `amount` of a non-EVM delivery's token, in its decimals and with its symbol from the record.
pub(super) fn non_evm_amount(delivery: &NonEvmDelivery, amount: U256) -> String {
    format!(
        "{} {}",
        railgun_ui::format_token_amount(amount, delivery.decimals),
        delivery.symbol
    )
}

/// The icon of `network`'s token at `contract`: the network's own for its native asset, and
/// for another, the icon of a known token with exactly its `symbol`. `None` without one.
pub(super) fn non_evm_token_icon(
    network: wallet_ops::bridge::NonEvmNetwork,
    contract: Option<&str>,
    symbol: &str,
) -> Option<crate::assets::WalletIconSource> {
    let path = match contract {
        None => network.icon_asset_path().to_owned(),
        Some(_) => railgun_ui::symbol_token_icon_asset_path(symbol)?,
    };
    Some(crate::assets::WalletIconSource::embedded(path))
}

/// "0x7b2E…91F4": a receiver's address, shortened, in its checksummed case.
pub(super) fn short_receiver(receiver: Address) -> String {
    let checksummed = receiver.to_checksum(None);
    format!("{}…{}", &checksummed[..6], &checksummed[38..])
}

/// Local wall-clock time of a Unix timestamp, such as "14:32".
pub(super) fn local_time_label(at: u64) -> String {
    let at = std::time::UNIX_EPOCH + Duration::from_secs(at);
    chrono::DateTime::<chrono::Local>::from(at)
        .format("%H:%M")
        .to_string()
}

/// Local date and time of a Unix timestamp: "Today 14:21", "Yesterday 18:05", "Sep 24 10:42",
/// or "Sep 24, 2025 10:42" in an earlier year.
pub(super) fn local_date_time_label(at: u64) -> String {
    let at =
        chrono::DateTime::<chrono::Local>::from(std::time::UNIX_EPOCH + Duration::from_secs(at));
    format!("{} {}", day_label(at.date_naive()), at.format("%H:%M"))
}

/// What a setup says whose fee `required` in `token` is above the approved `maximum`. `control`
/// opens the review of the new fee limit.
fn setup_fee_increased(
    chain_id: u64,
    registry: Option<&EffectiveTokenRegistry>,
    token: Address,
    (maximum, required): (U256, U256),
    control: &str,
) -> String {
    let amount =
        |value| super::format_exact_token_amount_for_display(chain_id, token, value, registry);
    format!(
        "Setup fee increased. Approved maximum: {}. New estimate: {}, exceeding the maximum by {}. {control} to review the new fee limit.",
        amount(maximum),
        amount(required),
        amount(required - maximum),
    )
}

/// What a setup says that would pay its fee in `token` when the limit was approved as `amount`
/// of `approved`, another token. The two amounts don't compare. `control` opens the review of
/// the new fee limit.
fn setup_fee_token_changed(
    chain_id: u64,
    registry: Option<&EffectiveTokenRegistry>,
    (approved, amount): (Address, U256),
    token: Address,
    control: &str,
) -> String {
    format!(
        "Setup fee token changed. Approved: up to {}. This setup pays in {}. {control} to review the new fee limit.",
        super::format_exact_token_amount_for_display(chain_id, approved, amount, registry),
        super::token_display_label(chain_id, token, registry),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wallet_ops::settings::build_effective_chain_configs;

    /// A fee in another token names the approved limit in the approved token and the token the
    /// setup pays in, and both fee messages name the control that reviews the new limit.
    #[test]
    fn setup_fee_messages_name_the_approved_token_and_the_reviewing_control() {
        let (approved, paid) = (Address::repeat_byte(0xaa), Address::repeat_byte(0xbb));
        // Without a token registry an amount names its token by address.
        let label = |token| railgun_ui::short_address(&token);
        for control in ["Continue", "Retry setup"] {
            assert_eq!(
                setup_fee_token_changed(1, None, (approved, U256::from(520_000)), paid, control),
                format!(
                    "Setup fee token changed. Approved: up to 520000 raw token units ({}). This setup pays in {}. {control} to review the new fee limit.",
                    label(approved),
                    label(paid)
                )
            );
            let increased = setup_fee_increased(
                1,
                None,
                approved,
                (U256::from(520_000), U256::from(600_000)),
                control,
            );
            assert!(
                increased.ends_with(&format!(
                    "exceeding the maximum by 80000 raw token units ({}). {control} to review the new fee limit.",
                    label(approved)
                )),
                "{increased}"
            );
        }
    }

    #[test]
    fn swap_entry_is_hidden_without_a_profile_and_explains_why_a_token_is_disabled() {
        let chains =
            build_effective_chain_configs(&wallet_ops::settings::WalletSettings::default())
                .expect("built-in chains");
        let profile = chains
            .get(1)
            .and_then(wallet_ops::settings::EffectiveChainConfig::swap_profile)
            .expect("built-in mainnet swap profile");
        let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .expect("WETH");

        for token in [weth, Address::repeat_byte(0x42)] {
            assert_eq!(
                swap_entry_availability(Some(&profile), Some(token), true, true),
                Some(Ok(()))
            );
        }
        // Native assets, empty balances and a stopped session keep the action visible but
        // disabled with a reason.
        for (token, spendable, active) in [
            (Address::ZERO, true, true),
            (weth, false, true),
            (weth, true, false),
        ] {
            assert!(matches!(
                swap_entry_availability(Some(&profile), Some(token), spendable, active),
                Some(Err(_))
            ));
        }
        assert_eq!(swap_entry_availability(None, Some(weth), true, true), None);
    }

    #[test]
    fn bridge_polls_back_off_to_a_capped_interval() {
        assert_eq!(bridge_poll_interval(0), SWAP_OBSERVATION_INTERVAL);
        assert_eq!(bridge_poll_interval(1), SWAP_OBSERVATION_INTERVAL * 2);
        assert_eq!(bridge_poll_interval(5), MAX_BRIDGE_POLL_INTERVAL);
        assert_eq!(bridge_poll_interval(u32::MAX), MAX_BRIDGE_POLL_INTERVAL);
    }

    /// Two swaps tracked together are each polled only once due, a fresh 9-15 s after their
    /// previous poll or first sight, so never on a shared tick; catch-up makes one due at once.
    #[test]
    fn swaps_are_polled_out_of_step_within_their_intervals() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        fn scripted() -> Duration {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            const SECS: [u64; 5] = [9, 14, 15, 11, 12];
            Duration::from_secs(SECS[NEXT.fetch_add(1, Ordering::Relaxed) % SECS.len()])
        }
        let bounds = Duration::from_secs(9)..=Duration::from_secs(15);
        for _ in 0..1_000 {
            assert!(bounds.contains(&swap_poll_interval()));
        }

        let base = Instant::now();
        let mut schedule = SwapSchedule::with_interval(scripted, base);
        let mut polls = BTreeMap::<char, Vec<Duration>>::new();
        // The loop is driven once a second; a swap's page runs only when it is due.
        for second in 0..=60 {
            let now = base + Duration::from_secs(second);
            let due = schedule.poll_due(vec!['a', 'b'], |swap| *swap, now);
            assert!(due.len() <= 1, "both swaps polled at {second} s");
            for swap in due {
                polls.entry(swap).or_default().push(now - base);
            }
            assert!(schedule.next_due().is_some_and(|next| next > now));
        }
        assert_ne!(polls.get(&'a'), polls.get(&'b'));
        for (swap, times) in &polls {
            assert!(times.len() >= 4, "{swap}: {times:?}");
            let mut previous = Duration::ZERO;
            for &at in times {
                assert!(
                    bounds.contains(&at.saturating_sub(previous)),
                    "{swap}: {times:?}"
                );
                previous = at;
            }
        }

        // A lagging swap is due again at once; the other keeps its schedule.
        let now = base + Duration::from_secs(61);
        schedule.wake('b', now);
        assert_eq!(schedule.poll_due(vec!['a', 'b'], |swap| *swap, now), ['b']);
    }

    /// Swaps loaded with the view are first due 9-15 s after it opened, and later swaps 9-15 s
    /// after they are first seen. A poll slower than its interval waits a fresh one from its
    /// completion, unless the swap was woken while it ran.
    #[test]
    fn swaps_wait_from_their_poll_completion_and_loaded_swaps_from_the_start() {
        let secs = Duration::from_secs;
        let within = |at: Instant, from: Instant| (from + secs(9)..=from + secs(15)).contains(&at);
        let start = Instant::now();
        let mut schedule = SwapSchedule::with_interval(swap_poll_interval, start);
        assert_eq!(
            schedule.poll_due(vec!['a'], |swap| *swap, start + secs(5)),
            [] as [char; 0]
        );
        assert!(within(schedule.due[&'a'], start));
        let later = start + secs(8);
        assert_eq!(
            schedule.poll_due(vec!['a', 'b'], |swap| *swap, later),
            [] as [char; 0]
        );
        assert!(within(schedule.due[&'b'], later));

        let polled = BTreeSet::from(['a']);
        let t0 = start + secs(15);
        assert!(
            schedule
                .poll_due(vec!['a', 'b'], |swap| *swap, t0)
                .contains(&'a')
        );
        let done = t0 + secs(20);
        schedule.polled(&polled, done);
        assert!(
            !schedule
                .poll_due(vec!['a', 'b'], |swap| *swap, done)
                .contains(&'a')
        );
        assert!(within(schedule.due[&'a'], done));

        let t1 = schedule.due[&'a'];
        assert!(
            schedule
                .poll_due(vec!['a', 'b'], |swap| *swap, t1)
                .contains(&'a')
        );
        schedule.wake('a', t1 + secs(1));
        schedule.polled(&polled, t1 + secs(20));
        assert!(
            schedule
                .poll_due(vec!['a', 'b'], |swap| *swap, t1 + secs(20))
                .contains(&'a')
        );
    }

    /// A background read installs the route it opened only through the swap's current opening:
    /// neither a read over a route Retry then dropped, nor an opening Retry interrupted, brings
    /// a route back.
    #[test]
    fn background_reads_keep_only_a_route_opened_through_the_current_opening() {
        let client = || {
            CowOrderbookClient::new(
                wallet_ops::OperationHttpClient::for_tests(
                    reqwest::Client::new(),
                    wallet_ops::OperationNetworkIsolation::Dedicated,
                ),
                "http://127.0.0.1:1".parse().unwrap(),
                1,
            )
            .unwrap()
        };
        let mut tracking = SwapTracking::default();
        tracking.set_route(Some(client()));
        let kept = tracking.route_cell();
        tracking.set_route(None);
        tracking.keep_route(&kept);
        assert!(tracking.orderbook.is_none());

        let interrupted = tracking.route_cell();
        assert!(interrupted.set(client()).is_ok());
        tracking.set_route(None);
        tracking.keep_route(&interrupted);
        assert!(tracking.orderbook.is_none());

        let current = tracking.route_cell();
        assert!(current.set(client()).is_ok());
        tracking.keep_route(&current);
        assert!(tracking.orderbook.is_some());
    }
}
