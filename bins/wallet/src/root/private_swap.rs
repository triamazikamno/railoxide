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
mod swaps_panel;
#[cfg(debug_assertions)]
mod ui_fixture;

mod destination_setup;
mod order_observation;

pub(super) use destination_setup::*;
use order_observation::{
    DestinationPage, HintRequest, HintResult, ObservationPage, ObservationResult, SwapRoute,
    fetch_order_hints, observe_pages,
};
pub(super) use ui_fixture::activity as ui_fixture_activity;
#[cfg(debug_assertions)]
pub(super) use ui_fixture::fill_logs as ui_fixture_fill_logs;
pub(super) use ui_fixture::first_start_old_notice as ui_fixture_first_start_old_notice;
pub(super) use ui_fixture::hardware_derive as ui_fixture_hardware_derive;
#[cfg(debug_assertions)]
pub(super) use ui_fixture::holds_delivery as ui_fixture_holds_delivery;
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
pub(super) use ui_fixture::walletconnect_request as ui_fixture_walletconnect_request;
#[cfg(debug_assertions)]
pub(super) use ui_fixture::{Tron as UiFixtureTron, tron as ui_fixture_tron};
#[cfg(debug_assertions)]
pub(super) use ui_fixture::{
    amount_statistics as ui_fixture_amount_statistics, hints_active as ui_fixture_hints_active,
    hints_shield_token as ui_fixture_hints_shield_token,
    own_deposit_stand_ins as ui_fixture_own_deposit_stand_ins,
    shield_hint_stand_ins as ui_fixture_shield_hint_stand_ins,
};
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
mod tests;
