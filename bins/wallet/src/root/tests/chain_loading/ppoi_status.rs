use super::*;

#[test]
fn wallet_status_presence_classifies_sync_and_ppoi_health() {
    let ppoi_attention = WalletStatusCounts {
        recoverable_poi_outputs: 2,
        ..WalletStatusCounts::default()
    };
    let blocked_shield_attention = WalletStatusCounts {
        blocked_shield_outputs: 1,
        ..WalletStatusCounts::default()
    };
    let automatic_recovery = WalletStatusCounts {
        recoverable_poi_outputs: 1,
        ppoi_workflow_status: WalletPpoiWorkflowStatus {
            awaiting_recovery: 2,
            ..WalletPpoiWorkflowStatus::default()
        },
        ..WalletStatusCounts::default()
    };

    assert_eq!(
        ppoi_presence_status(true, true, false, None, WalletStatusCounts::default()),
        PresenceStatus::Active
    );
    assert_eq!(
        ppoi_presence_status(false, true, false, None, WalletStatusCounts::default()),
        PresenceStatus::Healthy
    );
    assert_eq!(
        ppoi_presence_status(false, false, false, None, WalletStatusCounts::default()),
        PresenceStatus::Unknown
    );
    assert_eq!(
        ppoi_presence_status(false, true, true, None, WalletStatusCounts::default()),
        PresenceStatus::Unknown
    );

    let attempt_id = poi_artifact_attempt_id(1);
    let active_cache =
        poi_artifact_progress(attempt_id, PoiArtifactCachePhase::ReplayingRanges, false);
    let ready_cache = poi_artifact_progress(attempt_id, PoiArtifactCachePhase::Ready, true);
    let usable_error = poi_artifact_progress(attempt_id, PoiArtifactCachePhase::Failed, true);
    let blocking_error = poi_artifact_progress(attempt_id, PoiArtifactCachePhase::Failed, false);
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&active_cache),
            WalletStatusCounts::default()
        ),
        PresenceStatus::Active
    );
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&ready_cache),
            WalletStatusCounts::default()
        ),
        PresenceStatus::Healthy
    );
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&usable_error),
            WalletStatusCounts::default()
        ),
        PresenceStatus::Active
    );
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&blocking_error),
            WalletStatusCounts {
                pending_poi_assets: 1,
                ..WalletStatusCounts::default()
            }
        ),
        PresenceStatus::Error
    );
    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&blocking_error),
            WalletStatusCounts {
                blocked_shield_outputs: 1,
                ..WalletStatusCounts::default()
            }
        ),
        PresenceStatus::Active
    );
    assert_eq!(ppoi_attention.ppoi_attention_count(), 2);
    assert_eq!(blocked_shield_attention.ppoi_attention_count(), 1);
    assert_eq!(automatic_recovery.ppoi_attention_count(), 1);
    assert_eq!(automatic_recovery.ppoi_status_count(), 3);
    assert_eq!(
        ppoi_presence_status(false, true, false, None, automatic_recovery),
        PresenceStatus::Active
    );
    let distinct_attention = WalletStatusCounts {
        recoverable_poi_outputs: 1,
        ppoi_workflow_status: WalletPpoiWorkflowStatus {
            needs_attention: 1,
            ..WalletPpoiWorkflowStatus::default()
        },
        ..WalletStatusCounts::default()
    };
    assert_eq!(distinct_attention.ppoi_attention_count(), 2);
    assert_eq!(distinct_attention.ppoi_status_count(), 2);
    let candidate_attention = WalletStatusCounts {
        ppoi_workflow_status: WalletPpoiWorkflowStatus {
            awaiting_recovery: 1,
            recovery_needs_attention: 1,
            ..WalletPpoiWorkflowStatus::default()
        },
        ..WalletStatusCounts::default()
    };
    assert_eq!(candidate_attention.ppoi_attention_count(), 1);
    assert_eq!(
        ppoi_presence_status(false, true, false, None, candidate_attention),
        PresenceStatus::Error
    );
}

#[test]
fn ppoi_retry_completion_rejects_stale_wallet_or_session() {
    assert!(ppoi_retry_completion_is_current(7, 7, true));
    assert!(!ppoi_retry_completion_is_current(8, 7, true));
    assert!(!ppoi_retry_completion_is_current(7, 7, false));
}

#[test]
fn cold_ppoi_download_reports_chunk_and_byte_progress_without_readiness() {
    let mut progress = poi_artifact_progress(
        poi_artifact_attempt_id(2),
        PoiArtifactCachePhase::DownloadingChunks,
        false,
    );
    progress.graph = PoiArtifactCacheGraphProgress {
        verified_chunks: 3,
        total_chunks: 10,
        verified_encoded_bytes: 12 * 1024 * 1024,
        total_authenticated_encoded_bytes: Some(31 * 1024 * 1024),
        ..PoiArtifactCacheGraphProgress::default()
    };

    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, Some(&progress), false),
        "Downloading POI chunks"
    );
    assert_eq!(
        ppoi_chunk_progress_label(&progress).as_deref(),
        Some("3/10 chunks · 12 MiB/31 MiB")
    );
    assert!(!progress.ready_for_wallet_checks);
}

#[test]
fn warm_ppoi_refresh_keeps_local_corpus_readiness_visible() {
    let progress = poi_artifact_progress(
        poi_artifact_attempt_id(3),
        PoiArtifactCachePhase::Planning,
        true,
    );

    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, Some(&progress), false),
        "Refreshing PPOI data"
    );
    assert_eq!(
        ppoi_hover_detail(PresenceStatus::Active, Some(&progress), false),
        Some(
            "Refreshing in the background while wallet checks use verified PPOI data saved on this device."
        )
    );
}

#[test]
fn retained_bridge_replay_reports_authenticated_event_range() {
    let mut progress = poi_artifact_progress(
        poi_artifact_attempt_id(4),
        PoiArtifactCachePhase::ReplayingRanges,
        true,
    );
    progress.graph = PoiArtifactCacheGraphProgress {
        replay_start_event_index: Some(32_768),
        replay_end_event_index: Some(33_023),
        replayed_event_count: 128,
        total_replay_event_count: 256,
        ..PoiArtifactCacheGraphProgress::default()
    };

    assert_eq!(
        ppoi_replay_progress_label(&progress).as_deref(),
        Some("Events 32768-33023 · 128/256 replayed")
    );
    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, Some(&progress), false),
        "Refreshing PPOI data"
    );
}

#[test]
fn failed_ppoi_refresh_keeps_serving_corpus_available() {
    let mut progress = poi_artifact_progress(
        poi_artifact_attempt_id(5),
        PoiArtifactCachePhase::Failed,
        true,
    );
    progress.last_error = Some("catalog unavailable".to_string());

    assert_eq!(
        ppoi_presence_status(
            false,
            true,
            true,
            Some(&progress),
            WalletStatusCounts::default()
        ),
        PresenceStatus::Active
    );
    assert_eq!(
        ppoi_hover_heading(PresenceStatus::Active, Some(&progress), false),
        "Artifact cache refresh failed"
    );
    assert_eq!(
        ppoi_hover_detail(PresenceStatus::Active, Some(&progress), false),
        Some(
            "Refresh failed; wallet checks continue using verified PPOI data saved on this device."
        )
    );
}

#[test]
fn official_ppoi_artifact_runtime_has_no_legacy_or_proxy_fallback() {
    let settings = wallet_ops::settings::WalletSettings::default();
    let wallet_ops::PoiReadSource::IndexedArtifacts {
        artifact_source,
        wallet_read_fallback,
        ..
    } = settings
        .poi_read_source()
        .expect("official PPOI artifact runtime")
    else {
        panic!("official PPOI artifact runtime should use indexed artifacts");
    };

    assert!(matches!(
        artifact_source.manifest_source,
        wallet_ops::PoiArtifactManifestSource::IpnsName(ref name)
            if name == wallet_ops::settings::OFFICIAL_POI_ARTIFACT_IPNS_NAME
                && name != wallet_ops::settings::LEGACY_OFFICIAL_POI_ARTIFACT_IPNS_NAME
    ));
    assert_eq!(wallet_read_fallback, wallet_ops::PoiProxyFallback::Disabled);
}

#[test]
fn ppoi_retry_pending_admission_owns_chain() {
    let mut attempts = PoiArtifactCacheRetryAttempts::default();
    let request_token = attempts.begin(1).expect("start retry request");

    assert!(attempts.contains(1));
    assert!(attempts.begin(1).is_none());
    assert!(attempts.cancel_pending(1, request_token));
    assert!(!attempts.contains(1));
}

#[test]
fn ppoi_retry_admission_binds_only_matching_provisional_request() {
    let mut attempts = PoiArtifactCacheRetryAttempts::default();
    let stale_request = attempts.begin(1).expect("start stale retry request");
    attempts.clear();
    let replacement_request = attempts.begin(1).expect("start replacement retry request");
    let stale_attempt_id = poi_artifact_attempt_id(10);
    let replacement_attempt_id = poi_artifact_attempt_id(11);

    assert_ne!(stale_request, replacement_request);
    assert!(!attempts.bind(1, stale_request, stale_attempt_id));
    assert!(attempts.contains(1));
    assert!(attempts.bind(1, replacement_request, replacement_attempt_id));
    assert!(!attempts.cancel_pending(1, replacement_request));
}

#[test]
fn ppoi_retry_exact_core_completion_releases_ownership() {
    let mut attempts = PoiArtifactCacheRetryAttempts::default();
    let request_token = attempts.begin(1).expect("start retry request");
    let attempt_id = poi_artifact_attempt_id(20);
    assert!(attempts.bind(1, request_token, attempt_id));

    assert!(attempts.finish(1, attempt_id));
    assert!(!attempts.contains(1));
}

#[test]
fn ppoi_retry_stale_completion_does_not_clear_replacement() {
    let mut attempts = PoiArtifactCacheRetryAttempts::default();
    let stale_request = attempts.begin(1).expect("start stale retry request");
    let stale_attempt_id = poi_artifact_attempt_id(30);
    assert!(attempts.bind(1, stale_request, stale_attempt_id));
    attempts.clear();
    let replacement_request = attempts.begin(1).expect("start replacement retry request");
    let replacement_attempt_id = poi_artifact_attempt_id(31);
    assert!(attempts.bind(1, replacement_request, replacement_attempt_id));

    assert!(!attempts.finish(1, stale_attempt_id));
    assert!(attempts.contains(1));
    assert!(attempts.finish(1, replacement_attempt_id));
}
