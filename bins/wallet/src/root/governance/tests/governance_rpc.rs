use super::*;

fn tagged_client(tag: &str) -> reqwest::Client {
    let headers = reqwest::header::HeaderMap::from_iter([(
        reqwest::header::HeaderName::from_static("x-test-client"),
        reqwest::header::HeaderValue::from_str(tag).unwrap(),
    )]);
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

fn governance_call_output(to: Address, input: &[u8], chain: &GovernanceMockChain) -> Vec<u8> {
    use GovernanceRpcFixture as F;
    use alloy::sol_types::{SolCall, SolValue};

    let contracts = governance_contracts(1).unwrap();
    let selector: [u8; 4] = input[..4].try_into().unwrap();
    let uint = |value: u64| U256::from(value).abi_encode();
    match selector {
        F::stakingTokenCall::SELECTOR => contracts.governance_token.abi_encode(),
        F::stakingCall::SELECTOR => contracts.staking.abi_encode(),
        F::proposalsLengthCall::SELECTOR if to == contracts.voting => {
            uint(chain.proposals.load(Ordering::SeqCst))
        }
        F::proposalsLengthCall::SELECTOR => uint(0),
        // Proposal 0 is open for sponsorship, and the actor has sponsored nothing yet.
        F::SPONSOR_WINDOWCall::SELECTOR => uint(100),
        F::getSponsoredCall::SELECTOR => uint(0),
        F::VOTING_START_OFFSETCall::SELECTOR => uint(1),
        F::VOTING_YAY_END_OFFSETCall::SELECTOR => uint(2),
        F::VOTING_NAY_END_OFFSETCall::SELECTOR => uint(3),
        F::EXECUTION_START_OFFSETCall::SELECTOR => uint(4),
        F::EXECUTION_END_OFFSETCall::SELECTOR => uint(5),
        F::proposalsCall::SELECTOR => F::proposalsCall::abi_encode_returns(&F::proposalsReturn {
            executed: false,
            proposer: Address::ZERO,
            proposalDocument: String::new(),
            publishTime: U256::ONE,
            voteCallTime: U256::ZERO,
            sponsorship: U256::ZERO,
            yayVotes: U256::ZERO,
            nayVotes: U256::ZERO,
            sponsorInterval: U256::ONE,
            votingInterval: U256::from(chain.voting_interval.load(Ordering::SeqCst)),
        }),
        F::getActionsCall::SELECTOR => F::getActionsCall::abi_encode_returns(&Vec::new()),
        F::accountSnapshotLengthCall::SELECTOR => uint(0),
        F::accountSnapshotAtCall::SELECTOR => (U256::from(1_000), U256::ONE).abi_encode(),
        F::lastSponsoredCall::SELECTOR => (U256::ZERO, U256::ZERO).abi_encode(),
        F::getClaimedCall::SELECTOR => false.abi_encode(),
        F::calculateRewardsCall::SELECTOR => {
            let call = F::calculateRewardsCall::abi_decode(input).unwrap();
            F::calculateRewardsCall::abi_encode_returns(&vec![U256::ONE; call.tokens.len()])
        }
        // Balances, voting power, reward metadata and the other governance values.
        _ => uint(1),
    }
}

fn governance_rpc_answer(
    request: &serde_json::Value,
    chain: &GovernanceMockChain,
) -> serde_json::Value {
    let result = match request["method"].as_str().unwrap_or_default() {
        "eth_call" => {
            let call = &request["params"][0];
            let to = call["to"].as_str().unwrap().parse::<Address>().unwrap();
            let input = call
                .get("input")
                .or_else(|| call.get("data"))
                .and_then(serde_json::Value::as_str)
                .unwrap()
                .parse::<alloy::primitives::Bytes>()
                .unwrap();
            Some(serde_json::json!(alloy::primitives::Bytes::from(
                governance_call_output(to, &input, chain)
            )))
        }
        // Fees are served so that an estimate reaches `eth_estimateGas`, which is refused.
        "eth_gasPrice" => Some(serde_json::json!("0x2")),
        "eth_maxPriorityFeePerGas" => Some(serde_json::json!("0x1")),
        "eth_getBlockByNumber" => Some(serde_json::json!({
            "number": "0x1",
            "hash": B256::repeat_byte(1),
            "parentHash": B256::ZERO,
            "sha3Uncles": B256::ZERO,
            "miner": Address::ZERO,
            "stateRoot": B256::ZERO,
            "transactionsRoot": B256::ZERO,
            "receiptsRoot": B256::ZERO,
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "timestamp": "0x2",
            "extraData": "0x",
            "mixHash": B256::ZERO,
            "nonce": "0x0000000000000000",
            "baseFeePerGas": "0x1",
            "uncles": [],
            "transactions": [],
        })),
        _ => None,
    };
    match result {
        Some(result) => {
            serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
        }
        None => serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "error": {"code": -32601, "message": "not served by the governance mock"},
        }),
    }
}

/// Serves governance reads on a local port, each connection on its own thread so a held client
/// does not block the others, and records each call with the client's tag.
fn spawn_governance_rpc_endpoint(
    endpoint: &'static str,
    seen: Arc<std::sync::Mutex<Vec<SeenRpcCall>>>,
    chain: Arc<GovernanceMockChain>,
) -> reqwest::Url {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = reqwest::Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                return;
            };
            let seen = Arc::clone(&seen);
            let chain = Arc::clone(&chain);
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                let (header_end, length) = loop {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break (0, 0);
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..index]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map_or(0, |value| value.trim().parse().unwrap());
                        break (index + 4, length);
                    }
                };
                if header_end == 0 {
                    return;
                }
                while bytes.len() < header_end + length {
                    let read = stream.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
                let client = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("x-test-client:"))
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let mut held = chain.held_client.lock().unwrap();
                while held.as_deref() == Some(client.as_str()) {
                    held = chain.released.wait(held).unwrap();
                }
                drop(held);
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                let requests = body
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![body.clone()]);
                let answers = requests
                    .iter()
                    .map(|request| {
                        seen.lock().unwrap().push(SeenRpcCall {
                            endpoint,
                            client: client.clone(),
                            method: request["method"].as_str().unwrap_or_default().to_owned(),
                        });
                        governance_rpc_answer(request, &chain)
                    })
                    .collect::<Vec<_>>();
                let response = if body.is_array() {
                    serde_json::Value::Array(answers)
                } else {
                    answers.into_iter().next().unwrap()
                };
                let response = response.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
            });
        }
    });
    url
}

fn governance_rpc_mock() -> GovernanceRpcMock {
    let seen = Arc::default();
    let chain = Arc::new(GovernanceMockChain {
        proposals: 1.into(),
        voting_interval: 0.into(),
        held_client: std::sync::Mutex::default(),
        released: std::sync::Condvar::new(),
    });
    let general = spawn_governance_rpc_endpoint("general", Arc::clone(&seen), Arc::clone(&chain));
    let sensitive =
        spawn_governance_rpc_endpoint("sensitive", Arc::clone(&seen), Arc::clone(&chain));
    let only = |general, sensitive| wallet_ops::RpcEndpointRoles {
        general,
        sensitive,
        submit: sensitive,
    };
    let route = wallet_ops::RpcChainRoute::new_with_transports(
        1,
        vec![
            (
                general.into(),
                wallet_ops::RpcTransport::Wallet,
                only(true, false),
            ),
            (
                sensitive.into(),
                wallet_ops::RpcTransport::Wallet,
                only(false, true),
            ),
        ],
    );
    let mut effective_chain = wallet_ops::settings::build_effective_chain_configs(
        &wallet_ops::settings::WalletSettings::default(),
    )
    .unwrap()
    .get(1)
    .cloned()
    .unwrap();
    effective_chain.rpc_route = route.clone();
    let http = wallet_ops::HttpContext::with_rpc_client_for_tests(
        tagged_client("shared"),
        wallet_ops::WalletNetworkMode::Direct,
    );
    let client = wallet_ops::OperationHttpClient::for_tests(
        tagged_client("isolated"),
        wallet_ops::OperationNetworkIsolation::Unavailable(wallet_ops::WalletNetworkMode::Direct),
    );
    let isolated = route
        .for_class(wallet_ops::RpcTrafficClass::Sensitive)
        .unwrap()
        .isolated(client.clone());
    GovernanceRpcMock {
        seen,
        chain,
        effective_chain,
        http,
        client,
        isolated,
    }
}

#[tokio::test]
async fn action_sends_governance_reads_and_estimates_only_over_the_action_client() {
    use wallet_ops::vault::{DesktopVaultStore, KdfParams, WalletSource};
    const PASSWORD: &str = "governance action test password";

    let mock = governance_rpc_mock();
    let contracts = governance_contracts(1).unwrap();
    let actor = Address::from([7; 20]);
    let tokens = [contracts.reward_tokens[0].token];

    // Both reward runs read everything, then split the claim on each refused gas estimate
    // until no range is left to try.
    let _ = plan_reward_claim_draft(
        1,
        actor,
        &tokens,
        &mock.isolated,
        &mock.effective_chain,
        &mock.http,
        &mock.client,
        RewardEvidenceMode::Fresh,
    )
    .await;
    let fresh_reward = mock.take();
    let _ = read_reward_continuation(
        1,
        actor,
        &tokens,
        &mock.isolated,
        &mock.effective_chain,
        &mock.http,
        &mock.client,
    )
    .await;
    let continued_reward = mock.take();
    let proposal = wallet_ops::GovernanceProposal {
        contract_version: wallet_ops::GovernanceContractVersion::V2,
        index: U256::ZERO,
        contract_address: contracts.voting,
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
    // The sponsor draft holds the action's slot, which already holds its client.
    let vault_dir = std::env::temp_dir().join(format!(
        "railoxide-governance-action-{:032x}",
        rand::random::<u128>()
    ));
    let store = Arc::new(DesktopVaultStore::open(vault_dir.clone()).unwrap());
    store
        .create_vault_with_params(PASSWORD, KdfParams::new(1024, 1, 1))
        .unwrap();
    let metadata = store
        .new_wallet_metadata(PASSWORD, "wallet", 0, WalletSource::Imported, "Wallet")
        .unwrap();
    store
        .import_wallet_mnemonic_with_metadata(
            PASSWORD,
            "wallet",
            0,
            "english",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
            &metadata,
        )
        .unwrap();
    let view = Arc::new(store.load_view_session(PASSWORD, "wallet").unwrap());
    let sponsor = crate::root::governance_action::build_governance_spend_draft(
        proposal.clone(),
        crate::root::governance_action::proposal_participation_key(
            &proposal,
            GovernanceContextKey {
                wallet_id: Some("wallet".to_owned()),
                chain_id: 1,
                participants: Vec::new(),
            },
        ),
        ProposalActionSelection {
            actor,
            kind: ProposalActionKind::Sponsor,
        },
        Some(U256::ONE),
        Arc::from("wallet"),
        Arc::from("actor"),
        wallet_ops::vault::PublicAccountSource::Derived,
        view,
        Arc::clone(&store),
        mock.effective_chain.clone(),
        mock.http.clone(),
        wallet_ops::OperationHttpClientSlot::for_tests(mock.http.clone(), mock.client.clone()),
        PublicActionGasFeeSelection::Auto,
    )
    .await;
    assert!(sponsor.is_err(), "the mock refuses the gas estimate");
    let sponsor_requests = mock.take();
    drop(store);
    std::fs::remove_dir_all(vault_dir).unwrap();

    for requests in [fresh_reward, continued_reward, sponsor_requests] {
        // Chain time or the block gas limit was read, and the gas was estimated.
        for method in ["eth_getBlockByNumber", "eth_estimateGas"] {
            assert!(
                requests.iter().any(|request| request.method == method),
                "{method}: {requests:?}"
            );
        }
        for request in &requests {
            assert_eq!(
                (request.endpoint, request.client.as_str()),
                ("sensitive", "isolated"),
                "{request:?}"
            );
        }
    }
}

#[tokio::test]
async fn dashboard_general_reads_use_the_shared_general_route() {
    let mock = governance_rpc_mock();
    let general = governance_route(
        1,
        &mock.effective_chain,
        wallet_ops::RpcTrafficClass::General,
    )
    .unwrap();
    let loaded = crate::root::proposals::load_initial(1, 0, &general, &mock.http).await;
    assert!(loaded.is_ok(), "{:?}", loaded.err());
    let reads = mock.take();
    assert!(
        reads
            .iter()
            .any(|read| read.method == "eth_getBlockByNumber")
    );
    assert!(reads.iter().any(|read| read.method == "eth_call"));
    for read in &reads {
        assert_eq!(
            (read.endpoint, read.client.as_str()),
            ("general", "shared"),
            "{read:?}"
        );
    }
}

/// Drives the root's tasks, letting jitter timers fire, until `done` holds.
fn settle_governance(
    root: &gpui::Entity<WalletRoot>,
    cx: &gpui::VisualTestContext,
    runtime: &tokio::runtime::Runtime,
    done: impl Fn(&WalletRoot) -> bool,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(10)));
        cx.run_until_parked();
        if root.read_with(cx, |root, _| done(root)) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "governance reads did not settle"
        );
        // Participation reads wait up to three seconds of jitter between accounts.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
    }
}

/// Drives the root's tasks for a while, for checks that nothing more happens.
fn drive_governance(cx: &gpui::VisualTestContext, runtime: &tokio::runtime::Runtime) {
    for _ in 0..40 {
        runtime.block_on(tokio::time::sleep(std::time::Duration::from_millis(10)));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
    }
}

/// Whether `account` has a loaded row matching `proposal` under `key`, whatever is shown.
fn has_loaded_row(
    root: &WalletRoot,
    key: &ProposalParticipationKey,
    proposal: &wallet_ops::GovernanceProposal,
    account: Address,
) -> bool {
    let mut state = root.governance.proposal_participation.clone();
    state.invalidate();
    state.show(key);
    matches!(
        state.row(proposal, &account),
        Some(crate::root::governance_action::ProposalParticipationRow::Ready(_))
    )
}

/// The number of isolated clients that read from the Sensitive endpoint since the last take.
fn isolated_participation_reads(mock: &GovernanceRpcMock) -> usize {
    let calls = mock.take();
    calls
        .iter()
        .filter(|call| call.endpoint == "sensitive")
        .map(|call| {
            assert!(call.client.starts_with("isolated-"), "{call:?}");
            call.client.as_str()
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

#[gpui::test]
fn proposal_participation_is_read_only_on_request(cx: &mut gpui::TestAppContext) {
    use crate::root::governance_action::ProposalParticipationRow;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let path = std::env::temp_dir().join(format!(
        "railoxide-proposal-participation-{:032x}",
        rand::random::<u128>()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mock = governance_rpc_mock();
    // Each operation client is tagged by its open, so each isolated route is told apart.
    let opens = Arc::new(AtomicUsize::new(0));
    let fail_next_open = Arc::new(AtomicBool::new(false));
    let http = {
        let opens = Arc::clone(&opens);
        let fail_next_open = Arc::clone(&fail_next_open);
        wallet_ops::HttpContext::with_rpc_client_for_tests(
            tagged_client("shared"),
            wallet_ops::WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(move || {
            let open = opens.fetch_add(1, Ordering::SeqCst);
            if fail_next_open.swap(false, Ordering::SeqCst) {
                return Err(eyre::eyre!("synthetic open failure"));
            }
            Ok(wallet_ops::OperationHttpClient::for_tests(
                tagged_client(&format!("isolated-{open}")),
                wallet_ops::OperationNetworkIsolation::Unavailable(
                    wallet_ops::WalletNetworkMode::Direct,
                ),
            ))
        })
    };
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let accounts = [1_u8, 2, 3].map(Address::repeat_byte);
    let set_participants = |root: &mut WalletRoot, count: usize| {
        root.ui_state.governance_participants.insert(
            "preview".to_owned(),
            (1..=count)
                .map(|index| format!("account-{index}"))
                .collect(),
        );
    };
    root.update(cx, |root, _| {
        root.http = http;
        root.effective_chain_configs = std::iter::once(mock.effective_chain.clone()).collect();
        set_participants(root, 2);
        root.governance.tab = GovernanceTab::Staking;
    });

    // Switching to Proposals loads the list, then opening a proposal, switching back to it
    // and refreshing its page or the list read no account.
    root.update(cx, |root, cx| {
        root.select_governance_tab(GovernanceTab::Proposals, cx);
    });
    settle_governance(&root, cx, &runtime, |root| {
        root.proposals.rows(0).is_some_and(|rows| !rows.is_empty())
    });
    let identity = root.read_with(cx, |root, _| root.proposals.rows(0).unwrap()[0].identity());
    root.update_in(cx, |root, window, cx| {
        root.select_proposal(0, &identity, window, cx);
        root.governance.tab = GovernanceTab::Staking;
        root.select_governance_tab(GovernanceTab::Proposals, cx);
        root.refresh_selected_proposal_page(cx);
    });
    settle_governance(&root, cx, &runtime, |root| {
        root.proposals.selected_proposal().is_some()
    });
    root.update(cx, |root, cx| root.start_proposals_refresh(true, cx));
    settle_governance(&root, cx, &runtime, |root| !root.proposals.loading);
    let proposal = root.read_with(cx, |root, _| {
        root.proposals.selected_proposal().cloned().unwrap()
    });
    let row_states = |root: &WalletRoot| {
        accounts
            .iter()
            .map(|account| {
                match root
                    .governance
                    .proposal_participation
                    .row(&proposal, account)
                {
                    None => "not loaded",
                    Some(ProposalParticipationRow::Loading) => "loading",
                    Some(ProposalParticipationRow::Ready(_)) => "ready",
                    Some(ProposalParticipationRow::Unavailable(_)) => "unavailable",
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(isolated_participation_reads(&mock), 0);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root)),
        ["not loaded"; 3]
    );

    let request = |refresh: bool, cx: &mut gpui::VisualTestContext| {
        root.update(cx, |root, cx| {
            root.request_proposal_participation(refresh, cx);
        });
        settle_governance(&root, cx, &runtime, |root| {
            !root.governance.proposal_participation.loading()
        });
        isolated_participation_reads(&mock)
    };
    // Showing participation reads each participant once, each over its own client.
    assert_eq!(request(false, cx), 2);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root)),
        ["ready", "ready", "not loaded"]
    );

    // Reopening the proposal shows the loaded rows without reading.
    root.update_in(cx, |root, window, cx| {
        root.clear_selected_proposal(cx);
        root.select_proposal(0, &identity, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(isolated_participation_reads(&mock), 0);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root)),
        ["ready", "ready", "not loaded"]
    );

    // An added participant is the only account the next request reads.
    root.update(cx, |root, _| {
        set_participants(root, 3);
        root.invalidate_governance_context();
        root.show_proposal_participation(&proposal);
    });
    cx.run_until_parked();
    assert_eq!(isolated_participation_reads(&mock), 0);
    assert_eq!(request(false, cx), 1);
    assert_eq!(root.read_with(cx, |root, _| row_states(root)), ["ready"; 3]);

    // Refreshing reads every participant again.
    assert_eq!(request(true, cx), 3);

    // A failed account is read again by the next request, without the loaded ones.
    fail_next_open.store(true, Ordering::SeqCst);
    assert_eq!(request(true, cx), 2);
    let states = root.read_with(cx, |root, _| row_states(root));
    assert_eq!(
        states
            .iter()
            .filter(|state| **state == "unavailable")
            .count(),
        1,
        "{states:?}"
    );
    assert_eq!(request(false, cx), 1);
    assert_eq!(root.read_with(cx, |root, _| row_states(root)), ["ready"; 3]);

    // A successful action reads its actor once over the action's own client, even when it
    // completes while another chain is selected. No participant route opens.
    let (wallet_id, wallet_generation, chain_id, key) = root.read_with(cx, |root, _| {
        (
            root.selected_wallet_id.clone().unwrap(),
            root.active_wallet_generation,
            root.last_evm_chain,
            root.governance.proposal_participation.key.clone().unwrap(),
        )
    });
    let target = GovernanceRefreshTarget::Proposal(key.clone());
    let action =
        wallet_ops::OperationHttpClientSlot::for_tests(mock.http.clone(), mock.client.clone());
    let opened = opens.load(Ordering::SeqCst);
    root.update(cx, |root, cx| {
        root.selected_network = crate::root::SelectedNetwork::Evm(chain_id + 1);
        root.last_evm_chain = chain_id + 1;
        root.invalidate_governance_context();
        root.record_governance_action_success(
            &wallet_id,
            wallet_generation,
            &target,
            &proposal,
            accounts[0],
            &mock.effective_chain,
            &action,
            cx,
        );
        assert!(!has_loaded_row(root, &key, &proposal, accounts[0]));
    });
    settle_governance(&root, cx, &runtime, |root| {
        has_loaded_row(root, &key, &proposal, accounts[0])
    });
    assert_eq!(opens.load(Ordering::SeqCst), opened);
    let sensitive = mock
        .take()
        .into_iter()
        .filter(|call| call.endpoint == "sensitive")
        .collect::<Vec<_>>();
    assert!(!sensitive.is_empty());
    assert!(
        sensitive.iter().all(|call| call.client == "isolated"),
        "{sensitive:?}"
    );
    root.update(cx, |root, _| {
        root.selected_network = crate::root::SelectedNetwork::Evm(chain_id);
        root.last_evm_chain = chain_id;
        root.invalidate_governance_context();
        root.show_proposal_participation(&proposal);
        assert_eq!(row_states(root), ["ready"; 3]);
    });

    // A wallet change discards the loaded rows, and an action from the previous wallet
    // session reads nothing.
    root.update(cx, |root, cx| {
        root.advance_active_wallet_generation();
        root.reset_wallet_scoped_state(cx);
        root.record_governance_action_success(
            &wallet_id,
            wallet_generation,
            &target,
            &proposal,
            accounts[0],
            &mock.effective_chain,
            &action,
            cx,
        );
        root.show_proposal_participation(&proposal);
    });
    drive_governance(cx, &runtime);
    assert_eq!(opens.load(Ordering::SeqCst), opened);
    assert!(mock.take().iter().all(|call| call.endpoint != "sensitive"));
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root)),
        ["not loaded"; 3]
    );

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}

#[gpui::test]
fn an_action_reads_its_actor_once_whatever_pages_land_meanwhile(cx: &mut gpui::TestAppContext) {
    use crate::root::governance_action::{ProposalParticipationRow, proposal_participation_key};
    use std::sync::atomic::AtomicUsize;

    let path = std::env::temp_dir().join(format!(
        "railoxide-proposal-participation-action-{:032x}",
        rand::random::<u128>()
    ));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mock = governance_rpc_mock();
    let opens = Arc::new(AtomicUsize::new(0));
    let http = {
        let opens = Arc::clone(&opens);
        wallet_ops::HttpContext::with_rpc_client_for_tests(
            tagged_client("shared"),
            wallet_ops::WalletNetworkMode::Direct,
        )
        .with_operation_clients_for_tests(move || {
            let open = opens.fetch_add(1, Ordering::SeqCst);
            Ok(wallet_ops::OperationHttpClient::for_tests(
                tagged_client(&format!("isolated-{open}")),
                wallet_ops::OperationNetworkIsolation::Unavailable(
                    wallet_ops::WalletNetworkMode::Direct,
                ),
            ))
        })
    };
    cx.update(gpui_component::init);
    let mut root = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let wallet = crate::root::tests::public_accounts::fixture_root(&path, &runtime, window, cx);
        root = Some(wallet.clone());
        gpui_component::Root::new(wallet, window, cx)
    });
    let root = root.unwrap();
    let accounts = [1_u8, 2, 3].map(Address::repeat_byte);
    root.update(cx, |root, cx| {
        root.http = http;
        root.effective_chain_configs = std::iter::once(mock.effective_chain.clone()).collect();
        root.ui_state.governance_participants.insert(
            "preview".to_owned(),
            (1..=3).map(|index| format!("account-{index}")).collect(),
        );
        root.governance.tab = GovernanceTab::Staking;
        root.select_governance_tab(GovernanceTab::Proposals, cx);
    });
    settle_governance(&root, cx, &runtime, |root| {
        root.proposals.rows(0).is_some_and(|rows| !rows.is_empty())
    });
    let identity = root.read_with(cx, |root, _| root.proposals.rows(0).unwrap()[0].identity());
    root.update_in(cx, |root, window, cx| {
        root.select_proposal(0, &identity, window, cx);
    });
    let (proposal, key, wallet_id, wallet_generation) = root.read_with(cx, |root, _| {
        let proposal = root.proposals.selected_proposal().cloned().unwrap();
        let key = proposal_participation_key(&proposal, root.governance_context_key());
        (
            proposal,
            key,
            root.selected_wallet_id.clone().unwrap(),
            root.active_wallet_generation,
        )
    });
    // The proposal once its vote is called.
    let called = wallet_ops::GovernanceProposal {
        voting_snapshot_interval: U256::from(7),
        ..proposal.clone()
    };
    let row_states = |root: &WalletRoot, proposal: &wallet_ops::GovernanceProposal| {
        accounts
            .iter()
            .map(|account| {
                match root
                    .governance
                    .proposal_participation
                    .row(proposal, account)
                {
                    None => "not loaded",
                    Some(ProposalParticipationRow::Loading) => "loading",
                    Some(ProposalParticipationRow::Ready(_)) => "ready",
                    Some(ProposalParticipationRow::Unavailable(_)) => "unavailable",
                }
            })
            .collect::<Vec<_>>()
    };
    let record_success = |actor: Address, client: &str, cx: &mut gpui::VisualTestContext| {
        let operation = wallet_ops::OperationHttpClientSlot::for_tests(
            mock.http.clone(),
            wallet_ops::OperationHttpClient::for_tests(
                tagged_client(client),
                wallet_ops::OperationNetworkIsolation::Unavailable(
                    wallet_ops::WalletNetworkMode::Direct,
                ),
            ),
        );
        root.update(cx, |root, cx| {
            root.record_governance_action_success(
                &wallet_id,
                wallet_generation,
                &GovernanceRefreshTarget::Proposal(key.clone()),
                &proposal,
                actor,
                &mock.effective_chain,
                &operation,
                cx,
            );
        });
    };

    // One participant is read while the next participant's read is held at the endpoint. A
    // refresh then waits for the held read, opening no route until it is released.
    let first = opens.load(Ordering::SeqCst);
    mock.hold(Some(format!("isolated-{}", first + 1)));
    root.update(cx, |root, cx| {
        root.request_proposal_participation(false, cx);
    });
    settle_governance(&root, cx, &runtime, |_| {
        opens.load(Ordering::SeqCst) == first + 2
    });
    root.update(cx, |root, cx| {
        root.request_proposal_participation(true, cx);
    });
    drive_governance(cx, &runtime);
    assert_eq!(opens.load(Ordering::SeqCst), first + 2);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root, &proposal)),
        ["loading"; 3]
    );
    mock.hold(None);
    settle_governance(&root, cx, &runtime, |root| {
        !root.governance.proposal_participation.loading()
    });
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root, &proposal)),
        ["ready"; 3]
    );
    let actor = accounts[0];
    let opened = opens.load(Ordering::SeqCst);
    mock.take();

    // A call-vote completes. The action reads the actor once, over its own client, and sees
    // the called vote.
    mock.chain.voting_interval.store(7, Ordering::SeqCst);
    record_success(actor, "action", cx);
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root, &proposal)),
        ["not loaded", "ready", "ready"]
    );
    settle_governance(&root, cx, &runtime, |root| {
        has_loaded_row(root, &key, &called, actor)
    });
    let calls = mock
        .take()
        .into_iter()
        .filter(|call| call.endpoint == "sensitive")
        .collect::<Vec<_>>();
    assert!(!calls.is_empty());
    for call in &calls {
        assert_eq!(call.client, "action", "{call:?}");
    }

    // A full list refresh that read the proposal before the vote was called lands afterwards.
    // Pages start no participation read, so the stale page shows the actor as not loaded
    // without asking to read it, and loses nothing.
    mock.chain.voting_interval.store(0, Ordering::SeqCst);
    root.update(cx, |root, cx| root.start_proposals_refresh(true, cx));
    settle_governance(&root, cx, &runtime, |root| {
        !root.proposals.loading && root.proposals.selected_proposal().is_some()
    });
    assert!(mock.take().iter().all(|call| call.endpoint != "sensitive"));
    root.read_with(cx, |root, _| {
        let shown = root.proposals.selected_proposal().unwrap();
        assert_eq!(shown.voting_snapshot_interval, U256::ZERO);
        assert_eq!(row_states(root, shown), ["not loaded", "ready", "ready"]);
        assert_eq!(
            root.governance.proposal_participation.unloaded(shown),
            [] as [Address; 0]
        );
    });

    // The page with the called vote shows the actor's row without another read.
    mock.chain.voting_interval.store(7, Ordering::SeqCst);
    root.update(cx, WalletRoot::refresh_selected_proposal_page);
    settle_governance(&root, cx, &runtime, |root| {
        root.proposals
            .selected_proposal()
            .is_some_and(|shown| shown.voting_snapshot_interval == U256::from(7))
    });
    assert_eq!(
        root.read_with(cx, |root, _| row_states(root, &called)[0]),
        "ready"
    );
    assert!(mock.take().iter().all(|call| call.endpoint != "sensitive"));
    assert_eq!(opens.load(Ordering::SeqCst), opened);

    // A wallet change while an action's read is outstanding discards its result.
    let reads = calls.len();
    record_success(actor, "late", cx);
    root.update(cx, |root, cx| {
        root.advance_active_wallet_generation();
        root.reset_wallet_scoped_state(cx);
    });
    settle_governance(&root, cx, &runtime, |_| {
        mock.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.client == "late")
            .count()
            == reads
    });
    drive_governance(cx, &runtime);
    assert!(!root.read_with(cx, |root, _| has_loaded_row(root, &key, &called, actor)));

    cx.update(|window, _| window.remove_window());
    drop(root);
    cx.run_until_parked();
    std::fs::remove_dir_all(path).unwrap();
}
