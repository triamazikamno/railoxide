use super::*;
use crate::rpc_broker::actor::JobExecutor;
use crate::rpc_broker::model::{
    DEFAULT_ATTEMPT_TIMEOUT, RpcChainRoute, RpcSubmission, WalletRpcOrigin,
};
use crate::rpc_broker::resolution::{WaiterPolicy, WorkKey};
use crate::rpc_broker::tests::{
    RpcMockGate, RpcResponder, aggregate_response, data_result, read_calldata, remote_error,
    rpc_error, rpc_read_from_request, rpc_result, spawn_gated_rpc_mock, spawn_rpc_mock,
    spawn_status_rpc_mock, test_broker, test_broker_with_executor, test_origin, test_resources,
    test_route,
};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, U256, hex};
use alloy::rpc::types::eth::transaction::{TransactionInput, TransactionRequest};
use alloy::sol_types::SolType;
use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

async fn execute_aggregate_with_failover(
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    route: RpcRoute,
    items: &[WorkItem],
) -> AggregateFailoverOutput {
    let endpoints = route.endpoints().to_vec();
    let output =
        execute_aggregate_with_failover_attempt(client, semaphore, route, endpoints, items).await;
    AggregateFailoverOutput {
        values: output
            .values
            .into_iter()
            .map(AttemptOutcome::into_result)
            .collect(),
        attempts: output.attempts,
    }
}

struct AggregateFailoverOutput {
    values: Vec<Result<RpcResult, RpcBrokerError>>,
    attempts: Vec<PhysicalAttempt>,
}

async fn execute_single_with_shared_waiters(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    read: &RpcRead,
    chain_id: u64,
    endpoints: &[SensitiveUrl],
    waiters: Arc<WaiterState>,
    fallback_attempt_timeout: Duration,
) -> (
    Result<RpcResult, RpcBrokerError>,
    Vec<(Url, EndpointHealthOutcome)>,
) {
    let (result, attempted) = execute_single_with_shared_waiters_attempt(
        client,
        semaphore,
        read,
        chain_id,
        endpoints,
        waiters,
        fallback_attempt_timeout,
    )
    .await;
    (result.into_result(), attempted)
}

async fn rpc_request(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    endpoint: &Url,
    method: &str,
    params: Value,
    preserve_revert: bool,
    deadline: Option<Instant>,
    attempt_timeout: Duration,
) -> Result<Value, RpcBrokerError> {
    rpc_request_attempt(
        client,
        semaphore,
        endpoint,
        method,
        params,
        preserve_revert,
        deadline,
        attempt_timeout,
    )
    .await
    .into_result()
}

async fn rpc_request_attempt(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    endpoint: &Url,
    method: &str,
    params: Value,
    preserve_revert: bool,
    deadline: Option<Instant>,
    attempt_timeout: Duration,
) -> AttemptOutcome<Value> {
    let body = wire_request(method, params);
    let value =
        execute_wire_request(client, semaphore, endpoint, body, deadline, attempt_timeout).await;
    value.and_then(|value| AttemptOutcome::from(parse_rpc_result(&value, preserve_revert)))
}

async fn execute_wire_request(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    endpoint: &Url,
    body: Request<Value>,
    deadline: Option<Instant>,
    attempt_timeout: Duration,
) -> AttemptOutcome<Value> {
    let permit = match acquire_permit(semaphore, deadline).await {
        PermitAcquisition::Acquired(permit) => permit,
        PermitAcquisition::DeadlineElapsed => return AttemptOutcome::NotDispatched,
        PermitAcquisition::Closed => return AttemptOutcome::CallerError(RpcBrokerError::Shutdown),
    };
    execute_wire_request_with_acquired_permit(
        client,
        endpoint,
        permit,
        body,
        deadline,
        None,
        attempt_timeout,
    )
    .await
}

alloy::sol! {
    interface TypedHelperTest {
        function read() external view returns (uint256);
    }
}

#[path = "execution/aggregate_dispatch.rs"]
mod aggregate_dispatch;
#[path = "execution/errors_and_endpoints.rs"]
mod errors_and_endpoints;
#[path = "execution/reductions.rs"]
mod reductions;
