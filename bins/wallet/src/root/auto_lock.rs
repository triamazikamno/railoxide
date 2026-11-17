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

    pub(super) fn elapsed_since(self, earlier: Self) -> Duration {
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
impl AutoLockTimestamp {
    /// Models a suspend, during which only wall time advances.
    pub(super) fn with_wall_advanced(self, duration: Duration) -> Self {
        Self {
            monotonic: self.monotonic,
            wall: self.wall + duration,
        }
    }
}

#[cfg(test)]
mod tests;
