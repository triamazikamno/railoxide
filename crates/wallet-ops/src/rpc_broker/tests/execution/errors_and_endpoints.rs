use super::*;

#[test]
fn remote_error_boundary_preserves_structured_data_and_revert_projection() {
    assert_eq!(
        RpcBrokerError::Timeout.failure_class(),
        FailureClass::Transient
    );
    assert_eq!(
        remote_error(-32005).failure_class(),
        FailureClass::Transient
    );
    assert_eq!(
        RpcBrokerError::HttpStatus(429).failure_class(),
        FailureClass::Transient
    );
    for status in [429, 500, 599] {
        let error = RpcBrokerError::HttpStatus(status);
        assert_eq!(error.failure_class(), FailureClass::Transient);
    }
    assert_eq!(
        remote_error(-32000).failure_class(),
        FailureClass::RecoverableByReduction
    );
    assert_eq!(
        remote_error(-32601).failure_class(),
        FailureClass::Unrecoverable
    );
    assert_eq!(
        RpcBrokerError::InvalidResponse.failure_class(),
        FailureClass::Unrecoverable
    );
    let outer = parse_rpc_result(
        &json!({"id": 1, "error": {"code": -32000, "data": "0xdeadbeef"}}),
        false,
    );
    assert!(matches!(outer, Err(RpcBrokerError::Remote(remote)) if remote.code() == -32000));

    let inner = parse_rpc_result(
        &json!({
            "id": 1,
            "error": {
                "code": 3,
                "message": "sentinel revert message",
                "data": "0xdeadbeef",
                "unknown": {"sentinel": "remote payload"}
            }
        }),
        true,
    );
    let Err(RpcBrokerError::InnerRevert(revert)) = inner else {
        panic!("code 3 with hex data should project to a revert");
    };
    assert_eq!(
        revert.expose_bytes(),
        &Bytes::from_static(b"\xde\xad\xbe\xef")
    );
    let source = revert.source().expect("individual revert source");
    assert_eq!(source.code(), 3);
    assert_eq!(source.expose_message(), "sentinel revert message");
    assert_eq!(source.expose_data(), Some(&json!("0xdeadbeef")));
    assert_eq!(
        source.expose_payload().other["unknown"]["sentinel"],
        "remote payload"
    );

    let marked = parse_rpc_result(
        &json!({
            "id": 1,
            "error": {"code": -32000, "message": "Execution ReVeRtEd", "data": "0xcafe"}
        }),
        true,
    );
    assert!(matches!(marked, Err(RpcBrokerError::InnerRevert(revert))
        if revert.expose_bytes() == &Bytes::from_static(b"\xca\xfe")
            && revert.source().is_some_and(|source| source.code() == -32000)));

    let ambiguous = parse_rpc_result(
        &json!({"id": 1, "error": {"code": -32000, "message": "capacity unavailable", "data": "0xcafe"}}),
        true,
    );
    assert!(matches!(ambiguous, Err(RpcBrokerError::Remote(remote))
        if remote.code() == -32000
            && ambiguous_error_is_recoverable(&remote)));

    for code in [-32005, -32601, -32603] {
        let error = parse_rpc_result(
            &json!({"id": 1, "error": {
                "code": code,
                "message": "sentinel revert failure",
                "data": "0xdeadbeef"
            }}),
            true,
        )
        .expect_err("protected remote errors remain remote");
        assert!(matches!(error, RpcBrokerError::Remote(ref remote) if remote.code() == code));
    }

    let Err(RpcBrokerError::Remote(structured)) = parse_rpc_result(
        &json!({
            "id": 1,
            "error": {
                "code": -32042,
                "message": "sentinel message",
                "data": ["sentinel data"],
                "unknown": {"sentinel": true}
            }
        }),
        true,
    ) else {
        panic!("arbitrary remote error data should remain structured");
    };
    let cloned = structured.clone();
    assert!(std::ptr::eq(
        structured.expose_payload(),
        cloned.expose_payload()
    ));
    assert_eq!(structured.code(), -32042);
    assert_eq!(structured.expose_message(), "sentinel message");
    assert_eq!(structured.expose_data(), Some(&json!(["sentinel data"])));
    assert_eq!(structured.to_string(), "-32042");
    let remote = RpcBrokerError::Remote(structured.clone());
    assert!(remote.to_string().contains("-32042"));
    let revert =
        RpcRevert::from_individual(Bytes::from_static(b"\xde\xad\xbe\xef"), structured.clone());
    let revert_error = RpcBrokerError::InnerRevert(revert.clone());
    for rendered in [
        format!("{structured:?}"),
        structured.to_string(),
        format!("{revert:?}"),
        revert.to_string(),
        format!("{remote:?}"),
        remote.to_string(),
        format!("{revert_error:?}"),
        revert_error.to_string(),
    ] {
        assert!(
            !rendered.contains("sentinel"),
            "sensitive payload leaked: {rendered}"
        );
        assert!(
            !rendered.contains("deadbeef"),
            "raw revert leaked: {rendered}"
        );
    }

    assert!(matches!(
        parse_rpc_result(
            &json!({"id": 1, "error": {"message": "missing code"}}),
            false,
        ),
        Err(RpcBrokerError::InvalidResponse)
    ));

    assert!(matches!(
        parse_rpc_result(&json!({"id": 2, "result": "0x01"}), false),
        Err(RpcBrokerError::InvalidResponse)
    ));
}

fn ambiguous_error_is_recoverable(remote: &RpcRemoteError) -> bool {
    RpcBrokerError::Remote(remote.clone()).failure_class() == FailureClass::RecoverableByReduction
}

#[tokio::test(start_paused = true)]
async fn started_request_timeout_is_recorded_as_an_attributed_request() {
    let read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"deadline"));
    let responder: RpcResponder = Arc::new(|request| rpc_result(&request, &json!("0x01")));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let (endpoint, server) = spawn_gated_rpc_mock(responder, active, maximum, gate.clone()).await;
    let route = RpcRoute::new(RpcChainRoute::new(1, vec![endpoint.clone()]));
    let item = WorkItem {
        key: WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce: 1,
            latest_epoch: None,
        },
        execution_route: route.clone(),
        read: read.clone(),
        origins: vec![test_origin()],
        waiters: WaiterState::new(WaiterPolicy {
            deadline: Some(Instant::now() + Duration::from_secs(5)),
            attempt_timeout: route.attempt_timeout,
        }),
    };
    let endpoints = item.execution_route.endpoints().to_vec();
    let job = tokio::spawn(run_job(
        test_resources(),
        ExecutionJob::Individual(Box::new(item)),
        endpoints,
        JobControl::granted(),
    ));
    gate.request_started.notified().await;
    time::advance(Duration::from_secs(6)).await;
    let output = job.await.expect("started request job");
    assert_eq!(output.completions[0].1, Err(RpcBrokerError::Timeout));
    assert_eq!(output.requests.len(), 1);
    gate.release_response.notify_one();
    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(2)).await;
    tokio::task::yield_now().await;
    server.abort();
    let _ = server.await;
}

#[tokio::test(start_paused = true)]
async fn aggregate_semaphore_wait_filters_expired_members_and_preserves_order() {
    let multicall = Address::from([15_u8; 20]);
    let observed_calls = Arc::new(Mutex::new(Vec::new()));
    let observed_calls_for_responder = observed_calls.clone();
    let responder: RpcResponder = Arc::new(move |request| {
        let data = hex::decode(
            request["params"][0]["input"]
                .as_str()
                .expect("aggregate calldata"),
        )
        .expect("aggregate calldata hex");
        let decoded =
            IMulticall3::tryAggregateCall::abi_decode(&data).expect("aggregate calldata ABI");
        observed_calls_for_responder
            .lock()
            .expect("calls lock")
            .push(decoded.calls.len());
        aggregate_response(&request, vec![(true, Bytes::from_static(b"later"))])
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = spawn_rpc_mock(responder, active, maximum).await;
    let route = RpcRoute::new(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall));
    let read = |index| {
        RpcRead::eth_call(
            Address::from([index; 20]),
            Bytes::from_static(b"\x70\xa0\x82\x31"),
        )
    };
    let early = Instant::now() + Duration::from_secs(1);
    let late = Instant::now() + Duration::from_secs(10);
    let items = vec![
        WorkItem {
            key: WorkKey {
                identity: read(1).identity_for_route(&route),
                route: route.chain_route().clone(),
                nonce: 1,
                latest_epoch: None,
            },
            execution_route: route.clone(),
            read: read(1),
            origins: vec![test_origin()],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: Some(early),
                attempt_timeout: route.attempt_timeout,
            }),
        },
        WorkItem {
            key: WorkKey {
                identity: read(2).identity_for_route(&route),
                route: route.chain_route().clone(),
                nonce: 2,
                latest_epoch: None,
            },
            execution_route: route.clone(),
            read: read(2),
            origins: vec![test_origin()],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: Some(late),
                attempt_timeout: route.attempt_timeout,
            }),
        },
    ];
    let semaphore = Arc::new(Semaphore::new(0));
    let request_items = items;
    let request_route = request_items[0].execution_route.clone();
    let request_semaphore = semaphore.clone();
    let request = tokio::spawn(async move {
        execute_aggregate_with_failover(
            reqwest::Client::new(),
            request_semaphore,
            request_route,
            &request_items,
        )
        .await
    });
    tokio::task::yield_now().await;
    time::advance(Duration::from_secs(2)).await;
    time::resume();
    semaphore.add_permits(1);
    let output = time::timeout(Duration::from_secs(5), request)
        .await
        .expect("aggregate completion")
        .expect("aggregate execution");
    assert_eq!(observed_calls.lock().expect("calls lock").as_slice(), [1]);
    assert_eq!(output.attempts[0].indices, vec![1]);
    assert_eq!(
        output.values,
        vec![
            Err(RpcBrokerError::TimeoutBeforeDispatch),
            Ok(data_result(Bytes::from_static(b"later"))),
        ]
    );
    server.abort();
}

#[tokio::test]
async fn aggregate_failover_filters_expired_member_but_keeps_first_outcome() {
    let multicall = Address::from([16_u8; 20]);
    let first_calls = Arc::new(Mutex::new(Vec::new()));
    let first_calls_for_responder = first_calls.clone();
    let first_responder: RpcResponder = Arc::new(move |request| {
        let data = hex::decode(request["params"][0]["input"].as_str().unwrap())
            .expect("aggregate calldata hex");
        let decoded =
            IMulticall3::tryAggregateCall::abi_decode(&data).expect("aggregate calldata ABI");
        first_calls_for_responder
            .lock()
            .expect("first calls lock")
            .push(decoded.calls.len());
        rpc_error(&request, -32005)
    });
    let second_calls = Arc::new(Mutex::new(Vec::new()));
    let second_calls_for_responder = second_calls.clone();
    let second_responder: RpcResponder = Arc::new(move |request| {
        let data = hex::decode(request["params"][0]["input"].as_str().unwrap())
            .expect("aggregate calldata hex");
        let decoded =
            IMulticall3::tryAggregateCall::abi_decode(&data).expect("aggregate calldata ABI");
        second_calls_for_responder
            .lock()
            .expect("second calls lock")
            .push(decoded.calls.len());
        aggregate_response(&request, vec![(true, Bytes::from_static(b"late"))])
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let first_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let second_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let (first, first_server) = spawn_gated_rpc_mock(
        first_responder,
        active.clone(),
        maximum.clone(),
        first_gate.clone(),
    )
    .await;
    let (second, second_server) =
        spawn_gated_rpc_mock(second_responder, active, maximum, second_gate.clone()).await;
    let route =
        RpcRoute::new(RpcChainRoute::new(1, vec![first.clone(), second]).with_multicall(multicall));
    let read = |index| {
        RpcRead::eth_call(
            Address::from([index; 20]),
            Bytes::from_static(b"\x70\xa0\x82\x31"),
        )
    };
    // `early` leaves real time for the first attempt to dispatch under load.
    let early = Instant::now() + Duration::from_secs(20);
    let late = Instant::now() + Duration::from_secs(100);
    let items = vec![
        WorkItem {
            key: WorkKey {
                identity: read(1).identity_for_route(&route),
                route: route.chain_route().clone(),
                nonce: 1,
                latest_epoch: None,
            },
            execution_route: route.clone(),
            read: read(1),
            origins: vec![WalletRpcOrigin::PublicWallet.into()],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: Some(early),
                attempt_timeout: route.attempt_timeout,
            }),
        },
        WorkItem {
            key: WorkKey {
                identity: read(2).identity_for_route(&route),
                route: route.chain_route().clone(),
                nonce: 2,
                latest_epoch: None,
            },
            execution_route: route.clone(),
            read: read(2),
            origins: vec![WalletRpcOrigin::PublicWallet.into()],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: Some(late),
                attempt_timeout: route.attempt_timeout,
            }),
        },
    ];
    let late_waiter = items[1].waiters.clone();
    let semaphore = Arc::new(Semaphore::new(1));
    let request_semaphore = semaphore.clone();
    let request_items = items;
    let request_route = request_items[0].execution_route.clone();
    let request = tokio::spawn(async move {
        execute_aggregate_with_failover(
            reqwest::Client::new(),
            request_semaphore,
            request_route,
            &request_items,
        )
        .await
    });
    time::timeout(
        Duration::from_secs(30),
        first_gate.request_started.notified(),
    )
    .await
    .expect("request started");

    late_waiter.add(WaiterPolicy {
        deadline: Some(Instant::now() + Duration::from_secs(100)),
        attempt_timeout: route.attempt_timeout,
    });
    late_waiter.add(WaiterPolicy {
        deadline: Some(Instant::now() + Duration::from_secs(20)),
        attempt_timeout: route.attempt_timeout,
    });

    let blocker_acquired = Arc::new(tokio::sync::Notify::new());
    let blocker_release = Arc::new(tokio::sync::Notify::new());
    let blocker_acquired_for_task = blocker_acquired.clone();
    let blocker_release_for_task = blocker_release.clone();
    let blocker = tokio::spawn(async move {
        let _permit = semaphore.acquire_owned().await.expect("blocker permit");
        blocker_acquired_for_task.notify_one();
        blocker_release_for_task.notified().await;
    });
    // Queue the blocker before the first HTTP attempt releases its permit.
    tokio::task::yield_now().await;
    first_gate.release_response.notify_one();
    time::timeout(Duration::from_secs(30), blocker_acquired.notified())
        .await
        .expect("blocker acquired permit");
    time::pause();
    time::advance(Duration::from_secs(40)).await;
    time::resume();
    blocker_release.notify_one();
    time::timeout(
        Duration::from_secs(30),
        second_gate.request_started.notified(),
    )
    .await
    .expect("failover request started");
    second_gate.release_response.notify_one();

    let output = time::timeout(Duration::from_secs(30), request)
        .await
        .expect("aggregate failover")
        .expect("aggregate failover task");
    assert_eq!(
        first_calls.lock().expect("first calls lock").as_slice(),
        [2]
    );
    assert_eq!(
        second_calls.lock().expect("second calls lock").as_slice(),
        [1]
    );
    assert_eq!(output.attempts.len(), 2);
    assert_eq!(output.attempts[0].indices, vec![0, 1]);
    assert_eq!(output.attempts[1].indices, vec![1]);
    assert_eq!(
        output.values,
        vec![
            Err(remote_error(-32005)),
            Ok(data_result(Bytes::from_static(b"late"))),
        ]
    );
    blocker_release.notify_one();
    blocker.await.expect("blocker task");
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn reduction_drops_expired_child_but_preserves_parent_error() {
    let multicall = Address::from([17_u8; 20]);
    let observed_calls = Arc::new(Mutex::new(Vec::new()));
    let observed_calls_for_responder = observed_calls.clone();
    let responder: RpcResponder = Arc::new(move |request| {
        let data = hex::decode(request["params"][0]["input"].as_str().unwrap())
            .expect("aggregate calldata hex");
        let decoded =
            IMulticall3::tryAggregateCall::abi_decode(&data).expect("aggregate calldata ABI");
        observed_calls_for_responder
            .lock()
            .expect("calls lock")
            .push(decoded.calls.len());
        if decoded.calls.len() == 2 {
            rpc_error(&request, -32000)
        } else {
            aggregate_response(&request, vec![(true, Bytes::from_static(b"late"))])
        }
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let first_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let (endpoint, server) =
        spawn_gated_rpc_mock(responder, active, maximum, first_gate.clone()).await;
    let route = RpcRoute::new(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall));
    let read = |index| {
        RpcRead::eth_call(
            Address::from([index; 20]),
            Bytes::from_static(b"\x70\xa0\x82\x31"),
        )
    };
    // `early` leaves real time for the first attempt to dispatch under load.
    let early = Instant::now() + Duration::from_secs(20);
    let late = Instant::now() + Duration::from_secs(100);
    let items = vec![
        WorkItem {
            key: WorkKey {
                identity: read(1).identity_for_route(&route),
                route: route.chain_route().clone(),
                nonce: 1,
                latest_epoch: None,
            },
            execution_route: route.clone(),
            read: read(1),
            origins: vec![WalletRpcOrigin::PublicWallet.into()],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: Some(early),
                attempt_timeout: route.attempt_timeout,
            }),
        },
        WorkItem {
            key: WorkKey {
                identity: read(2).identity_for_route(&route),
                route: route.chain_route().clone(),
                nonce: 2,
                latest_epoch: None,
            },
            execution_route: route.clone(),
            read: read(2),
            origins: vec![WalletRpcOrigin::Staking.into()],
            waiters: WaiterState::new(WaiterPolicy {
                deadline: Some(late),
                attempt_timeout: route.attempt_timeout,
            }),
        },
    ];
    let semaphore = Arc::new(Semaphore::new(1));
    let request_semaphore = semaphore.clone();
    let request_items = items;
    let request_route = request_items[0].execution_route.clone();
    let request = tokio::spawn(async move {
        execute_aggregate_with_failover(
            reqwest::Client::new(),
            request_semaphore,
            request_route,
            &request_items,
        )
        .await
    });
    time::timeout(
        Duration::from_secs(30),
        first_gate.request_started.notified(),
    )
    .await
    .expect("request started");

    let blocker_acquired = Arc::new(tokio::sync::Notify::new());
    let blocker_release = Arc::new(tokio::sync::Notify::new());
    let blocker_acquired_for_task = blocker_acquired.clone();
    let blocker_release_for_task = blocker_release.clone();
    let blocker = tokio::spawn(async move {
        let _permit = semaphore.acquire_owned().await.expect("blocker permit");
        blocker_acquired_for_task.notify_one();
        blocker_release_for_task.notified().await;
    });
    // Queue the blocker before the first HTTP attempt releases its permit.
    tokio::task::yield_now().await;
    first_gate.release_response.notify_one();
    time::timeout(Duration::from_secs(30), blocker_acquired.notified())
        .await
        .expect("blocker acquired permit");
    time::pause();
    time::advance(Duration::from_secs(40)).await;
    time::resume();
    blocker_release.notify_one();
    time::timeout(
        Duration::from_secs(30),
        first_gate.request_started.notified(),
    )
    .await
    .expect("request started");
    first_gate.release_response.notify_one();

    let output = time::timeout(Duration::from_secs(30), request)
        .await
        .expect("reduction completion")
        .expect("reduction execution");
    assert_eq!(
        observed_calls.lock().expect("calls lock").as_slice(),
        [2, 1]
    );
    assert_eq!(output.attempts.len(), 2);
    assert_eq!(output.attempts[0].indices, vec![0, 1]);
    assert_eq!(output.attempts[1].indices, vec![1]);
    assert_eq!(
        output.values,
        vec![
            Err(remote_error(-32000)),
            Ok(data_result(Bytes::from_static(b"late"))),
        ]
    );
    blocker.await.expect("blocker task");
    server.abort();
}

#[tokio::test]
async fn ordinary_submission_emits_only_requested_rpc_method() {
    let methods = Arc::new(Mutex::new(Vec::<String>::new()));
    let methods_for_responder = methods.clone();
    let responder: RpcResponder = Arc::new(move |request| {
        let method = request["method"].as_str().expect("RPC method").to_owned();
        methods_for_responder
            .lock()
            .expect("method log lock")
            .push(method);
        rpc_result(&request, &json!("0x2a"))
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = spawn_rpc_mock(responder, active, maximum).await;
    let broker = test_broker(Duration::from_millis(1), 1);
    let result = broker
        .submit(RpcSubmission::new(
            RpcRoute::new(RpcChainRoute::new(1, vec![endpoint])),
            vec![RpcRead::get_balance(Address::ZERO)],
            test_origin(),
        ))
        .await
        .unwrap();
    assert_eq!(result, vec![Ok(RpcResult::new(json!("0x2a")))]);
    drop(broker);
    assert_eq!(
        methods.lock().expect("method log lock").clone(),
        vec!["eth_getBalance"]
    );
    server.abort();
}

#[test]
fn balance_quantity_validation_preserves_values_and_rejects_malformed_quantities() {
    let read = RpcRead::get_balance(Address::ZERO);
    for value in ["0x0", "0x2a", "0x002a", "42"] {
        assert_eq!(
            read.operation().validate_result(json!(value)),
            Ok(RpcResult::new(json!(value)))
        );
    }
    let oversize = format!("0x{}", "a".repeat(65));
    for value in ["0x0g", oversize.as_str()] {
        assert_eq!(
            read.operation().validate_result(json!(value)),
            Err(RpcBrokerError::InvalidResponse)
        );
    }
}

#[tokio::test]
async fn non_success_http_status_wins_over_valid_looking_json_body() {
    let semaphore = Arc::new(Semaphore::new(1));
    for status in [401, 403] {
        let (endpoint, server) = spawn_status_rpc_mock(status).await;
        let result = rpc_request(
            &reqwest::Client::new(),
            &semaphore,
            &endpoint,
            "eth_getBalance",
            json!([format!("{:#x}", Address::ZERO), BlockNumberOrTag::Latest]),
            false,
            None,
            DEFAULT_ATTEMPT_TIMEOUT,
        )
        .await;
        assert_eq!(result, Err(RpcBrokerError::HttpStatus(status)));
        server.abort();
    }
}

#[tokio::test]
async fn endpoint_failover_retries_unrecoverable_failure_for_aggregate_reads() {
    let multicall = Address::from([13_u8; 20]);
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let failing_calls = first_calls.clone();
    let succeeding_calls = second_calls.clone();
    let failing: RpcResponder = Arc::new(move |request| {
        failing_calls.fetch_add(1, Ordering::SeqCst);
        rpc_error(&request, -32600)
    });
    let succeeding_calls_for_responder = succeeding_calls.clone();
    let succeeding: RpcResponder = Arc::new(move |request| {
        succeeding_calls_for_responder.fetch_add(1, Ordering::SeqCst);
        aggregate_response(&request, vec![(true, Bytes::from_static(b"\x42"))])
    });
    let (first, first_server) = spawn_rpc_mock(failing, active.clone(), maximum.clone()).await;
    let (second, second_server) = spawn_rpc_mock(succeeding, active, maximum).await;
    let broker = test_broker(Duration::from_millis(1), 2);
    let result = broker
        .submit(RpcSubmission::new(
            RpcRoute::new(RpcChainRoute::new(1, vec![first, second]).with_multicall(multicall)),
            vec![RpcRead::eth_call(
                Address::from([1_u8; 20]),
                Bytes::from_static(b"\x70\xa0\x82\x31"),
            )],
            test_origin(),
        ))
        .await
        .unwrap();
    assert_eq!(result, vec![Ok(data_result(Bytes::from_static(b"\x42")))]);
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    drop(broker);
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn reverted_member_is_not_retried_on_the_next_endpoint() {
    let multicall = Address::from([14_u8; 20]);
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let reverting_calls = first_calls.clone();
    let unused_calls = second_calls.clone();
    let reverting: RpcResponder = Arc::new(move |request| {
        reverting_calls.fetch_add(1, Ordering::SeqCst);
        aggregate_response(
            &request,
            vec![(false, Bytes::from_static(b"\x08\xc3\x79\xa0"))],
        )
    });
    let unused: RpcResponder = Arc::new(move |request| {
        unused_calls.fetch_add(1, Ordering::SeqCst);
        rpc_result(&request, &json!("0x42"))
    });
    let (first, first_server) = spawn_rpc_mock(reverting, active.clone(), maximum.clone()).await;
    let (second, second_server) = spawn_rpc_mock(unused, active, maximum).await;
    let broker = test_broker(Duration::from_millis(1), 2);
    let result = broker
        .submit(RpcSubmission::new(
            RpcRoute::new(RpcChainRoute::new(1, vec![first, second]).with_multicall(multicall)),
            vec![RpcRead::eth_call(
                Address::from([1_u8; 20]),
                Bytes::from_static(b"\x70\xa0\x82\x31"),
            )],
            test_origin(),
        ))
        .await
        .unwrap();
    assert!(matches!(&result[0], Err(RpcBrokerError::InnerRevert(_))));
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    drop(broker);
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn verified_routes_reuse_identity_and_never_read_from_wrong_chain_endpoints() {
    let rejected_checks = Arc::new(AtomicUsize::new(0));
    let rejected = rejected_checks.clone();
    // A panic in the mock task would only close the connection, so count unexpected reads instead.
    let wrong_chain_reads = Arc::new(AtomicUsize::new(0));
    let unexpected_reads = wrong_chain_reads.clone();
    let (bad, bad_server) = spawn_rpc_mock(
        Arc::new(move |request| {
            if request["method"] == "eth_chainId" {
                rejected.fetch_add(1, Ordering::SeqCst);
                rpc_result(&request, &json!("0x2"))
            } else {
                unexpected_reads.fetch_add(1, Ordering::SeqCst);
                rpc_error(&request, -32603)
            }
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let identity_checks = Arc::new(AtomicUsize::new(0));
    let checks = identity_checks.clone();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = reads.clone();
    let (good, good_server) = spawn_rpc_mock(
        Arc::new(move |request| {
            if request["method"] == "eth_chainId" {
                checks.fetch_add(1, Ordering::SeqCst);
                rpc_result(&request, &json!("0x1"))
            } else {
                assert_eq!(request["method"], "eth_gasPrice");
                observed_reads.fetch_add(1, Ordering::SeqCst);
                rpc_result(&request, &json!("0x42"))
            }
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let route = RpcRoute::new(RpcChainRoute::new(1, vec![bad, good]).with_identity_verification());
    let broker = test_broker(Duration::from_millis(1), 1);
    for _ in 0..4 {
        let results = broker
            .submit(RpcSubmission::new(
                route.clone(),
                vec![RpcRead::from_method_params("eth_gasPrice", json!([]), 1).unwrap()],
                test_origin(),
            ))
            .await
            .unwrap();
        assert_eq!(results, vec![Ok(RpcResult::new(json!("0x42")))]);
    }
    assert_eq!(identity_checks.load(Ordering::SeqCst), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 4);
    // Later jobs read the verified endpoint without first re-checking the rejected one.
    assert!(rejected_checks.load(Ordering::SeqCst) >= 1);
    assert_eq!(wrong_chain_reads.load(Ordering::SeqCst), 0);
    bad_server.abort();
    good_server.abort();
}

#[tokio::test]
async fn a_direct_request_failure_gets_the_brokers_health_outcome() {
    use crate::rpc_broker::actor::EndpointHealthOutcome::{Healthy, Neutral, Strike};
    use alloy::transports::{RpcError, TransportErrorKind};
    for (error, expected) in [
        // A revert counts only with revert data.
        (
            json!({"code": 3, "message": "execution reverted", "data": "0xdeadbeef"}),
            Healthy,
        ),
        (
            json!({"code": -32000, "message": "execution reverted", "data": "0xdeadbeef"}),
            Healthy,
        ),
        (
            json!({"code": -32000, "message": "execution reverted"}),
            Neutral,
        ),
        // Endpoint-blamed codes strike even with revert data; other codes are neutral.
        (
            json!({"code": -32603, "message": "execution reverted", "data": "0xdeadbeef"}),
            Strike,
        ),
        (json!({"code": -32005, "message": "limit exceeded"}), Strike),
        (json!({"code": -32042, "message": "unknown"}), Neutral),
    ] {
        let wire = json!({"id": 1, "error": error.clone()});
        let brokered = EndpointHealthOutcome::for_result(&parse_rpc_result(&wire, true));
        let payload: ErrorPayload = serde_json::from_str(&error.to_string()).unwrap();
        let direct = EndpointHealthOutcome::for_direct_error(&RpcError::ErrorResp(payload));
        assert_eq!((brokered, direct), (expected, expected), "{error}");
    }

    // A caller's per-request timeout strikes; its own checks, such as the chain, don't.
    let elapsed = time::timeout(Duration::ZERO, std::future::pending::<()>())
        .await
        .unwrap_err();
    for (error, expected) in [
        (eyre::Report::from(elapsed), Strike),
        (
            TransportErrorKind::custom_str("connection refused").into(),
            Strike,
        ),
        (eyre::eyre!("chain mismatch"), Neutral),
    ] {
        assert_eq!(EndpointHealthOutcome::for_direct_failure(&error), expected);
    }
}
