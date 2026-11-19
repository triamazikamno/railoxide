use super::*;

fn isolated_test_route() -> wallet_ops::ClassRoute {
    wallet_ops::RpcChainRoute::new(1, vec![reqwest::Url::parse("http://127.0.0.1:1").unwrap()])
        .for_class(wallet_ops::RpcTrafficClass::Sensitive)
        .unwrap()
        .isolated(wallet_ops::OperationHttpClient::for_tests(
            reqwest::Client::new(),
            wallet_ops::OperationNetworkIsolation::Unavailable(
                wallet_ops::WalletNetworkMode::Direct,
            ),
        ))
}

#[tokio::test]
async fn participant_reads_each_get_one_participant_and_their_own_route() {
    let jitters = std::cell::Cell::new(0);
    let mut reads = Vec::new();
    read_participants_isolated(
        vec!["a", "b"],
        async || jitters.set(jitters.get() + 1),
        || true,
        async || Ok(isolated_test_route()),
        async |participant, route: Result<wallet_ops::ClassRoute, String>| {
            reads.push((participant, route.unwrap()));
        },
    )
    .await;
    assert_eq!(jitters.get(), 1);
    let mut participants = reads
        .iter()
        .map(|(participant, _)| *participant)
        .collect::<Vec<_>>();
    participants.sort_unstable();
    assert_eq!(participants, ["a", "b"]);
    assert_ne!(reads[0].1, reads[1].1);
}

#[tokio::test]
async fn failed_route_open_fails_only_that_participant() {
    let opens = std::cell::Cell::new(0);
    let mut reads = Vec::new();
    read_participants_isolated(
        vec!["a", "b"],
        async || {},
        || true,
        async || {
            opens.set(opens.get() + 1);
            if opens.get() == 1 {
                Err("isolated route unavailable".to_owned())
            } else {
                Ok(isolated_test_route())
            }
        },
        async |participant, route: Result<wallet_ops::ClassRoute, String>| {
            reads.push((participant, route.is_ok()));
        },
    )
    .await;
    assert_eq!(reads.len(), 2);
    assert_ne!(reads[0].0, reads[1].0);
    assert_eq!([reads[0].1, reads[1].1], [false, true]);
}

#[tokio::test]
async fn superseded_participant_refresh_stops_before_the_next_participant() {
    let a = GovernanceParticipantIdentity {
        uuid: "a".to_owned(),
        address: Address::from([1; 20]),
    };
    let b = GovernanceParticipantIdentity {
        uuid: "b".to_owned(),
        address: Address::from([2; 20]),
    };
    let proposal_key =
        |participants: Vec<GovernanceParticipantIdentity>| ProposalParticipationKey {
            version: wallet_ops::GovernanceContractVersion::V2,
            contract: Address::from([9; 20]),
            index: U256::ZERO,
            context: GovernanceContextKey {
                wallet_id: Some("wallet".to_owned()),
                chain_id: 1,
                participants,
            },
        };
    let key_ab = proposal_key(vec![a.clone(), b.clone()]);
    let key_a = proposal_key(vec![a.clone()]);
    let state = std::cell::RefCell::new(ProposalParticipationState::default());
    let g1 = state.borrow_mut().begin(&key_ab, &[a.address, b.address]);
    let g2 = std::cell::Cell::new(None);
    let opens = std::cell::Cell::new(0);
    let mut reads = Vec::new();
    read_participants_isolated(
        vec![a.address, b.address],
        async || {},
        || state.borrow().current(&key_ab, g1),
        async || {
            opens.set(opens.get() + 1);
            Ok(isolated_test_route())
        },
        async |account, _route: Result<wallet_ops::ClassRoute, String>| {
            reads.push(account);
            // The participant set changes while the first participant is being read.
            g2.set(Some(state.borrow_mut().begin(&key_a, &[a.address])));
        },
    )
    .await;
    assert_eq!(opens.get(), 1);
    assert_eq!(reads.len(), 1);

    let mut state = state.into_inner();
    let g2 = g2.get().unwrap();
    assert!(!state.apply(&key_ab, g1, b.address, Err(Arc::from("late"))));
    assert!(state.apply(&key_a, g2, a.address, Err(Arc::from("route unavailable"))));
    let proposal = wallet_ops::GovernanceProposal {
        contract_version: key_a.version,
        index: key_a.index,
        contract_address: key_a.contract,
        proposer: Address::ZERO,
        proposal_document: String::new(),
        publish_time: U256::ONE,
        vote_call_time: U256::ZERO,
        sponsorship: U256::ZERO,
        executed: false,
        yay_votes: U256::ZERO,
        nay_votes: U256::ZERO,
        sponsor_snapshot_interval: U256::ONE,
        voting_snapshot_interval: U256::ZERO,
        actions: Vec::new(),
    };
    assert!(matches!(
        state.row(&proposal, &a.address),
        Some(crate::root::governance_action::ProposalParticipationRow::Unavailable(_))
    ));
    // The superseded request left no stuck `Loading` row for `b`.
    assert!(state.row(&proposal, &b.address).is_none());
    assert!(!state.loading());
}

#[test]
fn an_action_drops_the_actors_participation_on_every_proposal_of_its_contract() {
    use crate::root::governance_action::ProposalParticipationRow;
    use wallet_ops::GovernanceContractVersion::{V1, V2};

    let actor = Address::from([1; 20]);
    let other = Address::from([2; 20]);
    let key = |chain_id: u64, version, contract: u8, index: u64| ProposalParticipationKey {
        version,
        contract: Address::from([contract; 20]),
        index: U256::from(index),
        context: GovernanceContextKey {
            wallet_id: Some("wallet".to_owned()),
            chain_id,
            participants: [("actor", actor), ("other", other)]
                .map(|(uuid, address)| GovernanceParticipantIdentity {
                    uuid: uuid.to_owned(),
                    address,
                })
                .into(),
        },
    };
    let proposal = |key: &ProposalParticipationKey| wallet_ops::GovernanceProposal {
        contract_version: key.version,
        index: key.index,
        contract_address: key.contract,
        proposer: Address::ZERO,
        proposal_document: String::new(),
        publish_time: U256::ONE,
        vote_call_time: U256::ZERO,
        sponsorship: U256::ZERO,
        executed: false,
        yay_votes: U256::ZERO,
        nay_votes: U256::ZERO,
        sponsor_snapshot_interval: U256::ONE,
        voting_snapshot_interval: U256::ZERO,
        actions: Vec::new(),
    };
    let snapshot = |interval| wallet_ops::GovernanceAccountSnapshot {
        interval,
        voting_power: U256::ZERO,
        hint: U256::ZERO,
    };
    let participation =
        |key: &ProposalParticipationKey, account| wallet_ops::GovernanceParticipation {
            proposal_version: key.version,
            proposal_id: key.index,
            voting_contract: key.contract,
            account,
            current_voting_power: U256::ZERO,
            sponsorship_snapshot: snapshot(U256::ONE),
            voting_snapshot: snapshot(U256::ZERO),
            sponsored: U256::ZERO,
            voted: U256::ZERO,
            last_sponsored: wallet_ops::GovernanceLastSponsored {
                last_sponsor_time: U256::ZERO,
                proposal_id: U256::ZERO,
            },
        };
    let ready = |state: &mut ProposalParticipationState,
                 key: &ProposalParticipationKey,
                 account: Address| {
        state.show(key);
        matches!(
            state.row(&proposal(key), &account),
            Some(ProposalParticipationRow::Ready(_))
        )
    };

    let acted = key(1, V2, 9, 0);
    let sibling = key(1, V2, 9, 1);
    let other_version = key(1, V1, 9, 0);
    let other_contract = key(1, V2, 8, 0);
    let other_chain = key(5, V2, 9, 0);
    let mut state = ProposalParticipationState::default();
    for loaded in [
        &acted,
        &sibling,
        &other_version,
        &other_contract,
        &other_chain,
    ] {
        let generation = state.begin(loaded, &[actor, other]);
        for account in [actor, other] {
            assert!(state.apply(
                loaded,
                generation,
                account,
                Ok(participation(loaded, account))
            ));
        }
    }
    // `lastSponsored` is per account and voting contract, so the actor's row on another
    // proposal of the same contract is as stale as on the acted one.
    state.invalidate_actor_on_contract(&acted, actor);
    assert!(!ready(&mut state, &acted, actor));
    assert!(!ready(&mut state, &sibling, actor));
    assert!(ready(&mut state, &sibling, other));
    assert!(ready(&mut state, &other_version, actor));
    assert!(ready(&mut state, &other_contract, actor));
    assert!(ready(&mut state, &other_chain, actor));

    // The actor's read in flight on another proposal of the contract cannot restore a
    // pre-action row, while the other participant's read still completes.
    let mut state = ProposalParticipationState::default();
    let generation = state.begin(&sibling, &[actor, other]);
    state.invalidate_actor_on_contract(&acted, actor);
    assert!(!state.apply(
        &sibling,
        generation,
        actor,
        Ok(participation(&sibling, actor))
    ));
    assert!(state.row(&proposal(&sibling), &actor).is_none());
    assert!(state.apply(
        &sibling,
        generation,
        other,
        Ok(participation(&sibling, other))
    ));
    assert!(ready(&mut state, &sibling, other));

    // After a called vote, the action's read and a refresh started later both carry the new
    // voting interval. The later read wins, even when it completes first.
    let called = wallet_ops::GovernanceProposal {
        voting_snapshot_interval: U256::from(7),
        ..proposal(&acted)
    };
    let fresh = |account, power: u64| wallet_ops::GovernanceParticipation {
        current_voting_power: U256::from(power),
        voting_snapshot: snapshot(U256::from(7)),
        ..participation(&acted, account)
    };
    let power = |state: &ProposalParticipationState,
                 proposal: &wallet_ops::GovernanceProposal,
                 account: Address| match state.row(proposal, &account) {
        Some(ProposalParticipationRow::Ready(participation)) => {
            Some(participation.current_voting_power)
        }
        _ => None,
    };
    let mut state = ProposalParticipationState::default();
    let action = state.invalidate_actor_on_contract(&acted, actor);
    let generation = state.begin(&acted, &[actor, other]);
    assert!(state.apply(&acted, generation, actor, Ok(fresh(actor, 2))));
    assert!(!state.apply_action_read(1, &called, actor, action, fresh(actor, 1)));
    assert_eq!(power(&state, &called, actor), Some(U256::from(2)));

    // An action's read completing first replaces the actor's row of a request still running.
    let mut state = ProposalParticipationState::default();
    let action = state.invalidate_actor_on_contract(&acted, actor);
    state.begin(&acted, &[actor, other]);
    assert!(state.apply_action_read(1, &called, actor, action, fresh(actor, 1)));
    assert_eq!(power(&state, &called, actor), Some(U256::from(1)));

    // A request from a page cached before the vote reads the old interval. It does not
    // replace the fresh row, which that page does not request again.
    let generation = state.begin(&acted, &[actor, other]);
    for account in [actor, other] {
        assert!(state.apply(
            &acted,
            generation,
            account,
            Ok(participation(&acted, account))
        ));
    }
    assert_eq!(power(&state, &called, actor), Some(U256::from(1)));
    assert!(state.row(&proposal(&acted), &actor).is_none());
    assert_eq!(state.unloaded(&proposal(&acted)), [] as [Address; 0]);

    // A later action on the contract retires an earlier action's read still outstanding, and
    // so does the end of the wallet session.
    let mut state = ProposalParticipationState::default();
    let earlier = state.invalidate_actor_on_contract(&acted, actor);
    state.invalidate_actor_on_contract(&sibling, actor);
    assert!(!state.apply_action_read(1, &called, actor, earlier, fresh(actor, 1)));
    let outstanding = state.invalidate_actor_on_contract(&acted, actor);
    state.clear_session();
    assert!(!state.apply_action_read(1, &called, actor, outstanding, fresh(actor, 1)));
    state.show(&acted);
    assert!(state.row(&called, &actor).is_none());
}

#[test]
fn failed_participant_fails_only_its_own_cells() {
    let tokens = [Address::from([2; 20]), Address::from([3; 20])];
    let key = GovernanceContextKey {
        wallet_id: Some("wallet".to_owned()),
        chain_id: 1,
        participants: vec![
            GovernanceParticipantIdentity {
                uuid: "a".to_owned(),
                address: Address::from([1; 20]),
            },
            GovernanceParticipantIdentity {
                uuid: "b".to_owned(),
                address: Address::from([4; 20]),
            },
        ],
    };
    let mut state = StakingReadState::default();
    let generation = state.begin(key.clone());
    let error: Arc<str> = Arc::from("route unavailable");

    state.fail_participant(
        &key,
        generation.wrapping_sub(1),
        "a".into(),
        &tokens,
        Arc::clone(&error),
    );
    assert!(state.accounts.is_empty() && state.current_account_ids.is_empty());
    assert!(state.rewards.is_empty() && state.current_reward_keys.is_empty());
    assert!(state.bulk_reward_readiness.is_empty());

    state.fail_participant(&key, generation, "a".into(), &tokens, error);
    let account = &state.accounts["a"];
    assert!(account.voting_power.is_err() && account.balance.is_err() && account.stakes.is_err());
    for token in tokens {
        assert!(matches!(
            state.resolved_reward("a", token),
            Some(RewardView::Unavailable(_))
        ));
        assert!(state.current_reward_keys.contains(&("a".to_owned(), token)));
    }
    assert_eq!(
        state.bulk_reward_readiness.get("a"),
        Some(&BulkRewardReadiness {
            tokens: tokens.to_vec(),
            ready: false,
        })
    );
    assert!(!state.accounts.contains_key("b"));
    assert!(state.rewards.keys().all(|(uuid, _)| uuid == "a"));
    assert!(!state.bulk_reward_readiness.contains_key("b"));
}
