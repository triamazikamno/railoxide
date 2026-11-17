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
        relay_adapt: Some(address!("0000000000000000000000000000000000000003")),
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
    assert_eq!(
        state.promote_due_fee_announcements(
            monotonic + Duration::from_secs(39),
            wall + Duration::from_secs(39),
        ),
        [] as [u64; 0]
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
    assert_eq!(
        state.promote_due_fee_announcements(
            monotonic + Duration::from_secs(50),
            wall + Duration::from_secs(50),
        ),
        [] as [u64; 0]
    );
    assert_eq!(
        state.promote_due_fee_announcements(
            monotonic + Duration::from_secs(70),
            wall + Duration::from_secs(70),
        ),
        vec![2]
    );
    assert_eq!(state.fee_rows(), [] as [FeeRow; 0]);
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

    let past_active_grace = active_expiration + Duration::from_mins(10) + Duration::from_secs(1);
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
