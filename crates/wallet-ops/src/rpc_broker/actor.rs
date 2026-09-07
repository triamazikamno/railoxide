use super::cache::ReadCache;
use super::model::RpcResult;
use super::model::{
    JsonRpcFailurePolicy, RpcBrokerError, RpcOrigin, RpcRead, RpcRoute, RpcSubmission,
};
use super::profile::EndpointProfile;
use super::resolution::{ActiveWork, ReadReply, WaiterPolicy, WaiterState, WorkItem, WorkKey};
use super::scheduler::{ExecutionJob, ReadyScheduler, RouteLoad};
use alloy::eips::{BlockId, BlockNumberOrTag};
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};
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
            reqwest::Client,
            Arc<Semaphore>,
            ExecutionJob,
            Vec<poi::SensitiveUrl>,
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
}

pub(super) struct Actor {
    client: reqwest::Client,
    rx: mpsc::Receiver<Command>,
    interval: Duration,
    semaphore: Arc<Semaphore>,
    executor: JobExecutor,
    pending: Vec<Pending>,
    active: HashMap<WorkKey, ActiveWork>,
    next_work_nonce: u64,
    jobs: FuturesUnordered<BoxFuture<'static, JobOutput>>,
    max_in_flight: usize,
    scheduler: ReadyScheduler,
    profiles: HashMap<(u64, Url), EndpointProfile>,
    cache: ReadCache,
    next_flush_at: Option<Instant>,
}

impl Actor {
    pub(super) fn new(
        client: reqwest::Client,
        rx: mpsc::Receiver<Command>,
        interval: Duration,
        max_in_flight: usize,
        executor: JobExecutor,
    ) -> Self {
        Self {
            client,
            rx,
            interval,
            // Top-level jobs are capped by `max_in_flight`; this semaphore independently caps
            // physical HTTP attempts, including concurrent reduction fan-out within one job.
            semaphore: Arc::new(Semaphore::new(max_in_flight)),
            executor,
            pending: Vec::new(),
            active: HashMap::new(),
            next_work_nonce: 1,
            jobs: FuturesUnordered::new(),
            max_in_flight,
            scheduler: ReadyScheduler::new(),
            profiles: HashMap::new(),
            cache: ReadCache::default(),
            next_flush_at: None,
        }
    }
    pub(super) async fn run(mut self, mut block_rx: mpsc::UnboundedReceiver<BlockEvent>) {
        let mut block_events_closed = false;
        loop {
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
                    None => {
                        self.fail_pending(&RpcBrokerError::Shutdown);
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
        if read.is_cacheable()
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
    pub(super) fn pump_ready(&mut self) {
        self.expire_ready();
        while let Some(job) = self.scheduler.next(self.jobs.len(), self.max_in_flight) {
            self.start_job(job);
        }
    }
    fn start_job(&mut self, job: ExecutionJob) {
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
        let future = (self.executor)(self.client.clone(), self.semaphore.clone(), job, selected);
        self.jobs.push(future);
    }
    pub(super) fn select_route(
        &mut self,
        route: &RpcRoute,
    ) -> Result<Vec<poi::SensitiveUrl>, RpcBrokerError> {
        if route.endpoints().is_empty() {
            return Err(RpcBrokerError::NoEndpoint {
                chain_id: route.chain_id(),
            });
        }
        let now = Instant::now();
        let mut candidates = Vec::new();
        let mut withdrawn = Vec::new();
        for (index, endpoint) in route.endpoints().iter().enumerate() {
            let url = endpoint.expose_url().clone();
            let profile = self
                .profiles
                .entry((route.chain_id(), url.clone()))
                .or_default();
            if profile.restore_if_ready(now) {
                tracing::debug!(
                    chain_id = route.chain_id(),
                    endpoint = %crate::http::redact_url_for_display(&url),
                    "RPC endpoint restored"
                );
            }
            if profile.is_withdrawn(now) {
                withdrawn.push((profile.request_count, index));
            } else {
                candidates.push((profile.request_count, index));
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
        ordered.sort_by_key(|(count, _)| *count);
        let endpoints = ordered
            .into_iter()
            .map(|(_, index)| route.endpoints()[index].clone())
            .collect::<Vec<_>>();
        Ok(endpoints)
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
                if read.is_cacheable() && result.is_ok() {
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
            let receiving_endpoint = crate::http::redact_url_for_display(&event.endpoint);
            let key = (event.chain_id, event.endpoint.clone());
            let profile = self.profiles.entry(key).or_default();
            profile.request_count = profile.request_count.saturating_add(1);
            match event.health_outcome {
                EndpointHealthOutcome::Strike => {
                    if profile.record_failure(now) {
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
            let Command::Submit { replies, .. } = command;
            for reply in replies {
                reply.send(Err(error.clone()));
            }
        }
    }
}
