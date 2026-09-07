use super::model::{
    HEALTH_STRIKE_THRESHOLD, HEALTH_WINDOW, HEALTH_WITHDRAWAL_BASE, HEALTH_WITHDRAWAL_MAX,
};
use tokio::time::Instant;

/// Session-scoped endpoint health and physical request load.
#[derive(Debug, Clone, Default)]
pub(super) struct EndpointProfile {
    pub(super) request_count: u64,
    pub(super) recent_failures: Vec<Instant>,
    pub(super) withdrawn_until: Option<Instant>,
    pub(super) withdrawal_level: u32,
}

impl EndpointProfile {
    pub(super) fn is_withdrawn(&self, now: Instant) -> bool {
        self.withdrawn_until.is_some_and(|until| until > now)
    }

    pub(super) fn record_failure(&mut self, now: Instant) -> bool {
        self.recent_failures
            .retain(|failure| now.saturating_duration_since(*failure) <= HEALTH_WINDOW);
        self.recent_failures.push(now);
        if self.recent_failures.len() < HEALTH_STRIKE_THRESHOLD {
            return false;
        }
        self.recent_failures.clear();
        self.withdrawal_level = self.withdrawal_level.saturating_add(1);
        let exponent = self.withdrawal_level.saturating_sub(1).min(10);
        let cooldown = HEALTH_WITHDRAWAL_BASE
            .saturating_mul(1_u32 << exponent)
            .min(HEALTH_WITHDRAWAL_MAX);
        self.withdrawn_until = Some(now + cooldown);
        true
    }

    pub(super) fn restore_if_ready(&mut self, now: Instant) -> bool {
        if self.withdrawn_until.is_some_and(|until| until <= now) {
            self.withdrawn_until = None;
            return true;
        }
        false
    }
}
