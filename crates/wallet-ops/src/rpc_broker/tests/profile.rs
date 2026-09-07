use super::*;

#[test]
fn health_cooldown_escalates_without_resetting() {
    let mut profile = EndpointProfile::default();
    let now = Instant::now();
    for _ in 0..HEALTH_STRIKE_THRESHOLD.saturating_sub(1) {
        assert!(!profile.record_failure(now));
    }
    assert!(profile.record_failure(now));
    assert!(profile.is_withdrawn(now));
    let first = profile.withdrawn_until.expect("withdrawal");
    assert!(profile.restore_if_ready(first));
    for _ in 0..HEALTH_STRIKE_THRESHOLD.saturating_sub(1) {
        assert!(!profile.record_failure(first));
    }
    assert!(profile.record_failure(first));
    let second = profile.withdrawn_until.expect("second withdrawal");
    assert!(second > first);
    assert!(second - first > HEALTH_WITHDRAWAL_BASE);
}
