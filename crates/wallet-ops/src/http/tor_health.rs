#[allow(clippy::wildcard_imports)]
use super::*;

impl TorConnectionObservation {
    pub(super) const fn new(
        generation: u64,
        succeeded: bool,
        completed_at: Instant,
        setup_duration: Duration,
    ) -> Self {
        Self {
            generation,
            succeeded,
            completed_at,
            setup_duration,
            admitted_sequence: 0,
        }
    }
}

impl TorRuntimeHealthTracker {
    pub(super) const fn new(generation: u64) -> Self {
        Self {
            generation,
            observations: VecDeque::new(),
            last_admitted_completed_at: None,
            next_admitted_sequence: 1,
            episode_boundary_sequence: None,
            latched_cause: None,
        }
    }

    pub(super) fn record_at(
        &mut self,
        mut observation: TorConnectionObservation,
        now: Instant,
    ) -> bool {
        if observation.generation != self.generation {
            return false;
        }
        if observation.completed_at > now {
            return false;
        }
        if self
            .last_admitted_completed_at
            .is_some_and(|last| observation.completed_at < last)
        {
            return false;
        }
        self.expire(now);
        if now.saturating_duration_since(observation.completed_at) > TOR_OBSERVATION_AGE {
            return false;
        }
        let Some(next_admitted_sequence) = self.next_admitted_sequence.checked_add(1) else {
            return false;
        };
        observation.admitted_sequence = self.next_admitted_sequence;
        self.next_admitted_sequence = next_admitted_sequence;
        self.last_admitted_completed_at = Some(observation.completed_at);
        self.observations.push_back(observation);
        if self.observations.len() > TOR_OBSERVATION_LIMIT {
            self.observations.pop_front();
        }
        self.transition(now);
        true
    }

    fn expire(&mut self, now: Instant) {
        self.observations.retain(|observation| {
            observation.completed_at <= now
                && now.duration_since(observation.completed_at) <= TOR_OBSERVATION_AGE
        });
        if self.observations.is_empty() {
            self.latched_cause = None;
            self.episode_boundary_sequence = None;
        }
    }

    pub(super) fn classify(&self, now: Instant) -> TorRuntimeHealth {
        let (attempts, failures, mut successful_samples) = self.current_evidence(now);
        if attempts < TOR_MIN_OBSERVATIONS {
            return TorRuntimeHealth::Ready;
        }
        if failures >= TOR_FAILURE_MINIMUM && failures * 2 >= attempts {
            return TorRuntimeHealth::Unreliable;
        }

        if successful_samples.len() >= TOR_MIN_OBSERVATIONS
            && tor_setup_p75(&mut successful_samples).is_some_and(|p75| p75 >= TOR_SETUP_THRESHOLD)
        {
            return TorRuntimeHealth::Slow;
        }
        TorRuntimeHealth::Ready
    }

    #[cfg(test)]
    pub(super) fn health(&mut self, now: Instant) -> TorRuntimeHealth {
        self.health_report(now).health
    }

    pub(super) fn health_report(&mut self, now: Instant) -> TorRuntimeHealthReport {
        self.expire(now);
        self.transition(now);

        let health = self.latched_cause.unwrap_or(TorRuntimeHealth::Ready);
        let (recent_attempt_count, recent_failed_attempt_count, mut successful_samples) =
            self.current_evidence(now);
        let recent_successful_setup_p75 = tor_setup_p75(&mut successful_samples);
        TorRuntimeHealthReport {
            health,
            recent_attempt_count,
            recent_failed_attempt_count,
            recent_successful_sample_count: successful_samples.len(),
            recent_successful_setup_p75,
        }
    }

    pub(super) fn observations(&mut self, now: Instant) -> Vec<TorConnectionObservation> {
        self.expire(now);
        self.observations.iter().copied().collect()
    }

    fn current_evidence(&self, now: Instant) -> (usize, usize, Vec<Duration>) {
        let mut attempts = 0;
        let mut failures = 0;
        let mut successful_samples = Vec::new();
        for observation in &self.observations {
            if observation.completed_at > now
                || now.duration_since(observation.completed_at) > TOR_OBSERVATION_AGE
            {
                continue;
            }
            attempts += 1;
            if observation.succeeded {
                successful_samples.push(observation.setup_duration);
            } else {
                failures += 1;
            }
        }
        (attempts, failures, successful_samples)
    }

    fn transition(&mut self, now: Instant) {
        match self.classify(now) {
            TorRuntimeHealth::Ready => {
                if self.latched_cause.is_some()
                    && self.trailing_post_boundary_successes() >= TOR_RECOVERY_SUCCESS_STREAK
                {
                    self.latched_cause = None;
                    self.episode_boundary_sequence = None;
                }
            }
            cause => {
                if self.latched_cause.is_none() {
                    self.episode_boundary_sequence = self
                        .observations
                        .back()
                        .map(|observation| observation.admitted_sequence);
                }
                self.latched_cause = Some(cause);
            }
        }
    }

    fn trailing_post_boundary_successes(&self) -> usize {
        let Some(boundary) = self.episode_boundary_sequence else {
            return 0;
        };
        let mut successes = 0_usize;
        for observation in self.observations.iter().rev() {
            if observation.admitted_sequence <= boundary || !observation.succeeded {
                break;
            }
            successes = successes.saturating_add(1);
        }
        successes.min(TOR_RECOVERY_SUCCESS_STREAK)
    }
}

fn tor_setup_p75(successful_samples: &mut [Duration]) -> Option<Duration> {
    if successful_samples.is_empty() {
        return None;
    }
    successful_samples.sort_unstable();
    let rank = (successful_samples.len() * 3).div_ceil(4);
    Some(successful_samples[rank - 1])
}

impl TorBridgeActivity {
    pub(super) fn new(generation: u64) -> Arc<Self> {
        Arc::new(Self {
            generation,
            activity_origin: Instant::now(),
            downloaded_bytes: AtomicU64::new(0),
            connecting_streams: AtomicU64::new(0),
            active_streams: AtomicU64::new(0),
            successful_connections: AtomicU64::new(0),
            failed_connections: AtomicU64::new(0),
            last_activity_tick: AtomicU64::new(0),
            tracker: Mutex::new(TorRuntimeHealthTracker::new(generation)),
        })
    }

    pub(super) fn begin_connection(self: &Arc<Self>) -> TorConnectingGuard {
        self.connecting_streams.fetch_add(1, Ordering::Relaxed);
        self.touch();
        TorConnectingGuard {
            activity: Arc::clone(self),
            started_at: Instant::now(),
        }
    }

    pub(super) fn touch(&self) {
        let elapsed_nanos = Instant::now()
            .saturating_duration_since(self.activity_origin)
            .as_nanos();
        let tick = u64::try_from(elapsed_nanos).unwrap_or(u64::MAX).max(1);
        self.last_activity_tick.fetch_max(tick, Ordering::Relaxed);
    }

    pub(super) fn record_connection(&self, succeeded: bool, setup_duration: Duration) {
        if succeeded {
            self.successful_connections.fetch_add(1, Ordering::Relaxed);
            self.active_streams.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed_connections.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut tracker) = self.tracker.lock() {
            let completed_at = Instant::now();
            let _ = tracker.record_at(
                TorConnectionObservation::new(
                    self.generation,
                    succeeded,
                    completed_at,
                    setup_duration,
                ),
                completed_at,
            );
        }
        self.touch();
    }

    pub(super) fn finish_active_stream(&self) {
        let _ = self
            .active_streams
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_sub(1)
            });
        self.touch();
    }

    pub(super) fn add_downloaded_bytes(&self, bytes: usize) {
        self.downloaded_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.touch();
    }

    pub(super) fn snapshot(&self) -> TorBridgeActivitySnapshot {
        self.snapshot_at(Instant::now())
    }

    pub(super) fn snapshot_at(&self, now: Instant) -> TorBridgeActivitySnapshot {
        let observations = self
            .tracker
            .lock()
            .map(|mut tracker| tracker.observations(now))
            .unwrap_or_default();
        let mut successful: Vec<_> = observations
            .iter()
            .filter(|observation| observation.succeeded)
            .map(|observation| observation.setup_duration)
            .collect();
        successful.sort_unstable();
        let median_setup_duration = median_duration(&successful);
        let last_activity_age = self.last_activity_age(now);
        TorBridgeActivitySnapshot {
            generation: self.generation,
            session_duration: now.saturating_duration_since(self.activity_origin),
            downloaded_bytes: self.downloaded_bytes.load(Ordering::Relaxed),
            connecting_streams: self.connecting_streams.load(Ordering::Relaxed),
            active_streams: self.active_streams.load(Ordering::Relaxed),
            successful_connections: self.successful_connections.load(Ordering::Relaxed),
            failed_connections: self.failed_connections.load(Ordering::Relaxed),
            recent_connection_sample_count: observations.len(),
            recent_successful_sample_count: successful.len(),
            median_setup_duration,
            last_activity_age,
        }
    }

    pub(super) fn runtime_health_report(&self) -> TorRuntimeHealthReport {
        let default_report = TorRuntimeHealthReport {
            health: TorRuntimeHealth::Ready,
            recent_attempt_count: 0,
            recent_failed_attempt_count: 0,
            recent_successful_sample_count: 0,
            recent_successful_setup_p75: None,
        };
        self.tracker.lock().map_or(default_report, |mut tracker| {
            tracker.health_report(Instant::now())
        })
    }

    fn last_activity_age(&self, now: Instant) -> Option<Duration> {
        let tick = self.last_activity_tick.load(Ordering::Relaxed);
        if tick == 0 {
            return None;
        }
        let elapsed_since_origin = now.saturating_duration_since(self.activity_origin);
        Some(elapsed_since_origin.saturating_sub(Duration::from_nanos(tick)))
    }
}

fn median_duration(values: &[Duration]) -> Option<Duration> {
    let middle = values.len() / 2;
    match values.len() {
        0 => None,
        length if length % 2 == 1 => values.get(middle).copied(),
        _ => {
            let lower = values.get(middle - 1).copied()?;
            let upper = values.get(middle).copied()?;
            Some(lower + upper.saturating_sub(lower) / 2)
        }
    }
}

pub(super) fn format_tor_runtime_health_detail(
    cause: &'static str,
    report: TorRuntimeHealthReport,
) -> String {
    let setup_duration = report
        .recent_successful_setup_p75
        .map_or_else(String::new, |p75| {
            format!(
                "; measured p75 connection setup: {} ms ({} successful samples)",
                p75.as_millis(),
                report.recent_successful_sample_count,
            )
        });
    format!(
        "Degraded. Recent Tor connections are {cause} ({} of {} recent Tor connection attempts failed{setup_duration})",
        report.recent_failed_attempt_count, report.recent_attempt_count,
    )
}
