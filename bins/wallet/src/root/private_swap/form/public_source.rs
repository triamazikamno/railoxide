//! Public-paid swaps. The origin is public; every private capability and persisted use belongs
//! to the independently selected destination network.

use super::*;
use crate::root::private_swap::public_progress::PublicSwapAction;
use crate::root::private_swap::ui_fixture;
use crate::root::private_swap::{local_date_time_label, local_time_label, now_unix};
use crate::root::spend_authorization::SpendAuthorizationIntent;
use crate::root::ui_helpers::dialog_footer;
use crate::root::vault::hardware_device_label;
use alloy::primitives::B256;
use wallet_ops::{
    AcrossPrivateDelivery, AuthorizedPublicSwapSource, PublicActionGasFeeSelection,
    PublicSwapBatchTerms, PublicSwapDelivery, PublicSwapDeliverySigning, PublicSwapOrderOutcome,
    PublicSwapOrderRequest, PublicSwapReview, PublicSwapReviewRequest, PublicSwapSource,
    PublicSwapTracking, PublicSwapUseClaim, PublicSwapWithdrawalReview, SwapSetupStatus,
    bridge::{
        AcrossClient, AcrossRoute, PublicBridgeDestination, PublicSellAsset,
        public_across_destination_tokens,
    },
    cow::OrderLimit,
    new_public_swap_batch_nonce, quote_public_action_gas_fee,
    vault::{
        AcrossOrderTerms, ExecutorStoreError, PublicAccountMetadata, PublicAccountScope,
        PublicAccountSource, PublicAccountStatus, PublicSwapApproval, PublicSwapSourceTerms,
        SwapUseRole,
    },
};

/// The hardware review's title: the step whose signatures it decodes.
const DEVICE_SIGNATURES_TITLE: &str = "Approve and place order";

#[derive(Clone)]
struct PublicSwapClients {
    across: AcrossClient,
    orderbook: Option<CowOrderbookClient>,
}

pub(super) struct PublicSwapForm {
    pub(super) account: PublicAccountMetadata,
    pub(super) routes: HashMap<(Address, u64), Vec<PublicBridgeDestination>>,
    pub(super) route_errors: HashMap<(Address, u64), String>,
    pub(super) route_tasks: HashMap<(Address, u64), Task<()>>,
    /// What Across answered for each entry of `routes`, with the sell asset it was asked for:
    /// what `routes` is listed from again once the chain is known to take orders.
    across_routes: HashMap<(Address, u64), (PublicSellAsset, Vec<AcrossRoute>)>,
    /// Whether the account's chain takes orders, read once for this form: its math contract is
    /// deployed there. `None` until the read answers, which lists what a deposit serves, as a
    /// failed read does.
    pub(super) orders_available: Option<bool>,
    orders_task: Option<Task<()>>,
    pub(super) review: Option<Arc<PublicSwapReview>>,
    pub(super) operation: Option<ExecutorOperationId>,
    pub(super) swap_use: Option<SwapUseId>,
    clients: Option<PublicSwapClients>,
    quote_terms: Option<PublicQuoteTerms>,
    /// The open swap that keeps this draft's pair from being reviewed: what the destination
    /// owner's source check names, or what a claim was refused for.
    conflict: Option<PublicSwapConflict>,
    /// The form's error that names the account's `CoW` proxy, and the proxy, to copy while that
    /// error shows.
    proxy_holds: Option<(String, Address)>,
    /// What changed between the last review and its signing, which the next review says.
    review_change: Option<SwapReviewChange>,
}

/// What the gas strip shows of an order paid from a Public account: its limit at the form's
/// gas share, priced from the reviewed quote until the bridge leg is previewed again.
#[derive(Clone, Copy)]
pub(super) struct PublicStrip {
    /// The order's limit at the shown share, in the bought token on the network it is paid on.
    pub(super) limit: OrderLimit,
    /// The shown share: the form's, or the review's when the quote can't support the form's.
    pub(super) share_bps: u16,
    /// The share the review's bridge leg was previewed at.
    reviewed_bps: u16,
    /// From the order's bought token to the destination token, at the review's ratio of the
    /// minimum received there to the order's buy amount.
    pub(super) scale: StripScale,
    pub(super) bar: GasBar,
}

impl PublicStrip {
    /// The strip shows a share the bridge leg wasn't previewed at.
    const fn pending(&self) -> bool {
        self.share_bps != self.reviewed_bps
    }
}

/// The gas strip of the form's order, once a Public account's swap has a review. `None` for a
/// direct deposit, which has no order.
pub(super) fn public_strip(form: &SwapForm) -> Option<PublicStrip> {
    let review = form.public.as_ref()?.review.as_ref()?;
    let reviewed_bps = review.gas_share_bps()?;
    let reviewed = review.order_limit_at(reviewed_bps).ok()?;
    // A share the quote can't support shows the quote as it was priced.
    let (limit, share_bps) = review
        .order_limit_at(form.gas_share_bps)
        .map_or((reviewed, reviewed_bps), |limit| {
            (limit, form.gas_share_bps)
        });
    let scale = if reviewed.min_received.is_zero() {
        StripScale {
            shown: U256::ONE,
            ordered: U256::ONE,
        }
    } else {
        StripScale {
            shown: review.bridge().received_minimum(),
            ordered: reviewed.min_received,
        }
    };
    Some(PublicStrip {
        limit,
        share_bps,
        reviewed_bps,
        scale,
        bar: GasBar::new(limit.best_case, review.slippage_bps(), limit.gas_estimate),
    })
}

/// Another swap paid from the same Public account still trades one of the draft's tokens.
#[derive(Clone, Copy)]
struct PublicSwapConflict {
    /// The draft's pair. The refusal stands until either changes.
    sell: Address,
    buy: Option<Address>,
    /// The open swap.
    swap: SwapUseId,
    /// The token both swaps trade on the network they are paid on.
    token: Address,
    /// The open swap buys the token; otherwise its order sells it.
    buys: bool,
    /// Unix seconds: when the token can be bought again, or when the open order expires.
    until: Option<u64>,
}

/// Why a swap paid from a Public account can't be quoted or reviewed, under the Sell card.
pub(super) struct PublicFormReason {
    pub(super) text: String,
    /// Review stays unavailable. An informational reason leaves the form working.
    pub(super) blocks_review: bool,
    /// The open swap the reason names, for "Open it…".
    open: Option<super::super::model::SwapIdentity>,
}

/// One row of the details section.
pub(super) struct PublicDetail {
    pub(super) label: String,
    value: PublicDetailValue,
    /// A cost taken from what the swap delivers or from the Public account.
    indented: bool,
    help: Option<String>,
}

enum PublicDetailValue {
    /// A value, and a muted note after it.
    Text {
        value: String,
        note: Option<String>,
    },
    Price(PriceDelta),
    /// The price tolerance select.
    Tolerance,
}

impl PublicDetail {
    fn cost(label: impl Into<String>, value: String, note: Option<String>, help: String) -> Self {
        Self {
            label: label.into(),
            value: PublicDetailValue::Text { value, note },
            indented: true,
            help: Some(help),
        }
    }
}

/// One row of a group the hardware review decodes: its label, its value, and the address the
/// value names, shortened.
pub(super) struct PublicSignatureRow {
    pub(super) label: String,
    pub(super) value: String,
    pub(super) address: Option<String>,
}

impl PublicSignatureRow {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            address: None,
        }
    }

    fn naming(mut self, address: Address) -> Self {
        self.address = Some(railgun_ui::short_address(&address));
        self
    }
}

impl PublicSwapForm {
    pub(super) fn invalidate_quote_route(&mut self) {
        self.clients = None;
        self.review = None;
        self.quote_terms = None;
    }

    /// Whether the account's chain was read to take orders. Not read yet, and a failed read,
    /// are both no.
    const fn takes_orders(&self) -> bool {
        matches!(self.orders_available, Some(true))
    }

    pub(super) fn new(account: PublicAccountMetadata) -> Self {
        Self {
            account,
            routes: HashMap::new(),
            route_errors: HashMap::new(),
            route_tasks: HashMap::new(),
            across_routes: HashMap::new(),
            orders_available: None,
            orders_task: None,
            review: None,
            operation: None,
            swap_use: None,
            clients: None,
            quote_terms: None,
            conflict: None,
            proxy_holds: None,
            review_change: None,
        }
    }
}

#[derive(Clone)]
struct PublicQuoteTerms {
    source: PublicAccountMetadata,
    origin: EffectiveChainConfig,
    network: u64,
    sell: Address,
    buy: Address,
    amount: U256,
    slippage_bps: u32,
    gas_share_bps: u16,
    shield_failure: BridgeShieldFailure,
}

impl PublicQuoteTerms {
    fn matches(&self, form: &SwapForm, origin: &EffectiveChainConfig, amount: U256) -> bool {
        form.public
            .as_ref()
            .is_some_and(|public| public.account == self.source)
            && origin == &self.origin
            && form.network == Some(self.network)
            && form.sell == self.sell
            && form.buy == Some(self.buy)
            && amount == self.amount
            && form.slippage_bps == self.slippage_bps
            && form.gas_share_bps == self.gas_share_bps
            && form.bridge.shield_failure == self.shield_failure
    }
}

/// An immutable review, bound to the source metadata and the destination's actual session.
pub(in crate::root) struct PublicSwapAuthorization {
    source: PublicAccountMetadata,
    origin: EffectiveChainConfig,
    destination_session: Arc<WalletSession>,
    owner: Arc<ExecutorOwner>,
    operation: ExecutorOperationId,
    swap_use: SwapUseId,
    action: PublicSwapCommand,
    summary: SpendAuthorizationSummary,
    revision: u64,
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum PublicSwapCommand {
    Swap {
        account: SwapAccountChoice,
        destination_token: Address,
        candidate: Option<PublicBroadcasterCandidate>,
        review: Arc<PublicSwapReview>,
        approval: PublicSwapApproval,
        clients: PublicSwapClients,
        waku: Option<Arc<WakuDeliveryClient>>,
    },
    Withdraw {
        review: PublicSwapWithdrawalReview,
        fee: PublicActionGasFeeSelection,
    },
    Cancel {
        fee: PublicActionGasFeeSelection,
        gas_limit: u64,
        orderbook: CowOrderbookClient,
    },
}

impl PublicSwapAuthorization {
    const fn identity(&self) -> super::super::model::SwapIdentity {
        super::super::model::SwapIdentity {
            operation: self.operation,
            swap_use: self.swap_use,
        }
    }

    pub(in crate::root) const fn source(&self) -> &PublicAccountMetadata {
        &self.source
    }
    pub(in crate::root) fn public_authorization_summary(&self) -> SpendAuthorizationSummary {
        self.summary.clone()
    }
    pub(in crate::root) const fn hardware_executor_action(
        &self,
    ) -> wallet_ops::HardwareExecutorAction {
        wallet_ops::HardwareExecutorAction::Execute(self.operation)
    }
    #[cfg(feature = "hardware")]
    pub(in crate::root) fn hardware_executor_chain(&self) -> u64 {
        self.destination_session.chain_id
    }
    pub(in crate::root) fn sessions_are_current(&self, root: &WalletRoot) -> bool {
        root.view_session.is_some()
            && root
                .public_accounts
                .iter()
                .any(|account| account == &self.source)
            && matches!(root.chain_states.get(&self.destination_session.chain_id),
                Some(ChainUtxoState::Ready { session, .. })
                if Arc::ptr_eq(session, &self.destination_session))
            && root
                .effective_chain_configs
                .get(self.origin.chain_id)
                .is_some_and(|chain| chain == &self.origin)
    }
}

pub(in crate::root::private_swap) struct PublicSwapExecution {
    command: Arc<PublicSwapAuthorization>,
    private_authorization: Option<DesktopPrivateSpendAuthorization>,
    /// The next block of the destination setup's history to observe.
    setup_cursor: Option<u64>,
    signer: AuthorizedPublicSwapSource,
    signed: Option<PublicSwapSigned>,
}

struct PublicSwapSigned {
    delivery: AcrossPrivateDelivery,
    terms: AcrossOrderTerms,
    valid_to: u32,
    nonce: B256,
    batch: Option<PublicSwapBatchTerms>,
}

async fn public_swap_clients(
    origin: &EffectiveChainConfig,
    http: &wallet_ops::HttpContext,
) -> eyre::Result<PublicSwapClients> {
    let bridge = origin
        .bridge_origin_profile()
        .ok_or_else(|| eyre::eyre!("Swaps aren't available from this network."))?;
    let route = http.operation_http_client().await?;
    let across = AcrossClient::new(route.clone(), bridge.across_api_base().parse()?)?;
    let orderbook = match origin.public_swap_profile() {
        Some(profile) => Some(CowOrderbookClient::new(
            route,
            profile.orderbook_api_base().parse()?,
            origin.chain_id,
        )?),
        None => None,
    };
    Ok(PublicSwapClients { across, orderbook })
}

fn public_sell(origin: &EffectiveChainConfig, token: Address) -> eyre::Result<PublicSellAsset> {
    if token == Address::ZERO {
        Ok(PublicSellAsset::Native {
            wrapped: origin.wrapped_native_token.ok_or_else(|| {
                eyre::eyre!("This network has no configured wrapped native token.")
            })?,
        })
    } else {
        Ok(PublicSellAsset::Erc20(token))
    }
}

const fn public_gas_fee(review: &PublicSwapReview) -> PublicActionGasFeeSelection {
    PublicActionGasFeeSelection::Custom {
        max_fee_per_gas: review.gas_plan().max_fee_per_gas,
        max_priority_fee_per_gas: review.gas_plan().max_priority_fee_per_gas,
    }
}

impl PrivateSwapsView {
    fn public_origin(&self, cx: &App) -> Option<EffectiveChainConfig> {
        self.root
            .upgrade()?
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)
            .cloned()
    }

    pub(super) fn public_form_delivery(
        &self,
        form: &SwapForm,
        cx: &App,
    ) -> Result<SwapDelivery, DeliveryProblem> {
        // An untouched form has no network yet, which the Buy card's label prompts for. Only
        // a picked network that can't be used is a problem.
        let network = form.network.ok_or(DeliveryProblem::Bridge)?;
        if network == self.origin_chain_id {
            return Err(DeliveryProblem::Network {
                problem: "Choose another network for the private balance.".into(),
                syncing: false,
            });
        }
        let root = self.root.upgrade().ok_or(DeliveryProblem::Bridge)?;
        let root = root.read(cx);
        let chain = root
            .effective_chain_configs
            .get(network)
            .ok_or(DeliveryProblem::Bridge)?;
        if chain.bridge_profile().is_none() || chain.swap_profile().is_none() {
            return Err(DeliveryProblem::Network {
                problem: "Private bridge delivery isn't available on this network.".into(),
                syncing: false,
            });
        }
        let token = form
            .buy
            .filter(|token| *token != Address::ZERO)
            .ok_or(DeliveryProblem::Bridge)?;
        let public = form.public.as_ref().ok_or(DeliveryProblem::Bridge)?;
        if !public
            .routes
            .get(&(form.sell, network))
            .is_some_and(|routes| {
                routes
                    .iter()
                    .any(|route| route.destination.destination_token == token)
            })
        {
            return Err(DeliveryProblem::Bridge);
        }
        Ok(SwapDelivery::Bridge(BridgeDelivery {
            provider: BridgeProvider::Across,
            destination_chain: network,
            destination_token: token,
            receiver: form
                .destination_choice()
                .map_or(Address::ZERO, |account| account.address),
            surplus: BridgeSurplus::Reshield,
            private: Some(BridgePrivateDelivery {
                on_shield_failure: form.bridge.shield_failure,
            }),
        }))
    }

    /// Whether the form's Public account can place orders: its chain has a Public swap profile
    /// and was read to take orders.
    const fn public_can_swap(origin: &EffectiveChainConfig, public: &PublicSwapForm) -> bool {
        origin.public_swap_profile().is_some() && public.takes_orders()
    }

    /// List the form's routes from what Across answered: with the tokens an order reaches only
    /// where the account's chain takes orders, otherwise what a deposit serves.
    fn list_public_routes(&mut self, cx: &App) {
        let Some(origin) = self.public_origin(cx) else {
            return;
        };
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let Some(public) = self.form.as_mut().and_then(|form| form.public.as_mut()) else {
            return;
        };
        let can_swap = Self::public_can_swap(&origin, public);
        let registry = &root.read(cx).effective_token_registry;
        for (key, (sell, routes)) in &public.across_routes {
            public.routes.insert(
                *key,
                public_across_destination_tokens(routes, *sell, can_swap, registry, key.1),
            );
        }
    }

    /// Read whether the account's chain takes orders, once for this form; a reopened form reads
    /// it again. The read is of the account's own chain and binds no destination, so it goes
    /// through the owner of the form's network when that network is ready, otherwise through
    /// any ready network's. Until one is ready nothing is read.
    fn load_public_orders_available(&mut self, window: &Window, cx: &Context<'_, Self>) {
        let Some(origin) = self.public_origin(cx) else {
            return;
        };
        if origin.public_swap_profile().is_none() {
            return;
        }
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(public) = form.public.as_ref() else {
            return;
        };
        if public.orders_available.is_some() || public.orders_task.is_some() {
            return;
        }
        // The read is of public chain state, so a network still syncing serves it too.
        let ready = |state: &ChainUtxoState| match state {
            ChainUtxoState::Syncing { session, .. } | ChainUtxoState::Ready { session, .. } => {
                session.executor_owner()
            }
            _ => None,
        };
        let states = &root.read(cx).chain_states;
        let Some(owner) = form
            .network
            .and_then(|network| states.get(&network))
            .and_then(ready)
            .or_else(|| states.values().find_map(ready))
        else {
            return;
        };
        let runtime = self.runtime.clone();
        let task = cx.spawn_in(window, async move |view, cx| {
            let task =
                runtime.spawn(async move { owner.public_swap_orders_available(&origin).await });
            let _abort = AbortOnDrop(task.abort_handle());
            // A chain that couldn't be read stays unread, so the next quote reads it again.
            let available = match task.await {
                Ok(Ok(available)) => Some(available),
                _ => None,
            };
            let _ = view.update_in(cx, |view, window, cx| {
                let Some(public) = view.form.as_mut().and_then(|form| form.public.as_mut()) else {
                    return;
                };
                public.orders_task = None;
                public.orders_available = available;
                let reviewed = public.review.is_some();
                if available == Some(true) {
                    // The routes read so far listed only what a deposit serves.
                    view.list_public_routes(cx);
                    view.refresh_form_delivery(cx);
                    if !reviewed {
                        view.schedule_public_quote(window, cx);
                    }
                }
                cx.notify();
            });
        });
        if let Some(public) = self.form.as_mut().and_then(|form| form.public.as_mut()) {
            public.orders_task = Some(task);
        }
    }

    pub(super) fn load_public_network_routes(
        &mut self,
        network: u64,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        let Some(origin) = self.public_origin(cx) else {
            return;
        };
        self.load_public_orders_available(window, cx);
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let http = root.read(cx).http.clone();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let key = (form.sell, network);
        let Some(public) = form.public.as_mut() else {
            return;
        };
        if network == origin.chain_id
            || public.routes.contains_key(&key)
            || public.route_tasks.contains_key(&key)
        {
            return;
        }
        public.route_errors.remove(&key);
        let runtime = self.runtime.clone();
        public.route_tasks.insert(
            key,
            cx.spawn_in(window, async move |view, cx| {
                let task = runtime.spawn(async move {
                    let clients = public_swap_clients(&origin, &http).await?;
                    let routes = clients
                        .across
                        .available_routes(origin.chain_id, network)
                        .await?;
                    Ok::<_, eyre::Report>((clients, public_sell(&origin, key.0)?, routes))
                });
                let _abort = AbortOnDrop(task.abort_handle());
                let result = task.await;
                let _ = view.update_in(cx, |view, window, cx| {
                    let Some(form) = view.form.as_mut() else {
                        return;
                    };
                    let Some(public) = form.public.as_mut() else {
                        return;
                    };
                    public.route_tasks.remove(&key);
                    match result {
                        Ok(Ok((clients, sell, routes))) => {
                            public.clients = Some(clients);
                            public.across_routes.insert(key, (sell, routes));
                            view.list_public_routes(cx);
                        }
                        Ok(Err(error)) => {
                            public.route_errors.insert(key, format!("{error:#}"));
                        }
                        Err(_) => {
                            public
                                .route_errors
                                .insert(key, "The route request stopped. Try again.".into());
                        }
                    }
                    view.refresh_form_delivery(cx);
                    view.schedule_public_quote(window, cx);
                    cx.notify();
                });
            }),
        );
    }

    pub(super) fn schedule_public_quote(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        self.load_public_orders_available(window, cx);
        // A changed draft loses its old review even when the new route or destination is
        // unavailable. An unquotable pair must never retain approval of a different pair.
        let revision = {
            let Some(form) = self.form.as_mut() else {
                return;
            };
            let Some(public) = form.public.as_mut() else {
                return;
            };
            form.quote_revision = form.quote_revision.wrapping_add(1);
            form.quote_task = None;
            form.price_acknowledged = false;
            form.high_costs_acknowledged = false;
            form.quote = QuoteState::Idle;
            form.bridge_quote_error = None;
            public.review = None;
            public.quote_terms = None;
            form.quote_revision
        };
        // The draft's pair may have changed, and with it the open swap that blocks it.
        self.refresh_public_conflict(cx);
        cx.notify();
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(public) = form.public.as_ref() else {
            return;
        };
        let amount = self.form_amount(form, cx);
        let Some(network) = form.network else {
            return;
        };
        let Some(destination) = form
            .buy
            .and_then(|buy| {
                public
                    .routes
                    .get(&(form.sell, network))?
                    .iter()
                    .find(|route| route.destination.destination_token == buy)
            })
            .cloned()
        else {
            return;
        };
        let Some(owner) = self.public_quote_destination(network, cx) else {
            return;
        };
        let Some(origin) = self.public_origin(cx) else {
            return;
        };
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let (http, registry, anchors) = {
            let root = root.read(cx);
            (
                root.http.clone(),
                root.effective_token_registry.clone(),
                root.public_broadcaster_anchor_cache.clone(),
            )
        };
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let Some(public) = form.public.as_mut() else {
            return;
        };
        public.review = None;
        let Ok(amount) = amount else {
            form.quote = QuoteState::Idle;
            cx.notify();
            return;
        };
        let (source, sell, slippage, gas_share, failure) = (
            public.account.address,
            form.sell,
            form.slippage_bps,
            form.gas_share_bps,
            form.bridge.shield_failure,
        );
        let clients = public.clients.clone();
        let quoted = PublicQuoteTerms {
            source: public.account.clone(),
            origin: origin.clone(),
            network,
            sell,
            buy: destination.destination.destination_token,
            amount,
            slippage_bps: slippage,
            gas_share_bps: gas_share,
            shield_failure: failure,
        };
        form.quote = QuoteState::Loading;
        let runtime = self.runtime.clone();
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let task = runtime.spawn(async move {
                let clients = match clients {
                    Some(clients) => clients,
                    None => public_swap_clients(&origin, &http).await?,
                };
                let fee = quote_public_action_gas_fee(origin.chain_id, &origin, &http).await?;
                let review = owner
                    .review_public_swap(PublicSwapReviewRequest {
                        origin: &origin,
                        source,
                        sell: public_sell(&origin, sell)?,
                        sell_amount: amount,
                        destination: &destination,
                        slippage_bps: slippage,
                        gas_share_bps: gas_share,
                        on_shield_failure: failure,
                        orderbook: clients.orderbook.as_ref(),
                        across: &clients.across,
                        anchor_cache: Some(&anchors),
                        token_registry: &registry,
                        max_fee_per_gas: fee.suggested_max_fee_per_gas,
                        max_priority_fee_per_gas: fee.suggested_max_priority_fee_per_gas,
                    })
                    .await?;
                Ok::<_, eyre::Report>((clients, review, quoted))
            });
            let _abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                let Some(form) = view
                    .form
                    .as_mut()
                    .filter(|form| form.quote_revision == revision)
                else {
                    return;
                };
                let Some(public) = form
                    .public
                    .as_mut()
                    .filter(|public| public.account.address == source)
                else {
                    return;
                };
                match result {
                    Ok(Ok((clients, review, terms))) => {
                        if public.account != terms.source {
                            return;
                        }
                        public.clients = Some(clients);
                        public.review = Some(Arc::new(review));
                        public.quote_terms = Some(terms);
                        form.quote = QuoteState::Idle;
                        form.error = None;
                    }
                    Ok(Err(error)) => form.quote = QuoteState::Failed(error),
                    Err(_) => {
                        form.quote =
                            QuoteState::Failed(eyre::eyre!("The quote stopped. Try again."));
                    }
                }
                form.quote_task = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The form's gas share changed. A reviewed order is priced at it locally and its bridge
    /// leg previewed again, without asking the orderbook. Before a review, the share prices
    /// the next quote.
    pub(super) fn public_gas_share_changed(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let reviewed = self
            .form
            .as_ref()
            .and_then(|form| form.public.as_ref())
            .is_some_and(|public| public.review.is_some());
        if reviewed {
            self.schedule_public_gas_share(window, cx);
        } else {
            self.schedule_public_quote(window, cx);
        }
    }

    /// Preview the bridge leg again for the share the gas strip shows, after the quote
    /// debounce. The review stays in view, and the orderbook isn't asked. A share back at the
    /// reviewed one has nothing to refresh, which also ends a refresh still under way.
    fn schedule_public_gas_share(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(form) = self.form.as_ref() else {
            return;
        };
        let Some(public) = form.public.as_ref() else {
            return;
        };
        let (Some(review), Some(strip)) = (public.review.clone(), public_strip(form)) else {
            return;
        };
        let gas_share_bps = strip.share_bps;
        let pending = strip.pending();
        let clients = public.clients.clone();
        let owner = form
            .network
            .and_then(|network| self.public_quote_destination(network, cx));
        let registries = self.root.upgrade().map(|root| {
            let root = root.read(cx);
            (
                root.public_broadcaster_anchor_cache.clone(),
                root.effective_token_registry.clone(),
            )
        });
        let runtime = self.runtime.clone();
        let Some(form) = self.form.as_mut() else {
            return;
        };
        // A reviewed order's task is an earlier refresh. Dropping it stops its request.
        form.quote_task = None;
        form.quote_revision = form.quote_revision.wrapping_add(1);
        form.bridge_quote_error = None;
        if !pending {
            cx.notify();
            return;
        }
        form.price_acknowledged = false;
        form.high_costs_acknowledged = false;
        let (Some(clients), Some(owner), Some((anchors, registry))) = (clients, owner, registries)
        else {
            form.bridge_quote_error =
                Some("The swap's bridge route isn't ready. Retry the quote.".into());
            cx.notify();
            return;
        };
        let revision = form.quote_revision;
        form.quote_task = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(QUOTE_DEBOUNCE).await;
            let quoted = Arc::clone(&review);
            let work = runtime.spawn(async move {
                Box::pin(owner.requote_public_swap_gas_share(
                    &quoted,
                    gas_share_bps,
                    &clients.across,
                    Some(&anchors),
                    &registry,
                ))
                .await
            });
            // Dropping a Tokio handle detaches its task, so a superseded refresh aborts it.
            let _abort = AbortOnDrop(work.abort_handle());
            let result = work.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.apply_public_gas_share(
                    revision,
                    &review,
                    gas_share_bps,
                    result.ok(),
                    window,
                    cx,
                );
            });
        }));
        cx.notify();
    }

    /// Install a gas share refresh's reply, if the form still shows the review and the share
    /// it was asked for. A failure keeps the review and the share, and says why.
    fn apply_public_gas_share(
        &mut self,
        revision: u64,
        quoted: &Arc<PublicSwapReview>,
        gas_share_bps: u16,
        result: Option<eyre::Result<PublicSwapReview>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self
            .form
            .as_mut()
            .filter(|form| form.quote_revision == revision)
        else {
            return;
        };
        form.quote_task = None;
        let Some(public) = form.public.as_mut().filter(|public| {
            public
                .review
                .as_ref()
                .is_some_and(|review| Arc::ptr_eq(review, quoted))
        }) else {
            cx.notify();
            return;
        };
        match result {
            Some(Ok(review)) => {
                // The destination terms changed, so consent to the old ones doesn't carry over.
                form.price_acknowledged = false;
                form.high_costs_acknowledged = false;
                if let Some(terms) = public.quote_terms.as_mut() {
                    terms.gas_share_bps = gas_share_bps;
                }
                public.review = Some(Arc::new(review));
            }
            Some(Err(error)) => {
                form.bridge_quote_error = Some(
                    bridge_unreachable(&error)
                        .unwrap_or_else(|| format!("{error:#}"))
                        .into(),
                );
            }
            None => {
                form.bridge_quote_error =
                    Some("Updating the bridge quote stopped unexpectedly. Try again.".into());
            }
        }
        self.sync_gas_controls(window, cx);
        cx.notify();
    }

    /// The open swap of the form's Public account that already trades one of the draft's
    /// tokens, as the destination network's owner reads it from the wallet's records. Known
    /// once the form has a source, a Sell token and a routed Buy token, before Review.
    fn public_source_conflict(&self, cx: &App) -> Option<PublicSwapConflict> {
        let form = self.form.as_ref()?;
        let public = form.public.as_ref()?;
        let route = public_route(form)?;
        let (_, owner) = self.destination_owner(form.network?, cx)?;
        let bridged = route.destination.intermediate;
        let refusal = owner
            .public_swap_source_conflict(&PublicSwapSourceTerms {
                // A swap the form continues doesn't compete with itself.
                id: public.swap_use,
                origin_chain: self.origin_chain_id,
                source: public.account.address,
                source_scope: public.account.scope.clone(),
                sell_token: form.sell,
                bridged_token: bridged,
                order: route.path == PublicBridgePath::Order,
                now: now_unix(),
            })
            .ok()??;
        PublicSwapConflict::naming(&refusal, form, form.sell, bridged)
    }

    /// Read again which open swap, if any, blocks the draft's pair.
    fn refresh_public_conflict(&mut self, cx: &App) {
        let conflict = self.public_source_conflict(cx);
        if let Some(public) = self.form.as_mut().and_then(|form| form.public.as_mut()) {
            public.conflict = conflict;
        }
    }

    /// Take the account's chain as one that takes orders or not, in place of the form's read.
    #[cfg(test)]
    pub(super) fn stub_public_orders_for_tests(&mut self, available: bool, cx: &App) {
        let form = self.form.as_mut().expect("test form");
        let public = form.public.as_mut().expect("Public test form");
        public.orders_task = None;
        public.orders_available = Some(available);
        self.list_public_routes(cx);
    }

    #[cfg(test)]
    pub(super) fn install_public_review_for_tests(
        &mut self,
        review: PublicSwapReview,
        across: AcrossClient,
        orderbook: Option<CowOrderbookClient>,
        cx: &App,
    ) {
        // A reviewed swap is on a chain whose orders were read as available.
        self.stub_public_orders_for_tests(true, cx);
        let origin = self.public_origin(cx).expect("test origin");
        let form = self.form.as_ref().expect("test form");
        let amount = self.form_amount(form, cx).expect("test amount");
        let terms = PublicQuoteTerms {
            source: form
                .public
                .as_ref()
                .expect("Public test form")
                .account
                .clone(),
            origin,
            network: form.network.expect("test destination"),
            sell: form.sell,
            buy: form.buy.expect("test token"),
            amount,
            slippage_bps: form.slippage_bps,
            gas_share_bps: form.gas_share_bps,
            shield_failure: form.bridge.shield_failure,
        };
        let form = self.form.as_mut().expect("test form");
        form.quote_task = None;
        form.quote = QuoteState::Idle;
        let public = form.public.as_mut().expect("Public test form");
        public.review = Some(Arc::new(review));
        public.quote_terms = Some(terms);
        public.clients = Some(PublicSwapClients { across, orderbook });
    }

    #[cfg(test)]
    pub(super) fn hold_public_preparation_for_tests(
        &mut self,
        command: Arc<PublicSwapAuthorization>,
        gate: tokio::sync::oneshot::Receiver<()>,
        completed: Arc<std::sync::atomic::AtomicUsize>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> eyre::Result<super::super::model::SwapIdentity> {
        let PublicSwapCommand::Swap {
            account,
            destination_token,
            review,
            approval,
            ..
        } = &command.action
        else {
            return Err(eyre::eyre!("Expected a preparation command."));
        };
        command.owner.claim_public_swap(PublicSwapUseClaim {
            id: command.swap_use,
            origin_chain: command.origin.chain_id,
            source: command.source.address,
            source_scope: command.source.scope.clone(),
            account: *account,
            destination_token: *destination_token,
            intent: review.intent(),
            approval: approval.clone(),
        })?;
        let public = self
            .form
            .as_mut()
            .and_then(|form| form.public.as_mut())
            .expect("Public test form");
        public.operation = Some(command.operation);
        public.swap_use = Some(command.swap_use);
        self.public_authorization = None;
        let identity = super::super::model::SwapIdentity {
            operation: command.operation,
            swap_use: command.swap_use,
        };
        self.start_public_job(
            command,
            async move {
                gate.await?;
                completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(PublicExecutionResult::Finished(identity))
            },
            window,
            cx,
        );
        Ok(identity)
    }

    pub(super) fn request_public_review(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.busy() || !self.session_is_current(cx) {
            return;
        }
        self.refresh_setup_route(cx);
        let waku = self.broadcaster_network(cx);
        let result = self.public_review_command(waku, cx);
        let command = match result {
            Ok(command) => Arc::new(command),
            Err(error) => {
                if let Some(form) = self.form.as_mut() {
                    form.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        self.public_authorization = Some(command.clone());
        let view = cx.entity();
        let summary = command.summary.clone();
        let _ = self.root.update(cx, |root, cx| {
            root.request_spend_authorization(
                SpendAuthorizationIntent::PublicSwap(view, command),
                summary,
                window,
                cx,
            );
        });
        cx.notify();
    }

    /// The setup the debug UI fixture stands in for a new destination account that has no
    /// estimate: a fee in the token delivered there. `None` unless the fixture is on.
    fn ui_fixture_setup(&self, form: &SwapForm, cx: &App) -> Option<SetupFee> {
        if !ui_fixture::active() || form.destination_route.estimate.is_some() {
            return None;
        }
        let chain_id = self.destination_setup_chain(form)?;
        let token = form.buy?;
        let decimals = self.chain_token_metadata(chain_id, token, cx)?.decimals;
        Some(SetupFee {
            chain_id,
            token,
            maximum: ui_fixture::setup_fee(decimals)?,
            broadcaster: "a fixture broadcaster".to_owned(),
        })
    }

    fn public_review_command(
        &self,
        waku: Option<Arc<WakuDeliveryClient>>,
        cx: &mut Context<'_, Self>,
    ) -> Result<PublicSwapAuthorization, String> {
        let form = self.form.as_ref().ok_or("The swap form closed.")?;
        let public = form.public.as_ref().ok_or("Choose a Public account.")?;
        let origin = self
            .public_origin(cx)
            .ok_or("The origin network is unavailable.")?;
        let network = form.network.ok_or("Choose a destination network.")?;
        let (destination_session, owner) = self.ready_destination(network, cx)?;
        let review = public.review.clone().ok_or("Wait for the quote.")?;
        let amount = self.form_amount(form, cx)?;
        // An open swap of the account that trades one of the draft's tokens, and the form's
        // other reasons, keep the review unavailable.
        if let Some(reason) = self
            .public_form_reason(form, cx)
            .filter(|reason| reason.blocks_review)
        {
            return Err(reason.text);
        }
        // The strip shows a share the bridge leg wasn't previewed at, or the quote can't
        // support the form's share.
        if let Some(strip) = public_strip(form) {
            if strip.pending() {
                return Err(form.bridge_quote_error.as_ref().map_or_else(
                    || "Updating the bridge quote…".to_owned(),
                    ToString::to_string,
                ));
            }
            if strip.share_bps != form.gas_share_bps {
                return Err("Gas is too high for this swap right now.".into());
            }
        }
        if amount != review.sell_amount()
            || !public
                .quote_terms
                .as_ref()
                .is_some_and(|terms| terms.matches(form, &origin, amount))
        {
            return Err(
                "The swap's source or terms changed. Quote it again before reviewing.".into(),
            );
        }
        if !review.price_verified() && !form.price_acknowledged {
            return Err(UNVERIFIED_PRICE_WARNING.into());
        }
        if self.public_high_cost(form, &review, cx).is_some() && !form.high_costs_acknowledged {
            return Err("A large share of this swap may go to costs. Review them and choose Swap anyway to continue.".into());
        }
        {
            let root = self.root.upgrade().ok_or("The wallet session ended.")?;
            let root = root.read(cx);
            if public.account.scope == PublicAccountScope::Global
                || !root
                    .public_accounts
                    .iter()
                    .any(|account| account == &public.account)
                || public.account.status != PublicAccountStatus::Active
                || matches!(
                    public.account.source,
                    PublicAccountSource::ExecutorDerived(_)
                )
            {
                return Err(
                    "Choose an active Public account scoped to this Private wallet.".into(),
                );
            }
        }
        if let Some(shortfall) = self.public_native_shortfall(form, &review, cx) {
            return Err(self.public_shortfall_text(form, shortfall, cx));
        }
        let held = public.operation.and_then(|operation| {
            self.public_records
                .iter()
                .find(|(_, record)| record.operation() == operation)
                .map(|(_, record)| record)
        });
        let held_swap =
            held.and_then(|record| public.swap_use.and_then(|id| record.public_swap_use(id)));
        // Debug UI fixture: a new account's stand-in setup, reviewed without a broadcaster.
        let fixture_setup = self
            .ui_fixture_setup(form, cx)
            .filter(|_| held_swap.is_none());
        let (account, approved, candidate, setup_fee) = if let Some((claimed, saved)) = held_swap {
            let fresh = claimed.is_fresh();
            let needs_setup = fresh && self.destination_setup_chain(form).is_some();
            let candidate = if needs_setup {
                // The broadcaster the fee was estimated for, which a repeated estimate
                // under way may differ from.
                Some(
                    form.destination_route
                        .offered_estimate()
                        .map(|estimate| estimate.broadcaster().clone())
                        .ok_or("Choose a setup broadcaster on the destination network.")?,
                )
            } else {
                None
            };
            let fee = if needs_setup {
                Some(
                    form.destination_route
                        .offered_estimate()
                        .map(|estimate| default_public_broadcaster_fee_limit(estimate.fee_amount()))
                        .ok_or("Wait for the destination setup fee.")?,
                )
            } else {
                saved.approval().bounds.destination_setup_fee
            };
            let operation = public.operation.expect("held operation");
            (
                if fresh {
                    SwapAccountChoice::New(operation)
                } else {
                    SwapAccountChoice::Existing(operation)
                },
                SwapApprovedAccount {
                    address: held.and_then(ExecutorRecord::address),
                    setup: fresh,
                },
                candidate,
                fee,
            )
        } else if let Some(selected) = form.destination_choice() {
            (
                SwapAccountChoice::Existing(selected.operation),
                SwapApprovedAccount {
                    address: Some(selected.address),
                    setup: false,
                },
                None,
                None,
            )
        } else {
            let route = &form.destination_route;
            let (candidate, fee) = if let Some(setup) = &fixture_setup {
                (None, setup.maximum)
            } else {
                // The broadcaster the fee was estimated for, which a repeated estimate
                // under way may differ from.
                let candidate = route
                    .offered_estimate()
                    .map(|estimate| estimate.broadcaster().clone())
                    .ok_or("Choose a setup broadcaster on the destination network.")?;
                let fee = route
                    .offered_estimate()
                    .map(|estimate| default_public_broadcaster_fee_limit(estimate.fee_amount()))
                    .ok_or("Wait for the destination setup fee.")?;
                (Some(candidate), fee)
            };
            let operation = public
                .operation
                .unwrap_or(ExecutorOperationId::random().map_err(|error| error.to_string())?);
            (
                SwapAccountChoice::New(operation),
                SwapApprovedAccount {
                    address: None,
                    setup: true,
                },
                candidate,
                Some(fee),
            )
        };
        let operation = account.operation();
        let swap_use = public
            .swap_use
            .unwrap_or(SwapUseId::random().map_err(|error| error.to_string())?);
        let approval = review
            .approval(approved, setup_fee, form.price_acknowledged)
            .map_err(|error| error.to_string())?;
        let clients = public
            .clients
            .clone()
            .ok_or("The swap's route is unavailable. Quote it again.")?;
        if candidate.is_some() && waku.is_none() {
            return Err("The broadcaster network isn't ready.".into());
        }
        // The setup this review pays for, when it sends one.
        let setup = candidate
            .as_ref()
            .zip(setup_fee)
            .map(|(candidate, maximum)| SetupFee {
                chain_id: network,
                token: candidate.token,
                maximum,
                broadcaster: broadcaster_candidate_label(candidate),
            })
            .or(fixture_setup);
        let summary = self.public_review_summary(form, &review, &approval, setup.as_ref(), cx);
        Ok(PublicSwapAuthorization {
            source: public.account.clone(),
            origin,
            destination_session,
            owner,
            operation,
            swap_use,
            action: PublicSwapCommand::Swap {
                account,
                destination_token: form.buy.ok_or("Choose a destination token.")?,
                candidate,
                review,
                approval,
                clients,
                waku,
            },
            summary,
            revision: form.quote_revision,
        })
    }

    fn public_high_cost(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Option<u64> {
        let network = form.network?;
        let bridge = review.bridge();
        let sold = self.usd_micro_value(form.sell, review.sell_amount(), cx);
        let received =
            self.network_usd_micro_value(network, form.buy?, bridge.received_minimum(), cx);
        let gas = self.usd_micro_value(Address::ZERO, review.gas_plan().max_gas_cost, cx);
        let setup = if self.destination_setup_chain(form).is_some() {
            form.destination_route
                .estimate
                .as_ref()
                .and_then(|estimate| {
                    self.network_usd_micro_value(
                        network,
                        form.destination_route.fee_token?,
                        default_public_broadcaster_fee_limit(estimate.fee_amount()),
                        cx,
                    )
                })
        } else {
            Some(U256::ZERO)
        };
        let bps = if let (Some(sold), Some(received), Some(gas), Some(setup)) =
            (sold, received, gas, setup)
        {
            if sold.is_zero() {
                return None;
            }
            sold.saturating_sub(received)
                .saturating_add(gas)
                .saturating_add(setup)
                .saturating_mul(U256::from(10_000_u32))
                .checked_div(sold)?
                .saturating_to::<u64>()
        } else {
            // Without cached USD values, compare only amounts with the same denomination.
            let input = review.buy_amount().unwrap_or_else(|| review.sell_amount());
            let share = |cost: U256, amount: U256| {
                cost.saturating_mul(U256::from(10_000_u32))
                    .checked_div(amount)
                    .unwrap_or_default()
                    .saturating_to::<u64>()
            };
            let mut bps = share(bridge.fee.unwrap_or_default(), input);
            if let Some(private) = bridge.private {
                bps = bps
                    .saturating_add(share(private.delivery_allowance, private.quoted_output))
                    .saturating_add(private.destination_shield_fee_bps.saturating_to::<u64>());
            }
            if let Ok(approval) = review.approval(
                SwapApprovedAccount {
                    address: None,
                    setup: false,
                },
                None,
                true,
            ) {
                let allowance = approval.bounds.gas_allowance.unwrap_or_default();
                bps = bps.saturating_add(share(allowance, input.saturating_add(allowance)));
            }
            if form.sell == Address::ZERO {
                bps =
                    bps.saturating_add(share(review.gas_plan().max_gas_cost, review.sell_amount()));
            }
            bps
        };
        (bps >= AUTHORIZED_COST_WARNING_BPS).then_some(bps)
    }

    /// Whether a review that pays for no setup finds its fresh account already set up, as when
    /// a swap is continued. Its one dialog then is the last step, with nothing to wait for.
    fn public_setup_ready(
        &self,
        form: &SwapForm,
        approval: &PublicSwapApproval,
        pays_setup: bool,
    ) -> bool {
        approval.destination.setup
            && !pays_setup
            && matches!(
                self.public_destination_setup_progress(form),
                Some(SwapSetupProgress::Done)
            )
    }

    /// The compact review of a swap paid from a Public account. `setup` is the destination
    /// setup this review pays for.
    pub(super) fn public_review_summary(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        approval: &PublicSwapApproval,
        setup: Option<&SetupFee>,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let public = form.public.as_ref().expect("Public review");
        // The last step: the review of an account already set up.
        let ready = self.public_setup_ready(form, approval, setup.is_some());
        let destination = form.network.expect("Public destination");
        let (origin, network) = (self.chain_label(), network_name(destination));
        let label = public_source_label(&public.account);
        let order = review.intent().order;
        let bridged = review.intent().bridged_token;
        let buy = form.buy.unwrap_or_default();
        let bounds = &approval.bounds;
        let sold = self.form_sell_amount(form, review.sell_amount(), cx);
        let native = self.token_symbol(Address::ZERO, cx);
        let sell_metadata = self.form_sell_metadata(form, cx);
        let send_label = if order { "Sell" } else { "Send" };
        let send = SpendAuthorizationCard::new(
            if destination == self.origin_chain_id {
                SpendAuthorizationLabel::from(send_label)
            } else {
                SpendAuthorizationLabel::on_network(
                    format!("{send_label} on"),
                    self.origin_chain_id,
                    "",
                )
            },
            sold.clone(),
            sell_metadata
                .as_ref()
                .and_then(|metadata| metadata.icon_path.clone()),
        )
        .with_network(self.origin_chain_id)
        .with_usd(
            self.usd_label(form.sell, review.sell_amount(), cx)
                .filter(|usd| {
                    !usd_repeats_amount(
                        usd,
                        &sell_metadata.as_ref().map_or_else(String::new, |metadata| {
                            railgun_ui::format_token_amount(review.sell_amount(), metadata.decimals)
                        }),
                    )
                }),
        )
        .with_account(label.clone(), public.account.address.to_checksum(None));
        // An order's amount is its minimum. A deposit delivers what Across quoted.
        let receive = self.destination_card(
            if order { ", at least" } else { "" },
            destination,
            buy,
            buy,
            review.bridge().received_minimum(),
            cx,
        );
        // An order's best case, as the private review's Receive card shows it.
        let receive = match review.best_case() {
            Some(best) => receive.with_emphasis(
                "up to",
                self.network_bare_amount(destination, buy, best, cx),
                "if solvers pay all gas",
            ),
            None => receive,
        }
        .with_line("to your private balance");
        // The rows of the Costs line, then the rows after it.
        let (mut costs, mut rows) = (Vec::new(), Vec::new());
        let approvals = match review.gas_plan().approval_gas_limits.len() {
            0 => None,
            1 => Some("one approval"),
            _ => Some("two approvals"),
        };
        let sends = match (approvals, order) {
            (Some(approvals), true) => approvals.to_owned(),
            (None, true) => "no approval needed".to_owned(),
            (Some("one approval"), false) => "approval and deposit".to_owned(),
            (Some(approvals), false) => format!("{approvals} and deposit"),
            (None, false) => "deposit".to_owned(),
        };
        // What the account does in the second step, which names an approval only when one is
        // sent.
        let acts = match (approvals.is_some(), order) {
            (true, true) => "sends its approval and signs the order",
            (false, true) => "signs the order",
            (true, false) => "sends its approval and the deposit",
            (false, false) => "sends the deposit",
        };
        let gas_paragraphs = match (approvals, order) {
            (Some(approvals), true) => vec![
                format!(
                    "{label} sends {approvals}, so CoW can take exactly {sold}. It pays that gas in {native}."
                ),
                "The swap and the bridge deposit are sent by a CoW solver. Their gas comes out of what you receive.".to_owned(),
            ],
            (None, true) => vec![
                format!("{label} already lets CoW take {sold}, so it sends nothing."),
                "The swap and the bridge deposit are sent by a CoW solver. Their gas comes out of what you receive.".to_owned(),
            ],
            (Some(_), false) => vec![format!(
                "{label} sends the approval and the deposit itself, so Across takes exactly {sold}. It pays that gas in {native}."
            )],
            (None, false) => vec![format!(
                "{label} sends the deposit of {sold} itself. It pays that gas in {native}."
            )],
        };
        // What is paid up front outside the swap: a new account's setup fee and the gas of what
        // the Public account sends itself. An order whose approval stands sends nothing.
        let gas = (approvals.is_some() || !order)
            .then(|| self.token_amount(Address::ZERO, approval.max_gas_cost, cx));
        // Neither the setup fee nor the account's own gas is refunded.
        let pay_now = if let Some(fee) = setup {
            let amount = self.setup_fee_amount(fee, cx);
            let (title, total) = match &gas {
                Some(gas) => ("Setup fee and gas", format!("{amount} + {gas}")),
                None => ("Setup fee", amount),
            };
            costs.push(
                SpendAuthorizationSummaryRow::new(
                    "Pay now",
                    format!("up to {total} · not refunded"),
                )
                .with_hint(SpendAuthorizationHint::new(
                    title,
                    std::iter::once(format!(
                        "Paid to broadcaster {} from your private balance on {} to create the stealth account there. Not refunded if the swap doesn't go through.",
                        fee.broadcaster,
                        network_name(fee.chain_id)
                    ))
                    .chain(gas_paragraphs),
                )),
            );
            Some(total)
        } else if let Some(gas) = gas {
            costs.push(
                SpendAuthorizationSummaryRow::new("Pay now", format!("up to {gas} · {sends}"))
                    .with_hint(SpendAuthorizationHint::new(
                        format!("Gas from {label}"),
                        gas_paragraphs,
                    )),
            );
            Some(gas)
        } else {
            None
        };
        let valid_for = u64::from(bounds.valid_for_secs.unwrap_or_default()) / 60;
        if let (Some(share), Some(allowance), Some(estimate)) = (
            bounds.gas_share_bps,
            bounds.gas_allowance,
            bounds.gas_estimate,
        ) {
            costs.push(self.gas_row(bridged, share, allowance, estimate, valid_for, cx));
        }
        let bridge = review.bridge();
        let fee = bridge.fee.map(|fee| self.token_amount(bridged, fee, cx));
        let allowance = bridge.private.map(|private| {
            (
                self.network_bare_amount(destination, buy, private.delivery_allowance, cx),
                self.network_token_amount(destination, buy, private.delivery_allowance, cx),
            )
        });
        costs.push(
            SpendAuthorizationSummaryRow::new(
                "Bridge",
                across_row_value(
                    provider_name(BridgeProvider::Across),
                    fee.as_deref(),
                    allowance.as_ref().map(|(allowance, _)| allowance.as_str()),
                ),
            )
            .with_hint(SpendAuthorizationHint::new(
                provider_name(BridgeProvider::Across),
                fee.as_deref()
                    .zip(allowance.as_ref())
                    .map(|(fee, (_, allowance))| across_delivery_note(fee, destination, allowance))
                    .into_iter()
                    .chain([format!(
                        "If the deposit isn't filled before it expires, Across refunds it to {label} on {origin}, usually within a few hours."
                    )]),
            )),
        );
        // An existing account is named, with what reusing it links.
        let reused = (!approval.destination.setup)
            .then(|| self.public_reused_account(form))
            .flatten();
        let kind = if order { "swap" } else { "deposit" };
        let reuse_warning = reused.map(|(index, _)| {
            format!(
                "Reusing {} on {network} links this {kind} to that account's earlier swap.",
                index.map_or_else(
                    || "this account".to_owned(),
                    |index| format!("account #{index}")
                )
            )
        });
        if let Some((index, address)) = reused {
            rows.push(
                SpendAuthorizationSummaryRow::naming_network(
                    SpendAuthorizationLabel::on_network("Account on", destination, ""),
                    address.to_checksum(None),
                )
                .with_copyable_account(
                    index.map_or_else(
                        || "Reuses ".to_owned(),
                        |index| format!("Reuses #{index} · "),
                    ),
                    "stealth account address",
                )
                .with_hint(SpendAuthorizationHint::new(
                    format!("Account on {network}"),
                    reuse_warning
                        .clone()
                        .into_iter()
                        .chain(["A new stealth account offers more privacy.".to_owned()]),
                )),
            );
        }
        // The review states the choice made on the form, so its hint covers that choice only.
        let [refund, keep] = self.public_failure_paragraphs(form, destination, cx);
        let (failure, paragraph) = match approval.on_shield_failure {
            BridgeShieldFailure::RefundOnOrigin => refund,
            BridgeShieldFailure::KeepOnDestination => keep,
        };
        // A refund names the Public account, and a kept delivery its network.
        let failure_value = match approval.on_shield_failure {
            BridgeShieldFailure::RefundOnOrigin => SpendAuthorizationLabel::from(failure.clone()),
            choice @ BridgeShieldFailure::KeepOnDestination => {
                shield_failure_mention(choice, self.origin_chain_id, destination)
            }
        };
        rows.push(
            SpendAuthorizationSummaryRow::naming_network("If the shield fails", failure_value)
                .with_hint(SpendAuthorizationHint::new(
                    failure,
                    [
                        format!(
                            "This covers the rare case where the shield on {network} can't run."
                        ),
                        paragraph,
                    ],
                )),
        );
        let step = if order {
            "Approve and place order"
        } else {
            "Approve and deposit"
        };
        let title = match (ready, order, approval.destination.setup) {
            (true, ..) => step.to_owned(),
            (false, false, _) => format!("Shield on {network}"),
            (false, true, true) => "Set up account and swap".to_owned(),
            (false, true, false) => "Swap to private balance".to_owned(),
        };
        let mut public_paragraphs = vec![if order {
            format!("{label}'s order and its Across deposit on {origin}, with their amounts.")
        } else {
            format!("{label}'s Across deposit on {origin}, with its amount.")
        }];
        public_paragraphs.push(format!(
            "The deposit names a stealth account on {network} and the shield it runs. Anyone can see that {label} paid for a shield of about this amount on {network} at about this time."
        ));
        public_paragraphs.push(format!(
            "Your private address and what you do with the funds on {network} afterwards stay private."
        ));
        let mut summary = SpendAuthorizationSummary::new(title, "", rows)
            .with_cards(send, receive)
            .with_compact_rows()
            .with_row_group(
                "Costs",
                review_costs_summary(
                    self.public_costs_total(form, review, cx),
                    pay_now.as_deref(),
                ),
                costs,
            )
            .with_disclosure(
                format!("that {label} paid for a shield on {network}"),
                SpendAuthorizationHint::new("What becomes public", public_paragraphs),
            )
            .with_confirm_label(if approval.destination.setup && !ready {
                "Create stealth account"
            } else {
                step
            })
            .requiring_explicit_review();
        if order {
            let slippage = format_bps_percent(u64::from(bounds.slippage_bps));
            let mut details = Vec::new();
            if let (Some(estimate), Some(price)) = (bounds.gas_estimate, bounds.gas_price_wei) {
                details.push((
                    "Gas estimate",
                    format!(
                        "≈ {} at {} gwei",
                        self.gas_money(bridged, estimate, cx),
                        format_gwei(price)
                    ),
                ));
            }
            details.push(("Price tolerance", slippage.clone()));
            // The proxy the order goes through, with what it does behind the terms' note.
            let proxy = review.proxy().map(|proxy| {
                details.push(("Your CoW proxy", proxy.to_checksum(None)));
                format!(
                    "Your CoW proxy is a small contract that only {label} controls. The swap pays into it and, in the same transaction, it deposits everything into Across. If the deposit step fails, the {} stays in the proxy and you can withdraw it to {label}.",
                    self.token_symbol(bridged, cx)
                )
            });
            details.push((
                "Deposit to Across",
                format!(
                    "{} on {origin}",
                    self.token_amount(bridged, bounds.buy_amount, cx)
                ),
            ));
            details.push((
                "Order valid for",
                if approval.destination.setup && !ready {
                    format!("{valid_for} minutes after setup")
                } else {
                    format!("{valid_for} minutes")
                },
            ));
            summary = summary.with_details(
                "Order terms",
                format!("{slippage} price · {valid_for} min"),
                details,
                proxy.as_deref(),
            );
        }
        let mut warnings = Vec::new();
        if !review.price_verified() {
            warnings.push(Arc::from(UNVERIFIED_PRICE_WARNING));
        }
        if let Some(bps) = self.public_high_cost(form, review, cx) {
            warnings.push(Arc::from(public_cost_warning(bps).message()));
        }
        warnings.extend(reuse_warning.map(Arc::from));
        // As the private swap's next review says it.
        if let Some(change) = public.review_change {
            warnings.push(Arc::from(format!(
                "Your last review wasn't used: {}. These are the new terms. Check them before you approve.",
                review_change_label(change)
            )));
        }
        let summary = summary.with_warnings(warnings);
        // A same-token deposit to an existing account waits on nothing, so it has one step.
        match (approval.destination.setup, order) {
            (true, _) => summary.with_steps(
                if ready { 2 } else { 1 },
                [
                    SpendAuthorizationLabel::on_network("Set up account on", destination, ""),
                    step.into(),
                ],
                SpendAuthorizationHint::new(
                    "Two steps",
                    [
                        format!(
                            "1. Set up account on {network}. A broadcaster creates a one-time stealth account there, and you pay the setup fee from your private balance on {network}."
                        ),
                        format!(
                            "2. {step}. Once setup confirms, the wallet continues by itself and {label} {acts}. If the terms changed, you review them first."
                        ),
                    ],
                ),
            ),
            (false, true) => summary.with_steps(
                2,
                [
                    SpendAuthorizationLabel::on_network("Account on", destination, " ready"),
                    step.into(),
                ],
                SpendAuthorizationHint::new(
                    "Two steps",
                    [
                        format!(
                            "1. The stealth account on {network} is already set up, so there is no setup and no setup fee."
                        ),
                        format!("2. {step}. {label} {acts}."),
                    ],
                ),
            ),
            (false, false) => summary,
        }
    }

    /// The existing destination account the swap reuses: the one the form chose, or the one a
    /// continued swap holds. Its number is known once its record is read.
    fn public_reused_account(&self, form: &SwapForm) -> Option<(Option<u32>, Address)> {
        if let Some(account) = form.destination_choice() {
            return Some((Some(account.index), account.address));
        }
        let operation = form.public.as_ref()?.operation?;
        let (_, record) = self
            .public_records
            .iter()
            .find(|(_, record)| record.operation() == operation)?;
        Some((Some(record.index()), record.address()?))
    }

    pub(in crate::root) fn cancel_public_authorization(
        &mut self,
        command: &Arc<PublicSwapAuthorization>,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .public_authorization
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, command))
        {
            self.public_authorization = None;
            cx.notify();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::root) fn continue_authorized_public_swap(
        &mut self,
        command: &Arc<PublicSwapAuthorization>,
        private_authorization: DesktopPrivateSpendAuthorization,
        public_authorization: Option<DesktopPrivateSpendAuthorization>,
        trezor_app_passphrase: Option<zeroize::Zeroizing<String>>,
        trezor_pin_matrix_provider: Option<wallet_ops::HardwareTrezorPinMatrixProvider>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self
            .public_authorization
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
        let Some(public_authorization) = public_authorization else {
            self.error = Some("Authorize the Public account before preparing this swap.".into());
            cx.notify();
            return;
        };
        if matches!(command.action, PublicSwapCommand::Swap { .. })
            && (!self
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.view == SwapDialogView::Form)
                || !self.form.as_ref().is_some_and(|form| {
                    form.quote_revision == command.revision
                        && form
                            .public
                            .as_ref()
                            .is_some_and(|public| public.account == command.source)
                }))
        {
            return;
        }
        if matches!(command.action, PublicSwapCommand::Swap { .. })
            && let Some(public) = self.form.as_mut().and_then(|form| form.public.as_mut())
        {
            public.operation = Some(command.operation);
            public.swap_use = Some(command.swap_use);
            public.review_change = None;
        }
        self.public_status = Some("Preparing the swap".into());
        let command = command.clone();
        self.start_public_job(
            command.clone(),
            async move {
                // Admitting the source can unlock an account or prompt its device; do it before
                // the destination acquires any claim or issues a signature.
                let signer = command
                    .owner
                    .authorize_public_swap_source(
                        &command.origin,
                        PublicSwapSource {
                            public_account_uuid: &command.source.public_account_uuid,
                            authorization: Some(&public_authorization),
                            trezor_app_passphrase,
                            trezor_pin_matrix_provider,
                        },
                    )
                    .await?;
                drop(public_authorization);
                let execution = PublicSwapExecution {
                    command,
                    private_authorization: Some(private_authorization),
                    setup_cursor: None,
                    signer,
                    signed: None,
                };
                prepare_public_execution(execution).await
            },
            window,
            cx,
        );
    }

    /// Whether the dialog still shows the swap of `command`: its reviewed form, or its detail
    /// once the swap is claimed. A recovery's command isn't tied to a view.
    fn public_command_is_current(&self, command: &PublicSwapAuthorization) -> bool {
        if !matches!(command.action, PublicSwapCommand::Swap { .. }) {
            return true;
        }
        match self.dialog.as_ref().map(|dialog| dialog.view) {
            Some(SwapDialogView::Form) => self.form.as_ref().is_some_and(|form| {
                form.quote_revision == command.revision
                    && form.network == Some(command.destination_session.chain_id)
                    && form.public.as_ref().is_some_and(|public| {
                        public.account == command.source
                            && public.operation == Some(command.operation)
                            && public.swap_use == Some(command.swap_use)
                    })
            }),
            Some(SwapDialogView::PublicDetail(shown)) => shown == command.identity(),
            _ => false,
        }
    }

    /// The Public account swap the wallet is working on: a job runs for it, or its execution
    /// is held between jobs.
    pub(in crate::root::private_swap) fn public_swap_at_work(
        &self,
    ) -> Option<super::super::model::SwapIdentity> {
        self.public_execution
            .as_ref()
            .map(|execution| execution.command.identity())
            .or(self.public_running)
    }

    /// Drop the Public account swap in preparation when the dialog leaves its form, or the
    /// detail that shows it at work. Its saved record continues it later.
    pub(in crate::root::private_swap) fn dismiss_public_preparation(&mut self) {
        let leaves = match self.dialog.as_ref().map(|dialog| dialog.view) {
            Some(SwapDialogView::Form) => {
                self.form.as_ref().is_some_and(|form| form.public.is_some())
            }
            Some(SwapDialogView::PublicDetail(shown)) => self.public_swap_at_work() == Some(shown),
            _ => false,
        };
        if leaves {
            self.public_job = None;
            self.public_running = None;
            self.public_execution = None;
            self.public_authorization = None;
        }
    }

    fn start_public_job(
        &mut self,
        command: Arc<PublicSwapAuthorization>,
        future: impl std::future::Future<Output = eyre::Result<PublicExecutionResult>> + Send + 'static,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let runtime = self.runtime.clone();
        self.public_running =
            matches!(command.action, PublicSwapCommand::Swap { .. }).then(|| command.identity());
        if self.public_running.is_some() {
            // An earlier failure doesn't stay beside the work that follows it.
            self.error = None;
        }
        self.public_job = Some(cx.spawn_in(window, async move |view, cx| {
            let task = runtime.spawn(future);
            let _abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.public_job = None;
                view.public_running = None;
                if !view.session_is_current(cx)
                    || !view.public_command_is_current(&command)
                    || !view
                        .root
                        .upgrade()
                        .is_some_and(|root| command.sessions_are_current(root.read(cx)))
                {
                    return;
                }
                match result {
                    Ok(Ok(PublicExecutionResult::Claimed(execution))) => {
                        // Debug UI fixture: a synthesized record stands in for the claim.
                        #[cfg(debug_assertions)]
                        view.stage_ui_fixture_swap(&command);
                        view.refresh_public_swap_records(cx);
                        // The swap has its record, so its detail shows the rest. The held
                        // execution marks the swap as at work, which keeps the switch from
                        // dropping it.
                        view.public_execution = Some(execution);
                        view.navigate(SwapDialogView::PublicDetail(command.identity()), window, cx);
                        view.prepare_claimed_public_swap(window, cx);
                    }
                    Ok(Ok(PublicExecutionResult::Waiting(execution))) => {
                        view.public_execution = Some(execution);
                    }
                    Ok(Ok(PublicExecutionResult::Ready(execution))) => {
                        // Only an order has signatures a device can't explain: its hook batch
                        // and the order itself. A deposit is a transaction the device decodes.
                        let decoded = matches!(
                            execution.command.source.source,
                            PublicAccountSource::HardwareDerived
                        ) && execution
                            .signed
                            .as_ref()
                            .is_some_and(|signed| signed.batch.is_some());
                        view.public_execution = Some(execution);
                        if decoded {
                            view.review_public_signatures(window, cx);
                        } else {
                            view.submit_public_signed(false, window, cx);
                        }
                    }
                    Ok(Ok(PublicExecutionResult::Changed(change))) => {
                        // Nothing was placed or deposited. The form comes back from the saved
                        // swap and quotes again, and the next review names the change, as a
                        // private swap's does.
                        view.reopen_public_swap_form(command.identity(), false, window, cx);
                        if let Some(public) =
                            view.form.as_mut().and_then(|form| form.public.as_mut())
                        {
                            public.review_change = Some(change);
                        }
                    }
                    Ok(Ok(PublicExecutionResult::ProxyHolds { proxy, balance })) => {
                        let message = view.public_proxy_holds_text(&command, proxy, balance, cx);
                        view.reopen_public_swap_form(command.identity(), false, window, cx);
                        if let Some(form) = view.form.as_mut() {
                            form.error = Some(message.clone());
                            if let Some(public) = form.public.as_mut() {
                                public.proxy_holds = Some((message.clone(), proxy));
                            }
                        }
                        view.error = Some(message);
                    }
                    Ok(Ok(PublicExecutionResult::Finished(identity))) => {
                        view.public_execution = None;
                        view.refresh_public_swap_records(cx);
                        view.refresh_public_source_balances(cx);
                        view.navigate(SwapDialogView::PublicDetail(identity), window, cx);
                    }
                    Ok(Err(error)) => {
                        // An open swap that trades the same token is named under the Sell
                        // card, where it keeps Review unavailable. Past the form, the detail
                        // shows the error and its record's actions take over.
                        let conflict = view
                            .form
                            .as_ref()
                            .and_then(|form| PublicSwapConflict::of(&error, &command, form));
                        if let Some(conflict) = conflict {
                            if let Some(public) =
                                view.form.as_mut().and_then(|form| form.public.as_mut())
                            {
                                public.conflict = Some(conflict);
                            }
                        } else {
                            view.error = Some(format!("{error:#}"));
                            if let Some(form) = view.form.as_mut() {
                                form.error.clone_from(&view.error);
                            }
                        }
                    }
                    Err(_) => {
                        view.error =
                            Some("The swap stopped. Check its status before trying again.".into());
                    }
                }
                view.refresh_public_swap_records(cx);
                // Debug UI fixture: a swap that stops at its review is in no record, and keeps
                // its form.
                if !ui_fixture::active()
                    && let Some(public) = view.form.as_mut().and_then(|form| form.public.as_mut())
                    && public.operation.is_some_and(|operation| {
                        !view.public_records.iter().any(|(_, record)| {
                            record.operation() == operation
                                && public
                                    .swap_use
                                    .is_some_and(|id| record.public_swap_use(id).is_some())
                        })
                    })
                {
                    public.operation = None;
                    public.swap_use = None;
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Start what follows the claim, which the swap's detail shows: the destination account's
    /// preparation and, for a new account, its setup.
    fn prepare_claimed_public_swap(&mut self, window: &Window, cx: &mut Context<'_, Self>) {
        let Some(execution) = self.public_execution.take() else {
            return;
        };
        let command = execution.command.clone();
        self.public_status = Some(match &command.action {
            // Debug UI fixture: its stand-in setup names no broadcaster.
            PublicSwapCommand::Swap {
                candidate,
                approval,
                ..
            } if candidate.is_some() || (ui_fixture::active() && approval.destination.setup) => {
                // Shown on the detail's setup step, which names the account and its network.
                "Sending the setup to a broadcaster".into()
            }
            _ => "Preparing the swap".into(),
        });
        self.start_public_job(command, prepare_public_destination(execution), window, cx);
    }

    /// Debug UI fixture: put the synthesized record of the swap `command` reviewed where its
    /// claim would be.
    #[cfg(debug_assertions)]
    fn stage_ui_fixture_swap(&mut self, command: &PublicSwapAuthorization) {
        let PublicSwapCommand::Swap {
            destination_token,
            review,
            approval,
            ..
        } = &command.action
        else {
            return;
        };
        self.stage_ui_fixture_flow(
            command.identity(),
            command.destination_session.chain_id,
            &ui_fixture::Staged {
                origin: command.origin.chain_id,
                source: command.source.address,
                sell: approval.sell_token,
                bridged: review.intent().bridged_token,
                delivered: *destination_token,
                order: review.intent().order,
                sold: review.sell_amount(),
                bought: review.buy_amount().unwrap_or_else(|| review.sell_amount()),
                received: approval.bounds.private_minimum,
            },
        );
    }

    pub(in crate::root::private_swap) fn continue_public_swap_after_setup(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_job.is_some() || self.public_authorization.is_some() {
            return;
        }
        let Some(execution) = self
            .public_execution
            .as_ref()
            .filter(|execution| execution.signed.is_none())
        else {
            if self.public_execution.is_none()
                && self.form.as_ref().is_some_and(|form| {
                    form.public
                        .as_ref()
                        .is_some_and(|public| public.review.is_none())
                        && form.quote_task.is_none()
                        && matches!(form.quote, QuoteState::Idle)
                })
            {
                self.schedule_public_quote(window, cx);
            }
            return;
        };
        if !self.public_command_is_current(&execution.command)
            || !self
                .root
                .upgrade()
                .is_some_and(|root| execution.command.sessions_are_current(root.read(cx)))
        {
            self.public_execution = None;
            return;
        }
        let chain = execution.command.destination_session.chain_id;
        let Some(confirmed) = self
            .root
            .upgrade()
            .and_then(|root| root.read(cx).confirmed_block(chain))
        else {
            return;
        };
        // The job can't report that it got past the setup, so only an account already known to
        // be set up, as a continued swap's is, names what follows.
        let set_up = ui_fixture::setup_done()
            || matches!(
                self.public_setup_progress(execution.command.operation),
                Some(SwapSetupProgress::Done)
            );
        let execution = self.public_execution.take().expect("checked execution");
        // The job goes on to the approvals, which a hardware account's device signs.
        let approves = matches!(
            &execution.command.action,
            PublicSwapCommand::Swap { review, .. }
                if !review.gas_plan().approval_gas_limits.is_empty()
        );
        let device = self
            .public_device_hint(&execution.command, cx)
            .filter(|_| approves)
            .unwrap_or_default();
        self.public_status = Some(if set_up {
            format!(
                "{}{device}",
                Self::public_finishing_status(&execution.command)
            )
            .into()
        } else {
            format!(
                "Setting up the stealth account on {} · waiting for the setup to be confirmed{device}",
                network_name(chain)
            )
            .into()
        });
        self.start_public_job(
            execution.command.clone(),
            advance_public_execution(execution, confirmed),
            window,
            cx,
        );
    }

    /// What a status adds while the job may wait on a hardware account's device. `None` for
    /// an account the wallet signs for itself.
    fn public_device_hint(&self, command: &PublicSwapAuthorization, cx: &App) -> Option<String> {
        (command.source.source == PublicAccountSource::HardwareDerived).then(|| {
            format!(
                " · confirm on your {} when it asks",
                self.public_device_label(cx)
            )
        })
    }

    /// What the account does once its destination is ready, before anything is placed.
    fn public_finishing_status(command: &PublicSwapAuthorization) -> SharedString {
        match &command.action {
            PublicSwapCommand::Swap { review, .. } if review.intent().order => {
                "Getting the order ready · this can take a minute".into()
            }
            _ => "Getting the deposit ready · this can take a minute".into(),
        }
    }

    /// What a hardware Public account's device is about to sign for an order, decoded: its
    /// proxy's bridge instructions, then the order. The device may show both as raw data.
    fn review_public_signatures(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(execution) = self.public_execution.as_ref() else {
            return;
        };
        let (
            Some(signed),
            PublicSwapCommand::Swap {
                review, approval, ..
            },
        ) = (execution.signed.as_ref(), &execution.command.action)
        else {
            return;
        };
        let Some(batch) = signed.batch.as_ref() else {
            return;
        };
        let device = self.public_device_label(cx);
        let groups = Arc::new(self.public_signature_groups(
            &execution.command.source,
            execution.command.operation,
            review,
            approval.sell_token,
            batch,
            signed.valid_to,
            cx,
        ));
        let view = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let sign = view.clone();
            let close = view.clone();
            dialog
                .title(DEVICE_SIGNATURES_TITLE)
                .footer(dialog_footer(public_sign_label(device), true))
                .on_close(move |_, window, cx| {
                    close.update(cx, |view, cx| {
                        if view.public_execution.take().is_some() {
                            view.schedule_public_quote(window, cx);
                        }
                        cx.notify();
                    });
                })
                .on_ok(move |_, window, cx| {
                    sign.update(cx, |view, cx| view.submit_public_signed(true, window, cx));
                    true
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            app_muted_text(format!(
                                "Your {device} will ask for two signatures. It may show them as raw data, so check them here first."
                            ))
                            .whitespace_normal(),
                        )
                        .children(groups.iter().map(|(title, rows)| {
                            div()
                                .w_full()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .border_1()
                                .border_color(rgb(theme::BORDER_SUBTLE))
                                .bg(rgb(theme::SURFACE))
                                .child(app_muted_text(title.clone()).text_xs())
                                .children(rows.iter().map(|row| {
                                    detail_row(
                                        row.label.clone(),
                                        div()
                                            .flex()
                                            .items_baseline()
                                            .gap_1()
                                            .child(app_text(row.value.clone()))
                                            .children(row.address.clone().map(|address| {
                                                app_text(address)
                                                    .text_xs()
                                                    .font_family(theme::APP_MONO_FONT_FAMILY)
                                            })),
                                        false,
                                        None,
                                    )
                                }))
                        })),
                )
        });
    }

    /// The device a hardware Public account signs on, as its button and intro name it.
    fn public_device_label(&self, cx: &App) -> &'static str {
        self.root
            .upgrade()
            .and_then(|root| {
                root.read(cx)
                    .view_session
                    .as_ref()
                    .and_then(|view| view.hardware_profile_session())
                    .map(|session| hardware_device_label(session.device_kind))
            })
            .unwrap_or("device")
    }

    /// The two groups a hardware review decodes for an order paid by `source`: the bridge
    /// instructions of `batch`, then the order of `review` that sells `sell_token`, valid
    /// until `valid_to`. `operation` is the destination stealth account.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn public_signature_groups(
        &self,
        source: &PublicAccountMetadata,
        operation: ExecutorOperationId,
        review: &PublicSwapReview,
        sell_token: Address,
        batch: &PublicSwapBatchTerms,
        valid_to: u32,
        cx: &App,
    ) -> [(String, Vec<PublicSignatureRow>); 2] {
        let network = batch.destination_chain;
        let bought = self.token_symbol(batch.input_token, cx);
        let delivered = self.network_token_symbol(network, batch.output_token, cx);
        // What the destination gets for each whole token the proxy holds.
        let scale = self
            .token_decimals(batch.input_token, cx)
            .zip(self.chain_token_metadata(network, batch.output_token, cx))
            .and_then(|(decimals, output)| {
                let rate = unit_rate(batch.scale_denominator, decimals, batch.scale_numerator)?;
                Some(railgun_ui::format_token_amount(rate, output.decimals))
            });
        let account = self
            .public_records
            .iter()
            .find(|(_, record)| record.operation() == operation)
            .map_or_else(
                || "your stealth account".to_owned(),
                |(_, record)| format!("account #{}", record.index()),
            );
        // The form names the Sell token as the account's balance does.
        let sold = self
            .form
            .as_ref()
            .filter(|form| form.sell == sell_token)
            .map_or_else(
                || self.token_amount(sell_token, review.sell_amount(), cx),
                |form| self.form_sell_amount(form, review.sell_amount(), cx),
            );
        [
            (
                "1 · Bridge instructions for your CoW proxy".to_owned(),
                vec![
                    PublicSignatureRow::new(
                        "Runs only if the proxy holds",
                        format!(
                            "at least {}",
                            self.token_amount(batch.guard_token, batch.guard_amount, cx)
                        ),
                    ),
                    PublicSignatureRow::new(
                        "Deposits into Across",
                        format!("all {bought} in the proxy"),
                    ),
                    PublicSignatureRow::new("Refunds go to", public_source_label(source))
                        .naming(batch.depositor),
                    PublicSignatureRow::new(
                        format!("Delivered on {}", network_name(network)),
                        scale.map_or_else(
                            || format!("{delivered} for the {bought} in the proxy"),
                            |scale| format!("at least {scale} {delivered} per {bought}"),
                        ),
                    ),
                    PublicSignatureRow::new("Recipient", format!("Across handler, then {account}")),
                    PublicSignatureRow::new(
                        "Valid until",
                        local_date_time_label(u64::from(batch.deadline)),
                    ),
                ],
            ),
            (
                "2 · CoW order".to_owned(),
                vec![
                    PublicSignatureRow::new("Sell", sold),
                    PublicSignatureRow::new(
                        "Buy at least",
                        // The signed order's amount, which a short bridge quote can raise.
                        self.token_amount(batch.guard_token, batch.guard_amount, cx),
                    ),
                    PublicSignatureRow::new("Receiver", "Your CoW proxy").naming(batch.proxy),
                    PublicSignatureRow::new(
                        "Valid until",
                        local_date_time_label(u64::from(valid_to)),
                    ),
                ],
            ),
        ]
    }

    fn submit_public_signed(
        &mut self,
        hash_fallback_confirmed: bool,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(execution) = self.public_execution.take() else {
            return;
        };
        if !self.public_command_is_current(&execution.command)
            || !self
                .root
                .upgrade()
                .is_some_and(|root| execution.command.sessions_are_current(root.read(cx)))
        {
            return;
        }
        let device = self
            .public_device_hint(&execution.command, cx)
            .unwrap_or_default();
        self.public_status = Some(
            match &execution.command.action {
                PublicSwapCommand::Swap { review, .. } if review.intent().order => {
                    format!("Placing the order{device}")
                }
                _ => format!("Sending the deposit{device}"),
            }
            .into(),
        );
        self.start_public_job(
            execution.command.clone(),
            submit_public_execution(execution, hash_fallback_confirmed),
            window,
            cx,
        );
    }

    pub(super) fn public_destination_setup_progress(
        &self,
        form: &SwapForm,
    ) -> Option<SwapSetupProgress> {
        self.public_setup_progress(form.public.as_ref()?.operation?)
    }

    /// How far the setup of the destination stealth account `operation` is, by its record.
    fn public_setup_progress(&self, operation: ExecutorOperationId) -> Option<SwapSetupProgress> {
        let (chain, record) = self
            .public_records
            .iter()
            .find(|(_, record)| record.operation() == operation)?;
        Some(
            match super::super::account_stage(record, *chain, None, false) {
                SwapStage::Ready => SwapSetupProgress::Done,
                SwapStage::SetupPending => SwapSetupProgress::Pending,
                SwapStage::SetupFailed => SwapSetupProgress::Failed,
                _ => SwapSetupProgress::NotSent,
            },
        )
    }

    pub(in crate::root::private_swap) fn refresh_public_swap_records(&mut self, cx: &App) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let root = root.read(cx);
        self.public_synced.extend(
            root.chain_states
                .iter()
                .filter(|(_, state)| matches!(state, ChainUtxoState::Ready { .. }))
                .map(|(chain, _)| *chain),
        );
        // Debug UI fixture: its synthesized records aren't read over.
        if ui_fixture::holds_records() {
            return;
        }
        self.public_railgun = root
            .effective_chain_configs
            .iter()
            .filter_map(|(&id, chain)| Some((id, chain.railgun.as_ref()?.deployment.contract)))
            .collect();
        let mut records = Vec::new();
        for (chain, state) in &root.chain_states {
            let (ChainUtxoState::Ready { session, .. } | ChainUtxoState::Syncing { session, .. }) =
                state
            else {
                continue;
            };
            let Some(owner) = session.executor_owner() else {
                continue;
            };
            match owner.records() {
                Ok(accounts) => records.extend(
                    accounts
                        .into_iter()
                        .filter(|record| {
                            record
                                .swap_uses()
                                .iter()
                                .any(|swap_use| swap_use.public_swap().is_some())
                        })
                        .map(|record| (*chain, record)),
                ),
                Err(error) => self.error = Some(format!("{error:#}")),
            }
        }
        records.sort_by_key(|(_, record)| std::cmp::Reverse((record.created_at(), record.index())));
        self.public_records = records;
        // An open swap that ended, or a new one, changes what blocks the form's pair.
        self.refresh_public_conflict(cx);
    }

    fn refresh_public_source_balances(&self, cx: &mut Context<'_, Self>) {
        let chains = self
            .public_records
            .iter()
            .flat_map(|(_, record)| record.swap_uses().iter())
            .filter_map(|swap_use| match swap_use.role() {
                SwapUseRole::PublicSourceDestination { origin_chain, .. } => Some(*origin_chain),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let _ = self.root.update(cx, |root, cx| {
            for chain in chains {
                root.schedule_public_balance_refresh_for_chain(
                    chain,
                    PublicAccountStatus::Active,
                    None,
                    cx,
                );
            }
        });
    }

    pub(in crate::root::private_swap) fn track_public_swaps(
        &mut self,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        let requests = self
            .public_records
            .iter()
            .flat_map(|(chain, record)| {
                record.public_swaps_to_track().filter_map(move |(id, _)| {
                    let SwapUseRole::PublicSourceDestination { origin_chain, .. } =
                        record.swap_use(id)?.role()
                    else {
                        return None;
                    };
                    Some((*chain, record.operation(), id, *origin_chain))
                })
            })
            .collect::<Vec<_>>();
        for (chain, operation, id, origin) in requests {
            self.track_public_swap_request(chain, operation, id, origin, false, window, cx);
        }
    }

    fn track_public_swap_request(
        &mut self,
        chain: u64,
        operation: ExecutorOperationId,
        id: SwapUseId,
        origin_chain: u64,
        explicit: bool,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        // Debug UI fixture: a synthesized record has no swap to track.
        if ui_fixture::holds_records() {
            return;
        }
        let key = (chain, operation, id);
        if !self.public_tracking.insert(key) {
            return;
        }
        let Some((_, owner)) = self.destination_owner(chain, cx) else {
            self.public_tracking.remove(&key);
            return;
        };
        let Some(root) = self.root.upgrade() else {
            self.public_tracking.remove(&key);
            return;
        };
        let (origin, http) = {
            let root = root.read(cx);
            (
                root.effective_chain_configs.get(origin_chain).cloned(),
                root.http.clone(),
            )
        };
        let Some(origin) = origin else {
            self.public_tracking.remove(&key);
            return;
        };
        let runtime = self.runtime.clone();
        cx.spawn_in(window, async move |view, cx| {
            let task = runtime.spawn(async move {
                let clients = public_swap_clients(&origin, &http).await?;
                owner
                    .track_public_swap(
                        operation,
                        id,
                        PublicSwapTracking {
                            origin: &origin,
                            across: &clients.across,
                            orderbook: clients.orderbook.as_ref(),
                            explicit,
                        },
                    )
                    .await
            });
            let _abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                view.public_tracking.remove(&key);
                view.refresh_public_swap_records(cx);
                match &result {
                    Ok(Ok(_)) => {
                        view.public_tracking_failures.remove(&(operation, id));
                    }
                    Ok(Err(_)) => {
                        let failures = view
                            .public_tracking_failures
                            .entry((operation, id))
                            .or_default();
                        *failures = failures.saturating_add(1);
                    }
                    Err(_) => {}
                }
                match result {
                    Ok(Ok(progress)) if progress.refresh_public_balances => {
                        view.refresh_public_source_balances(cx);
                    }
                    // A background pass is read again on the next one, so only a check the
                    // user asked for reports its failure.
                    Ok(Err(error)) if explicit => view.error = Some(format!("{error:#}")),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root::private_swap) fn perform_public_swap_action(
        &mut self,
        identity: super::super::model::SwapIdentity,
        action: PublicSwapAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Debug UI fixture: a synthesized record has nothing to act on.
        if self.busy() || ui_fixture::holds_records() {
            return;
        }
        let Some((chain, record)) = self
            .public_records
            .iter()
            .find(|(_, record)| record.operation() == identity.operation)
        else {
            return;
        };
        let chain = *chain;
        let Some((_, swap)) = record.public_swap_use(identity.swap_use) else {
            return;
        };
        let Some(SwapUseRole::PublicSourceDestination {
            origin_chain,
            source,
            destination_token,
            ..
        }) = record.swap_use(identity.swap_use).map(SwapUseRecord::role)
        else {
            return;
        };
        let (origin_chain, source, destination_token) =
            (*origin_chain, *source, *destination_token);
        let saved = swap.clone();
        let Some((session, owner)) = self.destination_owner(chain, cx) else {
            return;
        };
        match action {
            PublicSwapAction::CheckStatus => self.track_public_swap_request(
                chain,
                identity.operation,
                identity.swap_use,
                origin_chain,
                true,
                window,
                cx,
            ),
            PublicSwapAction::CancelPreparation => {
                match owner.cancel_public_swap(identity.operation, identity.swap_use) {
                    Ok(_) => {
                        self.public_execution = None;
                        self.refresh_public_swap_records(cx);
                    }
                    Err(error) => self.error = Some(format!("{error:#}")),
                }
                cx.notify();
            }
            PublicSwapAction::Continue | PublicSwapAction::RetrySetup | PublicSwapAction::Retry => {
                let fresh_retry = action == PublicSwapAction::Retry;
                if fresh_retry
                    && !crate::root::private_swap::public_progress::public_swap_can_retry(
                        &saved,
                        now_unix(),
                    )
                {
                    return;
                }
                self.reopen_public_swap_form(identity, fresh_retry, window, cx);
            }

            PublicSwapAction::Withdraw | PublicSwapAction::CancelOrder => {
                let Some(root) = self.root.upgrade() else {
                    return;
                };
                let (origin, http, account) = {
                    let root = root.read(cx);
                    (
                        root.effective_chain_configs.get(origin_chain).cloned(),
                        root.http.clone(),
                        root.public_accounts
                            .iter()
                            .find(|account| account.address == source)
                            .cloned(),
                    )
                };
                let (Some(origin), Some(account)) = (origin, account) else {
                    self.error =
                        Some("The Public source account or network is unavailable.".into());
                    cx.notify();
                    return;
                };
                self.review_public_recovery(
                    identity, session, owner, origin, http, account, action, window, cx,
                );
            }
            PublicSwapAction::RecoverDestination => {
                let root = self.root.clone();
                window.defer(cx, move |window, cx| {
                    let _ = root.update(cx, |root, cx| {
                        root.open_stealth_account_recovery_on(
                            chain,
                            identity.operation,
                            wallet_ops::vault::ExecutorAsset::Erc20(destination_token),
                            None,
                            window,
                            cx,
                        );
                    });
                });
            }
        }
    }

    /// Bring back the form of the saved swap `identity` for a new review, on its origin
    /// network. A fresh retry leaves the swap's use behind.
    fn reopen_public_swap_form(
        &mut self,
        identity: super::super::model::SwapIdentity,
        fresh_retry: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((record, claimed, saved)) = self.public_swap_record(identity) else {
            return;
        };
        let (
            Some(chain),
            SwapUseRole::PublicSourceDestination {
                origin_chain,
                source,
                destination_token,
                ..
            },
        ) = (self.public_record_chain(record), claimed.role())
        else {
            return;
        };
        let (origin_chain, source, destination_token) =
            (*origin_chain, *source, *destination_token);
        let saved = saved.clone();
        let Some(root) = self.root.upgrade() else {
            return;
        };
        let account = root
            .read(cx)
            .public_accounts
            .iter()
            .find(|account| account.address == source)
            .cloned();
        let Some(account) = account else {
            self.error = Some("The paying Public account is unavailable.".into());
            cx.notify();
            return;
        };
        if origin_chain != self.origin_chain_id {
            let root = self.root.clone();
            window.defer(cx, move |window, cx| {
                let _ = root.update(cx, |root, cx| {
                    root.select_chain(origin_chain, window, cx);
                    root.ensure_private_swaps(window, cx);
                    if let Some(view) = root.private_swaps_view() {
                        window.defer(cx, move |window, cx| {
                            view.update(cx, |view, cx| {
                                view.restore_public_swap_form(
                                    identity,
                                    chain,
                                    destination_token,
                                    account,
                                    &saved,
                                    fresh_retry,
                                    window,
                                    cx,
                                );
                            });
                        });
                    }
                });
            });
            return;
        }
        self.restore_public_swap_form(
            identity,
            chain,
            destination_token,
            account,
            &saved,
            fresh_retry,
            window,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn restore_public_swap_form(
        &mut self,
        identity: super::super::model::SwapIdentity,
        chain: u64,
        destination_token: Address,
        account: PublicAccountMetadata,
        saved: &wallet_ops::vault::PublicSwapRecord,
        fresh_retry: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.open_public_form(account, saved.approval().sell_token, window, cx);
        let decimals = self
            .form
            .as_ref()
            .and_then(|form| self.form_sell_metadata(form, cx))
            .map(|metadata| metadata.decimals);
        if let Some(form) = self.form.as_mut() {
            form.network = Some(chain);
            form.buy = Some(destination_token);
            form.slippage_bps = saved.approval().bounds.slippage_bps;
            form.gas_share_bps = saved
                .approval()
                .bounds
                .gas_share_bps
                .unwrap_or(GAS_SHARE_BALANCED_BPS);
            form.bridge.shield_failure = saved.approval().on_shield_failure;
            form.amount_input.update(cx, |input, cx| {
                input.set_value(
                    format_unshield_amount_input(saved.approval().bounds.sell_amount, decimals),
                    window,
                    cx,
                );
            });
            if !fresh_retry && let Some(public) = form.public.as_mut() {
                public.operation = Some(identity.operation);
                public.swap_use = Some(identity.swap_use);
            }
        }
        self.refresh_public_swap_records(cx);
        self.load_public_network_routes(chain, window, cx);
        self.refresh_destination_accounts(window, cx);
        self.refresh_setup_route(cx);
        self.schedule_public_quote(window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn review_public_recovery(
        &mut self,
        identity: super::super::model::SwapIdentity,
        session: Arc<WalletSession>,
        owner: Arc<ExecutorOwner>,
        origin: EffectiveChainConfig,
        http: wallet_ops::HttpContext,
        source: PublicAccountMetadata,
        action: PublicSwapAction,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let runtime = self.runtime.clone();
        self.public_job = Some(cx.spawn_in(window, async move |view, cx| {
            let task = runtime.spawn(async move {
                let fee = quote_public_action_gas_fee(origin.chain_id, &origin, &http).await?;
                let gas_fee = PublicActionGasFeeSelection::Custom {
                    max_fee_per_gas: fee.suggested_max_fee_per_gas,
                    max_priority_fee_per_gas: fee.suggested_max_priority_fee_per_gas,
                };
                let (command, max_gas_cost) = if action == PublicSwapAction::Withdraw {
                    let review = owner
                        .review_public_swap_withdrawal(
                            identity.operation,
                            identity.swap_use,
                            &origin,
                            fee.suggested_max_fee_per_gas,
                            fee.suggested_max_priority_fee_per_gas,
                        )
                        .await?;
                    (
                        PublicSwapCommand::Withdraw {
                            review,
                            fee: gas_fee,
                        },
                        review.max_gas_cost,
                    )
                } else {
                    let clients = public_swap_clients(&origin, &http).await?;
                    let orderbook = clients
                        .orderbook
                        .ok_or_else(|| eyre::eyre!("This origin has no orderbook."))?;
                    let gas_limit = 100_000_u64
                        .checked_add(origin.gas.gas_limit_buffer)
                        .ok_or_else(|| eyre::eyre!("The cancellation gas limit overflows."))?;
                    (
                        PublicSwapCommand::Cancel {
                            fee: gas_fee,
                            gas_limit,
                            orderbook,
                        },
                        U256::from(gas_limit) * U256::from(fee.suggested_max_fee_per_gas),
                    )
                };
                Ok::<_, eyre::Report>((source, origin, session, owner, command, max_gas_cost))
            });
            let _abort = AbortOnDrop(task.abort_handle());
            let result = task.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.public_job = None;
                match result {
                    Ok(Ok((source, origin, destination_session, owner, action, max_gas_cost))) => {
                        // The review's amounts and accounts are formatted here, with the
                        // wallet's token metadata.
                        let summary = view.public_recovery_summary(
                            &source,
                            &origin,
                            &action,
                            max_gas_cost,
                            cx,
                        );
                        let command = Arc::new(PublicSwapAuthorization {
                            source,
                            origin,
                            destination_session,
                            owner,
                            operation: identity.operation,
                            swap_use: identity.swap_use,
                            action,
                            summary: summary.clone(),
                            revision: 0,
                        });
                        view.public_authorization = Some(command.clone());
                        let entity = cx.entity();
                        let _ = view.root.update(cx, |root, cx| {
                            root.request_spend_authorization(
                                SpendAuthorizationIntent::PublicSwap(entity, command),
                                summary,
                                window,
                                cx,
                            );
                        });
                    }
                    Ok(Err(error)) => view.error = Some(format!("{error:#}")),
                    Err(_) => view.error = Some("Couldn't review this action. Try again.".into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The review of a withdrawal of the proceeds a `CoW` proxy holds, or of an order's
    /// cancellation: what moves, the accounts by name with their addresses to copy, and the
    /// most the Public account pays in gas.
    fn public_recovery_summary(
        &self,
        source: &PublicAccountMetadata,
        origin: &EffectiveChainConfig,
        action: &PublicSwapCommand,
        max_gas_cost: U256,
        cx: &App,
    ) -> SpendAuthorizationSummary {
        let label = public_source_label(source);
        let network = network_name(origin.chain_id);
        let account = |row: &'static str| {
            SpendAuthorizationSummaryRow::new(row, source.address.to_checksum(None))
                .with_copyable_account(format!("{label} "), "account address")
        };
        let gas = SpendAuthorizationSummaryRow::new(
            format!("Maximum gas on {network}"),
            origin.native_currency.format_amount(max_gas_cost),
        );
        let summary = match action {
            PublicSwapCommand::Withdraw { review, .. } => SpendAuthorizationSummary::new(
                format!("Withdraw to {label}"),
                format!(
                    "{label} sends one transaction that moves the {} from its CoW proxy to {label}. It costs gas on {network}.",
                    self.network_token_symbol(origin.chain_id, review.token, cx)
                ),
                vec![
                    SpendAuthorizationSummaryRow::new(
                        "Amount",
                        self.chain_amount(origin.chain_id, review.token, review.amount, cx),
                    ),
                    SpendAuthorizationSummaryRow::new("From", review.proxy.to_checksum(None))
                        .with_copyable_account("Your CoW proxy ", "CoW proxy address"),
                    account("Return to"),
                    gas,
                ],
            )
            .with_confirm_label("Withdraw"),
            PublicSwapCommand::Cancel { .. } | PublicSwapCommand::Swap { .. } => {
                SpendAuthorizationSummary::new(
                    "Cancel Public order",
                    format!(
                        "{label} sends one transaction that cancels the order. It costs gas on {network}. If the order fills before the cancellation confirms, the swap still goes through and is delivered. {label} can buy the same token again once the order's validity has passed."
                    ),
                    vec![account("Pay from"), gas],
                )
                .with_confirm_label("Cancel order")
            }
        };
        summary.requiring_explicit_review()
    }

    /// The form of a swap paid from a Public account, and its footer. It is the swap form with
    /// what a Public source changes: Receive to is fixed, the flip is off, and there is one
    /// stealth account, on the destination.
    pub(super) fn render_public_form(
        &self,
        cx: &Context<'_, Self>,
    ) -> (gpui::Div, Option<gpui::Div>) {
        let Some(form) = self.form.as_ref() else {
            return (app_muted_text("The swap form closed."), None);
        };
        let Some(public) = form.public.as_ref() else {
            return (app_muted_text("Choose a Public account."), None);
        };
        let review = public.review.as_ref();
        let editable = !self.busy() && public.operation.is_none();
        let sell_assets = &form.assets.sell_assets;
        let high_cost = review.and_then(|review| self.public_high_cost(form, review, cx));
        let close_settings = cx.entity();
        let footer_close_settings = close_settings.clone();
        let body = div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_4()
            // A press elsewhere in the form closes the setup broadcaster popover.
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                close_settings.update(cx, Self::close_setup_settings);
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.render_sell_panel(form, sell_assets, editable, !editable, cx))
                    .child(
                        div()
                            .relative()
                            .child(self.render_buy_panel(form, editable, !editable, cx))
                            // The tokens are on different networks, so the flip is off.
                            .child(self.render_flip(form, sell_assets, false, false, cx)),
                    ),
            )
            .children(
                review
                    .filter(|review| !review.price_verified())
                    .map(|_| Self::price_acknowledgement(form, self.busy(), cx)),
            )
            .children(Self::render_cost_acknowledgement(
                form,
                high_cost.map(public_cost_warning),
                !self.busy(),
                cx,
            ))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(labeled_row(
                        "Receive to",
                        app_text(form.network.map_or_else(
                            || "Private balance on another network".to_owned(),
                            |network| format!("Private balance on {}", network_name(network)),
                        )),
                    ))
                    .children(receive_to_problem(form, cx)),
            )
            .children(
                form.network
                    .map(|network| self.render_shield_failure_row(form, network, editable, cx)),
            )
            .child(self.render_account_row(form, FormMode::Order, editable, cx))
            .children(review.map(|review| self.render_public_details(form, review, editable, cx)))
            // The reviewed swap is being claimed. Its detail shows what follows.
            .children(
                self.public_status
                    .clone()
                    .filter(|_| self.public_job.is_some())
                    .map(|status| {
                        div()
                            .debug_selector(|| "public-swap-status".into())
                            .w_full()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Spinner::new().small())
                            .child(app_text(status).flex_1().min_w_0().whitespace_normal())
                    }),
            )
            .children(match &form.quote {
                QuoteState::Failed(error) => Some(
                    Alert::error(
                        "public-swap-quote-error",
                        public_quote_error_message(error, form.network),
                    )
                    .small()
                    .min_w_0(),
                ),
                _ => None,
            })
            .children(form.error.as_ref().map(|error| {
                // The error that names the account's CoW proxy has its address to copy.
                let proxy = public
                    .proxy_holds
                    .as_ref()
                    .filter(|(text, _)| text == error)
                    .map(|(_, proxy)| *proxy);
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_start()
                    .gap_1()
                    .child(
                        Alert::error("public-swap-form-error", error.clone())
                            .small()
                            .flex_1()
                            .min_w_0(),
                    )
                    .children(proxy.map(|proxy| {
                        clipboard_with_toast("public-swap-proxy-copy", proxy.to_checksum(None))
                            .xsmall()
                            .tooltip("Copy address")
                    }))
            }));
        let ready = review.is_some_and(|review| {
            (review.price_verified() || form.price_acknowledged)
                && (high_cost.is_none() || form.high_costs_acknowledged)
                // Debug UI fixture: its stand-in setup fee needs no estimate.
                && (self.destination_setup_chain(form).is_none()
                    || form.destination_route.offered_estimate().is_some()
                    || ui_fixture::active())
        }) && !self
            .public_form_reason(form, cx)
            .is_some_and(|reason| reason.blocks_review)
            // A share the bridge leg wasn't previewed at, or one the quote can't support.
            && !public_strip(form)
                .is_some_and(|strip| strip.pending() || strip.share_bps != form.gas_share_bps);
        // Only an order is placed on CoW. A same-token deposit has no credit.
        let order = Self::public_form_path(form) != Some(PublicBridgePath::Deposit);
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                footer_close_settings.update(cx, Self::close_setup_settings);
            })
            .when(order, |footer| footer.child(powered_by_cow(cx)))
            .child(
                div()
                    .ml_auto()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        app_button("public-swap-close", "Cancel")
                            .flex_none()
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.close_swap_dialog(window, cx);
                            })),
                    )
                    .child(
                        app_button("public-swap-review", "Review…")
                            .primary()
                            .flex_none()
                            .debug_selector(|| "public-swap-review".into())
                            .disabled(self.busy() || !ready)
                            .loading(self.public_job.is_some())
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.request_public_review(window, cx);
                            })),
                    ),
            );
        (body, Some(footer))
    }

    /// The decimals of the gas strip's amounts, the destination token's, for the Minimum
    /// field.
    pub(super) fn public_strip_decimals(&self, form: &SwapForm, cx: &App) -> Option<u8> {
        self.chain_token_metadata(form.network?, form.buy?, cx)
            .map(|metadata| metadata.decimals)
    }

    /// The gas strip of an order paid from a Public account: the swap form's row of share
    /// presets and its edit button, and under it, for a reviewed order, the bar and the
    /// Minimum field while they are open, a pending bridge refresh, and why a share the quote
    /// can't support shows no minimum. Its amounts are in the destination token.
    pub(super) fn render_public_gas_strip(
        &self,
        form: &SwapForm,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let strip = public_strip(form);
        let order = form
            .public
            .as_ref()
            .and_then(|public| public.review.as_ref())
            .filter(|_| strip.is_some())
            .map(|review| (review, review.intent().bridged_token));
        // A quote that fell back from the form's share selects no preset.
        let fallback = strip.is_some_and(|strip| strip.share_bps != form.gas_share_bps);
        let selected = if form.gas_custom || fallback {
            None
        } else {
            GasPreset::of_share(strip.map_or(form.gas_share_bps, |strip| strip.share_bps))
        };
        let estimate_money = strip
            .zip(order)
            .map(|(strip, (_, bridged))| self.gas_money(bridged, strip.limit.gas_estimate, cx));
        let row = Self::render_gas_strip_row(
            form,
            selected,
            |preset| {
                let Some((review, bridged)) = order else {
                    return (false, None);
                };
                // A preset that would leave no positive minimum can't be chosen, and one whose
                // minimum solvers are unlikely to accept carries a warning.
                review.order_limit_at(preset.share_bps()).map_or_else(
                    |_| (true, None),
                    |limit| {
                        (
                            false,
                            self.fill_warning_for(
                                bridged,
                                limit.gas_estimate,
                                limit.best_case,
                                review.slippage_bps(),
                                limit.gas_allowance,
                                cx,
                            ),
                        )
                    },
                )
            },
            estimate_money.as_deref(),
            strip.is_some(),
            self.render_gas_help(form, None, cx),
            editable,
            cx,
        );
        let (Some(strip), Some((review, bridged)), Some(network), Some(buy)) =
            (strip, order, form.network, form.buy)
        else {
            return row;
        };
        let open = form.gas_bar_open();
        let bar = open.then(|| {
            // The bar starts at the minimum if the account's order pays all gas, or at zero
            // when the gas exceeds the swap, and ends at the best case.
            let (start, label) = review
                .order_limit_at(GAS_SHARE_LOOSE_BPS)
                .ok()
                .filter(|_| !strip.bar.gas_exceeds())
                .map_or((U256::ZERO, "gas exceeds the swap"), |loose| {
                    (loose.min_received, "fills most easily")
                });
            Self::render_gas_bar(
                form,
                strip.bar,
                strip.share_bps,
                (
                    self.network_bare_amount(network, buy, strip.scale.show(start), cx),
                    label,
                    self.network_bare_amount(
                        network,
                        buy,
                        review.best_case().unwrap_or_default(),
                        cx,
                    ),
                ),
                editable,
                cx,
            )
        });
        let minimum_field = open.then(|| {
            let usd = self
                .network_usd_micro_value(
                    network,
                    buy,
                    strip.scale.show(strip.limit.min_received),
                    cx,
                )
                .map(|usd| format!("≈ {}", railgun_ui::format_usd_micro_value(usd)));
            Self::render_gas_minimum_field(
                form,
                self.network_token_symbol(network, buy, cx),
                usd,
                editable,
                cx,
            )
        });
        let refresh = strip
            .pending()
            .then(|| self.render_gas_pending(form, editable, cx));
        let warning = fallback.then(|| {
            div()
                .w_full()
                .min_w_0()
                .debug_selector(|| "swap-gas-too-high".into())
                .child(
                    Alert::warning(
                        "swap-gas-too-high",
                        self.gas_too_high_message(bridged, strip.limit.gas_estimate, cx),
                    )
                    .small()
                    .min_w_0(),
                )
        });
        row.children(bar)
            .children(minimum_field)
            .children(refresh)
            .children(warning)
    }

    /// The details section of a swap paid from a Public account: collapsed, an order's rate
    /// or that Across bridges the deposit, and the total costs; open, each cost and, for an
    /// order, the price check and the price tolerance.
    fn render_public_details(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        editable: bool,
        cx: &Context<'_, Self>,
    ) -> Collapsible {
        let open = form.details_open;
        let headline = if review.intent().order {
            self.public_rate_label(form, review, cx)
                .unwrap_or_else(|| "Unavailable".into())
        } else {
            "Bridged by Across".to_owned()
        };
        let details = Self::details_header(
            open,
            headline,
            self.public_costs_total(form, review, cx)
                .map(|total| format!("{total} in costs")),
            cx,
        );
        if !open {
            return details;
        }
        details.content(
            div().w_full().min_w_0().flex().flex_col().gap_2().children(
                self.public_details(form, review, cx)
                    .into_iter()
                    .map(|row| {
                        let value = match row.value {
                            PublicDetailValue::Text { value, note } => div()
                                .flex()
                                .items_baseline()
                                .gap_1()
                                .child(app_text(value))
                                .children(note.map(app_muted_text))
                                .into_any_element(),
                            PublicDetailValue::Price(delta) => delta.value(cx).into_any_element(),
                            PublicDetailValue::Tolerance => {
                                Self::render_slippage(form, editable, cx).into_any_element()
                            }
                        };
                        detail_row(row.label, value, row.indented, row.help)
                    }),
            ),
        )
    }

    /// "1 DAI = 0.9996 USDC" at an order's quoted trading rate, excluding explicit fees.
    fn public_rate_label(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Option<String> {
        let sell = self.form_sell_metadata(form, cx)?;
        let bought = review.intent().bridged_token;
        let quote = review.quote()?;
        let rate = unit_rate(quote.sell_amount, sell.decimals, quote.buy_amount)?;
        Some(format!(
            "1 {} = {} {}",
            sell.symbol,
            railgun_ui::format_token_amount(rate, self.token_decimals(bought, cx)?),
            self.token_symbol(bought, cx)
        ))
    }

    /// "≈ $8.20": every cost of the details section in money. `None` while any of them has no
    /// cached rate, as they are in three tokens.
    fn public_costs_total(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Option<String> {
        let (network, buy) = (form.network?, form.buy?);
        let bridged = review.intent().bridged_token;
        let bridge = review.bridge();
        let bounds = public_bounds(review);
        let on_origin = bounds
            .and_then(|bounds| bounds.gas_allowance)
            .unwrap_or_default()
            .saturating_add(bridge.fee.unwrap_or_default());
        let on_destination = bridge
            .private
            .map_or(U256::ZERO, |private| private.delivery_allowance)
            .saturating_add(
                bridge
                    .destination_minimum
                    .saturating_sub(bridge.received_minimum()),
            );
        let value = |chain: u64, token: Address, amount: U256| {
            if amount.is_zero() {
                Some(U256::ZERO)
            } else {
                self.network_usd_micro_value(chain, token, amount, cx)
            }
        };
        let total = value(self.origin_chain_id, bridged, on_origin)?
            .saturating_add(value(network, buy, on_destination)?)
            .saturating_add(value(
                self.origin_chain_id,
                Address::ZERO,
                review.gas_plan().max_gas_cost,
            )?);
        Some(format!("≈ {}", railgun_ui::format_usd_micro_value(total)))
    }

    /// The rows of the details section, in order: an order's gas, then what the bridge, the
    /// delivery and the shield take, the Public account's own gas, and for an order the price
    /// check and the price tolerance.
    pub(super) fn public_details(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Vec<PublicDetail> {
        let Some((public, network, buy)) = form
            .public
            .as_ref()
            .zip(form.network)
            .zip(form.buy)
            .map(|((public, network), buy)| (public, network, buy))
        else {
            return Vec::new();
        };
        let order = review.intent().order;
        let bridged = review.intent().bridged_token;
        let bridge = review.bridge();
        let destination = network_name(network);
        let label = public_source_label(&public.account);
        let native = self.token_symbol(Address::ZERO, cx);
        let step = if order {
            "order is placed"
        } else {
            "deposit is sent"
        };
        let mut rows = Vec::new();
        if let Some(fee) = review.cow_fee() {
            rows.push(PublicDetail::cost(
                "CoW fee",
                self.token_amount(bridged, fee, cx),
                None,
                "CoW Protocol's fee, already out of the quote.".to_owned(),
            ));
        }
        if let Some(bounds) = public_bounds(review) {
            if let (Some(price), Some(estimate)) = (bounds.gas_price_wei, bounds.gas_estimate) {
                rows.push(PublicDetail::cost(
                    format!("Gas at {} gwei", format_gwei(price)),
                    format!("≈ {}", self.gas_money(bridged, estimate, cx)),
                    None,
                    "Gas for the trade and the bridge deposit, at the current network gas price with a 25% cushion. Solvers may charge less.".to_owned(),
                ));
            }
            // At the share the gas strip shows, as the Buy card's minimum is.
            if let Some(strip) = public_strip(form) {
                rows.push(PublicDetail {
                    label: "Gas you pay".to_owned(),
                    value: PublicDetailValue::Text {
                        value: format!(
                            "up to {}",
                            self.gas_money(bridged, strip.limit.gas_allowance, cx)
                        ),
                        note: Some(format_bps_percent(u64::from(strip.share_bps))),
                    },
                    indented: true,
                    help: None,
                });
            }
        }
        if let Some(fee) = bridge.fee {
            rows.push(PublicDetail::cost(
                "Across fee",
                self.token_amount(bridged, fee, cx),
                None,
                "Across's fee, already out of the amount you receive.".to_owned(),
            ));
        }
        if let Some(private) = bridge.private {
            let percent = format_bps_percent(
                u64::try_from(private.destination_shield_fee_bps).unwrap_or(u64::MAX),
            );
            rows.push(PublicDetail::cost(
                format!("Delivery on {destination}"),
                format!(
                    "≈ {}",
                    self.network_token_amount(network, buy, private.delivery_allowance, cx)
                ),
                None,
                format!(
                    "What shielding on delivery costs the relayer in gas on {destination}. An estimate, checked against Across's own quote when the {step}."
                ),
            ));
            // An order's amount is a minimum, so its shield fee is a rate. A deposit's is exact.
            let (value, note) = if order {
                (percent, "of what you receive".to_owned())
            } else {
                (
                    self.network_token_amount(
                        network,
                        buy,
                        bridge
                            .destination_minimum
                            .saturating_sub(bridge.received_minimum()),
                        cx,
                    ),
                    percent,
                )
            };
            rows.push(PublicDetail::cost(
                format!("Railgun shield on {destination}"),
                value,
                Some(note),
                format!(
                    "Railgun's fee on shielding the delivered tokens to your private balance on {destination}."
                ),
            ));
        }
        let max_gas = review.gas_plan().max_gas_cost;
        // An allowance that already covers the amount needs no approval, as the review says.
        let approves = !review.gas_plan().approval_gas_limits.is_empty();
        let up_to = format!("up to {}", self.token_amount(Address::ZERO, max_gas, cx));
        let usd = self.usd_label(Address::ZERO, max_gas, cx);
        rows.push(match (approves, order) {
            (true, true) => PublicDetail::cost(
                format!("Approval from {label}"),
                up_to,
                usd,
                format!(
                    "The most {label} pays in {native} for its approval. The swap and the bridge deposit are sent by a CoW solver."
                ),
            ),
            (false, true) => PublicDetail::cost(
                format!("Approval from {label}"),
                "not needed".to_owned(),
                None,
                format!(
                    "{label} already lets CoW take the amount, so it sends nothing. The swap and the bridge deposit are sent by a CoW solver."
                ),
            ),
            (true, false) => PublicDetail::cost(
                format!("Approval and deposit from {label}"),
                up_to,
                usd,
                format!("The most {label} pays in {native} for its approval and the deposit."),
            ),
            (false, false) => PublicDetail::cost(
                format!("Deposit from {label}"),
                up_to,
                usd,
                format!("The most {label} pays in {native} for the deposit."),
            ),
        });
        if order {
            rows.extend(
                checked_price_delta(
                    review.price(),
                    review
                        .quote()
                        .map(|quote| (quote.sell_amount, quote.buy_amount)),
                )
                .map(|delta| PublicDetail {
                    label: if delta.anchor == "Chainlink" {
                        "Price vs Chainlink".to_owned()
                    } else {
                        "Price vs anchor".to_owned()
                    },
                    help: delta.checked.clone(),
                    value: PublicDetailValue::Price(delta),
                    indented: false,
                }),
            );
            rows.push(PublicDetail {
                label: "Price tolerance".to_owned(),
                value: PublicDetailValue::Tolerance,
                indented: false,
                help: None,
            });
        }
        rows
    }

    /// The Buy card's label. A same-token deposit is received as it is, so its card reads
    /// "Receive". An order's amount, once `quoted`, is a minimum, which the label says.
    pub(super) fn public_buy_label(form: &SwapForm, quoted: bool) -> String {
        let deposit = Self::public_form_path(form) == Some(PublicBridgePath::Deposit);
        match form.network.map(network_name) {
            None => "Buy on another network".to_owned(),
            Some(network) if deposit => format!("Receive on {network}"),
            Some(network) if quoted => format!("Buy on {network}, at least"),
            Some(network) => format!("Buy on {network}"),
        }
    }

    /// The path the form's pair takes: its review's, or before one its route's.
    pub(super) fn public_form_path(form: &SwapForm) -> Option<PublicBridgePath> {
        let public = form.public.as_ref()?;
        public
            .review
            .as_ref()
            .map(|review| review.path())
            .or_else(|| public_route(form).map(|route| route.path))
    }

    /// The Public account's native balance and what `review` needs of it, when it has less:
    /// its gas maximum, and the amount of a native Sell asset. `None` too while the balance
    /// isn't read.
    fn public_native_shortfall(
        &self,
        form: &SwapForm,
        review: &PublicSwapReview,
        cx: &App,
    ) -> Option<(U256, U256)> {
        let public = form.public.as_ref()?;
        let required =
            review
                .gas_plan()
                .max_gas_cost
                .saturating_add(if form.sell == Address::ZERO {
                    review.sell_amount()
                } else {
                    U256::ZERO
                });
        if required.is_zero() {
            return None;
        }
        let root = self.root.upgrade()?;
        // A balance that wasn't read isn't a shortfall: the reason states what the account has.
        let held = crate::root::public_balances::public_account_balances_for_chain(
            root.read(cx).public_balance_snapshot.as_deref(),
            self.origin_chain_id,
            &public.account.public_account_uuid,
            PublicAccountStatus::Active,
        )?
        .iter()
        .find(|balance| balance.asset.id == wallet_ops::PublicAssetId::Native)?
        .amount
        .amount()?;
        (held < required).then_some((required, held))
    }

    /// "Main needs 0.0004 ETH on Ethereum for gas and has 0.0001."
    fn public_shortfall_text(
        &self,
        form: &SwapForm,
        (required, held): (U256, U256),
        cx: &App,
    ) -> String {
        format!(
            "{} needs {} on {} for {} and has {}.",
            form.public
                .as_ref()
                .map_or_else(String::new, |public| public_source_label(&public.account)),
            self.token_amount(Address::ZERO, required, cx),
            self.chain_label(),
            if form.sell == Address::ZERO {
                "the amount and gas"
            } else {
                "gas"
            },
            self.bare_amount(Address::ZERO, held, cx)
        )
    }

    /// "Main's `CoW` proxy 0x5cD8…a310 holds 994.47 USDC. Withdraw it to Main first, then review
    /// the swap again. No order was placed."
    fn public_proxy_holds_text(
        &self,
        command: &PublicSwapAuthorization,
        proxy: Address,
        balance: Option<U256>,
        cx: &App,
    ) -> String {
        let label = public_source_label(&command.source);
        let held = match &command.action {
            PublicSwapCommand::Swap { review, .. } => {
                let token = review.intent().bridged_token;
                balance.map_or_else(
                    || format!("an unreadable balance of {}", self.token_symbol(token, cx)),
                    |balance| self.token_amount(token, balance, cx),
                )
            }
            PublicSwapCommand::Withdraw { .. } | PublicSwapCommand::Cancel { .. } => {
                "an unreadable balance".to_owned()
            }
        };
        format!(
            "{label}'s CoW proxy {} holds {held}. Withdraw it to {label} first, then review the swap again. No order was placed.",
            railgun_ui::short_address(&proxy)
        )
    }

    /// Why the form can't quote or be reviewed, one reason at a time: an open swap the
    /// backend refused the pair for, a native Sell asset that would need a swap, a destination
    /// whose private balance isn't ready, a pair no route serves, an amount too small to
    /// bridge, and a Public account short of gas. Informational only: the wrap suggestion and
    /// a network without swaps.
    pub(super) fn public_form_reason(&self, form: &SwapForm, cx: &App) -> Option<PublicFormReason> {
        let public = form.public.as_ref()?;
        let label = public_source_label(&public.account);
        if let Some(conflict) = public
            .conflict
            .filter(|conflict| conflict.sell == form.sell && conflict.buy == form.buy)
        {
            let token = self.token_symbol(conflict.token, cx);
            let until = conflict.until.map(local_time_label);
            let text = match (conflict.buys, until) {
                (true, Some(until)) => format!(
                    "{label} already has an open swap that buys {token}. It can buy {token} again from {until}."
                ),
                (true, None) => format!("{label} already has an open swap that buys {token}."),
                (false, Some(until)) => format!(
                    "{label} already has an open order that sells {token}. It expires at {until}."
                ),
                (false, None) => {
                    format!("{label} already has an open order that sells {token}.")
                }
            };
            return Some(PublicFormReason {
                text,
                blocks_review: true,
                open: self.public_records.iter().find_map(|(_, record)| {
                    record.public_swap_use(conflict.swap).map(|_| {
                        super::super::model::SwapIdentity {
                            operation: record.operation(),
                            swap_use: conflict.swap,
                        }
                    })
                }),
            });
        }
        let root = self.root.upgrade()?;
        let origin = root
            .read(cx)
            .effective_chain_configs
            .get(self.origin_chain_id)
            .cloned()?;
        let blocking = |text: String| PublicFormReason {
            text,
            blocks_review: true,
            open: None,
        };
        // The routes of the form's sell token to its network, once they are read, and whether
        // one of them delivers the picked Buy token.
        let pair = form.network.zip(form.buy);
        let routes = form
            .network
            .and_then(|network| public.routes.get(&(form.sell, network)));
        let unrouted = pair.zip(routes).is_some_and(|((_, buy), routes)| {
            !routes
                .iter()
                .any(|route| route.destination.destination_token == buy)
        });
        // Where orders aren't available, by the chain's profile or because its math contract
        // isn't deployed, only deposits are offered.
        let can_swap = Self::public_can_swap(&origin, public);
        // A native Sell asset is only deposited, as its wrapped token: no order sells it.
        let native = form.sell == Address::ZERO && can_swap;
        let wrap = |advice: &str| {
            format!(
                "{} can't be swapped from a Public account{advice}",
                origin.native_currency.symbol
            )
        };
        let wrapped = origin.wrapped_native_token.map_or_else(
            || "its wrapped token".to_owned(),
            |token| self.token_symbol(token, cx),
        );
        if native && unrouted {
            return Some(blocking(wrap(&format!(
                ". Wrap it first, or pick {wrapped} on the destination to bridge it."
            ))));
        }
        if let Some((network, buy)) = pair {
            if let Some(problem) = self.public_destination_problem(network, cx) {
                return Some(blocking(problem));
            }
            if unrouted {
                return Some(blocking(format!(
                    "Across doesn't deliver {} on {} for {} from {}. Pick another token.",
                    self.network_token_symbol(network, buy, cx),
                    network_name(network),
                    self.form_sell_metadata(form, cx).map_or_else(
                        || self.token_symbol(form.sell, cx),
                        |metadata| metadata.symbol
                    ),
                    self.chain_label()
                )));
            }
            if routes.is_none()
                && let Some(error) = public.route_errors.get(&(form.sell, network))
            {
                return Some(blocking(format!(
                    "Couldn't read what Across delivers on {}. {error}",
                    network_name(network)
                )));
            }
        }
        // Across's least deposit with the shield's gas, estimated from the review's own quote.
        if let Some((review, network)) = public.review.as_ref().zip(form.network)
            && let Some(floor) = review.too_small_to_bridge()
        {
            let network = network_name(network);
            // An order's floor is in the bridged token, which the form doesn't show.
            return Some(blocking(match review.path() {
                PublicBridgePath::Deposit => format!(
                    "{} is too small to bridge to your private balance on {network}. The shield's gas there would take too large a share. Try at least about {}.",
                    self.form_sell_amount(form, review.sell_amount(), cx),
                    self.form_sell_amount(form, floor, cx)
                ),
                PublicBridgePath::Order => format!(
                    "This amount is too small to bridge to your private balance on {network}. The shield's gas there would take too large a share. Try a larger amount."
                ),
            }));
        }
        if let Some(shortfall) = public
            .review
            .as_ref()
            .and_then(|review| self.public_native_shortfall(form, review, cx))
        {
            return Some(blocking(self.public_shortfall_text(form, shortfall, cx)));
        }
        // No Buy token is picked yet: the suggestion only informs. A picked one that a deposit
        // serves needs none.
        if native && form.buy.is_none() {
            return Some(PublicFormReason {
                text: wrap(&format!(
                    ". Wrap it first, or pick {wrapped} on the destination to bridge it."
                )),
                blocks_review: false,
                open: None,
            });
        }
        // Said only once it is known: a chain not read yet may still take orders.
        let no_orders =
            origin.public_swap_profile().is_none() || public.orders_available == Some(false);
        (origin.bridge_origin_profile().is_some() && no_orders).then(|| PublicFormReason {
            text: format!(
                "From {}, a token can only be bridged as itself, such as USDC to USDC. Swapping to a different token isn't supported on this network.",
                self.chain_label()
            ),
            blocks_review: false,
            open: None,
        })
    }

    /// The owner on `network` that a quote for a delivery there reads. A network that was
    /// ready and syncs again still quotes. Reviewing a swap needs it ready.
    fn public_quote_destination(
        &self,
        network: u64,
        cx: &mut Context<'_, Self>,
    ) -> Option<Arc<ExecutorOwner>> {
        if self.public_synced.contains(&network)
            && let Some((_, owner)) = self.destination_owner(network, cx)
        {
            return Some(owner);
        }
        self.ready_destination(network, cx)
            .ok()
            .map(|(_, owner)| owner)
    }

    /// Why the private balance on `network` can't take the delivery yet, which keeps the form
    /// from quoting: the conditions of [`Self::ready_destination`], read without loading
    /// anything.
    fn public_destination_problem(&self, network: u64, cx: &App) -> Option<String> {
        let name = network_name(network);
        let root = self.root.upgrade()?;
        Some(match root.read(cx).chain_states.get(&network) {
            Some(ChainUtxoState::Ready { session, .. }) => {
                if session.executor_owner().is_some() {
                    return None;
                }
                format!("Stealth accounts aren't available on {name} for this wallet.")
            }
            // A network that was ready is syncing again, as it does from time to time.
            Some(ChainUtxoState::Syncing { session, .. })
                if self.public_synced.contains(&network) && session.executor_owner().is_some() =>
            {
                return None;
            }
            Some(ChainUtxoState::Error { .. }) => format!(
                "Private balance needs {name} synced, and its sync failed. Pick a token on another network."
            ),
            _ => format!(
                "Private balance needs {name} synced first. Wait for it, or pick a token on another network."
            ),
        })
    }

    /// A reason under the Sell card, with "Open it…" for the open swap it names.
    pub(super) fn render_public_form_reason(
        &self,
        reason: PublicFormReason,
        cx: &Context<'_, Self>,
    ) -> gpui::Div {
        let open = reason.open.map(|identity| {
            app_button("swap-public-open-swap", "Open it…")
                .debug_selector(|| "swap-public-open-swap".into())
                .outline()
                .small()
                .flex_none()
                .disabled(self.busy())
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.navigate(SwapDialogView::PublicDetail(identity), window, cx);
                }))
        });
        let line = div()
            .debug_selector(|| "swap-public-reason".into())
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2();
        if reason.blocks_review {
            return line.child(match open {
                Some(open) => action_alert("swap-public-reason-text", reason.text, open, cx),
                None => error_alert("swap-public-reason-text", reason.text),
            });
        }
        line.child(
            // A limit of the source, not a problem: it keeps nothing from being reviewed.
            div()
                .debug_selector(|| "swap-public-reason-text".into())
                .flex_1()
                .min_w_0()
                .child(
                    Alert::warning("swap-public-reason-text", reason.text)
                        .small()
                        .min_w_0(),
                ),
        )
        .children(open)
    }

    /// The line under the stealth account select: what reusing the chosen account links, a
    /// new account's setup fee and where it is paid from, or that a continued swap's account
    /// is on its way.
    pub(super) fn render_public_account_line(&self, form: &SwapForm, cx: &App) -> gpui::Div {
        if let Some(account) = form.destination_choice() {
            return reuse_line(
                "swap-destination-reuse",
                format!(
                    "Reuses #{} on {} and links this swap to its earlier activity.",
                    account.index,
                    network_name(account.chain_id)
                ),
                account.address,
            );
        }
        // Without a network there is no account to describe yet.
        if form.network.is_none() {
            return div();
        }
        if self.destination_setup_chain(form).is_some() {
            // Debug UI fixture: its stand-in setup fee, as an estimate's line reads.
            if let Some(setup) = self.ui_fixture_setup(form, cx) {
                return setup_fee_line(
                    format!(
                        "Setup ≈ {} on {}, from your private balance there",
                        self.chain_amount(setup.chain_id, setup.token, setup.maximum, cx),
                        network_name(setup.chain_id)
                    ),
                    false,
                    cx,
                );
            }
            let (line, problem) = self.setup_line(form, cx);
            let line = if !problem && form.destination_route.estimate.is_some() {
                format!("{line}, from your private balance there")
            } else {
                line
            };
            return setup_fee_line(line, problem, cx);
        }
        app_muted_text(match self.public_destination_setup_progress(form) {
            Some(SwapSetupProgress::Done) | None => "Already set up for this swap. No setup fee.",
            Some(_) => {
                "Setting up the swap's stealth account · Waiting for the setup to be confirmed"
            }
        })
        .text_xs()
        .whitespace_normal()
    }

    /// The two failure choices of a swap paid from a Public account, each with what it does
    /// with the deposit: refunded to the account in the token it deposited, or kept on
    /// `network` in the token delivered there.
    pub(super) fn public_failure_paragraphs(
        &self,
        form: &SwapForm,
        network: u64,
        cx: &App,
    ) -> [(String, String); 2] {
        let (origin, destination) = (self.chain_label(), network_name(network));
        let label = form
            .public
            .as_ref()
            .map_or_else(String::new, |public| public_source_label(&public.account));
        let (deposited, delivered) = public_route(form).map_or_else(
            || ("tokens".to_owned(), "tokens".to_owned()),
            |route| {
                (
                    self.token_symbol(route.destination.intermediate, cx),
                    route.destination.symbol.clone(),
                )
            },
        );
        [
            (
                self.form_failure_label(form, BridgeShieldFailure::RefundOnOrigin, network),
                format!(
                    "The delivery doesn't happen. Across returns the {deposited} to {label} on {origin} after the deposit expires, usually within a few hours. Nothing needs recovering."
                ),
            ),
            (
                self.form_failure_label(form, BridgeShieldFailure::KeepOnDestination, network),
                format!(
                    "The delivery happens and the {delivered} stays in the stealth account on {destination}, publicly visible, until you recover it there. Relayers price this option less precisely, so the deposit is more likely to go unfilled."
                ),
            ),
        ]
    }
}

impl PublicSwapConflict {
    /// The open swap a backend refusal names, for the draft `form` that `command` reviewed.
    fn of(
        error: &eyre::Report,
        command: &PublicSwapAuthorization,
        form: &SwapForm,
    ) -> Option<Self> {
        let PublicSwapCommand::Swap {
            review, approval, ..
        } = &command.action
        else {
            return None;
        };
        Self::naming(
            error.downcast_ref::<ExecutorStoreError>()?,
            form,
            approval.sell_token,
            review.intent().bridged_token,
        )
    }

    /// The open swap `refusal` names, for the draft `form` that sells `sell_token` and buys
    /// `bridged` on the network it is paid on.
    const fn naming(
        refusal: &ExecutorStoreError,
        form: &SwapForm,
        sell_token: Address,
        bridged: Address,
    ) -> Option<Self> {
        let (swap, token, buys, until) = match refusal {
            ExecutorStoreError::PublicSwapBuysSameToken {
                swap, available_at, ..
            } => (*swap, bridged, true, *available_at),
            ExecutorStoreError::PublicSwapSellsSameToken {
                swap, expires_at, ..
            } => (*swap, sell_token, false, *expires_at),
            _ => return None,
        };
        Some(Self {
            sell: form.sell,
            buy: form.buy,
            swap,
            token,
            buys,
            until,
        })
    }
}

/// The route of the form's pair, once its network's routes are read.
/// A failed quote of a swap paid from a Public account, in the form's words. An amount that
/// the bridge or the shield's gas on `network` rules out says so and what to do.
fn public_quote_error_message(error: &eyre::Report, network: Option<u64>) -> String {
    let there = network.map_or_else(|| "the destination network".to_owned(), network_name);
    if matches!(
        error.downcast_ref::<OrderLimitError>(),
        Some(OrderLimitError::DeliveryAllowanceExceedsOutput { .. })
    ) {
        return format!(
            "This amount is too small to bridge to your private balance on {there}. The shield's gas there costs more than the amount. Try a larger amount."
        );
    }
    if matches!(
        error.downcast_ref::<BridgeApiError>(),
        Some(BridgeApiError::AmountTooLow)
    ) {
        return format!(
            "This amount is too small for Across to bridge to {there}. Try a larger amount."
        );
    }
    format!("{error:#}")
}

fn public_route(form: &SwapForm) -> Option<&PublicBridgeDestination> {
    let buy = form.buy?;
    form.public
        .as_ref()?
        .routes
        .get(&(form.sell, form.network?))?
        .iter()
        .find(|route| route.destination.destination_token == buy)
}

/// The bounds `review` binds, whichever account it is approved for: its gas share,
/// estimate, allowance and price, and its validity.
fn public_bounds(review: &PublicSwapReview) -> Option<SwapApprovedBounds> {
    review
        .approval(
            SwapApprovedAccount {
                address: None,
                setup: false,
            },
            None,
            true,
        )
        .ok()
        .map(|approval| approval.bounds)
}

/// The warning on a swap `bps` of which may go to costs.
fn public_cost_warning(bps: u64) -> AuthorizedCostWarning {
    AuthorizedCostWarning {
        headline: format!("{}% of this swap may go to costs.", bps / 100),
        details: "The received minimum accounts for bridge, destination gas and shield costs. Source gas and any destination setup fee are paid separately.".into(),
    }
}

/// "Sign on Ledger…": the hardware review's button, which opens the device's prompts.
pub(super) fn public_sign_label(device: &str) -> String {
    format!("Sign on {device}…")
}

enum PublicExecutionResult {
    /// The swap's record exists, and nothing was prepared or sent for it yet.
    Claimed(PublicSwapExecution),
    Waiting(PublicSwapExecution),
    Ready(PublicSwapExecution),
    Changed(SwapReviewChange),
    /// The account's `CoW` proxy holds the bought token, or its balance there couldn't be read:
    /// no order was placed.
    ProxyHolds {
        proxy: Address,
        balance: Option<U256>,
    },
    Finished(super::super::model::SwapIdentity),
}

/// Debug UI fixture: the scripted answer that stands in for a real job of `execution`.
#[cfg(debug_assertions)]
async fn ui_fixture_job(
    execution: PublicSwapExecution,
    (delay, step): (std::time::Duration, ui_fixture::Step),
) -> eyre::Result<PublicExecutionResult> {
    use ui_fixture::Step;
    tokio::time::sleep(delay).await;
    Ok(match step {
        Step::Hold => std::future::pending().await,
        Step::Claimed => PublicExecutionResult::Claimed(execution),
        Step::Waiting => PublicExecutionResult::Waiting(execution),
        Step::Ready => PublicExecutionResult::Ready(execution),
        Step::Changed(change) => PublicExecutionResult::Changed(change),
        Step::Fail(message) => return Err(eyre::eyre!(message)),
    })
}

async fn prepare_public_execution(
    execution: PublicSwapExecution,
) -> eyre::Result<PublicExecutionResult> {
    // Debug UI fixture: a scripted answer stands in, and nothing is claimed or sent.
    #[cfg(debug_assertions)]
    {
        if let Some(script) = ui_fixture::step(ui_fixture::Phase::Approved) {
            return ui_fixture_job(execution, script).await;
        }
    }
    let command = &execution.command;
    match &command.action {
        PublicSwapCommand::Swap {
            account,
            destination_token,
            review,
            approval,
            ..
        } => {
            let record = command.owner.claim_public_swap(PublicSwapUseClaim {
                id: command.swap_use,
                origin_chain: command.origin.chain_id,
                source: command.source.address,
                source_scope: command.source.scope.clone(),
                account: *account,
                destination_token: *destination_token,
                intent: review.intent(),
                approval: approval.clone(),
            })?;
            if record
                .public_swap_use(command.swap_use)
                .is_some_and(|(_, saved)| saved.approval() != approval)
            {
                command.owner.reapprove_public_swap(
                    command.operation,
                    command.swap_use,
                    approval.clone(),
                )?;
            }
            Ok(PublicExecutionResult::Claimed(execution))
        }
        PublicSwapCommand::Withdraw { review, fee } => {
            let result = command
                .owner
                .submit_public_swap_withdrawal(
                    command.operation,
                    command.swap_use,
                    &command.origin,
                    &execution.signer,
                    *fee,
                    review,
                    true,
                    &mut |_| {},
                )
                .await?;
            if matches!(
                result,
                wallet_ops::PublicSwapTransactionOutcome::Reverted { .. }
            ) {
                return Err(eyre::eyre!(
                    "The withdrawal reverted. Check status before retrying."
                ));
            }
            Ok(PublicExecutionResult::Finished(
                super::super::model::SwapIdentity {
                    operation: command.operation,
                    swap_use: command.swap_use,
                },
            ))
        }
        PublicSwapCommand::Cancel {
            fee,
            gas_limit,
            orderbook,
        } => {
            let result = command
                .owner
                .submit_public_swap_invalidation(
                    command.operation,
                    command.swap_use,
                    &command.origin,
                    &execution.signer,
                    *fee,
                    orderbook,
                    *gas_limit,
                    &mut |_| {},
                )
                .await?;
            if matches!(
                result,
                wallet_ops::PublicSwapTransactionOutcome::Reverted { .. }
            ) {
                return Err(eyre::eyre!(
                    "The order cancellation reverted. Check status before retrying."
                ));
            }
            Ok(PublicExecutionResult::Finished(
                super::super::model::SwapIdentity {
                    operation: command.operation,
                    swap_use: command.swap_use,
                },
            ))
        }
    }
}

/// What follows a swap's claim: its destination account is prepared and, when that gives a
/// setup, the setup is sent. The swap then waits for its destination.
async fn prepare_public_destination(
    execution: PublicSwapExecution,
) -> eyre::Result<PublicExecutionResult> {
    // Debug UI fixture: a scripted answer stands in, and nothing is prepared or sent.
    #[cfg(debug_assertions)]
    {
        if let Some(script) = ui_fixture::step(ui_fixture::Phase::Claimed) {
            return ui_fixture_job(execution, script).await;
        }
    }
    let command = &execution.command;
    let PublicSwapCommand::Swap {
        account,
        candidate,
        approval,
        waku,
        ..
    } = &command.action
    else {
        return Err(eyre::eyre!("This action has no destination delivery."));
    };
    // A resumed fresh account whose setup is pending or confirmed is not prepared
    // or sent again. Its retained use continues from canonical setup observation.
    if candidate.is_some() || matches!(account, SwapAccountChoice::Existing(_)) {
        let side = command
            .owner
            .prepare_public_swap_destination(
                command.operation,
                command.swap_use,
                candidate.clone(),
                execution
                    .private_authorization
                    .as_ref()
                    .expect("destination authorization"),
            )
            .await?;
        if let SwapPairSide::Setup(prepared) = side {
            let outcome = command
                .owner
                .submit_swap_setup(
                    &prepared,
                    SwapSetupRequest {
                        maximum_private_fee: approval
                            .bounds
                            .destination_setup_fee
                            .ok_or_else(|| eyre::eyre!("The review has no setup fee ceiling."))?,
                        session: command.destination_session.clone(),
                        authorization: execution
                            .private_authorization
                            .as_ref()
                            .expect("destination authorization"),
                        waku: waku
                            .clone()
                            .ok_or_else(|| eyre::eyre!("The broadcaster network isn't ready."))?,
                        verify_proof: true,
                        progress_tx: None,
                        response_timeout: SWAP_BROADCASTER_RESPONSE_TIMEOUT,
                        republish_interval: SWAP_BROADCASTER_REPUBLISH_INTERVAL,
                    },
                )
                .await?;
            if let PublicBroadcasterResultKind::Failed { error } = outcome.result {
                return Err(eyre::eyre!(
                    "The broadcaster couldn't submit the destination setup: {error}"
                ));
            }
        }
    }
    Ok(PublicExecutionResult::Waiting(execution))
}

/// How often signing a delivery is tried again after a background read changed the account's
/// record under it, and how long it waits before each try.
const PUBLIC_SIGNING_RETRIES: u32 = 3;
const PUBLIC_SIGNING_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(750);

async fn advance_public_execution(
    mut execution: PublicSwapExecution,
    confirmed: u64,
) -> eyre::Result<PublicExecutionResult> {
    // Debug UI fixture: a scripted answer stands in, and nothing is read, signed or sent.
    #[cfg(debug_assertions)]
    {
        if let Some(script) = ui_fixture::step(ui_fixture::Phase::Setup) {
            return ui_fixture_job(execution, script).await;
        }
    }
    let command = &execution.command;
    let PublicSwapCommand::Swap {
        review,
        approval,
        clients,
        ..
    } = &command.action
    else {
        return Err(eyre::eyre!("This action has no destination delivery."));
    };
    let record = command
        .owner
        .records()?
        .into_iter()
        .find(|record| record.operation() == command.operation)
        .ok_or_else(|| eyre::eyre!("The destination account is unavailable."))?;
    if approval.destination.setup {
        // One read covers a bounded page, so a history longer than that is caught up page by
        // page, as the private swap's observation does.
        let mut cursor = execution
            .setup_cursor
            .or_else(|| super::super::model::swap_history_start(&record))
            .unwrap_or(confirmed);
        loop {
            let range = super::super::model::swap_observation_range(cursor, confirmed);
            let end = range.end;
            match command
                .owner
                .observe_swap_setup(command.operation, range)
                .await?
            {
                SwapSetupStatus::Pending if end > confirmed => {
                    execution.setup_cursor = Some(end);
                    return Ok(PublicExecutionResult::Waiting(execution));
                }
                SwapSetupStatus::Pending => cursor = end,
                SwapSetupStatus::Failed | SwapSetupStatus::MissingDelegation => {
                    return Err(eyre::eyre!(
                        "The destination setup failed. Review its retry before continuing."
                    ));
                }
                SwapSetupStatus::Delegated(_) => break,
            }
        }
    }
    let notes = Some(command.destination_session.as_ref() as &dyn wallet_ops::SwapShieldNotes);
    let secs = approval.bounds.valid_for_secs.unwrap_or(600);
    let valid_to = || u32::try_from(super::super::now_unix().saturating_add(u64::from(secs)));
    let (across, authorization) = (
        &clients.across,
        execution
            .private_authorization
            .as_ref()
            .expect("destination authorization"),
    );
    let input_amount = review.buy_amount().unwrap_or_else(|| review.sell_amount());
    // A background read of the account can update its record while the delivery is signed,
    // most often just after its setup confirmed. Nothing is signed then, so the account is
    // read again and the delivery signed once more.
    let sign = |valid_to: u32| async move {
        let mut retries = 0;
        loop {
            let delegated = command
                .owner
                .delegated_public_swap_destination(
                    command.operation,
                    confirmed,
                    command.swap_use,
                    notes,
                )
                .await?;
            match command
                .owner
                .sign_public_swap_delivery(PublicSwapDeliverySigning {
                    operation: command.operation,
                    swap_use: command.swap_use,
                    delegated,
                    origin: &command.origin,
                    across,
                    input_amount,
                    valid_to,
                    authorization,
                    notes,
                })
                .await
            {
                Err(error)
                    if error.is::<wallet_ops::ExecutorRecordChanged>()
                        && retries < PUBLIC_SIGNING_RETRIES =>
                {
                    retries += 1;
                    tokio::time::sleep(PUBLIC_SIGNING_RETRY_DELAY).await;
                }
                signed => break signed,
            }
        }
    };
    // Across is asked before the account pays for an approval: an amount it won't bridge, a
    // fill that fails in its simulation or changed terms stop the swap with nothing sent. The
    // delivery signed here is not used: its quote is taken again once the approvals confirm.
    if !review.gas_plan().approval_gas_limits.is_empty()
        && let PublicSwapDelivery::ReviewRequired(change) = sign(valid_to()?).await?
    {
        return Ok(PublicExecutionResult::Changed(change));
    }
    command
        .owner
        .submit_public_swap_approvals(
            command.operation,
            command.swap_use,
            &command.origin,
            &execution.signer,
            public_gas_fee(review),
            &mut |_| {},
        )
        .await?;
    let valid_to = valid_to()?;
    match sign(valid_to).await? {
        PublicSwapDelivery::ReviewRequired(change) => Ok(PublicExecutionResult::Changed(change)),
        PublicSwapDelivery::Signed {
            delivery, terms, ..
        } => {
            let nonce = new_public_swap_batch_nonce()?;
            let batch = if review.intent().order {
                Some(command.owner.public_swap_order_batch_terms(
                    command.operation,
                    command.swap_use,
                    &command.origin,
                    &delivery,
                    &terms,
                    valid_to,
                    nonce,
                )?)
            } else {
                None
            };
            execution.private_authorization = None;
            execution.signed = Some(PublicSwapSigned {
                delivery,
                terms,
                valid_to,
                nonce,
                batch,
            });
            Ok(PublicExecutionResult::Ready(execution))
        }
    }
}

async fn submit_public_execution(
    mut execution: PublicSwapExecution,
    hash_fallback_confirmed: bool,
) -> eyre::Result<PublicExecutionResult> {
    // Debug UI fixture: a scripted answer stands in, and nothing is placed or deposited.
    #[cfg(debug_assertions)]
    {
        if let Some(script) = ui_fixture::step(ui_fixture::Phase::Submit) {
            return ui_fixture_job(execution, script).await;
        }
    }
    let signed = execution
        .signed
        .take()
        .ok_or_else(|| eyre::eyre!("Review the signed delivery first."))?;
    let command = &execution.command;
    let PublicSwapCommand::Swap {
        review, clients, ..
    } = &command.action
    else {
        return Err(eyre::eyre!("This action has no signed delivery."));
    };
    if review.intent().order {
        let orderbook = clients
            .orderbook
            .as_ref()
            .ok_or_else(|| eyre::eyre!("The origin network has no orderbook."))?;
        match command
            .owner
            .submit_public_swap_order(PublicSwapOrderRequest {
                operation: command.operation,
                swap_use: command.swap_use,
                origin: &command.origin,
                source: &execution.signer,
                orderbook,
                delivery: &signed.delivery,
                terms: &signed.terms,
                valid_to: signed.valid_to,
                batch_nonce: signed.nonce,
                quote_id: review.quote_id(),
                hash_fallback_confirmed,
            })
            .await?
        {
            PublicSwapOrderOutcome::Submitted { .. } => {}
            PublicSwapOrderOutcome::ProxyHoldsBoughtToken { proxy, balance } => {
                return Ok(PublicExecutionResult::ProxyHolds { proxy, balance });
            }
        }
    } else {
        let result = command
            .owner
            .submit_public_swap_deposit(
                command.operation,
                command.swap_use,
                &command.origin,
                &execution.signer,
                public_gas_fee(review),
                &signed.delivery,
                &signed.terms,
                &mut |_| {},
            )
            .await?;
        if matches!(
            result,
            wallet_ops::PublicSwapTransactionOutcome::Reverted { .. }
        ) {
            return Err(eyre::eyre!(
                "The Across deposit reverted. Check status before retrying."
            ));
        }
    }
    Ok(PublicExecutionResult::Finished(
        super::super::model::SwapIdentity {
            operation: command.operation,
            swap_use: command.swap_use,
        },
    ))
}
