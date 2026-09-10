use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui::{Context, Window};
use wallet_ops::{
    SyncProgressUpdate, WalletIndexedCatchUpStatus, WalletSyncTip, vault::ViewUnlock,
};

const AUTO_LOCK_MONITOR_INTERVAL: Duration = Duration::from_secs(15);
const EXTENSION_ACTIVITY_INTERVAL: Duration = Duration::from_secs(15);
const EXTENSION_ACTIVITY_BUDGET: Duration = Duration::from_mins(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WalletActivitySource {
    Desktop,
    Extension,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AutoLockTimestamp {
    monotonic: Instant,
    wall: SystemTime,
}

impl AutoLockTimestamp {
    pub(super) fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    fn elapsed_since(self, earlier: Self) -> Duration {
        let monotonic = self.monotonic.saturating_duration_since(earlier.monotonic);
        let wall = self.wall.duration_since(earlier.wall).unwrap_or_default();
        monotonic.max(wall)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AutoLockDeadlineStatus {
    Locked,
    Disabled,
    Waiting,
    Overdue,
}

pub(super) fn auto_lock_deadline_status(
    effective_timeout: Option<Duration>,
    last_activity: Option<AutoLockTimestamp>,
    is_unlocked: bool,
    now: AutoLockTimestamp,
) -> AutoLockDeadlineStatus {
    if !is_unlocked || last_activity.is_none() {
        return AutoLockDeadlineStatus::Locked;
    }
    let Some(timeout) = effective_timeout else {
        return AutoLockDeadlineStatus::Disabled;
    };
    if now.elapsed_since(last_activity.expect("checked above")) >= timeout {
        AutoLockDeadlineStatus::Overdue
    } else {
        AutoLockDeadlineStatus::Waiting
    }
}

fn invoke_auto_lock_lifecycle(status: AutoLockDeadlineStatus, lock_vault: impl FnOnce()) -> bool {
    if status != AutoLockDeadlineStatus::Overdue {
        return false;
    }
    lock_vault();
    true
}

pub(super) struct AutoLockState {
    effective_timeout: Option<Duration>,
    last_activity: Option<AutoLockTimestamp>,
    pending_activity: Option<AutoLockTimestamp>,
    extension_budget_remaining: Duration,
    last_extension_activity: Option<AutoLockTimestamp>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AutoLockActivityStatus {
    Ignored,
    Recorded,
    Overdue,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct InitialCatchUpFingerprint {
    startup_progress: Option<SyncProgressUpdate>,
    indexed_catch_up: Option<WalletIndexedCatchUpStatus>,
    last_scanned_block: Option<u64>,
}

impl InitialCatchUpFingerprint {
    pub(super) const fn new(
        startup_progress: Option<SyncProgressUpdate>,
        sync_tip: Option<WalletSyncTip>,
    ) -> Self {
        let (indexed_catch_up, last_scanned_block) = match sync_tip {
            Some(sync_tip) => (sync_tip.indexed_catch_up, sync_tip.last_scanned_block),
            None => (None, None),
        };
        Self {
            startup_progress,
            indexed_catch_up,
            last_scanned_block,
        }
    }

    fn advances_from(self, previous: Option<Self>) -> bool {
        let previous = previous.unwrap_or_default();
        self.startup_progress.is_some() && self.startup_progress != previous.startup_progress
            || self.indexed_catch_up.is_some() && self.indexed_catch_up != previous.indexed_catch_up
            || self
                .last_scanned_block
                .is_some_and(|block| previous.last_scanned_block.is_none_or(|last| block > last))
    }

    fn merge(self, previous: Option<Self>) -> Self {
        let previous = previous.unwrap_or_default();
        Self {
            startup_progress: self.startup_progress.or(previous.startup_progress),
            indexed_catch_up: self.indexed_catch_up.or(previous.indexed_catch_up),
            last_scanned_block: match (self.last_scanned_block, previous.last_scanned_block) {
                (Some(current), Some(previous)) => Some(current.max(previous)),
                (current, previous) => current.or(previous),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum InitialCatchUpChainState {
    #[default]
    Pending,
    PendingWithProgress(InitialCatchUpFingerprint),
    Ready,
}

pub(super) struct InitialSyncActivity {
    wallet_generation: u64,
    chains: BTreeMap<u64, InitialCatchUpChainState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InitialSyncObservation {
    Started,
    Progress(InitialCatchUpFingerprint),
    Ready,
    Error,
}

pub(super) fn apply_initial_sync_observation(
    tracker: &mut InitialSyncActivity,
    auto_lock: &mut AutoLockState,
    is_unlocked: bool,
    wallet_generation: u64,
    chain_id: u64,
    observation: InitialSyncObservation,
    now: AutoLockTimestamp,
) -> bool {
    let activity = match observation {
        InitialSyncObservation::Started => {
            tracker.start_chain(wallet_generation, chain_id);
            false
        }
        InitialSyncObservation::Progress(fingerprint) => {
            tracker.observe_progress(wallet_generation, chain_id, fingerprint)
        }
        InitialSyncObservation::Ready => tracker.mark_ready(wallet_generation, chain_id),
        InitialSyncObservation::Error => {
            tracker.observe_error(wallet_generation, chain_id);
            false
        }
    };
    if activity {
        auto_lock.record_activity(is_unlocked, now);
    }
    activity
}

impl InitialSyncActivity {
    pub(super) const fn new(wallet_generation: u64) -> Self {
        Self {
            wallet_generation,
            chains: BTreeMap::new(),
        }
    }

    pub(super) fn reset_wallet_generation(&mut self, wallet_generation: u64) {
        if self.wallet_generation != wallet_generation {
            self.wallet_generation = wallet_generation;
            self.chains.clear();
        }
    }

    pub(super) fn start_chain(&mut self, wallet_generation: u64, chain_id: u64) {
        self.reset_wallet_generation(wallet_generation);
        self.chains.entry(chain_id).or_default();
    }

    pub(super) fn observe_progress(
        &mut self,
        wallet_generation: u64,
        chain_id: u64,
        fingerprint: InitialCatchUpFingerprint,
    ) -> bool {
        self.reset_wallet_generation(wallet_generation);
        let state = self.chains.entry(chain_id).or_default();
        let previous = match *state {
            InitialCatchUpChainState::Pending => None,
            InitialCatchUpChainState::PendingWithProgress(previous) => Some(previous),
            InitialCatchUpChainState::Ready => return false,
        };
        let advanced = fingerprint.advances_from(previous);
        *state = InitialCatchUpChainState::PendingWithProgress(fingerprint.merge(previous));
        advanced
    }

    pub(super) fn mark_ready(&mut self, wallet_generation: u64, chain_id: u64) -> bool {
        self.reset_wallet_generation(wallet_generation);
        let state = self.chains.entry(chain_id).or_default();
        if *state == InitialCatchUpChainState::Ready {
            return false;
        }
        *state = InitialCatchUpChainState::Ready;
        true
    }

    pub(super) fn observe_error(&mut self, wallet_generation: u64, chain_id: u64) {
        self.reset_wallet_generation(wallet_generation);
        self.chains.entry(chain_id).or_default();
    }
}

impl AutoLockState {
    pub(super) const fn new(effective_timeout: Option<Duration>) -> Self {
        Self {
            effective_timeout,
            last_activity: None,
            pending_activity: None,
            extension_budget_remaining: EXTENSION_ACTIVITY_BUDGET,
            last_extension_activity: None,
        }
    }

    pub(super) fn apply_policy(
        &mut self,
        effective_timeout: Option<Duration>,
        is_unlocked: bool,
        now: AutoLockTimestamp,
    ) {
        self.effective_timeout = effective_timeout;
        self.last_activity = is_unlocked.then_some(now);
        self.pending_activity = None;
    }

    fn record_wallet_activity(
        &mut self,
        source: WalletActivitySource,
        is_unlocked: bool,
        now: AutoLockTimestamp,
    ) -> AutoLockActivityStatus {
        if source == WalletActivitySource::Desktop {
            self.extension_budget_remaining = EXTENSION_ACTIVITY_BUDGET;
            self.last_extension_activity = None;
            return self.record_activity(is_unlocked, now);
        }
        match self.deadline_status(is_unlocked, now) {
            AutoLockDeadlineStatus::Overdue => return AutoLockActivityStatus::Overdue,
            AutoLockDeadlineStatus::Locked | AutoLockDeadlineStatus::Disabled => {
                return AutoLockActivityStatus::Ignored;
            }
            AutoLockDeadlineStatus::Waiting => {}
        }
        if self.extension_budget_remaining.is_zero()
            || self.last_extension_activity.is_some_and(|last| {
                now.monotonic.saturating_duration_since(last.monotonic)
                    < EXTENSION_ACTIVITY_INTERVAL
            })
        {
            return AutoLockActivityStatus::Ignored;
        }
        let last = self.last_activity.expect("waiting deadline has activity");
        let Some(monotonic_elapsed) = now.monotonic.checked_duration_since(last.monotonic) else {
            return AutoLockActivityStatus::Ignored;
        };
        let Ok(wall_elapsed) = now.wall.duration_since(last.wall) else {
            return AutoLockActivityStatus::Ignored;
        };
        // Cap each clock's advance so a partial final marker spends only the remaining
        // deadline extension, without moving either timestamp backward or past now.
        let monotonic_extension = monotonic_elapsed.min(self.extension_budget_remaining);
        let wall_extension = wall_elapsed.min(self.extension_budget_remaining);
        let extension = monotonic_extension.max(wall_extension);
        if extension.is_zero() {
            return AutoLockActivityStatus::Ignored;
        }
        self.last_activity = Some(AutoLockTimestamp {
            monotonic: last.monotonic + monotonic_extension,
            wall: last.wall + wall_extension,
        });
        self.pending_activity = None;
        self.extension_budget_remaining -= extension;
        self.last_extension_activity = Some(now);
        AutoLockActivityStatus::Recorded
    }

    fn record_activity(
        &mut self,
        is_unlocked: bool,
        now: AutoLockTimestamp,
    ) -> AutoLockActivityStatus {
        if !is_unlocked {
            return AutoLockActivityStatus::Ignored;
        }
        if self.last_activity.is_none() {
            self.last_activity = Some(now);
            self.pending_activity = None;
            return AutoLockActivityStatus::Recorded;
        }
        match self.deadline_status(is_unlocked, now) {
            AutoLockDeadlineStatus::Locked => AutoLockActivityStatus::Ignored,
            AutoLockDeadlineStatus::Overdue => {
                self.pending_activity = Some(now);
                AutoLockActivityStatus::Overdue
            }
            AutoLockDeadlineStatus::Disabled | AutoLockDeadlineStatus::Waiting => {
                self.last_activity = Some(now);
                self.pending_activity = None;
                AutoLockActivityStatus::Recorded
            }
        }
    }

    pub(super) const fn arm_after_view_unlock(&mut self, now: AutoLockTimestamp) {
        self.last_activity = Some(now);
        self.pending_activity = None;
    }

    pub(super) const fn disarm(&mut self) {
        self.last_activity = None;
        self.pending_activity = None;
    }

    const fn accept_pending_activity_after_denial(&mut self) {
        if let Some(activity) = self.pending_activity.take() {
            self.last_activity = Some(activity);
        }
    }

    pub(super) fn deadline_status(
        &self,
        is_unlocked: bool,
        now: AutoLockTimestamp,
    ) -> AutoLockDeadlineStatus {
        auto_lock_deadline_status(self.effective_timeout, self.last_activity, is_unlocked, now)
    }
}

impl super::WalletRoot {
    pub(super) fn advance_active_wallet_generation(&mut self) {
        self.advance_active_wallet_generation_for_gateway_unlock(None);
    }

    pub(super) fn advance_active_wallet_generation_for_gateway_unlock(
        &mut self,
        continuation: Option<u64>,
    ) {
        if !self.gateway_unlock_is_current(continuation) {
            self.retire_gateway_unlock();
        }
        self.active_wallet_generation = self.active_wallet_generation.wrapping_add(1);
        self.initial_sync_activity
            .reset_wallet_generation(self.active_wallet_generation);
        self.publish_gateway_desktop_state();
    }

    pub(super) fn install_vault_view_unlock(&mut self, view_unlock: Arc<ViewUnlock>) {
        self.vault_view_unlock = Some(view_unlock);
        self.auto_lock
            .arm_after_view_unlock(AutoLockTimestamp::now());
    }

    pub(super) fn handle_wallet_activity(
        &mut self,
        source: WalletActivitySource,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let now = AutoLockTimestamp::now();
        if self
            .auto_lock
            .record_wallet_activity(source, self.vault_view_unlock.is_some(), now)
            != AutoLockActivityStatus::Overdue
        {
            return false;
        }
        self.enforce_auto_lock_at(now, window, cx)
    }

    pub(super) fn handle_initial_sync_observation(
        &mut self,
        wallet_generation: u64,
        chain_id: u64,
        observation: InitialSyncObservation,
    ) -> bool {
        apply_initial_sync_observation(
            &mut self.initial_sync_activity,
            &mut self.auto_lock,
            self.vault_view_unlock.is_some(),
            wallet_generation,
            chain_id,
            observation,
            AutoLockTimestamp::now(),
        )
    }

    pub(super) fn enforce_auto_lock(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        self.enforce_auto_lock_at(AutoLockTimestamp::now(), window, cx)
    }

    fn enforce_auto_lock_at(
        &mut self,
        now: AutoLockTimestamp,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let status = self
            .auto_lock
            .deadline_status(self.vault_view_unlock.is_some(), now);
        if !invoke_auto_lock_lifecycle(status, || self.lock_vault(window, cx)) {
            return false;
        }
        let locked = self.vault_view_unlock.is_none();
        if !locked {
            self.auto_lock.accept_pending_activity_after_denial();
        }
        locked
    }

    pub(super) fn start_auto_lock_monitor(window: &Window, cx: &Context<'_, Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(AUTO_LOCK_MONITOR_INTERVAL)
                    .await;
                if this
                    .update_in(cx, |root, window, cx| {
                        root.enforce_auto_lock(window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
impl std::ops::Add<Duration> for AutoLockTimestamp {
    type Output = Self;

    fn add(self, duration: Duration) -> Self::Output {
        Self {
            monotonic: self.monotonic + duration,
            wall: self.wall + duration,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::root::vault::vault_lock_is_allowed;
    use wallet_ops::{SyncProgressStage, WalletIndexedCatchUpSource};

    use super::*;

    #[test]
    fn monitor_uses_low_frequency_fallback_cadence() {
        assert_eq!(AUTO_LOCK_MONITOR_INTERVAL, Duration::from_secs(15));
    }

    #[test]
    fn deadline_status_covers_locked_disabled_waiting_and_overdue() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(15);

        assert_eq!(
            auto_lock_deadline_status(Some(timeout), Some(started), false, started + timeout),
            AutoLockDeadlineStatus::Locked
        );
        assert_eq!(
            auto_lock_deadline_status(Some(timeout), None, true, started + timeout),
            AutoLockDeadlineStatus::Locked
        );
        assert_eq!(
            auto_lock_deadline_status(None, Some(started), true, started + timeout),
            AutoLockDeadlineStatus::Disabled
        );
        assert_eq!(
            auto_lock_deadline_status(
                Some(timeout),
                Some(started),
                true,
                started + timeout.saturating_sub(Duration::from_secs(1)),
            ),
            AutoLockDeadlineStatus::Waiting
        );
        assert_eq!(
            auto_lock_deadline_status(Some(timeout), Some(started), true, started + timeout),
            AutoLockDeadlineStatus::Overdue
        );
    }

    #[test]
    fn wall_clock_elapsed_during_suspend_expires_the_deadline() {
        let started = AutoLockTimestamp {
            monotonic: Instant::now(),
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
        };
        let timeout = Duration::from_mins(1);
        let resumed = AutoLockTimestamp {
            monotonic: started.monotonic + Duration::from_secs(10),
            wall: started.wall + timeout,
        };

        assert_eq!(
            auto_lock_deadline_status(Some(timeout), Some(started), true, resumed),
            AutoLockDeadlineStatus::Overdue
        );
    }

    #[test]
    fn backward_wall_clock_does_not_extend_monotonic_deadline() {
        let started = AutoLockTimestamp {
            monotonic: Instant::now(),
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
        };
        let timeout = Duration::from_mins(1);
        let now = AutoLockTimestamp {
            monotonic: started.monotonic + timeout,
            wall: SystemTime::UNIX_EPOCH + Duration::from_secs(500),
        };

        assert_eq!(
            auto_lock_deadline_status(Some(timeout), Some(started), true, now),
            AutoLockDeadlineStatus::Overdue
        );
    }

    #[test]
    fn state_only_arms_for_an_unlocked_vault() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(1);
        let mut state = AutoLockState::new(Some(timeout));

        state.record_activity(false, started);
        assert_eq!(
            state.deadline_status(false, started + timeout),
            AutoLockDeadlineStatus::Locked
        );

        state.record_activity(true, started);
        assert_eq!(
            state.deadline_status(true, started),
            AutoLockDeadlineStatus::Waiting
        );
        state.disarm();
        assert_eq!(
            state.deadline_status(true, started + timeout),
            AutoLockDeadlineStatus::Locked
        );
    }

    #[test]
    fn qualifying_activity_resets_enabled_deadline_but_external_activity_does_not() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(1);
        let mut state = AutoLockState::new(Some(timeout));
        state.record_activity(true, started);

        assert_eq!(
            state.deadline_status(true, started + timeout),
            AutoLockDeadlineStatus::Overdue,
            "without a wallet event, external activity must not change the deadline"
        );

        let wallet_activity = started + Duration::from_secs(45);
        state.record_activity(true, wallet_activity);
        assert_eq!(
            state.deadline_status(true, started + timeout),
            AutoLockDeadlineStatus::Waiting
        );
        assert_eq!(
            state.deadline_status(true, wallet_activity + timeout),
            AutoLockDeadlineStatus::Overdue
        );
    }

    #[test]
    #[allow(clippy::duration_suboptimal_units)]
    fn disabled_never_expires_and_locked_state_never_requests_teardown() {
        let started = AutoLockTimestamp::now();
        let elapsed = Duration::from_secs(365 * 24 * 60 * 60);
        let mut disabled = AutoLockState::new(None);
        disabled.record_activity(true, started);
        assert_eq!(
            disabled.deadline_status(true, started + elapsed),
            AutoLockDeadlineStatus::Disabled
        );

        let mut locked = AutoLockState::new(Some(Duration::from_mins(1)));
        locked.record_activity(true, started);
        assert_eq!(
            locked.deadline_status(false, started + elapsed),
            AutoLockDeadlineStatus::Locked
        );
        locked.disarm();
        assert_eq!(
            locked.deadline_status(false, started + elapsed),
            AutoLockDeadlineStatus::Locked
        );
    }

    #[test]
    fn view_unlock_installation_restarts_the_deadline() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(1);
        let mut state = AutoLockState::new(Some(timeout));

        for index in 0_u64..3 {
            let unlocked_at = started + Duration::from_secs(index * 120);
            state.arm_after_view_unlock(unlocked_at);
            assert_eq!(
                state.deadline_status(
                    true,
                    unlocked_at + timeout.saturating_sub(Duration::from_secs(1)),
                ),
                AutoLockDeadlineStatus::Waiting,
                "view unlock did not receive a fresh deadline"
            );
            assert_eq!(
                state.deadline_status(true, unlocked_at + timeout),
                AutoLockDeadlineStatus::Overdue,
                "deadline did not start at capability installation"
            );
        }
    }

    #[test]
    fn overdue_status_dispatches_once_and_other_statuses_do_not() {
        let mut dispatches = 0;

        for status in [
            AutoLockDeadlineStatus::Locked,
            AutoLockDeadlineStatus::Disabled,
            AutoLockDeadlineStatus::Waiting,
        ] {
            assert!(!invoke_auto_lock_lifecycle(status, || dispatches += 1));
        }
        assert_eq!(dispatches, 0);
        assert!(invoke_auto_lock_lifecycle(
            AutoLockDeadlineStatus::Overdue,
            || dispatches += 1,
        ));
        assert_eq!(dispatches, 1);
    }

    #[test]
    fn overdue_evaluations_retry_until_lock_admission() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(1);
        let now = started + timeout;
        let mut state = AutoLockState::new(Some(timeout));
        state.arm_after_view_unlock(started);
        let mut dispatches = 0;
        let mut locked = false;

        for (maintenance_idle, deletion_in_progress) in [(false, false), (true, true)] {
            let status = state.deadline_status(true, now);
            assert!(invoke_auto_lock_lifecycle(status, || {
                dispatches += 1;
                if vault_lock_is_allowed(maintenance_idle, deletion_in_progress) {
                    locked = true;
                }
            }));
            assert!(!locked);
            assert_eq!(
                state.deadline_status(true, now),
                AutoLockDeadlineStatus::Overdue,
                "denied lock must not consume the deadline"
            );
        }
        assert_eq!(dispatches, 2);

        assert!(invoke_auto_lock_lifecycle(
            state.deadline_status(true, now),
            || {
                dispatches += 1;
                if vault_lock_is_allowed(true, false) {
                    locked = true;
                }
            },
        ));
        assert!(locked, "the first admitted evaluation should lock");
        assert_eq!(dispatches, 3);
        state.disarm();
        assert_eq!(
            state.deadline_status(false, now),
            AutoLockDeadlineStatus::Locked
        );
    }

    #[test]
    fn overdue_activity_waits_for_lock_admission_outcome() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(1);
        let overdue_activity = started + timeout;
        let mut state = AutoLockState::new(Some(timeout));
        state.arm_after_view_unlock(started);

        assert_eq!(
            state.record_activity(true, overdue_activity),
            AutoLockActivityStatus::Overdue
        );
        assert_eq!(
            state.deadline_status(true, overdue_activity),
            AutoLockDeadlineStatus::Overdue,
            "activity must not erase an expired deadline before lock admission"
        );

        state.accept_pending_activity_after_denial();
        assert_eq!(
            state.deadline_status(true, overdue_activity),
            AutoLockDeadlineStatus::Waiting,
            "qualifying activity may reset the deadline after lock admission is denied"
        );
    }

    #[test]
    fn extension_activity_throttles_and_caps_total_deadline_extension() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(10);
        let mut state = AutoLockState::new(Some(timeout));
        state.arm_after_view_unlock(started);

        for (seconds, expected) in [
            (140, AutoLockActivityStatus::Recorded),
            (154, AutoLockActivityStatus::Ignored),
            (155, AutoLockActivityStatus::Recorded),
            (290, AutoLockActivityStatus::Recorded),
            (305, AutoLockActivityStatus::Recorded),
            (320, AutoLockActivityStatus::Ignored),
        ] {
            assert_eq!(
                state.record_wallet_activity(
                    WalletActivitySource::Extension,
                    true,
                    started + Duration::from_secs(seconds),
                ),
                expected,
            );
        }
        let capped_activity = started + Duration::from_mins(5);
        assert_eq!(state.last_activity, Some(capped_activity));
        assert_eq!(
            state.deadline_status(true, capped_activity + timeout),
            AutoLockDeadlineStatus::Overdue,
        );
        assert_eq!(
            state.record_wallet_activity(
                WalletActivitySource::Extension,
                true,
                capped_activity + timeout,
            ),
            AutoLockActivityStatus::Overdue,
            "an exhausted budget must still enforce expiry",
        );
        state.accept_pending_activity_after_denial();
        assert_eq!(
            state.deadline_status(true, capped_activity + timeout),
            AutoLockDeadlineStatus::Overdue,
            "an overdue extension marker cannot rescue a denied lock",
        );
    }

    #[test]
    fn only_desktop_activity_replenishes_extension_budget() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(10);
        let mut state = AutoLockState::new(Some(timeout));
        state.arm_after_view_unlock(started);
        assert_eq!(
            state.record_wallet_activity(
                WalletActivitySource::Extension,
                true,
                started + Duration::from_mins(5),
            ),
            AutoLockActivityStatus::Recorded,
        );
        let resumed = started + Duration::from_mins(6);
        let mut tracker = InitialSyncActivity::new(1);
        assert!(apply_initial_sync_observation(
            &mut tracker,
            &mut state,
            true,
            1,
            1,
            InitialSyncObservation::Progress(progress(10)),
            resumed,
        ));
        assert_eq!(
            state.record_wallet_activity(
                WalletActivitySource::Extension,
                true,
                resumed + Duration::from_secs(1),
            ),
            AutoLockActivityStatus::Ignored,
        );
        state.disarm();
        assert_eq!(
            state.record_wallet_activity(WalletActivitySource::Extension, false, resumed),
            AutoLockActivityStatus::Ignored,
        );
        state.arm_after_view_unlock(resumed);
        state.apply_policy(None, true, resumed);
        assert_eq!(
            state.record_wallet_activity(WalletActivitySource::Extension, true, resumed),
            AutoLockActivityStatus::Ignored,
        );
        state.apply_policy(Some(timeout), true, resumed);
        assert_eq!(
            state.record_wallet_activity(
                WalletActivitySource::Extension,
                true,
                resumed + Duration::from_secs(1),
            ),
            AutoLockActivityStatus::Ignored,
        );
        let desktop = resumed + Duration::from_secs(2);
        state.record_wallet_activity(WalletActivitySource::Desktop, true, desktop);
        assert_eq!(
            state.record_wallet_activity(
                WalletActivitySource::Extension,
                true,
                desktop + Duration::from_secs(1),
            ),
            AutoLockActivityStatus::Recorded,
        );
        state.record_wallet_activity(
            WalletActivitySource::Desktop,
            true,
            desktop + Duration::from_secs(2),
        );
        assert_eq!(
            state.record_wallet_activity(
                WalletActivitySource::Extension,
                true,
                desktop + Duration::from_secs(3),
            ),
            AutoLockActivityStatus::Recorded,
            "desktop input also clears the extension throttle",
        );
    }

    #[test]
    fn extension_markers_cannot_arm_or_rescue_an_expired_deadline() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(1);
        let mut state = AutoLockState::new(Some(timeout));
        assert_eq!(
            state.record_wallet_activity(WalletActivitySource::Extension, true, started),
            AutoLockActivityStatus::Ignored,
        );
        assert_eq!(
            state.deadline_status(true, started),
            AutoLockDeadlineStatus::Locked
        );
        state.arm_after_view_unlock(started);
        let activity = started + Duration::from_secs(1);
        state.record_wallet_activity(WalletActivitySource::Extension, true, activity);
        state.apply_policy(Some(Duration::from_secs(1)), true, activity);
        let overdue = activity + Duration::from_secs(1);
        assert_eq!(
            state.record_wallet_activity(WalletActivitySource::Extension, true, overdue),
            AutoLockActivityStatus::Overdue,
            "expiry takes precedence even during the throttle interval",
        );
        state.accept_pending_activity_after_denial();
        assert!(invoke_auto_lock_lifecycle(
            state.deadline_status(true, overdue),
            || state.disarm(),
        ));
        assert_eq!(
            state.record_wallet_activity(WalletActivitySource::Extension, false, overdue),
            AutoLockActivityStatus::Ignored,
        );
        assert_eq!(
            state.deadline_status(true, overdue),
            AutoLockDeadlineStatus::Locked
        );
    }

    #[test]
    fn extension_budget_accounts_for_both_clocks_without_rewinding_activity() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(10);
        for wall_leads in [false, true] {
            let mut state = AutoLockState::new(Some(timeout));
            state.arm_after_view_unlock(started);
            let now = AutoLockTimestamp {
                monotonic: started.monotonic
                    + Duration::from_secs(if wall_leads { 10 } else { 290 }),
                wall: started.wall + Duration::from_secs(if wall_leads { 290 } else { 10 }),
            };
            assert_eq!(
                state.record_wallet_activity(WalletActivitySource::Extension, true, now),
                AutoLockActivityStatus::Recorded,
            );
            let wall_jump = AutoLockTimestamp {
                monotonic: now.monotonic + Duration::from_secs(14),
                wall: now.wall + Duration::from_secs(20),
            };
            assert_eq!(
                state.record_wallet_activity(WalletActivitySource::Extension, true, wall_jump),
                AutoLockActivityStatus::Ignored,
                "wall-clock advances cannot bypass the monotonic throttle",
            );
            let final_marker = now + Duration::from_secs(15);
            assert_eq!(
                state.record_wallet_activity(WalletActivitySource::Extension, true, final_marker),
                AutoLockActivityStatus::Recorded,
            );
            assert_eq!(state.last_activity, Some(now + Duration::from_secs(10)));
            assert_eq!(
                state.deadline_status(true, now + Duration::from_secs(10) + timeout),
                AutoLockDeadlineStatus::Overdue,
            );
        }
        let mut state = AutoLockState::new(Some(timeout));
        state.arm_after_view_unlock(started);
        let rollback = AutoLockTimestamp {
            monotonic: started.monotonic + Duration::from_secs(15),
            wall: started.wall - Duration::from_secs(15),
        };
        assert_eq!(
            state.record_wallet_activity(WalletActivitySource::Extension, true, rollback),
            AutoLockActivityStatus::Ignored,
        );
        assert_eq!(state.last_activity, Some(started));
        assert_eq!(
            state.record_wallet_activity(
                WalletActivitySource::Extension,
                true,
                AutoLockTimestamp {
                    monotonic: started.monotonic + timeout,
                    wall: rollback.wall,
                },
            ),
            AutoLockActivityStatus::Overdue,
        );
    }

    fn progress(current_block: u64) -> InitialCatchUpFingerprint {
        InitialCatchUpFingerprint::new(
            Some(SyncProgressUpdate::new(
                SyncProgressStage::IndexingUtxos,
                0,
                current_block,
                100,
            )),
            None,
        )
    }

    #[test]
    fn initial_sync_progress_does_not_erase_elapsed_deadline_before_admission() {
        let started = AutoLockTimestamp::now();
        let timeout = Duration::from_mins(1);
        let mut tracker = InitialSyncActivity::new(1);
        let mut auto_lock = AutoLockState::new(Some(timeout));
        auto_lock.arm_after_view_unlock(started);
        tracker.start_chain(1, 1);

        assert!(apply_initial_sync_observation(
            &mut tracker,
            &mut auto_lock,
            true,
            1,
            1,
            InitialSyncObservation::Progress(progress(10)),
            started + timeout,
        ));
        assert_eq!(
            auto_lock.deadline_status(true, started + timeout),
            AutoLockDeadlineStatus::Overdue
        );
    }

    #[test]
    fn initial_sync_tracks_advancement_duplicates_ready_and_post_ready_resync() {
        let mut tracker = InitialSyncActivity::new(1);
        tracker.start_chain(1, 1);

        assert!(tracker.observe_progress(1, 1, progress(10)));
        assert!(!tracker.observe_progress(1, 1, progress(10)));
        assert!(tracker.observe_progress(1, 1, progress(20)));
        assert!(tracker.mark_ready(1, 1));
        assert!(!tracker.mark_ready(1, 1));
        assert!(
            !tracker.observe_progress(1, 1, progress(30)),
            "post-Ready resync must not count as activity"
        );
    }

    #[test]
    fn initial_sync_tracks_indexed_and_last_scanned_advancement() {
        let mut tracker = InitialSyncActivity::new(1);
        tracker.start_chain(1, 1);
        let tip = WalletSyncTip {
            last_scanned_block: Some(10),
            indexed_catch_up: Some(WalletIndexedCatchUpStatus {
                source: WalletIndexedCatchUpSource::IndexedArtifacts,
                from_block: 1,
                target_block: 100,
            }),
            ..WalletSyncTip::default()
        };
        let fingerprint = InitialCatchUpFingerprint::new(None, Some(tip));

        assert!(tracker.observe_progress(1, 1, fingerprint));
        assert!(!tracker.observe_progress(1, 1, fingerprint));
        assert!(tracker.observe_progress(
            1,
            1,
            InitialCatchUpFingerprint::new(
                None,
                Some(WalletSyncTip {
                    last_scanned_block: Some(11),
                    ..tip
                }),
            ),
        ));
    }

    #[test]
    fn stalled_and_failed_initial_sync_do_not_count_without_new_progress() {
        let mut tracker = InitialSyncActivity::new(1);
        tracker.start_chain(1, 1);
        assert!(tracker.observe_progress(1, 1, progress(10)));

        tracker.observe_error(1, 1);
        assert!(!tracker.observe_progress(1, 1, progress(10)));
        assert!(tracker.observe_progress(1, 1, progress(11)));
    }

    #[test]
    fn wallet_switch_and_new_chain_get_independent_initial_sync_tracking() {
        let mut tracker = InitialSyncActivity::new(1);
        tracker.start_chain(1, 1);
        assert!(tracker.observe_progress(1, 1, progress(10)));
        assert!(tracker.mark_ready(1, 1));

        tracker.start_chain(1, 137);
        assert!(tracker.observe_progress(1, 137, progress(10)));
        assert!(!tracker.observe_progress(1, 1, progress(20)));

        tracker.reset_wallet_generation(2);
        tracker.start_chain(2, 1);
        assert!(
            tracker.observe_progress(2, 1, progress(10)),
            "the same progress is fresh activity for a new wallet generation"
        );
    }
}
