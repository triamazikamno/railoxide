use super::*;

pub async fn estimate_desktop_unshield_public_broadcaster_cost(
    request: DesktopUnshieldPublicBroadcasterEstimateRequest,
    http: &HttpContext,
) -> Result<PublicBroadcasterCostEstimate> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    let chain = effective_desktop_chain_config(request.chain_id, &request.effective_chain)?;
    validate_desktop_executor_preparation(&request.session, request.executor.as_deref())?;
    // The first approval needs the executor gas budget before seed access and allocation.
    // Public broadcaster delivery is never privately sponsored.
    let executor_profile = if request.executor.is_none()
        && unshield_requires_executor(request.unwrap, request.native_top_up.is_some(), false)
    {
        Some(unshield_executor_route(
            request.session.executor_owner().is_some(),
            &request.effective_chain,
        )?)
    } else {
        None
    };
    let uses_executor = request.executor.is_some() || executor_profile.is_some();
    if request.unwrap && !is_effective_wrapped_native_token(request.chain_id, request.token, &chain)
    {
        return Err(eyre!("selected token does not support unwrap-to-native"));
    }

    let policy = request.fee_policy;
    let anchor_rate = public_broadcaster_anchor_rate_for_policy(
        request.anchor_cache.as_ref(),
        request.chain_id,
        request.fee_token,
    );
    let broadcaster = if let Some(profile) = executor_profile {
        let candidates = public_broadcaster_candidates(
            &request.fee_rows,
            request.chain_id,
            request.fee_token,
            SystemTime::now(),
            policy,
            anchor_rate,
        )
        .into_iter()
        .filter(|candidate| {
            ExecutorDelivery::PublicBroadcaster(Box::new(candidate.clone()))
                .admit(profile)
                .is_ok()
        })
        .collect::<Vec<_>>();
        select_public_broadcaster_with_policy_and_trust(
            &candidates,
            &request.selection,
            policy,
            &request.trust_filter,
        )?
    } else {
        public_broadcaster_for_request(
            &request.fee_rows,
            request.chain_id,
            request.fee_token,
            &request.selection,
            policy,
            &request.trust_filter,
            anchor_rate,
            request
                .executor
                .as_ref()
                .map(|prepared| prepared.delivery()),
            None,
        )?
    };
    if let Some(executor) = &request.executor {
        executor.require_broadcaster(&broadcaster)?;
    }
    let min_gas_price = buffered_gas_price_from_broker(
        http,
        chain.rpc_route.for_class(RpcTrafficClass::General)?,
        &chain.gas,
    )
    .await?;
    let pinned = PublicBroadcasterInputPin::active(request.input_pin.as_ref());
    // Executor retries are bound by their operation's nonce instead; a pin can't apply to them.
    if pinned.is_some() && uses_executor {
        return Err(PublicBroadcasterInputPinUnavailable.into());
    }
    let utxos = if let Some(prepared) = request.executor.as_deref() {
        request.session.unspent_utxos_for_executor(prepared)?
    } else if let Some(operation) = request.reused_executor_operation {
        request.session.unspent_utxos_for_operation(operation)?
    } else {
        request.session.unspent_utxos()
    };
    let same_token_fee = request.fee_token == request.token;
    let native_top_up = request
        .native_top_up
        .as_ref()
        .map(|_| {
            desktop_native_top_up_plan_for_estimate(
                request.chain_id,
                &chain,
                request.token,
                request.recipient,
                request.unwrap,
                request.amount,
            )
        })
        .transpose()?;
    let initial_fee_amount = initial_public_broadcaster_fee_amount(
        &broadcaster,
        chain.gas.gas_limit_buffer,
        min_gas_price,
        same_token_fee,
        || {
            let seed_split = public_broadcaster_amount_split_for_tokens_and_protocol(
                request.amount,
                U256::ZERO,
                request.fee_mode,
                same_token_fee,
                RAILGUN_PROTOCOL_FEE_BPS,
            )?;
            if let Some(native_top_up) = &native_top_up {
                return native_top_up_approximate_shape(
                    &utxos,
                    request.token,
                    request.fee_token,
                    seed_split.receiver_amount,
                    U256::ZERO,
                    native_top_up,
                )
                .map(|shape| shape.with_executor(uses_executor));
            }
            let selection = unshield_selection_info_with_separate_broadcaster_fee_seed(
                &utxos,
                request.token,
                request.fee_token,
                seed_split.receiver_amount,
                false,
            )
            .map_err(|error| {
                public_broadcaster_build_error(
                    error,
                    U256::ZERO,
                    seed_split.fee_mode,
                    same_token_fee,
                    RAILGUN_PROTOCOL_FEE_BPS,
                )
            })?;
            Ok(
                unshield_approximate_shape(&selection, selection.max_spendable, request.unwrap)
                    .with_executor(uses_executor),
            )
        },
    )?;

    let mut estimate = approximate_public_broadcaster_cost(
        broadcaster,
        chain.gas.gas_limit_buffer,
        request.token,
        request.fee_token,
        request.amount,
        request.fee_mode,
        RAILGUN_PROTOCOL_FEE_BPS,
        min_gas_price,
        initial_fee_amount,
        request.custom_fee_amount,
        |split| {
            if let Some(native_top_up) = &native_top_up {
                return native_top_up_approximate_shape(
                    &utxos,
                    request.token,
                    request.fee_token,
                    split.receiver_amount,
                    split.fee_amount,
                    native_top_up,
                )
                .map(|shape| shape.with_executor(uses_executor));
            }
            let selection = unshield_selection_info_with_broadcaster_fee_token(
                &utxos,
                request.token,
                request.fee_token,
                split.receiver_amount,
                split.fee_amount,
                false,
            )
            .map_err(|error| {
                public_broadcaster_build_error(
                    error,
                    split.fee_amount,
                    split.fee_mode,
                    same_token_fee,
                    RAILGUN_PROTOCOL_FEE_BPS,
                )
            })?;
            Ok(
                unshield_approximate_shape(&selection, selection.max_spendable, request.unwrap)
                    .with_executor(uses_executor),
            )
        },
    )?;
    require_pinned_estimate(pinned.is_some(), &estimate)?;
    let reported_amounts = public_broadcaster_reported_amounts(
        request.token,
        request.fee_token,
        PublicBroadcasterAmountSplit {
            entered_amount: estimate.entered_amount,
            receiver_amount: estimate.receiver_amount,
            total_private_spend: estimate.total_private_spend,
            fee_amount: estimate.fee_amount,
            fee_mode: estimate.fee_mode,
        },
        RAILGUN_PROTOCOL_FEE_BPS,
        native_top_up.as_ref(),
    );
    estimate.recipient_amount = reported_amounts.recipient_amount;
    estimate.total_private_spend = reported_amounts.total_private_spend;
    estimate.protocol_fee_amount = reported_amounts.protocol_fee_amount;
    estimate.native_top_up = native_top_up;
    Ok(estimate)
}

/// The estimate includes `gas_limit_buffer`, which self-broadcast adds to the RPC estimate.
pub fn estimate_desktop_send_self_broadcast_cost(
    chain_id: u64,
    gas_limit_buffer: u64,
    utxos: &[Utxo],
    token: Address,
    amount: U256,
    quote: SelfBroadcastGasFeeQuote,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Result<DesktopSelfBroadcastCostEstimate> {
    let selection = send_selection_info(utxos, token, amount, false)
        .wrap_err("select POI-verified send notes for self-broadcast estimate")?;
    let shape = send_approximate_shape(&selection, selection.max_spendable);
    Ok(desktop_self_broadcast_cost_estimate(
        chain_id,
        gas_limit_buffer,
        shape,
        quote,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        Vec::new(),
    ))
}

/// `executor` includes delegation overhead before an account is reserved. The estimate
/// includes `gas_limit_buffer`, which self-broadcast adds to the RPC estimate. This estimate
/// does not authorize signing or delivery. The executor quote becomes
/// `ExecutorUnshieldReview::maximum_gas`, the approved gas ceiling, so its `gas_limit` uses
/// `UpperBound` gas plus any Arbitrum One data gas allowance; its expected cost uses `Expected`
/// gas.
pub fn estimate_desktop_unshield_self_broadcast_cost(
    chain_id: u64,
    gas_limit_buffer: u64,
    executor: bool,
    utxos: &[Utxo],
    token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    unwrap: bool,
    native_top_up: Option<&DesktopNativeTopUpPlan>,
    quote: SelfBroadcastGasFeeQuote,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> Result<DesktopSelfBroadcastCostEstimate> {
    if executor != (unwrap || native_top_up.is_some()) {
        return Err(eyre!("executor quote does not match the selected action"));
    }
    let receiver_amount = unshield_receiver_amount_for_fee_mode(entered_amount, fee_mode)?;
    let shape = if let Some(native_top_up) = native_top_up {
        native_top_up_approximate_shape(
            utxos,
            token,
            token,
            receiver_amount,
            U256::ZERO,
            native_top_up,
        )?
    } else {
        let selection = unshield_selection_info(utxos, token, receiver_amount, false)
            .wrap_err("select POI-verified unshield notes for self-broadcast estimate")?;
        unshield_approximate_shape(&selection, selection.max_spendable, unwrap)
    };
    let protocol_fees = if let Some(native_top_up) = native_top_up {
        if token == native_top_up.wrapped_native_token {
            let combined_amount = native_top_up_required_wrapped_native_amount(
                token,
                native_top_up.wrapped_native_token,
                receiver_amount,
                native_top_up.native_amount,
            );
            vec![DesktopSelfBroadcastProtocolFee {
                token,
                amount: combined_amount
                    .saturating_sub(native_top_up_net_after_protocol_fee(combined_amount)),
            }]
        } else {
            vec![
                DesktopSelfBroadcastProtocolFee {
                    token,
                    amount: unshield_protocol_fee_amount_for_fee_mode(entered_amount, fee_mode)?,
                },
                DesktopSelfBroadcastProtocolFee {
                    token: native_top_up.wrapped_native_token,
                    amount: native_top_up
                        .wrapped_native_amount
                        .saturating_sub(native_top_up.native_amount),
                },
            ]
        }
    } else {
        vec![DesktopSelfBroadcastProtocolFee {
            token,
            amount: unshield_protocol_fee_amount_for_fee_mode(entered_amount, fee_mode)?,
        }]
    };
    Ok(desktop_self_broadcast_cost_estimate(
        chain_id,
        gas_limit_buffer,
        shape.with_executor(executor),
        quote,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        protocol_fees,
    ))
}

fn desktop_self_broadcast_cost_estimate(
    chain_id: u64,
    gas_limit_buffer: u64,
    shape: ApproximateTransactionShape,
    quote: SelfBroadcastGasFeeQuote,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    protocol_fees: Vec<DesktopSelfBroadcastProtocolFee>,
) -> DesktopSelfBroadcastCostEstimate {
    let model = RailgunGasModel::for_chain(chain_id);
    let expected_gas = approximate_public_broadcaster_gas(model, GasEstimateMode::Expected, shape)
        .saturating_add(gas_limit_buffer);
    let gas_limit = if shape.executor {
        // The executor quote is the approved gas ceiling for the signed transaction.
        let execution_gas =
            approximate_public_broadcaster_gas(model, GasEstimateMode::UpperBound, shape);
        execution_gas
            .saturating_add(gas_limit_buffer)
            .saturating_add(arbitrum_data_gas_allowance(chain_id, execution_gas))
    } else {
        expected_gas
    };
    let mut gas_cost =
        eip1559_gas_cost_projection(gas_limit, quote, max_fee_per_gas, max_priority_fee_per_gas);
    gas_cost.expected_cost = U256::from(expected_gas) * U256::from(gas_cost.expected_fee_per_gas);
    DesktopSelfBroadcastCostEstimate {
        gas_limit,
        gas_cost,
        protocol_fees,
    }
}

pub async fn estimate_desktop_send_public_broadcaster_cost(
    request: DesktopSendPublicBroadcasterEstimateRequest,
    http: &HttpContext,
) -> Result<PublicBroadcasterCostEstimate> {
    if request.session.chain_id != request.chain_id {
        return Err(eyre!(
            "selected wallet session is for chain {}, not {}",
            request.session.chain_id,
            request.chain_id
        ));
    }
    parse_railgun_recipient(&request.recipient)?;

    let chain = effective_desktop_chain_config(request.chain_id, &request.effective_chain)?;
    let policy = request.fee_policy;
    let anchor_rate = public_broadcaster_anchor_rate_for_policy(
        request.anchor_cache.as_ref(),
        request.chain_id,
        request.fee_token,
    );
    let candidates = public_broadcaster_candidates(
        &request.fee_rows,
        request.chain_id,
        request.fee_token,
        SystemTime::now(),
        policy,
        anchor_rate,
    );
    let broadcaster = select_public_broadcaster_with_policy_and_trust(
        &candidates,
        &request.selection,
        policy,
        &request.trust_filter,
    )?;
    let min_gas_price = buffered_gas_price_from_broker(
        http,
        chain.rpc_route.for_class(RpcTrafficClass::General)?,
        &chain.gas,
    )
    .await?;
    let pinned = PublicBroadcasterInputPin::active(request.input_pin.as_ref());
    let utxos = request.session.unspent_utxos();
    let same_token_fee = request.fee_token == request.token;
    let initial_fee_amount = initial_public_broadcaster_fee_amount(
        &broadcaster,
        chain.gas.gas_limit_buffer,
        min_gas_price,
        same_token_fee,
        || {
            let selection = send_selection_info_with_separate_broadcaster_fee_seed(
                &utxos,
                request.token,
                request.fee_token,
                request.amount,
                false,
            )
            .map_err(|error| {
                public_broadcaster_build_error(
                    error,
                    U256::ZERO,
                    FeeHandlingMode::AddToAmount,
                    same_token_fee,
                    U256::ZERO,
                )
            })?;
            Ok(send_approximate_shape(&selection, selection.max_spendable))
        },
    )?;

    let estimate = approximate_public_broadcaster_cost(
        broadcaster,
        chain.gas.gas_limit_buffer,
        request.token,
        request.fee_token,
        request.amount,
        request.fee_mode,
        U256::ZERO,
        min_gas_price,
        initial_fee_amount,
        request.custom_fee_amount,
        |split| {
            let selection = send_selection_info_with_broadcaster_fee_token(
                &utxos,
                request.token,
                request.fee_token,
                split.receiver_amount,
                split.fee_amount,
                false,
            )
            .map_err(|error| {
                public_broadcaster_build_error(
                    error,
                    split.fee_amount,
                    split.fee_mode,
                    same_token_fee,
                    U256::ZERO,
                )
            })?;
            Ok(send_approximate_shape(&selection, selection.max_spendable))
        },
    )?;
    require_pinned_estimate(pinned.is_some(), &estimate)?;
    Ok(estimate)
}
