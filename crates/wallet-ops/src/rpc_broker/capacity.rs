//! Read-slot and probe-runner accounting shared between the actor and its running jobs.
//!
//! The actor grants read slots through fair scheduling and probe runners in FIFO order. A job
//! releases either slot itself while it keeps running, and dropping the job releases whatever it
//! still holds.

use super::model::RpcClients;
use super::resolution::WorkKey;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};

pub(super) const DEFAULT_MAX_IDENTITY_PROBES: usize = 8;

/// Broker-wide resources handed to every job.
#[derive(Clone)]
pub(super) struct JobResources {
    pub(super) clients: RpcClients,
    /// Bounds physical read requests independently of identity probes.
    pub(super) http: Arc<Semaphore>,
    /// Bounds identity probes across all jobs and background warmup.
    pub(super) probes: Arc<Semaphore>,
    /// Concurrent probes one job may drive.
    pub(super) probe_limit: usize,
}

impl JobResources {
    pub(super) fn new(clients: RpcClients, max_in_flight: usize) -> Self {
        let max_in_flight = max_in_flight.max(1);
        Self {
            clients,
            http: Arc::new(Semaphore::new(max_in_flight)),
            probes: Arc::new(Semaphore::new(DEFAULT_MAX_IDENTITY_PROBES)),
            probe_limit: DEFAULT_MAX_IDENTITY_PROBES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlotKind {
    /// Executes requested reads.
    Read,
    /// Drives the job's identity probes.
    Runner,
}

/// Counts of jobs holding each slot kind. Jobs release slots from their own tasks, so the counts
/// are atomic and every release wakes the actor.
pub(super) struct CapacityLedger {
    reads: AtomicUsize,
    runners: AtomicUsize,
    changed: Notify,
}

impl CapacityLedger {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            reads: AtomicUsize::new(0),
            runners: AtomicUsize::new(0),
            changed: Notify::new(),
        })
    }

    pub(super) fn held(&self, kind: SlotKind) -> usize {
        self.counter(kind).load(Ordering::Acquire)
    }

    /// Completes after a slot release or an endpoint verified by a broker job since the previous
    /// wake. The actor watches checks made outside the broker separately.
    pub(super) async fn changed(&self) {
        self.changed.notified().await;
    }

    const fn counter(&self, kind: SlotKind) -> &AtomicUsize {
        match kind {
            SlotKind::Read => &self.reads,
            SlotKind::Runner => &self.runners,
        }
    }

    fn wake(&self) {
        self.changed.notify_one();
    }
}

/// The slots one job holds. Taking and releasing are idempotent, and dropping the last reference
/// releases both.
pub(super) struct JobSlots {
    ledger: Arc<CapacityLedger>,
    read: AtomicBool,
    runner: AtomicBool,
}

impl JobSlots {
    pub(super) fn new(ledger: Arc<CapacityLedger>) -> Arc<Self> {
        Arc::new(Self {
            ledger,
            read: AtomicBool::new(false),
            runner: AtomicBool::new(false),
        })
    }

    const fn flag(&self, kind: SlotKind) -> &AtomicBool {
        match kind {
            SlotKind::Read => &self.read,
            SlotKind::Runner => &self.runner,
        }
    }

    fn holds(&self, kind: SlotKind) -> bool {
        self.flag(kind).load(Ordering::Acquire)
    }

    /// Takes a slot. The actor checks the bound first.
    pub(super) fn acquire(&self, kind: SlotKind) {
        if !self.flag(kind).swap(true, Ordering::AcqRel) {
            self.ledger.counter(kind).fetch_add(1, Ordering::AcqRel);
        }
    }

    fn release(&self, kind: SlotKind) {
        if self.flag(kind).swap(false, Ordering::AcqRel) {
            self.ledger.counter(kind).fetch_sub(1, Ordering::AcqRel);
            self.ledger.wake();
        }
    }
}

impl Drop for JobSlots {
    fn drop(&mut self) {
        self.release(SlotKind::Read);
        self.release(SlotKind::Runner);
    }
}

/// A running job asking the actor for a slot. A queued request does not keep a finished job's
/// slots alive.
pub(super) struct SlotRequest {
    pub(super) kind: SlotKind,
    pub(super) slots: Weak<JobSlots>,
    pub(super) grant: oneshot::Sender<()>,
    /// Scheduling class of a read request's continuation, and the job's work, whose current
    /// origins the actor queues it under.
    pub(super) aggregate: bool,
    pub(super) keys: Vec<WorkKey>,
}

impl SlotRequest {
    /// Hands the slot to the job unless it has stopped waiting.
    pub(super) fn grant(self) {
        if self.grant.is_closed() {
            return;
        }
        let Some(slots) = self.slots.upgrade() else {
            return;
        };
        slots.acquire(self.kind);
        if self.grant.send(()).is_err() {
            // The job dropped its receiver after the check and does not treat the slot as held.
            slots.release(self.kind);
        }
    }
}

/// A job's handle for releasing and requesting its slots.
pub(super) struct JobControl {
    slots: Arc<JobSlots>,
    /// `None` grants every request immediately, for jobs run without an actor in tests.
    requests: Option<mpsc::UnboundedSender<SlotRequest>>,
    aggregate: bool,
    keys: Vec<WorkKey>,
}

impl JobControl {
    pub(super) const fn new(
        slots: Arc<JobSlots>,
        requests: mpsc::UnboundedSender<SlotRequest>,
        aggregate: bool,
        keys: Vec<WorkKey>,
    ) -> Self {
        Self {
            slots,
            requests: Some(requests),
            aggregate,
            keys,
        }
    }

    /// Holds both slots and grants every later request at once.
    #[cfg(test)]
    pub(super) fn granted() -> Self {
        let slots = JobSlots::new(CapacityLedger::new());
        slots.acquire(SlotKind::Read);
        slots.acquire(SlotKind::Runner);
        Self {
            slots,
            requests: None,
            aggregate: false,
            keys: Vec::new(),
        }
    }

    pub(super) fn holds(&self, kind: SlotKind) -> bool {
        self.slots.holds(kind)
    }

    pub(super) fn release(&self, kind: SlotKind) {
        self.slots.release(kind);
    }

    /// Queues a request for `kind`. The receiver completes once the slot is held and fails if the
    /// actor drops the request; `None` means the actor no longer accepts requests.
    pub(super) fn request(&self, kind: SlotKind) -> Option<oneshot::Receiver<()>> {
        let (grant, granted) = oneshot::channel();
        let request = SlotRequest {
            kind,
            slots: Arc::downgrade(&self.slots),
            grant,
            aggregate: self.aggregate,
            keys: self.keys.clone(),
        };
        match &self.requests {
            Some(requests) => requests.send(request).ok()?,
            None => request.grant(),
        }
        Some(granted)
    }

    /// Wakes the actor so queued jobs on the same route can become read-ready.
    pub(super) fn notify_verified(&self) {
        self.slots.ledger.wake();
    }
}
