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
use tokio::sync::watch;
use wallet_ops::cow::OrderUid;
use wallet_ops::{
    DesktopPrivateSpendAuthorization, ExecutorOwner, HardwareExecutorAction,
    PublicBroadcasterCandidate, SwapOrderState, SwapReviewChange, SwapSetupStatus,
    TransactionGenerationStage, WakuDeliveryClient, WalletSession,
    cow::{CowOrderStatusHint, CowOrderStatusReport, CowOrderbookClient},
    is_swap_record,
    settings::{EffectiveChainConfig, ExecutorProfile, SwapProfile, SwapTokenEligibility},
    swap_setup_recorded_executed,
    vault::{
        BridgeDelivery, ExecutorNonceObservation, ExecutorOperationId, ExecutorPayloadPurpose,
        ExecutorRecord, SwapApproval, SwapApprovedBounds, SwapBridgeOutcome, SwapDelivery,
        SwapOrderRecord, SwapUseId, SwapUseRecord,
    },
};

use super::WalletRoot;
use super::spend_authorization::{SpendAuthorizationIntent, SpendAuthorizationSummary};

mod dialog;
mod form;
mod model;
mod progress;
mod public_progress;
#[cfg(debug_assertions)]
mod ui_fixture;
pub(super) use ui_fixture::stealth_accounts as ui_fixture_stealth_accounts;
/// The debug UI fixture's queries in a release build, which has no fixture.
#[cfg(not(debug_assertions))]
mod ui_fixture {
    pub(in crate::root) const fn stealth_accounts() -> Option<Vec<super::ExecutorRecord>> {
        None
    }

    pub(super) const fn active() -> bool {
        false
    }

    pub(super) const fn holds_records() -> bool {
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

use form::SwapForm;
use model::{
    SwapBridgeLabels, SwapFillHint, SwapIdentity, SwapLabels, SwapPrivateBridgeLabels,
    SwapSetupLabels, SwapSetupProgress, SwapStepAccount, bridge_sent_amount,
    swap_observation_range, swap_observation_start, swap_private_delivery,
};
pub(super) use model::{
    SwapStage, swap_account_status, swap_delivery, swap_pair_label, swap_recovery_token,
    swap_sell_amount, swap_stage, swap_tokens,
};

/// Delay between caught-up observation passes, and before retrying failed reads.
const SWAP_OBSERVATION_INTERVAL: Duration = Duration::from_secs(12);
/// Longest wait between routine polls of a bridge provider about one order.
const MAX_BRIDGE_POLL_INTERVAL: Duration = Duration::from_mins(5);
/// The order validity a prepared swap's draft restores from an approval saved without one. The
/// form shows it, and the user can change it before the review.
const DRAFT_VALIDITY: Duration = Duration::from_mins(30);
const SWAP_BROADCASTER_RESPONSE_TIMEOUT: Duration = Duration::from_mins(2);
const SWAP_BROADCASTER_REPUBLISH_INTERVAL: Duration = Duration::from_secs(5);

pub(super) struct PrivateSwapsPanel {
    origin_chain_id: u64,
    session: Option<Arc<WalletSession>>,
    view: Entity<PrivateSwapsView>,
}

/// A spend the user authorizes for a swap: its setup with the approved order, a private Bridge
/// swap's destination setup sent again, the order, or an early cancellation.
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
    /// Only the destination stealth account's setup of a private Bridge swap, on its network.
    DestinationSetup(Box<form::DestinationRetry>),
    Order(Box<form::OrderApproval>),
    Cancel(Box<progress::CancelApproval>),
}

impl SwapAction {
    /// The swap the action belongs to.
    fn operation(&self) -> ExecutorOperationId {
        match self {
            Self::Setup(approval) => approval.operation,
            Self::DestinationSetup(approval) => approval.operation,
            Self::Order(approval) => approval.operation,
            Self::Cancel(approval) => approval.operation,
        }
    }
}

impl SwapAuthorization {
    pub(super) fn hardware_executor_action(&self) -> HardwareExecutorAction {
        match &self.action {
            SwapAction::Setup(approval) => HardwareExecutorAction::Execute(approval.operation),
            SwapAction::DestinationSetup(approval) => {
                HardwareExecutorAction::Execute(approval.setup.operation)
            }
            SwapAction::Order(approval) => HardwareExecutorAction::Execute(approval.operation),
            SwapAction::Cancel(approval) => HardwareExecutorAction::Recover(approval.operation),
        }
    }

    /// The network whose executor owner approves [`Self::hardware_executor_action`]: the
    /// swap's own, or the destination network for a setup sent there by itself.
    #[cfg(feature = "hardware")]
    pub(super) fn hardware_executor_chain(&self) -> u64 {
        match &self.action {
            SwapAction::DestinationSetup(approval) => approval.setup.chain_id,
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
    /// A private Bridge swap's destination setup is estimated again before its retry is
    /// reviewed. Nothing is signed.
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
#[derive(Clone, Copy)]
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
            delivery: approval.delivery,
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
        })
    }

    /// Whether the draft is another swap than the record's latest order belongs to.
    fn starts_new_swap(self, record: &ExecutorRecord) -> bool {
        record
            .swap()
            .and_then(|swap| swap.orders().last())
            .is_some_and(|order| order.use_id() != Some(self.swap_use))
    }

    /// When the draft's swap started. A reused account's new swap starts when its use claims
    /// the account, which a retry of the draft keeps.
    fn started(self, record: &ExecutorRecord) -> u64 {
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
    /// The next block of an order's history to observe. A setup keeps none: each pass reads
    /// its account at the confirmed block.
    cursor: Option<u64>,
    /// The last observation or submission problem, shown in the swap's progress.
    error: Option<String>,
    /// The latest successful explicit status check, scoped to its order and completion time.
    status_checked: Option<(OrderUid, u64)>,
    /// The sell amount entered before the first order records it.
    amount: Option<U256>,
    slippage_bps: Option<u32>,
    /// The gas share and order validity entered before the first order records them.
    gas_share_bps: Option<u16>,
    valid_for: Option<Duration>,
    /// The swap's own orderbook route, kept for its quotes, its order submission, and its
    /// order reports.
    orderbook: Option<CowOrderbookClient>,
    /// The orderbook's last report on the latest order. Display only; never an outcome.
    order_hint: Option<SwapOrderHint>,
    /// The app-data budget after the orderbook rejected an order's size.
    byte_budget: Option<usize>,
    /// Routine bridge provider polls of handed-off orders without an outcome, which back off.
    bridge_polls: HashMap<OrderUid, BridgePoll>,
    /// Bridge provider clients on the `orderbook` route, kept with it for status checks.
    bridge_clients: Option<wallet_ops::SwapBridgeClients>,
    /// Traded orders whose executed fee this session already asked the orderbook for, whatever
    /// the answer, so a failed read isn't repeated until the next session.
    fee_asked: HashSet<OrderUid>,
    /// The bought token in the stealth account, and the block it was read at, from this
    /// session's last explicit check of a Bridge swap's refund or undelivered deposit. Recovery
    /// of a refund waits for it, and opens with it.
    stealth_balance: Option<(U256, alloy::eips::BlockNumHash)>,
    /// Stage of the setup handed to a broadcaster in this session.
    setup_stage: Option<watch::Receiver<TransactionGenerationStage>>,
    setup_watch: Option<Task<()>>,
    /// The last setup observation of a private Bridge swap's destination stealth account, read
    /// on its own network.
    destination_setup: Option<SwapSetupStatus>,
    /// The delivered token in the destination stealth account, and the block it was read at,
    /// from this session's last explicit check of held proceeds. Recovery there opens with it.
    destination_balance: Option<(U256, alloy::eips::BlockNumHash)>,
    /// An early cancellation was handed to a broadcaster; the observation decides the winner.
    cancelling: bool,
    /// The user approved this swap's setup in this session: once the setup is confirmed, the
    /// wallet checks the approved terms again and asks to place the order without waiting
    /// for the user to return. Cleared after the first attempt, so failures don't repeat.
    auto_place: bool,
    /// The delivery allowance earlier signing quotes of this swap showed its previews to lack,
    /// added to each later review until an order is submitted.
    delivery_shortfall: Option<U256>,
}

/// A swap's name, after the token its receiver gets on a Bridge swap's destination network.
fn bridge_pair_label(labels: &SwapLabels, bridge: Option<&SwapBridgeLabels>) -> String {
    swap_pair_label(
        &labels.sell,
        bridge.map_or(&labels.buy_symbol, |bridge| &bridge.token),
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

/// What the orderbook last reported about one order.
#[derive(Clone, Copy)]
struct SwapOrderHint {
    uid: OrderUid,
    report: CowOrderStatusReport,
    /// The reported trade's block, read once the order is reported filled.
    trade_block: Option<u64>,
}

pub(super) struct PrivateSwapsView {
    root: WeakEntity<WalletRoot>,
    origin_chain_id: u64,
    session: Option<Arc<WalletSession>>,
    owner: Option<Arc<ExecutorOwner>>,
    public_records: Vec<(u64, ExecutorRecord)>,
    public_tracking: BTreeSet<(u64, ExecutorOperationId, SwapUseId)>,
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
    tracking: BTreeMap<ExecutorOperationId, SwapTracking>,
    /// Each private Bridge swap's destination stealth account record, by the swap's operation,
    /// as last read from the destination network's own executor records. An entry exists once
    /// that network's session is loaded, and holds `None` when no such record is there.
    destinations: BTreeMap<SwapIdentity, Option<ExecutorRecord>>,
    /// Each loaded account's attribution evidence, from its own network's owner, as read when
    /// its record was last loaded. It decides whether a recovery returned stranded funds.
    attributions: BTreeMap<ExecutorOperationId, wallet_ops::ExecutorAttribution>,
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
        let origin_chain_id = self.selected_chain;
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
                view.cancel = None;
                view.pending_authorization = None;
                view.reapproval = None;
                view.cancelled = None;
                view.tracking.clear();
                view.destinations.clear();
                view.attributions.clear();
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
                panel.origin_chain_id == self.selected_chain
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
            .get(self.selected_chain)?
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
        match token.map(|token| profile.token_eligibility(token, SwapTokenRole::Sell)) {
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

impl PrivateSwapsView {
    fn watch_origin_changes(
        session: Option<&Arc<WalletSession>>,
        owner: Option<&Arc<ExecutorOwner>>,
        cx: &Context<'_, Self>,
    ) -> Task<()> {
        let watches = owner.zip(session).map(|(owner, session)| {
            (
                owner.subscribe(),
                session.observation_rx.clone(),
                session.sync_tip_rx.clone(),
            )
        });
        cx.spawn(async move |this, cx| {
            let Some((mut changes, mut private_changes, mut tip_changes)) = watches else {
                return;
            };
            loop {
                let reload = tokio::select! {
                    changed = changes.changed() => changed.map(|()| true),
                    changed = private_changes.changed() => changed.map(|()| true),
                    changed = tip_changes.changed() => changed.map(|()| false),
                };
                let Ok(reload) = reload else {
                    break;
                };
                if this
                    .update(cx, |this, cx| {
                        if this.session_is_current(cx) {
                            if reload {
                                this.reload_records();
                                this.refresh_public_swap_records(cx);
                                this.reload_destinations(cx);
                                this.refresh_form_assets(cx);
                            }
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    fn new_for_origin(
        root: WeakEntity<WalletRoot>,
        origin_chain_id: u64,
        session: Option<Arc<WalletSession>>,
        owner: Option<Arc<ExecutorOwner>>,
        runtime: tokio::runtime::Handle,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let changes_task = Self::watch_origin_changes(session.as_ref(), owner.as_ref(), cx);
        // Every change to the records, tracking or jobs notifies, so the open count follows
        // notifications. It reads the root, which is busy creating this view, so the first
        // count waits for the first notification; opening the dialog sends one.
        cx.observe_self(|this, cx| {
            let open = this.open_order_count(cx);
            this.open_orders = open;
        })
        .detach();
        let observation_wake = watch::channel(0).0;
        let mut wake = observation_wake.subscribe();
        let polling = cx.spawn_in(window, async move |this, cx| {
            let mut catchup = Vec::new();
            loop {
                let mut woken = false;
                if catchup.is_empty() {
                    tokio::select! {
                        () = cx.background_executor().timer(SWAP_OBSERVATION_INTERVAL) => {}
                        changed = wake.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            woken = true;
                        }
                    }
                }
                let Ok(work) = this.update_in(cx, |this, window, cx| {
                    // A recorded executed setup is ready without an account read, so the
                    // inclusion that woke this pass may leave no page to observe. The form
                    // continues here, as approved orders do on every pass.
                    if woken {
                        this.continue_form_after_observation(window, cx);
                    }
                    this.refresh_public_swap_records(cx);
                    this.track_public_swaps(window, cx);
                    this.continue_public_swap_after_setup(window, cx);
                    this.refresh_destinations(cx);
                    this.continue_approved_swaps(window, cx);
                    let (runtime, owner, mut pages) = this.next_observations(cx)?;
                    if !catchup.is_empty() {
                        // Drain only successful, lagging scans. Caught-up and failed
                        // accounts keep the normal polling interval.
                        pages.retain(|page| catchup.contains(&page.operation));
                    }
                    (!pages.is_empty()).then_some((runtime, owner, pages))
                }) else {
                    break;
                };
                catchup.clear();
                if let Some((runtime, owner, pages)) = work {
                    let results = runtime.spawn(observe_pages(owner, pages)).await;
                    let Ok(next) = this.update_in(cx, |this, window, cx| {
                        results.map_or_else(
                            |_| Vec::new(),
                            |results| this.apply_observations(results, window, cx),
                        )
                    }) else {
                        break;
                    };
                    catchup = next;
                }
            }
        });
        // Orderbook reports only add copy, so they poll on their own loop: a stalled orderbook
        // never delays canonical observation or order placement. At most one batch is in flight.
        let hint_polling = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(SWAP_OBSERVATION_INTERVAL)
                    .await;
                let Ok(hints) = this.update_in(cx, |this, _, cx| this.next_order_hints(cx)) else {
                    break;
                };
                let Some((runtime, owner, requests)) = hints else {
                    continue;
                };
                let results = runtime.spawn(fetch_order_hints(owner, requests)).await;
                if this
                    .update_in(cx, |this, _, cx| {
                        if let Ok(results) = results {
                            this.apply_order_hints(results, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut view = Self {
            root,
            origin_chain_id,
            session,
            owner,
            public_records: Vec::new(),
            public_tracking: BTreeSet::new(),
            public_destination_balances: BTreeMap::new(),
            public_railgun: BTreeMap::new(),
            public_synced: BTreeSet::new(),
            public_tracking_failures: BTreeMap::new(),
            public_permit_used_up: BTreeSet::new(),
            public_authorization: None,
            public_execution: None,
            public_job: None,
            public_running: None,
            public_status: None,
            runtime,
            active: true,
            records: Vec::new(),
            setup_inclusions: None,
            observation_wake,
            tracking: BTreeMap::new(),
            destinations: BTreeMap::new(),
            attributions: BTreeMap::new(),
            open_orders: 0,
            dialog: None,
            orders_filter: None,
            orders_list: None,
            form: None,
            cancel: None,
            pending_authorization: None,
            reapproval: None,
            cancelled: None,
            job: None,
            job_revision: 0,
            error: None,
            origin_changes: changes_task,
            _polling: polling,
            _hint_polling: hint_polling,
        };
        view.reload_records();
        view
    }

    /// The origin network's private session, which every private swap spends from. `None` in
    /// a view that only tracks swaps paid from Public accounts, which need none there.
    const fn private_session(&self) -> Option<&Arc<WalletSession>> {
        self.session.as_ref()
    }

    /// The origin network's executor owner, as [`Self::private_session`] is available.
    const fn private_owner(&self) -> Option<&Arc<ExecutorOwner>> {
        self.owner.as_ref()
    }

    fn public_record_chain(&self, record: &ExecutorRecord) -> Option<u64> {
        self.public_records.iter().find_map(|(chain, candidate)| {
            (candidate.operation() == record.operation()).then_some(*chain)
        })
    }

    fn session_is_current(&self, cx: &gpui::App) -> bool {
        self.active
            && self.root.upgrade().is_some_and(|root| {
                let root = root.read(cx);
                root.selected_chain == self.origin_chain_id
                    && root.view_session.is_some()
                    && self
                        .session
                        .as_ref()
                        .is_none_or(|session| root.stealth_session_is_current(session))
            })
    }

    fn reload_records(&mut self) {
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        match owner.records() {
            Ok(records) => {
                let selected = self.form.as_ref().and_then(SwapForm::operation);
                let mut records = records
                    .into_iter()
                    .filter(|record| {
                        is_swap_record(record)
                            || model::cancelled_swap_uses(record).next().is_some()
                            || Some(record.operation()) == selected
                            || self.pending_order(record).is_some()
                    })
                    .collect::<Vec<_>>();
                records
                    .sort_by_key(|record| std::cmp::Reverse((record.created_at(), record.index())));
                // Only a setup that private sync newly shows in a transaction recorded for it
                // wakes the observation loop: it makes the setup's account check due at once.
                // Waking on every owner change would spin, since each pass records its own
                // observations.
                let inclusions = {
                    let observation = self
                        .session
                        .as_ref()
                        .map(|session| session.observation_rx.borrow());
                    let utxos = observation
                        .as_ref()
                        .map_or(&[][..], |observation| &observation.snapshot.utxos[..]);
                    records
                        .iter()
                        .filter(|record| {
                            record.swap().is_none()
                                && matches!(
                                    model::swap_setup_confirmation(record, utxos),
                                    Some(model::SwapSetupConfirmation::Included(_))
                                )
                        })
                        .map(ExecutorRecord::operation)
                        .collect::<BTreeSet<_>>()
                };
                if self
                    .setup_inclusions
                    .as_ref()
                    .is_some_and(|known| !inclusions.is_subset(known))
                {
                    self.observation_wake
                        .send_modify(|wakes| *wakes = wakes.wrapping_add(1));
                }
                self.setup_inclusions = Some(inclusions);
                self.attributions.extend(
                    records.iter().filter_map(|record| {
                        Some((record.operation(), owner.attribution(record)?))
                    }),
                );
                self.records = records;
            }
            // Keep the last records; the owner reports the same problem to every caller.
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn record(&self, operation: ExecutorOperationId) -> Option<&ExecutorRecord> {
        self.records
            .iter()
            .find(|record| record.operation() == operation)
    }

    /// The attribution evidence read with `record`, an account of any loaded network.
    pub(super) fn attribution(
        &self,
        record: &ExecutorRecord,
    ) -> Option<&wallet_ops::ExecutorAttribution> {
        self.attributions.get(&record.operation())
    }

    fn stage(&self, record: &ExecutorRecord) -> SwapStage {
        let operation = record.operation();
        let submitting = self
            .job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup);
        let setup = self
            .tracking
            .get(&operation)
            .and_then(|tracking| tracking.setup);
        account_stage(
            record,
            self.origin_chain_id,
            setup,
            submitting,
            self.attribution(record),
        )
    }

    /// The wallet session and executor owner of `chain_id`, a private Bridge swap's destination
    /// network, once that network's session is loaded.
    pub(super) fn destination_owner(
        &self,
        chain_id: u64,
        cx: &gpui::App,
    ) -> Option<(Arc<WalletSession>, Arc<ExecutorOwner>)> {
        use super::chain_load::ChainUtxoState;
        let root = self.root.upgrade()?;
        match root.read(cx).chain_states.get(&chain_id)? {
            ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. } => {
                Some((Arc::clone(session), session.executor_owner()?))
            }
            _ => None,
        }
    }

    /// [`Self::destination_owner`] once `chain_id`'s private sync is ready, for a setup or an
    /// order that spends or signs there. Otherwise what the user is told. A network that isn't
    /// loaded starts loading.
    fn ready_destination(
        &self,
        chain_id: u64,
        cx: &mut Context<'_, Self>,
    ) -> Result<(Arc<WalletSession>, Arc<ExecutorOwner>), String> {
        use super::chain_load::ChainUtxoState;
        let network = form::network_name(chain_id);
        let root = self
            .root
            .upgrade()
            .ok_or_else(|| "The wallet session ended.".to_owned())?;
        match root.read(cx).chain_states.get(&chain_id) {
            Some(ChainUtxoState::Ready { session, .. }) => {
                let owner = session.executor_owner().ok_or_else(|| {
                    format!("Stealth accounts aren't available on {network} for this wallet.")
                })?;
                return Ok((Arc::clone(session), owner));
            }
            Some(ChainUtxoState::Syncing { .. } | ChainUtxoState::Loading { .. }) => {
                return Err(format!(
                    "{network} is still syncing. Try again once it is ready. Nothing was sent."
                ));
            }
            Some(ChainUtxoState::Error { .. }) => {
                return Err(format!(
                    "{network} couldn't be synced. Switch to {network} to see why, then try again. Nothing was sent."
                ));
            }
            None | Some(ChainUtxoState::Idle) => {}
        }
        self.ensure_destination_load(chain_id, cx);
        Err(format!(
            "{network} isn't loaded yet. It is loading now. Try again once it is ready. Nothing was sent."
        ))
    }

    /// Read each private Bridge swap's destination stealth account again from the destination
    /// network's executor records. Networks that aren't loaded are left out.
    fn reload_destinations(&mut self, cx: &gpui::App) {
        let mut loaded = BTreeMap::new();
        let mut destinations = BTreeMap::new();
        let mut attributions = Vec::new();
        for record in &self.records {
            // Each swap of a reused account names its own destination account.
            for swap_use in record.swap_uses() {
                let Some((delivery, operation)) =
                    model::swap_use_destination(record, swap_use.id())
                else {
                    continue;
                };
                let records =
                    loaded
                        .entry(delivery.destination_chain)
                        .or_insert_with_key(|chain_id| {
                            let (_, owner) = self.destination_owner(*chain_id, cx)?;
                            let records = owner.records().ok()?;
                            Some((owner, records))
                        });
                if let Some((owner, records)) = records {
                    let destination = records
                        .iter()
                        .find(|destination| destination.operation() == operation);
                    attributions.extend(destination.and_then(|destination| {
                        Some((destination.operation(), owner.attribution(destination)?))
                    }));
                    destinations.insert(
                        SwapIdentity {
                            operation: record.operation(),
                            swap_use: swap_use.id(),
                        },
                        destination.cloned(),
                    );
                }
            }
        }
        self.destinations = destinations;
        self.attributions.extend(attributions);
    }

    /// Start loading the destination network of each private Bridge swap that is still being
    /// set up, whose destination account only that network's session can tell about, then read
    /// the destination accounts again.
    fn refresh_destinations(&mut self, cx: &mut Context<'_, Self>) {
        let chains = self
            .records
            .iter()
            .filter(|record| {
                record.swap().is_none() && !record.is_retired() && !record.is_swap_setup_stopped()
            })
            .filter_map(swap_private_delivery)
            .map(|delivery| delivery.destination_chain)
            // A reused account's prepared swap waits for a new destination account's setup
            // the same way.
            .chain(self.records.iter().filter_map(|record| {
                let swap_use = model::prepared_swap_use(record)?;
                sets_up_destination(swap_use).then_some(())?;
                Some(
                    swap_use
                        .approval()?
                        .delivery
                        .private_bridge()?
                        .destination_chain,
                )
            }))
            .collect::<BTreeSet<_>>();
        for chain_id in chains {
            self.ensure_destination_load(chain_id, cx);
        }
        let known = std::mem::take(&mut self.destinations);
        self.reload_destinations(cx);
        if self.destinations != known {
            cx.notify();
        }
    }

    /// Start loading `chain_id`'s wallet session unless it is loaded, loading, or failed to
    /// load. The root starts it after this update, since a load replaces per-chain views.
    fn ensure_destination_load(&self, chain_id: u64, cx: &mut Context<'_, Self>) {
        use super::chain_load::ChainUtxoState;
        let idle = self.root.upgrade().is_some_and(|root| {
            matches!(
                root.read(cx).chain_states.get(&chain_id),
                None | Some(ChainUtxoState::Idle)
            )
        });
        if !idle {
            return;
        }
        let root = self.root.clone();
        cx.defer(move |cx| {
            let _ = root.update(cx, |root, cx| root.ensure_chain_load(chain_id, cx));
        });
    }

    /// The swap `record`'s own progress shows: the draft's while a new order is being
    /// prepared, otherwise the latest order's, or before any order the one that claims the
    /// account.
    fn shown_swap(&self, record: &ExecutorRecord) -> Option<SwapIdentity> {
        let swap_use = self.pending_order(record).map_or_else(
            || model::record_latest_use(record),
            |pending| Some(pending.swap_use),
        )?;
        Some(SwapIdentity {
            operation: record.operation(),
            swap_use,
        })
    }

    /// The swap `order` of `record` belongs to; without an order, the swap the record shows.
    fn order_swap(
        &self,
        record: &ExecutorRecord,
        order: Option<&SwapOrderRecord>,
    ) -> Option<SwapIdentity> {
        order
            .and_then(SwapOrderRecord::use_id)
            .map(|swap_use| SwapIdentity {
                operation: record.operation(),
                swap_use,
            })
            .or_else(|| self.shown_swap(record))
    }

    /// The destination stealth account record of the private Bridge swap `record` shows, when
    /// its network's records were read and hold the account `delivery` names.
    fn destination_account(
        &self,
        record: &ExecutorRecord,
        delivery: BridgeDelivery,
    ) -> Option<&ExecutorRecord> {
        self.swap_destination_account(self.shown_swap(record)?, delivery)
    }

    /// [`Self::destination_account`] for one swap of a stealth account, which an earlier swap
    /// of a reused account keeps apart from the account's later ones.
    fn swap_destination_account(
        &self,
        swap: SwapIdentity,
        delivery: BridgeDelivery,
    ) -> Option<&ExecutorRecord> {
        self.destinations
            .get(&swap)?
            .as_ref()
            .filter(|destination| destination.address() == Some(delivery.receiver))
    }

    /// Whether this session's setup job covers `operation`. It sends both setups of a private
    /// Bridge swap.
    fn submitting_setup(&self, operation: ExecutorOperationId) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.operation == operation && job.kind == SwapJobKind::Setup)
    }

    /// How far the setup of a private Bridge swap's destination stealth account is, before the
    /// swap's first order.
    fn destination_setup_progress(
        &self,
        record: &ExecutorRecord,
        delivery: BridgeDelivery,
    ) -> SwapSetupProgress {
        match self.shown_swap(record) {
            Some(swap) => self.swap_destination_setup_progress(swap, delivery),
            None => SwapSetupProgress::NetworkLoading,
        }
    }

    /// [`Self::destination_setup_progress`] for one swap of a stealth account, which a draft
    /// on a reused account asks about before its swap is the one the record shows.
    fn swap_destination_setup_progress(
        &self,
        swap: SwapIdentity,
        delivery: BridgeDelivery,
    ) -> SwapSetupProgress {
        let operation = swap.operation;
        match self.destinations.get(&swap) {
            None => SwapSetupProgress::NetworkLoading,
            Some(None) => SwapSetupProgress::NotSent,
            Some(Some(destination)) => account_setup_progress(
                destination,
                delivery.destination_chain,
                self.tracking
                    .get(&operation)
                    .and_then(|tracking| tracking.destination_setup),
                self.submitting_setup(operation),
            ),
        }
    }

    /// Have the destination network's owner, when that network is loaded, settle the
    /// destination stealth account of the private Bridge swap `operation` from the swap's
    /// recorded bridge outcome. It runs on the runtime, and that owner's change is picked up
    /// with the next read of the destination accounts.
    ///
    /// A fill verified as shielded consumed the account's execution nonce, which only a read
    /// of the account records. Until then its shield counts as outstanding and the account
    /// can't be chosen for another swap. So the same owner reads the account once at its
    /// network's confirmed block, through the wallet's network context. Nothing is asked
    /// while that network isn't loaded: the account then stays unavailable until the
    /// network is loaded and the swap is checked.
    fn reconcile_destination(&self, operation: ExecutorOperationId, cx: &gpui::App) {
        let Some(delivery) = self.record(operation).and_then(swap_private_delivery) else {
            return;
        };
        let chain_id = delivery.destination_chain;
        let Some((_, owner)) = self.destination_owner(chain_id, cx) else {
            return;
        };
        let Some(origin) = self.private_owner().cloned() else {
            return;
        };
        drop(self.runtime.spawn(async move {
            let settling = Arc::clone(&owner);
            // A failed write is repeated by the owner's next reconciliation.
            let _ =
                tokio::task::spawn_blocking(move || settling.reconcile_swap_destinations()).await;
            let reading = Arc::clone(&owner);
            let delivered = tokio::task::spawn_blocking(move || {
                delivered_destinations(&origin, &reading, operation, chain_id)
            })
            .await
            .unwrap_or_default();
            for account in delivered {
                // A failed read leaves the account unavailable until the swap is checked again.
                let _ = Box::pin(owner.reconcile_account(account)).await;
            }
        }));
    }

    /// Whether a recovery on the destination network returned the proceeds a private Bridge
    /// swap's destination stealth account held. Only that network's loaded records tell.
    fn held_proceeds_recovered(&self, record: &ExecutorRecord, delivery: BridgeDelivery) -> bool {
        let held = record
            .swap()
            .and_then(|swap| swap.orders().last())
            .and_then(|order| match order.observations().bridge_outcome {
                Some(SwapBridgeOutcome::HeldOnDestination { block, .. }) => Some(block.number),
                _ => None,
            });
        held.zip(self.destination_account(record, delivery))
            .is_some_and(|(block, destination)| {
                model::held_proceeds_recovered(destination, block, self.attribution(destination))
            })
    }

    /// A new submission must not borrow the previous order's outcome. Once its own swap use
    /// records an order, that record becomes the source of progress even if submission is
    /// still running.
    ///
    /// Without a draft of this session, a reused account's prepared swap gives one from its
    /// saved approval: see [`PendingSwapOrder::prepared`].
    fn pending_order(&self, record: &ExecutorRecord) -> Option<PendingSwapOrder> {
        self.tracking
            .get(&record.operation())
            .and_then(|tracking| tracking.pending_order)
            .filter(|pending| {
                pending.previous_order
                    == model::swap_use_last_order(record, pending.swap_use)
                        .map(SwapOrderRecord::uid)
            })
            .or_else(|| PendingSwapOrder::prepared(record))
            .map(|mut pending| {
                if let SwapDelivery::Bridge(delivery) = &mut pending.delivery
                    && delivery.is_private()
                    && delivery.receiver.is_zero()
                    && let Some(SwapDelivery::Bridge(bound)) = model::prepared_swap_use(record)
                        .filter(|swap_use| swap_use.id() == pending.swap_use)
                        .and_then(SwapUseRecord::approval)
                        .map(|approval| approval.delivery)
                    && bound.is_private()
                    && bound.destination_chain == delivery.destination_chain
                    && !bound.receiver.is_zero()
                {
                    // Preparation binds the new account after this session saved its draft.
                    // Keep the draft's terms, resolving only its placeholder receiver.
                    delivery.receiver = bound.receiver;
                }
                pending
            })
    }

    /// The swap use of `record` whose first order the approval saved with it still binds, once
    /// every account the swap uses is ready for that order: an account the use set up is
    /// confirmed, and an account it reuses is set up already. The approved order is then
    /// placed without another review, whichever of its accounts needed setup. Signing still
    /// checks each account afresh.
    fn approved_use<'a>(
        &self,
        record: &'a ExecutorRecord,
    ) -> Option<(SwapUseId, &'a SwapApproval)> {
        if record.is_swap_setup_stopped() {
            return None;
        }
        let swap_use = model::prepared_swap_use(record)?;
        let approval = swap_use.approval()?;
        let operation = record.operation();
        let source_ready = if swap_use.is_fresh() {
            self.stage(record) == SwapStage::Approved
        } else {
            account_setup_progress(
                record,
                self.origin_chain_id,
                self.tracking
                    .get(&operation)
                    .and_then(|tracking| tracking.setup),
                false,
            ) == SwapSetupProgress::Done
        };
        let destination_ready = match approval.delivery.private_bridge() {
            Some(delivery) => {
                let swap = SwapIdentity {
                    operation,
                    swap_use: swap_use.id(),
                };
                self.swap_destination_setup_progress(swap, delivery) == SwapSetupProgress::Done
            }
            None => true,
        };
        (source_ready && destination_ready).then_some((swap_use.id(), approval))
    }

    /// The private Bridge delivery `record`'s own progress is about: its draft's, or its
    /// latest order's or approved one.
    fn shown_private_delivery(&self, record: &ExecutorRecord) -> Option<BridgeDelivery> {
        match self.pending_order(record) {
            Some(pending) => pending.delivery.private_bridge(),
            None => swap_private_delivery(record),
        }
    }

    /// Presentation only; signing and observation still use the durable account stage. A
    /// private Bridge swap also shows its destination stealth account's setup, and a recovery
    /// of held proceeds on the destination network.
    fn progress_stage(&self, record: &ExecutorRecord) -> SwapStage {
        if let Some(pending) = self.pending_order(record) {
            // A draft's own account is set up. A destination account its swap use sets up
            // shows that setup until it is confirmed; one it reuses has none to wait for.
            let setup = record
                .swap_use(pending.swap_use)
                .filter(|swap_use| sets_up_destination(swap_use))
                .and_then(|_| model::swap_use_destination(record, pending.swap_use));
            return match setup {
                Some((delivery, _)) => model::private_bridge_setup_stage(
                    SwapStage::Ready,
                    self.swap_destination_setup_progress(
                        SwapIdentity {
                            operation: record.operation(),
                            swap_use: pending.swap_use,
                        },
                        delivery,
                    ),
                ),
                None => SwapStage::Ready,
            };
        }
        let stage = self.stage(record);
        let Some(delivery) = swap_private_delivery(record) else {
            return stage;
        };
        match stage {
            SwapStage::Ready | SwapStage::Approved => model::private_bridge_setup_stage(
                stage,
                self.destination_setup_progress(record, delivery),
            ),
            SwapStage::Order(SwapOrderState::HeldOnDestination)
                if self.held_proceeds_recovered(record, delivery) =>
            {
                SwapStage::Recovered
            }
            _ => stage,
        }
    }

    const fn busy(&self) -> bool {
        self.job.is_some()
            || self.pending_authorization.is_some()
            || self.public_authorization.is_some()
            || self.public_job.is_some()
    }

    /// The confirmed block of the session's last synced head.
    fn confirmed_block(&self, cx: &gpui::App) -> Option<u64> {
        self.root
            .upgrade()?
            .read(cx)
            .confirmed_block(self.origin_chain_id)
    }

    /// A token's symbol, decimals and icon. The native asset, which a Public address swap can
    /// buy as `Address::ZERO`, isn't in the token registry, so it comes from the chain.
    fn token_metadata(
        &self,
        token: Address,
        cx: &gpui::App,
    ) -> Option<super::tokens::TokenDisplayMetadata> {
        self.chain_token_metadata(self.origin_chain_id, token, cx)
    }

    /// [`Self::token_metadata`] on `chain_id`, such as a Bridge swap's destination network.
    fn chain_token_metadata(
        &self,
        chain_id: u64,
        token: Address,
        cx: &gpui::App,
    ) -> Option<super::tokens::TokenDisplayMetadata> {
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        if token == Address::ZERO {
            let native = &root.effective_chain_configs.get(chain_id)?.native_currency;
            return Some(super::tokens::TokenDisplayMetadata {
                symbol: native.symbol.clone(),
                decimals: native.decimals,
                icon_path: railgun_ui::chain_icon_asset_path(chain_id)
                    .map(crate::assets::WalletIconSource::embedded),
            });
        }
        super::token_display_metadata(Some(&root.effective_token_registry), chain_id, &token)
    }

    fn token_symbol(&self, token: Address, cx: &gpui::App) -> String {
        self.token_metadata(token, cx)
            .map_or_else(|| railgun_ui::short_address(&token), |info| info.symbol)
    }

    fn token_amount(&self, token: Address, amount: U256, cx: &gpui::App) -> String {
        if token == Address::ZERO
            && let Some(native) = self.token_metadata(token, cx)
        {
            return format!(
                "{} {}",
                railgun_ui::format_token_amount(amount, native.decimals),
                native.symbol
            );
        }
        self.root.upgrade().map_or_else(
            || amount.to_string(),
            |root| {
                super::format_token_amount_for_display(
                    self.origin_chain_id,
                    token,
                    amount,
                    Some(&root.read(cx).effective_token_registry),
                )
            },
        )
    }

    fn labels(&self, record: &ExecutorRecord, cx: &gpui::App) -> SwapLabels {
        if let Some(pending) = self.pending_order(record) {
            let labels = self.order_labels(
                record,
                (pending.sell, pending.buy),
                Some(pending.amount),
                None,
                None,
                cx,
            );
            let bridge = self.bridge_labels(record, pending.delivery, pending.buy, None, None, cx);
            return SwapLabels {
                pair: bridge_pair_label(&labels, bridge.as_ref()),
                receiver: self.receiver_name(pending.delivery, cx),
                minimum: Some(self.token_amount(pending.buy, pending.private_minimum, cx)),
                bridge,
                ..labels
            };
        }
        let Some((sell, buy)) = swap_tokens(record) else {
            return SwapLabels {
                pair: "Private swap".into(),
                sell: "Private swap".into(),
                buy_symbol: String::new(),
                expires: None,
                lapsed: false,
                fill_hint: None,
                received: None,
                receiver: None,
                minimum: None,
                uncovered_gas: None,
                order: None,
                bridge: None,
            };
        };
        let amount = swap_sell_amount(record).or_else(|| {
            self.tracking
                .get(&record.operation())
                .and_then(|tracking| tracking.amount)
        });
        let order = record.swap().and_then(|swap| swap.orders().last());
        let labels = self.order_labels(
            record,
            (sell, buy),
            amount,
            order,
            self.fill_hint(record, cx),
            cx,
        );
        if order.is_some() {
            return labels;
        }
        // Before its first order, the swap delivers as approved with its setup.
        let delivery = swap_delivery(record);
        let approved = record.swap_approval().map(|approval| &approval.bounds);
        let bridge = self.bridge_labels(record, delivery, buy, None, approved, cx);
        SwapLabels {
            pair: bridge_pair_label(&labels, bridge.as_ref()),
            receiver: self.receiver_name(delivery, cx),
            minimum: model::swap_private_minimum(record)
                .map(|minimum| self.token_amount(buy, minimum, cx)),
            bridge,
            ..labels
        }
    }

    fn setup_confirmation(&self, record: &ExecutorRecord) -> Option<model::SwapSetupConfirmation> {
        model::swap_setup_confirmation(
            record,
            &self
                .private_session()?
                .observation_rx
                .borrow()
                .snapshot
                .utxos,
        )
    }

    fn setup_retry_problem(&self, record: &ExecutorRecord) -> Option<&'static str> {
        (matches!(
            self.stage(record),
            SwapStage::SetupPending | SwapStage::SetupSubmitting
        ) && self.setup_confirmation(record).is_some())
        .then_some("The setup is already included. Wait for confirmation and verification.")
    }

    fn setup_confirmation_detail(&self, record: &ExecutorRecord, cx: &gpui::App) -> Option<String> {
        let root = self.root.upgrade()?;
        let depth = root
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)?
            .finality_depth;
        let tip = *self.private_session()?.sync_tip_rx.borrow();
        self.setup_confirmation(record)?
            .detail(super::utxo::UtxoFinalityContext::new(
                tip.head_block,
                tip.safe_head_block,
                Some(depth),
            ))
    }

    /// Display strings for one swap of `record`: `amount` of `sell` for `buy`, with `order`
    /// its latest order when it has one.
    fn order_labels(
        &self,
        record: &ExecutorRecord,
        (sell, buy): (Address, Address),
        amount: Option<U256>,
        order: Option<&SwapOrderRecord>,
        fill_hint: Option<SwapFillHint>,
        cx: &gpui::App,
    ) -> SwapLabels {
        let sell_symbol = self.token_symbol(sell, cx);
        let buy_symbol = self.token_symbol(buy, cx);
        let sell_label = amount.map_or(sell_symbol, |amount| self.token_amount(sell, amount, cx));
        let received = order.and_then(|order| {
            let observed = order.observations();
            let amount = match order.delivery() {
                SwapDelivery::Reshield => observed.shielded.map(|shield| shield.private_amount),
                // The trade paid the receiver directly.
                SwapDelivery::External { .. } => observed
                    .delivered
                    .and(observed.trade_amounts)
                    .map(|trade| trade.buy_amount),
                // The destination network's token and amount, verified or reported.
                SwapDelivery::Bridge(bridge) => {
                    let amount = match observed.bridge_outcome? {
                        // The destination stealth account shielded this, less the shield fee.
                        SwapBridgeOutcome::DeliveredVerified {
                            output_amount,
                            shielded: true,
                            ..
                        } => Some(model::private_delivery_credit(
                            output_amount,
                            order.bounds(),
                        )),
                        SwapBridgeOutcome::DeliveredVerified { output_amount, .. } => {
                            Some(output_amount)
                        }
                        SwapBridgeOutcome::DeliveredReported { amount_out, .. } => amount_out,
                        // Held proceeds weren't received; the bridge labels name them.
                        SwapBridgeOutcome::Refunding
                        | SwapBridgeOutcome::NeedsAttention
                        | SwapBridgeOutcome::HeldOnDestination { .. } => None,
                    }?;
                    return Some(self.network_token_amount(
                        bridge.destination_chain,
                        self.delivered_token(bridge, cx),
                        amount,
                        cx,
                    ));
                }
            };
            amount.map(|amount| self.token_amount(buy, amount, cx))
        });
        let valid_to = order.map(|order| u64::from(order.valid_to()));
        let bridge = order.and_then(|order| {
            self.bridge_labels(record, order.delivery(), buy, Some(order), None, cx)
        });
        SwapLabels {
            pair: swap_pair_label(
                &sell_label,
                bridge.as_ref().map_or(&buy_symbol, |bridge| &bridge.token),
            ),
            sell: sell_label,
            buy_symbol,
            expires: valid_to.map(local_time_label),
            lapsed: valid_to.is_some_and(|valid_to| valid_to < now_unix()),
            fill_hint,
            received,
            receiver: order.and_then(|order| self.receiver_name(order.delivery(), cx)),
            minimum: order.map(|order| self.token_amount(buy, order.bounds().private_minimum, cx)),
            uncovered_gas: order
                .and_then(|order| {
                    let bounds = order.bounds();
                    bounds.gas_estimate?.checked_sub(bounds.gas_allowance?)
                })
                .filter(|rest| !rest.is_zero())
                .map(|rest| self.money(buy, rest, cx)),
            order: order.map(SwapOrderRecord::uid),
            bridge,
        }
    }

    /// `amount` of `token` in the wallet's currency, "$4.42", or as a token amount without a
    /// cached rate.
    fn money(&self, token: Address, amount: U256, cx: &gpui::App) -> String {
        self.usd_micro_value(token, amount, cx).map_or_else(
            || self.token_amount(token, amount, cx),
            railgun_ui::format_usd_micro_value,
        )
    }

    /// How swaps name a Bridge delivery of `buy`: its network, provider and receiver, and from
    /// `order`, what it handed to the bridge and its approved destination minimum. Before an
    /// order exists, `approved` holds the bounds approved with the setup. A private delivery
    /// also names both stealth accounts of `record`'s swap and what the destination account
    /// holds. `None` for delivery on this network.
    fn bridge_labels(
        &self,
        record: &ExecutorRecord,
        delivery: SwapDelivery,
        buy: Address,
        order: Option<&SwapOrderRecord>,
        approved: Option<&SwapApprovedBounds>,
        cx: &gpui::App,
    ) -> Option<SwapBridgeLabels> {
        let SwapDelivery::Bridge(bridge) = delivery else {
            return None;
        };
        let token = self.delivered_token(bridge, cx);
        let destination_amount =
            |amount| self.network_token_amount(bridge.destination_chain, token, amount, cx);
        let swap = self.order_swap(record, order);
        let private = bridge.is_private().then(|| SwapPrivateBridgeLabels {
            setups: self.setup_labels(record, swap, bridge, order.is_some(), cx),
            held: order.and_then(|order| match order.observations().bridge_outcome {
                Some(SwapBridgeOutcome::HeldOnDestination { amount, .. }) => {
                    Some(destination_amount(amount))
                }
                _ => None,
            }),
        });
        Some(SwapBridgeLabels {
            provider: bridge.provider,
            network: form::network_name(bridge.destination_chain),
            token: self.network_token_symbol(bridge.destination_chain, token, cx),
            origin: form::network_name(self.origin_chain_id),
            receiver: self
                .receiver_label(bridge.receiver, cx)
                .map_or_else(|| short_receiver(bridge.receiver), |(label, _)| label),
            sent: order
                .and_then(bridge_sent_amount)
                .map(|amount| self.token_amount(buy, amount, cx)),
            minimum: order
                .map(SwapOrderRecord::bounds)
                .or(approved)
                .and_then(|bounds| Some((bounds, bounds.destination_minimum?)))
                .map(|(bounds, minimum)| {
                    // The private balance gets the minimum less the destination shield fee.
                    destination_amount(if bridge.is_private() {
                        model::private_delivery_credit(minimum, bounds)
                    } else {
                        minimum
                    })
                }),
            private,
            reported: order.is_some_and(|order| {
                matches!(
                    order.observations().bridge_outcome,
                    Some(SwapBridgeOutcome::DeliveredReported { .. })
                )
            }),
        })
    }

    /// The token a Bridge `delivery` pays out, as swaps name it. A private delivery shields
    /// the destination token itself, so a wrapped native token stays wrapped.
    fn delivered_token(&self, delivery: BridgeDelivery, cx: &gpui::App) -> Address {
        if delivery.is_private() {
            delivery.destination_token
        } else {
            self.bridge_received_token(delivery, cx)
        }
    }

    /// The setup of each stealth account of `record`'s private Bridge swap `swap`, the swap's
    /// own network first. `placed` tells that the order these labels describe exists, which
    /// needs both accounts set up.
    fn setup_labels(
        &self,
        record: &ExecutorRecord,
        swap: Option<SwapIdentity>,
        delivery: BridgeDelivery,
        placed: bool,
        cx: &gpui::App,
    ) -> [SwapSetupLabels; 2] {
        let operation = record.operation();
        let tracking = self.tracking.get(&operation);
        let origin = if placed {
            SwapSetupProgress::Done
        } else {
            account_setup_progress(
                record,
                self.origin_chain_id,
                tracking.and_then(|tracking| tracking.setup),
                self.submitting_setup(operation),
            )
        };
        // What this session knows the swap's own setup waits for.
        let waiting = match origin {
            SwapSetupProgress::Submitting => tracking
                .and_then(|tracking| tracking.setup_stage.as_ref())
                .map(|stage| stage.borrow().label().to_owned()),
            SwapSetupProgress::Pending => self.setup_confirmation_detail(record, cx),
            _ => None,
        };
        let destination = swap.and_then(|swap| self.swap_destination_account(swap, delivery));
        // An account its swap use didn't reserve fresh is reused: the use sends no setup for
        // it. A draft tells before its use claims the account, and the saved approval tells
        // of a destination account whose network isn't loaded.
        let swap_use = swap.and_then(|swap| record.swap_use(swap.swap_use));
        let reuses = |account: &ExecutorRecord| {
            swap.and_then(|swap| account.swap_use(swap.swap_use))
                .is_some_and(|swap_use| !swap_use.is_fresh())
        };
        let source_reused = reuses(record)
            || self.pending_order(record).is_some_and(|pending| {
                pending.reuse_account && swap.is_some_and(|swap| swap.swap_use == pending.swap_use)
            });
        let destination = destination_account_metadata(
            destination,
            swap.map(|swap| swap.swap_use),
            swap_use.and_then(SwapUseRecord::approval),
        );
        let destination_fresh = destination.fresh_from_record_or_approval();
        [
            SwapSetupLabels {
                network: form::network_name(self.origin_chain_id),
                account: record.address().map(|address| SwapStepAccount {
                    index: Some(record.index()),
                    address,
                }),
                progress: origin,
                reused: source_reused,
                detail: waiting,
            },
            SwapSetupLabels {
                network: form::network_name(delivery.destination_chain),
                account: (!delivery.receiver.is_zero()).then(|| SwapStepAccount {
                    index: destination.record.map(ExecutorRecord::index),
                    address: delivery.receiver,
                }),
                progress: if placed {
                    SwapSetupProgress::Done
                } else {
                    self.destination_setup_progress(record, delivery)
                },
                reused: !destination_fresh,
                detail: None,
            },
        ]
    }

    /// How swaps name a Public address receiver: the wallet's label for it, or its short
    /// address. `None` for a swap back to the private balance.
    fn receiver_name(&self, delivery: SwapDelivery, cx: &gpui::App) -> Option<String> {
        let SwapDelivery::External { receiver } = delivery else {
            return None;
        };
        Some(
            self.receiver_label(receiver, cx)
                .map_or_else(|| short_receiver(receiver), |(label, _)| label),
        )
    }

    /// The orderbook's report that the latest order filled, while canonical observation still
    /// shows it open.
    fn fill_hint(&self, record: &ExecutorRecord, cx: &gpui::App) -> Option<SwapFillHint> {
        if !matches!(
            self.stage(record),
            SwapStage::Order(SwapOrderState::Open | SwapOrderState::PreHookOnly { expired: false })
        ) {
            return None;
        }
        let order = record.swap()?.orders().last()?;
        let hint = self
            .tracking
            .get(&record.operation())?
            .order_hint
            .filter(|hint| {
                hint.uid == order.uid() && hint.report.status == CowOrderStatusHint::Fulfilled
            })?;
        let root = self.root.upgrade()?;
        let root = root.read(cx);
        let depth = root
            .effective_chain_configs
            .get(self.origin_chain_id)?
            .finality_depth;
        let head = root
            .chain_states
            .get(&self.origin_chain_id)
            .and_then(super::chain_load::ChainUtxoState::sync_tip)
            .and_then(|tip| tip.head_block);
        Some(SwapFillHint {
            confirmations: hint
                .trade_block
                .zip(head)
                .map(|(block, head)| head.saturating_sub(block)),
            depth,
        })
    }

    /// The swaps the Private tab shows, newest first.
    fn shown_swaps(&self) -> impl Iterator<Item = (&ExecutorRecord, SwapStage)> {
        self.records
            .iter()
            .filter(|record| is_swap_record(record) || self.pending_order(record).is_some())
            .filter_map(|record| {
                let stage = self.progress_stage(record);
                (!record.is_swap_setup_stopped()
                    && (self.pending_order(record).is_some()
                        || stage.is_shown_on_private_tab(
                            record.is_hidden(),
                            model::swap_valid_to(record)
                                .is_some_and(|valid_to| valid_to < now_unix()),
                        )))
                .then_some((record, stage))
            })
    }

    fn next_observations(
        &self,
        cx: &gpui::App,
    ) -> Option<(
        tokio::runtime::Handle,
        Arc<ExecutorOwner>,
        Vec<ObservationPage>,
    )> {
        if self.session.is_none() || !self.session_is_current(cx) {
            return None;
        }
        let confirmed = self.confirmed_block(cx)?;
        let busy = self.job.as_ref().map(|job| job.operation);
        let pages = self
            .records
            .iter()
            .filter(|record| Some(record.operation()) != busy)
            // A stopped setup never places its order, so nothing waits on its observation. Its
            // notes stay reserved until the user checks or releases them as locked notes.
            .filter(|record| !record.is_swap_setup_stopped())
            // Only unfinished work is observed automatically. Ended swaps retain their
            // confirmed history across restart, even when hidden or lacking old evidence.
            // An explicit retry, recovery or lock check refreshes that one account.
            .filter(|record| self.stage(record).is_observed())
            .filter_map(|record| {
                let tracking = self.tracking.get(&record.operation());
                if let Some(order) = record.swap().and_then(|swap| swap.orders().last())
                    && !tracking.is_some_and(|tracking| tracking.cancelling)
                {
                    // Private sync's spend of the pre-hook inputs locates a settlement the
                    // orderbook hasn't reported. It comes last: anyone can run the public
                    // pre-hook alone, and that block would never hold the trade.
                    let block = tracking
                        .and_then(|tracking| tracking.order_hint)
                        .filter(|hint| hint.uid == order.uid())
                        .and_then(|hint| hint.trade_block)
                        .or_else(|| order.observations().traded.map(|trade| trade.block.number))
                        .or_else(|| {
                            self.private_owner()?
                                .synced_settlement_block(record, order.uid())
                        })?;
                    return (block <= confirmed).then_some(ObservationPage {
                        operation: record.operation(),
                        settlement: Some((order.uid(), block)),
                        // Settlement checks do not page through account history or catch up.
                        range: Some(confirmed..confirmed.saturating_add(1)),
                        destination: None,
                    });
                }
                // A setup waits on one read of its account at the confirmed block each pass.
                if record.swap().is_none() {
                    return has_issued_setup(record).then_some(ObservationPage {
                        operation: record.operation(),
                        settlement: None,
                        range: None,
                        destination: None,
                    });
                }
                let cursor = tracking
                    .and_then(|tracking| tracking.cursor)
                    .or_else(|| swap_observation_start(record))?;
                Some(ObservationPage {
                    operation: record.operation(),
                    settlement: None,
                    range: Some(swap_observation_range(cursor, confirmed)),
                    destination: None,
                })
            })
            .chain(
                self.records
                    .iter()
                    .filter(|record| Some(record.operation()) != busy)
                    .filter_map(|record| self.destination_observation(record, cx)),
            )
            .collect::<Vec<_>>();
        let owner = self.private_owner().filter(|_| !pages.is_empty())?;
        Some((self.runtime.clone(), Arc::clone(owner), pages))
    }

    /// The next observation of a private Bridge swap's destination stealth account, on its own
    /// network and through that network's owner, while its setup is unconfirmed and the swap
    /// has no order. Each pass reads the account once, like the swap's own setup. A reused
    /// account's draft is such a swap while its swap use has no order, whatever orders the
    /// account's earlier swaps have.
    fn destination_observation(
        &self,
        record: &ExecutorRecord,
        cx: &gpui::App,
    ) -> Option<ObservationPage> {
        if record.is_swap_setup_stopped() {
            return None;
        }
        let (delivery, destination) = match self.pending_order(record) {
            Some(pending) if pending.reuse_account => {
                let swap = SwapIdentity {
                    operation: record.operation(),
                    swap_use: pending.swap_use,
                };
                let (delivery, _) = model::swap_use_destination(record, pending.swap_use)?;
                (delivery, self.swap_destination_account(swap, delivery)?)
            }
            _ if record.swap().is_some() || record.is_retired() => return None,
            _ => {
                let delivery = swap_private_delivery(record)?;
                (delivery, self.destination_account(record, delivery)?)
            }
        };
        let chain_id = delivery.destination_chain;
        let tracking = self.tracking.get(&record.operation());
        let observed = tracking.and_then(|tracking| tracking.destination_setup);
        if account_stage(
            destination,
            chain_id,
            observed,
            false,
            self.attribution(destination),
        ) != SwapStage::SetupPending
        {
            return None;
        }
        let (_, owner) = self.destination_owner(chain_id, cx)?;
        // Nothing is read before the destination network has a confirmed block.
        self.root.upgrade()?.read(cx).confirmed_block(chain_id)?;
        has_issued_setup(destination).then_some(ObservationPage {
            operation: destination.operation(),
            settlement: None,
            range: None,
            destination: Some(DestinationPage {
                owner,
                origin: record.operation(),
            }),
        })
    }

    /// Orders to ask the orderbook about in this pass, at most one report each. An open order
    /// is asked about until its settlement is verified, including fills while offline.
    /// These reports only locate evidence; they cannot change a persisted outcome. A traded
    /// order's executed fee is asked for once per session until it's recorded, also after
    /// verification ends the reports.
    ///
    /// Handed-off Bridge orders ask their provider through the same route until an outcome is
    /// recorded, including after a restart, backing off while it reports none. A destination
    /// chain that isn't enabled here isn't tracked.
    fn next_order_hints(
        &self,
        cx: &gpui::App,
    ) -> Option<(tokio::runtime::Handle, Arc<ExecutorOwner>, Vec<HintRequest>)> {
        if self.session.is_none() || !self.session_is_current(cx) {
            return None;
        }
        let root = self.root.upgrade();
        let chains = root
            .as_ref()
            .map(|root| &root.read(cx).effective_chain_configs);
        let busy = self.job.as_ref().map(|job| job.operation);
        let now = Instant::now();
        let requests = self
            .records
            .iter()
            .filter(|record| Some(record.operation()) != busy)
            .filter_map(|record| {
                let order = record.swap()?.orders().last()?;
                let tracking = self.tracking.get(&record.operation());
                // Until verification succeeds, keep refreshing the untrusted location,
                // including after a reorg moves a reported trade to a different block.
                let wanted = matches!(
                    self.stage(record),
                    SwapStage::SubmissionPending
                        | SwapStage::SubmissionRejected
                        | SwapStage::Order(
                            SwapOrderState::Open
                                | SwapOrderState::PreHookOnly { expired: false }
                                | SwapOrderState::Traded,
                        )
                );
                let bridges = record
                    .swap_bridges_to_track()
                    .filter(|order| {
                        tracking
                            .and_then(|tracking| tracking.bridge_polls.get(&order.uid()))
                            .is_none_or(|poll| poll.next_at <= now)
                    })
                    .filter_map(|order| {
                        let SwapDelivery::Bridge(delivery) = order.delivery() else {
                            return None;
                        };
                        let chain = chains?
                            .get(delivery.destination_chain)
                            .filter(|chain| chain.enabled)?;
                        Some((order.uid(), chain.clone()))
                    })
                    .collect::<Vec<_>>();
                // A verified trade ends routine hints, but its fee is asked for once.
                let fee = model::needs_executed_fee(order)
                    && tracking.is_none_or(|tracking| !tracking.fee_asked.contains(&order.uid()));
                (wanted || fee || !bridges.is_empty()).then(|| HintRequest {
                    operation: record.operation(),
                    uid: order.uid(),
                    hint: wanted,
                    fee,
                    client: tracking.and_then(|tracking| tracking.orderbook.clone()),
                    bridges,
                })
            })
            .collect::<Vec<_>>();
        let owner = self.private_owner().filter(|_| !requests.is_empty())?;
        Some((self.runtime.clone(), Arc::clone(owner), requests))
    }

    fn apply_order_hints(&mut self, results: Vec<HintResult>, cx: &mut Context<'_, Self>) {
        if !self.session_is_current(cx) {
            return;
        }
        let now = Instant::now();
        let mut settled = Vec::new();
        for result in results {
            let tracking = self.tracking.entry(result.operation).or_default();
            if tracking.orderbook.is_none() {
                tracking.orderbook = result.client;
            }
            if result.fee {
                tracking.fee_asked.insert(result.uid);
            }
            // An outcome ends the order's polling; otherwise the next poll waits longer.
            for (uid, outcome) in result.bridges {
                if outcome {
                    tracking.bridge_polls.remove(&uid);
                    settled.push(result.operation);
                } else {
                    let attempts = tracking
                        .bridge_polls
                        .get(&uid)
                        .map_or(0, |poll| poll.attempts);
                    tracking.bridge_polls.insert(
                        uid,
                        BridgePoll {
                            attempts: attempts.saturating_add(1),
                            next_at: now + bridge_poll_interval(attempts),
                        },
                    );
                }
            }
            let Some((report, trade_block)) = result.report else {
                continue;
            };
            let known = tracking
                .order_hint
                .filter(|hint| hint.uid == result.uid)
                .and_then(|hint| hint.trade_block);
            tracking.order_hint = Some(SwapOrderHint {
                uid: result.uid,
                report,
                trade_block: if report.status == CowOrderStatusHint::Fulfilled {
                    trade_block.or(known)
                } else {
                    None
                },
            });
        }
        for operation in settled {
            self.reconcile_destination(operation, cx);
        }
        // Forget orders that are no longer tracked, such as those with a recorded outcome.
        let records = &self.records;
        for (operation, tracking) in &mut self.tracking {
            let record = records
                .iter()
                .find(|record| record.operation() == *operation);
            tracking.bridge_polls.retain(|uid, _| {
                record.is_some_and(|record| {
                    record
                        .swap_bridges_to_track()
                        .any(|order| order.uid() == *uid)
                })
            });
        }
        cx.notify();
    }

    fn apply_observations(
        &mut self,
        results: Vec<ObservationResult>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<ExecutorOperationId> {
        if !self.session_is_current(cx) {
            return Vec::new();
        }
        let confirmed = self.confirmed_block(cx);
        let mut catchup = Vec::new();
        for result in results {
            // Another task updated the account during this background read. Keep the page
            // due for the next pass; no action failed and no stale result may advance it.
            if result
                .outcome
                .as_ref()
                .is_err_and(eyre::Report::is::<wallet_ops::ExecutorRecordChanged>)
            {
                continue;
            }
            // A destination account's result is kept with its swap.
            if let Some(origin) = result.destination {
                let tracking = self.tracking.entry(origin).or_default();
                match result.outcome {
                    Ok(setup) => {
                        tracking.error = None;
                        if setup.is_some() {
                            tracking.destination_setup = setup;
                        }
                    }
                    Err(error) => tracking.error = Some(format!("{error:#}")),
                }
                continue;
            }
            let tracking = self.tracking.entry(result.operation).or_default();
            match result.outcome {
                Ok(setup) => {
                    tracking.error = None;
                    // Only an order's history is paged. A page short of the confirmed block
                    // is followed at once.
                    if let Some(end) = result.range_end {
                        tracking.cursor = Some(end);
                        if confirmed.is_some_and(|block| end <= block) {
                            catchup.push(result.operation);
                        }
                    }
                    if setup.is_some() {
                        tracking.setup = setup;
                    }
                }
                Err(error) => tracking.error = Some(format!("{error:#}")),
            }
        }
        self.reload_records();
        self.reload_destinations(cx);
        // A cancellation is settled once canonical observation says which payload won.
        let settled = self
            .records
            .iter()
            .filter(|record| !self.stage(record).is_observed())
            .map(ExecutorRecord::operation)
            .collect::<Vec<_>>();
        for operation in settled {
            if let Some(tracking) = self.tracking.get_mut(&operation) {
                tracking.cancelling = false;
            }
        }
        self.continue_form_after_observation(window, cx);
        self.continue_approved_swaps(window, cx);
        cx.notify();
        catchup
    }

    /// Place the order of a swap approved with its setup in this session once every setup it
    /// needs is confirmed: its own account's, a private Bridge swap's destination account's,
    /// or both. After a restart, or once the user declined, its detail offers the step.
    /// Only this swap's detail may advance while a dialog is open.
    fn continue_approved_swaps(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.busy() || self.form.is_some() || !self.session_is_current(cx) {
            return;
        }
        let next = self
            .records
            .iter()
            .filter(|record| self.approved_use(record).is_some())
            .map(ExecutorRecord::operation)
            .find(|operation| {
                self.tracking
                    .get(operation)
                    .is_some_and(|tracking| tracking.auto_place)
            });
        if let Some(operation) = next
            && (!window.has_active_dialog(cx) || self.detail_is_active(operation, window, cx))
        {
            self.place_approved_order(operation, window, cx);
        }
    }

    /// Start a signing or submission job for one swap. Only one runs at a time.
    fn start_job<T: Send + 'static>(
        &mut self,
        operation: ExecutorOperationId,
        kind: SwapJobKind,
        work: impl Future<Output = eyre::Result<T>> + Send + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Window, &mut Context<'_, Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        self.error = None;
        let tracking = self.tracking.entry(operation).or_default();
        tracking.error = None;
        tracking.status_checked = None;
        self.job_revision = self.job_revision.wrapping_add(1);
        let revision = self.job_revision;
        let join = self.runtime.spawn(work);
        self.job = Some(SwapJob {
            operation,
            kind,
            abort: join.abort_handle(),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.job_revision != revision || !this.session_is_current(cx) {
                    return;
                }
                this.job = None;
                this.reload_records();
                if kind == SwapJobKind::Check {
                    // A check may persist an outcome before a later RPC fails. Reconcile
                    // the destination on either result, just as routine bridge polls do.
                    this.reconcile_destination(operation, cx);
                }
                this.reload_destinations(cx);
                match result {
                    Ok(Ok(value)) => apply(this, value, window, cx),
                    Ok(Err(error)) => {
                        let message = this.job_error(&error, cx);
                        this.fail(operation, message);
                    }
                    Err(error) if !error.is_cancelled() => this.fail(
                        operation,
                        "The local operation stopped unexpectedly. Check the swap before retrying."
                            .into(),
                    ),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        if kind == SwapJobKind::Order {
            self.show_detail(operation, window, cx);
        }
        cx.notify();
    }

    fn job_error(&self, error: &eyre::Report, cx: &gpui::App) -> String {
        let Some(exceeded) = error.downcast_ref::<wallet_ops::ExecutorPrivateFeeLimitExceeded>()
        else {
            return format!("{error:#}");
        };
        let root = self.root.upgrade();
        let registry = root
            .as_ref()
            .map(|root| &root.read(cx).effective_token_registry);
        let amount = |value| {
            super::format_exact_token_amount_for_display(
                self.origin_chain_id,
                exceeded.fee_token(),
                value,
                registry,
            )
        };
        format!(
            "Setup fee increased. Approved maximum: {}. New estimate: {}, exceeding the maximum by {}. Continue to review the new fee limit.",
            amount(exceeded.maximum()),
            amount(exceeded.required()),
            amount(exceeded.required() - exceeded.maximum()),
        )
    }

    fn fail(&mut self, operation: ExecutorOperationId, message: String) {
        if let Some(form) = self
            .form
            .as_mut()
            .filter(|form| form.operation() == Some(operation))
        {
            form.set_error(message.clone());
        }
        self.tracking.entry(operation).or_default().error = Some(message.clone());
        self.error = Some(message);
    }

    fn request_authorization(
        &mut self,
        action: SwapAction,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let destination = self.authorized_destination(&action);
        let destination_chain = match &action {
            SwapAction::Setup(approval) => approval.destination_account().map(|(chain, _)| chain),
            SwapAction::DestinationSetup(approval) => Some(approval.setup.chain_id),
            SwapAction::Order(approval) => approval
                .private_delivery()
                .map(|delivery| delivery.destination_chain),
            SwapAction::Cancel(_) => None,
        };
        let destination_session = match destination_chain {
            Some(chain) => match self.ready_destination(chain, cx) {
                Ok((session, _)) => Some(session),
                Err(error) => {
                    self.fail(action.operation(), error);
                    cx.notify();
                    return;
                }
            },
            None => None,
        };
        let Some(session) = self.private_session().cloned() else {
            return;
        };
        let command = Arc::new(SwapAuthorization {
            session,
            destination_session,
            action,
            destination,
        });
        self.pending_authorization = Some(Arc::clone(&command));
        let view = cx.entity();
        // Reviews require explicit approval; the confirm-only step for an approved order
        // accepts a remembered spend authorization.
        let _ = self.root.update(cx, |root, cx| {
            if command.sessions_are_current(root) {
                root.request_spend_authorization(
                    SpendAuthorizationIntent::PrivateSwap(view, command),
                    summary,
                    window,
                    cx,
                );
            }
        });
        cx.notify();
    }

    /// The destination stealth account `action` also signs for: a private Bridge swap's, when
    /// its setup sets that account up, or its order pre-signs that account's shield.
    fn authorized_destination(&self, action: &SwapAction) -> Option<(u64, ExecutorOperationId)> {
        match action {
            SwapAction::Setup(approval) => approval.destination_account(),
            SwapAction::Order(approval) => {
                let delivery = approval.private_delivery()?;
                // An existing destination the order claims with its account before it signs.
                if let Some(destination) = approval.pair_destination {
                    return Some((destination.chain_id, destination.operation));
                }
                // The destination the order's own swap use names, not a later use's.
                let record = self.record(approval.operation)?;
                let swap_use = approval.swap_use.or_else(|| record.active_swap_use())?;
                let (_, operation) = model::swap_use_destination(record, swap_use)?;
                Some((delivery.destination_chain, operation))
            }
            // A setup sent again on the destination network is authorized there itself.
            SwapAction::DestinationSetup(_) | SwapAction::Cancel(_) => None,
        }
    }

    pub(super) fn cancel_authorization(
        &mut self,
        command: &Arc<SwapAuthorization>,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .pending_authorization
            .as_ref()
            .is_some_and(|pending| Arc::ptr_eq(pending, command))
        {
            self.pending_authorization = None;
            cx.notify();
        }
    }

    /// Continue the action `command` asked authorization for. A private Bridge swap's setup or
    /// order also signs on its destination network: a hardware wallet's approval arrives with
    /// `destination`, the second authorization of the same device session, and a software
    /// wallet's `authorization` is duplicated for it.
    pub(super) fn continue_authorized(
        &mut self,
        command: &Arc<SwapAuthorization>,
        authorization: DesktopPrivateSpendAuthorization,
        destination: Option<DesktopPrivateSpendAuthorization>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .pending_authorization
            .take()
            .is_some_and(|current| Arc::ptr_eq(&current, command))
            || !self.session_is_current(cx)
            || !self
                .root
                .upgrade()
                .is_some_and(|root| command.sessions_are_current(root.read(cx)))
        {
            return;
        }
        let destination = match (destination, command.destination) {
            (_, None) => None,
            (Some(destination), Some(_)) => Some(destination),
            (None, Some(_)) => match authorization.for_destination() {
                Ok(destination) => Some(destination),
                Err(error) => {
                    self.fail(command.action.operation(), format!("{error:#}"));
                    cx.notify();
                    return;
                }
            },
        };
        match command.action.clone() {
            SwapAction::Setup(approval) => {
                self.submit_setup(*approval, authorization, destination, window, cx);
            }
            SwapAction::DestinationSetup(approval) => {
                self.submit_destination_setup(*approval, authorization, window, cx);
            }
            SwapAction::Order(approval) => {
                self.submit_order(*approval, authorization, destination, window, cx);
            }
            SwapAction::Cancel(approval) => {
                self.submit_cancellation(*approval, authorization, window, cx);
            }
        }
    }

    /// The broadcaster network client for a paid setup or cancellation.
    fn broadcaster_network(&self, cx: &mut Context<'_, Self>) -> Option<Arc<WakuDeliveryClient>> {
        self.root
            .update(cx, |root, cx| {
                root.ensure_waku_for_delivery(super::DeliveryMode::PublicBroadcaster, cx);
                root.active_waku()
            })
            .ok()
            .flatten()
    }

    /// Candidates and policy for paying a broadcaster on `chain_id` in `fee_token` for a swap's
    /// executor there.
    fn broadcaster_candidates(
        &self,
        chain_id: u64,
        fee_token: Address,
        allow_out_of_range: bool,
        favorites_only: bool,
        cx: &gpui::App,
    ) -> Vec<PublicBroadcasterCandidate> {
        let Some(root) = self.root.upgrade() else {
            return Vec::new();
        };
        let root = root.read(cx);
        let Some(profile) = root
            .effective_chain_configs
            .get(chain_id)
            .and_then(wallet_ops::settings::EffectiveChainConfig::accepted_executor_profile)
        else {
            return Vec::new();
        };
        let policy = root.public_broadcaster_fee_policy(allow_out_of_range);
        wallet_ops::fee_policy_eligible_public_broadcasters(
            &super::public_broadcaster::public_broadcaster_candidates_for_route(
                &root.monitor_fee_rows(),
                chain_id,
                fee_token,
                None,
                Some(profile),
                policy,
                root.public_broadcaster_anchor_cache
                    .cached_rate(chain_id, fee_token),
                &root.public_broadcaster_trust_filter(favorites_only),
            ),
            policy,
        )
    }
}

/// Whether a source use's saved approval has the swap set its destination account up. An
/// approval from before accounts were bound has it do so, as every such swap did.
fn sets_up_destination(swap_use: &SwapUseRecord) -> bool {
    swap_use.approval().is_some_and(|approval| {
        approval
            .accounts
            .and_then(|accounts| accounts.destination)
            .is_none_or(|account| account.setup)
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
    outcome: eyre::Result<Option<SwapSetupStatus>>,
    /// A destination account's swap.
    destination: Option<ExecutorOperationId>,
}

struct HintRequest {
    operation: ExecutorOperationId,
    uid: OrderUid,
    /// Whether to ask the orderbook about `uid`.
    hint: bool,
    /// Whether to read the traded order's executed fee, once per session.
    fee: bool,
    /// The swap's own orderbook route, when this session has one.
    client: Option<CowOrderbookClient>,
    /// Handed-off Bridge orders to poll, with their destination chain.
    bridges: Vec<(OrderUid, EffectiveChainConfig)>,
}

struct HintResult {
    operation: ExecutorOperationId,
    uid: OrderUid,
    client: Option<CowOrderbookClient>,
    /// The executed fee was asked for, whether or not the read succeeded.
    fee: bool,
    /// Each polled Bridge order, and whether it has an outcome now.
    bridges: Vec<(OrderUid, bool)>,
    /// The report and trade block, or `None` when the orderbook couldn't be asked.
    report: Option<(CowOrderStatusReport, Option<u64>)>,
}

/// Ask the orderbook about each order through its swap's own route, sending only the order
/// UID. A failed request leaves the last report; the client logs failures without URLs. A
/// traded order's executed fee is read once, and the owner persists it; a failed read is
/// dropped.
/// Bridge providers are polled on the same route, and the owner persists any outcome. The
/// caller backs off after a poll without an outcome, including a failed one.
async fn fetch_order_hints(
    owner: Arc<ExecutorOwner>,
    requests: Vec<HintRequest>,
) -> Vec<HintResult> {
    let mut results = Vec::with_capacity(requests.len());
    for request in requests {
        let client = match request.client {
            Some(client) => Some(client),
            None => owner.swap_orderbook_client().await.ok(),
        };
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
        // Without clients, every poll counts as one without an outcome.
        let clients = if request.bridges.is_empty() {
            None
        } else {
            client
                .as_ref()
                .and_then(|client| owner.swap_bridge_clients(client).ok())
        };
        let mut bridges = Vec::with_capacity(request.bridges.len());
        for (uid, destination) in &request.bridges {
            let outcome = if let Some(clients) = &clients {
                matches!(
                    Box::pin(owner.observe_swap_bridge(
                        request.operation,
                        *uid,
                        clients,
                        destination,
                    ))
                    .await,
                    Ok(Some(_))
                )
            } else {
                false
            };
            bridges.push((*uid, outcome));
        }
        results.push(HintResult {
            operation: request.operation,
            uid: request.uid,
            client,
            fee: request.fee,
            bridges,
            report,
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
        let owner = page
            .destination
            .as_ref()
            .map_or(&owner, |destination| &destination.owner);
        let outcome = if let Some((uid, block)) = page.settlement {
            Box::pin(owner.observe_swap_settlement(page.operation, uid, block))
                .await
                .map(|()| None)
        } else if let Some(range) = page.range.clone() {
            Box::pin(owner.observe_swap(page.operation, range))
                .await
                .map(|_| None)
        } else {
            Box::pin(owner.observe_swap_setup(page.operation))
                .await
                .map(Some)
        };
        results.push(ObservationResult {
            operation: page.operation,
            range_end: page.range.map(|range| range.end),
            outcome,
            destination: page.destination.map(|destination| destination.origin),
        });
    }
    results
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
    use chrono::Datelike as _;
    let at =
        chrono::DateTime::<chrono::Local>::from(std::time::UNIX_EPOCH + Duration::from_secs(at));
    let today = chrono::Local::now().date_naive();
    let day = at.date_naive();
    let time = at.format("%H:%M");
    if day == today {
        format!("Today {time}")
    } else if today.pred_opt() == Some(day) {
        format!("Yesterday {time}")
    } else if day.year() == today.year() {
        format!("{} {time}", at.format("%b %-d"))
    } else {
        format!("{} {time}", at.format("%b %-d, %Y"))
    }
}

pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wallet_ops::settings::build_effective_chain_configs;

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
}
