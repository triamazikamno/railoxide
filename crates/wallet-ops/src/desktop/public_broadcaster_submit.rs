use super::public_broadcaster_node::{
    SUBMISSION_NODE_READY_TIMEOUT, SUBMISSION_ROUTE_LOSS_GRACE, SUBMISSION_ROUTE_POLL_INTERVAL,
    SUBMISSION_ROUTE_RECOVERY_LIMIT, SubmissionNode,
};
use super::*;
use alloy::sol_types::SolCall;
use broadcaster_core::contracts::railgun::{RelayAdapt7702, relayCall};
use broadcaster_core::transact::{
    BroadcasterAuthorization, BroadcasterTransactRequestType, compute_railgun_txid,
    railgun_txid_leaf_hash,
};
use eyre::eyre;
use futures_util::future::BoxFuture;

mod delivery;
mod send_preparation;
mod unshield_preparation;

pub(in crate::desktop) use delivery::submit_public_broadcaster_plan;
pub(crate) use delivery::{PublicBroadcasterDelivery, submit_public_broadcaster_transaction};
pub(in crate::desktop) use send_preparation::prepare_desktop_send_public_broadcaster;
pub(in crate::desktop) use unshield_preparation::prepare_desktop_unshield_public_broadcaster;

/// Keeps the tracing target of the moved functions at this module's path.
const LOG_TARGET: &str = module_path!();

/// Logs the approximate fee the first proof starts from. Without an approximation, only a
/// same-token fee with no custom amount may start at zero.
fn initial_public_broadcaster_fee_estimate(
    action: &'static str,
    estimate: Result<PublicBroadcasterCostEstimate>,
    zero_fallback_allowed: bool,
    broadcaster: &PublicBroadcasterCandidate,
    min_gas_price: u128,
    bound_min_gas_price: u128,
) -> Result<Option<PublicBroadcasterCostEstimate>> {
    match estimate {
        Ok(estimate) => {
            tracing::debug!(
                fee_amount = %estimate.fee_amount,
                gas_limit = estimate.gas_limit,
                min_gas_price,
                bound_min_gas_price,
                transaction_count = estimate.transaction_count,
                input_count = estimate.input_count,
                private_output_count = estimate.private_output_count,
                public_output_count = estimate.public_output_count,
                broadcaster = %broadcaster.railgun_address,
                fees_id = %broadcaster.fees_id,
                "using approximate public broadcaster {action} fee for first proof"
            );
            Ok(Some(estimate))
        }
        Err(err) => {
            if !zero_fallback_allowed {
                return Err(err)
                    .wrap_err(format!("estimate initial public broadcaster {action} fee"));
            }
            // The error can carry the private spendable limit, so it stays out of the log.
            tracing::warn!(
                broadcaster = %broadcaster.railgun_address,
                fees_id = %broadcaster.fees_id,
                "failed to estimate initial same-token public broadcaster {action} fee; starting at zero"
            );
            Ok(None)
        }
    }
}

/// The fee the first proof carries: a custom fee validated against the reviewed limit, or the
/// initial estimate capped by that limit.
fn starting_public_broadcaster_fee(
    custom_fee_amount: Option<U256>,
    maximum_fee: Option<U256>,
    initial_fee_estimate: Option<&PublicBroadcasterCostEstimate>,
) -> Result<U256> {
    let initial_fee_amount =
        initial_fee_estimate.map_or(U256::ZERO, |estimate| estimate.fee_amount);
    match custom_fee_amount {
        Some(amount) => validate_custom_public_broadcaster_fee(amount, U256::ZERO, maximum_fee),
        None => Ok(maximum_fee.map_or(initial_fee_amount, |maximum| {
            initial_fee_amount.min(maximum)
        })),
    }
}

pub(super) async fn estimate_public_broadcaster_fee(
    provider: &(impl Provider + Clone),
    chain_id: u64,
    transaction: TransactionRequest,
    token_fee_per_unit_gas: U256,
    min_gas_price: u128,
    gas_limit_buffer: u64,
) -> Result<(u64, U256)> {
    let tx_req = if transaction.transaction_type == Some(4) {
        transaction.with_chain_id(chain_id)
    } else {
        transaction
            .with_chain_id(chain_id)
            .with_gas_price(min_gas_price)
    };
    let estimated_gas = super::executors::estimate_delegated_gas(provider, &tx_req)
        .await
        .wrap_err("estimate public broadcaster gas")?;
    let gas_limit = public_broadcaster_gas_limit_with_buffer(estimated_gas, gas_limit_buffer);
    let service_gas_price = public_broadcaster_service_gas_price(min_gas_price);
    Ok((
        gas_limit,
        broadcaster_fee_amount(token_fee_per_unit_gas, gas_limit, service_gas_price),
    ))
}

pub(super) async fn estimate_public_broadcaster_fee_from_rpc_pool(
    query_rpc_pool: &QueryRpcPool,
    chain_id: u64,
    transaction: TransactionRequest,
    token_fee_per_unit_gas: U256,
    min_gas_price: u128,
    gas_limit_buffer: u64,
) -> Result<(u64, U256)> {
    let mut last_error = None;
    for _ in 0..query_rpc_pool.len() {
        let Some(provider_handle) = query_rpc_pool.random_provider() else {
            break;
        };
        match estimate_public_broadcaster_fee(
            &provider_handle.provider,
            chain_id,
            transaction.clone(),
            token_fee_per_unit_gas,
            min_gas_price,
            gas_limit_buffer,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) => {
                let rpc = http::redact_url_for_display(&provider_handle.url);
                tracing::warn!(%error, %rpc, "estimate public broadcaster gas failed");
                query_rpc_pool.mark_bad_provider(&provider_handle);
                last_error = Some(error);
            }
        }
    }
    if let Some(error) = last_error {
        Err(error).wrap_err("all query RPC public broadcaster gas estimate attempts failed")
    } else {
        Err(eyre!("no healthy query RPC available"))
    }
}

pub(crate) const fn public_broadcaster_gas_limit_with_buffer(
    estimated_gas: u64,
    gas_limit_buffer: u64,
) -> u64 {
    estimated_gas.saturating_add(gas_limit_buffer)
}

pub(crate) fn public_broadcaster_transact_params(
    broadcaster: &PublicBroadcasterCandidate,
    transaction: TransactionRequest,
    min_gas_price: u128,
    pre_transaction_pois_per_txid_leaf_per_list: PreTransactionPoiMap,
) -> Result<BroadcasterRawParamsTransact> {
    check_broadcaster_representable(&transaction, broadcaster.chain_id)?;
    let to = transaction
        .to
        .and_then(alloy::primitives::TxKind::into_to)
        .ok_or_else(|| eyre!("broadcaster transaction destination is unavailable"))?;
    let data = transaction
        .input
        .into_input()
        .ok_or_else(|| eyre!("broadcaster transaction calldata is unavailable"))?;
    let authorization = broadcaster_authorization(
        transaction.authorization_list.as_deref(),
        transaction.transaction_type,
    )?;
    let has_fee_caps =
        transaction.max_fee_per_gas.is_some() && transaction.max_priority_fee_per_gas.is_some();
    if authorization.is_some() && !has_fee_caps {
        return Err(eyre!(
            "executor broadcaster delivery requires approved gas fee caps"
        ));
    }
    let uses_executor = data.starts_with(&RelayAdapt7702::executeCall::SELECTOR);
    if uses_executor {
        let call = RelayAdapt7702::executeCall::abi_decode(&data)?;
        if call._transactions.is_empty() {
            return Err(eyre!("executor broadcaster call has no transactions"));
        }
        check_executor_authorization(broadcaster, authorization.as_ref(), to)?;
        check_executor_pois(
            broadcaster,
            &call,
            &pre_transaction_pois_per_txid_leaf_per_list,
        )?;
    } else if authorization.is_some() || data.starts_with(&RelayAdapt7702::multicallCall::SELECTOR)
    {
        return Err(eyre!(
            "broadcaster executor delivery requires the current execute wrapper"
        ));
    }
    let mut other =
        sdk_broadcaster_route_fields(uses_executor || data.starts_with(&relayCall::SELECTOR));
    if authorization.is_some() {
        let gas_limit = transaction.gas.ok_or_else(|| {
            eyre!("executor broadcaster delivery requires an estimated gas limit")
        })?;
        // The shared wrapper has no gas-limit field; SDK TX7702 expects decimal text.
        other.insert("gasLimit".to_owned(), gas_limit.to_string().into());
    }
    Ok(BroadcasterRawParamsTransact {
        chain_type: 0,
        chain_id: broadcaster.chain_id,
        transact_type: authorization
            .as_ref()
            .map(|_| BroadcasterTransactRequestType::Tx7702),
        min_gas_price: Some(U256::from(min_gas_price)),
        max_fee_per_gas: transaction.max_fee_per_gas.map(U256::from),
        max_priority_fee_per_gas: transaction.max_priority_fee_per_gas.map(U256::from),
        authorization,
        fees_id: Some(broadcaster.fees_id.clone()),
        to,
        data,
        broadcaster_viewing_key: FixedBytes::from(broadcaster.viewing_public_key),
        txid_version: Some(DEFAULT_TXID_VERSION.to_string()),
        pre_transaction_pois_per_txid_leaf_per_list,
        other,
    })
}

/// The broadcaster owns the outer sender/nonce and this wire format has no
/// value field. Refuse an unrepresentable request instead of dropping fields.
fn check_broadcaster_representable(transaction: &TransactionRequest, chain_id: u64) -> Result<()> {
    let unrepresentable =
        || eyre!("transaction cannot be represented by the selected broadcaster route");
    if transaction.chain_id.is_some_and(|chain| chain != chain_id) {
        return Err(unrepresentable());
    }
    if transaction.from.is_some() || transaction.nonce.is_some() {
        return Err(unrepresentable());
    }
    if transaction.value.is_some_and(|value| value != U256::ZERO) {
        return Err(unrepresentable());
    }
    if transaction
        .transaction_type
        .is_some_and(|kind| kind != 2 && kind != 4)
    {
        return Err(unrepresentable());
    }
    Ok(())
}

/// The single delegation authorization of a type-4 request, or `None` for any other type.
fn broadcaster_authorization(
    authorization_list: Option<&[alloy::eips::eip7702::SignedAuthorization]>,
    transaction_type: Option<u8>,
) -> Result<Option<BroadcasterAuthorization>> {
    let authorization = match authorization_list {
        None => None,
        Some([authorization]) => Some(BroadcasterAuthorization {
            address: authorization.inner().address,
            nonce: U256::from(authorization.inner().nonce),
            chain_id: authorization.inner().chain_id,
            signature: alloy::serde::WithOtherFields::new(authorization.signature()?),
            other: alloy::serde::OtherFields::default(),
        }),
        Some(_) => {
            return Err(eyre!(
                "broadcaster executor delivery requires one delegation authorization"
            ));
        }
    };
    if authorization.is_some() != (transaction_type == Some(4)) {
        return Err(eyre!(
            "broadcaster transaction type does not match its authorization"
        ));
    }
    Ok(authorization)
}

/// The executor delegation must be fresh and name the broadcaster's accepted delegate on its
/// chain, signed by the delivery's destination account.
fn check_executor_authorization(
    broadcaster: &PublicBroadcasterCandidate,
    authorization: Option<&BroadcasterAuthorization>,
    to: Address,
) -> Result<()> {
    let profile = broadcaster
        .relay_adapt_7702
        .and_then(|delegate| settings::ExecutorProfile::accepted(broadcaster.chain_id, delegate))
        .ok_or_else(|| eyre!("broadcaster executor profile is unavailable"))?;
    // The caller decoded the nonce-bearing v1 ABI; a new capability must be handled here.
    let settings::ExecutorCapability::NonceBearingV1 = profile.capability();
    let authorization = authorization.ok_or_else(|| {
        eyre!("executor broadcaster delivery requires a fresh delegation authorization")
    })?;
    let mismatch = || eyre!("executor authorization does not match the selected delivery");
    if authorization.chain_id != U256::from(broadcaster.chain_id)
        || authorization.address != profile.delegate()
    {
        return Err(mismatch());
    }
    if authorization.signed_authorization()?.recover_authority()? != to {
        return Err(mismatch());
    }
    Ok(())
}

/// Every private transaction of an executor call needs a POI on each required list.
fn check_executor_pois(
    broadcaster: &PublicBroadcasterCandidate,
    call: &RelayAdapt7702::executeCall,
    pre_transaction_pois_per_txid_leaf_per_list: &PreTransactionPoiMap,
) -> Result<()> {
    let list_keys = broadcaster.parsed_required_poi_list_keys()?;
    for inner in &call._transactions {
        let txid = compute_railgun_txid(inner, Some(DEFAULT_TXID_VERSION))?;
        let leaf = FixedBytes::from(railgun_txid_leaf_hash(
            txid,
            u64::from(inner.boundParams.treeNumber),
        ));
        if list_keys.iter().any(|key| {
            !pre_transaction_pois_per_txid_leaf_per_list
                .get(key)
                .is_some_and(|proofs| proofs.contains_key(&leaf))
        }) {
            return Err(eyre!(
                "executor request is missing a required private transaction POI"
            ));
        }
    }
    Ok(())
}

/// SDK broadcasters require the route flag even for direct transact requests.
fn sdk_broadcaster_route_fields(use_relay_adapt: bool) -> alloy::serde::OtherFields {
    [
        ("minVersion", serde_json::Value::from("8.0.0")),
        ("maxVersion", serde_json::Value::from("8.999.0")),
        // The shared request type has no devLog field. SDK broadcasters use it
        // to return the original error instead of "Unknown Broadcaster error."
        ("devLog", true.into()),
        ("useRelayAdapt", use_relay_adapt.into()),
    ]
    .into_iter()
    .collect()
}

pub(super) async fn publish_public_broadcaster_payload(
    node: &dyn SubmissionNode,
    pubsub_path: &str,
    transact_topic: &str,
    payload: &[u8],
    attempt: usize,
) -> Result<()> {
    tracing::info!(
        pubsub_path = %pubsub_path,
        transact_topic = %transact_topic,
        payload_len = payload.len(),
        attempt,
        "publishing public broadcaster transact request"
    );
    let publish_started = Instant::now();
    node.publish(transact_topic, payload)
        .await
        .wrap_err("publish public broadcaster transact request")?;
    tracing::info!(
        pubsub_path = %pubsub_path,
        transact_topic = %transact_topic,
        elapsed_ms = publish_started.elapsed().as_millis(),
        attempt,
        "published public broadcaster transact request"
    );
    Ok(())
}

#[cfg(test)]
mod gas_estimate_tests {
    use super::*;
    use alloy::eips::eip7702::Authorization;
    use alloy::signers::{SignerSync, local::PrivateKeySigner};
    use serde_json::json;

    #[tokio::test]
    async fn broadcaster_gas_requires_delegated_execution_and_preserves_legacy_estimates() {
        #[derive(Clone, Copy, Debug)]
        enum Simulation {
            IgnoresDelegation,
            UnderestimatesExecution,
            Executes,
            Legacy,
        }

        let signer = PrivateKeySigner::from_bytes(&FixedBytes::repeat_byte(7)).unwrap();
        let authorization = Authorization {
            chain_id: U256::ONE,
            address: Address::repeat_byte(0x42),
            nonce: 0,
        };
        let signature = signer
            .sign_hash_sync(&authorization.signature_hash())
            .unwrap();
        let authorization = authorization.into_signed(signature);
        let call_data = Bytes::from_static(&[1, 2, 3]);
        for simulation in [
            Simulation::IgnoresDelegation,
            Simulation::UnderestimatesExecution,
            Simulation::Executes,
            Simulation::Legacy,
        ] {
            let mut transaction = TransactionRequest::default()
                .to(signer.address())
                .input(call_data.clone().into());
            let delegated = !matches!(simulation, Simulation::Legacy);
            if delegated {
                transaction.transaction_type = Some(4);
                transaction.authorization_list = Some(vec![authorization.clone()]);
                transaction.max_fee_per_gas = Some(125);
                transaction.max_priority_fee_per_gas = Some(100);
            }
            let expected_authorization =
                serde_json::to_value(&transaction.authorization_list).unwrap();
            let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
                Arc::new(move |request| {
                    assert_eq!(request["params"][1], "pending");
                    let tx = &request["params"][0];
                    assert_eq!(tx["authorizationList"], expected_authorization);
                    let result = match request["method"].as_str().unwrap() {
                        "eth_estimateGas" => json!(if matches!(simulation, Simulation::Executes) {
                            "0x1bc22a" // Full execution: 1,819,178 gas.
                        } else {
                            "0x1c092" // Empty-account execution: 114,834 gas.
                        }),
                        "eth_call" => {
                            assert!(delegated, "legacy estimates need no delegation checks");
                            let tx: TransactionRequest =
                                serde_json::from_value(tx.clone()).unwrap();
                            if tx.input.input().unwrap().as_ref()
                                == RelayAdapt7702::nonceCall::SELECTOR
                            {
                                if matches!(simulation, Simulation::IgnoresDelegation) {
                                    json!("0x")
                                } else {
                                    json!(Bytes::from(U256::ZERO.to_be_bytes::<32>().to_vec()))
                                }
                            } else {
                                assert_eq!(tx.input.input().unwrap().as_ref(), [1, 2, 3]);
                                if tx.gas.unwrap() < 1_768_554 {
                                    return json!({"jsonrpc": "2.0", "id": request["id"],
                                        "error": {"code": -32000, "message": "out of gas"}});
                                }
                                json!("0x")
                            }
                        }
                        method => panic!("unexpected RPC method: {method}"),
                    };
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
                }),
                Arc::default(),
                Arc::default(),
            )
            .await;
            let pool = QueryRpcPool::with_http_client(
                vec![endpoint],
                Duration::from_secs(30),
                HttpContext::direct_for_tests().rpc_client,
            );
            let result = estimate_public_broadcaster_fee_from_rpc_pool(
                &pool,
                1,
                transaction,
                U256::from(1_000_000_000u64),
                100,
                100_000,
            )
            .await;
            server.abort();
            match simulation {
                Simulation::IgnoresDelegation | Simulation::UnderestimatesExecution => {
                    assert!(
                        result.is_err(),
                        "accepted an invalid estimate: {simulation:?}"
                    );
                    assert!(pool.available_providers().is_empty());
                }
                Simulation::Executes | Simulation::Legacy => {
                    let expected = if delegated { 1_919_178 } else { 214_834 };
                    let (gas, fee) = result.unwrap();
                    assert_eq!(gas, expected);
                    assert_eq!(
                        fee,
                        broadcaster_fee_amount(U256::from(1_000_000_000u64), gas, 125)
                    );
                }
            }
        }
    }
}
