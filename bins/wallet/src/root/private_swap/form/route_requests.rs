#[allow(clippy::wildcard_imports)]
use super::*;

/// What each provider delivers on `destination`'s chain for `sell`, asked on `clients`' route.
/// One provider failing leaves its list empty and is named in the routes; both failing is the
/// error. NEAR Intents isn't offered on a chain without a 1Click name and isn't asked: its
/// list is empty, and an Across failure is then the error.
async fn fetch_bridge_routes(
    clients: &SwapBridgeClients,
    origin: BridgeProfile,
    destination: BridgeDestinationProfile,
    sell: Address,
    tokens: &EffectiveTokenRegistry,
    across_native: Option<Address>,
) -> eyre::Result<BridgeRoutes> {
    let chain = destination.chain_id();
    let near_offered = destination.one_click_blockchain().is_some();
    let (routes, listed, unavailable) = match tokio::join!(
        clients.across.available_routes(origin.chain_id(), chain),
        async {
            if near_offered {
                clients.near.tokens().await
            } else {
                Ok(Vec::new())
            }
        },
    ) {
        (Ok(routes), Ok(listed)) => (routes, listed, None),
        (Err(error), Ok(_)) if !near_offered => return Err(error.into()),
        (Ok(routes), Err(error)) => (
            routes,
            Vec::new(),
            Some((BridgeProvider::NearIntents, eyre::Report::from(error))),
        ),
        (Err(error), Ok(listed)) => (
            Vec::new(),
            listed,
            Some((BridgeProvider::Across, eyre::Report::from(error))),
        ),
        (Err(across), Err(near)) => {
            let (across, near) = (eyre::Report::from(across), eyre::Report::from(near));
            // The form shows one error: the first it can describe.
            return Err(
                if bridge_unreachable(&across).is_none() && bridge_unreachable(&near).is_some() {
                    near
                } else {
                    across
                },
            );
        }
    };
    // Across failing here means NEAR Intents is offered, so the network isn't unserved.
    let served = near_offered || !routes.is_empty();
    let across = across_destination_tokens(&routes, sell, tokens, origin.chain_id(), chain);
    let near = near_destination_tokens(&listed, &origin, &destination, sell, tokens);
    // Without a sell token to leave out, the lists also hold the sell token's own asset.
    let mut same_asset: Vec<BridgeDestination> = Vec::new();
    for candidate in
        across_destination_tokens(&routes, Address::ZERO, tokens, origin.chain_id(), chain)
            .into_iter()
            .chain(near_destination_tokens(
                &listed,
                &origin,
                &destination,
                Address::ZERO,
                tokens,
            ))
    {
        let token = candidate.destination_token;
        if destination_for(&across, token).is_none()
            && destination_for(&near, token).is_none()
            && destination_for(&same_asset, token).is_none()
        {
            same_asset.push(candidate);
        }
    }
    Ok(BridgeRoutes {
        across,
        near,
        same_asset,
        across_native,
        unavailable,
        served,
    })
}

/// The approved Bridge delivery's destination in its provider's list today. It must still
/// bridge the approved bought token.
pub(super) async fn approved_bridge_destination(
    clients: &SwapBridgeClients,
    origin: BridgeProfile,
    chain: &EffectiveChainConfig,
    request: &QuoteRequest,
) -> eyre::Result<BridgeDestination> {
    let SwapDelivery::Bridge(delivery) = request.delivery else {
        return Err(eyre::eyre!("Only a Bridge swap has a bridge route."));
    };
    let destination = chain
        .bridge_destination()
        .ok_or_else(|| eyre::eyre!("Bridging to this network isn't supported."))?;
    let routes = fetch_bridge_routes(
        clients,
        origin,
        destination,
        request.sell,
        &request.tokens,
        across_unwrapped_token(chain),
    )
    .await?;
    // The approved provider couldn't be asked, which says nothing about its route.
    if let Some((provider, error)) = routes.unavailable
        && provider == delivery.provider
    {
        return Err(error);
    }
    let offered = match delivery.provider {
        BridgeProvider::Across => routes.across,
        BridgeProvider::NearIntents => routes.near,
    };
    offered
        .into_iter()
        .find(|offered| {
            (offered.destination_token, offered.intermediate)
                == (delivery.destination_token, request.buy)
        })
        .ok_or_else(|| {
            eyre::eyre!(
                "{} no longer offers the approved route. Open the swap to review it again.",
                provider_name(delivery.provider)
            )
        })
}

/// The approved non-EVM delivery's destination in 1Click's list today, bought through the
/// approved intermediate.
pub(super) async fn approved_non_evm_destination(
    clients: &SwapBridgeClients,
    origin: &BridgeProfile,
    request: &QuoteRequest,
) -> eyre::Result<NonEvmBridgeDestination> {
    let SwapDelivery::NonEvmBridge(delivery) = &request.delivery else {
        return Err(eyre::eyre!("Only a Bridge swap has a bridge route."));
    };
    fetch_non_evm_routes(
        clients,
        origin,
        delivery.network,
        request.sell,
        &request.tokens,
    )
    .await?
    .into_iter()
    .find(|offered| {
        offered.near.destination_asset == delivery.asset_id
            && offered.contract == delivery.contract
            && offered.intermediate == request.buy
    })
    .ok_or_else(|| {
        eyre::eyre!(
            "NEAR Intents no longer offers the approved route. Open the swap to review it again."
        )
    })
}

/// What NEAR Intents delivers on a non-EVM `network` for `sell`, from 1Click's token list on
/// `clients`' route.
async fn fetch_non_evm_routes(
    clients: &SwapBridgeClients,
    origin: &BridgeProfile,
    network: NonEvmNetwork,
    sell: Address,
    tokens: &EffectiveTokenRegistry,
) -> eyre::Result<Vec<NonEvmBridgeDestination>> {
    let listed = clients.near.tokens().await?;
    Ok(near_non_evm_destination_tokens(
        &listed, origin, network, sell, tokens,
    ))
}

/// The bridge clients on the swap's orderbook route, creating that route and the clients if
/// the form has none yet. What is created is left in `orderbook` and `bridge_clients`.
async fn swap_route_bridge_clients(
    owner: &ExecutorOwner,
    http: &wallet_ops::HttpContext,
    orderbook: &mut Option<CowOrderbookClient>,
    bridge_clients: &mut Option<SwapBridgeClients>,
) -> eyre::Result<SwapBridgeClients> {
    let client = if let Some(client) = orderbook.clone() {
        client
    } else {
        let client = owner.swap_orderbook_client(&http.operation_http_client().await?)?;
        *orderbook = Some(client.clone());
        client
    };
    Ok(if let Some(clients) = bridge_clients.clone() {
        clients
    } else {
        let clients = owner.swap_bridge_clients(&client)?;
        *bridge_clients = Some(clients.clone());
        clients
    })
}

/// Fetch the Buy list of a Bridge swap on the swap's orderbook route, creating that route if
/// the form has none yet.
pub(super) async fn request_bridge_routes(
    owner: Arc<ExecutorOwner>,
    http: wallet_ops::HttpContext,
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    origin: BridgeProfile,
    destination: BridgeDestinationProfile,
    sell: Address,
    tokens: EffectiveTokenRegistry,
    across_native: Option<Address>,
) -> BridgeRoutesResult {
    let mut bridge_clients = bridge_clients.filter(|_| orderbook.is_some());
    let mut orderbook = orderbook;
    let routes = async {
        let clients =
            swap_route_bridge_clients(&owner, &http, &mut orderbook, &mut bridge_clients).await?;
        fetch_bridge_routes(&clients, origin, destination, sell, &tokens, across_native).await
    }
    .await;
    BridgeRoutesResult {
        orderbook,
        bridge_clients,
        routes,
    }
}

/// Fetch the Buy list of a non-EVM `network` as [`request_bridge_routes`] does a chain's.
pub(super) async fn request_non_evm_routes(
    owner: Arc<ExecutorOwner>,
    http: wallet_ops::HttpContext,
    orderbook: Option<CowOrderbookClient>,
    bridge_clients: Option<SwapBridgeClients>,
    origin: BridgeProfile,
    network: NonEvmNetwork,
    sell: Address,
    tokens: EffectiveTokenRegistry,
) -> BridgeRoutesResult<Vec<NonEvmBridgeDestination>> {
    let mut bridge_clients = bridge_clients.filter(|_| orderbook.is_some());
    let mut orderbook = orderbook;
    let routes = async {
        let clients =
            swap_route_bridge_clients(&owner, &http, &mut orderbook, &mut bridge_clients).await?;
        fetch_non_evm_routes(&clients, &origin, network, sell, &tokens).await
    }
    .await;
    BridgeRoutesResult {
        orderbook,
        bridge_clients,
        routes,
    }
}
