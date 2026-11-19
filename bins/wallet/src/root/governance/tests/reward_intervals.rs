use super::*;

fn reward_metadata(
    staking_deploy_time: U256,
    distribution_interval: U256,
) -> GovernorRewardsIntervalMetadata {
    GovernorRewardsIntervalMetadata {
        multiplier: U256::ONE,
        staking_deploy_time,
        distribution_interval,
        current_interval: U256::ZERO,
        next_earmark_intervals: BTreeMap::new(),
    }
}

#[test]
fn reward_interval_countdown_uses_fourteen_day_boundaries() {
    let fourteen_days = U256::from(14 * 24 * 60 * 60);
    let metadata = reward_metadata(U256::from(100), fourteen_days);
    let countdown = reward_interval_countdown(
        &metadata,
        U256::from(100) + fourteen_days * U256::from(3) + U256::from(42),
    )
    .expect("deployed reward interval");

    assert_eq!(countdown.interval, U256::from(3));
    assert_eq!(
        countdown.boundary,
        U256::from(100) + fourteen_days * U256::from(4)
    );
    assert_eq!(
        remaining_reward_interval_seconds(
            countdown,
            U256::from(100) + fourteen_days * U256::from(3) + U256::from(42),
        ),
        Some(fourteen_days.to::<u64>() - 42)
    );
}

#[test]
fn reward_interval_countdown_rejects_zero_interval_and_predeployment_time() {
    let zero_interval = reward_metadata(U256::from(100), U256::ZERO);
    assert!(reward_interval_countdown(&zero_interval, U256::from(100)).is_none());

    let metadata = reward_metadata(U256::from(100), U256::from(14));
    assert!(reward_interval_countdown(&metadata, U256::from(99)).is_none());
}

#[test]
fn reward_interval_countdown_rejects_checked_arithmetic_overflow() {
    let increment_overflow = reward_metadata(U256::ZERO, U256::ONE);
    assert!(reward_interval_countdown(&increment_overflow, U256::MAX).is_none());

    let multiplication_overflow = reward_metadata(U256::ZERO, U256::MAX);
    assert!(reward_interval_countdown(&multiplication_overflow, U256::MAX).is_none());

    let addition_overflow = reward_metadata(U256::MAX, U256::ONE);
    assert!(reward_interval_countdown(&addition_overflow, U256::MAX).is_none());
}

#[test]
fn governance_boundary_crossing_uses_the_stored_reward_boundary() {
    let metadata = reward_metadata(U256::from(10), U256::from(5));
    let countdown =
        reward_interval_countdown(&metadata, U256::from(20)).expect("deployed reward interval");
    let state = StakingReadState {
        reward_interval_countdown: Some(countdown),
        ..StakingReadState::default()
    };

    assert!(!governance_boundary_crossed(&state, U256::from(24)));
    assert!(governance_boundary_crossed(&state, U256::from(25)));
}

#[test]
fn reward_interval_state_is_generation_scoped_and_cleared_on_refresh() {
    let address = Address::from([1; 20]);
    let key = key("wallet", 1, "a", address);
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    let countdown = RewardIntervalCountdown {
        interval: U256::ZERO,
        boundary: U256::ONE,
    };
    assert!(state.apply_reward_interval(&key, generation, Some(countdown)));

    let next_generation = state.begin(key.clone());
    assert_ne!(generation, next_generation);
    assert_eq!(state.reward_interval_countdown, None);
    assert!(!state.apply_reward_interval(&key, generation, Some(countdown)));
    assert!(state.apply_reward_interval(&key, next_generation, Some(countdown)));
    state.invalidate();
    assert_eq!(state.reward_interval_countdown, None);
}

#[test]
fn governance_time_tick_new_generation_supersedes_previous_owner() {
    let (owner, armed) = arm_governance_time_tick(None, 10);
    assert_eq!(owner, Some(10));
    assert!(armed);

    let (owner, armed) = arm_governance_time_tick(owner, 10);
    assert_eq!(owner, Some(10));
    assert!(!armed);

    let (owner, armed) = arm_governance_time_tick(owner, 11);
    assert_eq!(owner, Some(11));
    assert!(armed);
}

#[test]
fn stale_governance_time_tick_claim_leaves_new_owner_intact() {
    let (owner, claimed) = claim_governance_time_tick(Some(11), 10);
    assert_eq!(owner, Some(11));
    assert!(!claimed);
}

#[test]
fn governance_time_tick_owner_claims_only_once() {
    let (owner, claimed) = claim_governance_time_tick(Some(11), 11);
    assert_eq!(owner, None);
    assert!(claimed);

    let (owner, claimed) = claim_governance_time_tick(owner, 11);
    assert_eq!(owner, None);
    assert!(!claimed);
}

#[test]
fn non_crossed_governance_time_tick_claim_can_rearm_generation() {
    let (owner, claimed) = claim_governance_time_tick(Some(11), 11);
    assert!(claimed);
    assert_eq!(owner, None);

    let (owner, armed) = arm_governance_time_tick(owner, 11);
    assert_eq!(owner, Some(11));
    assert!(armed);
}

#[test]
fn unlock_boundary_is_checked_at_equality_and_ignores_terminal_history() {
    let owner = Address::from([4; 20]);
    let mut state = StakingReadState::default();
    state.accounts.insert(
        "a".into(),
        StakingAccountView {
            voting_power: Ok(U256::ZERO),
            balance: Ok(U256::ZERO),
            stakes: Ok(vec![
                StakePosition {
                    owner,
                    id: U256::ZERO,
                    delegate: owner,
                    amount: U256::from(1),
                    staketime: U256::ZERO,
                    locktime: U256::from(20),
                    claimed_time: U256::ZERO,
                    state: StakeState::Unlocking,
                },
                StakePosition {
                    owner,
                    id: U256::from(1),
                    delegate: owner,
                    amount: U256::from(1),
                    staketime: U256::ZERO,
                    locktime: U256::from(10),
                    claimed_time: U256::from(11),
                    state: StakeState::Claimed,
                },
            ]),
        },
    );
    assert_eq!(nearest_unlock_boundary(&state), Some(U256::from(20)));
    assert!(!governance_boundary_crossed(&state, U256::from(19)));
    assert!(governance_boundary_crossed(&state, U256::from(20)));
    state.invalidate();
    assert!(!governance_boundary_crossed(&state, U256::from(20)));
}
