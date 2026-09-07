use super::cache::LatestEpoch;
use super::model::RpcResult;
use super::model::{ReadIdentity, RpcBrokerError, RpcChainRoute, RpcOrigin, RpcRead, RpcRoute};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio::time::Instant;

/// Owns a caller reply sender and the admission credit for its submission.
///
/// The credit is shared by all members of one submission and is retained until every reply
/// owner has been retired by the actor. This keeps caller cancellation from releasing work that
/// remains registered in the broker.
pub(super) struct ReadReply {
    sender: oneshot::Sender<Result<RpcResult, RpcBrokerError>>,
    _permit: Arc<OwnedSemaphorePermit>,
}

impl ReadReply {
    pub(super) const fn new(
        sender: oneshot::Sender<Result<RpcResult, RpcBrokerError>>,
        permit: Arc<OwnedSemaphorePermit>,
    ) -> Self {
        Self {
            sender,
            _permit: permit,
        }
    }

    /// Delivers the result to the caller. A dropped receiver is a cancelled caller, not an error.
    pub(super) fn send(self, result: Result<RpcResult, RpcBrokerError>) {
        let _ = self.sender.send(result);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct WorkKey {
    pub(super) identity: ReadIdentity,
    pub(super) route: RpcChainRoute,
    pub(super) nonce: u64,
    pub(super) latest_epoch: Option<LatestEpoch>,
}

pub(super) struct WorkItem {
    pub(super) key: WorkKey,
    pub(super) execution_route: RpcRoute,
    pub(super) read: RpcRead,
    pub(super) origins: Vec<RpcOrigin>,
    pub(super) waiters: Arc<WaiterState>,
}

#[derive(Clone, Copy)]
pub(super) struct WaiterPolicy {
    pub(super) deadline: Option<Instant>,
    pub(super) attempt_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WaiterSnapshot {
    pub(super) has_live: bool,
    pub(super) deadline: Option<Instant>,
    pub(super) attempt_timeout: Duration,
}

/// Clone-shared policy for one logical read. A detached job consults this before every future
/// physical attempt, while the current HTTP attempt continues with the cap selected at dispatch.
pub(super) struct WaiterState {
    registrations: Mutex<Vec<WaiterPolicy>>,
}

impl WaiterState {
    pub(super) fn new(policy: WaiterPolicy) -> Arc<Self> {
        Arc::new(Self {
            registrations: Mutex::new(vec![policy]),
        })
    }

    pub(super) fn add(&self, policy: WaiterPolicy) {
        self.registrations
            .lock()
            .expect("waiter state lock poisoned")
            .push(policy);
    }

    pub(super) fn snapshot(&self, now: Instant) -> WaiterSnapshot {
        let registrations = self
            .registrations
            .lock()
            .expect("waiter state lock poisoned");
        let mut latest: Option<Instant> = None;
        let mut unbounded = false;
        let mut shortest = None;
        for registration in registrations
            .iter()
            .filter(|registration| registration.deadline.is_none_or(|deadline| deadline > now))
        {
            if let Some(candidate) = registration.deadline {
                latest = Some(latest.map_or(candidate, |current| current.max(candidate)));
            } else {
                unbounded = true;
            }
            shortest = Some(
                shortest.map_or(registration.attempt_timeout, |current: Duration| {
                    current.min(registration.attempt_timeout)
                }),
            );
        }
        WaiterSnapshot {
            has_live: shortest.is_some(),
            deadline: (!unbounded).then_some(latest).flatten(),
            attempt_timeout: shortest.unwrap_or_default(),
        }
    }

    /// Returns the deadline represented by all registrations, including expired ones. This keeps
    /// an all-expired bounded set distinct from a set containing an unbounded waiter.
    pub(super) fn merged_deadline(&self) -> Option<Instant> {
        let registrations = self
            .registrations
            .lock()
            .expect("waiter state lock poisoned");
        let mut latest = None;
        for registration in registrations.iter() {
            let deadline = registration.deadline?;
            latest = Some(latest.map_or(deadline, |current: Instant| current.max(deadline)));
        }
        latest
    }
}

pub(super) struct ActiveWork {
    pub(super) read: RpcRead,
    pub(super) execution_route: RpcRoute,
    pub(super) origins: Vec<RpcOrigin>,
    pub(super) replies: Vec<ReadReply>,
    pub(super) waiters: Arc<WaiterState>,
}
