#[allow(clippy::wildcard_imports)]
use super::*;

pub(crate) fn public_broadcaster_build_error(
    error: BuildError,
    fee_amount: U256,
    fee_mode: FeeHandlingMode,
    same_token_fee: bool,
    protocol_fee_bps: U256,
) -> Report {
    match error {
        BuildError::InsufficientBalance(max_receiver_amount) => eyre!(
            "{PUBLIC_BROADCASTER_MAX_ENTERED_AMOUNT_ERROR}{}",
            public_broadcaster_max_entered_amount_for_tokens_and_protocol(
                max_receiver_amount,
                fee_amount,
                fee_mode,
                same_token_fee,
                protocol_fee_bps,
            )
        ),
        BuildError::InsufficientFeeTokenBalance(max_spendable) => {
            eyre!(
                "{PUBLIC_BROADCASTER_FEE_TOKEN_MAX_SPENDABLE_ERROR}{max_spendable}{PUBLIC_BROADCASTER_REQUIRED_FEE_ERROR}{fee_amount}"
            )
        }
        other => Report::new(other),
    }
}

/// The input notes of the only transaction that spends `action_token`, or `None` when several
/// do. A separate fee token's fee-only transaction is not a payload.
pub(crate) fn single_payload_inputs<'a, C>(
    chunk_inputs: impl IntoIterator<Item = C>,
    action_token: Address,
) -> Option<Vec<SelectedInputIdentity>>
where
    C: IntoIterator<Item = &'a Utxo>,
{
    let token = U256::from_be_slice(action_token.as_slice());
    let mut payloads = chunk_inputs
        .into_iter()
        .map(|inputs| inputs.into_iter().collect::<Vec<_>>())
        .filter(|inputs| inputs.iter().any(|utxo| utxo.note.token_hash == token));
    let payload = payloads.next()?;
    payloads.next().is_none().then(|| {
        payload
            .into_iter()
            .map(SelectedInputIdentity::from_utxo)
            .collect()
    })
}

/// A pinned attempt is one payload transaction that spends at least one pinned note, so it
/// can't execute alongside the earlier proof.
pub(crate) fn require_pinned_payload(
    pinned: Option<&[SelectedInputIdentity]>,
    payload: Option<&[SelectedInputIdentity]>,
) -> Result<()> {
    let Some(pinned) = pinned else {
        return Ok(());
    };
    match payload {
        Some(inputs) if inputs.iter().any(|input| pinned.contains(input)) => Ok(()),
        _ => Err(PublicBroadcasterInputPinUnavailable.into()),
    }
}

/// A pinned estimate has one payload transaction besides any separate fee transaction.
pub(crate) fn require_pinned_estimate(
    pinned: bool,
    estimate: &PublicBroadcasterCostEstimate,
) -> Result<()> {
    let fee_transactions = usize::from(estimate.fee_token != estimate.action_token);
    if pinned && estimate.transaction_count.saturating_sub(fee_transactions) != 1 {
        return Err(PublicBroadcasterInputPinUnavailable.into());
    }
    Ok(())
}

pub(crate) fn public_broadcaster_anchor_rate_for_policy(
    anchor_cache: Option<&Arc<TokenAnchorRateCache>>,
    chain_id: u64,
    token: Address,
) -> Option<U256> {
    anchor_cache
        .and_then(|cache| cache.cached_rate(chain_id, token))
        .or_else(|| fixed_token_anchor_rate(chain_id, token))
}

/// Submissions keep the reviewed quote, or the executor-reserved one.
/// Refreshed advertisements need a new review, not this submission.
pub(crate) fn public_broadcaster_for_request(
    fee_rows: &[FeeRow],
    chain_id: u64,
    token: Address,
    selection: &PublicBroadcasterSelection,
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
    anchor_rate: Option<U256>,
    executor_delivery: Option<&ExecutorDelivery>,
    reviewed: Option<&PublicBroadcasterCandidate>,
) -> Result<PublicBroadcasterCandidate> {
    let candidates = if let Some(delivery) = executor_delivery {
        let ExecutorDelivery::PublicBroadcaster(candidate) = delivery else {
            return Err(eyre!("executor preparation selected another funding route"));
        };
        if candidate.chain_id != chain_id || candidate.token != token {
            return Err(eyre!("executor broadcaster chain or fee token changed"));
        }
        let profile = candidate
            .relay_adapt_7702
            .and_then(|delegate| settings::ExecutorProfile::accepted(chain_id, delegate))
            .ok_or_else(|| eyre!("broadcaster executor profile is unavailable"))?;
        delivery.admit(profile)?;
        let mut candidate = candidate.as_ref().clone();
        candidate.fee_policy_status = policy.classify_fee(candidate.fee, anchor_rate);
        vec![candidate]
    } else if let Some(reviewed) = reviewed {
        if reviewed.chain_id != chain_id || reviewed.token != token {
            return Err(eyre!("reviewed broadcaster chain or fee token changed"));
        }
        // Expiry is informational: the broadcaster simulates and settles any profitable request.
        let mut candidate = reviewed.clone();
        candidate.fee_policy_status = policy.classify_fee(candidate.fee, anchor_rate);
        vec![candidate]
    } else {
        public_broadcaster_candidates(
            fee_rows,
            chain_id,
            token,
            SystemTime::now(),
            policy,
            anchor_rate,
        )
    };
    select_public_broadcaster_with_policy_and_trust(&candidates, selection, policy, trust_filter)
}

pub(in crate::desktop) async fn public_broadcaster_setup(
    session: &WalletSession,
    chain_id: u64,
    effective_chain: &settings::EffectiveChainConfig,
    token: Address,
    quote: &PublicBroadcasterCandidate,
    executor_delivery: Option<&ExecutorDelivery>,
    policy: BroadcasterFeePolicy,
    trust_filter: &PublicBroadcasterTrustFilter,
    anchor_cache: Option<&Arc<TokenAnchorRateCache>>,
    http: &HttpContext,
    client: &OperationHttpClient,
) -> Result<PublicBroadcasterSetup> {
    let chain = effective_desktop_chain_config(chain_id, effective_chain)?;
    let anchor_rate = public_broadcaster_anchor_rate_for_policy(anchor_cache, chain_id, token);
    let broadcaster = public_broadcaster_for_request(
        &[],
        chain_id,
        token,
        &PublicBroadcasterSelection::Specific {
            railgun_address: quote.railgun_address.clone(),
        },
        policy,
        trust_filter,
        anchor_rate,
        executor_delivery,
        Some(quote),
    )?;
    let route = chain
        .rpc_route
        .for_class(RpcTrafficClass::Sensitive)?
        .isolated(client.clone());
    let query_rpc_pool = query_rpc_pool_for_route(&route, http);
    // Refresh after approval, then keep this price stable while building the proof.
    let fee_candidates = acquire_fee_candidates(http, &route, None).await?;
    let min_gas_price = buffered_gas_price_from_candidates(&fee_candidates, &chain.gas).await?;
    let artifact_source = artifact_source(http, session.db.as_ref())?;
    let prover = session.prover_service(&artifact_source);
    let chain_handle = session
        .sync_manager
        .chain_handle(&session.chain_key)
        .await
        .ok_or_else(|| eyre!("chain handle not found for chain {chain_id}"))?;
    let mut forest = chain_handle.forest.read().await.clone();
    forest.compute_roots();
    let utxos = session.unspent_utxos();

    Ok(PublicBroadcasterSetup {
        chain,
        broadcaster,
        query_rpc_pool,
        min_gas_price,
        prover,
        forest,
        utxos,
    })
}
