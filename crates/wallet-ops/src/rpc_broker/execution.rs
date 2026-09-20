use super::actor::{EndpointHealthOutcome, JobOutput, RequestEvent};
use super::model::{
    FailureClass, MAX_REDUCTION_ATTEMPTS_PER_ENDPOINT, RpcBrokerError, RpcRead, RpcRemoteError,
    RpcResult, RpcRevert, RpcRoute,
};
use super::operation::RpcOperation;
use super::resolution::{WaiterSnapshot, WaiterState, WorkItem};
use super::scheduler::ExecutionJob;
use alloy::network::Ethereum;
use alloy::primitives::{Bytes, TxKind};
use alloy::providers::EthCallParams;
use alloy::providers::bindings::IMulticall3;
use alloy::rpc::json_rpc::{ErrorPayload, Id, Request};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::serde::WithOtherFields;
use alloy::sol_types::SolCall;
use futures_util::future::BoxFuture;
use poi::SensitiveUrl;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{self, Instant};
use url::Url;

enum PermitAcquisition {
    Acquired(OwnedSemaphorePermit),
    DeadlineElapsed,
    Closed,
}

/// The result of one physical attempt. Reduction and admission control stay inside this
/// boundary; only `CallerError` values are converted into public broker results.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AttemptOutcome<T> {
    Success(T),
    CallerError(RpcBrokerError),
    Recoverable(RpcBrokerError),
    ReductionLimit(RpcBrokerError),
    NotDispatched,
}

impl<T> AttemptOutcome<T> {
    fn into_result(self) -> Result<T, RpcBrokerError> {
        match self {
            Self::Success(value) => Ok(value),
            Self::CallerError(error) | Self::Recoverable(error) | Self::ReductionLimit(error) => {
                Err(error)
            }
            Self::NotDispatched => Err(RpcBrokerError::TimeoutBeforeDispatch),
        }
    }

    fn and_then<U>(self, and_then: impl FnOnce(T) -> AttemptOutcome<U>) -> AttemptOutcome<U> {
        match self {
            Self::Success(value) => and_then(value),
            Self::CallerError(error) => AttemptOutcome::CallerError(error),
            Self::Recoverable(error) => AttemptOutcome::Recoverable(error),
            Self::ReductionLimit(error) => AttemptOutcome::ReductionLimit(error),
            Self::NotDispatched => AttemptOutcome::NotDispatched,
        }
    }
}

impl<T> From<Result<T, RpcBrokerError>> for AttemptOutcome<T> {
    fn from(result: Result<T, RpcBrokerError>) -> Self {
        match result {
            Ok(value) => Self::Success(value),
            Err(error) if error.failure_class() == FailureClass::RecoverableByReduction => {
                Self::Recoverable(error)
            }
            Err(error) => Self::CallerError(error),
        }
    }
}

pub(super) fn run_job(
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    job: ExecutionJob,
    endpoints: Vec<SensitiveUrl>,
) -> BoxFuture<'static, JobOutput> {
    Box::pin(async move {
        let mut identity_events = Vec::new();
        let endpoints =
            verify_job_endpoints(&client, &semaphore, &job, endpoints, &mut identity_events).await;
        let mut output = match job {
            ExecutionJob::Aggregate(group) => {
                let Some(route) = group.first().map(|item| item.execution_route.clone()) else {
                    return JobOutput::noop();
                };
                let values = execute_aggregate_with_failover_attempt(
                    client,
                    semaphore,
                    route.clone(),
                    endpoints,
                    &group,
                )
                .await;
                JobOutput {
                    completions: group
                        .into_iter()
                        .zip(values.values)
                        .map(|(item, value)| (item.key, value.into_result()))
                        .collect(),
                    requests: values
                        .attempts
                        .into_iter()
                        .map(|attempt| {
                            RequestEvent::new(
                                route.chain_id(),
                                attempt.endpoint,
                                attempt.health_outcome,
                            )
                        })
                        .collect(),
                }
            }
            ExecutionJob::Individual(item) => {
                let route = item.execution_route.clone();
                let (value, attempted_endpoints) = execute_single_with_shared_waiters_attempt(
                    &client,
                    &semaphore,
                    &item.read,
                    route.chain_id(),
                    &endpoints,
                    item.waiters.clone(),
                    route.attempt_timeout,
                )
                .await;
                JobOutput {
                    completions: vec![(item.key, value.into_result())],
                    requests: attempted_endpoints
                        .into_iter()
                        .map(|(endpoint, outcome)| {
                            RequestEvent::new(route.chain_id(), endpoint, outcome)
                        })
                        .collect(),
                }
            }
        };
        output.requests.extend(identity_events);
        output
    })
}

async fn verify_job_endpoints(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    job: &ExecutionJob,
    endpoints: Vec<SensitiveUrl>,
    events: &mut Vec<RequestEvent>,
) -> Vec<SensitiveUrl> {
    let items: &[WorkItem] = match job {
        ExecutionJob::Aggregate(items) => items,
        ExecutionJob::Individual(item) => std::slice::from_ref(item),
    };
    let Some(route) = items.first().map(|item| &item.execution_route) else {
        return endpoints;
    };
    if !route.chain_route().requires_identity_verification() {
        return endpoints;
    }
    let members: Vec<_> = items
        .iter()
        .map(|item| AggregateMember {
            read: item.read.clone(),
            waiters: item.waiters.clone(),
        })
        .collect();
    let indices: Vec<_> = (0..members.len()).collect();
    let mut verified = Vec::new();
    for endpoint in endpoints {
        if route.chain_route().endpoint_identity_verified(&endpoint) {
            verified.push(endpoint);
            continue;
        }
        let permit = loop {
            let current = ScopedView::new(&members, &indices, Instant::now());
            if current.members.is_empty() {
                return verified;
            }
            match acquire_permit(semaphore, current.earliest_permit_deadline()).await {
                PermitAcquisition::Acquired(permit) => break permit,
                PermitAcquisition::DeadlineElapsed => {}
                PermitAcquisition::Closed => return verified,
            }
        };
        let now = Instant::now();
        let current = ScopedView::new(&members, &indices, now);
        if current.members.is_empty() {
            return verified;
        }
        let timeout = current.min_attempt_timeout(route.attempt_timeout);
        let timeout = current
            .latest_physical_deadline()
            .map_or(timeout, |deadline| {
                timeout.min(deadline.saturating_duration_since(now))
            });
        let result = route
            .chain_route()
            .verify_endpoint_identity(client, &endpoint, timeout)
            .await;
        if result.is_ok() {
            verified.push(endpoint);
        } else {
            events.push(RequestEvent::new(
                route.chain_id(),
                endpoint.expose_url().clone(),
                EndpointHealthOutcome::for_result(&result),
            ));
        }
        drop(permit);
    }
    verified
}

async fn execute_aggregate_with_failover_attempt(
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    route: RpcRoute,
    endpoints: Vec<SensitiveUrl>,
    items: &[WorkItem],
) -> ReductionOutput {
    let members: Arc<[AggregateMember]> = items
        .iter()
        .map(|item| AggregateMember {
            read: item.read.clone(),
            waiters: item.waiters.clone(),
        })
        .collect::<Vec<_>>()
        .into();
    let mut unresolved = (0..members.len()).collect::<Vec<_>>();
    let mut final_values = vec![
        AttemptOutcome::CallerError(RpcBrokerError::NoEndpoint {
            chain_id: route.chain_id(),
        });
        members.len()
    ];
    let mut attempts = Vec::new();
    let mut dispatched = vec![false; members.len()];
    for endpoint in endpoints.iter().map(SensitiveUrl::expose_url) {
        if unresolved.is_empty() {
            break;
        }
        let now = Instant::now();
        let live = ScopedView::new(&members, &unresolved, now).indices();
        for index in unresolved
            .iter()
            .copied()
            .filter(|index| !live.contains(index))
        {
            if !dispatched[index] {
                final_values[index] = AttemptOutcome::NotDispatched;
            }
        }
        unresolved = live;
        if unresolved.is_empty() {
            break;
        }
        let previously_dispatched = dispatched.clone();
        let context = Arc::new(ReductionContext {
            client: client.clone(),
            semaphore: semaphore.clone(),
            route: route.clone(),
            endpoint: endpoint.clone(),
            members: Arc::clone(&members),
            reduction_budget: Arc::new(AtomicUsize::new(0)),
        });
        let output = reduce(context, unresolved.clone()).await;
        for attempt in &output.attempts {
            for index in &attempt.indices {
                dispatched[*index] = true;
            }
        }
        attempts.extend(output.attempts);
        let mut next = Vec::new();
        for index in unresolved {
            let value = &output.values[index];
            if !(matches!(value, AttemptOutcome::NotDispatched) && previously_dispatched[index]) {
                final_values[index] = value.clone();
            }
            if should_failover_aggregate_outcome(value) {
                next.push(index);
            }
        }
        unresolved = next;
    }
    ReductionOutput {
        values: final_values,
        attempts,
    }
}

/// Reverts and the fixed local body cap are terminal. Other failures retain endpoint failover.
pub(super) const fn should_failover_error(error: &RpcBrokerError) -> bool {
    !matches!(
        error,
        RpcBrokerError::InnerRevert(_) | RpcBrokerError::ResponseTooLarge
    )
}

const fn should_failover_aggregate_outcome(outcome: &AttemptOutcome<RpcResult>) -> bool {
    match outcome {
        // Member reverts arrive as `InnerRevert` under `requireSuccess=false`, never as
        // `Recoverable`, so a recoverable error that outlived reduction is an endpoint
        // condition and moves to the next endpoint like a reduction limit does.
        AttemptOutcome::ReductionLimit(_) | AttemptOutcome::Recoverable(_) => true,
        AttemptOutcome::CallerError(error) => should_failover_error(error),
        AttemptOutcome::Success(_) | AttemptOutcome::NotDispatched => false,
    }
}

struct ReductionOutput {
    values: Vec<AttemptOutcome<RpcResult>>,
    attempts: Vec<PhysicalAttempt>,
}
struct PhysicalAttempt {
    endpoint: Url,
    health_outcome: EndpointHealthOutcome,
    indices: Vec<usize>,
}

struct AggregateMember {
    read: RpcRead,
    waiters: Arc<WaiterState>,
}

struct ScopedMember {
    index: usize,
    snapshot: WaiterSnapshot,
}

struct ScopedView {
    members: Vec<ScopedMember>,
}

impl ScopedView {
    fn new(members: &[AggregateMember], indices: &[usize], now: Instant) -> Self {
        Self {
            members: indices
                .iter()
                .filter_map(|&index| {
                    let member = &members[index];
                    let snapshot = member.waiters.snapshot(now);
                    snapshot
                        .has_live
                        .then_some(ScopedMember { index, snapshot })
                })
                .collect(),
        }
    }

    fn indices(&self) -> Vec<usize> {
        self.members.iter().map(|member| member.index).collect()
    }

    fn earliest_permit_deadline(&self) -> Option<Instant> {
        self.members
            .iter()
            .filter_map(|member| member.snapshot.deadline)
            .min()
    }

    fn latest_physical_deadline(&self) -> Option<Instant> {
        let mut latest = None;
        for member in &self.members {
            let deadline = member.snapshot.deadline?;
            latest = latest.max(Some(deadline));
        }
        latest
    }

    fn min_attempt_timeout(&self, fallback: Duration) -> Duration {
        self.members
            .iter()
            .map(|member| member.snapshot.attempt_timeout)
            .filter(|timeout| !timeout.is_zero())
            .min()
            .unwrap_or(fallback)
    }
}

struct ReductionContext {
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    route: RpcRoute,
    endpoint: Url,
    members: Arc<[AggregateMember]>,
    reduction_budget: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct ReductionAttempt {
    budget: Arc<AtomicUsize>,
    fallback: RpcBrokerError,
}

impl ReductionAttempt {
    const fn new(budget: Arc<AtomicUsize>, fallback: RpcBrokerError) -> Self {
        Self { budget, fallback }
    }

    fn consume(&self) -> Result<(), RpcBrokerError> {
        self.budget
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |current| (current < MAX_REDUCTION_ATTEMPTS_PER_ENDPOINT).then_some(current + 1),
            )
            .map(|_| ())
            .map_err(|_| self.fallback.clone())
    }
}

fn reduce(
    context: Arc<ReductionContext>,
    indices: Vec<usize>,
) -> BoxFuture<'static, ReductionOutput> {
    reduce_with_parent(context, indices, None)
}

fn reduce_with_parent(
    context: Arc<ReductionContext>,
    indices: Vec<usize>,
    parent_error: Option<RpcBrokerError>,
) -> BoxFuture<'static, ReductionOutput> {
    Box::pin(async move {
        let mut values = vec![AttemptOutcome::NotDispatched; context.members.len()];
        let candidates = ScopedView::new(&context.members, &indices, Instant::now()).indices();
        if candidates.is_empty() {
            return ReductionOutput {
                values,
                attempts: Vec::new(),
            };
        }
        let aggregate = execute_aggregate(
            &context,
            &candidates,
            Some(ReductionAttempt::new(
                context.reduction_budget.clone(),
                parent_error.unwrap_or(RpcBrokerError::InvalidResponse),
            )),
        )
        .await;
        let actual = aggregate.indices.clone();
        let Some(&first_index) = actual.first() else {
            return ReductionOutput {
                values,
                attempts: Vec::new(),
            };
        };
        let mut attempt = PhysicalAttempt {
            endpoint: context.endpoint.clone(),
            health_outcome: EndpointHealthOutcome::Neutral,
            indices: actual.clone(),
        };
        let aggregate_error = match aggregate.result {
            AttemptOutcome::NotDispatched => {
                return ReductionOutput {
                    values,
                    attempts: Vec::new(),
                };
            }
            AttemptOutcome::ReductionLimit(error) => {
                for index in &actual {
                    values[*index] = AttemptOutcome::ReductionLimit(error.clone());
                }
                return ReductionOutput {
                    values,
                    attempts: Vec::new(),
                };
            }
            AttemptOutcome::Success(local_values) => {
                attempt.health_outcome = EndpointHealthOutcome::for_results(&local_values);
                for (index, value) in actual.iter().copied().zip(local_values) {
                    values[index] = match value {
                        Ok(value) => AttemptOutcome::Success(value),
                        Err(error) => AttemptOutcome::CallerError(error),
                    };
                }
                return ReductionOutput {
                    values,
                    attempts: vec![attempt],
                };
            }
            AttemptOutcome::Recoverable(error) => {
                attempt.health_outcome =
                    EndpointHealthOutcome::for_result::<RpcResult>(&Err(error.clone()));
                error
            }
            AttemptOutcome::CallerError(error) => {
                attempt.health_outcome =
                    EndpointHealthOutcome::for_result::<RpcResult>(&Err(error.clone()));
                for index in &actual {
                    values[*index] = AttemptOutcome::CallerError(error.clone());
                }
                return ReductionOutput {
                    values,
                    attempts: vec![attempt],
                };
            }
        };
        if actual.len() > 1 {
            let error = aggregate_error;
            let midpoint = actual.len() / 2;
            let (left, right) = actual.split_at(midpoint);
            let left_indices = left.to_vec();
            let right_indices = right.to_vec();
            let (left, right) = tokio::join!(
                reduce_with_parent(
                    Arc::clone(&context),
                    left_indices.clone(),
                    Some(error.clone()),
                ),
                reduce_with_parent(
                    Arc::clone(&context),
                    right_indices.clone(),
                    Some(error.clone()),
                )
            );
            let mut left_values =
                replace_undispatched_with_parent_error(left.values, &left_indices, &error);
            let mut right_values =
                replace_undispatched_with_parent_error(right.values, &right_indices, &error);
            for index in left_indices {
                values[index] =
                    std::mem::replace(&mut left_values[index], AttemptOutcome::NotDispatched);
            }
            for index in right_indices {
                values[index] =
                    std::mem::replace(&mut right_values[index], AttemptOutcome::NotDispatched);
            }
            ReductionOutput {
                values,
                attempts: {
                    let mut all = vec![attempt];
                    all.extend(left.attempts);
                    all.extend(right.attempts);
                    all
                },
            }
        } else {
            let error = aggregate_error;
            let index = first_index;
            let result = execute_single_shared_waiter_attempt(
                &context.client,
                &context.semaphore,
                &context.members[index].read,
                &context.endpoint,
                &context.members[index].waiters,
                context.route.attempt_timeout,
                Some(ReductionAttempt::new(
                    context.reduction_budget.clone(),
                    error.clone(),
                )),
            )
            .await;
            match result {
                AttemptOutcome::NotDispatched => {
                    values[index] = AttemptOutcome::CallerError(error);
                    ReductionOutput {
                        values,
                        attempts: vec![attempt],
                    }
                }
                AttemptOutcome::ReductionLimit(error) => {
                    values[index] = AttemptOutcome::ReductionLimit(error);
                    ReductionOutput {
                        values,
                        attempts: vec![attempt],
                    }
                }
                result => {
                    let public_result = result.clone().into_result();
                    let individual_attempt = PhysicalAttempt {
                        endpoint: context.endpoint.clone(),
                        health_outcome: EndpointHealthOutcome::for_result(&public_result),
                        indices: vec![index],
                    };
                    values[index] = result;
                    ReductionOutput {
                        values,
                        attempts: vec![attempt, individual_attempt],
                    }
                }
            }
        }
    })
}

fn replace_undispatched_with_parent_error(
    values: Vec<AttemptOutcome<RpcResult>>,
    indices: &[usize],
    parent_error: &RpcBrokerError,
) -> Vec<AttemptOutcome<RpcResult>> {
    let mut values = values;
    for index in indices.iter().copied() {
        if matches!(&values[index], AttemptOutcome::NotDispatched) {
            values[index] = AttemptOutcome::CallerError(parent_error.clone());
        }
    }
    values
}

struct AggregateExecution {
    indices: Vec<usize>,
    result: AttemptOutcome<Vec<Result<RpcResult, RpcBrokerError>>>,
}

async fn execute_aggregate(
    context: &ReductionContext,
    indices: &[usize],
    reduction_attempt: Option<ReductionAttempt>,
) -> AggregateExecution {
    let Some(multicall) = context.route.multicall() else {
        return AggregateExecution {
            indices: Vec::new(),
            result: AttemptOutcome::Recoverable(RpcBrokerError::InvalidResponse),
        };
    };
    let mut current = ScopedView::new(&context.members, indices, Instant::now());
    let mut candidates = current.indices();
    if candidates.is_empty() {
        return AggregateExecution {
            indices: Vec::new(),
            result: AttemptOutcome::NotDispatched,
        };
    }
    let permit = loop {
        match acquire_permit(&context.semaphore, current.earliest_permit_deadline()).await {
            PermitAcquisition::Acquired(permit) => break permit,
            PermitAcquisition::DeadlineElapsed => {
                current = ScopedView::new(&context.members, &candidates, Instant::now());
                candidates = current.indices();
                if candidates.is_empty() {
                    return AggregateExecution {
                        indices: Vec::new(),
                        result: AttemptOutcome::NotDispatched,
                    };
                }
            }
            PermitAcquisition::Closed => {
                return AggregateExecution {
                    indices: Vec::new(),
                    result: AttemptOutcome::CallerError(RpcBrokerError::Shutdown),
                };
            }
        }
    };
    current = ScopedView::new(&context.members, &candidates, Instant::now());
    let actual = current.indices();
    let Some(&first_index) = actual.first() else {
        return AggregateExecution {
            indices: Vec::new(),
            result: AttemptOutcome::NotDispatched,
        };
    };
    if let Some(attempt) = reduction_attempt
        && let Err(error) = attempt.consume()
    {
        return AggregateExecution {
            indices: actual,
            result: AttemptOutcome::ReductionLimit(error),
        };
    }
    let calls = actual
        .iter()
        .map(|index| {
            let read = &context.members[*index].read;
            let (target, call_data) = match read.operation() {
                RpcOperation::GetBalance { account, .. } => (
                    multicall,
                    Bytes::from(IMulticall3::getEthBalanceCall { addr: *account }.abi_encode()),
                ),
                RpcOperation::EthCall { request, .. } => {
                    let target = request
                        .to
                        .and_then(TxKind::into_to)
                        .expect("multicall eligibility requires an eth_call target");
                    let calldata = request.input.input().cloned().unwrap_or_default();
                    (target, calldata)
                }
                _ => unreachable!("only eligible calls and balances are aggregated"),
            };
            IMulticall3::Call {
                target,
                callData: call_data,
            }
        })
        .collect();
    let input = IMulticall3::tryAggregateCall {
        requireSuccess: false,
        calls,
    }
    .abi_encode();
    let attempt_timeout = current.min_attempt_timeout(context.route.attempt_timeout);
    let request_deadline = Instant::now() + attempt_timeout;
    let request_deadline = current
        .latest_physical_deadline()
        .map_or(request_deadline, |deadline| deadline.min(request_deadline));
    let transaction = TransactionRequest::default()
        .to(multicall)
        .input(TransactionInput::new(input.into()));
    let params = EthCallParams::<Ethereum>::new(transaction).with_block(
        context.members[first_index]
            .read
            .reuse_block_id()
            .expect("aggregate member has a block"),
    );
    let params = serde_json::to_value(params).expect("Alloy eth_call params serialize");
    let body = wire_request("eth_call", params);
    let value = execute_wire_request_with_permit(
        &context.client,
        &context.endpoint,
        permit,
        body,
        request_deadline,
    )
    .await;
    let value = value.and_then(|value| parse_rpc_result(&value, false));
    let result = match value {
        Ok(value) => match decode_hex_value(&value).and_then(|output| {
            let decoded = IMulticall3::tryAggregateCall::abi_decode_returns_validate(&output)
                .map_err(|_| RpcBrokerError::InvalidResponse)?;
            if decoded.len() != actual.len() {
                return Err(RpcBrokerError::InvalidResponse);
            }
            decoded
                .into_iter()
                .zip(&actual)
                .map(|(result, index)| {
                    if result.success {
                        context.members[*index]
                            .read
                            .result_from_abi(&result.returnData)
                            .map(Ok)
                    } else {
                        Ok(Err(RpcBrokerError::InnerRevert(RpcRevert::from_multicall(
                            result.returnData,
                        ))))
                    }
                })
                .collect::<Result<Vec<_>, RpcBrokerError>>()
        }) {
            Ok(values) => AttemptOutcome::Success(values),
            Err(error) => AttemptOutcome::from(Err(error)),
        },
        Err(error) => AttemptOutcome::from(Err(error)),
    };
    let result = if Instant::now() >= request_deadline
        && !matches!(
            result,
            AttemptOutcome::CallerError(RpcBrokerError::ResponseTooLarge)
        ) {
        AttemptOutcome::CallerError(RpcBrokerError::Timeout)
    } else {
        result
    };
    AggregateExecution {
        indices: actual,
        result,
    }
}
async fn execute_single_with_shared_waiters_attempt(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    read: &RpcRead,
    chain_id: u64,
    endpoints: &[SensitiveUrl],
    waiters: Arc<WaiterState>,
    fallback_attempt_timeout: Duration,
) -> (AttemptOutcome<RpcResult>, Vec<(Url, EndpointHealthOutcome)>) {
    let mut attempted = Vec::new();
    let mut result = AttemptOutcome::CallerError(RpcBrokerError::NoEndpoint { chain_id });
    for endpoint in endpoints.iter().map(SensitiveUrl::expose_url) {
        let next_outcome = execute_single_shared_waiter_attempt(
            client,
            semaphore,
            read,
            endpoint,
            &waiters,
            fallback_attempt_timeout,
            None,
        )
        .await;
        if matches!(&next_outcome, AttemptOutcome::NotDispatched) && !attempted.is_empty() {
            break;
        }
        result = next_outcome;
        if !matches!(
            &result,
            AttemptOutcome::NotDispatched
                | AttemptOutcome::CallerError(
                    RpcBrokerError::Shutdown | RpcBrokerError::NoEndpoint { .. }
                )
        ) {
            attempted.push((
                endpoint.clone(),
                EndpointHealthOutcome::for_result(&result.clone().into_result()),
            ));
        }
        if !matches!(
            &result,
            AttemptOutcome::CallerError(error) | AttemptOutcome::Recoverable(error)
                if should_failover_error(error)
        ) {
            break;
        }
    }
    (result, attempted)
}

async fn execute_single_shared_waiter_attempt(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    read: &RpcRead,
    endpoint: &Url,
    waiters: &WaiterState,
    fallback_attempt_timeout: Duration,
    reduction_attempt: Option<ReductionAttempt>,
) -> AttemptOutcome<RpcResult> {
    let permit = loop {
        let snapshot = waiters.snapshot(Instant::now());
        if !snapshot.has_live {
            return AttemptOutcome::NotDispatched;
        }
        match acquire_permit(semaphore, snapshot.deadline).await {
            PermitAcquisition::Acquired(permit) => break permit,
            PermitAcquisition::DeadlineElapsed => {}
            PermitAcquisition::Closed => {
                return AttemptOutcome::CallerError(RpcBrokerError::Shutdown);
            }
        }
    };

    // The permit may have been held across a waiter deadline. This is the final snapshot for
    // this physical attempt; later attachments belong only to a future endpoint attempt.
    let snapshot = waiters.snapshot(Instant::now());
    if !snapshot.has_live
        || snapshot
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
    {
        return AttemptOutcome::NotDispatched;
    }
    let (method, params, preserve_revert) = wire_request_for(read);
    let body = wire_request(method, params);
    let attempt_timeout = if snapshot.attempt_timeout.is_zero() {
        fallback_attempt_timeout
    } else {
        snapshot.attempt_timeout
    };
    let attempt_deadline = Instant::now() + attempt_timeout;
    let request_deadline = snapshot
        .deadline
        .map_or(attempt_deadline, |deadline| deadline.min(attempt_deadline));
    let value = execute_wire_request_with_acquired_permit(
        client,
        endpoint,
        permit,
        body,
        Some(request_deadline),
        reduction_attempt,
        attempt_timeout,
    )
    .await;
    let result =
        value.and_then(|value| AttemptOutcome::from(parse_rpc_result(&value, preserve_revert)));
    let result =
        result.and_then(|value| AttemptOutcome::from(read.operation().validate_result(value)));
    if Instant::now() >= request_deadline
        && !matches!(
            result,
            AttemptOutcome::CallerError(RpcBrokerError::ResponseTooLarge)
        )
    {
        AttemptOutcome::CallerError(RpcBrokerError::Timeout)
    } else {
        result
    }
}

pub(super) const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

// RPC encoding and decoding
fn wire_request(method: &str, params: Value) -> Request<Value> {
    Request::new(method.to_owned(), Id::Number(1), params)
}

async fn execute_wire_request_with_acquired_permit(
    client: &reqwest::Client,
    endpoint: &Url,
    permit: OwnedSemaphorePermit,
    body: Request<Value>,
    deadline: Option<Instant>,
    reduction_attempt: Option<ReductionAttempt>,
    attempt_timeout: Duration,
) -> AttemptOutcome<Value> {
    if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
        return AttemptOutcome::NotDispatched;
    }
    if let Some(attempt) = reduction_attempt
        && let Err(error) = attempt.consume()
    {
        return AttemptOutcome::ReductionLimit(error);
    }
    let attempt_deadline = Instant::now() + attempt_timeout;
    let request_deadline = deadline.map_or(attempt_deadline, |total| total.min(attempt_deadline));
    match execute_wire_request_with_permit(client, endpoint, permit, body, request_deadline).await {
        Ok(value) => AttemptOutcome::Success(value),
        Err(error) => AttemptOutcome::from(Err(error)),
    }
}

async fn acquire_permit(
    semaphore: &Arc<Semaphore>,
    deadline: Option<Instant>,
) -> PermitAcquisition {
    match deadline {
        Some(deadline) => time::timeout_at(deadline, semaphore.clone().acquire_owned())
            .await
            .map_or(PermitAcquisition::DeadlineElapsed, |result| {
                result.map_or(PermitAcquisition::Closed, PermitAcquisition::Acquired)
            }),
        None => semaphore
            .clone()
            .acquire_owned()
            .await
            .map_or(PermitAcquisition::Closed, PermitAcquisition::Acquired),
    }
}

async fn execute_wire_request_with_permit(
    client: &reqwest::Client,
    endpoint: &Url,
    _permit: OwnedSemaphorePermit,
    body: Request<Value>,
    request_deadline: Instant,
) -> Result<Value, RpcBrokerError> {
    let request = async {
        let mut response = client
            .post(endpoint.clone())
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    RpcBrokerError::Timeout
                } else {
                    RpcBrokerError::Transport
                }
            })?;
        if !response.status().is_success() {
            return Err(RpcBrokerError::HttpStatus(response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            if error.is_timeout() {
                RpcBrokerError::Timeout
            } else {
                RpcBrokerError::InvalidResponse
            }
        })? {
            let size = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(RpcBrokerError::ResponseTooLarge)?;
            if size > MAX_RESPONSE_BYTES {
                return Err(RpcBrokerError::ResponseTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = serde_json::from_slice(&bytes).map_err(|_| RpcBrokerError::InvalidResponse);
        if Instant::now() >= request_deadline {
            Err(RpcBrokerError::Timeout)
        } else {
            value
        }
    };
    time::timeout_at(request_deadline, request)
        .await
        .map_err(|_| RpcBrokerError::Timeout)?
}

pub(super) fn wire_request_for(read: &RpcRead) -> (&'static str, Value, bool) {
    read.operation().wire()
}
/// The JSON-RPC response envelope, parsed once so the structured error payload (including its
/// unknown fields) survives without a second deserialization of the same value.
#[derive(Deserialize)]
struct WireResponse {
    id: Id,
    #[serde(default)]
    error: Option<WithOtherFields<ErrorPayload<Value>>>,
}

pub(super) fn parse_rpc_result(
    value: &Value,
    preserve_revert: bool,
) -> Result<Value, RpcBrokerError> {
    let response = WireResponse::deserialize(value).map_err(|_| RpcBrokerError::InvalidResponse)?;
    if response.id != Id::Number(1) {
        return Err(RpcBrokerError::InvalidResponse);
    }
    if let Some(payload) = response.error {
        let remote = RpcRemoteError::from(payload);
        let data = remote
            .expose_data()
            .and_then(Value::as_str)
            .and_then(|raw| raw.parse::<Bytes>().ok());
        let has_revert_marker = remote
            .expose_message()
            .to_ascii_lowercase()
            .contains("revert");
        if preserve_revert
            && let Some(data) = data
            && (remote.code() == 3 || (remote.code() == -32000 && has_revert_marker))
        {
            return Err(RpcBrokerError::InnerRevert(RpcRevert::from_individual(
                data, remote,
            )));
        }
        return Err(RpcBrokerError::Remote(remote));
    }
    value
        .get("result")
        .cloned()
        .ok_or(RpcBrokerError::InvalidResponse)
}
pub(super) fn decode_hex_value(value: &Value) -> Result<Bytes, RpcBrokerError> {
    value
        .as_str()
        .ok_or(RpcBrokerError::InvalidResponse)?
        .parse()
        .map_err(|_| RpcBrokerError::InvalidResponse)
}

#[cfg(test)]
#[path = "tests/execution.rs"]
mod tests;
