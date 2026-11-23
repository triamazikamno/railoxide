#[allow(clippy::wildcard_imports)]
use super::*;

/// L1 data gas that an approved gas ceiling adds to `execution_gas`. Zero outside Arbitrum One.
pub(crate) const fn arbitrum_data_gas_allowance(chain_id: u64, execution_gas: u64) -> u64 {
    if chain_id == 42161 {
        execution_gas.saturating_mul(ARBITRUM_ONE_DATA_GAS_ALLOWANCE_BPS) / 10_000
    } else {
        0
    }
}

/// Gas of a transaction with `shape` in `mode`, including the intrinsic gas and excluding the
/// chain's gas limit buffer. Each public output is one transaction's unshield. Final proved
/// requests use RPC estimation.
pub(crate) const fn approximate_public_broadcaster_gas(
    model: &RailgunGasModel,
    mode: GasEstimateMode,
    shape: ApproximateTransactionShape,
) -> u64 {
    let transact = model.transact(
        mode,
        TransactGasShape {
            transactions: shape.transaction_count,
            inputs: shape.input_count,
            outputs: shape
                .private_output_count
                .saturating_add(shape.public_output_count),
            unshields: shape.public_output_count,
        },
    );
    let relay = if shape.uses_relay_adapt {
        model.relay(shape.relay_call_count)
    } else {
        0
    };
    let executor = if shape.executor { model.executor() } else { 0 };
    TRANSACTION_INTRINSIC_GAS
        .saturating_add(transact)
        .saturating_add(relay)
        .saturating_add(RELAY_NATIVE_UNWRAP_GAS.saturating_mul(shape.unwrap_count as u64))
        .saturating_add(executor)
}

pub(in crate::desktop) const fn gas_shortfall_bps(
    predicted_gas_limit: u64,
    actual_gas_limit: u64,
) -> Option<u64> {
    if predicted_gas_limit == 0 || actual_gas_limit <= predicted_gas_limit {
        return None;
    }
    Some((actual_gas_limit - predicted_gas_limit) * 10_000 / predicted_gas_limit)
}

pub(in crate::desktop) fn log_public_broadcaster_fee_prediction_failure(
    action: &'static str,
    attempt: usize,
    available_fee: U256,
    computed_fee: U256,
    gas_limit: u64,
    estimate: Option<&PublicBroadcasterCostEstimate>,
    plan_transaction_count: usize,
    plan_input_count: usize,
    plan_private_output_count: usize,
    plan_public_output_count: usize,
    broadcaster: &PublicBroadcasterCandidate,
) {
    let predicted_gas_limit = estimate.map(|estimate| estimate.gas_limit);
    let gas_shortfall = predicted_gas_limit.map(|predicted| gas_limit.saturating_sub(predicted));
    let gas_shortfall_bps =
        predicted_gas_limit.and_then(|predicted| gas_shortfall_bps(predicted, gas_limit));
    let estimated_transaction_count = estimate.map(|estimate| estimate.transaction_count);
    let estimated_input_count = estimate.map(|estimate| estimate.input_count);
    let estimated_private_output_count = estimate.map(|estimate| estimate.private_output_count);
    let estimated_public_output_count = estimate.map(|estimate| estimate.public_output_count);
    tracing::warn!(target: LOG_TARGET,
        action,
        attempt,
        fee_shortfall = %computed_fee.saturating_sub(available_fee),
        gas_limit,
        ?predicted_gas_limit,
        ?gas_shortfall,
        ?gas_shortfall_bps,
        plan_transaction_count,
        plan_input_count,
        plan_private_output_count,
        plan_public_output_count,
        ?estimated_transaction_count,
        ?estimated_input_count,
        ?estimated_private_output_count,
        ?estimated_public_output_count,
        broadcaster = %broadcaster.railgun_address,
        fees_id = %broadcaster.fees_id,
        "public broadcaster fee prediction failed; retrying with buffered fee"
    );
}

/// The live fee check adds `gas_limit_buffer` to the RPC estimate, so the quote adds it too.
pub(crate) fn approximate_public_broadcaster_cost(
    broadcaster: PublicBroadcasterCandidate,
    gas_limit_buffer: u64,
    action_token: Address,
    fee_token: Address,
    entered_amount: U256,
    fee_mode: FeeHandlingMode,
    protocol_fee_bps: U256,
    min_gas_price: u128,
    initial_fee_amount: U256,
    custom_fee_amount: Option<U256>,
    mut select_shape: impl FnMut(PublicBroadcasterAmountSplit) -> Result<ApproximateTransactionShape>,
) -> Result<PublicBroadcasterCostEstimate> {
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    let model = RailgunGasModel::for_chain(broadcaster.chain_id);
    let mut fee_amount = custom_fee_amount.map_or(Ok(initial_fee_amount), |amount| {
        validate_custom_public_broadcaster_fee(amount, U256::ZERO, None)
    })?;
    let mut latest_shape = None;
    let mut latest_split = None;
    let mut latest_gas_limit = 0;
    let same_token_fee = action_token == fee_token;

    for _ in 0..PUBLIC_BROADCASTER_FEE_ATTEMPTS {
        let split = public_broadcaster_amount_split_for_tokens_and_protocol(
            entered_amount,
            fee_amount,
            fee_mode,
            same_token_fee,
            protocol_fee_bps,
        )?;
        let shape = select_shape(split)?;
        let gas_limit = approximate_public_broadcaster_gas(model, GasEstimateMode::Expected, shape)
            .saturating_add(gas_limit_buffer);
        let computed_fee = broadcaster_fee_amount(broadcaster.fee, gas_limit, service_gas_price);
        latest_shape = Some(shape);
        latest_split = Some(split);
        latest_gas_limit = gas_limit;
        if broadcaster_fee_covers(fee_amount, computed_fee) {
            let protocol_fee_amount =
                railgun_protocol_fee_amount(split.receiver_amount, protocol_fee_bps);
            return Ok(PublicBroadcasterCostEstimate {
                broadcaster,
                action_token,
                fee_token,
                entered_amount: split.entered_amount,
                receiver_amount: split.receiver_amount,
                recipient_amount: recipient_amount_after_protocol_fee(
                    split.receiver_amount,
                    protocol_fee_amount,
                ),
                total_private_spend: split.total_private_spend,
                fee_amount,
                protocol_fee_amount,
                protocol_fee_bps,
                fee_mode: split.fee_mode,
                max_receiver_amount: shape.max_receiver_amount,
                max_entered_amount: public_broadcaster_max_entered_amount_for_tokens_and_protocol(
                    shape.max_receiver_amount,
                    fee_amount,
                    split.fee_mode,
                    same_token_fee,
                    protocol_fee_bps,
                ),
                gas_limit,
                min_gas_price,
                native_gas_cost: public_broadcaster_native_gas_cost(gas_limit, min_gas_price),
                transaction_count: shape.transaction_count,
                input_count: shape.input_count,
                private_output_count: shape.private_output_count,
                public_output_count: shape.public_output_count,
                relay_call_count: shape.relay_call_count,
                uses_relay_adapt: shape.uses_relay_adapt,
                native_top_up: None,
                planned_unshields: None,
            });
        }
        fee_amount = custom_fee_amount.map_or_else(
            || Ok(buffered_public_broadcaster_fee(computed_fee)),
            |amount| validate_custom_public_broadcaster_fee(amount, computed_fee, None),
        )?;
    }

    let shape = latest_shape.ok_or_else(|| eyre!("could not estimate public broadcaster cost"))?;
    let split = latest_split.ok_or_else(|| eyre!("could not estimate public broadcaster cost"))?;
    let protocol_fee_amount = railgun_protocol_fee_amount(split.receiver_amount, protocol_fee_bps);
    Ok(PublicBroadcasterCostEstimate {
        broadcaster,
        action_token,
        fee_token,
        entered_amount: split.entered_amount,
        receiver_amount: split.receiver_amount,
        recipient_amount: recipient_amount_after_protocol_fee(
            split.receiver_amount,
            protocol_fee_amount,
        ),
        total_private_spend: split.total_private_spend,
        fee_amount: split.fee_amount,
        protocol_fee_amount,
        protocol_fee_bps,
        fee_mode: split.fee_mode,
        max_receiver_amount: shape.max_receiver_amount,
        max_entered_amount: public_broadcaster_max_entered_amount_for_tokens_and_protocol(
            shape.max_receiver_amount,
            split.fee_amount,
            split.fee_mode,
            same_token_fee,
            protocol_fee_bps,
        ),
        gas_limit: latest_gas_limit,
        min_gas_price,
        native_gas_cost: public_broadcaster_native_gas_cost(latest_gas_limit, min_gas_price),
        transaction_count: shape.transaction_count,
        input_count: shape.input_count,
        private_output_count: shape.private_output_count,
        public_output_count: shape.public_output_count,
        relay_call_count: shape.relay_call_count,
        uses_relay_adapt: shape.uses_relay_adapt,
        native_top_up: None,
        planned_unshields: None,
    })
}

/// Like [`approximate_public_broadcaster_cost`], the seed fee includes `gas_limit_buffer`.
pub(crate) fn initial_separate_token_public_broadcaster_fee(
    broadcaster: &PublicBroadcasterCandidate,
    gas_limit_buffer: u64,
    min_gas_price: u128,
    seed_shape: ApproximateTransactionShape,
) -> U256 {
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    let gas_limit = approximate_public_broadcaster_gas(
        RailgunGasModel::for_chain(broadcaster.chain_id),
        GasEstimateMode::Expected,
        seed_shape,
    )
    .saturating_add(gas_limit_buffer);
    buffered_public_broadcaster_fee(broadcaster_fee_amount(
        broadcaster.fee,
        gas_limit,
        service_gas_price,
    ))
}

pub(in crate::desktop) fn initial_public_broadcaster_fee_amount(
    broadcaster: &PublicBroadcasterCandidate,
    gas_limit_buffer: u64,
    min_gas_price: u128,
    same_token_fee: bool,
    seed_shape: impl FnOnce() -> Result<ApproximateTransactionShape>,
) -> Result<U256> {
    if same_token_fee {
        Ok(U256::ZERO)
    } else {
        Ok(initial_separate_token_public_broadcaster_fee(
            broadcaster,
            gas_limit_buffer,
            min_gas_price,
            seed_shape()?,
        ))
    }
}
