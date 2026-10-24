use super::cache::ReadCache;
use super::capacity::{CapacityLedger, JobControl, JobResources, JobSlots, SlotKind, SlotRequest};
use super::identity::CandidateOrder;
use super::model::RpcResult;
use super::model::{
    JsonRpcFailurePolicy, RpcBrokerError, RpcChainRoute, RpcClients, RpcOrigin, RpcRead, RpcRoute,
    RpcSubmission,
};
use super::profile::EndpointProfile;
use super::resolution::{ActiveWork, ReadReply, WaiterPolicy, WaiterState, WorkItem, WorkKey};
use super::scheduler::{Claim, ExecutionJob, ReadyScheduler, RouteLoad};
use super::warmup::{self, WarmupPass, WarmupRequest};
use alloy::eips::{BlockId, BlockNumberOrTag};
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::futures::Notified;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Instant};
use url::Url;

pub(super) struct Pending {
    pub(super) submission: RpcSubmission,
    pub(super) replies: Vec<ReadReply>,
    pub(super) deadline: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndpointHealthOutcome {
    Healthy,
    Neutral,
    Strike,
}

impl EndpointHealthOutcome {
    pub(super) fn for_result<T>(result: &Result<T, RpcBrokerError>) -> Self {
        match result {
            Ok(_) | Err(RpcBrokerError::InnerRevert(_)) => Self::Healthy,
            Err(
                RpcBrokerError::Timeout
                | RpcBrokerError::Transport
                | RpcBrokerError::InvalidResponse
                | RpcBrokerError::HttpStatus(_),
            ) => Self::Strike,
            Err(RpcBrokerError::Remote(remote))
                if matches!(
                    JsonRpcFailurePolicy::from(remote.code()),
                    JsonRpcFailurePolicy::TransientStrike
                        | JsonRpcFailurePolicy::UnrecoverableStrike
                ) =>
            {
                Self::Strike
            }
            Err(_) => Self::Neutral,
        }
    }

    pub(super) const fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Strike, _) | (_, Self::Strike) => Self::Strike,
            (Self::Neutral, _) | (_, Self::Neutral) => Self::Neutral,
            _ => Self::Healthy,
        }
    }

    pub(super) fn for_results<T>(results: &[Result<T, RpcBrokerError>]) -> Self {
        results.iter().fold(Self::Healthy, |outcome, result| {
            outcome.combine(Self::for_result(result))
        })
    }
}

pub(super) struct RequestEvent {
    pub(super) chain_id: u64,
    pub(super) endpoint: Url,
    pub(super) health_outcome: EndpointHealthOutcome,
}

impl RequestEvent {
    pub(super) const fn new(
        chain_id: u64,
        endpoint: Url,
        health_outcome: EndpointHealthOutcome,
    ) -> Self {
        Self {
            chain_id,
            endpoint,
            health_outcome,
        }
    }
}

pub(super) struct JobOutput {
    pub(super) completions: Vec<(WorkKey, Result<RpcResult, RpcBrokerError>)>,
    pub(super) requests: Vec<RequestEvent>,
}

impl JobOutput {
    pub(super) const fn noop() -> Self {
        Self {
            completions: Vec::new(),
            requests: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum BlockEvent {
    HeadObserved { chain_id: u64, block_number: u64 },
    Invalidate { chain_id: u64 },
}

pub(super) type JobExecutor = Arc<
    dyn Fn(
            JobResources,
            ExecutionJob,
            Vec<poi::SensitiveUrl>,
            JobControl,
        ) -> BoxFuture<'static, JobOutput>
        + Send
        + Sync,
>;

pub(super) enum Command {
    Submit {
        submission: Box<RpcSubmission>,
        replies: Vec<ReadReply>,
        deadline: Option<Instant>,
    },
    /// Answered at once with the route's eligible endpoints, without admission, slots or HTTP.
    Candidates {
        route: RpcChainRoute,
        reply: oneshot::Sender<Result<Vec<poi::SensitiveUrl>, RpcBrokerError>>,
    },
}

/// How a claimed job starts.
#[derive(Clone, Copy)]
enum StartMode {
    /// With a read slot, and a probe runner if it needs one and one is free.
    Read,
    /// With a probe runner only, because no selected endpoint can read yet.
    Verify,
}

/// Whether a queued job can use a read slot now: its route needs no identity verification, or
/// one of its endpoints is already verified.
fn job_read_ready(job: &ExecutionJob) -> bool {
    job.first().is_none_or(|item| {
        let route = item.execution_route.chain_route();
        !route.requires_identity_verification()
            || route
                .endpoints()
                .iter()
                .any(|endpoint| route.endpoint_identity_verified(endpoint))
    })
}

pub(super) struct Actor {
    rx: mpsc::Receiver<Command>,
    interval: Duration,
    resources: JobResources,
    executor: JobExecutor,
    pending: Vec<Pending>,
    active: HashMap<WorkKey, ActiveWork>,
    next_work_nonce: u64,
    jobs: FuturesUnordered<BoxFuture<'static, JobOutput>>,
    /// Bounds read-active jobs. Probe runners use the independent verification limit.
    max_in_flight: usize,
    ledger: Arc<CapacityLedger>,
    slot_tx: mpsc::UnboundedSender<SlotRequest>,
    slot_rx: mpsc::UnboundedReceiver<SlotRequest>,
    /// Started jobs waiting for a probe runner, oldest first.
    runner_waiters: VecDeque<SlotRequest>,
    scheduler: ReadyScheduler,
    pub(super) profiles: HashMap<(u64, Url), EndpointProfile>,
    cache: ReadCache,
    next_flush_at: Option<Instant>,
    candidate_order: CandidateOrder,
    /// Kept so the warmup channel never closes while the actor runs.
    warmup_tx: mpsc::UnboundedSender<WarmupRequest>,
    warmup_rx: mpsc::UnboundedReceiver<WarmupRequest>,
    /// The current installation's warmup pass; a newer request replaces it.
    warmup: Option<WarmupPass>,
}

impl Actor {
    pub(super) fn new(
        clients: RpcClients,
        rx: mpsc::Receiver<Command>,
        interval: Duration,
        max_in_flight: usize,
        executor: JobExecutor,
        candidate_order: CandidateOrder,
    ) -> Self {
        let (slot_tx, slot_rx) = mpsc::unbounded_channel();
        let (warmup_tx, warmup_rx) = mpsc::unbounded_channel();
        Self {
            rx,
            interval,
            // Read slots and probe runners are counted by the ledger; these semaphores
            // independently cap physical HTTP attempts, including concurrent reduction fan-out
            // within one job, and identity probes.
            resources: JobResources::new(clients, max_in_flight),
            executor,
            pending: Vec::new(),
            active: HashMap::new(),
            next_work_nonce: 1,
            jobs: FuturesUnordered::new(),
            max_in_flight,
            ledger: CapacityLedger::new(),
            slot_tx,
            slot_rx,
            runner_waiters: VecDeque::new(),
            scheduler: ReadyScheduler::new(),
            profiles: HashMap::new(),
            cache: ReadCache::default(),
            next_flush_at: None,
            candidate_order,
            warmup_tx,
            warmup_rx,
            warmup: None,
        }
    }
    /// Sender for the broker handle's warmup requests.
    pub(super) fn warmup_requests(&self) -> mpsc::UnboundedSender<WarmupRequest> {
        self.warmup_tx.clone()
    }
    pub(super) async fn run(mut self, mut block_rx: mpsc::UnboundedReceiver<BlockEvent>) {
        let mut block_events_closed = false;
        loop {
            // Queued work that cannot read yet becomes ready when its route verifies an
            // endpoint. A check outside the broker signals only that route's clones.
            let watched = self.scheduler.unready_routes(job_read_ready);
            let mut checks = watched
                .iter()
                .map(|route| Box::pin(route.identity_checked()))
                .collect::<Vec<Pin<Box<Notified<'_>>>>>();
            // Subscribed first, so this pump sees a check that finished since the last one and a
            // later check wakes the select below.
            self.pump_ready();
            let ledger = Arc::clone(&self.ledger);
            let next_wake = self.next_wake_at();
            let timer = async move {
                if let Some(deadline) = next_wake {
                    time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(timer);
            tokio::select! {
                command = self.rx.recv() => match command {
                    Some(Command::Submit { submission, replies, deadline }) => {
                        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                            for reply in replies {
                                reply.send(Err(RpcBrokerError::TimeoutBeforeDispatch));
                            }
                            continue;
                        }
                        if self.would_cross_threshold(&submission) {
                            self.flush();
                        }
                        let exceeds_after_admission = Self::pending_exceeds(&submission);
                        let was_empty = self.pending.is_empty();
                        self.pending.push(Pending {
                            submission: *submission,
                            replies,
                            deadline,
                        });
                        if was_empty {
                            self.next_flush_at = Some(Instant::now() + self.interval);
                        }
                        if exceeds_after_admission || self.any_threshold_reached() {
                            self.flush();
                        }
                    }
                    Some(Command::Candidates { route, reply }) => {
                        let _ = reply.send(self.select_candidates(&route));
                    }
                    None => {
                        // Shutdown never waits for warmup probes.
                        self.warmup = None;
                        self.fail_pending(&RpcBrokerError::Shutdown);
                        // Running jobs waiting for a slot settle their work as shut down.
                        self.slot_rx.close();
                        while self.slot_rx.try_recv().is_ok() {}
                        self.runner_waiters.clear();
                        self.fail_ready(&RpcBrokerError::Shutdown);
                        self.drain_jobs().await;
                        self.fail_ready(&RpcBrokerError::Shutdown);
                        self.fail_queued(&RpcBrokerError::Shutdown);
                        break;
                    }
                },
                () = &mut timer, if next_wake.is_some() => {
                    let now = Instant::now();
                    self.expire_pending();
                    self.expire_ready();
                    if self.next_flush_at.is_some_and(|deadline| deadline <= now) {
                        self.flush();
                    }
                    self.pump_ready();
                },
                event = async {
                    if block_events_closed {
                        std::future::pending().await
                    } else {
                        block_rx.recv().await
                    }
                } => match event {
                    Some(BlockEvent::HeadObserved { chain_id, block_number }) => {
                        let now = Instant::now();
                        // Notifications are advisory and arrive from more than one
                        // observer, so a repeated or stale head must not move the
                        // epoch backwards and must not discard results still valid
                        // for the head already recorded.
                        if self.cache.observe_head(chain_id, block_number, now) {
                            self.flush();
                        }
                    }
                    Some(BlockEvent::Invalidate { chain_id }) => {
                        self.cache.invalidate_head(chain_id);
                        self.flush();
                    }
                    None => block_events_closed = true,
                },
                Some(output) = self.jobs.next() => {
                    self.complete_job(output);
                    self.pump_ready();
                },
                Some(request) = self.slot_rx.recv() => {
                    match request.kind {
                        SlotKind::Read => self.admit_resume(request),
                        SlotKind::Runner => self.runner_waiters.push_back(request),
                    }
                    self.pump_ready();
                },
                () = ledger.changed() => self.pump_ready(),
                Some(request) = self.warmup_rx.recv() => {
                    // Dropping the previous pass drops its probes and their permits.
                    self.warmup = Some(warmup::pass(
                        self.resources.clone(),
                        request,
                        self.candidate_order,
                    ));
                },
                event = warmup::next_event(self.warmup.as_mut(), &self.profiles) => match event {
                    Some(event) => self.apply_warmup_event(&event),
                    None => self.warmup = None,
                },
                // Built inside the branch because `select_all` panics on an empty set.
                () = async {
                    futures_util::future::select_all(checks.iter_mut()).await;
                }, if !checks.is_empty() => self.pump_ready(),
            }
        }
    }
    pub(super) fn next_wake_at(&self) -> Option<Instant> {
        let mut next = self.next_flush_at;
        let mut consider = |candidate: Option<Instant>| {
            if let Some(candidate) = candidate {
                next = Some(next.map_or(candidate, |current| current.min(candidate)));
            }
        };
        for pending in &self.pending {
            consider(pending.deadline);
        }
        self.scheduler.deadlines(&mut consider);
        let now = Instant::now();
        for profile in self.profiles.values() {
            consider(profile.withdrawn_until.filter(|at| *at > now));
        }
        next
    }
    pub(super) fn would_cross_threshold(&self, submission: &RpcSubmission) -> bool {
        let pending = self.pending_load(submission.route());
        let incoming = RouteLoad::from(submission);
        let mut combined = pending;
        combined.add(incoming);
        combined.exceeds(submission.route())
    }
    pub(super) fn pending_exceeds(submission: &RpcSubmission) -> bool {
        RouteLoad::from(submission).exceeds(submission.route())
    }
    pub(super) fn any_threshold_reached(&self) -> bool {
        #[expect(
            clippy::mutable_key_type,
            reason = "route equality and hashing exclude the endpoint verification cache and the \
                      isolation client"
        )]
        let mut loads = HashMap::<&RpcRoute, RouteLoad>::new();
        for pending in &self.pending {
            let route = pending.submission.route();
            loads
                .entry(route)
                .or_default()
                .add(RouteLoad::from(&pending.submission));
        }
        loads.into_iter().any(|(route, load)| load.reaches(route))
    }
    fn pending_load(&self, route: &RpcRoute) -> RouteLoad {
        self.pending
            .iter()
            .filter(|pending| pending.submission.route() == route)
            .map(|pending| RouteLoad::from(&pending.submission))
            .sum()
    }

    fn resolve_read(
        &mut self,
        route: RpcRoute,
        read: RpcRead,
        reply: ReadReply,
        deadline: Option<Instant>,
        origin: RpcOrigin,
        current_batch: &mut Vec<WorkItem>,
    ) {
        let latest_epoch = matches!(
            read.reuse_block_id(),
            Some(BlockId::Number(BlockNumberOrTag::Latest))
        )
        .then(|| self.cache.reconcile_latest_cache_epoch(route.chain_id()))
        .flatten();
        let is_dedupable = read.is_dedupable()
            && (!matches!(
                read.reuse_block_id(),
                Some(BlockId::Number(BlockNumberOrTag::Latest))
            ) || latest_epoch.is_some());
        let key = WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce: if is_dedupable {
                0
            } else {
                let nonce = self.next_work_nonce;
                self.next_work_nonce = self.next_work_nonce.saturating_add(1);
                nonce
            },
            latest_epoch,
        };
        let expected_epoch = match read.reuse_block_id() {
            Some(BlockId::Number(BlockNumberOrTag::Latest)) => {
                latest_epoch.map(|epoch| epoch.block_number)
            }
            Some(BlockId::Number(BlockNumberOrTag::Number(number))) => Some(number),
            Some(BlockId::Hash(hash)) if hash.require_canonical != Some(true) => Some(0),
            _ => None,
        };
        // Isolated reads neither use nor fill the cache, which is shared by every route.
        if read.is_cacheable()
            && !key.route.is_isolated()
            && expected_epoch.is_some()
            && let Some(cached) = self.cache.lookup(&key.identity, expected_epoch)
        {
            reply.send(cached);
            return;
        }
        if is_dedupable && let Some(active) = self.active.get_mut(&key) {
            active.waiters.add(WaiterPolicy {
                deadline,
                attempt_timeout: route.attempt_timeout,
            });
            active.replies.push(reply);
            active.origins.push(origin.clone());
            if let Some(item) = current_batch.iter_mut().find(|item| item.key == key) {
                item.origins.push(origin);
            } else {
                self.scheduler.attach(&key, &origin);
            }
            return;
        }
        let waiters = WaiterState::new(WaiterPolicy {
            deadline,
            attempt_timeout: route.attempt_timeout,
        });
        self.active.insert(
            key.clone(),
            ActiveWork {
                read: read.clone(),
                execution_route: route.clone(),
                origins: vec![origin.clone()],
                replies: vec![reply],
                waiters: waiters.clone(),
            },
        );
        current_batch.push(WorkItem {
            key,
            execution_route: route,
            read,
            origins: vec![origin],
            waiters,
        });
    }
    pub(super) fn flush(&mut self) {
        self.next_flush_at = None;
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return;
        }
        let mut work: Vec<WorkItem> = Vec::new();
        for pending in pending {
            let Pending {
                submission,
                replies,
                deadline,
            } = pending;
            if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                for reply in replies {
                    reply.send(Err(RpcBrokerError::TimeoutBeforeDispatch));
                }
                continue;
            }
            let (route, reads, origin) = submission.into_parts();
            for (read, reply) in reads.into_iter().zip(replies) {
                self.resolve_read(
                    route.clone(),
                    read,
                    reply,
                    deadline,
                    origin.clone(),
                    &mut work,
                );
            }
        }
        self.scheduler.partition_and_admit(work);
        self.pump_ready();
    }
    pub(super) fn expire_pending(&mut self) {
        let now = Instant::now();
        self.pending.retain_mut(|pending| {
            if pending.deadline.is_some_and(|deadline| deadline <= now) {
                for reply in std::mem::take(&mut pending.replies) {
                    reply.send(Err(RpcBrokerError::TimeoutBeforeDispatch));
                }
                false
            } else {
                true
            }
        });
        if self.pending.is_empty() {
            self.next_flush_at = None;
        }
    }
    pub(super) fn expire_ready(&mut self) {
        let completions = self.scheduler.expire(Instant::now());
        if !completions.is_empty() {
            self.complete_job(JobOutput {
                completions,
                requests: Vec::new(),
            });
        }
    }
    /// Hands free read slots to fairly scheduled read-ready work, then free probe runners to
    /// waiting jobs and queued work that cannot read yet.
    pub(super) fn pump_ready(&mut self) {
        self.expire_ready();
        // A job that stopped waiting, e.g. because another owner verified its endpoint, must not
        // keep its route's client alive or hold a runner turn while every runner is busy.
        self.runner_waiters
            .retain(|request| !request.grant.is_closed());
        while self.ledger.held(SlotKind::Read) < self.max_in_flight {
            match self.scheduler.next(job_read_ready) {
                Some(Claim::Queued(job)) => self.start_job(job, StartMode::Read),
                Some(Claim::Resume(request)) => request.grant(),
                None => break,
            }
        }
        while self.ledger.held(SlotKind::Runner) < self.resources.probe_limit {
            if let Some(request) = self.runner_waiters.pop_front() {
                request.grant();
            } else if let Some(job) = self.scheduler.next_verification(job_read_ready) {
                self.start_job(job, StartMode::Verify);
            } else {
                break;
            }
        }
    }
    /// Queues a started job's read request under the current origins of its work, including
    /// callers that joined it after the job started.
    fn admit_resume(&mut self, request: SlotRequest) {
        if request.grant.is_closed() {
            return;
        }
        let mut origins: Vec<RpcOrigin> = Vec::new();
        for active in request.keys.iter().filter_map(|key| self.active.get(key)) {
            for origin in &active.origins {
                if !origins.contains(origin) {
                    origins.push(origin.clone());
                }
            }
        }
        self.scheduler.admit_resume(request, origins);
    }
    fn start_job(&mut self, job: ExecutionJob, mode: StartMode) {
        let now = Instant::now();
        let job = match job {
            ExecutionJob::Aggregate(items) => {
                let mut retained = Vec::with_capacity(items.len());
                let mut expired = Vec::new();
                for item in items {
                    if item
                        .waiters
                        .merged_deadline()
                        .is_some_and(|deadline| deadline <= now)
                    {
                        expired.push((item.key, Err(RpcBrokerError::TimeoutBeforeDispatch)));
                    } else {
                        retained.push(item);
                    }
                }
                if !expired.is_empty() {
                    self.complete_job(JobOutput {
                        completions: expired,
                        requests: Vec::new(),
                    });
                }
                (!retained.is_empty()).then_some(ExecutionJob::Aggregate(retained))
            }
            ExecutionJob::Individual(item) => {
                if item
                    .waiters
                    .merged_deadline()
                    .is_some_and(|deadline| deadline <= now)
                {
                    self.complete_job(JobOutput {
                        completions: vec![(item.key, Err(RpcBrokerError::TimeoutBeforeDispatch))],
                        requests: Vec::new(),
                    });
                    None
                } else {
                    Some(ExecutionJob::Individual(item))
                }
            }
        };
        let Some(job) = job else {
            return;
        };
        let Some(first) = job.first() else {
            return;
        };
        let selected = match self.select_route(&first.execution_route) {
            Ok(selection) => selection,
            Err(error) => {
                self.complete_job(JobOutput {
                    completions: job
                        .into_iter()
                        .map(|item| (item.key, Err(error.clone())))
                        .collect(),
                    requests: Vec::new(),
                });
                return;
            }
        };
        let route = first.execution_route.chain_route();
        let needs_runner = selected
            .iter()
            .any(|endpoint| !route.endpoint_identity_verified(endpoint));
        let keys = job.items().iter().map(|item| item.key.clone()).collect();
        let slots = JobSlots::new(Arc::clone(&self.ledger));
        match mode {
            StartMode::Read => {
                slots.acquire(SlotKind::Read);
                // Jobs already waiting for a runner keep their turn.
                if needs_runner
                    && self.ledger.held(SlotKind::Runner) + self.runner_waiters.len()
                        < self.resources.probe_limit
                {
                    slots.acquire(SlotKind::Runner);
                }
            }
            StartMode::Verify => slots.acquire(SlotKind::Runner),
        }
        let control = JobControl::new(
            Arc::clone(&slots),
            self.slot_tx.clone(),
            matches!(job, ExecutionJob::Aggregate(_)),
            keys,
        );
        let future = (self.executor)(self.resources.clone(), job, selected, control);
        // The actor keeps the slots until the job finishes, even if an executor drops its control.
        self.jobs.push(Box::pin(async move {
            let _slots = slots;
            future.await
        }));
    }
    pub(super) fn select_route(
        &mut self,
        route: &RpcRoute,
    ) -> Result<Vec<poi::SensitiveUrl>, RpcBrokerError> {
        let route = route.chain_route();
        let (mut ordered, counts) = self.eligible_endpoints(route)?;
        // The hook orders ties; the stable sort still puts lower session counts first.
        ordered.sort_by_key(|&index| counts[index]);
        let endpoints = ordered
            .into_iter()
            .map(|index| route.endpoints()[index].clone())
            .collect::<Vec<_>>();
        Ok(endpoints)
    }
    /// Eligible endpoints in hook order for a caller that sends its own requests. Session counts
    /// exclude that traffic, so ranking by them would keep favoring the same endpoints.
    pub(super) fn select_candidates(
        &mut self,
        route: &RpcChainRoute,
    ) -> Result<Vec<poi::SensitiveUrl>, RpcBrokerError> {
        let (ordered, _) = self.eligible_endpoints(route)?;
        let endpoints = ordered
            .into_iter()
            .map(|index| &route.endpoints()[index])
            .filter(|endpoint| {
                !self
                    .warmup
                    .as_ref()
                    .is_some_and(|pass| pass.excludes(route, endpoint))
            })
            .cloned()
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            return Err(RpcBrokerError::NoEndpoint {
                chain_id: route.chain_id(),
            });
        }
        Ok(endpoints)
    }
    /// Indices of the endpoints not withdrawn, or of every endpoint when all are, in hook order,
    /// with each configured endpoint's session request count.
    fn eligible_endpoints(
        &mut self,
        route: &RpcChainRoute,
    ) -> Result<(Vec<usize>, Vec<u64>), RpcBrokerError> {
        if route.endpoints().is_empty() {
            return Err(RpcBrokerError::NoEndpoint {
                chain_id: route.chain_id(),
            });
        }
        let now = Instant::now();
        let mut candidates = Vec::new();
        let mut withdrawn = Vec::new();
        let mut counts = Vec::with_capacity(route.endpoints().len());
        for (index, endpoint) in route.endpoints().iter().enumerate() {
            let url = endpoint.expose_url().clone();
            let recovering = self
                .warmup
                .as_ref()
                .is_some_and(|pass| pass.excludes(route, endpoint));
            let profile = self
                .profiles
                .entry((route.chain_id(), url.clone()))
                .or_default();
            if !recovering && profile.restore_if_ready(now) {
                tracing::debug!(
                    chain_id = route.chain_id(),
                    endpoint = %crate::http::redact_url_for_display(&url),
                    "RPC endpoint restored"
                );
            }
            counts.push(profile.request_count);
            if recovering || profile.is_withdrawn(now) {
                withdrawn.push(index);
            } else {
                candidates.push(index);
            }
        }
        let all_withdrawn = candidates.is_empty();
        if all_withdrawn {
            tracing::debug!(
                chain_id = route.chain_id(),
                "all RPC endpoints withdrawn; selecting cooldown endpoint"
            );
        }
        let mut ordered = if all_withdrawn { withdrawn } else { candidates };
        (self.candidate_order)(&mut ordered);
        Ok((ordered, counts))
    }
    pub(super) fn complete_job(&mut self, output: JobOutput) {
        self.apply_request_events(&output.requests);
        let mut undispatched = Vec::new();
        for (key, result) in output.completions {
            if result == Err(RpcBrokerError::TimeoutBeforeDispatch)
                && let Some(active) = self.active.get(&key)
                && active.waiters.snapshot(Instant::now()).has_live
            {
                // The completed aggregate may have dropped this member before a later
                // waiter attached. Keep its admission owners until the replacement retires.
                undispatched.push(WorkItem {
                    key,
                    execution_route: active.execution_route.clone(),
                    read: active.read.clone(),
                    origins: active.origins.clone(),
                    waiters: active.waiters.clone(),
                });
                continue;
            }
            if let Some(active) = self.active.remove(&key) {
                let read = active.read;
                if read.is_cacheable() && result.is_ok() && !key.route.is_isolated() {
                    let chain_id = key.identity.chain_id();
                    let observed = match read.reuse_block_id() {
                        Some(BlockId::Number(BlockNumberOrTag::Latest)) => self
                            .cache
                            .reconcile_latest_cache_epoch(chain_id)
                            .map(|epoch| epoch.block_number),
                        Some(BlockId::Number(BlockNumberOrTag::Number(number))) => Some(number),
                        Some(BlockId::Hash(_)) => Some(0),
                        _ => None,
                    };
                    let latest_still_current =
                        self.cache.latest_is_current(chain_id, key.latest_epoch);
                    if !matches!(
                        read.reuse_block_id(),
                        Some(BlockId::Number(BlockNumberOrTag::Latest))
                    ) || latest_still_current
                    {
                        self.cache
                            .insert(key.identity.clone(), observed, result.clone());
                    }
                }
                for waiter in active.replies {
                    waiter.send(result.clone());
                }
            }
        }
        self.scheduler.partition_and_admit(undispatched);
    }
    pub(super) fn apply_request_events(&mut self, events: &[RequestEvent]) {
        let now = Instant::now();
        for event in events {
            self.apply_request_event(event, now, false);
        }
    }
    /// Warmup strikes withdraw immediately; a successful recovery clears its cooldown.
    pub(super) fn apply_warmup_event(&mut self, event: &RequestEvent) {
        if event.health_outcome == EndpointHealthOutcome::Healthy {
            let profile = self
                .profiles
                .get_mut(&(event.chain_id, event.endpoint.clone()));
            if let Some(profile) = profile {
                profile.withdrawn_until = None;
                tracing::debug!(
                    chain_id = event.chain_id,
                    endpoint = %crate::http::redact_url_for_display(&event.endpoint),
                    "RPC endpoint recovered"
                );
            }
        } else {
            self.apply_request_event(event, Instant::now(), true);
        }
    }
    fn apply_request_event(&mut self, event: &RequestEvent, now: Instant, warmup: bool) {
        let receiving_endpoint = crate::http::redact_url_for_display(&event.endpoint);
        let key = (event.chain_id, event.endpoint.clone());
        let profile = self.profiles.entry(key).or_default();
        profile.request_count = profile.request_count.saturating_add(1);
        match event.health_outcome {
            EndpointHealthOutcome::Strike => {
                let withdrawn = if warmup {
                    profile.withdraw_after_warmup_strike(now)
                } else {
                    profile.record_failure(now)
                };
                if withdrawn {
                    tracing::debug!(
                        chain_id = event.chain_id,
                        endpoint = %receiving_endpoint,
                        withdrawal_level = profile.withdrawal_level,
                        cooldown_ms = profile
                            .withdrawn_until
                            .map_or(0, |until| until.saturating_duration_since(now).as_millis()),
                        "RPC endpoint withdrawn"
                    );
                }
            }
            EndpointHealthOutcome::Healthy | EndpointHealthOutcome::Neutral => {}
        }
    }
    pub(super) async fn drain_jobs(&mut self) {
        while let Some(output) = self.jobs.next().await {
            self.complete_job(output);
        }
    }
    pub(super) fn fail_pending(&mut self, error: &RpcBrokerError) {
        self.next_flush_at = None;
        for pending in self.pending.drain(..) {
            for reply in pending.replies {
                reply.send(Err(error.clone()));
            }
        }
    }
    pub(super) fn fail_ready(&mut self, error: &RpcBrokerError) {
        let completions = self.scheduler.drain(error);
        if !completions.is_empty() {
            self.complete_job(JobOutput {
                completions,
                requests: Vec::new(),
            });
        }
    }
    pub(super) fn fail_queued(&mut self, error: &RpcBrokerError) {
        while let Ok(command) = self.rx.try_recv() {
            // Dropping a candidate lookup's reply reports shutdown to its caller.
            if let Command::Submit { replies, .. } = command {
                for reply in replies {
                    reply.send(Err(error.clone()));
                }
            }
        }
    }
}
