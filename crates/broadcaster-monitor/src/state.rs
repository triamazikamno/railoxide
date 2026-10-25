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
    pub relay_adapt: Address,
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
        }
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
mod tests {
    use super::*;
    use alloy::primitives::address;
    use alloy::uint;
    use std::time::UNIX_EPOCH;

    fn sample_row(chain_id: u64, token: Address, fee: u64, fees_id: &str) -> FeeRow {
        sample_row_with_metadata(chain_id, token, fee, fees_id, 1, "8.2.3", Vec::new())
    }

    fn sample_row_with_metadata(
        chain_id: u64,
        token: Address,
        fee: u64,
        fees_id: &str,
        available_wallets: u32,
        version: &str,
        required_poi_list_keys: Vec<Arc<str>>,
    ) -> FeeRow {
        FeeRow {
            chain_id,
            railgun_address: Arc::from("0zk-test"),
            token_address: token,
            fee: U256::from(fee),
            fees_id: Arc::from(fees_id),
            fee_expiration: SystemTime::now(),
            available_wallets,
            version: Arc::from(version),
            relay_adapt: address!("0000000000000000000000000000000000000003"),
            relay_adapt_7702: Some(address!("0000000000000000000000000000000000000004")),
            required_poi_list_keys,
            identifier: None,
            last_seen: SystemTime::now(),
            reliability: 1.0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn announcement_row(
        chain_id: u64,
        broadcaster: &str,
        token: Address,
        fee: u64,
        fees_id: &str,
        received_at: SystemTime,
        fee_expiration: SystemTime,
    ) -> FeeRow {
        let mut row = sample_row(chain_id, token, fee, fees_id);
        row.railgun_address = Arc::from(broadcaster);
        row.last_seen = received_at;
        row.fee_expiration = fee_expiration;
        row
    }

    #[test]
    fn upsert_replaces_existing_row_for_same_key() {
        let mut state = MonitorState::new();
        let token = address!("0000000000000000000000000000000000000001");
        state.upsert_fee(sample_row(1, token, 100, "a"));
        state.upsert_fee(sample_row(1, token, 200, "b"));

        let rows = state.fee_rows();
        assert_eq!(
            rows.len(),
            1,
            "same (chain, broadcaster, token) must not duplicate"
        );
        let row = &rows[0];
        assert_eq!(row.fee, uint!(200_U256));
        assert_eq!(row.fees_id.as_ref(), "b");
    }

    #[test]
    fn upsert_replaces_metadata_for_same_key() {
        let mut state = MonitorState::new();
        let token = address!("0000000000000000000000000000000000000001");
        state.upsert_fee(sample_row_with_metadata(
            1,
            token,
            100,
            "a",
            0,
            "7.9.0",
            Vec::new(),
        ));
        state.upsert_fee(sample_row_with_metadata(
            1,
            token,
            200,
            "b",
            2,
            "8.2.3",
            vec![Arc::from("poi-list")],
        ));

        let rows = state.fee_rows();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.available_wallets, 2);
        assert_eq!(row.version.as_ref(), "8.2.3");
        assert_eq!(row.required_poi_list_keys, vec![Arc::from("poi-list")]);
    }

    #[test]
    fn upsert_keeps_separate_rows_per_token() {
        let mut state = MonitorState::new();
        let t1 = address!("0000000000000000000000000000000000000001");
        let t2 = address!("0000000000000000000000000000000000000002");
        state.upsert_fee(sample_row(1, t1, 100, "a"));
        state.upsert_fee(sample_row(1, t2, 200, "b"));
        assert_eq!(state.fee_rows().len(), 2);
    }

    #[test]
    fn upsert_keeps_separate_rows_per_chain() {
        let mut state = MonitorState::new();
        let token = address!("0000000000000000000000000000000000000001");
        state.upsert_fee(sample_row(1, token, 100, "a"));
        state.upsert_fee(sample_row(137, token, 200, "b"));
        assert_eq!(state.fee_rows().len(), 2);
    }

    fn admit_verified(
        state: &mut MonitorState,
        chain_id: u64,
        broadcaster: &Arc<str>,
        token_fees: &[(Address, u64)],
        fees_id: &str,
        wall_now: SystemTime,
        monotonic_now: Instant,
        expiration: SystemTime,
    ) -> FeeAnnouncementAdmission {
        let rows = token_fees
            .iter()
            .map(|(token, fee)| {
                announcement_row(
                    chain_id,
                    broadcaster,
                    *token,
                    *fee,
                    fees_id,
                    wall_now,
                    expiration,
                )
            })
            .collect();
        state.admit_fee_announcement(
            chain_id,
            broadcaster,
            expiration,
            wall_now,
            monotonic_now,
            rows,
        )
    }

    #[test]
    fn early_verified_update_is_pending_then_promoted_without_another_message() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let token = address!("0000000000000000000000000000000000000001");
        let first_expiration = wall + Duration::from_mins(2);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[(token, 100)],
                "first",
                wall,
                monotonic,
                first_expiration,
            ),
            FeeAnnouncementAdmission::Applied {
                revision: Some(1),
                visible_rows: 1,
            }
        );

        let early_wall = wall + Duration::from_secs(10);
        let early_expiration = early_wall + Duration::from_mins(2);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[(token, 200)],
                "early",
                early_wall,
                monotonic + Duration::from_secs(10),
                early_expiration,
            ),
            FeeAnnouncementAdmission::Pending {
                deadline: monotonic + Duration::from_secs(40),
                revision: None,
            }
        );
        assert_eq!(state.rev(), 1);
        assert_eq!(state.fee_rows()[0].fees_id.as_ref(), "first");
        assert!(
            state
                .promote_due_fee_announcements(
                    monotonic + Duration::from_secs(39),
                    wall + Duration::from_secs(39),
                )
                .is_empty()
        );
        assert_eq!(
            state.promote_due_fee_announcements(
                monotonic + Duration::from_secs(40),
                wall + Duration::from_secs(40),
            ),
            vec![2]
        );
        assert_eq!(state.fee_rows()[0].fees_id.as_ref(), "early");
    }

    #[test]
    fn pending_updates_coalesce_to_greatest_signed_expiration() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let token = address!("0000000000000000000000000000000000000001");
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[(token, 100)],
            "first",
            wall,
            monotonic,
            wall + Duration::from_mins(5),
        );
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[(token, 200)],
            "pending-a",
            wall + Duration::from_secs(10),
            monotonic + Duration::from_secs(10),
            wall + Duration::from_secs(310),
        );
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[(token, 300)],
            "pending-b",
            wall + Duration::from_secs(20),
            monotonic + Duration::from_secs(20),
            wall + Duration::from_secs(320),
        );
        for (fees_id, candidate_expiration) in [
            ("out-of-order", wall + Duration::from_secs(315)),
            ("equal-conflict", wall + Duration::from_secs(320)),
        ] {
            assert_eq!(
                admit_verified(
                    &mut state,
                    1,
                    &broadcaster,
                    &[(token, 999)],
                    fees_id,
                    wall + Duration::from_secs(30),
                    monotonic + Duration::from_secs(30),
                    candidate_expiration,
                ),
                FeeAnnouncementAdmission::Rejected { revision: None }
            );
        }

        assert_eq!(state.rev(), 1);
        assert_eq!(
            state.promote_due_fee_announcements(
                monotonic + Duration::from_mins(1),
                wall + Duration::from_mins(1),
            ),
            vec![2]
        );
        assert_eq!(state.fee_rows()[0].fees_id.as_ref(), "pending-b");
    }

    #[test]
    fn verified_admission_rejects_expired_and_non_increasing_expiration() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let token = address!("0000000000000000000000000000000000000001");
        let expiration = wall + Duration::from_mins(2);
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[(token, 100)],
            "first",
            wall,
            monotonic,
            expiration,
        );

        for rejected_expiration in [expiration - Duration::from_secs(1), expiration] {
            assert_eq!(
                admit_verified(
                    &mut state,
                    1,
                    &broadcaster,
                    &[(token, 200)],
                    "replay",
                    wall + Duration::from_secs(1),
                    monotonic + Duration::from_secs(1),
                    rejected_expiration,
                ),
                FeeAnnouncementAdmission::Rejected { revision: None }
            );
        }
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[(token, 300)],
                "expired",
                expiration + Duration::from_secs(2),
                monotonic + Duration::from_secs(2),
                expiration + Duration::from_secs(1),
            ),
            FeeAnnouncementAdmission::Rejected { revision: None }
        );
        assert_eq!(state.rev(), 1);
        assert_eq!(state.fee_rows()[0].fees_id.as_ref(), "first");
    }

    #[test]
    fn refresh_deadline_is_ttl_third_capped_at_one_minute() {
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let token = address!("0000000000000000000000000000000000000001");

        for (ttl, expected_interval) in [(120, 40), (300, 60)] {
            let mut state = MonitorState::new();
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[(token, 100)],
                "first",
                wall,
                monotonic,
                wall + Duration::from_secs(ttl),
            );
            assert_eq!(
                admit_verified(
                    &mut state,
                    1,
                    &broadcaster,
                    &[(token, 200)],
                    "early",
                    wall + Duration::from_secs(1),
                    monotonic + Duration::from_secs(1),
                    wall + Duration::from_secs(ttl + 1),
                ),
                FeeAnnouncementAdmission::Pending {
                    deadline: monotonic + Duration::from_secs(expected_interval),
                    revision: None,
                }
            );
        }
    }

    #[test]
    fn promotion_atomically_replaces_only_matching_publisher_and_chain() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster_a: Arc<str> = Arc::from("0zk-a");
        let broadcaster_b: Arc<str> = Arc::from("0zk-b");
        let token_a = address!("0000000000000000000000000000000000000001");
        let token_b = address!("0000000000000000000000000000000000000002");
        let expiration = wall + Duration::from_mins(5);

        for (chain, broadcaster, token) in
            [(137, &broadcaster_a, token_a), (1, &broadcaster_b, token_b)]
        {
            admit_verified(
                &mut state,
                chain,
                broadcaster,
                &[(token, 50)],
                "isolated",
                wall,
                monotonic,
                expiration,
            );
        }
        admit_verified(
            &mut state,
            1,
            &broadcaster_a,
            &[(token_a, 100), (token_b, 200)],
            "first",
            wall,
            monotonic,
            expiration,
        );
        admit_verified(
            &mut state,
            1,
            &broadcaster_a,
            &[(token_a, 300)],
            "replacement",
            wall + Duration::from_secs(10),
            monotonic + Duration::from_secs(10),
            expiration + Duration::from_secs(10),
        );
        state.promote_due_fee_announcements(
            monotonic + Duration::from_mins(1),
            wall + Duration::from_mins(1),
        );

        let rows = state.fee_rows();
        assert_eq!(rows.len(), 3);
        let replaced: Vec<_> = rows
            .iter()
            .filter(|row| row.chain_id == 1 && row.railgun_address == broadcaster_a)
            .collect();
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].token_address, token_a);
        assert!(
            rows.iter()
                .any(|row| row.chain_id == 137 && row.railgun_address == broadcaster_a)
        );
        assert!(
            rows.iter()
                .any(|row| row.chain_id == 1 && row.railgun_address == broadcaster_b)
        );
    }

    #[test]
    fn direct_upsert_never_changes_verified_publisher() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let token = address!("0000000000000000000000000000000000000001");
        let expiration = wall + Duration::from_mins(2);
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[(token, 100)],
            "verified",
            wall,
            monotonic,
            expiration,
        );
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[(token, 200)],
            "pending",
            wall + Duration::from_secs(10),
            monotonic + Duration::from_secs(10),
            expiration + Duration::from_secs(10),
        );
        let deadline = state.next_pending_fee_announcement_deadline();
        let bypass_row = announcement_row(
            1,
            &broadcaster,
            token,
            999,
            "direct-upsert",
            wall + Duration::from_hours(24 * 365),
            wall + Duration::from_hours(24 * 366),
        );
        assert_eq!(state.upsert_fee(bypass_row), 1);
        assert_eq!(state.next_pending_fee_announcement_deadline(), deadline);
        assert_eq!(state.fee_rows()[0].fees_id.as_ref(), "verified");
        assert_eq!(
            state.promote_due_fee_announcements(
                monotonic + Duration::from_secs(40),
                wall + Duration::from_secs(40),
            ),
            vec![2]
        );
        assert_eq!(state.fee_rows()[0].fees_id.as_ref(), "pending");
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[(token, 300)],
                "after-upsert",
                wall + Duration::from_secs(41),
                monotonic + Duration::from_secs(41),
                expiration + Duration::from_secs(20),
            ),
            FeeAnnouncementAdmission::Pending {
                deadline: monotonic + Duration::from_secs(70),
                revision: None,
            }
        );
    }

    #[test]
    fn revisions_only_track_visible_fee_map_changes() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let token = address!("0000000000000000000000000000000000000001");

        let expiration = wall + Duration::from_mins(2);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[],
                "empty",
                wall,
                monotonic,
                expiration,
            ),
            FeeAnnouncementAdmission::Applied {
                revision: None,
                visible_rows: 0,
            }
        );
        assert_eq!(state.rev(), 0);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[],
                "replay",
                wall,
                monotonic,
                expiration,
            ),
            FeeAnnouncementAdmission::Rejected { revision: None }
        );
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[(token, 100)],
            "visible",
            wall + Duration::from_secs(10),
            monotonic + Duration::from_secs(10),
            expiration + Duration::from_secs(10),
        );
        assert_eq!(state.rev(), 0);
        assert_eq!(
            state.promote_due_fee_announcements(
                monotonic + Duration::from_secs(40),
                wall + Duration::from_secs(40),
            ),
            vec![1]
        );
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[],
            "withdraw",
            wall + Duration::from_secs(49),
            monotonic + Duration::from_secs(49),
            expiration + Duration::from_secs(49),
        );
        assert_eq!(state.rev(), 1);
        assert!(
            state
                .promote_due_fee_announcements(
                    monotonic + Duration::from_secs(50),
                    wall + Duration::from_secs(50),
                )
                .is_empty()
        );
        assert_eq!(
            state.promote_due_fee_announcements(
                monotonic + Duration::from_secs(70),
                wall + Duration::from_secs(70),
            ),
            vec![2]
        );
        assert!(state.fee_rows().is_empty());
    }

    #[test]
    fn implausible_ttl_does_not_pin_verified_expiration_ordering() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let token = address!("0000000000000000000000000000000000000001");

        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[(token, 100)],
                "far-future",
                wall,
                monotonic,
                wall + Duration::from_mins(31),
            ),
            FeeAnnouncementAdmission::Rejected { revision: None }
        );
        assert_eq!(state.rev(), 0);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[(token, 200)],
                "normal",
                wall,
                monotonic,
                wall + Duration::from_mins(5),
            ),
            FeeAnnouncementAdmission::Applied {
                revision: Some(1),
                visible_rows: 1,
            }
        );
    }

    #[test]
    fn expired_active_generation_does_not_block_refresh_after_suspend() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let token = address!("0000000000000000000000000000000000000001");
        admit_verified(
            &mut state,
            1,
            &broadcaster,
            &[(token, 100)],
            "before-suspend",
            wall,
            monotonic,
            wall + Duration::from_mins(5),
        );

        let resumed_wall = wall + Duration::from_mins(6);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[(token, 200)],
                "after-suspend",
                resumed_wall,
                monotonic + Duration::from_secs(10),
                resumed_wall + Duration::from_mins(5),
            ),
            FeeAnnouncementAdmission::Applied {
                revision: Some(2),
                visible_rows: 1,
            }
        );
        assert_eq!(state.fee_rows()[0].fees_id.as_ref(), "after-suspend");
    }

    #[test]
    fn repeated_clear_does_not_advance_revision() {
        let mut state = MonitorState::new();
        assert_eq!(state.clear(), None);
        let token = address!("0000000000000000000000000000000000000001");
        assert_eq!(state.upsert_fee(sample_row(1, token, 100, "fees")), 1);
        assert_eq!(state.clear(), Some(2));
        assert_eq!(state.clear(), None);
        assert_eq!(state.rev(), 2);
    }

    #[test]
    fn clearing_internal_empty_verified_generation_has_no_visible_revision() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster: Arc<str> = Arc::from("0zk-a");
        let expiration = wall + Duration::from_mins(5);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[],
                "empty",
                wall,
                monotonic,
                expiration,
            ),
            FeeAnnouncementAdmission::Applied {
                revision: None,
                visible_rows: 0,
            }
        );
        assert_eq!(state.clear(), None);
        assert_eq!(state.rev(), 0);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster,
                &[],
                "empty-again",
                wall,
                monotonic,
                expiration,
            ),
            FeeAnnouncementAdmission::Applied {
                revision: None,
                visible_rows: 0,
            }
        );
    }

    #[test]
    fn expired_publisher_is_evicted_only_after_grace() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster_a: Arc<str> = Arc::from("0zk-a");
        let broadcaster_b: Arc<str> = Arc::from("0zk-b");
        let token = address!("0000000000000000000000000000000000000001");
        let a_expiration = wall + Duration::from_mins(2);
        admit_verified(
            &mut state,
            1,
            &broadcaster_a,
            &[(token, 100)],
            "a",
            wall,
            monotonic,
            a_expiration,
        );

        let within_grace = a_expiration + Duration::from_mins(3);
        let b_expiration = within_grace + Duration::from_mins(5);
        admit_verified(
            &mut state,
            1,
            &broadcaster_b,
            &[(token, 200)],
            "b",
            within_grace,
            monotonic,
            b_expiration,
        );
        assert_eq!(state.fee_rows().len(), 2);

        let past_grace = a_expiration + Duration::from_mins(10) + Duration::from_secs(1);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster_b,
                &[(token, 200)],
                "b",
                past_grace,
                monotonic,
                b_expiration,
            ),
            FeeAnnouncementAdmission::Rejected { revision: Some(3) }
        );
        let rows = state.fee_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].railgun_address, broadcaster_b);

        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster_a,
                &[(token, 100)],
                "a",
                past_grace,
                monotonic,
                a_expiration,
            ),
            FeeAnnouncementAdmission::Rejected { revision: None }
        );
        assert_eq!(state.rev(), 3);
        assert_eq!(state.fee_rows().len(), 1);
        // A direct upsert is only accepted for an address without a publisher entry.
        let probe = announcement_row(
            1,
            &broadcaster_a,
            token,
            300,
            "probe",
            past_grace,
            past_grace,
        );
        assert_eq!(state.upsert_fee(probe), 4);
    }

    #[test]
    fn pending_newer_announcement_defers_eviction() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let broadcaster_a: Arc<str> = Arc::from("0zk-a");
        let broadcaster_b: Arc<str> = Arc::from("0zk-b");
        let token = address!("0000000000000000000000000000000000000001");
        let active_expiration = wall + Duration::from_mins(2);
        admit_verified(
            &mut state,
            1,
            &broadcaster_a,
            &[(token, 100)],
            "active",
            wall,
            monotonic,
            active_expiration,
        );
        let pending_wall = wall + Duration::from_secs(10);
        assert!(matches!(
            admit_verified(
                &mut state,
                1,
                &broadcaster_a,
                &[(token, 200)],
                "pending",
                pending_wall,
                monotonic + Duration::from_secs(10),
                pending_wall + Duration::from_mins(25),
            ),
            FeeAnnouncementAdmission::Pending { .. }
        ));

        let past_active_grace =
            active_expiration + Duration::from_mins(10) + Duration::from_secs(1);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &broadcaster_b,
                &[(token, 300)],
                "b",
                past_active_grace,
                monotonic + Duration::from_secs(20),
                past_active_grace + Duration::from_mins(5),
            ),
            FeeAnnouncementAdmission::Applied {
                revision: Some(2),
                visible_rows: 1,
            }
        );
        assert!(
            state
                .fee_rows()
                .iter()
                .any(|row| row.railgun_address == broadcaster_a)
        );
    }

    #[test]
    fn full_chain_refuses_only_new_publishers() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let token = address!("0000000000000000000000000000000000000001");
        for i in 0..MAX_FEE_PUBLISHERS_PER_CHAIN {
            admit_verified(
                &mut state,
                1,
                &Arc::from(format!("0zk-{i}")),
                &[(token, 100)],
                "fill",
                wall,
                monotonic,
                wall + Duration::from_mins(5),
            );
        }
        let newcomer: Arc<str> = Arc::from("0zk-new");
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &newcomer,
                &[(token, 100)],
                "new",
                wall,
                monotonic,
                wall + Duration::from_mins(5),
            ),
            FeeAnnouncementAdmission::Rejected { revision: None }
        );
        assert_eq!(state.rev(), 128);
        assert!(
            !state
                .fee_rows()
                .iter()
                .any(|row| row.railgun_address == newcomer)
        );

        let later = wall + Duration::from_mins(2);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &Arc::from("0zk-0"),
                &[(token, 200)],
                "refresh",
                later,
                monotonic + Duration::from_mins(2),
                later + Duration::from_mins(5),
            ),
            FeeAnnouncementAdmission::Applied {
                revision: Some(129),
                visible_rows: 1,
            }
        );
        assert_eq!(
            admit_verified(
                &mut state,
                137,
                &newcomer,
                &[(token, 100)],
                "other-chain",
                later,
                monotonic + Duration::from_mins(2),
                later + Duration::from_mins(5),
            ),
            FeeAnnouncementAdmission::Applied {
                revision: Some(130),
                visible_rows: 1,
            }
        );
    }

    #[test]
    fn evicted_publisher_frees_chain_slot() {
        let mut state = MonitorState::new();
        let wall = UNIX_EPOCH + Duration::from_secs(1_000);
        let monotonic = Instant::now();
        let token = address!("0000000000000000000000000000000000000001");
        let short_expiration = wall + Duration::from_mins(1);
        for i in 0..MAX_FEE_PUBLISHERS_PER_CHAIN {
            let expiration = if i == 0 {
                short_expiration
            } else {
                wall + Duration::from_mins(30)
            };
            admit_verified(
                &mut state,
                1,
                &Arc::from(format!("0zk-{i}")),
                &[(token, 100)],
                "fill",
                wall,
                monotonic,
                expiration,
            );
        }

        let past_grace = short_expiration + Duration::from_mins(10) + Duration::from_secs(1);
        assert_eq!(
            admit_verified(
                &mut state,
                1,
                &Arc::from("0zk-new"),
                &[(token, 100)],
                "new",
                past_grace,
                monotonic,
                past_grace + Duration::from_mins(5),
            ),
            FeeAnnouncementAdmission::Applied {
                revision: Some(130),
                visible_rows: 1,
            }
        );
        let rows = state.fee_rows();
        assert_eq!(rows.len(), MAX_FEE_PUBLISHERS_PER_CHAIN);
        assert!(
            !rows
                .iter()
                .any(|row| row.railgun_address.as_ref() == "0zk-0")
        );
    }
}
