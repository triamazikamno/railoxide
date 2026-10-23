use super::capacity::SlotRequest;
use super::model::RpcResult;
use super::model::{BlockKey, RpcBrokerError, RpcChainRoute, RpcOrigin, RpcRoute, RpcSubmission};
use super::resolution::{WorkItem, WorkKey};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::time::Instant;

pub(super) enum ExecutionJob {
    Aggregate(Vec<WorkItem>),
    Individual(Box<WorkItem>),
}

#[cfg(test)]
#[path = "tests/scheduler.rs"]
mod tests;

impl ExecutionJob {
    pub(super) fn items(&self) -> &[WorkItem] {
        match self {
            Self::Aggregate(items) => items,
            Self::Individual(item) => std::slice::from_ref(&**item),
        }
    }

    pub(super) fn first(&self) -> Option<&WorkItem> {
        self.items().first()
    }
}

impl IntoIterator for ExecutionJob {
    type Item = WorkItem;
    type IntoIter = std::vec::IntoIter<WorkItem>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Aggregate(items) => items.into_iter(),
            Self::Individual(item) => vec![*item].into_iter(),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct RouteLoad {
    calls: usize,
    gas: u64,
    overflowed: bool,
}

impl RouteLoad {
    pub(super) const fn item(gas: u64) -> Self {
        Self {
            calls: 1,
            gas,
            overflowed: false,
        }
    }

    pub(super) fn add(&mut self, other: Self) {
        let (calls, calls_overflowed) = self
            .calls
            .checked_add(other.calls)
            .map_or((usize::MAX, true), |calls| (calls, false));
        let (gas, gas_overflowed) = self
            .gas
            .checked_add(other.gas)
            .map_or((u64::MAX, true), |gas| (gas, false));
        self.calls = calls;
        self.gas = gas;
        self.overflowed |= other.overflowed || calls_overflowed || gas_overflowed;
    }

    pub(super) const fn exceeds(self, route: &RpcRoute) -> bool {
        self.overflowed || self.calls > route.max_calls || self.gas > route.max_estimated_gas
    }

    pub(super) const fn reaches(self, route: &RpcRoute) -> bool {
        self.overflowed || self.calls >= route.max_calls || self.gas >= route.max_estimated_gas
    }
}

impl std::iter::Sum for RouteLoad {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |mut load, item| {
            load.add(item);
            load
        })
    }
}

impl From<&RpcSubmission> for RouteLoad {
    fn from(submission: &RpcSubmission) -> Self {
        submission
            .reads()
            .iter()
            .map(|read| Self::item(read.estimated_gas()))
            .sum()
    }
}

pub(super) fn partition_work(work: Vec<WorkItem>) -> Vec<Vec<WorkItem>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut load = RouteLoad::default();
    for item in work {
        let route = &item.execution_route;
        let item_gas = item.read.estimated_gas();
        let mut next_load = load;
        next_load.add(RouteLoad::item(item_gas));
        let over_count = current.len() >= route.max_calls.max(1);
        if !current.is_empty() && (over_count || next_load.exceeds(route)) {
            chunks.push(std::mem::take(&mut current));
            load = RouteLoad::default();
            next_load = load;
            next_load.add(RouteLoad::item(item_gas));
        }
        load = next_load;
        current.push(item);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

struct OriginLanes {
    aggregate: VecDeque<ReadyId>,
    individual: VecDeque<ReadyId>,
    prefer_aggregate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ReadyId(u64);

enum Entry {
    /// Resolved work that has not started.
    Queued(ExecutionJob),
    /// A started job asking for a read slot again. The job keeps its execution state; this entry
    /// only holds its place in fair scheduling under the current origins of the job's work.
    Resume {
        request: SlotRequest,
        origins: Vec<RpcOrigin>,
    },
}

pub(super) enum Claim {
    Queued(ExecutionJob),
    Resume(SlotRequest),
}

/// Owns work that has been resolved but is waiting for a read slot, and started jobs waiting to
/// resume reading.
///
/// `jobs` is the sole owner of these entries. Origin queues contain only generation references
/// into that map, so a deduplicated item attached to several origins is claimed once and leaves
/// stale references that can be discarded without consuming a read slot. References to entries
/// that cannot read yet stay in place so later eligible work in the same lane is not blocked.
pub(super) struct ReadyScheduler {
    jobs: HashMap<ReadyId, Entry>,
    key_index: HashMap<WorkKey, ReadyId>,
    lanes: HashMap<RpcOrigin, OriginLanes>,
    origins: VecDeque<RpcOrigin>,
    next_ready_id: u64,
}

impl ReadyScheduler {
    pub(super) fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            key_index: HashMap::new(),
            lanes: HashMap::new(),
            origins: VecDeque::new(),
            next_ready_id: 1,
        }
    }

    /// Queued work only; a resuming job wakes itself on its deadlines.
    pub(super) fn deadlines(&self, mut consider: impl FnMut(Option<Instant>)) {
        for entry in self.jobs.values() {
            let Entry::Queued(job) = entry else {
                continue;
            };
            for item in job.items() {
                consider(item.waiters.merged_deadline());
            }
        }
    }

    /// Chain routes of queued work for which `ready` fails, one per verification cache. Equal
    /// routes from separate configurations keep separate caches, so they are not merged.
    pub(super) fn unready_routes(
        &self,
        ready: impl Fn(&ExecutionJob) -> bool,
    ) -> Vec<RpcChainRoute> {
        let mut routes: Vec<RpcChainRoute> = Vec::new();
        for entry in self.jobs.values() {
            let Entry::Queued(job) = entry else {
                continue;
            };
            if ready(job) {
                continue;
            }
            let Some(item) = job.first() else {
                continue;
            };
            let route = item.execution_route.chain_route();
            if !routes.iter().any(|watched| {
                Arc::ptr_eq(&watched.verified_identities, &route.verified_identities)
            }) {
                routes.push(route.clone());
            }
        }
        routes
    }

    fn lane_mut(&mut self, origin: &RpcOrigin) -> &mut OriginLanes {
        if !self.lanes.contains_key(origin) {
            self.origins.push_back(origin.clone());
            self.lanes.insert(
                origin.clone(),
                OriginLanes {
                    aggregate: VecDeque::new(),
                    individual: VecDeque::new(),
                    prefer_aggregate: true,
                },
            );
        }
        self.lanes.get_mut(origin).expect("origin lane inserted")
    }

    /// Adds a deduplicated caller's origin to the queued or resuming entry that owns `key`.
    pub(super) fn attach(&mut self, key: &WorkKey, origin: &RpcOrigin) -> bool {
        let Some(&ready_id) = self.key_index.get(key) else {
            return false;
        };
        let (is_aggregate, already_eligible) = match self.jobs.get_mut(&ready_id) {
            Some(Entry::Queued(ExecutionJob::Aggregate(items))) => {
                let already_eligible = items.iter().any(|item| item.origins.contains(origin));
                let item = items
                    .iter_mut()
                    .find(|item| item.key == *key)
                    .expect("indexed aggregate member exists");
                item.origins.push(origin.clone());
                (true, already_eligible)
            }
            Some(Entry::Queued(ExecutionJob::Individual(item))) => {
                let already_eligible = item.origins.contains(origin);
                item.origins.push(origin.clone());
                (false, already_eligible)
            }
            Some(Entry::Resume { request, origins }) if !request.grant.is_closed() => {
                let already_eligible = origins.contains(origin);
                if !already_eligible {
                    origins.push(origin.clone());
                }
                (request.aggregate, already_eligible)
            }
            _ => {
                self.remove_index_if_current(key, ready_id);
                return false;
            }
        };
        if !already_eligible {
            let lane = self.lane_mut(origin);
            if is_aggregate {
                lane.aggregate.push_back(ready_id);
            } else {
                lane.individual.push_back(ready_id);
            }
        }
        true
    }

    const fn allocate_ready_id(&mut self) -> ReadyId {
        let id = ReadyId(self.next_ready_id);
        self.next_ready_id = self
            .next_ready_id
            .checked_add(1)
            .expect("RPC ready generation exhausted");
        id
    }

    fn remove_index_if_current(&mut self, key: &WorkKey, ready_id: ReadyId) {
        if self.key_index.get(key) == Some(&ready_id) {
            self.key_index.remove(key);
        }
    }

    fn job_has_origin(job: &ExecutionJob, origin: &RpcOrigin) -> bool {
        job.items().iter().any(|item| item.origins.contains(origin))
    }

    const fn job_class_matches(job: &ExecutionJob, aggregate: bool) -> bool {
        matches!(
            (aggregate, job),
            (true, ExecutionJob::Aggregate(_)) | (false, ExecutionJob::Individual(_))
        )
    }

    /// Whether `entry` still belongs in `origin`'s lane of the given class. A resuming job that
    /// stopped waiting no longer belongs anywhere.
    fn entry_belongs(entry: &Entry, origin: &RpcOrigin, aggregate: bool) -> bool {
        match entry {
            Entry::Queued(job) => {
                Self::job_class_matches(job, aggregate) && Self::job_has_origin(job, origin)
            }
            Entry::Resume { request, origins } => {
                request.aggregate == aggregate
                    && origins.contains(origin)
                    && !request.grant.is_closed()
            }
        }
    }

    /// Removes and returns the first reference that can read now, with its position. Stale
    /// references before it are discarded; blocked ones stay in place.
    fn take_eligible_ref(
        &self,
        refs: &mut VecDeque<ReadyId>,
        origin: &RpcOrigin,
        aggregate: bool,
        ready: &impl Fn(&ExecutionJob) -> bool,
    ) -> Option<(usize, ReadyId)> {
        let mut index = 0;
        while let Some(&ready_id) = refs.get(index) {
            match self
                .jobs
                .get(&ready_id)
                .filter(|entry| Self::entry_belongs(entry, origin, aggregate))
            {
                None => {
                    refs.remove(index);
                }
                Some(Entry::Queued(job)) if !ready(job) => index += 1,
                Some(_) => {
                    refs.remove(index);
                    return Some((index, ready_id));
                }
            }
        }
        None
    }

    /// Removes origin references to entries that have expired, been claimed, or stopped waiting.
    ///
    /// Each remaining reference must point at a live entry of the right class whose current
    /// members make that origin eligible. Empty lanes and their origin-queue entries are removed as
    /// well.
    fn prune_refs(&mut self) {
        let closed = self
            .jobs
            .iter()
            .filter(|(_, entry)| {
                matches!(entry, Entry::Resume { request, .. } if request.grant.is_closed())
            })
            .map(|(&ready_id, _)| ready_id)
            .collect::<Vec<_>>();
        for ready_id in closed {
            if let Some(Entry::Resume { request, .. }) = self.jobs.remove(&ready_id) {
                // The same work may have been readmitted under a newer entry.
                for key in &request.keys {
                    self.remove_index_if_current(key, ready_id);
                }
            }
        }
        let jobs = &self.jobs;
        self.lanes.retain(|origin, lane| {
            lane.aggregate.retain(|ready_id| {
                jobs.get(ready_id)
                    .is_some_and(|entry| Self::entry_belongs(entry, origin, true))
            });
            lane.individual.retain(|ready_id| {
                jobs.get(ready_id)
                    .is_some_and(|entry| Self::entry_belongs(entry, origin, false))
            });
            !lane.aggregate.is_empty() || !lane.individual.is_empty()
        });
        self.origins
            .retain(|origin| self.lanes.contains_key(origin));
    }

    pub(super) fn admit_chunks(&mut self, chunks: Vec<Vec<WorkItem>>) {
        for group in chunks {
            if group.is_empty() {
                continue;
            }
            let mut aggregate = Vec::new();
            let mut individual = Vec::new();
            for item in group {
                if item.execution_route.multicall().is_some() && item.read.is_multicall_eligible() {
                    aggregate.push(item);
                } else {
                    individual.push(item);
                }
            }
            if !aggregate.is_empty() {
                let mut origins: Vec<RpcOrigin> = Vec::new();
                for item in &aggregate {
                    for origin in &item.origins {
                        if !origins.contains(origin) {
                            origins.push(origin.clone());
                        }
                    }
                }
                let ready_id = self.allocate_ready_id();
                for item in &aggregate {
                    self.key_index.insert(item.key.clone(), ready_id);
                }
                self.jobs
                    .insert(ready_id, Entry::Queued(ExecutionJob::Aggregate(aggregate)));
                for origin in origins {
                    self.lane_mut(&origin).aggregate.push_back(ready_id);
                }
            }
            for item in individual {
                let key = item.key.clone();
                let mut origins: Vec<RpcOrigin> = Vec::new();
                for origin in &item.origins {
                    if !origins.contains(origin) {
                        origins.push(origin.clone());
                    }
                }
                let ready_id = self.allocate_ready_id();
                self.key_index.insert(key, ready_id);
                self.jobs.insert(
                    ready_id,
                    Entry::Queued(ExecutionJob::Individual(Box::new(item))),
                );
                for origin in origins {
                    self.lane_mut(&origin).individual.push_back(ready_id);
                }
            }
        }
    }

    /// Queues a started job's read request behind each of `origins`' existing work. Its keys
    /// index the entry so later deduplicated callers can attach their origins.
    pub(super) fn admit_resume(&mut self, request: SlotRequest, origins: Vec<RpcOrigin>) {
        let ready_id = self.allocate_ready_id();
        let aggregate = request.aggregate;
        for key in &request.keys {
            self.key_index.insert(key.clone(), ready_id);
        }
        self.jobs.insert(
            ready_id,
            Entry::Resume {
                request,
                origins: origins.clone(),
            },
        );
        for origin in origins {
            let lane = self.lane_mut(&origin);
            if aggregate {
                lane.aggregate.push_back(ready_id);
            } else {
                lane.individual.push_back(ready_id);
            }
        }
    }

    pub(super) fn partition_and_admit(&mut self, work: Vec<WorkItem>) {
        let mut groups: Vec<Vec<WorkItem>> = Vec::new();
        #[expect(
            clippy::mutable_key_type,
            reason = "route equality and hashing exclude the endpoint verification cache and the \
                      isolation client"
        )]
        let mut group_indices: HashMap<(RpcChainRoute, Option<BlockKey>), usize> = HashMap::new();
        for item in work {
            let group_key = (
                item.key.route.clone(),
                item.read.reuse_block_id().map(BlockKey),
            );
            let group_index = *group_indices.entry(group_key).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[group_index].push(item);
        }
        let chunks = groups
            .into_iter()
            .flat_map(|group| partition_work(group).into_iter())
            .collect();
        self.admit_chunks(chunks);
    }

    pub(super) fn expire(
        &mut self,
        now: Instant,
    ) -> Vec<(WorkKey, Result<RpcResult, RpcBrokerError>)> {
        let mut expired = Vec::new();
        let ids = self.jobs.keys().copied().collect::<Vec<_>>();
        for ready_id in ids {
            // A resuming job keeps its execution state and settles its own expiry.
            let job = match self.jobs.remove(&ready_id) {
                Some(Entry::Queued(job)) => job,
                Some(entry) => {
                    self.jobs.insert(ready_id, entry);
                    continue;
                }
                None => continue,
            };
            match job {
                ExecutionJob::Aggregate(items) => {
                    let mut survivors = Vec::with_capacity(items.len());
                    for item in items {
                        if item
                            .waiters
                            .merged_deadline()
                            .is_some_and(|deadline| deadline <= now)
                        {
                            self.remove_index_if_current(&item.key, ready_id);
                            expired.push((item.key, Err(RpcBrokerError::TimeoutBeforeDispatch)));
                        } else {
                            survivors.push(item);
                        }
                    }
                    if survivors.is_empty() {
                        continue;
                    }
                    self.jobs
                        .insert(ready_id, Entry::Queued(ExecutionJob::Aggregate(survivors)));
                }
                ExecutionJob::Individual(item) => {
                    if item
                        .waiters
                        .merged_deadline()
                        .is_some_and(|deadline| deadline <= now)
                    {
                        self.remove_index_if_current(&item.key, ready_id);
                        expired.push((item.key, Err(RpcBrokerError::TimeoutBeforeDispatch)));
                    } else {
                        self.jobs
                            .insert(ready_id, Entry::Queued(ExecutionJob::Individual(item)));
                    }
                }
            }
        }
        self.prune_refs();
        expired
    }

    /// Claims the next entry that can use a read slot: queued work for which `ready` holds, or a
    /// resuming job. Origins rotate round-robin and each origin alternates aggregate and
    /// individual work among eligible entries. One call visits each current origin at most once.
    pub(super) fn next(&mut self, ready: impl Fn(&ExecutionJob) -> bool) -> Option<Claim> {
        for _ in 0..self.origins.len() {
            let Some(origin) = self.origins.pop_front() else {
                break;
            };
            let Some(mut lane) = self.lanes.remove(&origin) else {
                continue;
            };
            let aggregate_ref = self.take_eligible_ref(&mut lane.aggregate, &origin, true, &ready);
            let individual_ref =
                self.take_eligible_ref(&mut lane.individual, &origin, false, &ready);
            let ready_id = match (aggregate_ref, individual_ref) {
                (None, None) => None,
                (Some((_, aggregate_ref)), None) => {
                    lane.prefer_aggregate = false;
                    Some(aggregate_ref)
                }
                (None, Some((_, individual_ref))) => {
                    lane.prefer_aggregate = true;
                    Some(individual_ref)
                }
                (
                    Some((aggregate_index, aggregate_ref)),
                    Some((individual_index, individual_ref)),
                ) => {
                    let choose_aggregate = lane.prefer_aggregate;
                    lane.prefer_aggregate = !lane.prefer_aggregate;
                    if choose_aggregate {
                        lane.individual.insert(individual_index, individual_ref);
                        Some(aggregate_ref)
                    } else {
                        lane.aggregate.insert(aggregate_index, aggregate_ref);
                        Some(individual_ref)
                    }
                }
            };
            if !lane.aggregate.is_empty() || !lane.individual.is_empty() {
                self.origins.push_back(origin.clone());
                self.lanes.insert(origin, lane);
            }
            let Some(ready_id) = ready_id else {
                continue;
            };
            let claim = self.claim(ready_id);
            self.prune_refs();
            if claim.is_some() {
                return claim;
            }
        }
        None
    }

    /// Claims the oldest queued job for which `ready` fails, to start it with a probe runner and
    /// no read slot. Origin rotation is unaffected because no read slot is consumed.
    pub(super) fn next_verification(
        &mut self,
        ready: impl Fn(&ExecutionJob) -> bool,
    ) -> Option<ExecutionJob> {
        let ready_id = self
            .jobs
            .iter()
            .filter_map(|(ready_id, entry)| match entry {
                Entry::Queued(job) if !ready(job) => Some(*ready_id),
                _ => None,
            })
            .min_by_key(|ready_id| ready_id.0)?;
        let claim = self.claim(ready_id);
        self.prune_refs();
        match claim {
            Some(Claim::Queued(job)) => Some(job),
            _ => None,
        }
    }

    fn claim(&mut self, ready_id: ReadyId) -> Option<Claim> {
        match self.jobs.remove(&ready_id)? {
            Entry::Queued(job) => {
                for item in job.items() {
                    self.remove_index_if_current(&item.key, ready_id);
                }
                Some(Claim::Queued(job))
            }
            Entry::Resume { request, .. } => {
                for key in &request.keys {
                    self.remove_index_if_current(key, ready_id);
                }
                Some(Claim::Resume(request))
            }
        }
    }

    pub(super) fn drain(
        &mut self,
        error: &RpcBrokerError,
    ) -> Vec<(WorkKey, Result<RpcResult, RpcBrokerError>)> {
        let mut completions = Vec::new();
        // Dropping a resuming job's request fails its grant, and the job settles its own work.
        for entry in self.jobs.drain().map(|(_, entry)| entry) {
            if let Entry::Queued(job) = entry {
                completions.extend(job.into_iter().map(|item| (item.key, Err(error.clone()))));
            }
        }
        self.key_index.clear();
        self.lanes.clear();
        self.origins.clear();
        completions
    }
}
