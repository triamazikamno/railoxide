#[allow(clippy::wildcard_imports)]
use super::*;

pub async fn estimate_public_advanced_transaction(
    request: PublicAdvancedTransactionEstimateRequest,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<PublicAdvancedTransactionEstimate> {
    validate_public_transaction_intent(&request.intent)?;
    if !matches!(request.intent, PublicTransactionIntent::Raw { .. }) {
        return Err(eyre!(
            "advanced gas estimation requires a raw transaction intent"
        ));
    }
    let chain = public_chain_runtime_config(request.chain_id, &request.effective_chain)?;
    // The quote and the estimate share one source, so neither stage probes an endpoint again.
    let candidates = public_fee_candidates(
        http,
        &chain
            .rpc_route
            .for_class(RpcTrafficClass::Sensitive)?
            .isolated(client.clone()),
        None,
    )
    .await?;
    let quote = public_action_gas_fee_quote_from_candidates(
        &candidates,
        http.network_mode(),
        request.chain_id,
    )
    .await
    .wrap_err("fetch advanced public transaction gas price")?;
    let resolved = resolve_public_action_gas_fee(
        request.chain_id,
        PublicShieldTransactionProfile::Railoxide,
        PublicAssetId::Native,
        request.gas_fee,
        Some(quote),
    )?;
    advanced_estimate_report(
        estimate_public_advanced_transaction_with_fee_core(
            &candidates,
            request,
            quote,
            resolved,
            None,
        )
        .await,
    )
}

pub async fn estimate_public_advanced_transaction_with_fee(
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<PublicAdvancedTransactionEstimate> {
    validate_advanced_transaction_estimate_request(&request)?;
    advanced_estimate_report(
        estimate_public_advanced_transaction_with_own_candidates(
            request, quote, resolved, http, client, None,
        )
        .await,
    )
}

fn advanced_estimate_report(
    result: std::result::Result<
        PublicAdvancedTransactionEstimate,
        PublicAdvancedTransactionSimulationError,
    >,
) -> Result<PublicAdvancedTransactionEstimate> {
    result
        .map_err(|error| eyre!(simulation_error_reason(&error).to_owned()))
        .wrap_err("all advanced public transaction query RPC attempts failed")
}

pub async fn simulate_public_advanced_transaction_with_fee(
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> std::result::Result<PublicAdvancedTransactionEstimate, PublicAdvancedTransactionSimulationError>
{
    simulate_public_advanced_transaction_with_fee_and_reads(
        request, quote, resolved, http, client, None,
    )
    .await
}

pub async fn simulate_public_advanced_transaction_with_fee_and_reads(
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    http: &HttpContext,
    client: &OperationHttpClient,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
) -> std::result::Result<PublicAdvancedTransactionEstimate, PublicAdvancedTransactionSimulationError>
{
    validate_advanced_transaction_estimate_request(&request).map_err(|error| {
        PublicAdvancedTransactionSimulationError::Unavailable(error.to_string())
    })?;
    estimate_public_advanced_transaction_with_own_candidates(
        request, quote, resolved, http, client, rpc_reads,
    )
    .await
}

fn validate_advanced_transaction_estimate_request(
    request: &PublicAdvancedTransactionEstimateRequest,
) -> Result<()> {
    validate_public_transaction_intent(&request.intent)?;
    if !matches!(request.intent, PublicTransactionIntent::Raw { .. }) {
        return Err(eyre!(
            "advanced gas estimation requires a raw transaction intent"
        ));
    }
    Ok(())
}

/// A standalone estimate owns its candidate source, on the Sensitive route isolated over
/// `client`.
async fn estimate_public_advanced_transaction_with_own_candidates(
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    http: &HttpContext,
    client: &OperationHttpClient,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
) -> std::result::Result<PublicAdvancedTransactionEstimate, PublicAdvancedTransactionSimulationError>
{
    let unavailable =
        |_: eyre::Report| unavailable_simulation_error("RPC providers are unavailable.");
    let chain = public_chain_runtime_config(request.chain_id, &request.effective_chain)
        .map_err(unavailable)?;
    let route = chain
        .rpc_route
        .for_class(RpcTrafficClass::Sensitive)
        .map_err(|error| unavailable(error.into()))?
        .isolated(client.clone());
    let candidates = public_fee_candidates(http, &route, rpc_reads)
        .await
        .map_err(unavailable)?;
    estimate_public_advanced_transaction_with_fee_core(
        &candidates,
        request,
        quote,
        resolved,
        rpc_reads,
    )
    .await
}

/// Tries ready candidates once each, including endpoints admitted while earlier ones fail,
/// until one estimates or discovery has ended.
async fn estimate_public_advanced_transaction_with_fee_core(
    candidates: &RpcCandidates,
    request: PublicAdvancedTransactionEstimateRequest,
    quote: PublicActionGasFeeQuote,
    resolved: PublicActionResolvedGasFee,
    rpc_reads: Option<&crate::public_wallet::DappRpcReadClient>,
) -> std::result::Result<PublicAdvancedTransactionEstimate, PublicAdvancedTransactionSimulationError>
{
    let mut tx_req =
        public_send_transaction_request(request.chain_id, request.from, &request.intent)
            .map_err(|error| unavailable_simulation_error(&error.to_string()))?;
    match quote.observed_fee_model {
        Some(crate::EvmFeeModel::Legacy) => {
            tx_req.gas_price = Some(resolved.max_fee_per_gas);
        }
        Some(crate::EvmFeeModel::Eip1559) => {
            tx_req.max_fee_per_gas = Some(resolved.max_fee_per_gas);
            tx_req.max_priority_fee_per_gas = Some(resolved.max_priority_fee_per_gas);
        }
        None => {
            return Err(unavailable_simulation_error(
                "Chain fee model has not been observed.",
            ));
        }
    }
    tx_req.access_list = request.access_list;

    // Without any candidate there are no failures, which reports the providers as unavailable.
    let mut tried = HashSet::new();
    let mut failures = Vec::new();
    while let Some(provider_handle) = candidates.next_untried(&mut tried).await {
        let estimate = match rpc_reads {
            Some(reads) => match reads
                .estimate_gas(
                    provider_handle.url.clone().into(),
                    request.chain_id,
                    &tx_req,
                )
                .await
            {
                Ok(value) => Ok(value),
                Err(error) => {
                    if crate::public_wallet::stops_dapp_read_retries(&error) {
                        return Err(PublicAdvancedTransactionSimulationError::RpcRead(error));
                    }
                    Err(classify_broker_provider_failure(&error))
                }
            },
            None => provider_handle
                .provider
                .estimate_gas(tx_req.clone())
                .await
                .inspect(|_| candidates.succeeded(&provider_handle))
                .map_err(|error| {
                    tracing::warn!(target: LOG_TARGET,%error, "advanced public transaction gas estimate failed");
                    candidates.report(
                        &provider_handle,
                        EndpointHealthOutcome::for_direct_error(&error),
                    );
                    classify_provider_failure(&error)
                }),
        };
        match estimate {
            Ok(estimated_gas) => {
                let gas_limit = buffered_advanced_gas_limit(
                    estimated_gas,
                    request.effective_chain.gas.gas_limit_buffer,
                );
                let projection = project_public_action_fee(
                    estimated_gas,
                    gas_limit,
                    quote,
                    resolved,
                    PublicActionFeeSource::NetworkSimulation,
                    None,
                    request.effective_chain.native_currency.decimals,
                );
                return Ok(PublicAdvancedTransactionEstimate {
                    payload_fingerprint: public_advanced_transaction_payload_fingerprint(
                        request.chain_id,
                        request.from,
                        &request.intent,
                        resolved.max_fee_per_gas,
                        resolved.max_priority_fee_per_gas,
                    ),
                    raw_gas_limit: estimated_gas,
                    gas_limit,
                    max_fee_per_gas: resolved.max_fee_per_gas,
                    max_priority_fee_per_gas: resolved.max_priority_fee_per_gas,
                    expected_fee_per_gas: projection.expected_fee_per_gas,
                    expected_gas_cost: projection.expected_gas_cost,
                    max_gas_cost: projection.maximum_gas_cost,
                });
            }
            Err(error) => {
                failures.push(error);
            }
        }
    }

    Err(select_provider_failure(failures))
}

pub(in crate::public_wallet) const fn buffered_advanced_gas_limit(
    estimated_gas: u64,
    buffer: u64,
) -> u64 {
    estimated_gas.saturating_add(buffer)
}

pub(in crate::public_wallet) fn public_advanced_transaction_payload_fingerprint(
    chain_id: u64,
    from: alloy::primitives::Address,
    intent: &PublicTransactionIntent,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> B256 {
    let mut encoded = b"railoxide:public-advanced-transaction:v1".to_vec();
    encoded.extend_from_slice(&chain_id.to_be_bytes());
    encoded.extend_from_slice(from.as_slice());
    match intent {
        PublicTransactionIntent::Transfer {
            asset,
            amount,
            recipient,
        } => {
            encoded.push(0);
            match asset {
                PublicAssetId::Native => encoded.push(0),
                PublicAssetId::Erc20(token) => {
                    encoded.push(1);
                    encoded.extend_from_slice(token.as_slice());
                }
            }
            encoded.extend_from_slice(&amount.to_be_bytes::<32>());
            encoded.extend_from_slice(recipient.as_slice());
        }
        PublicTransactionIntent::Raw { to, value, data } => {
            encoded.push(1);
            match to {
                Some(to) => {
                    encoded.push(1);
                    encoded.extend_from_slice(to.as_slice());
                }
                None => encoded.push(0),
            }
            encoded.extend_from_slice(&value.to_be_bytes::<32>());
            encoded.extend_from_slice(&(data.len() as u64).to_be_bytes());
            encoded.extend_from_slice(data);
        }
        PublicTransactionIntent::WrapNative { wrapped, amount } => {
            encoded.push(2);
            encoded.extend_from_slice(wrapped.as_slice());
            encoded.extend_from_slice(&amount.to_be_bytes::<32>());
        }
        PublicTransactionIntent::UnwrapNative { wrapped, amount } => {
            encoded.push(3);
            encoded.extend_from_slice(wrapped.as_slice());
            encoded.extend_from_slice(&amount.to_be_bytes::<32>());
        }
    }
    encoded.extend_from_slice(&max_fee_per_gas.to_be_bytes());
    encoded.extend_from_slice(&max_priority_fee_per_gas.to_be_bytes());
    keccak256(encoded)
}
