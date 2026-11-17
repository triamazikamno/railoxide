use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use alloy::primitives::Address;
use parking_lot::RwLock;
use ruint::aliases::U256;
use tokio::sync::watch;

pub const DEFAULT_EVENT_CAPACITY: usize = 1_024;
const MAX_FEE_ANNOUNCEMENT_REFRESH_INTERVAL: Duration = Duration::from_mins(1);
const MAX_FEE_ANNOUNCEMENT_TTL: Duration = Duration::from_mins(30);
const FEE_PUBLISHER_EVICTION_GRACE: Duration = Duration::from_mins(10);
const MAX_FEE_PUBLISHERS_PER_CHAIN: usize = 128;

/// Identifier for a single fee row, keyed by chain, broadcaster, and token.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct FeeRowKey {
    pub chain_id: u64,
    pub railgun_address: Arc<str>,
    pub token_address: Address,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct FeePublisherKey {
    chain_id: u64,
    railgun_address: Arc<str>,
}

impl FeePublisherKey {
    const fn new(chain_id: u64, railgun_address: Arc<str>) -> Self {
        Self {
            chain_id,
            railgun_address,
        }
    }
}

#[derive(Debug, Clone)]
struct PendingFeeAnnouncement {
    signed_expiration: SystemTime,
    rows: Vec<FeeRow>,
}

#[derive(Debug, Clone, Copy)]
struct ActiveVerifiedFeeGeneration {
    signed_expiration: SystemTime,
    refresh_at: Instant,
}

#[derive(Debug, Clone)]
struct FeePublisherState {
    active: ActiveVerifiedFeeGeneration,
    pending: Option<PendingFeeAnnouncement>,
    signed_expiration_high_water: SystemTime,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FeeAnnouncementAdmission {
    Rejected {
        revision: Option<u64>,
    },
    Pending {
        deadline: Instant,
        revision: Option<u64>,
    },
    Applied {
        revision: Option<u64>,
        visible_rows: usize,
    },
}

impl FeeAnnouncementAdmission {
    #[must_use]
    pub const fn revision(self) -> Option<u64> {
        match self {
            Self::Rejected { revision }
            | Self::Pending { revision, .. }
            | Self::Applied { revision, .. } => revision,
        }
    }

    #[must_use]
    pub const fn visible_rows(self) -> usize {
        match self {
            Self::Applied { visible_rows, .. } => visible_rows,
            Self::Rejected { .. } | Self::Pending { .. } => 0,
        }
    }
}

/// Snapshot of the latest fee entry for a single `(chain, broadcaster, token)` tuple.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub struct FeeRow {
    pub chain_id: u64,
    pub railgun_address: Arc<str>,
    pub token_address: Address,
    pub fee: U256,
    pub fees_id: Arc<str>,
    pub fee_expiration: SystemTime,
    pub available_wallets: u32,
    pub version: Arc<str>,
    pub relay_adapt: Option<Address>,
    pub relay_adapt_7702: Option<Address>,
    pub required_poi_list_keys: Vec<Arc<str>>,
    pub identifier: Option<Arc<str>>,
    pub last_seen: SystemTime,
    pub reliability: f64,
}

/// Aggregate peer statistics mirrored from the Waku node for the UI header.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct PeerSummary {
    pub connected: usize,
    pub known: usize,
    pub dialing: usize,
    pub lightpush_capable: usize,
    pub peer_exchange_capable: usize,
    pub network_label: Arc<str>,
    pub network_degraded: bool,
}

/// Read-only per-peer row derived from Waku peer state for the peers pane.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PeerRow {
    pub peer_id: Arc<str>,
    pub addrs: Vec<Arc<str>>,
    pub connected: bool,
    pub dialing: bool,
    pub supports_lightpush_v3: bool,
    pub supports_peer_exchange: bool,
    pub supports_filter: bool,
    pub dial_failures: u32,
}

/// Mutable broadcaster monitor state read by the UI and mutated by background events.
pub struct MonitorState {
    fees: HashMap<FeeRowKey, FeeRow>,
    fee_publishers: HashMap<FeePublisherKey, FeePublisherState>,
    peer_summary: PeerSummary,
    peer_rows: Vec<PeerRow>,
    rev: AtomicU64,
    fee_subscription_started: Option<Instant>,
}

impl MonitorState {
    fn fee_refresh_deadline(
        signed_expiration: SystemTime,
        wall_now: SystemTime,
        monotonic_now: Instant,
    ) -> Option<Instant> {
        let ttl = signed_expiration.duration_since(wall_now).ok()?;
        if ttl > MAX_FEE_ANNOUNCEMENT_TTL {
            return None;
        }
        let refresh_interval = (ttl / 3).min(MAX_FEE_ANNOUNCEMENT_REFRESH_INTERVAL);
        Some(monotonic_now + refresh_interval)
    }

    #[must_use]
    pub fn new() -> Self {
        Self {
            fees: HashMap::new(),
            fee_publishers: HashMap::new(),
            peer_summary: PeerSummary::default(),
            peer_rows: Vec::new(),
            rev: AtomicU64::new(0),
            fee_subscription_started: None,
        }
    }

    /// Marks the fee subscription live. Broadcasters' offers arrive over the following
    /// announcements, so an offer missing soon after may still come.
    pub fn start_fee_subscription(&mut self) {
        self.fee_subscription_started = Some(Instant::now());
    }

    /// When the latest fee subscription went live, or `None` before the first one.
    #[must_use]
    pub const fn fee_subscription_started(&self) -> Option<Instant> {
        self.fee_subscription_started
    }

    /// Current user-visible state revision. Increases on changes that should redraw the UI.
    #[must_use]
    pub fn rev(&self) -> u64 {
        self.rev.load(Ordering::Acquire)
    }

    fn bump_rev(&self) -> u64 {
        self.rev.fetch_add(1, Ordering::Release) + 1
    }

    pub fn upsert_fee(&mut self, row: FeeRow) -> u64 {
        // Direct row updates must not bypass verified publisher generation ownership.
        if self.fee_publishers.contains_key(&FeePublisherKey::new(
            row.chain_id,
            row.railgun_address.clone(),
        )) {
            return self.rev();
        }
        let key = FeeRowKey {
            chain_id: row.chain_id,
            railgun_address: row.railgun_address.clone(),
            token_address: row.token_address,
        };
        if self.fees.get(&key) == Some(&row) {
            return self.rev();
        }
        self.fees.insert(key, row);
        self.bump_rev()
    }

    pub fn admit_fee_announcement(
        &mut self,
        chain_id: u64,
        railgun_address: &Arc<str>,
        signed_expiration: SystemTime,
        wall_now: SystemTime,
        monotonic_now: Instant,
        rows: Vec<FeeRow>,
    ) -> FeeAnnouncementAdmission {
        let evicted_revision = self.evict_expired_fee_publishers(wall_now);
        let mut admission = self.admit_unevicted_fee_announcement(
            chain_id,
            railgun_address,
            signed_expiration,
            wall_now,
            monotonic_now,
            rows,
        );
        let (FeeAnnouncementAdmission::Rejected { revision }
        | FeeAnnouncementAdmission::Pending { revision, .. }
        | FeeAnnouncementAdmission::Applied { revision, .. }) = &mut admission;
        *revision = revision.or(evicted_revision);
        admission
    }

    fn evict_expired_fee_publishers(&mut self, wall_now: SystemTime) -> Option<u64> {
        let evicted: HashSet<_> = self
            .fee_publishers
            .extract_if(|_key, publisher| {
                wall_now
                    .duration_since(publisher.signed_expiration_high_water)
                    .is_ok_and(|elapsed| elapsed > FEE_PUBLISHER_EVICTION_GRACE)
            })
            .map(|(key, _publisher)| key)
            .collect();
        if evicted.is_empty() {
            return None;
        }
        let rows_before = self.fees.len();
        self.fees.retain(|key, _row| {
            !evicted.contains(&FeePublisherKey::new(
                key.chain_id,
                key.railgun_address.clone(),
            ))
        });
        (self.fees.len() != rows_before).then(|| self.bump_rev())
    }

    fn admit_unevicted_fee_announcement(
        &mut self,
        chain_id: u64,
        railgun_address: &Arc<str>,
        signed_expiration: SystemTime,
        wall_now: SystemTime,
        monotonic_now: Instant,
        rows: Vec<FeeRow>,
    ) -> FeeAnnouncementAdmission {
        const REJECTED: FeeAnnouncementAdmission =
            FeeAnnouncementAdmission::Rejected { revision: None };
        if !Self::candidate_rows_are_consistent(chain_id, railgun_address, signed_expiration, &rows)
        {
            return REJECTED;
        }

        let publisher_key = FeePublisherKey::new(chain_id, railgun_address.clone());
        if signed_expiration <= wall_now {
            return REJECTED;
        }
        let Some(refresh_at) =
            Self::fee_refresh_deadline(signed_expiration, wall_now, monotonic_now)
        else {
            return REJECTED;
        };
        let candidate = PendingFeeAnnouncement {
            signed_expiration,
            rows,
        };

        if let Some(publisher) = self.fee_publishers.get_mut(&publisher_key) {
            if signed_expiration <= publisher.signed_expiration_high_water {
                return REJECTED;
            }
            debug_assert!(signed_expiration > publisher.active.signed_expiration);
            publisher.signed_expiration_high_water = signed_expiration;
            if wall_now < publisher.active.signed_expiration
                && monotonic_now < publisher.active.refresh_at
            {
                let deadline = publisher.active.refresh_at;
                publisher.pending = Some(candidate);
                return FeeAnnouncementAdmission::Pending {
                    deadline,
                    revision: None,
                };
            }

            publisher.active = ActiveVerifiedFeeGeneration {
                signed_expiration: candidate.signed_expiration,
                refresh_at,
            };
            publisher.pending = None;
        } else {
            let chain_publishers = self
                .fee_publishers
                .keys()
                .filter(|key| key.chain_id == chain_id)
                .count();
            if chain_publishers >= MAX_FEE_PUBLISHERS_PER_CHAIN {
                return REJECTED;
            }
            self.fee_publishers.insert(
                publisher_key,
                FeePublisherState {
                    active: ActiveVerifiedFeeGeneration {
                        signed_expiration: candidate.signed_expiration,
                        refresh_at,
                    },
                    pending: None,
                    signed_expiration_high_water: candidate.signed_expiration,
                },
            );
        }

        self.apply_fee_announcement(chain_id, railgun_address, candidate.rows)
    }

    #[must_use]
    pub fn next_pending_fee_announcement_deadline(&self) -> Option<Instant> {
        self.fee_publishers
            .values()
            .filter(|publisher| publisher.pending.is_some())
            .map(|publisher| publisher.active.refresh_at)
            .min()
    }

    pub fn promote_due_fee_announcements(
        &mut self,
        monotonic_now: Instant,
        wall_now: SystemTime,
    ) -> Vec<u64> {
        let due_publishers: Vec<_> = self
            .fee_publishers
            .iter()
            .filter(|(_key, publisher)| {
                publisher.pending.is_some() && publisher.active.refresh_at <= monotonic_now
            })
            .map(|(key, _publisher)| key.clone())
            .collect();
        let mut revisions = Vec::new();

        for publisher_key in due_publishers {
            let candidate = {
                let Some(publisher) = self.fee_publishers.get_mut(&publisher_key) else {
                    continue;
                };
                let Some(candidate) = publisher.pending.take() else {
                    continue;
                };
                if candidate.signed_expiration <= wall_now {
                    continue;
                }
                let Some(refresh_at) = Self::fee_refresh_deadline(
                    candidate.signed_expiration,
                    wall_now,
                    monotonic_now,
                ) else {
                    continue;
                };
                debug_assert!(candidate.signed_expiration > publisher.active.signed_expiration);
                publisher.active = ActiveVerifiedFeeGeneration {
                    signed_expiration: candidate.signed_expiration,
                    refresh_at,
                };
                candidate
            };
            if let FeeAnnouncementAdmission::Applied {
                revision: Some(revision),
                ..
            } = self.apply_fee_announcement(
                publisher_key.chain_id,
                &publisher_key.railgun_address,
                candidate.rows,
            ) {
                revisions.push(revision);
            }
        }

        revisions
    }

    fn candidate_rows_are_consistent(
        chain_id: u64,
        railgun_address: &Arc<str>,
        signed_expiration: SystemTime,
        rows: &[FeeRow],
    ) -> bool {
        let mut tokens = HashSet::with_capacity(rows.len());
        rows.iter().all(|row| {
            row.chain_id == chain_id
                && &row.railgun_address == railgun_address
                && row.fee_expiration == signed_expiration
                && tokens.insert(row.token_address)
        })
    }

    fn apply_fee_announcement(
        &mut self,
        chain_id: u64,
        railgun_address: &Arc<str>,
        rows: Vec<FeeRow>,
    ) -> FeeAnnouncementAdmission {
        let visible_rows = rows.len();
        let existing_rows = self
            .fees
            .iter()
            .filter(|(key, _row)| {
                key.chain_id == chain_id && &key.railgun_address == railgun_address
            })
            .count();
        let unchanged = existing_rows == visible_rows
            && rows.iter().all(|row| {
                let key = FeeRowKey {
                    chain_id,
                    railgun_address: railgun_address.clone(),
                    token_address: row.token_address,
                };
                self.fees.get(&key) == Some(row)
            });
        if unchanged {
            return FeeAnnouncementAdmission::Applied {
                revision: None,
                visible_rows,
            };
        }

        self.fees.retain(|key, _row| {
            key.chain_id != chain_id || &key.railgun_address != railgun_address
        });
        for row in rows {
            let key = FeeRowKey {
                chain_id: row.chain_id,
                railgun_address: row.railgun_address.clone(),
                token_address: row.token_address,
            };
            self.fees.insert(key, row);
        }
        FeeAnnouncementAdmission::Applied {
            revision: Some(self.bump_rev()),
            visible_rows,
        }
    }

    pub fn set_peers(&mut self, summary: PeerSummary, rows: Vec<PeerRow>) -> Option<u64> {
        if self.peer_summary == summary && self.peer_rows == rows {
            return None;
        }
        self.peer_summary = summary;
        self.peer_rows = rows;
        Some(self.bump_rev())
    }

    pub fn clear(&mut self) -> Option<u64> {
        let visible_state_changed = !self.fees.is_empty()
            || self.peer_summary != PeerSummary::default()
            || !self.peer_rows.is_empty();
        if !visible_state_changed && self.fee_publishers.is_empty() {
            return None;
        }
        self.fees.clear();
        self.fee_publishers.clear();
        self.peer_summary = PeerSummary::default();
        self.peer_rows.clear();
        visible_state_changed.then(|| self.bump_rev())
    }

    #[must_use]
    pub fn fee_rows(&self) -> Vec<FeeRow> {
        self.fees.values().cloned().collect()
    }

    #[must_use]
    pub fn peer_summary(&self) -> PeerSummary {
        self.peer_summary.clone()
    }

    #[must_use]
    pub fn peer_rows(&self) -> Vec<PeerRow> {
        self.peer_rows.clone()
    }
}

impl Default for MonitorState {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared handle for the broadcaster monitor's mutable state.
pub type Shared = Arc<RwLock<MonitorState>>;

/// Build a fresh shared state container.
#[must_use]
pub fn shared() -> Shared {
    Arc::new(RwLock::new(MonitorState::new()))
}

/// Revision signal used between background tasks and the UI polling path.
pub type EventTx = watch::Sender<u64>;
pub type EventRx = watch::Receiver<u64>;

#[must_use]
pub fn event_channel(_capacity: usize) -> (EventTx, EventRx) {
    watch::channel(0)
}

pub fn publish_revision(events: &EventTx, revision: u64) {
    events.send_if_modified(|current| {
        if revision <= *current {
            return false;
        }
        *current = revision;
        true
    });
}

#[cfg(test)]
mod tests;
