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

use alloy::primitives::{Address, B256, U256};
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
        ExecutorOperationId, ExecutorRecord, IssuedExecutorPayload, SwapBridgeOutcome,
        SwapDelivery, SwapOrderRecord,
    },
};

use super::WalletRoot;
use super::spend_authorization::{SpendAuthorizationIntent, SpendAuthorizationSummary};

mod dialog;
mod form;
mod model;
mod progress;

use form::SwapForm;
use model::{
    SwapBridgeLabels, SwapFillHint, SwapLabels, bridge_sent_amount, swap_history_start,
    swap_observation_range,
};
pub(super) use model::{
    SwapStage, swap_account_status, swap_delivery, swap_pair_label, swap_recovery_token,
    swap_sell_amount, swap_stage, swap_tokens,
};

/// Delay between caught-up observation passes, and before retrying failed reads.
const SWAP_OBSERVATION_INTERVAL: Duration = Duration::from_secs(12);
/// Longest wait between routine polls of a bridge provider about one order.
const MAX_BRIDGE_POLL_INTERVAL: Duration = Duration::from_mins(5);
/// A setup still unconfirmed this many blocks (about 30 minutes) after it was sent is checked
/// only every [`DEFERRED_SETUP_OBSERVATION_INTERVAL`], sparing the account repeated RPC reads.
const STALE_SETUP_BLOCKS: u64 = 150;
/// Time between account reads of a deferred setup.
const DEFERRED_SETUP_OBSERVATION_INTERVAL: Duration = Duration::from_mins(5);
const SWAP_BROADCASTER_RESPONSE_TIMEOUT: Duration = Duration::from_mins(2);
const SWAP_BROADCASTER_REPUBLISH_INTERVAL: Duration = Duration::from_secs(5);

pub(super) struct PrivateSwapsPanel {
    session: Arc<WalletSession>,
    view: Entity<PrivateSwapsView>,
}

/// A spend the user authorizes for a swap: its setup with the approved order, the order, or an
/// early cancellation.
pub(super) struct SwapAuthorization {
    session: Arc<WalletSession>,
    action: SwapAction,
}

#[derive(Clone)]
enum SwapAction {
    Setup(Box<form::SetupApproval>),
    Order(Box<form::OrderApproval>),
    Cancel(Box<progress::CancelApproval>),
}

impl SwapAuthorization {
    pub(super) fn hardware_executor_action(&self) -> HardwareExecutorAction {
        match &self.action {
            SwapAction::Setup(approval) => HardwareExecutorAction::Execute(approval.operation),
            SwapAction::Order(approval) => HardwareExecutorAction::Execute(approval.operation),
            SwapAction::Cancel(approval) => HardwareExecutorAction::Recover(approval.operation),
        }
    }

    pub(super) const fn session(&self) -> &Arc<WalletSession> {
        &self.session
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SwapJobKind {
    Setup,
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
    started_at: u64,
}

impl PendingSwapOrder {
    fn starts_new_swap(self, record: &ExecutorRecord) -> bool {
        let Some(swap) = record.swap() else {
            return false;
        };
        let Some(order) = swap.orders().last() else {
            return false;
        };
        let terms = swap.order_terms(order);
        order.observations().traded.is_some()
            || (self.sell, self.buy, self.delivery)
                != (terms.sell_token(), terms.buy_token(), order.delivery())
    }
}

/// This session's knowledge about one swap that its record doesn't hold.
#[derive(Default)]
struct SwapTracking {
    pending_order: Option<PendingSwapOrder>,
    /// The last setup observation, including the delegated executor once confirmed.
    setup: Option<SwapSetupStatus>,
    /// The next block to observe.
    cursor: Option<u64>,
    /// When this session's last observation of the account reached the confirmed block.
    setup_read_at: Option<Instant>,
    /// The latest setup attempt handed off in this session, and when its transaction was
    /// first recorded. Private sync locates it, so the hand-off defers the first account read.
    located_at: Option<(B256, Instant)>,
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
    /// An early cancellation was handed to a broadcaster; the observation decides the winner.
    cancelling: bool,
    /// The user approved this swap's setup in this session: once the setup is confirmed, the
    /// wallet checks the approved terms again and asks to place the order without waiting
    /// for the user to return. Cleared after the first attempt, so failures don't repeat.
    auto_place: bool,
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
    session: Arc<WalletSession>,
    owner: Arc<ExecutorOwner>,
    runtime: tokio::runtime::Handle,
    active: bool,
    /// Swap executor records only, newest first.
    records: Vec<ExecutorRecord>,
    /// Setup payloads with a recorded inclusion as of the last reload; `None` before the first.
    setup_inclusions: Option<BTreeSet<(ExecutorOperationId, B256)>>,
    /// Counts newly recorded setup inclusions. The observation loop starts its next pass when
    /// it changes, so wakes during a pass coalesce into one follow-up pass.
    observation_wake: watch::Sender<u64>,
    tracking: BTreeMap<ExecutorOperationId, SwapTracking>,
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
    job: Option<SwapJob>,
    job_revision: u64,
    error: Option<String>,
    _changes: Task<()>,
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
        // Swaps exist only on chains with a swap profile, for wallets that run executors there.
        let Some(session) = self
            .stealth_session()
            .filter(|_| self.view_session.is_some())
            .filter(|session| {
                self.effective_chain_configs
                    .get(session.chain_id)
                    .and_then(wallet_ops::settings::EffectiveChainConfig::swap_profile)
                    .is_some()
            })
        else {
            self.clear_private_swaps(cx);
            return;
        };
        if self
            .private_swaps
            .as_ref()
            .is_some_and(|panel| Arc::ptr_eq(&panel.session, &session))
        {
            return;
        }
        self.clear_private_swaps(cx);
        let owner = session.executor_owner().expect("checked executor session");
        let root = cx.entity().downgrade();
        let runtime = self.runtime.clone();
        let view = cx.new(|cx| {
            PrivateSwapsView::new(root, Arc::clone(&session), owner, runtime, window, cx)
        });
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        self.private_swaps = Some(PrivateSwapsPanel { session, view });
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
                view.tracking.clear();
                view.records.clear();
                cx.notify();
            });
        }
    }

    /// The swap view of the current executor session.
    pub(super) fn private_swaps_view(&self) -> Option<Entity<PrivateSwapsView>> {
        self.private_swaps
            .as_ref()
            .filter(|panel| self.stealth_session_is_current(&panel.session))
            .map(|panel| panel.view.clone())
    }

    /// Read the swap form's balances again once a private snapshot of `chain_id` replaced the
    /// root's. The swap view hears of the same observation on its own and may read the root
    /// before this update, so it reads again after it.
    pub(super) fn refresh_private_swap_assets(&self, chain_id: u64, cx: &mut Context<'_, Self>) {
        if self
            .private_swaps
            .as_ref()
            .is_none_or(|panel| panel.session.chain_id != chain_id)
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
    fn new(
        root: WeakEntity<WalletRoot>,
        session: Arc<WalletSession>,
        owner: Arc<ExecutorOwner>,
        runtime: tokio::runtime::Handle,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mut changes = owner.subscribe();
        let mut private_changes = session.observation_rx.clone();
        let mut tip_changes = session.sync_tip_rx.clone();
        let changes_task = cx.spawn(async move |this, cx| {
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
        });
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
            session,
            owner,
            runtime,
            active: true,
            records: Vec::new(),
            setup_inclusions: None,
            observation_wake,
            tracking: BTreeMap::new(),
            open_orders: 0,
            dialog: None,
            orders_filter: None,
            orders_list: None,
            form: None,
            cancel: None,
            pending_authorization: None,
            reapproval: None,
            job: None,
            job_revision: 0,
            error: None,
            _changes: changes_task,
            _polling: polling,
            _hint_polling: hint_polling,
        };
        view.reload_records();
        view
    }

    fn session_is_current(&self, cx: &gpui::App) -> bool {
        self.active
            && self
                .root
                .upgrade()
                .is_some_and(|root| root.read(cx).stealth_session_is_current(&self.session))
    }

    fn reload_records(&mut self) {
        match self.owner.records() {
            Ok(records) => {
                let selected = self.form.as_ref().and_then(SwapForm::operation);
                let mut records = records
                    .into_iter()
                    .filter(|record| {
                        is_swap_record(record)
                            || Some(record.operation()) == selected
                            || self.pending_order(record).is_some()
                    })
                    .collect::<Vec<_>>();
                records
                    .sort_by_key(|record| std::cmp::Reverse((record.created_at(), record.index())));
                // Only a newly recorded setup inclusion wakes the observation loop: it makes the
                // setup's account check due at once. Waking on every owner change would spin,
                // since each pass records its own observations.
                let inclusions = records
                    .iter()
                    .filter(|record| record.swap().is_none())
                    .flat_map(|record| {
                        included_setups(record).map(|payload| (record.operation(), payload.hash()))
                    })
                    .collect::<BTreeSet<_>>();
                if self
                    .setup_inclusions
                    .as_ref()
                    .is_some_and(|known| !inclusions.is_subset(known))
                {
                    self.observation_wake
                        .send_modify(|wakes| *wakes = wakes.wrapping_add(1));
                }
                // A setup attempt whose transaction is first recorded after the initial load was
                // handed off in this session. Later reloads keep that instant; a new attempt
                // replaces it. After a restart, a setup located before the first load is read
                // at the usual pace.
                if self.setup_inclusions.is_some() {
                    let known = self
                        .records
                        .iter()
                        .filter_map(located_setup)
                        .collect::<BTreeSet<_>>();
                    for record in records.iter().filter(|record| record.swap().is_none()) {
                        let Some(latest) = latest_setup(record) else {
                            continue;
                        };
                        let hash = latest.hash();
                        let tracking = self.tracking.entry(record.operation()).or_default();
                        if tracking
                            .located_at
                            .is_some_and(|(located, _)| located != hash)
                        {
                            tracking.located_at = None;
                        }
                        if tracking.located_at.is_none()
                            && !latest.transaction_hashes().is_empty()
                            && !known.contains(&hash)
                        {
                            tracking.located_at = Some((hash, Instant::now()));
                        }
                    }
                }
                self.setup_inclusions = Some(inclusions);
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
        swap_stage(
            record,
            match setup {
                // A setup recorded as executed for the accepted delegate is ready without
                // this session reading the account, so `swap_stage` uses the recorded
                // outcome. That readiness can outlast a reorg until reconciliation; order
                // preparation checks the delegation afresh before anything is signed.
                None | Some(SwapSetupStatus::Pending) if self.setup_recorded_executed(record) => {
                    None
                }
                // Any other stored execution waits for this session's account check, and a
                // failure that check reported stands.
                setup => setup.or(Some(SwapSetupStatus::Pending)),
            },
            submitting,
        )
    }

    /// Whether the record's setup is recorded as executed for this chain's accepted delegate.
    fn setup_recorded_executed(&self, record: &ExecutorRecord) -> bool {
        ExecutorProfile::accepted(self.session.chain_id, record.delegate())
            .is_some_and(|profile| swap_setup_recorded_executed(record, profile))
    }

    /// A new submission must not borrow the previous order's outcome. Once its own order is
    /// recorded, that record becomes the source of progress even if submission is still running.
    fn pending_order(&self, record: &ExecutorRecord) -> Option<PendingSwapOrder> {
        self.tracking
            .get(&record.operation())?
            .pending_order
            .filter(|pending| {
                pending.previous_order
                    == record
                        .swap()
                        .and_then(|swap| swap.orders().last())
                        .map(SwapOrderRecord::uid)
            })
    }

    /// Presentation only; signing and observation still use the durable account stage.
    fn progress_stage(&self, record: &ExecutorRecord) -> SwapStage {
        if self.pending_order(record).is_some() {
            SwapStage::Ready
        } else {
            self.stage(record)
        }
    }

    const fn busy(&self) -> bool {
        self.job.is_some() || self.pending_authorization.is_some()
    }

    /// The confirmed block of the session's last synced head.
    fn confirmed_block(&self, cx: &gpui::App) -> Option<u64> {
        self.root
            .upgrade()?
            .read(cx)
            .confirmed_block(self.session.chain_id)
    }

    /// A token's symbol, decimals and icon. The native asset, which a Public address swap can
    /// buy as `Address::ZERO`, isn't in the token registry, so it comes from the chain.
    fn token_metadata(
        &self,
        token: Address,
        cx: &gpui::App,
    ) -> Option<super::tokens::TokenDisplayMetadata> {
        self.chain_token_metadata(self.session.chain_id, token, cx)
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
                    self.session.chain_id,
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
                (pending.sell, pending.buy),
                Some(pending.amount),
                None,
                None,
                cx,
            );
            let bridge = self.bridge_labels(pending.delivery, pending.buy, None, None, cx);
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
                bridge: None,
            };
        };
        let amount = swap_sell_amount(record).or_else(|| {
            self.tracking
                .get(&record.operation())
                .and_then(|tracking| tracking.amount)
        });
        let order = record.swap().and_then(|swap| swap.orders().last());
        let labels = self.order_labels((sell, buy), amount, order, self.fill_hint(record, cx), cx);
        if order.is_some() {
            return labels;
        }
        // Before its first order, the swap delivers as approved with its setup.
        let delivery = swap_delivery(record);
        let destination_minimum = record
            .swap_approval()
            .and_then(|approval| approval.bounds.destination_minimum);
        let bridge = self.bridge_labels(delivery, buy, None, destination_minimum, cx);
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
        model::swap_setup_confirmation(record, &self.session.observation_rx.borrow().snapshot.utxos)
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
            .get(self.session.chain_id)?
            .finality_depth;
        let tip = *self.session.sync_tip_rx.borrow();
        self.setup_confirmation(record)?
            .detail(super::utxo::UtxoFinalityContext::new(
                tip.head_block,
                tip.safe_head_block,
                Some(depth),
            ))
    }

    /// Display strings for one swap: `amount` of `sell` for `buy`, with `order` its latest
    /// order when it has one.
    fn order_labels(
        &self,
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
                        SwapBridgeOutcome::DeliveredVerified { output_amount, .. } => {
                            Some(output_amount)
                        }
                        SwapBridgeOutcome::DeliveredReported { amount_out, .. } => amount_out,
                        SwapBridgeOutcome::Refunding | SwapBridgeOutcome::NeedsAttention => None,
                    }?;
                    return Some(self.network_token_amount(
                        bridge.destination_chain,
                        self.bridge_received_token(bridge, cx),
                        amount,
                        cx,
                    ));
                }
            };
            amount.map(|amount| self.token_amount(buy, amount, cx))
        });
        let valid_to = order.map(|order| u64::from(order.valid_to()));
        let bridge = order
            .and_then(|order| self.bridge_labels(order.delivery(), buy, Some(order), None, cx));
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
    /// order exists, `destination_minimum` is the one approved with the setup. `None` for
    /// delivery on this network.
    fn bridge_labels(
        &self,
        delivery: SwapDelivery,
        buy: Address,
        order: Option<&SwapOrderRecord>,
        destination_minimum: Option<U256>,
        cx: &gpui::App,
    ) -> Option<SwapBridgeLabels> {
        let SwapDelivery::Bridge(bridge) = delivery else {
            return None;
        };
        Some(SwapBridgeLabels {
            provider: bridge.provider,
            network: form::network_name(bridge.destination_chain),
            token: self.network_token_symbol(
                bridge.destination_chain,
                self.bridge_received_token(bridge, cx),
                cx,
            ),
            origin: form::network_name(self.session.chain_id),
            receiver: self
                .receiver_label(bridge.receiver, cx)
                .map_or_else(|| short_receiver(bridge.receiver), |(label, _)| label),
            sent: order
                .and_then(bridge_sent_amount)
                .map(|amount| self.token_amount(buy, amount, cx)),
            minimum: order
                .and_then(|order| order.bounds().destination_minimum)
                .or(destination_minimum)
                .map(|minimum| {
                    self.network_token_amount(
                        bridge.destination_chain,
                        self.bridge_received_token(bridge, cx),
                        minimum,
                        cx,
                    )
                }),
        })
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
            .get(self.session.chain_id)?
            .finality_depth;
        let head = root
            .chain_states
            .get(&self.session.chain_id)
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
        if !self.session_is_current(cx) {
            return None;
        }
        let confirmed = self.confirmed_block(cx)?;
        let finality_depth = self.root.upgrade().and_then(|root| {
            root.read(cx)
                .effective_chain_configs
                .get(self.session.chain_id)
                .map(|chain| chain.finality_depth)
        });
        let busy = self.job.as_ref().map(|job| job.operation);
        let pages = self
            .records
            .iter()
            .filter(|record| Some(record.operation()) != busy)
            // A stopped setup never places its order, so nothing waits on its observation. Its
            // notes stay reserved until the user checks or releases them as locked notes.
            .filter(|record| !record.is_swap_setup_stopped())
            .filter(|record| !self.setup_observation_deferred(record, confirmed, finality_depth))
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
                        .or_else(|| self.owner.synced_settlement_block(record, order.uid()))?;
                    return (block <= confirmed).then_some(ObservationPage {
                        operation: record.operation(),
                        setup: false,
                        settlement: Some((order.uid(), block)),
                        // Settlement checks do not page through account history or catch up.
                        range: confirmed..confirmed.saturating_add(1),
                    });
                }
                // A setup recorded as executed for the accepted delegate is already ready and
                // not observed, so this only reaches a delegate the check will refuse.
                let needs_delegation = record.swap().is_none()
                    && tracking.is_none_or(|tracking| tracking.setup.is_none())
                    && record.issued().iter().any(|payload| {
                        payload.purpose() == wallet_ops::vault::ExecutorPayloadPurpose::Operation
                            && record.recorded_payload_status(payload.hash())
                                == Some(wallet_ops::vault::ExecutorPayloadStatus::Executed)
                    });
                let cursor = if needs_delegation {
                    confirmed
                } else {
                    tracking
                        .and_then(|tracking| tracking.cursor)
                        .or_else(|| swap_history_start(record))?
                };
                Some(ObservationPage {
                    operation: record.operation(),
                    setup: record.swap().is_none(),
                    settlement: None,
                    range: swap_observation_range(cursor, confirmed),
                })
            })
            .collect::<Vec<_>>();
        (!pages.is_empty()).then(|| (self.runtime.clone(), Arc::clone(&self.owner), pages))
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
        if !self.session_is_current(cx) {
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
        (!requests.is_empty()).then(|| (self.runtime.clone(), Arc::clone(&self.owner), requests))
    }

    fn apply_order_hints(&mut self, results: Vec<HintResult>, cx: &mut Context<'_, Self>) {
        if !self.session_is_current(cx) {
            return;
        }
        let now = Instant::now();
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

    /// Private sync locates a handed-off setup, and confirmation observation records its
    /// inclusion from that one block, so reading the account every pass until then repeats
    /// the last answer: a located setup is read only [`DEFERRED_SETUP_OBSERVATION_INTERVAL`]
    /// after its hand-off in this session or the last caught-up read, whichever is later. A
    /// setup sent long ago without that location is checked at the same pace from its last
    /// read. An inclusion of the latest attempt, or a recorded executed setup, is handled at
    /// once; an earlier attempt's effect-less inclusion leaves a retry at its own pace.
    ///
    /// A setup's history start is at most its signing head less `finality_depth`, so while
    /// the depth is unchanged a confirmed block before their sum cannot contain it. A located
    /// setup isn't read before then. An unlocated one keeps its pace, since these reads are
    /// its only confirmation path and a raised depth would delay it.
    fn setup_observation_deferred(
        &self,
        record: &ExecutorRecord,
        confirmed: u64,
        finality_depth: Option<u64>,
    ) -> bool {
        if record.swap().is_some() {
            return false;
        }
        // The latest setup attempt decides, so a retry is observed at the normal pace until
        // it is handed off.
        let Some(latest) = latest_setup(record) else {
            return false;
        };
        if setup_included(latest) || self.setup_recorded_executed(record) {
            return false;
        }
        let history_start = latest.context().history_start();
        let located = !latest.transaction_hashes().is_empty();
        if located
            && finality_depth.is_some_and(|depth| confirmed < history_start.saturating_add(depth))
        {
            return true;
        }
        let stale = confirmed.saturating_sub(history_start) > STALE_SETUP_BLOCKS;
        let tracking = self.tracking.get(&record.operation());
        let read_at = tracking.and_then(|tracking| tracking.setup_read_at);
        let last = if located {
            read_at.max(
                tracking
                    .and_then(|tracking| tracking.located_at)
                    .filter(|(hash, _)| *hash == latest.hash())
                    .map(|(_, at)| at),
            )
        } else {
            read_at
        };
        (located || stale)
            && last.is_some_and(|at| at.elapsed() < DEFERRED_SETUP_OBSERVATION_INTERVAL)
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
            let tracking = self.tracking.entry(result.operation).or_default();
            match result.outcome {
                Ok(setup) => {
                    tracking.cursor = Some(result.range_end);
                    tracking.error = None;
                    if confirmed.is_some_and(|block| result.range_end <= block) {
                        catchup.push(result.operation);
                    } else {
                        tracking.setup_read_at = Some(Instant::now());
                    }
                    if setup.is_some() {
                        tracking.setup = setup;
                    }
                }
                Err(error) => tracking.error = Some(error),
            }
        }
        self.reload_records();
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

    /// Place the order of a swap approved with its setup in this session once the setup is
    /// confirmed. After a restart, or once the user declined, its detail offers the step.
    /// Only this swap's detail may advance while a dialog is open.
    fn continue_approved_swaps(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.busy() || self.form.is_some() || !self.session_is_current(cx) {
            return;
        }
        let next = self
            .records
            .iter()
            .filter(|record| {
                !record.is_swap_setup_stopped() && self.stage(record) == SwapStage::Approved
            })
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
                self.session.chain_id,
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
        let command = Arc::new(SwapAuthorization {
            session: Arc::clone(&self.session),
            action,
        });
        self.pending_authorization = Some(Arc::clone(&command));
        let view = cx.entity();
        // Reviews require explicit approval; the confirm-only step for an approved order
        // accepts a remembered spend authorization.
        let _ = self.root.update(cx, |root, cx| {
            if root.stealth_session_is_current(&command.session) {
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

    pub(super) fn continue_authorized(
        &mut self,
        command: &Arc<SwapAuthorization>,
        authorization: DesktopPrivateSpendAuthorization,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .pending_authorization
            .take()
            .is_some_and(|current| Arc::ptr_eq(&current, command))
            || !self.session_is_current(cx)
        {
            return;
        }
        match command.action.clone() {
            SwapAction::Setup(approval) => {
                self.submit_setup(*approval, authorization, window, cx);
            }
            SwapAction::Order(approval) => {
                self.submit_order(*approval, authorization, window, cx);
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

    /// Candidates and policy for paying a broadcaster in `fee_token` for this swap's executor.
    fn broadcaster_candidates(
        &self,
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
            .get(self.session.chain_id)
            .and_then(wallet_ops::settings::EffectiveChainConfig::accepted_executor_profile)
        else {
            return Vec::new();
        };
        let policy = root.public_broadcaster_fee_policy(allow_out_of_range);
        wallet_ops::fee_policy_eligible_public_broadcasters(
            &super::public_broadcaster::public_broadcaster_candidates_for_route(
                &root.monitor_fee_rows(),
                self.session.chain_id,
                fee_token,
                None,
                Some(profile),
                policy,
                root.public_broadcaster_anchor_cache
                    .cached_rate(self.session.chain_id, fee_token),
                &root.public_broadcaster_trust_filter(favorites_only),
            ),
            policy,
        )
    }
}

/// Setup payloads with a recorded inclusion, which wake setup observation.
fn included_setups(record: &ExecutorRecord) -> impl Iterator<Item = &IssuedExecutorPayload> {
    record.issued().iter().filter(|payload| {
        payload.purpose() == wallet_ops::vault::ExecutorPayloadPurpose::Operation
            && setup_included(payload)
    })
}

/// Whether a setup payload has a recorded executed or effect-less inclusion.
fn setup_included(payload: &IssuedExecutorPayload) -> bool {
    payload.inclusion().is_some_and(|inclusion| {
        matches!(
            inclusion.result(),
            wallet_ops::vault::ExecutorExecutionResult::Executed
                | wallet_ops::vault::ExecutorExecutionResult::MissingEffects
        )
    })
}

/// The latest setup attempt of a record: the one with the latest history start.
fn latest_setup(record: &ExecutorRecord) -> Option<&IssuedExecutorPayload> {
    record
        .issued()
        .iter()
        .filter(|payload| payload.purpose() == wallet_ops::vault::ExecutorPayloadPurpose::Operation)
        .max_by_key(|payload| payload.context().history_start())
}

/// The latest setup attempt of a swap without orders, once its transaction is recorded.
fn located_setup(record: &ExecutorRecord) -> Option<B256> {
    if record.swap().is_some() {
        return None;
    }
    latest_setup(record)
        .filter(|payload| !payload.transaction_hashes().is_empty())
        .map(IssuedExecutorPayload::hash)
}

struct ObservationPage {
    operation: ExecutorOperationId,
    /// No order exists yet, so the setup is what can change.
    setup: bool,
    /// An orderbook hint or a previously verified trade locates a whole-block receipt read.
    settlement: Option<(OrderUid, u64)>,
    range: std::ops::Range<u64>,
}

struct ObservationResult {
    operation: ExecutorOperationId,
    range_end: u64,
    outcome: Result<Option<SwapSetupStatus>, String>,
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

/// Setups and user-requested cancellation checks reconcile their account. Routine order
/// confirmation reads only the settlement block, matching its receipts locally.
async fn observe_pages(
    owner: Arc<ExecutorOwner>,
    pages: Vec<ObservationPage>,
) -> Vec<ObservationResult> {
    let mut results = Vec::with_capacity(pages.len());
    for page in pages {
        let outcome = if let Some((uid, block)) = page.settlement {
            Box::pin(owner.observe_swap_settlement(page.operation, uid, block))
                .await
                .map(|()| None)
        } else if page.setup {
            Box::pin(owner.observe_swap_setup(page.operation, page.range.clone()))
                .await
                .map(Some)
        } else {
            Box::pin(owner.observe_swap(page.operation, page.range.clone()))
                .await
                .map(|_| None)
        };
        results.push(ObservationResult {
            operation: page.operation,
            range_end: page.range.end,
            outcome: outcome.map_err(|error| format!("{error:#}")),
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
