use super::*;

#[test]
fn stake_states_use_strict_chain_time_and_claimed_precedence() {
    assert_eq!(
        classify_stake_state(U256::ZERO, U256::ZERO, U256::ZERO),
        StakeState::Active
    );
    assert_eq!(
        classify_stake_state(U256::from(10), U256::ZERO, U256::from(10)),
        StakeState::Unlocking
    );
    assert_eq!(
        classify_stake_state(U256::from(10), U256::ZERO, U256::from(11)),
        StakeState::Claimable
    );
    assert_eq!(
        classify_stake_state(U256::ZERO, U256::from(1), U256::ZERO),
        StakeState::Claimed
    );
}

#[test]
fn snapshot_hint_covers_boundaries_and_rejects_unordered() {
    let snapshots = vec![
        AccountSnapshot {
            interval: U256::from(2),
            voting_power: U256::ZERO,
        },
        AccountSnapshot {
            interval: U256::from(5),
            voting_power: U256::ZERO,
        },
    ];
    assert_eq!(snapshot_hint(&snapshots, U256::ZERO).unwrap(), U256::ZERO);
    assert_eq!(
        snapshot_hint(&snapshots, U256::from(2)).unwrap(),
        U256::ZERO
    );
    assert_eq!(
        snapshot_hint(&snapshots, U256::from(3)).unwrap(),
        U256::from(1)
    );
    assert_eq!(
        snapshot_hint(&snapshots, U256::from(5)).unwrap(),
        U256::from(1)
    );
    assert_eq!(
        snapshot_hint(&snapshots, U256::from(8)).unwrap(),
        U256::from(2)
    );
    let unordered = vec![snapshots[1].clone(), snapshots[0].clone()];
    assert!(snapshot_hint(&unordered, U256::ZERO).is_err());
    assert_eq!(snapshot_hint(&[], U256::ZERO).unwrap(), U256::ZERO);
    assert!(reward_staking_interval(U256::MAX, U256::from(2)).is_err());
}

#[test]
fn chunk_planning_covers_every_index_without_a_silent_cap() {
    let chunks = chunk_indices(7, MulticallChunkSize::new(NonZeroUsize::new(3).unwrap()));
    assert_eq!(chunks, vec![vec![0, 1, 2], vec![3, 4, 5], vec![6]]);
    assert_eq!(
        chunks.iter().flatten().copied().collect::<Vec<_>>(),
        (0..7).collect::<Vec<_>>()
    );
    assert_eq!(
        chunk_indices(0, MulticallChunkSize::default()),
        Vec::<Vec<usize>>::new()
    );
    let failed = AccountStakeResult {
        account: Address::from([3_u8; 20]),
        voting_power: Err("count failed".into()),
        balance: Err("balance failed".into()),
        stakes: Err("count failed".into()),
    };
    let healthy = AccountStakeResult {
        account: Address::from([4_u8; 20]),
        voting_power: Ok(U256::ZERO),
        balance: Ok(U256::ZERO),
        stakes: Ok(Vec::new()),
    };
    assert!(failed.stakes.is_err());
    assert!(healthy.stakes.is_ok());
}

#[test]
fn stake_record_abi_decoding_is_strict() {
    let owner = Address::from([1_u8; 20]);
    let delegate = Address::from([2_u8; 20]);
    let mut encoded = Vec::with_capacity(160);
    encoded.extend([0_u8; 12]);
    encoded.extend(delegate.as_slice());
    for value in [U256::from(9), U256::from(10), U256::from(11), U256::ZERO] {
        encoded.extend(value.to_be_bytes::<32>());
    }
    let position = decode_stake_position(owner, U256::ZERO, U256::from(20), &encoded).unwrap();
    assert_eq!(position.delegate, delegate);
    assert_eq!(position.amount, U256::from(9));
    assert_eq!(position.state, StakeState::Claimable);
    assert!(decode_stake_position(owner, U256::ZERO, U256::ZERO, &encoded[..159]).is_err());
}

#[test]
fn global_metrics_validate_checked_invariants() {
    let metrics = StakingGlobalMetrics {
        total_staked: U256::ZERO,
        total_voting_power: U256::ZERO,
        deploy_time: U256::from(10),
        snapshot_interval: U256::from(5),
        current_interval: U256::from(2),
        stake_locktime: U256::ZERO,
        chain_time: U256::from(20),
    };
    assert!(metrics.validate().is_ok());
    let mut invalid = metrics.clone();
    invalid.snapshot_interval = U256::ZERO;
    assert!(invalid.validate().is_err());
    let mut inconsistent = metrics.clone();
    inconsistent.current_interval = U256::from(3);
    assert!(inconsistent.validate().is_err());
    let before_deploy = StakingGlobalMetrics {
        chain_time: U256::from(9),
        ..metrics
    };
    assert!(before_deploy.validate().is_err());
}

#[test]
fn deployment_relationships_fail_closed_by_relationship() {
    let token = Address::from([1_u8; 20]);
    let staking = Address::from([2_u8; 20]);
    assert!(validate_deployment_relationships(token, token, staking, staking).is_ok());
    assert_eq!(
        validate_deployment_relationships(token, Address::ZERO, staking, staking),
        Err(DeploymentValidationError::StakingTokenMismatch {
            expected: token,
            actual: Address::ZERO
        })
    );
    assert_eq!(
        validate_deployment_relationships(token, token, staking, Address::ZERO),
        Err(DeploymentValidationError::GovernorRewardsStakingMismatch {
            expected: staking,
            actual: Address::ZERO
        })
    );
}
