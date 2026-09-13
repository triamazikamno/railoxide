use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::PeerId;

const LIFETIME: Duration = Duration::from_mins(5);
const RATE_WINDOW: Duration = Duration::from_mins(1);

/// Transient input. Deliberately has no Debug, Clone, or Serialize implementation.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct GatewayUnlockSecret(String);

impl GatewayUnlockSecret {
    #[must_use]
    pub fn into_value(mut self) -> Zeroizing<String> {
        Zeroizing::new(std::mem::take(&mut self.0))
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayUnlockCommand {
    Password { password: GatewayUnlockSecret },
    Passphrase { passphrase: GatewayUnlockSecret },
    Standard,
    Retry,
    Desktop,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayUnlockPhase {
    Password,
    Busy,
    Opening,
    Passphrase,
    Unknown,
    Desktop,
    Complete,
    Failed,
    Unavailable,
    Cancelled,
    RateLimited,
}

impl GatewayUnlockPhase {
    const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Desktop
                | Self::Complete
                | Self::Failed
                | Self::Unavailable
                | Self::Cancelled
                | Self::RateLimited
        )
    }
}

/// One UI's unlock lifetime. The desktop retains this only while it owns pending vault state.
pub struct GatewayUnlockAttempt {
    id: String,
    deadline: Instant,
    phase: Mutex<GatewayUnlockPhase>,
}

impl GatewayUnlockAttempt {
    #[must_use]
    pub fn is_current(&self) -> bool {
        Instant::now() < self.deadline
            && !self
                .phase
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .terminal()
    }

    pub fn cancel(&self) {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !phase.terminal() {
            *phase = GatewayUnlockPhase::Cancelled;
        }
    }

    pub fn finish(&self, next: GatewayUnlockPhase) {
        let _ = self.finish_if_current(next);
    }

    /// Atomically claims a transition against cancellation and expiry.
    #[must_use]
    pub fn finish_if_current(&self, next: GatewayUnlockPhase) -> bool {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !phase.terminal() && Instant::now() < self.deadline {
            *phase = next;
            true
        } else {
            false
        }
    }

    fn phase(&self) -> GatewayUnlockPhase {
        if Instant::now() >= self.deadline {
            self.cancel();
        }
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Holds global derivation admission until native work has actually completed, even after cancellation.
pub struct GatewayUnlockGuard {
    attempt: Arc<GatewayUnlockAttempt>,
    _permit: OwnedSemaphorePermit,
}

impl GatewayUnlockGuard {
    #[must_use]
    pub fn attempt(&self) -> Arc<GatewayUnlockAttempt> {
        Arc::clone(&self.attempt)
    }

    #[must_use]
    pub fn is_current(&self) -> bool {
        self.attempt.is_current()
    }

    pub fn finish(&self, phase: GatewayUnlockPhase) {
        self.attempt.finish(phase);
    }
}

impl Drop for GatewayUnlockGuard {
    fn drop(&mut self) {
        if self.attempt.phase() == GatewayUnlockPhase::Busy {
            self.attempt.finish(GatewayUnlockPhase::Failed);
        }
    }
}

/// A single-consumer command transported through the desktop event channel without copying secrets.
pub struct GatewayUnlockRequest {
    command: Mutex<Option<(GatewayUnlockCommand, GatewayUnlockGuard)>>,
}

impl GatewayUnlockRequest {
    pub fn take(&self) -> Option<(GatewayUnlockCommand, GatewayUnlockGuard)> {
        self.command
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .filter(|(_, guard)| guard.is_current())
    }
}

#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GatewayUnlockView {
    allowed: bool,
    attempt_id: Option<String>,
    phase: GatewayUnlockPhase,
}

struct Active {
    session: u64,
    peer: PeerId,
    attempt: Arc<GatewayUnlockAttempt>,
}

pub(super) struct Unlocks {
    active: Option<Active>,
    admission: Arc<Semaphore>,
    attempts: HashMap<PeerId, VecDeque<Instant>>,
    global_attempts: VecDeque<Instant>,
    pub(super) sent: HashMap<u64, GatewayUnlockView>,
}

impl Default for Unlocks {
    fn default() -> Self {
        Self {
            active: None,
            admission: Arc::new(Semaphore::new(1)),
            attempts: HashMap::new(),
            global_attempts: VecDeque::new(),
            sent: HashMap::new(),
        }
    }
}

impl GatewayUnlockView {
    pub(super) fn response(self, id: &str) -> Self {
        if self.attempt_id.as_deref() == Some(id) {
            self
        } else {
            Self {
                allowed: self.allowed,
                attempt_id: Some(id.to_owned()),
                phase: GatewayUnlockPhase::Unavailable,
            }
        }
    }
}

impl Unlocks {
    pub(super) fn retire(&self, live: impl Fn(u64, PeerId) -> bool) {
        if let Some(active) = &self.active
            && (!live(active.session, active.peer) || Instant::now() >= active.attempt.deadline)
        {
            active.attempt.cancel();
        }
    }

    pub(super) fn view(&self, session: u64, allowed: bool) -> GatewayUnlockView {
        let active = self
            .active
            .as_ref()
            .filter(|active| active.session == session && allowed);
        GatewayUnlockView {
            allowed,
            attempt_id: active.map(|active| active.attempt.id.clone()),
            phase: active.map_or(GatewayUnlockPhase::Password, |active| {
                active.attempt.phase()
            }),
        }
    }

    fn charge(&mut self, peer: PeerId, now: Instant) -> bool {
        self.attempts.retain(|_, times| {
            times.retain(|time| now.duration_since(*time) < RATE_WINDOW);
            !times.is_empty()
        });
        self.global_attempts
            .retain(|time| now.duration_since(*time) < RATE_WINDOW);
        let times = self.attempts.entry(peer).or_default();
        if times.len() >= 5 || self.global_attempts.len() >= 20 {
            return false;
        }
        times.push_back(now);
        self.global_attempts.push_back(now);
        true
    }

    pub(super) fn command(
        &mut self,
        session: u64,
        peer: PeerId,
        id: &str,
        command: GatewayUnlockCommand,
    ) -> Option<Arc<GatewayUnlockRequest>> {
        if id.is_empty() || id.len() > 128 {
            return None;
        }
        if matches!(command, GatewayUnlockCommand::Cancel) {
            if let Some(active) = &self.active
                && active.session == session
                && active.attempt.id == id
            {
                active.attempt.cancel();
            }
            return None;
        }
        let permit = Arc::clone(&self.admission).try_acquire_owned().ok()?;
        let now = Instant::now();
        let password = matches!(command, GatewayUnlockCommand::Password { .. });
        if password {
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.attempt.is_current())
            {
                return None;
            }
            self.active = Some(Active {
                session,
                peer,
                attempt: Arc::new(GatewayUnlockAttempt {
                    id: id.to_owned(),
                    deadline: now + LIFETIME,
                    phase: Mutex::new(GatewayUnlockPhase::Password),
                }),
            });
        }
        let active = self.active.as_ref()?;
        if active.session != session
            || active.peer != peer
            || active.attempt.id != id
            || !active.attempt.is_current()
        {
            return None;
        }
        let phase = active.attempt.phase();
        let admitted = match &command {
            GatewayUnlockCommand::Password { password } => {
                matches!(phase, GatewayUnlockPhase::Password)
                    && !password.0.is_empty()
                    && password.0.len() <= 4096
            }
            GatewayUnlockCommand::Passphrase { passphrase } => {
                matches!(phase, GatewayUnlockPhase::Passphrase)
                    && !passphrase.0.is_empty()
                    && passphrase.0.len() <= 4096
            }
            GatewayUnlockCommand::Standard => phase == GatewayUnlockPhase::Passphrase,
            GatewayUnlockCommand::Retry => phase == GatewayUnlockPhase::Unknown,
            GatewayUnlockCommand::Desktop => matches!(
                phase,
                GatewayUnlockPhase::Passphrase | GatewayUnlockPhase::Unknown
            ),
            GatewayUnlockCommand::Cancel => false,
        };
        if !admitted {
            if password {
                active.attempt.cancel();
            }
            return None;
        }
        let attempt = Arc::clone(&active.attempt);
        if matches!(
            command,
            GatewayUnlockCommand::Password { .. } | GatewayUnlockCommand::Passphrase { .. }
        ) && !self.charge(peer, now)
        {
            attempt.finish(GatewayUnlockPhase::RateLimited);
            return None;
        }
        attempt.finish(GatewayUnlockPhase::Busy);
        Some(Arc::new(GatewayUnlockRequest {
            command: Mutex::new(Some((
                command,
                GatewayUnlockGuard {
                    attempt,
                    _permit: permit,
                },
            ))),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn password() -> GatewayUnlockCommand {
        GatewayUnlockCommand::Password {
            password: GatewayUnlockSecret("synthetic password".into()),
        }
    }

    #[test]
    fn unlock_continuations_are_owned_and_cancellation_does_not_release_running_work() {
        let mut unlocks = Unlocks::default();
        let peer = PeerId::from_bytes([1; 16]);
        let request = unlocks.command(1, peer, "first", password()).unwrap();
        let (_, guard) = request.take().unwrap();
        assert!(request.take().is_none(), "secrets have one consumer");
        assert!(unlocks.command(2, peer, "other", password()).is_none());
        guard.finish(GatewayUnlockPhase::Passphrase);
        drop(guard);
        assert!(
            unlocks
                .command(2, peer, "first", GatewayUnlockCommand::Standard)
                .is_none()
        );
        assert!(
            unlocks
                .command(1, peer, "other", GatewayUnlockCommand::Standard)
                .is_none()
        );
        let request = unlocks
            .command(
                1,
                peer,
                "first",
                GatewayUnlockCommand::Passphrase {
                    passphrase: GatewayUnlockSecret(" Exact passphrase  ".into()),
                },
            )
            .unwrap();
        let (command, guard) = request.take().unwrap();
        let GatewayUnlockCommand::Passphrase { passphrase } = command else {
            panic!("passphrase command");
        };
        assert_eq!(passphrase.into_value().as_str(), " Exact passphrase  ");
        guard.finish(GatewayUnlockPhase::Opening);
        assert!(guard.is_current());
        unlocks.retire(|_, _| false);
        assert!(!guard.is_current());
        assert!(
            !guard
                .attempt()
                .finish_if_current(GatewayUnlockPhase::Complete)
        );
        assert_eq!(unlocks.view(1, true).phase, GatewayUnlockPhase::Cancelled);
        assert!(
            unlocks
                .command(2, peer, "replacement", password())
                .is_none(),
            "cancelled native work still owns admission"
        );
        drop(guard);
        let request = unlocks.command(2, peer, "replacement", password()).unwrap();
        let (_, guard) = request.take().unwrap();
        guard.finish(GatewayUnlockPhase::Unknown);
        drop(guard);
        assert!(
            unlocks
                .command(2, peer, "replacement", GatewayUnlockCommand::Standard)
                .is_none(),
            "unknown input never falls back to standard"
        );
        let request = unlocks
            .command(2, peer, "replacement", GatewayUnlockCommand::Retry)
            .unwrap();
        let (_, guard) = request.take().unwrap();
        guard.finish(GatewayUnlockPhase::Passphrase);
        drop(guard);
        Arc::get_mut(&mut unlocks.active.as_mut().unwrap().attempt)
            .unwrap()
            .deadline = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        assert!(
            unlocks
                .command(2, peer, "replacement", GatewayUnlockCommand::Standard)
                .is_none()
        );
        assert_eq!(unlocks.view(2, true).phase, GatewayUnlockPhase::Cancelled);
        let request = unlocks.command(3, peer, "completed", password()).unwrap();
        let (_, guard) = request.take().unwrap();
        guard.finish(GatewayUnlockPhase::Opening);
        assert!(
            guard
                .attempt()
                .finish_if_current(GatewayUnlockPhase::Complete)
        );
        unlocks.retire(|_, _| false);
        assert_eq!(unlocks.view(3, true).phase, GatewayUnlockPhase::Complete);
    }

    #[test]
    fn password_attempt_budget_survives_reconnect_and_has_a_global_bound() {
        let mut unlocks = Unlocks::default();
        for peer_ix in 0..4 {
            let peer = PeerId::from_bytes([peer_ix; 16]);
            for session in 1..=5 {
                let request = unlocks
                    .command(session, peer, &session.to_string(), password())
                    .unwrap();
                let (_, guard) = request.take().unwrap();
                guard.finish(GatewayUnlockPhase::Failed);
            }
            assert!(unlocks.command(6, peer, "limited", password()).is_none());
            assert_eq!(unlocks.view(6, true).phase, GatewayUnlockPhase::RateLimited);
        }
        let fresh_peer = PeerId::from_bytes([9; 16]);
        assert!(
            unlocks
                .command(7, fresh_peer, "global", password())
                .is_none()
        );
        for times in unlocks.attempts.values_mut() {
            for time in times {
                *time = Instant::now().checked_sub(RATE_WINDOW).unwrap();
            }
        }
        for time in &mut unlocks.global_attempts {
            *time = Instant::now().checked_sub(RATE_WINDOW).unwrap();
        }
        assert!(
            unlocks
                .command(8, fresh_peer, "after-window", password())
                .is_some()
        );
    }
}
