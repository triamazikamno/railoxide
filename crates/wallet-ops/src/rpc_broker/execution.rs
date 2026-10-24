use super::actor::{EndpointHealthOutcome, JobOutput, RequestEvent};
use super::capacity::{JobControl, JobResources, SlotKind};
use super::identity::ProbeRunner;
use super::model::{
    FailureClass, MAX_REDUCTION_ATTEMPTS_PER_ENDPOINT, RpcBrokerError, RpcChainRoute, RpcRead,
    RpcRemoteError, RpcResult, RpcRevert, RpcRoute,
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
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
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
    resources: JobResources,
    job: ExecutionJob,
    endpoints: Vec<SensitiveUrl>,
    control: JobControl,
) -> BoxFuture<'static, JobOutput> {
    Box::pin(async move {
        match job {
            ExecutionJob::Aggregate(group) => {
                let Some(route) = group.first().map(|item| item.execution_route.clone()) else {
                    return JobOutput::noop();
                };
                let mut failover = AggregateFailover::new(route.clone(), &group);
                let identity_events =
                    drive_job(&resources, &route, &endpoints, &control, &mut failover).await;
                let values = failover.into_output();
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
                        .chain(identity_events)
                        .collect(),
                }
            }
            ExecutionJob::Individual(item) => {
                let route = item.execution_route.clone();
                let mut failover = SingleFailover::new(
                    &item.read,
                    item.waiters.clone(),
                    route.chain_route(),
                    route.chain_id(),
                    route.attempt_timeout,
                );
                let identity_events =
                    drive_job(&resources, &route, &endpoints, &control, &mut failover).await;
                let (value, attempted_endpoints) = failover.into_output();
                JobOutput {
                    completions: vec![(item.key, value.into_result())],
                    requests: attempted_endpoints
                        .into_iter()
                        .map(|(endpoint, outcome)| {
                            RequestEvent::new(route.chain_id(), endpoint, outcome)
                        })
                        .chain(identity_events)
                        .collect(),
                }
            }
        }
    })
}

/// Identity state of one selected endpoint within a job.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Candidate {
    /// Verified, or on a route without identity checks, and not yet tried.
    Ready,
    Unprobed,
    Probing,
    /// Failed its identity check, so it never receives requested reads.
    Rejected,
    Tried,
}

enum ProbeOutcome {
    Verified,
    /// The check was sent and failed.
    Failed(RpcBrokerError),
    /// No live waiter remained before the check was sent.
    NotDispatched,
    /// A permit semaphore closed.
    Closed,
}

/// Some members still have live waiters; the next of them may expire at `earliest_deadline`.
struct StillLive {
    earliest_deadline: Option<Instant>,
}

/// Endpoint failover for the members of one job, advanced one endpoint attempt at a time.
trait Failover {
    type Outcome;

    /// Settles members without live waiters, keeping a dispatched member's last result. Returns
    /// `None` once no member is live.
    fn live(&mut self, now: Instant) -> Option<StillLive>;

    /// Waiters of the members still being resolved.
    fn waiters(&self) -> Vec<Arc<WaiterState>>;

    /// Starts one physical attempt on `endpoint`. The attempt borrows only immutable job data.
    fn attempt<'a>(
        &'a self,
        resources: &'a JobResources,
        endpoint: &'a Url,
    ) -> BoxFuture<'a, Self::Outcome>;

    /// Records an attempt and returns whether another endpoint should be tried.
    fn apply(&mut self, endpoint: &Url, outcome: Self::Outcome) -> bool;

    /// Settles never-dispatched members once the broker can no longer grant capacity.
    fn shut_down(&mut self);
}

struct SingleFailover<'a> {
    read: &'a RpcRead,
    waiters: Arc<WaiterState>,
    route: &'a RpcChainRoute,
    fallback_attempt_timeout: Duration,
    result: AttemptOutcome<RpcResult>,
    attempted: Vec<(Url, EndpointHealthOutcome)>,
}

impl<'a> SingleFailover<'a> {
    const fn new(
        read: &'a RpcRead,
        waiters: Arc<WaiterState>,
        route: &'a RpcChainRoute,
        chain_id: u64,
        fallback_attempt_timeout: Duration,
    ) -> Self {
        Self {
            read,
            waiters,
            route,
            fallback_attempt_timeout,
            result: AttemptOutcome::CallerError(RpcBrokerError::NoEndpoint { chain_id }),
            attempted: Vec::new(),
        }
    }

    fn into_output(self) -> (AttemptOutcome<RpcResult>, Vec<(Url, EndpointHealthOutcome)>) {
        (self.result, self.attempted)
    }
}

impl Failover for SingleFailover<'_> {
    type Outcome = AttemptOutcome<RpcResult>;

    fn live(&mut self, now: Instant) -> Option<StillLive> {
        let snapshot = self.waiters.snapshot(now);
        if snapshot.has_live {
            return Some(StillLive {
                earliest_deadline: snapshot.deadline,
            });
        }
        if self.attempted.is_empty() {
            self.result = AttemptOutcome::NotDispatched;
        }
        None
    }

    fn waiters(&self) -> Vec<Arc<WaiterState>> {
        vec![Arc::clone(&self.waiters)]
    }

    fn attempt<'b>(
        &'b self,
        resources: &'b JobResources,
        endpoint: &'b Url,
    ) -> BoxFuture<'b, Self::Outcome> {
        Box::pin(execute_single_shared_waiter_attempt(
            self.route.read_client(&resources.clients, endpoint),
            &resources.http,
            self.read,
            endpoint,
            &self.waiters,
            self.fallback_attempt_timeout,
            None,
        ))
    }

    fn apply(&mut self, endpoint: &Url, outcome: Self::Outcome) -> bool {
        if matches!(&outcome, AttemptOutcome::NotDispatched) && !self.attempted.is_empty() {
            return false;
        }
        self.result = outcome;
        if !matches!(
            &self.result,
            AttemptOutcome::NotDispatched
                | AttemptOutcome::CallerError(
                    RpcBrokerError::Shutdown | RpcBrokerError::NoEndpoint { .. }
                )
        ) {
            self.attempted.push((
                endpoint.clone(),
                EndpointHealthOutcome::for_result(&self.result.clone().into_result()),
            ));
        }
        matches!(
            &self.result,
            AttemptOutcome::CallerError(error) | AttemptOutcome::Recoverable(error)
                if should_failover_error(error)
        )
    }

    fn shut_down(&mut self) {
        if self.attempted.is_empty() {
            self.result = AttemptOutcome::CallerError(RpcBrokerError::Shutdown);
        }
    }
}

struct AggregateFailover {
    route: RpcRoute,
    members: Arc<[AggregateMember]>,
    unresolved: Vec<usize>,
    final_values: Vec<AttemptOutcome<RpcResult>>,
    attempts: Vec<PhysicalAttempt>,
    dispatched: Vec<bool>,
}

impl AggregateFailover {
    fn new(route: RpcRoute, items: &[WorkItem]) -> Self {
        let members: Arc<[AggregateMember]> = items
            .iter()
            .map(|item| AggregateMember {
                read: item.read.clone(),
                waiters: item.waiters.clone(),
            })
            .collect::<Vec<_>>()
            .into();
        let final_values = vec![
            AttemptOutcome::CallerError(RpcBrokerError::NoEndpoint {
                chain_id: route.chain_id(),
            });
            members.len()
        ];
        Self {
            route,
            unresolved: (0..members.len()).collect(),
            final_values,
            attempts: Vec::new(),
            dispatched: vec![false; members.len()],
            members,
        }
    }

    fn into_output(self) -> ReductionOutput {
        ReductionOutput {
            values: self.final_values,
            attempts: self.attempts,
        }
    }
}

impl Failover for AggregateFailover {
    type Outcome = ReductionOutput;

    fn live(&mut self, now: Instant) -> Option<StillLive> {
        let current = ScopedView::new(&self.members, &self.unresolved, now);
        let live = current.indices();
        for index in self
            .unresolved
            .iter()
            .copied()
            .filter(|index| !live.contains(index))
        {
            if !self.dispatched[index] {
                self.final_values[index] = AttemptOutcome::NotDispatched;
            }
        }
        self.unresolved = live;
        (!self.unresolved.is_empty()).then(|| StillLive {
            earliest_deadline: current.earliest_permit_deadline(),
        })
    }

    fn waiters(&self) -> Vec<Arc<WaiterState>> {
        self.unresolved
            .iter()
            .map(|&index| Arc::clone(&self.members[index].waiters))
            .collect()
    }

    fn attempt<'b>(
        &'b self,
        resources: &'b JobResources,
        endpoint: &'b Url,
    ) -> BoxFuture<'b, Self::Outcome> {
        let context = Arc::new(ReductionContext {
            client: self
                .route
                .chain_route()
                .read_client(&resources.clients, endpoint)
                .clone(),
            semaphore: resources.http.clone(),
            route: self.route.clone(),
            endpoint: endpoint.clone(),
            members: Arc::clone(&self.members),
            reduction_budget: Arc::new(AtomicUsize::new(0)),
        });
        reduce(context, self.unresolved.clone())
    }

    fn apply(&mut self, _endpoint: &Url, output: Self::Outcome) -> bool {
        let previously_dispatched = self.dispatched.clone();
        for attempt in &output.attempts {
            for index in &attempt.indices {
                self.dispatched[*index] = true;
            }
        }
        self.attempts.extend(output.attempts);
        let mut next = Vec::new();
        for index in std::mem::take(&mut self.unresolved) {
            let value = &output.values[index];
            if !(matches!(value, AttemptOutcome::NotDispatched) && previously_dispatched[index]) {
                self.final_values[index] = value.clone();
            }
            if should_failover_aggregate_outcome(value) {
                next.push(index);
            }
        }
        self.unresolved = next;
        !self.unresolved.is_empty()
    }

    fn shut_down(&mut self) {
        for &index in &self.unresolved {
            if !self.dispatched[index] {
                self.final_values[index] = AttemptOutcome::CallerError(RpcBrokerError::Shutdown);
            }
        }
    }
}

/// Runs a job's endpoint failover while verifying endpoint identities progressively, and
/// returns the identity request events.
///
/// Endpoints are tried in the actor's selection order once they are ready. Probes for the others
/// run alongside reads and supply later fallback candidates. The job holds a read slot only while
/// it can try an endpoint, and a probe runner only while probes remain. Dropping the job drops
/// its queued and active probes.
async fn drive_job<W: Failover>(
    resources: &JobResources,
    route: &RpcRoute,
    endpoints: &[SensitiveUrl],
    control: &JobControl,
    failover: &mut W,
) -> Vec<RequestEvent> {
    let chain = route.chain_route();
    let mut candidates = endpoints
        .iter()
        .map(|endpoint| {
            if chain.endpoint_identity_verified(endpoint) {
                Candidate::Ready
            } else {
                Candidate::Unprobed
            }
        })
        .collect::<Vec<_>>();
    let mut events = Vec::new();
    let target = ProbeTarget {
        control,
        route,
        endpoints,
    };
    let start_probe = move |index: usize, waiters: Vec<Arc<WaiterState>>| {
        probe_endpoint(resources, route, &endpoints[index], waiters)
    };
    let mut probes = ProbeRunner::new(resources.probe_limit);
    // An outstanding request is kept across iterations so the job keeps its place in line.
    let mut read_grant: Option<oneshot::Receiver<()>> = None;
    let mut runner_grant: Option<oneshot::Receiver<()>> = None;
    loop {
        // Created before the re-check so a park below wakes for any later verification on the
        // route, whether by another job or a sync admission check on a clone.
        let identity_checked = chain.identity_checked();
        tokio::pin!(identity_checked);
        // Another job may have verified an endpoint of this route since the last decision.
        for (candidate, endpoint) in candidates.iter_mut().zip(endpoints) {
            if matches!(candidate, Candidate::Unprobed | Candidate::Probing)
                && chain.endpoint_identity_verified(endpoint)
            {
                *candidate = Candidate::Ready;
            }
        }
        let Some(StillLive {
            earliest_deadline: deadline,
        }) = failover.live(Instant::now())
        else {
            break;
        };
        // A runner granted while the job was reading is picked up here.
        if let Some(grant) = runner_grant.as_mut()
            && grant.try_recv().is_ok()
        {
            runner_grant = None;
        }
        let ready = candidates
            .iter()
            .position(|candidate| *candidate == Candidate::Ready);
        if may_refill(control, runner_grant.is_some()) {
            refill_probes(&probes, &mut candidates, &failover.waiters(), &start_probe);
        }
        if probes.is_empty() && !candidates.contains(&Candidate::Unprobed) {
            runner_grant = None;
            control.release(SlotKind::Runner);
        }
        if let Some(index) = ready {
            if read_grant.is_some() || !control.holds(SlotKind::Read) {
                if read_grant.is_none() {
                    read_grant = control.request(SlotKind::Read);
                }
                let Some(grant) = read_grant.as_mut() else {
                    failover.shut_down();
                    break;
                };
                tokio::select! {
                    granted = grant => {
                        read_grant = None;
                        if granted.is_err() {
                            failover.shut_down();
                            break;
                        }
                    }
                    Some((probe, outcome)) = probes.next(), if !probes.is_empty() => {
                        target.apply(&mut candidates, &mut events, probe, outcome);
                    }
                    () = sleep_until_deadline(deadline) => {}
                }
                continue;
            }
            candidates[index] = Candidate::Tried;
            let endpoint = endpoints[index].expose_url();
            let outcome = {
                // Probes keep running during the attempt, which is never restarted.
                let mut attempt = failover.attempt(resources, endpoint);
                loop {
                    tokio::select! {
                        outcome = &mut attempt => break outcome,
                        Some((probe, outcome)) = probes.next(), if !probes.is_empty() => {
                            target.apply(&mut candidates, &mut events, probe, outcome);
                            if may_refill(control, runner_grant.is_some()) {
                                let waiters = failover.waiters();
                                refill_probes(&probes, &mut candidates, &waiters, &start_probe);
                            }
                        }
                    }
                }
            };
            if failover.apply(endpoint, outcome) {
                continue;
            }
            break;
        }
        if probes.is_empty() && !candidates.contains(&Candidate::Unprobed) {
            // Exhausted: no endpoint or probe can supply another attempt.
            break;
        }
        // Park until a probe or another check verifies an endpoint, leaving the read slot to
        // other work.
        read_grant = None;
        control.release(SlotKind::Read);
        if probes.is_empty() {
            // Unprobed endpoints remain, but this job has no probe runner yet.
            if runner_grant.is_none() {
                runner_grant = control.request(SlotKind::Runner);
            }
            let Some(grant) = runner_grant.as_mut() else {
                failover.shut_down();
                break;
            };
            tokio::select! {
                granted = grant => {
                    runner_grant = None;
                    if granted.is_err() {
                        failover.shut_down();
                        break;
                    }
                }
                () = &mut identity_checked => {}
                () = sleep_until_deadline(deadline) => {}
            }
        } else {
            tokio::select! {
                Some((probe, outcome)) = probes.next() => {
                    target.apply(&mut candidates, &mut events, probe, outcome);
                }
                () = &mut identity_checked => {}
                () = sleep_until_deadline(deadline) => {}
            }
        }
    }
    events
}

/// Whether the job holds its probe runner and has received its grant.
fn may_refill(control: &JobControl, awaiting_runner: bool) -> bool {
    !awaiting_runner && control.holds(SlotKind::Runner)
}

/// Starts probes for unprobed endpoints in selection order while the runner has room.
fn refill_probes<F: Future>(
    probes: &ProbeRunner<usize, F>,
    candidates: &mut [Candidate],
    waiters: &[Arc<WaiterState>],
    start: &impl Fn(usize, Vec<Arc<WaiterState>>) -> F,
) {
    while probes.has_capacity() {
        let Some(index) = candidates
            .iter()
            .position(|candidate| *candidate == Candidate::Unprobed)
        else {
            break;
        };
        candidates[index] = Candidate::Probing;
        probes.push(index, start(index, waiters.to_vec()));
    }
}

#[derive(Clone, Copy)]
struct ProbeTarget<'a> {
    control: &'a JobControl,
    route: &'a RpcRoute,
    endpoints: &'a [SensitiveUrl],
}

impl ProbeTarget<'_> {
    /// Records a finished probe. A verified endpoint becomes ready and wakes the actor for other
    /// jobs on the route; a sent check that failed keeps its health classification.
    fn apply(
        self,
        candidates: &mut [Candidate],
        events: &mut Vec<RequestEvent>,
        index: usize,
        outcome: ProbeOutcome,
    ) {
        let probing = candidates[index] == Candidate::Probing;
        match outcome {
            ProbeOutcome::Verified => {
                if probing {
                    candidates[index] = Candidate::Ready;
                }
                self.control.notify_verified();
            }
            ProbeOutcome::Failed(error) => {
                if probing {
                    candidates[index] = Candidate::Rejected;
                }
                events.push(RequestEvent::new(
                    self.route.chain_id(),
                    self.endpoints[index].expose_url().clone(),
                    EndpointHealthOutcome::for_result::<()>(&Err(error)),
                ));
            }
            // A later live waiter may still need this endpoint.
            ProbeOutcome::NotDispatched if probing => candidates[index] = Candidate::Unprobed,
            ProbeOutcome::Closed if probing => candidates[index] = Candidate::Rejected,
            ProbeOutcome::NotDispatched | ProbeOutcome::Closed => {}
        }
    }
}

/// One identity check for a job. It waits for the independent broker-wide probe budget,
/// bounded by live waiter deadlines, and is never retried. Only the captured
/// route's verification cache is consulted and updated.
async fn probe_endpoint(
    resources: &JobResources,
    route: &RpcRoute,
    endpoint: &SensitiveUrl,
    waiters: Vec<Arc<WaiterState>>,
) -> ProbeOutcome {
    let chain = route.chain_route();
    if chain.endpoint_identity_verified(endpoint) {
        return ProbeOutcome::Verified;
    }
    let _budget = match acquire_live_permit(&resources.probes, &waiters).await {
        Ok(permit) => permit,
        Err(outcome) => return outcome,
    };
    let now = Instant::now();
    let current = ScopedView::from_waiters(&waiters, now);
    if current.members.is_empty() {
        return ProbeOutcome::NotDispatched;
    }
    let timeout = current.min_attempt_timeout(route.attempt_timeout);
    let timeout = current
        .latest_physical_deadline()
        .map_or(timeout, |deadline| {
            timeout.min(deadline.saturating_duration_since(now))
        });
    match chain
        .verify_endpoint_identity(&resources.clients, endpoint, timeout)
        .await
    {
        Ok(()) => ProbeOutcome::Verified,
        Err(error) => ProbeOutcome::Failed(error),
    }
}

/// Waits for a permit while any waiter is live, re-snapshotting when the earliest deadline passes.
async fn acquire_live_permit(
    semaphore: &Arc<Semaphore>,
    waiters: &[Arc<WaiterState>],
) -> Result<OwnedSemaphorePermit, ProbeOutcome> {
    loop {
        let current = ScopedView::from_waiters(waiters, Instant::now());
        if current.members.is_empty() {
            return Err(ProbeOutcome::NotDispatched);
        }
        match acquire_permit(semaphore, current.earliest_permit_deadline()).await {
            PermitAcquisition::Acquired(permit) => return Ok(permit),
            PermitAcquisition::DeadlineElapsed => {}
            PermitAcquisition::Closed => return Err(ProbeOutcome::Closed),
        }
    }
}

async fn sleep_until_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
async fn execute_aggregate_with_failover_attempt(
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    route: RpcRoute,
    endpoints: Vec<SensitiveUrl>,
    items: &[WorkItem],
) -> ReductionOutput {
    let resources = JobResources {
        http: semaphore,
        ..JobResources::new(super::model::RpcClients::new(client.clone(), client), 1)
    };
    let mut failover = AggregateFailover::new(route, items);
    for endpoint in endpoints.iter().map(SensitiveUrl::expose_url) {
        if failover.live(Instant::now()).is_none() {
            break;
        }
        let output = failover.attempt(&resources, endpoint).await;
        if !failover.apply(endpoint, output) {
            break;
        }
    }
    failover.into_output()
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

    /// A view over independent waiter sets, indexed by position.
    fn from_waiters(waiters: &[Arc<WaiterState>], now: Instant) -> Self {
        Self {
            members: waiters
                .iter()
                .enumerate()
                .filter_map(|(index, waiters)| {
                    let snapshot = waiters.snapshot(now);
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
#[cfg(test)]
async fn execute_single_with_shared_waiters_attempt(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    read: &RpcRead,
    chain_id: u64,
    endpoints: &[SensitiveUrl],
    waiters: Arc<WaiterState>,
    fallback_attempt_timeout: Duration,
) -> (AttemptOutcome<RpcResult>, Vec<(Url, EndpointHealthOutcome)>) {
    let resources = JobResources {
        http: Arc::clone(semaphore),
        ..JobResources::new(
            super::model::RpcClients::new(client.clone(), client.clone()),
            1,
        )
    };
    let route = RpcChainRoute::new(chain_id, endpoints.to_vec());
    let mut failover =
        SingleFailover::new(read, waiters, &route, chain_id, fallback_attempt_timeout);
    for endpoint in endpoints.iter().map(SensitiveUrl::expose_url) {
        let outcome = failover.attempt(&resources, endpoint).await;
        if !failover.apply(endpoint, outcome) {
            break;
        }
    }
    failover.into_output()
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
