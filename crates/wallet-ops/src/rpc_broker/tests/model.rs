use super::*;
use alloy::eips::{BlockId, RpcBlockHash};
use alloy::primitives::{B256, TxKind};
use alloy::rpc::types::eth::state::{AccountOverride, StateOverride};
use alloy::rpc::types::eth::transaction::{
    AccessList, AccessListItem, TransactionInput, TransactionRequest,
};

fn admission_submission(reads: Vec<RpcRead>) -> RpcSubmission {
    RpcSubmission::new(test_route(), reads, test_origin())
}

fn typed_read(
    calldata_len: usize,
    access_list: Option<AccessList>,
    state_overrides: Option<StateOverride>,
) -> RpcRead {
    RpcRead::from_rpc(
        WithOtherFields::new(TransactionRequest {
            to: Some(Address::ZERO.into()),
            input: TransactionInput::new(Bytes::from(vec![0; calldata_len])),
            access_list,
            ..TransactionRequest::default()
        }),
        BlockId::latest(),
        state_overrides,
        1,
    )
    .expect("typed test read")
}

#[test]
fn admission_limits_enforce_read_count_and_decoded_boundaries() {
    assert_eq!(
        RpcBrokerError::AdmissionRejected.to_string(),
        "RPC submission exceeds admission limits"
    );
    assert_eq!(
        RpcBrokerError::AdmissionRejected.failure_class(),
        FailureClass::Unrecoverable
    );

    let exact_reads = (0..64)
        .map(|_| RpcRead::get_balance(Address::ZERO))
        .collect();
    assert_eq!(
        admission_submission(exact_reads).validate_admission(),
        Ok(())
    );
    let wallet_over_count: Vec<RpcRead> = (0..65)
        .map(|_| RpcRead::get_balance(Address::ZERO))
        .collect();
    assert_eq!(
        admission_submission(wallet_over_count.clone()).validate_admission(),
        Ok(())
    );
    let dapp_over_count = RpcSubmission::new(
        test_route(),
        wallet_over_count,
        RpcOrigin::dapp("peer", "https://example.com").unwrap(),
    );
    assert_eq!(
        dapp_over_count.validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );

    let exact_read = RpcRead::eth_call(Address::ZERO, Bytes::from(vec![0; 128 * 1024]));
    assert_eq!(
        admission_submission(vec![exact_read]).validate_admission(),
        Ok(())
    );
    let oversized_read = RpcRead::eth_call(Address::ZERO, Bytes::from(vec![0; 128 * 1024 + 1]));
    assert_eq!(
        admission_submission(vec![oversized_read]).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );

    let exact_total = (0..4)
        .map(|_| RpcRead::eth_call(Address::ZERO, Bytes::from(vec![0; 128 * 1024])))
        .collect();
    assert_eq!(
        admission_submission(exact_total).validate_admission(),
        Ok(())
    );
    let oversized_total = (0..4)
        .map(|_| RpcRead::eth_call(Address::ZERO, Bytes::from(vec![0; 128 * 1024])))
        .chain(std::iter::once(RpcRead::eth_call(
            Address::ZERO,
            Bytes::from_static(b"x"),
        )))
        .collect();
    assert_eq!(
        admission_submission(oversized_total).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );
}

#[test]
fn admission_accounts_typed_access_list_and_state_override_input() {
    let access_list = AccessList(vec![AccessListItem {
        address: Address::from([1; 20]),
        storage_keys: Vec::new(),
    }]);
    let exact_access_list = typed_read(128 * 1024 - 20, Some(access_list), None);
    assert_eq!(
        admission_submission(vec![exact_access_list]).validate_admission(),
        Ok(())
    );
    let access_list = AccessList(vec![AccessListItem {
        address: Address::from([1; 20]),
        storage_keys: Vec::new(),
    }]);
    let oversized_access_address = typed_read(128 * 1024 - 19, Some(access_list), None);
    assert_eq!(
        admission_submission(vec![oversized_access_address]).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );
    let access_list = AccessList(vec![AccessListItem {
        address: Address::from([1; 20]),
        storage_keys: vec![B256::ZERO],
    }]);
    let oversized_access_list = typed_read(128 * 1024 - 20, Some(access_list), None);
    assert_eq!(
        admission_submission(vec![oversized_access_list]).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );

    let account = Address::from([2; 20]);
    let mut state_overrides = StateOverride::default();
    state_overrides.insert(
        account,
        AccountOverride {
            code: Some(Bytes::from_static(b"abc")),
            state: Some(std::iter::once((B256::ZERO, B256::ZERO)).collect()),
            state_diff: Some(std::iter::once((B256::from([1; 32]), B256::from([2; 32]))).collect()),
            ..AccountOverride::default()
        },
    );
    let exact_state = typed_read(128 * 1024 - (3 + 64 + 64), None, Some(state_overrides));
    assert_eq!(
        admission_submission(vec![exact_state]).validate_admission(),
        Ok(())
    );

    let mut state_overrides = StateOverride::default();
    state_overrides.insert(
        account,
        AccountOverride {
            code: Some(Bytes::from_static(b"abc")),
            ..AccountOverride::default()
        },
    );
    let oversized_code = typed_read(128 * 1024 - 2, None, Some(state_overrides));
    assert_eq!(
        admission_submission(vec![oversized_code]).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );

    let mut state_overrides = StateOverride::default();
    state_overrides.insert(
        account,
        AccountOverride {
            state_diff: Some(std::iter::once((B256::ZERO, B256::ZERO)).collect()),
            ..AccountOverride::default()
        },
    );
    let oversized_state_diff = typed_read(128 * 1024 - 63, None, Some(state_overrides));
    assert_eq!(
        admission_submission(vec![oversized_state_diff]).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );

    let mut state_overrides = StateOverride::default();
    state_overrides.insert(
        account,
        AccountOverride {
            state: Some(
                [
                    (B256::ZERO, B256::ZERO),
                    (B256::from([1; 32]), B256::from([2; 32])),
                ]
                .into_iter()
                .collect(),
            ),
            ..AccountOverride::default()
        },
    );
    let oversized_state = typed_read(128 * 1024 - 64, None, Some(state_overrides));
    assert_eq!(
        admission_submission(vec![oversized_state]).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );
}

#[test]
fn dapp_origins_preserve_full_parser_normalized_urls() {
    for (input, normalized) in [
        ("HTTPS://EXAMPLE.COM:443", "https://example.com/"),
        ("HTTP://EXAMPLE.COM:80", "http://example.com/"),
        ("https://例え.テスト", "https://xn--r8jz45g.xn--zckzah/"),
        ("https://[2001:db8::1]:443/", "https://[2001:db8::1]/"),
        ("ftp://example.com", "ftp://example.com/"),
        ("http:opaque", "http://opaque/"),
        ("custom:opaque", "custom:opaque"),
        ("file:///tmp/dapp", "file:///tmp/dapp"),
        (
            "https://user:secret@example.com:0/path?token=secret#fragment",
            "https://user:secret@example.com:0/path?token=secret#fragment",
        ),
        ("https://@example.com", "https://example.com/"),
        (
            "https://example.com/../secret",
            "https://example.com/secret",
        ),
        (
            "https://example.com/%2e%2e/secret",
            "https://example.com/secret",
        ),
        ("https://example.com//", "https://example.com//"),
        ("https://example.com\\secret", "https://example.com/secret"),
        ("https://example.com:", "https://example.com/"),
        ("https://example.com:/", "https://example.com/"),
        ("https://example.com?query", "https://example.com/?query"),
        (
            "https://example.com#fragment",
            "https://example.com/#fragment",
        ),
        ("https://example.com\t", "https://example.com/"),
    ] {
        let origin = RpcOrigin::dapp("peer", input).unwrap();
        assert_eq!(origin.web_origin().unwrap().as_str(), normalized);
        assert_eq!(origin, RpcOrigin::dapp("peer", normalized).unwrap());
    }

    let base = RpcOrigin::dapp("peer", "https://example.com/").unwrap();
    for distinct in [
        "http://example.com/",
        "https://other.example/",
        "https://user@example.com/",
        "https://:secret@example.com/",
        "https://example.com:8080/",
        "https://example.com/path",
        "https://example.com/?query",
        "https://example.com/#fragment",
    ] {
        assert_ne!(base, RpcOrigin::dapp("peer", distinct).unwrap());
    }
    assert_ne!(
        RpcOrigin::dapp("peer", "custom:first").unwrap(),
        RpcOrigin::dapp("peer", "custom:second").unwrap()
    );
    assert_ne!(
        base,
        RpcOrigin::dapp("other-peer", "https://example.com/").unwrap()
    );
    let wallet = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    assert_ne!(base, wallet);
    assert_ne!(wallet, RpcOrigin::from(WalletRpcOrigin::Staking));
}

#[test]
fn dapp_origins_reject_parser_errors_without_echoing_input() {
    for input in [
        "null",
        "/relative?token=secret",
        "https://user:secret@",
        "https://example.com:invalid/secret",
        "https://[invalid]/secret",
    ] {
        let error = RpcOrigin::dapp("peer", input).unwrap_err();
        assert_eq!(error, RpcOriginError::InvalidOrigin);
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains(input));
            assert!(!rendered.contains("secret"));
        }
    }
    let error = RpcOrigin::dapp("", "https://example.com").unwrap_err();
    assert_eq!(error, RpcOriginError::EmptyPeerId);
}

#[test]
fn expired_aggregate_member_cannot_be_claimed_by_its_stale_origin_lane() {
    let origin_a = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let origin_b = RpcOrigin::from(WalletRpcOrigin::Staking);
    let route = test_route_with_multicall(Address::from([6_u8; 20]));
    let mut nonce = 1;
    let mut item = |marker: &'static [u8], origin: RpcOrigin, deadline| {
        let read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(marker));
        let key = WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce,
            latest_epoch: None,
        };
        nonce += 1;
        WorkItem {
            key,
            execution_route: route.clone(),
            read,
            origins: vec![origin],
            waiters: WaiterState::new(WaiterPolicy {
                deadline,
                attempt_timeout: Duration::from_secs(1),
            }),
        }
    };
    let expired = item(
        b"expired",
        origin_a.clone(),
        Some(Instant::now() - Duration::from_secs(1)),
    );
    let survivor = item(b"survivor", origin_b, None);
    let backlog = item(b"backlog", origin_a, None);
    let mut scheduler = ReadyScheduler::new();
    scheduler.admit_chunks(vec![vec![expired, survivor], vec![backlog]]);
    scheduler.expire(Instant::now());

    let first = scheduler.next(0, 1).expect("origin A backlog");
    assert_eq!(
        read_calldata(&first.first().unwrap().read),
        Bytes::from_static(b"backlog")
    );
    let second = scheduler.next(0, 1).expect("origin B survivor");
    assert_eq!(
        read_calldata(&second.first().unwrap().read),
        Bytes::from_static(b"survivor")
    );
}

#[test]
fn expired_individual_generation_cannot_claim_a_later_same_key_admission() {
    let origin_a = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let origin_b = RpcOrigin::from(WalletRpcOrigin::Staking);
    let route = test_route();
    let make_item = |marker: &'static [u8], origin: RpcOrigin, nonce: u64, deadline| {
        let read = rpc_read_from_request(
            TransactionRequest {
                to: Some(Address::ZERO.into()),
                value: Some(U256::from(1)),
                input: TransactionInput::new(Bytes::from_static(marker)),
                ..TransactionRequest::default()
            },
            BlockId::latest(),
            None,
        );
        let key = WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce,
            latest_epoch: None,
        };
        WorkItem {
            key,
            execution_route: route.clone(),
            read,
            origins: vec![origin],
            waiters: WaiterState::new(WaiterPolicy {
                deadline,
                attempt_timeout: Duration::from_secs(1),
            }),
        }
    };
    let old = make_item(
        b"same-key",
        origin_a.clone(),
        0,
        Some(Instant::now() - Duration::from_secs(1)),
    );
    let key = old.key.clone();
    let backlog = make_item(b"a-backlog", origin_a, 2, None);
    let later = make_item(b"same-key", origin_b, 0, None);
    assert!(key == later.key);
    let mut scheduler = ReadyScheduler::new();
    scheduler.admit_chunks(vec![vec![old], vec![backlog]]);
    scheduler.expire(Instant::now());
    scheduler.admit_chunks(vec![vec![later]]);

    let first = scheduler.next(0, 1).expect("origin A backlog");
    assert_eq!(
        read_calldata(&first.first().unwrap().read),
        Bytes::from_static(b"a-backlog")
    );
    let second = scheduler.next(0, 1).expect("origin B later generation");
    assert_eq!(
        read_calldata(&second.first().unwrap().read),
        Bytes::from_static(b"same-key")
    );
}

#[test]
fn ready_origins_round_robin_across_aggregate_and_individual_work() {
    let origin_a = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let origin_b = RpcOrigin::from(WalletRpcOrigin::Staking);
    let route_a = test_route_with_multicall(Address::from([3_u8; 20]));
    let route_b = RpcRoute::from(
        RpcChainRoute::new(1, vec![Url::parse("https://other.invalid").unwrap()])
            .with_multicall(Address::from([3_u8; 20])),
    );
    let mut nonce = 0;
    let mut item = |route: RpcRoute, read: RpcRead, origin: RpcOrigin| {
        let key = WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce,
            latest_epoch: None,
        };
        nonce += 1;
        WorkItem {
            key,
            execution_route: route,
            read,
            origins: vec![origin],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: None,
                attempt_timeout: Duration::from_secs(1),
            }),
        }
    };
    let aggregate_a = item(
        route_a.clone(),
        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"a-aggregate")),
        origin_a.clone(),
    );
    let individual_a = item(
        route_a,
        rpc_read_from_request(
            TransactionRequest {
                to: Some(Address::ZERO.into()),
                value: Some(U256::from(1)),
                input: TransactionInput::new(Bytes::from_static(b"a-individual")),
                ..TransactionRequest::default()
            },
            BlockId::latest(),
            None,
        ),
        origin_a,
    );
    let aggregate_b = item(
        route_b,
        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"b-aggregate")),
        origin_b,
    );
    let mut scheduler = ReadyScheduler::new();
    scheduler.partition_and_admit(vec![aggregate_a, individual_a, aggregate_b]);

    let first = scheduler.next(0, 1).expect("first origin work");
    assert_eq!(
        read_calldata(&first.first().expect("first item").read),
        Bytes::from_static(b"a-aggregate")
    );
    let second = scheduler.next(0, 1).expect("second origin work");
    assert_eq!(
        read_calldata(&second.first().expect("second item").read),
        Bytes::from_static(b"b-aggregate")
    );
    let third = scheduler.next(0, 1).expect("first origin's next work");
    assert_eq!(
        read_calldata(&third.first().expect("third item").read),
        Bytes::from_static(b"a-individual")
    );
}

#[test]
fn partition_grouping_preserves_first_seen_group_and_member_order() {
    let origin = test_origin();
    let route_a = test_route_with_multicall(Address::from([7_u8; 20]));
    let route_b = test_route_with_multicall(Address::from([8_u8; 20]));
    let mut nonce = 0;
    let mut item = |route: RpcRoute, marker: &'static [u8]| {
        let read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(marker));
        let key = WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce,
            latest_epoch: None,
        };
        nonce += 1;
        WorkItem {
            key,
            execution_route: route,
            read,
            origins: vec![origin.clone()],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: None,
                attempt_timeout: Duration::from_secs(1),
            }),
        }
    };
    let mut scheduler = ReadyScheduler::new();
    scheduler.partition_and_admit(vec![
        item(route_a.clone(), b"a1"),
        item(route_b.clone(), b"b1"),
        item(route_a, b"a2"),
        item(route_b, b"b2"),
    ]);

    let first = scheduler.next(0, 1).expect("first aggregate");
    let ExecutionJob::Aggregate(first) = first else {
        panic!("expected first group to be aggregate");
    };
    assert_eq!(
        first
            .iter()
            .map(|item| read_calldata(&item.read))
            .collect::<Vec<_>>(),
        vec![Bytes::from_static(b"a1"), Bytes::from_static(b"a2")]
    );

    let second = scheduler.next(0, 1).expect("second aggregate");
    let ExecutionJob::Aggregate(second) = second else {
        panic!("expected second group to be aggregate");
    };
    assert_eq!(
        second
            .iter()
            .map(|item| read_calldata(&item.read))
            .collect::<Vec<_>>(),
        vec![Bytes::from_static(b"b1"), Bytes::from_static(b"b2")]
    );
}

#[test]
fn ready_shared_work_is_claimed_once_by_the_attached_origin_lane() {
    let origin_a = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let origin_b = RpcOrigin::from(WalletRpcOrigin::Staking);
    let route = test_route();
    let mut nonce = 0;
    let mut item = |read: RpcRead, origin: RpcOrigin| {
        let key = WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce,
            latest_epoch: None,
        };
        nonce += 1;
        WorkItem {
            key,
            execution_route: route.clone(),
            read,
            origins: vec![origin],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: None,
                attempt_timeout: Duration::from_secs(1),
            }),
        }
    };
    let first = item(
        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"backlog")),
        origin_a.clone(),
    );
    let second = item(
        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"backlog-2")),
        origin_a.clone(),
    );
    let shared = item(
        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"shared")),
        origin_a,
    );
    let shared_key = shared.key.clone();
    let mut scheduler = ReadyScheduler::new();
    scheduler.partition_and_admit(vec![first, second, shared]);
    assert!(scheduler.attach(&shared_key, &origin_b));

    let first_job = scheduler.next(0, 1).expect("origin A backlog");
    assert_eq!(
        read_calldata(&first_job.first().expect("first item").read),
        Bytes::from_static(b"backlog")
    );
    let shared_job = scheduler.next(0, 1).expect("origin B shared work");
    assert_eq!(
        read_calldata(&shared_job.first().expect("shared item").read),
        Bytes::from_static(b"shared")
    );
    let second_job = scheduler.next(0, 1).expect("origin A second backlog");
    assert_eq!(
        read_calldata(&second_job.first().expect("second item").read),
        Bytes::from_static(b"backlog-2")
    );
    assert!(scheduler.next(0, 1).is_none());
}

#[test]
fn ready_shared_aggregate_is_claimed_once_by_the_attached_origin_lane() {
    let origin_a = RpcOrigin::from(WalletRpcOrigin::PublicWallet);
    let origin_b = RpcOrigin::from(WalletRpcOrigin::Staking);
    let route = |endpoint: &str| {
        RpcRoute::from(
            RpcChainRoute::new(1, vec![Url::parse(endpoint).unwrap()])
                .with_multicall(Address::from([4_u8; 20])),
        )
        .with_test_thresholds(1, DEFAULT_MAX_ESTIMATED_GAS)
    };
    let mut nonce = 0;
    let mut item = |route: RpcRoute, marker: &'static [u8], origin: RpcOrigin| {
        let read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(marker));
        let key = WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce,
            latest_epoch: None,
        };
        nonce += 1;
        WorkItem {
            key,
            execution_route: route,
            read,
            origins: vec![origin],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: None,
                attempt_timeout: Duration::from_secs(1),
            }),
        }
    };
    let first = item(
        route("https://aggregate-a1.invalid"),
        b"aggregate-a1",
        origin_a.clone(),
    );
    let second = item(
        route("https://aggregate-a2.invalid"),
        b"aggregate-a2",
        origin_a.clone(),
    );
    let shared = item(
        route("https://aggregate-shared.invalid"),
        b"aggregate-shared",
        origin_a,
    );
    let shared_key = shared.key.clone();
    let mut scheduler = ReadyScheduler::new();
    scheduler.partition_and_admit(vec![first, second, shared]);
    assert!(scheduler.attach(&shared_key, &origin_b));

    let first_job = scheduler.next(0, 1).expect("origin A first aggregate");
    assert_eq!(
        read_calldata(&first_job.first().expect("first item").read),
        Bytes::from_static(b"aggregate-a1")
    );
    let shared_job = scheduler.next(0, 1).expect("origin B shared aggregate");
    assert_eq!(
        read_calldata(&shared_job.first().expect("shared item").read),
        Bytes::from_static(b"aggregate-shared")
    );
    let second_job = scheduler.next(0, 1).expect("origin A second aggregate");
    assert_eq!(
        read_calldata(&second_job.first().expect("second item").read),
        Bytes::from_static(b"aggregate-a2")
    );
    assert!(scheduler.next(0, 1).is_none());
}

#[test]
fn identity_matches_equal_requests_and_includes_block_caller_and_gas() {
    let target = Address::from([1_u8; 20]);
    let route = test_route();
    let first = RpcRead::eth_call(target, Bytes::from_static(b"abcd"));
    let equal = RpcRead::eth_call(target, Bytes::from_static(b"abcd"));
    let first_identity = first.identity_for_route(&route);
    let equal_identity = equal.identity_for_route(&route);
    assert_eq!(first_identity, equal_identity);
    let cached = std::collections::HashMap::from([(first_identity, Bytes::from_static(b"result"))]);
    assert_eq!(
        cached.get(&equal_identity),
        Some(&Bytes::from_static(b"result"))
    );
    let caller = rpc_read_from_request(
        TransactionRequest {
            from: Some(Address::from([2_u8; 20])),
            to: Some(target.into()),
            input: TransactionInput::maybe_both(Some(Bytes::from_static(b"abcd"))),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        None,
    );
    assert_ne!(
        first.identity_for_route(&route),
        caller.identity_for_route(&route)
    );
    let gas = rpc_read_from_request(
        TransactionRequest {
            to: Some(target.into()),
            gas: Some(100_001),
            input: TransactionInput::maybe_both(Some(Bytes::from_static(b"abcd"))),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        None,
    );
    assert_ne!(
        first.identity_for_route(&route),
        gas.identity_for_route(&route)
    );
    let block = first.clone().with_test_block(BlockNumberOrTag::Number(1));
    assert_ne!(
        first.identity_for_route(&route),
        block.identity_for_route(&route)
    );
    for different in [&caller, &gas, &block] {
        assert!(!cached.contains_key(&different.identity_for_route(&route)));
    }
    assert_ne!(
        RpcRead::eth_call(target, Bytes::new()).identity_for_route(&route),
        RpcRead::get_balance(target).identity_for_route(&route)
    );
}

#[tokio::test]
async fn route_selection_spreads_requests_across_configured_endpoints() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let first_counter = first_calls.clone();
    let second_counter = second_calls.clone();
    let first_responder: RpcResponder = Arc::new(move |request| {
        first_counter.fetch_add(1, Ordering::SeqCst);
        rpc_result(&request, &json!("0x01"))
    });
    let second_responder: RpcResponder = Arc::new(move |request| {
        second_counter.fetch_add(1, Ordering::SeqCst);
        rpc_result(&request, &json!("0x02"))
    });
    let (first, first_server) =
        spawn_rpc_mock(first_responder, active.clone(), maximum.clone()).await;
    let (second, second_server) = spawn_rpc_mock(second_responder, active, maximum).await;
    let broker = test_broker(Duration::from_millis(1), 1);
    let route = RpcRoute::from(RpcChainRoute::new(1, vec![first, second]));
    for marker in 0_u8..2 {
        assert!(
            broker
                .submit(RpcSubmission::new(
                    route.clone(),
                    vec![RpcRead::eth_call(Address::ZERO, Bytes::from(vec![marker]))],
                    test_origin(),
                ))
                .await
                .expect("load spreading submission")[0]
                .is_ok()
        );
    }

    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    drop(broker);
    first_server.abort();
    second_server.abort();
}

#[test]
fn eligibility_table_covers_value_override_caller_and_balance_cases() {
    let target = Address::from([1_u8; 20]);
    let caller = Address::from([2_u8; 20]);
    let allowed = rpc_read_from_request(
        TransactionRequest {
            from: Some(caller),
            to: Some(target.into()),
            input: TransactionInput::new(Bytes::from_static(b"\x70\xa0\x82\x31")),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        None,
    );
    let unknown = rpc_read_from_request(
        TransactionRequest {
            from: Some(caller),
            to: Some(target.into()),
            input: TransactionInput::new(Bytes::from_static(b"\x12\x34\x56\x78")),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        None,
    );
    let value = rpc_read_from_request(
        TransactionRequest {
            to: Some(target.into()),
            value: Some(U256::from(1)),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        None,
    );
    let mut state_overrides = StateOverride::default();
    state_overrides.insert(target, AccountOverride::default());
    let override_read = rpc_read_from_request(
        TransactionRequest {
            to: Some(target.into()),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        Some(state_overrides),
    );
    for (name, read, expected) in [
        ("no caller", RpcRead::eth_call(target, Bytes::new()), true),
        ("allowed caller selector", allowed, true),
        ("unknown caller selector", unknown, false),
        ("nonzero value", value, false),
        ("state override", override_read, false),
        ("get balance", RpcRead::get_balance(target), true),
    ] {
        assert_eq!(read.is_multicall_eligible(), expected, "{name}");
    }
}

#[test]
fn explicit_gas_is_serialized_and_excluded_from_multicall() {
    let read = rpc_read_from_request(
        TransactionRequest {
            to: Some(Address::ZERO.into()),
            gas: Some(55_000),
            input: TransactionInput::new(Bytes::from_static(b"\x70\xa0\x82\x31")),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        None,
    );
    assert!(!read.is_multicall_eligible());
    let (_, params, _) = wire_request_for(&read);
    assert_eq!(params[0]["gas"], json!("0xd6d8"));
}

#[test]
fn typed_eth_call_parser_preserves_unknown_fields_and_rejects_conflicting_input() {
    let route_chain_id = 1;
    let valid = serde_json::json!([
        {
            "to": format!("{:#x}", Address::ZERO),
            "data": "0x0102",
            "input": "0x0102",
            "chainId": "0x1",
            "gasPrice": "0x2"
        },
        "latest"
    ]);
    let read = RpcRead::from_rpc_params(valid, route_chain_id).expect("valid typed call");
    assert!(!read.is_multicall_eligible());
    let (_, params, _) = wire_request_for(&read);
    assert_eq!(params[0]["gasPrice"], serde_json::json!("0x2"));
    assert_eq!(params[1], serde_json::json!("latest"));

    let conflicting = serde_json::json!([
        { "to": format!("{:#x}", Address::ZERO), "data": "0x01", "input": "0x02" },
        "latest"
    ]);
    let error = RpcRead::from_rpc_params(conflicting, route_chain_id).unwrap_err();
    assert_eq!(error.code(), -32602);

    let unknown = serde_json::json!([
        { "to": format!("{:#x}", Address::ZERO), "proof": {"nested": ["0x0001", null]}, "context": "chain-specific", "optional": null },
        "latest"
    ]);
    let read = RpcRead::from_rpc_params(unknown.clone(), route_chain_id).unwrap();
    let (_, params, _) = wire_request_for(&read);
    for field in ["proof", "context", "optional"] {
        assert_eq!(params[0].get(field), unknown[0].get(field));
    }
    assert!(!read.is_dedupable());
    assert!(!read.is_cacheable());
    assert!(!read.is_multicall_eligible());
    assert!(matches!(
        read.identity_for_route(&test_route()),
        ReadIdentity::Individual { .. }
    ));
}

#[test]
fn typed_eth_call_forwards_all_common_fields_and_defaults_latest() {
    let target = Address::from([3_u8; 20]);
    let sender = Address::from([4_u8; 20]);
    let valid = serde_json::json!([{
        "from": format!("{sender:#x}"),
        "to": format!("{target:#x}"),
        "data": "0x0102",
        "value": "0x7",
        "gas": "0x5208",
        "gasPrice": "0x9",
        "chainId": "0x1",
        "nonce": "0x2",
        "type": "0x2",
        "accessList": [{ "address": format!("{target:#x}"), "storageKeys": [] }],
        "maxFeePerGas": "0xa",
        "maxPriorityFeePerGas": "0xb"
    }]);
    let read = RpcRead::from_rpc_params(valid, 1).expect("valid one-parameter eth_call");
    assert!(!read.is_multicall_eligible());
    let (_, params, _) = wire_request_for(&read);
    assert_eq!(params[1], serde_json::json!("latest"));
    for (field, expected) in [
        ("from", serde_json::json!(format!("{sender:#x}"))),
        ("to", serde_json::json!(format!("{target:#x}"))),
        ("data", serde_json::json!("0x0102")),
        ("value", serde_json::json!("0x7")),
        ("gas", serde_json::json!("0x5208")),
        ("gasPrice", serde_json::json!("0x9")),
        ("chainId", serde_json::json!("0x1")),
        ("nonce", serde_json::json!("0x2")),
        ("type", serde_json::json!("0x2")),
        ("maxFeePerGas", serde_json::json!("0xa")),
        ("maxPriorityFeePerGas", serde_json::json!("0xb")),
    ] {
        assert_eq!(params[0][field], expected, "forwarded {field}");
    }
    assert_eq!(
        params[0]["accessList"][0]["address"],
        format!("{target:#x}")
    );
    assert_eq!(
        params[0]["accessList"][0]["storageKeys"],
        serde_json::json!([])
    );
}

#[test]
fn typed_eth_call_accepts_contract_creation_but_keeps_it_individual() {
    let read = RpcRead::from_rpc_params(
        serde_json::json!([{ "from": format!("{:#x}", Address::ZERO), "input": "0x6000" }, "latest"]),
        1,
    )
    .expect("contract creation call");
    assert!(!read.is_multicall_eligible());
    let (_, params, _) = wire_request_for(&read);
    assert!(
        !params[0]
            .as_object()
            .expect("transaction object")
            .contains_key("to")
    );
    assert_eq!(params[0]["input"], serde_json::json!("0x6000"));

    let explicit_creation = rpc_read_from_request(
        TransactionRequest {
            to: Some(TxKind::Create),
            input: TransactionInput::new(Bytes::from_static(b"\x60\x00")),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        None,
    );
    assert!(!explicit_creation.is_multicall_eligible());
}

#[test]
fn typed_eth_call_rejects_block_overrides_and_chain_mismatch() {
    let block_override = serde_json::json!([{}, "latest", { "timestamp": "0x1" }]);
    assert_eq!(
        RpcRead::from_rpc_params(block_override, 1)
            .expect_err("block overrides are unsupported")
            .code(),
        -32602
    );
    assert_eq!(
        RpcRead::from_rpc_params(serde_json::json!([{ "chainId": "0x2" }, "latest"]), 1)
            .expect_err("route chain mismatch")
            .code(),
        -32000
    );
}

#[test]
fn typed_state_override_is_forwarded_and_never_cacheable_or_aggregateable() {
    let account = Address::from([8_u8; 20]);
    let read = RpcRead::from_rpc_params(
        serde_json::json!([
            { "to": format!("{:#x}", account), "input": "0x01" },
            "latest",
            { (format!("{account:#x}")): { "balance": "0x2", "code": "0x6000" } }
        ]),
        1,
    )
    .expect("typed state override");
    assert!(!read.is_multicall_eligible());
    assert!(!read.is_cacheable());
    assert!(!read.is_dedupable());
    let (_, params, _) = wire_request_for(&read);
    assert_eq!(
        params[2][format!("{account:#x}")]["balance"],
        serde_json::json!("0x2")
    );
    assert_eq!(
        params[2][format!("{account:#x}")]["code"],
        serde_json::json!("0x6000")
    );
}

#[test]
fn block_hash_identity_and_wire_form_retain_require_canonical() {
    let hash = alloy::primitives::B256::from([7_u8; 32]);
    let first = RpcRead::eth_call(Address::ZERO, Bytes::new()).with_test_block((hash, Some(false)));
    let second = RpcRead::eth_call(Address::ZERO, Bytes::new()).with_test_block((hash, Some(true)));
    let route = test_route();
    assert_ne!(
        first.identity_for_route(&route),
        second.identity_for_route(&route)
    );
    let (_, params, _) = wire_request_for(&first);
    assert_eq!(
        params[1],
        serde_json::json!({
            "blockHash": format!("{hash:#x}"),
            "requireCanonical": false
        })
    );
    assert!(first.is_cacheable());
    assert!(!second.is_cacheable());
}

#[test]
fn complete_block_identifiers_are_forwarded_for_numbers_tags_hashes_and_balance() {
    let hash = alloy::primitives::B256::from([9_u8; 32]);
    let route = test_route();
    let blocks = [
        (
            serde_json::json!("0x7"),
            BlockId::Number(BlockNumberOrTag::Number(7)),
        ),
        (
            serde_json::json!("safe"),
            BlockId::Number(BlockNumberOrTag::Safe),
        ),
        (
            serde_json::json!(format!("{hash:#x}")),
            BlockId::Hash(RpcBlockHash::from_hash(hash, None)),
        ),
        (
            serde_json::json!({ "blockHash": format!("{hash:#x}"), "requireCanonical": true }),
            BlockId::Hash(RpcBlockHash::from_hash(hash, Some(true))),
        ),
    ];
    for (expected, block) in blocks {
        let read = RpcRead::eth_call(Address::ZERO, Bytes::new()).with_test_block(block);
        assert_eq!(read.identity_for_route(&route).block(), Some(block));
        assert_eq!(wire_request_for(&read).1[1], expected);
        let balance = RpcRead::get_balance(Address::ZERO).with_test_block(block);
        assert_eq!(balance.identity_for_route(&route).block(), Some(block));
        assert_eq!(wire_request_for(&balance).1[1], expected);
    }
}

#[test]
fn endpoint_health_matrix_and_aggregate_precedence() {
    let inner = Err(RpcBrokerError::InnerRevert(RpcRevert::from_multicall(
        Bytes::from_static(b"revert"),
    )));
    let cases = [
        (Ok(Bytes::new()), EndpointHealthOutcome::Healthy),
        (inner, EndpointHealthOutcome::Healthy),
        (Err(RpcBrokerError::Timeout), EndpointHealthOutcome::Strike),
        (
            Err(RpcBrokerError::Transport),
            EndpointHealthOutcome::Strike,
        ),
        (
            Err(RpcBrokerError::InvalidResponse),
            EndpointHealthOutcome::Strike,
        ),
        (
            Err(RpcBrokerError::HttpStatus(301)),
            EndpointHealthOutcome::Strike,
        ),
        (
            Err(RpcBrokerError::HttpStatus(401)),
            EndpointHealthOutcome::Strike,
        ),
        (
            Err(RpcBrokerError::HttpStatus(500)),
            EndpointHealthOutcome::Strike,
        ),
        (Err(remote_error(-32005)), EndpointHealthOutcome::Strike),
        (Err(remote_error(-32601)), EndpointHealthOutcome::Strike),
        (Err(remote_error(-32603)), EndpointHealthOutcome::Strike),
        (Err(remote_error(-32000)), EndpointHealthOutcome::Neutral),
        (Err(remote_error(-32016)), EndpointHealthOutcome::Neutral),
        (Err(remote_error(-32700)), EndpointHealthOutcome::Neutral),
        (Err(remote_error(-32600)), EndpointHealthOutcome::Neutral),
        (Err(remote_error(-32602)), EndpointHealthOutcome::Neutral),
        (Err(remote_error(-32099)), EndpointHealthOutcome::Neutral),
        (
            Err(RpcBrokerError::TimeoutBeforeDispatch),
            EndpointHealthOutcome::Neutral,
        ),
        (
            Err(RpcBrokerError::Shutdown),
            EndpointHealthOutcome::Neutral,
        ),
        (
            Err(RpcBrokerError::NoEndpoint { chain_id: 1 }),
            EndpointHealthOutcome::Neutral,
        ),
    ];
    for (result, expected) in cases {
        assert_eq!(EndpointHealthOutcome::for_result(&result), expected);
    }
    assert_eq!(
        EndpointHealthOutcome::for_results(&[Ok(Bytes::new())]),
        EndpointHealthOutcome::Healthy
    );
    assert_eq!(
        EndpointHealthOutcome::for_results(&[Err(remote_error(-32000)), Ok(Bytes::new())]),
        EndpointHealthOutcome::Neutral
    );
    assert_eq!(
        EndpointHealthOutcome::for_results::<Bytes>(&[
            Err(remote_error(-32000)),
            Err(RpcBrokerError::HttpStatus(429)),
        ]),
        EndpointHealthOutcome::Strike
    );
}

#[tokio::test]
async fn struck_endpoints_are_withdrawn_but_request_blamed_failures_keep_receiving_traffic() {
    for (code, expected_first_calls) in [
        (-32603, HEALTH_STRIKE_THRESHOLD),
        (-32602, HEALTH_STRIKE_THRESHOLD + 1),
    ] {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let first_calls = Arc::new(AtomicUsize::new(0));
        let first_counter = first_calls.clone();
        let failing: RpcResponder = Arc::new(move |request| {
            first_counter.fetch_add(1, Ordering::SeqCst);
            rpc_error(&request, code)
        });
        let succeeding: RpcResponder = Arc::new(|request| rpc_result(&request, &json!("0x01")));
        let (first, first_server) = spawn_rpc_mock(failing, active.clone(), maximum.clone()).await;
        let (second, second_server) = spawn_rpc_mock(succeeding, active, maximum).await;
        let broker = test_broker(Duration::from_millis(1), 1);
        let route = RpcRoute::from(RpcChainRoute::new(1, vec![first, second]));
        for marker in 0..=u8::try_from(HEALTH_STRIKE_THRESHOLD).expect("strike threshold") {
            assert!(
                broker
                    .submit(RpcSubmission::new(
                        route.clone(),
                        vec![RpcRead::eth_call(Address::ZERO, Bytes::from(vec![marker]))],
                        test_origin(),
                    ))
                    .await
                    .expect("health submission")[0]
                    .is_ok()
            );
        }

        assert_eq!(
            first_calls.load(Ordering::SeqCst),
            expected_first_calls,
            "unexpected endpoint traffic for code {code}"
        );
        drop(broker);
        first_server.abort();
        second_server.abort();
    }
}

#[test]
fn total_failure_reports_only_whole_submission_transport_failures() {
    let request_failure = RpcBrokerError::Transport;
    assert_eq!(
        total_failure(&[
            Err::<Bytes, _>(request_failure.clone()),
            Err(RpcBrokerError::Timeout),
        ]),
        Some(&request_failure)
    );
    assert!(
        total_failure(&[
            Err::<Bytes, _>(RpcBrokerError::InnerRevert(RpcRevert::from_multicall(
                Bytes::from_static(b"revert")
            ))),
            Err(request_failure.clone()),
        ])
        .is_none()
    );
    assert!(total_failure(&[Ok(Bytes::from_static(b"ok")), Err(request_failure)]).is_none());
    assert!(total_failure::<Bytes>(&[]).is_none());
}

#[test]
fn gateway_web_origins_normalize_scope_and_reject_opaque_origins() {
    for (input, expected) in [
        (
            "HTTPS://EXAMPLE.COM:443/path?q=1#fragment",
            "https://example.com/",
        ),
        ("http://example.com:80/path", "http://example.com/"),
        (
            "https://例え.テスト/path",
            "https://xn--r8jz45g.xn--zckzah/",
        ),
        (
            "https://[2001:db8::1]:8443/path",
            "https://[2001:db8::1]:8443/",
        ),
        ("blob:https://example.com/document", "https://example.com/"),
    ] {
        assert_eq!(
            RpcOrigin::dapp_web_origin("peer", input).unwrap(),
            RpcOrigin::dapp("peer", expected).unwrap()
        );
    }
    for input in [
        "null",
        "data:text/plain,opaque",
        "file:///tmp/page",
        "ftp://example.com",
        "custom:page",
    ] {
        assert_eq!(
            RpcOrigin::dapp_web_origin("peer", input).unwrap_err(),
            RpcOriginError::InvalidOrigin
        );
    }
}
