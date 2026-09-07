use super::model::RpcResult;
use super::model::{BlockKey, RpcBrokerError, RpcChainRoute, RpcOrigin, RpcRoute, RpcSubmission};
use super::resolution::{WorkItem, WorkKey};
use std::collections::{HashMap, VecDeque};
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

/// Owns work that has been resolved but is waiting for an execution slot.
///
/// `jobs` is the sole owner of executable work. Origin queues contain only generation references
/// into that map, so a deduplicated item attached to several origins is claimed once and leaves
/// stale references that can be discarded without consuming an execution slot.
pub(super) struct ReadyScheduler {
    jobs: HashMap<ReadyId, ExecutionJob>,
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

    pub(super) fn deadlines(&self, mut consider: impl FnMut(Option<Instant>)) {
        for job in self.jobs.values() {
            for item in job.items() {
                consider(item.waiters.merged_deadline());
            }
        }
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

    pub(super) fn attach(&mut self, key: &WorkKey, origin: &RpcOrigin) -> bool {
        let Some(&ready_id) = self.key_index.get(key) else {
            return false;
        };
        let Some(job) = self.jobs.get_mut(&ready_id) else {
            self.remove_index_if_current(key, ready_id);
            return false;
        };
        let (is_aggregate, already_eligible) = match job {
            ExecutionJob::Aggregate(items) => {
                let already_eligible = items.iter().any(|item| item.origins.contains(origin));
                let item = items
                    .iter_mut()
                    .find(|item| item.key == *key)
                    .expect("indexed aggregate member exists");
                item.origins.push(origin.clone());
                (true, already_eligible)
            }
            ExecutionJob::Individual(item) => {
                let already_eligible = item.origins.contains(origin);
                item.origins.push(origin.clone());
                (false, already_eligible)
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

    fn pop_valid_ref(
        &self,
        refs: &mut VecDeque<ReadyId>,
        origin: &RpcOrigin,
        aggregate: bool,
    ) -> Option<ReadyId> {
        while let Some(ready_id) = refs.pop_front() {
            if self.jobs.get(&ready_id).is_some_and(|job| {
                Self::job_class_matches(job, aggregate) && Self::job_has_origin(job, origin)
            }) {
                return Some(ready_id);
            }
        }
        None
    }

    /// Removes origin references to jobs that have expired or been claimed.
    ///
    /// Each remaining reference must point at a live job of the right class whose current members
    /// make that origin eligible. Empty lanes and their origin-queue entries are removed as well.
    fn prune_refs(&mut self) {
        let jobs = &self.jobs;
        self.lanes.retain(|origin, lane| {
            lane.aggregate.retain(|ready_id| {
                jobs.get(ready_id).is_some_and(|job| {
                    Self::job_class_matches(job, true) && Self::job_has_origin(job, origin)
                })
            });
            lane.individual.retain(|ready_id| {
                jobs.get(ready_id).is_some_and(|job| {
                    Self::job_class_matches(job, false) && Self::job_has_origin(job, origin)
                })
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
                    .insert(ready_id, ExecutionJob::Aggregate(aggregate));
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
                self.jobs
                    .insert(ready_id, ExecutionJob::Individual(Box::new(item)));
                for origin in origins {
                    self.lane_mut(&origin).individual.push_back(ready_id);
                }
            }
        }
    }

    pub(super) fn partition_and_admit(&mut self, work: Vec<WorkItem>) {
        let mut groups: Vec<Vec<WorkItem>> = Vec::new();
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
            let Some(job) = self.jobs.remove(&ready_id) else {
                continue;
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
                        .insert(ready_id, ExecutionJob::Aggregate(survivors));
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
                        self.jobs.insert(ready_id, ExecutionJob::Individual(item));
                    }
                }
            }
        }
        self.prune_refs();
        expired
    }

    pub(super) fn next(&mut self, jobs: usize, max_in_flight: usize) -> Option<ExecutionJob> {
        if jobs >= max_in_flight {
            return None;
        }
        while let Some(origin) = self.origins.pop_front() {
            let Some(mut lane) = self.lanes.remove(&origin) else {
                continue;
            };
            let aggregate_ref = self.pop_valid_ref(&mut lane.aggregate, &origin, true);
            let individual_ref = self.pop_valid_ref(&mut lane.individual, &origin, false);
            let ready_id = match (aggregate_ref, individual_ref) {
                (None, None) => {
                    continue;
                }
                (Some(aggregate_ref), None) => {
                    lane.prefer_aggregate = false;
                    aggregate_ref
                }
                (None, Some(individual_ref)) => {
                    lane.prefer_aggregate = true;
                    individual_ref
                }
                (Some(aggregate_ref), Some(individual_ref)) => {
                    let choose_aggregate = lane.prefer_aggregate;
                    lane.prefer_aggregate = !lane.prefer_aggregate;
                    if choose_aggregate {
                        lane.individual.push_front(individual_ref);
                        aggregate_ref
                    } else {
                        lane.aggregate.push_front(aggregate_ref);
                        individual_ref
                    }
                }
            };
            let job = self.claim(ready_id);
            if !lane.aggregate.is_empty() || !lane.individual.is_empty() {
                self.origins.push_back(origin.clone());
                self.lanes.insert(origin, lane);
            }
            self.prune_refs();
            if let Some(job) = job {
                return Some(job);
            }
        }
        None
    }

    fn claim(&mut self, ready_id: ReadyId) -> Option<ExecutionJob> {
        let job = self.jobs.remove(&ready_id)?;
        for item in job.items() {
            self.remove_index_if_current(&item.key, ready_id);
        }
        Some(job)
    }

    pub(super) fn drain(
        &mut self,
        error: &RpcBrokerError,
    ) -> Vec<(WorkKey, Result<RpcResult, RpcBrokerError>)> {
        let mut completions = Vec::new();
        for job in self.jobs.drain().map(|(_, job)| job) {
            completions.extend(job.into_iter().map(|item| (item.key, Err(error.clone()))));
        }
        self.key_index.clear();
        self.lanes.clear();
        self.origins.clear();
        completions
    }
}
