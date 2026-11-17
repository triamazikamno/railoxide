use super::*;

fn activity_snapshot(generation: u64, downloaded_bytes: u64) -> TorBridgeActivitySnapshot {
    TorBridgeActivitySnapshot {
        generation,
        downloaded_bytes,
        connecting_streams: 0,
        active_streams: 0,
        successful_connections: 0,
        failed_connections: 0,
        recent_connection_sample_count: 0,
        recent_successful_sample_count: 0,
        median_setup_duration: None,
        last_activity_age: None,
        session_duration: Duration::ZERO,
    }
}

#[test]
fn reset_quits_only_after_the_marker_is_written() {
    let path = std::env::temp_dir().join(format!("network-reset-{:032x}", rand::random::<u128>()));
    let quits = std::cell::Cell::new(0);
    std::fs::write(&path, b"blocked directory").unwrap();
    assert!(reset_tor_state_and_quit(&path, || quits.set(quits.get() + 1)).is_err());
    assert_eq!(quits.get(), 0);
    std::fs::remove_file(&path).unwrap();
    reset_tor_state_and_quit(&path, || {
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 1);
        quits.set(quits.get() + 1);
    })
    .unwrap();
    assert_eq!(quits.get(), 1);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn download_rate_sampler_reports_first_sample_and_idle_zero() {
    let mut sampler = TorBridgeActivitySampler::new();
    let start = Instant::now();
    let first = activity_snapshot(1, 100);

    assert_eq!(sampler.sample(Some(&first), start), None);
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(1, 100)),
            start + Duration::from_secs(1)
        ),
        Some(0)
    );
}

#[test]
fn download_rate_sampler_smooths_weighted_intervals_and_clips_oldest() {
    let start = Instant::now();
    let mut sampler = TorBridgeActivitySampler::new();
    assert_eq!(sampler.sample(Some(&activity_snapshot(1, 0)), start), None);
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(1, 100)),
            start + Duration::from_secs(1)
        ),
        Some(100)
    );
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(1, 300)),
            start + Duration::from_secs(2)
        ),
        Some(150)
    );
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(1, 600)),
            start + Duration::from_secs(6)
        ),
        Some(100)
    );

    let mut partial_sampler = TorBridgeActivitySampler::new();
    assert_eq!(
        partial_sampler.sample(Some(&activity_snapshot(1, 0)), start),
        None
    );
    assert_eq!(
        partial_sampler.sample(
            Some(&activity_snapshot(1, 100)),
            start + Duration::from_secs(4),
        ),
        Some(25)
    );
    assert_eq!(
        partial_sampler.sample(
            Some(&activity_snapshot(1, 200)),
            start + Duration::from_secs(6),
        ),
        Some(35)
    );

    let mut long_sampler = TorBridgeActivitySampler::new();
    assert_eq!(
        long_sampler.sample(Some(&activity_snapshot(1, 0)), start),
        None
    );
    assert_eq!(
        long_sampler.sample(
            Some(&activity_snapshot(1, 1_000)),
            start + Duration::from_secs(10),
        ),
        Some(100)
    );
    assert!(long_sampler.intervals.len() <= TOR_ACTIVITY_INTERVAL_LIMIT);

    let mut bounded_sampler = TorBridgeActivitySampler::new();
    assert_eq!(
        bounded_sampler.sample(Some(&activity_snapshot(1, 0)), start),
        None
    );
    for index in 1..=32 {
        let _ = bounded_sampler.sample(
            Some(&activity_snapshot(1, index * 100)),
            start + Duration::from_secs(index),
        );
    }
    assert!(bounded_sampler.intervals.len() <= TOR_ACTIVITY_INTERVAL_LIMIT);
}

#[test]
fn download_rate_sampler_resets_on_missing_generation_and_rollback() {
    let start = Instant::now();
    let mut sampler = TorBridgeActivitySampler::new();
    assert_eq!(
        sampler.sample(Some(&activity_snapshot(1, 100)), start),
        None
    );
    assert_eq!(sampler.sample(None, start + Duration::from_secs(1)), None);
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(1, 200)),
            start + Duration::from_secs(2)
        ),
        None
    );
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(2, 0)),
            start + Duration::from_secs(3)
        ),
        None
    );
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(2, 20)),
            start + Duration::from_secs(4)
        ),
        Some(20)
    );
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(2, 10)),
            start + Duration::from_secs(5)
        ),
        None
    );
    assert_eq!(
        sampler.sample(
            Some(&activity_snapshot(2, 30)),
            start + Duration::from_secs(6)
        ),
        Some(20)
    );
}

#[test]
fn exit_ip_query_completion_requires_current_query_token() {
    let generation = next_tor_exit_ip_query_generation(7);
    let querying = TorExitIpQueryState::Querying;
    assert!(tor_exit_ip_query_completion_is_current(
        generation, generation, &querying
    ));
    assert!(!tor_exit_ip_query_completion_is_current(
        generation,
        generation - 1,
        &querying
    ));
    assert!(!tor_exit_ip_query_completion_is_current(
        generation,
        generation,
        &TorExitIpQueryState::Idle
    ));
    assert_eq!(next_tor_exit_ip_query_generation(u64::MAX), u64::MAX);
}

#[test]
fn tor_exit_ip_response_requires_a_valid_json_ip_field() {
    assert_eq!(
        parse_tor_exit_ip_response(r#"{"IsTor":false,"IP":"1.2.3.4"}"#).unwrap(),
        "1.2.3.4".parse::<IpAddr>().unwrap()
    );

    for response in ["not json", r#"{"IsTor":true}"#, r#"{"IP":"not-an-ip"}"#] {
        assert!(parse_tor_exit_ip_response(response).is_err());
    }
}
