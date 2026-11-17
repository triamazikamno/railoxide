use super::*;

#[test]
fn batch_evidence_projects_columns_onto_a_token_subset() {
    let first = Address::from([1; 20]);
    let second = Address::from([2; 20]);
    let third = Address::from([3; 20]);
    let evidence = RewardBatchEvidence {
        reward_tokens: vec![first, second, third],
        starting_interval: U256::from(2),
        ending_interval: U256::from(3),
        staking_intervals: vec![U256::from(7), U256::from(8)],
        hints: vec![U256::ZERO, U256::ONE],
        claimed_intervals: vec![vec![U256::from(2)], Vec::new(), vec![U256::from(3)]],
        expected_amounts: vec![U256::from(10), U256::from(20), U256::from(30)],
    };

    let projected = evidence.project(&[first, third]).unwrap();
    assert_eq!(projected.reward_tokens, vec![first, third]);
    assert_eq!(
        projected.expected_amounts,
        vec![U256::from(10), U256::from(30)]
    );
    assert_eq!(
        projected.claimed_intervals,
        vec![vec![U256::from(2)], vec![U256::from(3)]]
    );
    assert_eq!(projected.starting_interval, evidence.starting_interval);
    assert_eq!(projected.ending_interval, evidence.ending_interval);
    assert_eq!(projected.staking_intervals, evidence.staking_intervals);
    assert_eq!(projected.hints, evidence.hints);
    assert_eq!(evidence.project(&evidence.reward_tokens).unwrap(), evidence);
    assert!(evidence.project(&[first, Address::from([4; 20])]).is_err());
    assert!(evidence.project(&[]).is_err());
    assert!(evidence.project(&[third, first]).is_err());
}

#[test]
fn evidence_excludes_claimed_and_unearmarked_ranges() {
    let snapshots = vec![
        AccountSnapshot {
            interval: U256::from(1390),
            voting_power: U256::ZERO,
        },
        AccountSnapshot {
            interval: U256::from(1400),
            voting_power: U256::ZERO,
        },
        AccountSnapshot {
            interval: U256::from(1442),
            voting_power: U256::from(12),
        },
        AccountSnapshot {
            interval: U256::from(1500),
            voting_power: U256::ZERO,
        },
    ];
    let evidence = reward_evidence(
        Address::ZERO,
        U256::from(105),
        &[
            (U256::from(101), true),
            (U256::from(102), false),
            (U256::from(103), true),
            (U256::from(104), false),
        ],
        &snapshots,
        U256::from(14),
        U256::ZERO,
    )
    .unwrap()
    .unwrap();
    assert_eq!(evidence.starting_interval, U256::from(102));
    assert_eq!(evidence.ending_interval, U256::from(104));
    assert_eq!(evidence.claimed_intervals, vec![U256::from(103)]);
    assert_eq!(
        evidence.staking_intervals,
        vec![U256::from(1428), U256::from(1442), U256::from(1456)]
    );
    assert_eq!(
        evidence.hints,
        vec![U256::from(2), U256::from(2), U256::from(3)]
    );
    for next in [0, 100, 101] {
        assert!(
            reward_evidence(
                Address::ZERO,
                U256::from(next),
                &[],
                &snapshots,
                U256::from(14),
                U256::ZERO
            )
            .unwrap()
            .is_none()
        );
    }
    assert!(
        reward_evidence(
            Address::ZERO,
            U256::from(103),
            &[(U256::from(101), true), (U256::from(102), true)],
            &snapshots,
            U256::from(14),
            U256::ZERO
        )
        .unwrap()
        .is_none()
    );
    assert!(
        reward_evidence(
            Address::ZERO,
            U256::from(103),
            &[(U256::from(101), false), (U256::from(103), false)],
            &snapshots,
            U256::from(14),
            U256::ZERO
        )
        .is_err()
    );
}

#[test]
fn evidence_preserves_first_eligible_snapshot_boundary() {
    for (history, first_interval) in [
        (vec![], 0),
        (vec![(14, 5), (28, 0)], 0),
        (vec![(13, 0)], 1),
        (vec![(14, 0)], 2),
        (vec![(15, 0)], 2),
    ] {
        let snapshots = history
            .into_iter()
            .map(|(interval, power)| AccountSnapshot {
                interval: U256::from(interval),
                voting_power: U256::from(power),
            })
            .collect::<Vec<_>>();
        let first = U256::from(first_interval);
        let evidence = reward_evidence(
            Address::ZERO,
            first + U256::ONE,
            &[(first, false)],
            &snapshots,
            U256::from(14),
            U256::ONE,
        )
        .unwrap()
        .unwrap();
        assert_eq!(evidence.starting_interval, first);
        assert_eq!(evidence.ending_interval, first);
    }
}

#[tokio::test]
async fn reward_reads_skip_ineligible_history_and_advance_after_claims() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    let tokens = [Address::from([1; 20]), Address::from([2; 20])];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let claimed_all = Arc::new(AtomicBool::new(false));
    let (endpoint, server) = crate::rpc_broker::tests::spawn_rpc_mock(
        {
            let calls = calls.clone();
            let claimed_all = claimed_all.clone();
            Arc::new(move |request| {
                assert_eq!(request["method"], "eth_call");
                let input = request["params"][0]
                    .get("input")
                    .or_else(|| request["params"][0].get("data"))
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .parse::<Bytes>()
                    .unwrap();
                let after_claim = claimed_all.load(Ordering::SeqCst);
                let output = if input.starts_with(&GovernorRewards::getClaimedCall::SELECTOR) {
                    let call = GovernorRewards::getClaimedCall::abi_decode(&input).unwrap();
                    calls.lock().unwrap().push((call.token, call.interval));
                    let interval = u64::try_from(call.interval).unwrap();
                    let claimed = if after_claim {
                        (101..=104).contains(&interval)
                    } else if call.token == tokens[0] {
                        matches!(interval, 101 | 103)
                    } else {
                        matches!(interval, 101 | 102)
                    };
                    GovernorRewards::getClaimedCall::abi_encode_returns(&claimed)
                } else {
                    let call = GovernorRewards::calculateRewardsCall::abi_decode(&input).unwrap();
                    let (start, end, hints) = if after_claim {
                        (105, 105, vec![2])
                    } else if call.tokens == [tokens[1]] {
                        (103, 104, vec![1, 2])
                    } else {
                        (102, 104, vec![1, 1, 2])
                    };
                    assert_eq!(call.startingInterval, U256::from(start));
                    assert_eq!(call.endingInterval, U256::from(end));
                    assert_eq!(
                        call.hints,
                        hints.into_iter().map(U256::from).collect::<Vec<_>>()
                    );
                    assert!(call.ignoreClaimed);
                    GovernorRewards::calculateRewardsCall::abi_encode_returns(&vec![
                        U256::ONE;
                        call.tokens
                            .len()
                    ])
                };
                json!({"jsonrpc": "2.0", "id": request["id"], "result": Bytes::from(output)})
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let route = crate::RpcChainRoute::new(1, vec![endpoint])
        .for_class(crate::RpcTrafficClass::Sensitive)
        .unwrap();
    let http = HttpContext::direct_for_tests();
    let snapshots = [
        AccountSnapshot {
            interval: U256::from(1400),
            voting_power: U256::ZERO,
        },
        AccountSnapshot {
            interval: U256::from(1442),
            voting_power: U256::from(12),
        },
    ];
    // No eligible completed interval, mixed per-token claims, then a refresh after claiming.
    for next in [101, 105, 106] {
        claimed_all.store(next == 106, Ordering::SeqCst);
        let metadata = GovernorRewardsIntervalMetadata {
            multiplier: U256::from(14),
            staking_deploy_time: U256::ZERO,
            distribution_interval: U256::from(14 * 86400),
            current_interval: U256::from(next),
            next_earmark_intervals: tokens
                .iter()
                .map(|&token| (token, U256::from(next)))
                .collect(),
        };
        for batch in [false, true] {
            calls.lock().unwrap().clear();
            // Small chunks exercise offsets across more than one broker submission.
            let chunk_size = MulticallChunkSize::new(std::num::NonZeroUsize::new(3).unwrap());
            if batch {
                let evidence = fetch_reward_batch_evidence(
                    1,
                    Address::ZERO,
                    &tokens,
                    &metadata,
                    &snapshots,
                    &route,
                    &http,
                    chunk_size,
                )
                .await
                .unwrap();
                if next == 101 {
                    assert!(evidence.is_none());
                } else {
                    let evidence = evidence.unwrap();
                    assert_eq!(
                        evidence.starting_interval,
                        U256::from(if next == 105 { 102 } else { 105 })
                    );
                    assert_eq!(evidence.ending_interval, U256::from(next - 1));
                    assert_eq!(
                        evidence.claimed_intervals,
                        if next == 105 {
                            vec![vec![U256::from(103)], vec![U256::from(102)]]
                        } else {
                            vec![vec![], vec![]]
                        }
                    );
                }
            } else {
                let evidence = fetch_reward_evidence_multi(
                    1,
                    Address::ZERO,
                    &tokens,
                    &metadata,
                    &snapshots,
                    &route,
                    &http,
                    chunk_size,
                )
                .await
                .unwrap();
                for (index, result) in evidence.into_iter().enumerate() {
                    let evidence = result.evidence.unwrap();
                    if next == 101 {
                        assert!(evidence.is_none());
                    } else {
                        let evidence = evidence.unwrap();
                        let start = if next == 105 { 102 + index } else { 105 };
                        assert_eq!(evidence.starting_interval, U256::from(start));
                        assert_eq!(evidence.ending_interval, U256::from(next - 1));
                        assert_eq!(
                            evidence.claimed_intervals,
                            if next == 105 && index == 0 {
                                vec![U256::from(103)]
                            } else {
                                vec![]
                            }
                        );
                    }
                }
            }
            let mut observed = calls.lock().unwrap().clone();
            observed.sort_unstable();
            let expected = tokens
                .iter()
                .flat_map(|&token| (101..next).map(move |interval| (token, U256::from(interval))))
                .collect::<Vec<_>>();
            assert_eq!(observed, expected);
        }
    }
    server.abort();
}

#[test]
fn claimed_flag_decode_is_strict() {
    let valid = GovernorRewards::getClaimedCall::abi_encode_returns(&true);
    assert!(decode_claimed_flag(&valid).unwrap());
    assert!(decode_claimed_flag(&valid[..31]).is_err());
}

#[test]
fn claimed_interval_comparison_rejects_mismatch_and_malformed_shape() {
    let token = Address::from([1; 20]);
    let evidence = RewardBatchEvidence {
        reward_tokens: vec![token],
        starting_interval: U256::from(2),
        ending_interval: U256::from(3),
        staking_intervals: vec![U256::ZERO; 2],
        hints: vec![U256::ZERO; 2],
        claimed_intervals: vec![vec![U256::from(3)]],
        expected_amounts: vec![U256::ONE],
    };
    assert!(reward_batch_claimed_intervals_match(&evidence, &[vec![U256::from(3)]]).unwrap());
    assert!(!reward_batch_claimed_intervals_match(&evidence, &[vec![U256::from(2)]]).unwrap());
    assert!(reward_batch_claimed_intervals_match(&evidence, &[]).is_err());
    assert!(reward_batch_claimed_intervals_match(&evidence, &[vec![U256::from(4)]]).is_err());
}

#[test]
fn authorization_comparison_requires_claimed_and_calculated_amounts() {
    let token = Address::from([1; 20]);
    let evidence = RewardBatchEvidence {
        reward_tokens: vec![token],
        starting_interval: U256::from(2),
        ending_interval: U256::from(3),
        staking_intervals: vec![U256::ZERO; 2],
        hints: vec![U256::ZERO; 2],
        claimed_intervals: vec![vec![U256::from(3)]],
        expected_amounts: vec![U256::ONE],
    };
    let unchanged = RewardBatchAuthorizationState {
        claimed_intervals: vec![vec![U256::from(3)]],
        amounts: vec![U256::ONE],
    };
    assert!(reward_batch_authorization_state_match(&evidence, &unchanged).unwrap());
    assert!(
        !reward_batch_authorization_state_match(
            &evidence,
            &RewardBatchAuthorizationState {
                claimed_intervals: vec![vec![U256::from(2)]],
                amounts: vec![U256::ONE],
            }
        )
        .unwrap()
    );
    assert!(
        !reward_batch_authorization_state_match(
            &evidence,
            &RewardBatchAuthorizationState {
                claimed_intervals: vec![vec![U256::from(3)]],
                amounts: vec![U256::from(2)],
            }
        )
        .unwrap()
    );
    assert!(
        reward_batch_authorization_state_match(
            &evidence,
            &RewardBatchAuthorizationState {
                claimed_intervals: vec![vec![U256::from(3)]],
                amounts: Vec::new(),
            }
        )
        .is_err()
    );
    assert!(
        reward_batch_authorization_state_match(
            &RewardBatchEvidence {
                expected_amounts: Vec::new(),
                ..evidence
            },
            &unchanged
        )
        .is_err()
    );
}

#[test]
fn planner_covers_consecutive_ranges_without_empty_steps() {
    let entries = vec![
        RewardIntervalSubtotal {
            interval: U256::ZERO,
            subtotal: U256::from(2),
            gas: 5,
        },
        RewardIntervalSubtotal {
            interval: U256::from(1),
            subtotal: U256::from(3),
            gas: 5,
        },
        RewardIntervalSubtotal {
            interval: U256::from(2),
            subtotal: U256::from(4),
            gas: 5,
        },
    ];
    let steps = plan_reward_claim_steps(&entries, 10).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(
        (
            steps[0].starting_interval,
            steps[0].ending_interval,
            steps[0].subtotal
        ),
        (U256::ZERO, U256::from(1), U256::from(5))
    );
    assert_eq!(
        (
            steps[1].starting_interval,
            steps[1].ending_interval,
            steps[1].subtotal
        ),
        (U256::from(2), U256::from(2), U256::from(4))
    );
    assert!(plan_reward_claim_steps(&entries[..2], 0).is_err());
    let gap = [
        entries[0].clone(),
        RewardIntervalSubtotal {
            interval: U256::from(2),
            ..entries[1].clone()
        },
    ];
    assert!(plan_reward_claim_steps(&gap, 10).is_err());
    let over_ceiling = [RewardIntervalSubtotal {
        interval: U256::ZERO,
        subtotal: U256::ZERO,
        gas: 11,
    }];
    assert!(plan_reward_claim_steps(&over_ceiling, 10).is_err());
    let overflow = [
        RewardIntervalSubtotal {
            interval: U256::ZERO,
            subtotal: U256::MAX,
            gas: 1,
        },
        RewardIntervalSubtotal {
            interval: U256::from(1),
            subtotal: U256::from(1),
            gas: 1,
        },
    ];
    assert!(plan_reward_claim_steps(&overflow, 10).is_err());
}

#[test]
fn positive_split_keeps_zero_runs_with_positive_children() {
    let amounts = |values: &[u64]| {
        values
            .iter()
            .enumerate()
            .map(|(index, &subtotal)| RewardIntervalAmount {
                interval: U256::from(index),
                subtotal: U256::from(subtotal),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        reward_positive_split_boundary(&amounts(&[0, 4]), 0, 2),
        None
    );
    assert_eq!(
        reward_positive_split_boundary(&amounts(&[4, 0]), 0, 2),
        None
    );
    assert_eq!(
        reward_positive_split_boundary(&amounts(&[4, 0, 4]), 0, 3),
        Some(1)
    );
    assert_eq!(
        reward_positive_split_boundary(&amounts(&[0, 0]), 0, 2),
        None
    );
}

#[test]
fn batch_planner_preserves_per_token_amounts_across_steps() {
    let tokens = vec![Address::from([1; 20]), Address::from([2; 20])];
    let reviewed = vec![
        RewardBatchIntervalSubtotal {
            interval: U256::ZERO,
            subtotals: vec![U256::from(2), U256::from(20)],
            gas: 5,
        },
        RewardBatchIntervalSubtotal {
            interval: U256::from(1),
            subtotals: vec![U256::from(3), U256::from(40)],
            gas: 5,
        },
    ];
    let steps = plan_reward_claim_batch_steps(&reviewed, &tokens, 5).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].reward_tokens, tokens);
    assert_eq!(
        steps[0].expected_amounts,
        vec![U256::from(2), U256::from(20)]
    );
    assert_eq!(
        steps[1].expected_amounts,
        vec![U256::from(3), U256::from(40)]
    );
}
