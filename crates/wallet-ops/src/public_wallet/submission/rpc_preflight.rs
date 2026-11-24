#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::public_wallet) async fn public_action_preflight_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    http: &HttpContext,
    rpc_route: &ClassRoute,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
    funding_budget: Option<PublicShieldFundingBudget>,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    public_action_preflight_from_rpc_pool_with_reads(
        query_rpc_pool,
        http,
        rpc_route,
        chain_id,
        from,
        base_tx_req,
        gas_fee,
        gas,
        profile,
        asset,
        gas_limit_strategy,
        authorized_gas_limit,
        nonce,
        gas_limit,
        authorized_fee_ceiling,
        railway_auto,
        None,
        funding_budget,
    )
    .await
}

/// Quotes fees, when needed, from candidates on `rpc_route` that this preflight alone owns, then
/// reads the nonce, estimate and balance on one candidate per native attempt. Admitted dapp
/// reads retain their original pool and gateway admission.
pub(in crate::public_wallet) async fn public_action_preflight_from_rpc_pool_with_reads(
    query_rpc_pool: &QueryRpcPool,
    http: &HttpContext,
    rpc_route: &ClassRoute,
    chain_id: u64,
    from: Address,
    base_tx_req: TransactionRequest,
    gas_fee: PublicActionGasFeeSelection,
    gas: &EffectiveChainGasSettings,
    profile: PublicShieldTransactionProfile,
    asset: PublicAssetId,
    gas_limit_strategy: PublicActionGasLimitStrategy,
    authorized_gas_limit: Option<u64>,
    nonce: Option<u64>,
    gas_limit: Option<u64>,
    authorized_fee_ceiling: Option<PublicActionGasFeeSelection>,
    railway_auto: bool,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
    funding_budget: Option<PublicShieldFundingBudget>,
) -> std::result::Result<PublicActionPreflight, PublicActionPreflightError> {
    if rpc_reads.is_some() && profile != PublicShieldTransactionProfile::Railoxide {
        return Err(eyre!("admitted dapp reads require the Railoxide transaction profile").into());
    }
    let candidates = crate::public_wallet::gas::public_fee_candidates(http, rpc_route, rpc_reads)
        .await
        .wrap_err("select public action query RPC candidates")?;
    let quote = if profile == PublicShieldTransactionProfile::Railoxide
        || (matches!(gas_fee, PublicActionGasFeeSelection::Auto) && railway_auto)
    {
        Some(
            match rpc_reads {
                Some(reads) => {
                    crate::public_wallet::gas::public_action_gas_fee_quote_from_candidates_with_reads(
                        &candidates,
                        http.network_mode(),
                        chain_id,
                        Some(reads),
                    )
                    .await
                }
                None => {
                    crate::public_wallet::gas::public_action_gas_fee_quote_from_candidates_with_profile(
                        &candidates,
                        http.network_mode(),
                        chain_id,
                        profile,
                        asset,
                    )
                    .await
                }
            }
            .wrap_err("fetch public action gas price")?,
        )
    } else {
        None
    };
    let mut last_error = None;
    let mut tried = HashSet::new();
    let attempts = if rpc_reads.is_some() {
        query_rpc_pool.len()
    } else {
        candidates.len()
    };
    for attempt in 1..=attempts {
        let provider_handle = if rpc_reads.is_some() {
            query_rpc_pool.random_provider()
        } else {
            candidates.next_untried(&mut tried).await
        };
        let Some(provider_handle) = provider_handle else {
            break;
        };
        let rpc_index = rpc_route
            .endpoints()
            .iter()
            .position(|endpoint| endpoint.expose_url() == &provider_handle.url);
        let endpoint = crate::http::redact_url_for_display(&provider_handle.url);
        let started = std::time::Instant::now();
        let mut stage = "fee_resolution";
        // Gateway candidates report nothing, so a pool provider used for admitted reads isn't
        // reported either.
        match public_action_preflight(
            provider_handle.clone(),
            chain_id,
            from,
            base_tx_req.clone(),
            gas_fee,
            quote,
            gas,
            profile,
            asset,
            gas_limit_strategy,
            authorized_gas_limit,
            nonce,
            gas_limit,
            authorized_fee_ceiling,
            railway_auto,
            rpc_reads,
            &mut stage,
            funding_budget,
        )
        .await
        {
            Ok(preflight) => {
                candidates.succeeded(&provider_handle);
                return Ok(preflight);
            }
            // A gas refusal isn't an endpoint failure. A lower estimate elsewhere doesn't make this
            // one wrong, and signing below it risks an out-of-gas revert that spends the whole fee.
            Err(
                error @ (PublicActionPreflightError::FeeAuthorizationRequired { .. }
                | PublicActionPreflightError::GasAboveReviewedLimit { .. }),
            ) => {
                return Err(error);
            }
            Err(error) => {
                let insufficient_funding = matches!(
                    &error,
                    PublicActionPreflightError::InsufficientShieldFunding(_)
                );
                let error = error.into_report();
                candidates.failed(&provider_handle, &error);
                if rpc_reads.is_some() {
                    if error
                        .downcast_ref::<crate::rpc_broker::RpcBrokerError>()
                        .is_some_and(crate::public_wallet::stops_dapp_read_retries)
                    {
                        return Err(PublicActionPreflightError::Other(error));
                    }
                } else {
                    // Error messages and bodies can contain RPC credentials or transaction data.
                    let transport_error = error.downcast_ref::<TransportError>();
                    let rpc_code = transport_error
                        .and_then(RpcError::as_error_resp)
                        .map(|payload| payload.code);
                    let transport = transport_error.and_then(RpcError::as_transport_err);
                    let http_status = transport
                        .and_then(TransportErrorKind::as_http_error)
                        .map(|error| error.status);
                    let request_error = transport
                        .and_then(TransportErrorKind::as_custom)
                        .and_then(|error| error.downcast_ref::<reqwest::Error>());
                    let failure = match transport_error {
                        Some(RpcError::Transport(_)) if http_status.is_some() => "http",
                        Some(RpcError::Transport(_))
                            if request_error.is_some_and(reqwest::Error::is_timeout) =>
                        {
                            "timeout"
                        }
                        Some(RpcError::Transport(_))
                            if request_error.is_some_and(reqwest::Error::is_connect) =>
                        {
                            "connect"
                        }
                        Some(RpcError::Transport(_)) => "transport",
                        Some(RpcError::NullResp) => "missing_response",
                        Some(RpcError::DeserError { .. }) => "decode",
                        Some(RpcError::ErrorResp(_)) => "rpc_error",
                        Some(_) => "rpc_client",
                        None => "validation",
                    };
                    tracing::warn!(target: LOG_TARGET,
                        chain_id,
                        rpc_index,
                        endpoint = %endpoint,
                        attempt,
                        attempts,
                        stage,
                        elapsed_ms = started.elapsed().as_millis(),
                        failure,
                        rpc_code,
                        http_status,
                        "public action preflight failed"
                    );
                }
                if insufficient_funding {
                    return Err(PublicActionPreflightError::InsufficientShieldFunding(error));
                }
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(PublicActionPreflightError::Other(
            error.wrap_err("all public action query RPC attempts failed"),
        ))
    } else {
        Err(PublicActionPreflightError::Other(eyre!(
            "no healthy query RPC available"
        )))
    }
}
