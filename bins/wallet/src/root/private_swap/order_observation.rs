#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) struct ObservationPage {
    pub(super) operation: ExecutorOperationId,
    /// An orderbook hint or a previously verified trade locates a whole-block receipt read.
    pub(super) settlement: Option<(OrderUid, u64)>,
    /// The blocks of an order's history to read. `None` while no order exists yet: the setup
    /// is what can change, and it is read from the account at the confirmed block.
    pub(super) range: Option<std::ops::Range<u64>>,
    /// Set when `operation` is a private Bridge swap's destination stealth account, which
    /// its own network's owner observes.
    pub(super) destination: Option<DestinationPage>,
    /// The swap's route, which the page is read over.
    pub(super) route: SwapRoute,
}

impl ObservationPage {
    /// The swap the page is observed for, whose schedule it follows: a destination account's
    /// page follows its swap's.
    pub(super) fn swap(&self) -> ExecutorOperationId {
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
pub(super) struct SwapRoute {
    pub(super) http: wallet_ops::HttpContext,
    pub(super) orderbook: Arc<tokio::sync::OnceCell<CowOrderbookClient>>,
}

impl SwapRoute {
    /// The swap's orderbook client, opened through `owner`, the swap's own network's.
    pub(super) async fn orderbook(
        &self,
        owner: &ExecutorOwner,
    ) -> eyre::Result<CowOrderbookClient> {
        self.orderbook
            .get_or_try_init(|| async {
                owner.swap_orderbook_client(&self.http.operation_http_client().await?)
            })
            .await
            .cloned()
    }
}

pub(super) struct DestinationPage {
    pub(super) owner: Arc<ExecutorOwner>,
    /// The swap the destination account serves, on this view's network.
    pub(super) origin: ExecutorOperationId,
}

pub(super) struct ObservationResult {
    pub(super) operation: ExecutorOperationId,
    /// The end of the page read. `None` for a setup, which reads no page.
    pub(super) range_end: Option<u64>,
    /// A setup's observation; `None` for an order's page.
    pub(super) outcome: eyre::Result<Option<SwapSetupObservation>>,
    /// A destination account's swap.
    pub(super) destination: Option<ExecutorOperationId>,
    /// The cell of the swap's route the page was read over.
    pub(super) route: Arc<tokio::sync::OnceCell<CowOrderbookClient>>,
}

pub(super) struct HintRequest {
    pub(super) operation: ExecutorOperationId,
    pub(super) uid: OrderUid,
    /// Whether to ask the orderbook about `uid`.
    pub(super) hint: bool,
    /// Whether to read the traded order's executed fee, once per session.
    pub(super) fee: bool,
    /// The swap's own route.
    pub(super) route: SwapRoute,
    /// Handed-off Bridge orders to poll, with their destination chain. A non-EVM network has
    /// none.
    pub(super) bridges: Vec<(OrderUid, Option<EffectiveChainConfig>)>,
    /// Set when `uid` is due an early request: a Bridge order's provider, and what is known
    /// for its reported trade block.
    pub(super) early: Option<(Option<BridgeProvider>, SwapEarlyHint)>,
    /// The destination chain of a private Bridge order due an early request, when it is
    /// enabled here, where the order's fill block is read.
    pub(super) destination: Option<Box<EffectiveChainConfig>>,
}

pub(super) struct HintResult {
    pub(super) operation: ExecutorOperationId,
    pub(super) uid: OrderUid,
    /// The cell of the swap's route the request went over.
    pub(super) route: Arc<tokio::sync::OnceCell<CowOrderbookClient>>,
    /// The executed fee was asked for, whether or not the read succeeded.
    pub(super) fee: bool,
    /// Each polled Bridge order, and whether it has an outcome now.
    pub(super) bridges: Vec<(OrderUid, bool)>,
    /// The report and trade block, or `None` when the orderbook couldn't be asked.
    pub(super) report: Option<(CowOrderStatusReport, Option<u64>)>,
    /// What is known early about `uid` after this pass, for the trade block that was asked
    /// about. A failed read or request leaves what the request held.
    pub(super) early: Option<SwapEarlyHint>,
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
pub(super) async fn fetch_order_hints(
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
pub(super) async fn observe_pages(
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
