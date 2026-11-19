use super::*;

#[test]
fn scoped_successes_survive_account_and_asset_failures() {
    let address = Address::from([1; 20]);
    let token = Address::from([2; 20]);
    let token_two = Address::from([3; 20]);
    let mut state = StakingReadState::default();
    let key = GovernanceContextKey {
        wallet_id: Some(String::from("wallet")),
        chain_id: 1,
        participants: vec![
            GovernanceParticipantIdentity {
                uuid: String::from("a"),
                address,
            },
            GovernanceParticipantIdentity {
                uuid: String::from("good"),
                address,
            },
            GovernanceParticipantIdentity {
                uuid: String::from("pending"),
                address: Address::from([8; 20]),
            },
        ],
    };
    let generation = state.begin(key.clone());
    let metrics = StakingGlobalMetrics {
        total_staked: U256::from(3),
        total_voting_power: U256::from(2),
        deploy_time: U256::ZERO,
        snapshot_interval: U256::from(1),
        current_interval: U256::ZERO,
        stake_locktime: U256::ZERO,
        chain_time: U256::ZERO,
    };
    assert!(state.apply_global(&key, generation, Ok(metrics.clone())));
    assert!(state.apply_account(
        &key,
        generation,
        "a".into(),
        Err(Arc::from("account failed"))
    ));
    assert!(state.apply_reward(
        &key,
        generation,
        "a".into(),
        token,
        Ok(Some(RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: Vec::new(),
            amount: U256::from(1),
        }))
    ));
    assert!(state.apply_account(
        &key,
        generation,
        "good".into(),
        Ok(AccountStakeResult {
            account: address,
            voting_power: Ok(U256::from(4)),
            balance: Ok(U256::from(5)),
            stakes: Ok(Vec::new()),
        })
    ));
    assert!(state.apply_reward(
        &key,
        generation,
        "good".into(),
        token,
        Ok(Some(RewardEvidence {
            token,
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: Vec::new(),
            amount: U256::from(1),
        }))
    ));
    assert!(state.apply_reward(&key, generation, "good".into(), token_two, Ok(None),));
    assert!(state.apply_bulk_reward(
        &key,
        generation,
        "a".into(),
        vec![token, token_two],
        &Err(Arc::from("bulk asset failed")),
    ));
    assert!(state.apply_bulk_reward(
        &key,
        generation,
        "good".into(),
        vec![token, token_two],
        &Ok(Some(wallet_ops::RewardBatchEvidence {
            reward_tokens: vec![token, token_two],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: vec![Vec::new(), Vec::new()],
            expected_amounts: vec![U256::from(1), U256::ZERO],
        })),
    ));
    assert_eq!(state.metrics.as_ref(), Some(&metrics));
    assert!(state.accounts["a"].stakes.is_err());
    assert!(matches!(
        state.rewards[&(String::from("a"), token)],
        RewardView::Positive { .. }
    ));
    assert!(matches!(
        state.rewards[&(String::from("good"), token)],
        RewardView::Positive { .. }
    ));
    assert_eq!(state.status, StakingRefreshStatus::Loading);
    assert!(state.global_action_ready());
    assert!(state.account_action_ready("good"));
    assert!(!state.account_action_ready("a"));
    assert!(!state.account_action_ready("pending"));
    assert!(state.reward_action_ready("good", token));
    assert!(state.reward_action_ready("a", token));
    assert!(!state.reward_action_ready("pending", token));
    let claim_selection =
        |uuid: &str, actor: Address, tokens: Vec<Address>| StakingActionSelection {
            actor_uuid: uuid.to_owned(),
            actor,
            kind: StakingActionKind::RewardClaimSelected { tokens },
        };
    assert!(state.action_selection_ready(&claim_selection("good", address, vec![token])));
    assert!(state.action_selection_ready(&claim_selection("good", address, Vec::new())));
    assert!(!state.action_selection_ready(&claim_selection(
        "good",
        address,
        vec![token, token_two]
    )));
    assert!(!state.action_selection_ready(&claim_selection(
        "pending",
        Address::from([8; 20]),
        Vec::new()
    )));
    assert!(!state.action_selection_ready(&StakingActionSelection {
        actor_uuid: String::from("a"),
        actor: address,
        kind: StakingActionKind::Delegate {
            stake_id: U256::ZERO,
        },
    }));
    assert!(state.apply_reward(
        &key,
        generation,
        "good".into(),
        token_two,
        Err(Arc::from("second asset failed")),
    ));
    assert!(state.action_selection_ready(&claim_selection("good", address, vec![token])));
    assert!(!state.action_selection_ready(&claim_selection("good", address, vec![token_two])));
    assert!(!state.action_selection_ready(&StakingActionSelection {
        actor_uuid: String::from("good"),
        actor: Address::from([9; 20]),
        kind: StakingActionKind::RewardClaimSelected {
            tokens: vec![token]
        },
    }));

    let next_generation = state.begin(key.clone());
    assert_ne!(generation, next_generation);
    assert_eq!(state.metrics.as_ref(), Some(&metrics));
    assert!(state.accounts.contains_key("good"));
    assert!(state.rewards.contains_key(&(String::from("good"), token)));
    assert!(!state.global_action_ready());
    assert!(!state.account_action_ready("good"));
    assert!(!state.reward_action_ready("good", token));
    assert!(!state.apply_bulk_reward(
        &key,
        generation,
        "good".into(),
        vec![token, token_two],
        &Ok(Some(wallet_ops::RewardBatchEvidence {
            reward_tokens: vec![token, token_two],
            starting_interval: U256::ZERO,
            ending_interval: U256::ZERO,
            staking_intervals: vec![U256::ZERO],
            hints: vec![U256::ZERO],
            claimed_intervals: vec![Vec::new(), Vec::new()],
            expected_amounts: vec![U256::from(1), U256::ZERO],
        })),
    ));
    assert!(!state.action_selection_ready(&claim_selection("good", address, vec![token])));

    assert!(state.apply_global(
        &key,
        next_generation,
        Err(Arc::from("global refresh failed"))
    ));
    assert_eq!(state.status, StakingRefreshStatus::Stale);
    assert!(!state.global_action_ready());
}

#[test]
fn empty_participants_finish_fresh_after_global_metrics() {
    let mut state = StakingReadState::default();
    let key = GovernanceContextKey {
        wallet_id: Some(String::from("wallet")),
        chain_id: 1,
        participants: Vec::new(),
    };
    let generation = state.begin(key.clone());
    let metrics = StakingGlobalMetrics {
        total_staked: U256::from(3),
        total_voting_power: U256::from(2),
        deploy_time: U256::ZERO,
        snapshot_interval: U256::from(1),
        current_interval: U256::ZERO,
        stake_locktime: U256::ZERO,
        chain_time: U256::from(10),
    };
    assert!(state.apply_global(&key, generation, Ok(metrics.clone())));
    assert!(state.finish(&key, generation));
    assert_eq!(state.status, StakingRefreshStatus::Fresh);
    assert_eq!(state.metrics, Some(metrics));
    assert!(state.accounts.is_empty());
    assert!(state.rewards.is_empty());
}

#[test]
fn participant_states_cover_scopes_inactive_search_and_empty_results() {
    let accounts = vec![
        account("a", PublicAccountStatus::Active, false),
        account("b", PublicAccountStatus::Inactive, false),
        account("g", PublicAccountStatus::Active, true),
        account("i", PublicAccountStatus::Inactive, true),
    ];
    let selected = BTreeSet::from([String::from("b"), String::from("g")]);
    let summary = participant_summary(&accounts, &selected, Some("wallet"));
    assert_eq!(
        summary,
        ParticipantSummary {
            selected: 2,
            inactive: 1
        }
    );
    let choices = participant_choices(&accounts, &selected, Some("wallet"), "");
    assert_eq!(
        choices
            .iter()
            .map(|choice| choice.uuid.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "g"]
    );
    assert!(!choices.iter().any(|choice| choice.uuid == "i"));
    assert_eq!(
        participant_choices(&accounts, &selected, Some("wallet"), "missing"),
        [] as [ParticipantChoice; 0]
    );
    assert_eq!(
        participant_choices(&accounts, &BTreeSet::new(), Some("wallet"), "alice").len(),
        0
    );
    assert_eq!(
        participant_choices(&accounts, &selected, Some("other"), "")
            .iter()
            .map(|choice| choice.uuid.as_str())
            .collect::<Vec<_>>(),
        vec!["g"]
    );
}

#[test]
fn default_tab_and_context_invalidation_are_explicit() {
    assert_eq!(GovernanceTab::default(), GovernanceTab::Proposals);
    let mut state = StakingReadState::default();
    let key = key("wallet", 1, "a", Address::from([1; 20]));
    let generation = state.begin(key.clone());
    state.invalidate();
    assert!(!state.finish(&key, generation));
    assert_eq!(state.status, StakingRefreshStatus::Idle);
}

#[test]
fn staking_control_ids_bind_actor_subject_and_action() {
    let first = staking_control_id("actor-a", "reward-claim", "token");
    let second = staking_control_id("actor-b", "reward-claim", "token");
    let different_action = staking_control_id("actor-a", "delegate", "token");

    assert_ne!(first, second);
    assert_ne!(first, different_action);
}
