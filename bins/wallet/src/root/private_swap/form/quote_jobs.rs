#[allow(clippy::wildcard_imports)]
use super::*;

/// A setup job's work with its error as a value, so the job's completion can end the setup's
/// progress with it. `start_job` reports an error of the work itself and skips the completion.
pub(super) async fn setup_job_result<T>(
    work: impl Future<Output = eyre::Result<T>>,
) -> eyre::Result<eyre::Result<T>> {
    Ok(work.await)
}

/// One account of a reserved pair, prepared again: a new account on `side` for its setup's
/// retry, replaced when it is found used, and an existing one as it is.
pub(super) async fn resume_pair_side(
    accounts: &mut SwapAccountReplacements<'_>,
    side: SwapUseSide,
    owner: &ExecutorOwner,
    operation: ExecutorOperationId,
    candidate: Option<&PublicBroadcasterCandidate>,
    authorization: &DesktopPrivateSpendAuthorization,
    client: &OperationHttpClient,
) -> eyre::Result<SwapPairSide> {
    match candidate {
        Some(candidate) => Box::pin(accounts.resume_setup(side, candidate, authorization, client))
            .await
            .map(SwapPairSide::Setup),
        None => existing_pair_side(owner, operation),
    }
}

/// An existing stealth account as one side of a reserved swap pair.
pub(super) fn existing_pair_side(
    owner: &ExecutorOwner,
    operation: ExecutorOperationId,
) -> eyre::Result<SwapPairSide> {
    let executor = owner
        .records()?
        .into_iter()
        .find(|record| record.operation() == operation)
        .and_then(|record| record.address())
        .ok_or_else(|| eyre::eyre!("The stealth account is unavailable."))?;
    Ok(SwapPairSide::Existing {
        operation,
        executor,
    })
}

/// Plan the notes without proving, for a stand-in executor before setup or from recorded
/// account state, then quote without hooks and price the order
/// limit. Nothing is signed. The swap's orderbook route is returned for reuse even when
/// quoting fails.
pub(super) async fn quote_swap(
    owner: Arc<ExecutorOwner>,
    session: Arc<WalletSession>,
    request: QuoteRequest,
) -> QuoteResult {
    let mut orderbook = request.orderbook.clone();
    let mut bridge_clients = request
        .bridge_clients
        .clone()
        .filter(|_| orderbook.is_some());
    let mut bridge = None;
    let outcome = quote_swap_terms(
        &owner,
        &session,
        &request,
        &mut orderbook,
        &mut bridge_clients,
        &mut bridge,
    )
    .await;
    QuoteResult {
        orderbook,
        bridge_clients,
        bridge,
        outcome,
    }
}

async fn quote_swap_terms(
    owner: &Arc<ExecutorOwner>,
    session: &Arc<WalletSession>,
    request: &QuoteRequest,
    orderbook: &mut Option<CowOrderbookClient>,
    bridge_clients: &mut Option<SwapBridgeClients>,
    bridge: &mut Option<QuotedBridge>,
) -> eyre::Result<QuoteOutcome> {
    // A speed-up has no executor to preview: its plan names the open order's.
    let executor = match request.executor {
        QuoteExecutor::Preview => Ok(owner.swap_preview_executor()?),
        QuoteExecutor::Setup(operation) => Ok(owner.swap_setup_preview(operation)?),
        QuoteExecutor::Order { operation, reuse } => {
            Ok(owner.swap_order_preview(operation, reuse, request.inclusion)?)
        }
        QuoteExecutor::SpeedUp { operation, order } => Err((operation, order)),
    };
    let client = if let Some(client) = orderbook.clone() {
        client
    } else {
        let client = owner.swap_orderbook_client(&request.http.operation_http_client().await?)?;
        *orderbook = Some(client.clone());
        client
    };
    if let Some(request_bridge) = &request.bridge {
        let clients = if let Some(clients) = bridge_clients.clone() {
            clients
        } else {
            let clients = owner.swap_bridge_clients(&client)?;
            *bridge_clients = Some(clients.clone());
            clients
        };
        let origin = request_bridge.origin;
        let destination = match &request_bridge.destination {
            QuoteBridgeDestination::Chain { destination, chain } => QuotedDestination::Chain {
                destination: match destination {
                    Some(destination) => destination.clone(),
                    None => approved_bridge_destination(&clients, origin, chain, request).await?,
                },
                chain: chain.clone(),
            },
            QuoteBridgeDestination::NonEvm(destination) => {
                QuotedDestination::NonEvm(match destination {
                    Some(destination) => destination.clone(),
                    None => approved_non_evm_destination(&clients, &origin, request).await?,
                })
            }
        };
        *bridge = Some(QuotedBridge {
            clients,
            destination,
        });
    }
    let amount_request = SwapAmountRequest {
        sell_token: request.sell,
        buy_token: request.buy,
        amount: request.amount,
        delivery: request.delivery.clone(),
        byte_budget: request.byte_budget,
    };
    let planning_owner = Arc::clone(owner);
    let planning_session = Arc::clone(session);
    let inclusion = request.inclusion;
    // Note selection can take a moment on fragmented balances.
    let plan = tokio::task::spawn_blocking(move || match executor {
        Ok(executor) => {
            planning_owner.plan_swap_amount(&executor, &planning_session, &amount_request)
        }
        // The open order's own notes, tokens, spend and delivery, whatever the request names.
        Err((operation, order)) => {
            planning_owner.plan_swap_speed_up(&planning_session, operation, order, inclusion)
        }
    })
    .await;
    let plan = plan.map_err(|_| eyre::eyre!("Planning the swap stopped unexpectedly."))??;
    let plan = match plan {
        SwapAmountPlan::TooLarge { largest } => {
            return Ok(QuoteOutcome::TooLarge(largest.amount()));
        }
        SwapAmountPlan::Fits(plan) => plan,
    };
    match Box::pin(owner.review_swap(SwapReviewRequest {
        plan,
        slippage_bps: request.slippage_bps,
        gas_share_bps: request.gas_share_bps,
        valid_for: request.valid_for,
        orderbook: &client,
        anchor_cache: request.anchor_cache.as_deref(),
        token_registry: &request.tokens,
        bridge: bridge.as_ref().map(QuotedBridge::route),
    }))
    .await
    {
        Ok(review) => Ok(QuoteOutcome::Review(Box::new(review))),
        Err(error)
            if matches!(
                error.downcast_ref::<QuoteDeviationError>(),
                Some(QuoteDeviationError::ExceedsThreshold)
            ) =>
        {
            Ok(QuoteOutcome::PriceBlocked(PriceBlock::Deviates))
        }
        Err(error) => Err(error),
    }
}
