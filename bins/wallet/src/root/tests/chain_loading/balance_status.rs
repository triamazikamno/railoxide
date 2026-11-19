use super::*;

#[test]
fn balance_sync_presence_degrades_for_stalled_or_lagging_heads() {
    let now = 1_000;
    let ethereum_block_time = Duration::from_secs(12);
    let fresh = WalletSyncTip {
        last_scanned_block: Some(990),
        head_block: Some(1_012),
        safe_head_block: Some(1_000),
        head_last_advanced_at_unix_secs: Some(now - 30),
        indexed_catch_up: None,
    };

    assert_eq!(
        balance_stale_timeout(ethereum_block_time),
        Duration::from_mins(2)
    );
    assert_eq!(balance_lag_threshold_blocks(ethereum_block_time), 10);
    assert_eq!(
        balance_stale_timeout(Duration::from_millis(450)),
        Duration::from_secs(45)
    );
    assert_eq!(
        balance_lag_threshold_blocks(Duration::from_millis(450)),
        100
    );
    assert_eq!(balance_lag_threshold_blocks(Duration::from_secs(1)), 45);
    assert_eq!(
        balance_lag_threshold_blocks(Duration::from_millis(250)),
        180
    );
    assert_eq!(balance_lag_threshold_blocks(Duration::from_secs(2)), 22);
    assert_eq!(
        balance_sync_issue(Some(fresh), ethereum_block_time, now),
        None
    );
    assert_eq!(
        balances_presence_status(false, true, Some(fresh), ethereum_block_time, now),
        PresenceStatus::Healthy
    );

    let stalled = WalletSyncTip {
        head_last_advanced_at_unix_secs: Some(now - 121),
        ..fresh
    };
    assert_eq!(
        balance_sync_issue(Some(stalled), ethereum_block_time, now),
        Some(BalanceSyncIssue::HeadStalled {
            stale_secs: 121,
            threshold_secs: 120,
        })
    );
    assert_eq!(
        balances_presence_status(false, true, Some(stalled), ethereum_block_time, now),
        PresenceStatus::Active
    );

    let lagging = WalletSyncTip {
        last_scanned_block: Some(989),
        ..fresh
    };
    assert_eq!(
        balance_sync_issue(Some(lagging), ethereum_block_time, now),
        Some(BalanceSyncIssue::Lagging {
            lag_blocks: 11,
            threshold_blocks: 10,
        })
    );
    assert_eq!(
        balances_presence_status(false, true, Some(lagging), ethereum_block_time, now),
        PresenceStatus::Active
    );

    let indexed_catch_up = WalletSyncTip {
        indexed_catch_up: Some(WalletIndexedCatchUpStatus {
            source: WalletIndexedCatchUpSource::Squid,
            from_block: 990,
            target_block: 1_000,
        }),
        ..fresh
    };
    assert_eq!(
        balance_sync_issue(Some(indexed_catch_up), ethereum_block_time, now),
        None
    );
    assert_eq!(
        balances_presence_status(
            false,
            true,
            Some(indexed_catch_up),
            ethereum_block_time,
            now,
        ),
        PresenceStatus::Active
    );

    assert_eq!(
        balance_sync_issue(None, ethereum_block_time, now),
        Some(BalanceSyncIssue::HeadUnavailable)
    );
    assert_eq!(
        balances_presence_status(false, true, None, ethereum_block_time, now),
        PresenceStatus::Unknown
    );
    assert_eq!(
        balances_presence_status(true, false, None, ethereum_block_time, now),
        PresenceStatus::Active
    );
}

#[test]
fn balance_sync_issue_detail_suggests_network_remedies() {
    let lagging = BalanceSyncIssue::Lagging {
        lag_blocks: 186,
        threshold_blocks: 45,
    };
    let stalled = BalanceSyncIssue::HeadStalled {
        stale_secs: 60,
        threshold_secs: 45,
    };

    assert_eq!(
        balance_sync_issue_detail(lagging, WalletNetworkMode::Tor),
        "Wallet state is 186 safe-head blocks behind. Consider generating a new Tor session or using premium RPCs."
    );
    assert_eq!(
        balance_sync_issue_detail(lagging, WalletNetworkMode::Direct),
        "Wallet state is 186 safe-head blocks behind. Consider using premium RPCs."
    );
    assert_eq!(
        balance_sync_issue_detail(stalled, WalletNetworkMode::Proxy),
        "RPC head has not advanced for 1m. Consider using premium RPCs."
    );
    assert_eq!(
        balance_sync_issue_detail(BalanceSyncIssue::HeadUnavailable, WalletNetworkMode::Tor),
        "Waiting for chain head updates."
    );
}

#[test]
fn ready_wallet_status_labels_prioritize_actionable_private_attention() {
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts::default()),
        SyncStatusLabels {
            title: "Wallet ready".to_string(),
            percent: 100,
            detail: "Private wallet synced and ready".to_string(),
        }
    );
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts {
            blocked_shield_outputs: 1,
            recoverable_poi_outputs: 3,
            ..WalletStatusCounts::default()
        }),
        SyncStatusLabels {
            title: "Private assets need attention".to_string(),
            percent: 100,
            detail: "1 blocked Shield needs attention".to_string(),
        }
    );
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts {
            recoverable_poi_outputs: 3,
            ..WalletStatusCounts::default()
        }),
        SyncStatusLabels {
            title: "Not yet spendable".to_string(),
            percent: 100,
            detail: "3 PPOI verification retries are available".to_string(),
        }
    );
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts {
            pending_incoming_outputs: 1,
            pending_outgoing_outputs: 2,
            pending_poi_assets: 1,
            ..WalletStatusCounts::default()
        }),
        SyncStatusLabels {
            title: "Not yet spendable".to_string(),
            percent: 100,
            detail:
                "1 incoming transfer · 2 outgoing transfers · 1 asset awaiting PPOI verification"
                    .to_string(),
        }
    );
    assert_eq!(
        ready_wallet_status_labels(WalletStatusCounts {
            pending_outgoing_outputs: 2,
            ..WalletStatusCounts::default()
        }),
        SyncStatusLabels {
            title: "Waiting for confirmation".to_string(),
            percent: 100,
            detail: "2 outgoing transfers".to_string(),
        }
    );
}

#[test]
fn ready_wallet_status_text_is_hidden_for_all_ready_states() {
    assert!(!ready_wallet_status_shows_text(
        WalletStatusCounts::default()
    ));
    assert!(!ready_wallet_status_shows_text(WalletStatusCounts {
        pending_incoming_outputs: 1,
        ..WalletStatusCounts::default()
    }));
    assert!(!ready_wallet_status_shows_text(WalletStatusCounts {
        recoverable_poi_outputs: 1,
        ..WalletStatusCounts::default()
    }));
    assert!(!ready_wallet_status_shows_text(WalletStatusCounts {
        blocked_shield_outputs: 1,
        ..WalletStatusCounts::default()
    }));
}

#[test]
fn active_ppoi_hover_uses_exact_submission_copy() {
    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, None, true),
        "Submitting PPOIs…"
    );
    assert_eq!(
        ppoi_hover_detail(PresenceStatus::Active, None, true),
        Some("Submitting sender-created contexts and checking owned private-output PPOI status.")
    );
}
