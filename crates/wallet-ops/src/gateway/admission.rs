use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use tokio::time::Instant;

use crate::rpc_broker::RpcOrigin;

const ACTIVE_LIMIT: usize = 32;
const QUEUE_LIMIT: usize = 64;
const TOKEN_UNITS: u64 = 1_000_000_000;
const BUCKET_CAPACITY: u64 = 64 * TOKEN_UNITS;
const REFILL_PER_NANOSECOND: u64 = 32;
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub(super) struct ReadTicket {
    pub(super) id: u64,
    pub(super) deadline: Instant,
}

#[derive(Debug)]
pub(super) enum ReadAdmissionDecision {
    Ready(ReadTicket),
    Queued(ReadTicket),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadLimit {
    Deadline,
    IdExhausted,
    RateLimited { active: usize, queued: usize },
    QueueFull { active: usize, queued: usize },
}

#[derive(Debug, Default)]
pub(super) struct ReadAdmissionUpdates {
    pub(super) ready: Vec<ReadTicket>,
    pub(super) expired: Vec<ReadTicket>,
}

struct OriginState {
    active: usize,
    queued: VecDeque<ReadTicket>,
    tokens: u64,
    refilled_at: Instant,
}

impl OriginState {
    const fn new(now: Instant) -> Self {
        Self {
            active: 0,
            queued: VecDeque::new(),
            tokens: BUCKET_CAPACITY,
            refilled_at: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        // Two seconds fill even an empty bucket; cap before converting or multiplying.
        let elapsed = now
            .saturating_duration_since(self.refilled_at)
            .min(Duration::from_secs(2));
        let nanos = u64::from(elapsed.subsec_nanos()) + elapsed.as_secs() * TOKEN_UNITS;
        self.tokens = (self.tokens + nanos * REFILL_PER_NANOSECOND).min(BUCKET_CAPACITY);
        self.refilled_at = self.refilled_at.max(now);
    }

    fn expire_queued(&mut self, now: Instant, expired: &mut Vec<ReadTicket>) {
        self.queued.retain(|ticket| {
            if ticket.deadline <= now {
                expired.push(ticket.clone());
                false
            } else {
                true
            }
        });
    }
}

/// Admission accounting survives request-owner invalidation. The caller must drain every
/// accepted broker future before completing its active ticket, even after failing its promise.
#[derive(Default)]
pub(super) struct ReadAdmission {
    origins: HashMap<RpcOrigin, OriginState>,
    active: HashMap<u64, RpcOrigin>,
    next_id: u64,
}

impl ReadAdmission {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn admit(
        &mut self,
        origin: RpcOrigin,
        now: Instant,
    ) -> Result<ReadAdmissionDecision, ReadLimit> {
        let deadline = now.checked_add(READ_TIMEOUT).ok_or(ReadLimit::Deadline)?;
        self.admit_with_deadline(origin, now, deadline)
    }

    pub(super) fn admit_with_deadline(
        &mut self,
        origin: RpcOrigin,
        now: Instant,
        deadline: Instant,
    ) -> Result<ReadAdmissionDecision, ReadLimit> {
        if now >= deadline {
            return Err(ReadLimit::Deadline);
        }
        let next_id = self.next_id.checked_add(1).ok_or(ReadLimit::IdExhausted)?;
        let ticket = ReadTicket {
            id: self.next_id,
            deadline,
        };
        if origin.web_origin().is_none() {
            // Wallet reads retain unique tickets for owner/deadline checks and job drain.
            // The shared broker owns their admission; dapp budgets are unaffected.
            self.next_id = next_id;
            return Ok(ReadAdmissionDecision::Ready(ticket));
        }
        // Removing a full, idle bucket preserves exactly the allowance of a fresh bucket.
        self.origins.retain(|_, state| {
            state.refill(now);
            state.active != 0 || !state.queued.is_empty() || state.tokens != BUCKET_CAPACITY
        });
        let state = self
            .origins
            .entry(origin.clone())
            .or_insert_with(|| OriginState::new(now));
        if state.tokens < TOKEN_UNITS {
            return Err(ReadLimit::RateLimited {
                active: state.active,
                queued: state.queued.len(),
            });
        }
        if state.active >= ACTIVE_LIMIT && state.queued.len() >= QUEUE_LIMIT {
            return Err(ReadLimit::QueueFull {
                active: state.active,
                queued: state.queued.len(),
            });
        }
        self.next_id = next_id;
        state.tokens -= TOKEN_UNITS;
        if state.active < ACTIVE_LIMIT {
            state.active += 1;
            self.active.insert(ticket.id, origin);
            Ok(ReadAdmissionDecision::Ready(ticket))
        } else {
            state.queued.push_back(ticket.clone());
            Ok(ReadAdmissionDecision::Queued(ticket))
        }
    }

    pub(super) fn complete(&mut self, id: u64, now: Instant) -> ReadAdmissionUpdates {
        let mut updates = ReadAdmissionUpdates::default();
        let Some(origin) = self.active.remove(&id) else {
            return updates;
        };
        let state = self.origins.get_mut(&origin).expect("active origin exists");
        state.active -= 1;
        state.expire_queued(now, &mut updates.expired);
        while state.active < ACTIVE_LIMIT {
            let Some(ticket) = state.queued.pop_front() else {
                break;
            };
            state.active += 1;
            self.active.insert(ticket.id, origin.clone());
            updates.ready.push(ticket);
        }
        updates
    }

    /// Cancellation retires only queued tickets; active charges belong to their completion.
    pub(super) fn cancel_queued(&mut self, id: u64) -> bool {
        for state in self.origins.values_mut() {
            if let Some(index) = state.queued.iter().position(|ticket| ticket.id == id) {
                state.queued.remove(index);
                return true;
            }
        }
        false
    }

    pub(super) fn expire_queued(&mut self, now: Instant) -> Vec<ReadTicket> {
        let mut expired = Vec::new();
        for state in self.origins.values_mut() {
            state.expire_queued(now, &mut expired);
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(url: &str) -> RpcOrigin {
        RpcOrigin::dapp("paired-peer", url).unwrap()
    }

    fn ready(result: Result<ReadAdmissionDecision, ReadLimit>) -> ReadTicket {
        let ReadAdmissionDecision::Ready(ticket) = result.unwrap() else {
            panic!("expected ready read");
        };
        ticket
    }

    fn queued(result: Result<ReadAdmissionDecision, ReadLimit>) -> ReadTicket {
        let ReadAdmissionDecision::Queued(ticket) = result.unwrap() else {
            panic!("expected queued read");
        };
        ticket
    }

    #[test]
    fn saturated_origin_preserves_fifo_deadlines_and_other_origin_progress() {
        let mut admission = ReadAdmission::new();
        let origin = origin("https://dapp.invalid/path?query#fragment");
        let now = Instant::now();
        let active: Vec<_> = (0..32)
            .map(|_| ready(admission.admit(origin.clone(), now)))
            .collect();
        let first_queue: Vec<_> = (0..32)
            .map(|_| queued(admission.admit(origin.clone(), now)))
            .collect();
        assert!(admission.admit(origin.clone(), now).is_err());
        let later = now + Duration::from_secs(1);
        for _ in 0..32 {
            queued(admission.admit(origin.clone(), later));
        }
        let completion_time = later + Duration::from_secs(1);
        assert!(admission.admit(origin, completion_time).is_err());
        ready(admission.admit(
            RpcOrigin::dapp("paired-peer", "https://dapp.invalid/other").unwrap(),
            completion_time,
        ));

        let updates = admission.complete(active[0].id, completion_time);
        assert!(updates.expired.is_empty());
        assert_eq!(updates.ready.len(), 1);
        assert_eq!(updates.ready[0].id, first_queue[0].id);
        assert_eq!(updates.ready[0].deadline, now + Duration::from_secs(30));
        assert!(
            admission
                .complete(active[0].id, completion_time)
                .ready
                .is_empty()
        );

        let updates = admission.complete(active[1].id, now + Duration::from_secs(30));
        assert_eq!(updates.expired.len(), 31);
        assert_eq!(updates.ready.len(), 1);
        assert_eq!(updates.ready[0].deadline, later + Duration::from_secs(30));
        assert_eq!(
            admission
                .expire_queued(later + Duration::from_secs(30))
                .len(),
            31
        );
        assert!(
            admission
                .complete(active[2].id, later + Duration::from_secs(30))
                .ready
                .is_empty()
        );
    }

    #[test]
    fn reconnect_cancellation_retains_active_charges_and_spent_tokens() {
        let mut admission = ReadAdmission::new();
        let origin = origin("https://dapp.invalid");
        let now = Instant::now();
        let active: Vec<_> = (0..32)
            .map(|_| ready(admission.admit(origin.clone(), now)))
            .collect();
        let pending: Vec<_> = (0..32)
            .map(|_| queued(admission.admit(origin.clone(), now)))
            .collect();
        for ticket in pending {
            assert!(admission.cancel_queued(ticket.id));
        }
        assert!(!admission.cancel_queued(active[0].id));
        assert!(admission.admit(origin.clone(), now).is_err());
        let refilled = now + Duration::from_millis(32);
        let reconnect = queued(admission.admit(origin.clone(), refilled));
        assert!(admission.admit(origin, refilled).is_err());
        let updates = admission.complete(active[0].id, refilled);
        assert_eq!(updates.ready.len(), 1);
        assert_eq!(updates.ready[0].id, reconnect.id);

        // Local answers complete immediately but still spend the origin's rate allowance.
        let local_origin = RpcOrigin::dapp("other-peer", "https://dapp.invalid").unwrap();
        for _ in 0..64 {
            let ticket = ready(admission.admit(local_origin.clone(), refilled));
            admission.complete(ticket.id, refilled);
        }
        assert!(admission.admit(local_origin.clone(), refilled).is_err());
        ready(admission.admit(local_origin, refilled + Duration::from_secs(2)));
    }
}
