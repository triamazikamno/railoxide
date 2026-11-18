use super::*;

/// Each attempt re-quotes fees on a candidate from `route` and reads the balance on a
/// `query_rpc_pool` provider. The candidates belong to this replacement alone.
pub(in crate::desktop) async fn self_broadcast_replacement_preflight_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    http: &HttpContext,
    route: &ClassRoute,
    chain_id: u64,
    from: Address,
    transaction: TransactionRequest,
    gas_fee: SelfBroadcastGasFeeSelection,
    gas_limit: u64,
    nonce: u64,
) -> Result<SelfBroadcastPreflight> {
    let fee_candidates = acquire_fee_candidates(http, route, None).await?;
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        let Some(fee_provider) = fee_candidates.random_ready().await else {
            break;
        };
        match self_broadcast_replacement_preflight(
            &fee_candidates,
            &fee_provider,
            provider_handle,
            chain_id,
            from,
            transaction.clone(),
            gas_fee,
            gas_limit,
            nonce,
        )
        .await
        {
            Ok(preflight) => return Ok(preflight),
            Err(error) if is_self_broadcast_insufficient_native_gas_error(&error) => {
                return Err(error);
            }
            Err(error) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    %error,
                    "self-broadcast replacement preflight failed"
                );
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(error).wrap_err("all self-broadcast replacement RPC attempts failed")
    } else {
        Err(eyre!("no healthy query RPC available"))
    }
}

pub(in crate::desktop) async fn self_broadcast_replacement_preflight(
    fee_candidates: &RpcCandidates,
    fee_provider: &ProviderHandle,
    provider_handle: ProviderHandle,
    chain_id: u64,
    from: Address,
    transaction: TransactionRequest,
    gas_fee: SelfBroadcastGasFeeSelection,
    gas_limit: u64,
    nonce: u64,
) -> Result<SelfBroadcastPreflight> {
    let provider = &provider_handle.provider;
    let quote = self_broadcast_gas_fee_quote(fee_candidates, fee_provider)
        .await
        .wrap_err("fetch self-broadcast gas price")?;
    let SelfBroadcastResolvedGasFee {
        rpc_gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
    } = resolve_self_broadcast_gas_fee(gas_fee, quote)?;
    let estimated_native_gas_cost = self_broadcast_native_gas_cost(gas_limit, max_fee_per_gas);
    let required_native = estimated_native_gas_cost
        .checked_add(transaction.value.unwrap_or_default())
        .ok_or_else(|| eyre!("self-broadcast native cost exceeds the supported amount"))?;
    let live_native_balance = provider
        .get_balance(from)
        .await
        .wrap_err("fetch self-broadcast native balance")?;
    if live_native_balance < required_native {
        return Err(self_broadcast_insufficient_native_gas_error(
            live_native_balance,
            required_native,
        ));
    }
    Ok(SelfBroadcastPreflight {
        tx_req: self_broadcast_transaction_request(
            chain_id,
            from,
            transaction,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            nonce,
        )?
        .with_gas_limit(gas_limit),
        nonce,
        gas_limit,
        rpc_gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        estimated_native_gas_cost,
        live_native_balance,
    })
}

/// Quotes fees from `route`'s candidates, then reads the nonce, estimate and balance on one
/// `query_rpc_pool` provider per attempt.
pub(in crate::desktop) async fn self_broadcast_preflight_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    http: &HttpContext,
    route: &ClassRoute,
    chain_id: u64,
    from: Address,
    transaction: TransactionRequest,
    gas_fee: SelfBroadcastGasFeeSelection,
    gas: &settings::EffectiveChainGasSettings,
    nonce: Option<u64>,
) -> Result<SelfBroadcastPreflight> {
    let quote = self_broadcast_gas_fee_quote_from_route(http, route)
        .await
        .wrap_err("fetch self-broadcast gas price")?;
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        match self_broadcast_preflight(
            provider_handle,
            chain_id,
            from,
            transaction.clone(),
            gas_fee,
            quote,
            gas,
            nonce,
        )
        .await
        {
            Ok(preflight) => return Ok(preflight),
            Err(error) if is_self_broadcast_insufficient_native_gas_error(&error) => {
                return Err(error);
            }
            Err(error) => {
                tracing::warn!(target: LOG_TARGET, ?error, "self-broadcast preflight failed");
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(error).wrap_err("all self-broadcast query RPC attempts failed")
    } else {
        Err(eyre!("no healthy query RPC available"))
    }
}

pub(in crate::desktop) async fn self_broadcast_preflight(
    provider_handle: ProviderHandle,
    chain_id: u64,
    from: Address,
    transaction: TransactionRequest,
    gas_fee: SelfBroadcastGasFeeSelection,
    quote: SelfBroadcastGasFeeQuote,
    gas: &settings::EffectiveChainGasSettings,
    nonce: Option<u64>,
) -> Result<SelfBroadcastPreflight> {
    let provider = &provider_handle.provider;
    let SelfBroadcastResolvedGasFee {
        rpc_gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
    } = resolve_self_broadcast_gas_fee(gas_fee, quote)?;
    let nonce = if let Some(nonce) = nonce {
        nonce
    } else {
        provider
            .get_transaction_count(from)
            .await
            .wrap_err("fetch self-broadcast nonce")?
    };
    let tx_req = self_broadcast_transaction_request(
        chain_id,
        from,
        transaction,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        nonce,
    )?;
    let estimated_gas = crate::desktop::executors::estimate_delegated_gas(provider, &tx_req)
        .await
        .wrap_err("estimate self-broadcast gas")?;
    let gas_limit =
        self_broadcast_gas_limit_with_buffer(estimated_gas, gas.gas_limit_buffer, tx_req.gas)?;
    let estimated_native_gas_cost = self_broadcast_native_gas_cost(gas_limit, max_fee_per_gas);
    let required_native = estimated_native_gas_cost
        .checked_add(tx_req.value.unwrap_or_default())
        .ok_or_else(|| eyre!("self-broadcast native cost exceeds the supported amount"))?;
    let live_native_balance = provider
        .get_balance(from)
        .await
        .wrap_err("fetch self-broadcast native balance")?;
    if live_native_balance < required_native {
        return Err(self_broadcast_insufficient_native_gas_error(
            live_native_balance,
            required_native,
        ));
    }
    Ok(SelfBroadcastPreflight {
        tx_req: tx_req.with_gas_limit(gas_limit),
        nonce,
        gas_limit,
        rpc_gas_price,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        estimated_native_gas_cost,
        live_native_balance,
    })
}

pub(crate) fn self_broadcast_transaction_request(
    chain_id: u64,
    from: Address,
    transaction: TransactionRequest,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    nonce: u64,
) -> Result<TransactionRequest> {
    if transaction
        .chain_id
        .is_some_and(|prepared| prepared != chain_id)
        || transaction.from.is_some_and(|prepared| prepared != from)
        || transaction.nonce.is_some_and(|prepared| prepared != nonce)
    {
        return Err(eyre!(
            "prepared transaction no longer matches the selected chain, gas payer, or nonce"
        ));
    }
    Ok(transaction
        .with_chain_id(chain_id)
        .with_from(from)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
        .with_nonce(nonce))
}

pub(in crate::desktop) async fn sponsored_code_preflight_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    payer: Address,
    signer: Address,
) -> std::result::Result<ProviderHandle, SponsorshipError> {
    for provider_handle in query_rpc_pool.available_providers() {
        match sponsored_code_preflight(&provider_handle.provider, payer, signer).await {
            Ok(()) => return Ok(provider_handle),
            Err(SponsorshipError::CodeQueryFailed) => {}
            Err(error) => return Err(error),
        }
    }
    Err(SponsorshipError::CodeQueryFailed)
}

pub(in crate::desktop) async fn sponsored_exact_gas_estimate(
    provider_handle: &ProviderHandle,
    to: Address,
    data: Bytes,
) -> Result<u64> {
    provider_handle
        .provider
        .estimate_gas(TransactionRequest::default().with_to(to).with_input(data))
        .await
        .map_err(|_| eyre!("exact sponsored calldata gas estimation failed"))
}

pub(crate) fn self_broadcast_gas_limit_with_buffer(
    estimated_gas: u64,
    gas_limit_buffer: u64,
    approved_maximum: Option<u64>,
) -> Result<u64> {
    let gas_limit = estimated_gas.saturating_add(gas_limit_buffer);
    if approved_maximum.is_some_and(|maximum| gas_limit > maximum) {
        return Err(eyre!(
            "self-broadcast gas exceeds the reviewed maximum; refresh and approve the quote"
        ));
    }
    Ok(gas_limit)
}

pub(crate) fn self_broadcast_native_gas_cost(gas_limit: u64, max_fee_per_gas: u128) -> U256 {
    U256::from(gas_limit) * U256::from(max_fee_per_gas)
}

pub(crate) fn self_broadcast_insufficient_native_gas_error(
    balance: U256,
    estimated_cost: U256,
) -> Report {
    eyre!(
        "insufficient native gas for self-broadcast: live balance {balance}, estimated cost {estimated_cost}"
    )
}

pub(crate) fn is_self_broadcast_insufficient_native_gas_error(error: &Report) -> bool {
    error
        .to_string()
        .starts_with("insufficient native gas for self-broadcast:")
}
