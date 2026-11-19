use super::*;

#[tokio::test]
async fn reduction_parent_error_replaces_undispatched_children() {
    let multicall = Address::from([10_u8; 20]);
    let first_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let first_calls = Arc::new(AtomicUsize::new(0));
    let first_calls_for_responder = first_calls.clone();
    let first_responder: RpcResponder = Arc::new(move |request| {
        first_calls_for_responder.fetch_add(1, Ordering::SeqCst);
        rpc_error(&request, -32000)
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) =
        spawn_gated_rpc_mock(first_responder, active, maximum, first_gate.clone()).await;
    let route =
        RpcRoute::new(RpcChainRoute::new(1, vec![endpoint.clone()]).with_multicall(multicall));
    let reads = [
        RpcRead::eth_call(Address::from([1_u8; 20]), Bytes::from_static(b"first")),
        RpcRead::eth_call(Address::from([2_u8; 20]), Bytes::from_static(b"second")),
    ];
    // Leaves real time to dispatch under load; the test then skips past it.
    let deadline = Instant::now() + Duration::from_secs(30);
    let semaphore = Arc::new(Semaphore::new(1));
    let members: Arc<[AggregateMember]> = reads
        .iter()
        .map(|read| AggregateMember {
            read: read.clone(),
            waiters: WaiterState::new(WaiterPolicy {
                deadline: Some(deadline),
                attempt_timeout: route.attempt_timeout,
            }),
        })
        .collect::<Vec<_>>()
        .into();
    let request = tokio::spawn(reduce(
        Arc::new(ReductionContext {
            client: reqwest::Client::new(),
            semaphore: semaphore.clone(),
            route: route.clone(),
            endpoint: endpoint.clone(),
            members,
            reduction_budget: Arc::new(AtomicUsize::new(0)),
        }),
        vec![0, 1],
    ));
    time::timeout(
        Duration::from_secs(30),
        first_gate.request_started.notified(),
    )
    .await
    .expect("request started");
    let blocker_acquired = Arc::new(tokio::sync::Notify::new());
    let blocker_waiting = Arc::new(tokio::sync::Notify::new());
    let blocker_release = Arc::new(tokio::sync::Notify::new());
    let blocker_acquired_for_task = blocker_acquired.clone();
    let blocker_waiting_for_task = blocker_waiting.clone();
    let blocker_release_for_task = blocker_release.clone();
    let blocker = tokio::spawn(async move {
        blocker_waiting_for_task.notify_one();
        let _permit = semaphore.acquire_owned().await.expect("blocker permit");
        blocker_acquired_for_task.notify_one();
        blocker_release_for_task.notified().await;
    });
    blocker_waiting.notified().await;
    first_gate.release_response.notify_waiters();
    tokio::time::timeout(Duration::from_secs(30), blocker_acquired.notified())
        .await
        .expect("blocker acquired");
    tokio::task::yield_now().await;
    time::pause();
    time::advance(Duration::from_secs(31)).await;
    time::resume();
    let output = time::timeout(Duration::from_secs(30), request)
        .await
        .expect("reduction deadline")
        .expect("reduction task");
    assert_eq!(
        output
            .values
            .iter()
            .cloned()
            .map(AttemptOutcome::into_result)
            .collect::<Vec<_>>(),
        vec![Err(remote_error(-32000)), Err(remote_error(-32000)),]
    );
    assert_eq!(output.attempts.len(), 1);
    assert_eq!(output.attempts[0].endpoint, endpoint);
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    blocker_release.notify_one();
    blocker.await.expect("blocker task");
    server.abort();
}

#[tokio::test]
async fn singleton_reduction_retry_resnapshots_late_waiter() {
    let multicall = Address::from([18_u8; 20]);
    let first_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let responder: RpcResponder = Arc::new(move |request| {
        if request["params"][0]["to"] == json!(format!("{multicall:#x}")) {
            rpc_error(&request, -32000)
        } else {
            rpc_result(&request, &json!("0x02"))
        }
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) =
        spawn_gated_rpc_mock(responder, active, maximum, first_gate.clone()).await;
    let route =
        RpcRoute::new(RpcChainRoute::new(1, vec![endpoint.clone()]).with_multicall(multicall));
    let read = RpcRead::eth_call(Address::from([1_u8; 20]), Bytes::from_static(b"singleton"));
    let initial_deadline = Instant::now() + Duration::from_secs(30);
    let waiters = WaiterState::new(WaiterPolicy {
        deadline: Some(initial_deadline),
        attempt_timeout: route.attempt_timeout,
    });
    let late_waiter = waiters.clone();
    let semaphore = Arc::new(Semaphore::new(1));
    let mut request = tokio::spawn(reduce(
        Arc::new(ReductionContext {
            client: reqwest::Client::new(),
            semaphore: semaphore.clone(),
            route: route.clone(),
            endpoint,
            members: vec![AggregateMember { read, waiters }].into(),
            reduction_budget: Arc::new(AtomicUsize::new(0)),
        }),
        vec![0],
    ));
    tokio::select! {
        result = &mut request => match result {
            Ok(output) => panic!(
                "singleton reduction completed before first request started ({} attempts)",
                output.attempts.len(),
            ),
            Err(error) => panic!("singleton reduction task failed before first request: {error}"),
        },
        result = time::timeout(Duration::from_secs(5), first_gate.request_started.notified()) => {
            result.expect("first request started");
        }
    }

    let blocker_acquired = Arc::new(tokio::sync::Notify::new());
    let blocker_waiting = Arc::new(tokio::sync::Notify::new());
    let blocker_release = Arc::new(tokio::sync::Notify::new());
    let blocker_acquired_for_task = blocker_acquired.clone();
    let blocker_waiting_for_task = blocker_waiting.clone();
    let blocker_release_for_task = blocker_release.clone();
    let blocker = tokio::spawn(async move {
        blocker_waiting_for_task.notify_one();
        let _permit = semaphore.acquire_owned().await.expect("blocker permit");
        blocker_acquired_for_task.notify_one();
        blocker_release_for_task.notified().await;
    });
    time::timeout(Duration::from_secs(5), blocker_waiting.notified())
        .await
        .expect("blocker task started");
    tokio::task::yield_now().await;
    first_gate.release_response.notify_one();
    time::timeout(Duration::from_secs(5), blocker_acquired.notified())
        .await
        .expect("blocker acquired");
    late_waiter.add(WaiterPolicy {
        deadline: Some(Instant::now() + Duration::from_mins(1)),
        attempt_timeout: route.attempt_timeout,
    });

    time::pause();
    time::advance(Duration::from_secs(31)).await;
    time::resume();

    blocker_release.notify_one();
    tokio::select! {
        result = &mut request => match result {
            Ok(output) => panic!(
                "singleton reduction completed before retry request started ({} attempts)",
                output.attempts.len(),
            ),
            Err(error) => panic!("singleton reduction task failed before retry request: {error}"),
        },
        result = time::timeout(Duration::from_secs(5), first_gate.request_started.notified()) => {
            result.expect("retry request started");
        }
    }
    first_gate.release_response.notify_one();
    let output = time::timeout(Duration::from_secs(5), &mut request)
        .await
        .expect("singleton reduction completion")
        .expect("singleton reduction");
    assert_eq!(
        output.values,
        vec![AttemptOutcome::Success(data_result(Bytes::from_static(
            b"\x02"
        )))]
    );
    assert_eq!(output.attempts.len(), 2);
    assert_eq!(output.attempts[0].indices, vec![0]);
    assert_eq!(output.attempts[1].indices, vec![0]);
    time::timeout(Duration::from_secs(5), blocker)
        .await
        .expect("blocker task completion")
        .expect("blocker task");
    server.abort();
}

#[tokio::test]
async fn reduction_preserves_requested_cardinality() {
    let multicall = Address::from([12_u8; 20]);
    let responder: RpcResponder = Arc::new(move |request| {
        if request["method"] == "eth_call"
            && request["params"][0]["to"] == json!(format!("{multicall:#x}"))
        {
            rpc_error(&request, -32000)
        } else {
            let target = request["params"][0]["to"]
                .as_str()
                .expect("individual target");
            let marker = target.chars().last().expect("target marker");
            rpc_result(&request, &json!(format!("0x0{marker}")))
        }
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = spawn_rpc_mock(responder, active, maximum).await;
    let reads = (0_u8..3)
        .map(|index| {
            RpcRead::eth_call(
                Address::from([index + 1; 20]),
                Bytes::from_static(b"\x70\xa0\x82\x31"),
            )
        })
        .collect();
    let result = test_broker(Duration::from_millis(1), 2)
        .submit(RpcSubmission::new(
            RpcRoute::new(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall)),
            reads,
            test_origin(),
        ))
        .await
        .expect("aggregate reduction");
    assert_eq!(result.len(), 3);
    assert_eq!(
        result,
        vec![
            Ok(data_result(Bytes::from_static(b"\x01"))),
            Ok(data_result(Bytes::from_static(b"\x02"))),
            Ok(data_result(Bytes::from_static(b"\x03"))),
        ]
    );
    server.abort();
}

#[tokio::test]
async fn reduction_limit_fails_over_without_phantom_attempts() {
    let multicall = Address::from([13_u8; 20]);
    let first_responder: RpcResponder = Arc::new(|request| rpc_error(&request, -32000));
    let second_responder: RpcResponder = Arc::new(move |request| {
        let data = hex::decode(
            request["params"][0]["input"]
                .as_str()
                .expect("aggregate calldata"),
        )
        .expect("aggregate calldata hex");
        let decoded =
            IMulticall3::tryAggregateCall::abi_decode(&data).expect("aggregate calldata ABI");
        aggregate_response(
            &request,
            (0..decoded.calls.len())
                .map(|_| (true, Bytes::from_static(b"ok")))
                .collect(),
        )
    });
    let first_active = Arc::new(AtomicUsize::new(0));
    let first_maximum = Arc::new(AtomicUsize::new(0));
    let second_active = Arc::new(AtomicUsize::new(0));
    let second_maximum = Arc::new(AtomicUsize::new(0));
    let (first, first_server) = spawn_rpc_mock(first_responder, first_active, first_maximum).await;
    let (second, second_server) =
        spawn_rpc_mock(second_responder, second_active, second_maximum).await;
    let route = RpcRoute::new(
        RpcChainRoute::new(1, vec![first.clone(), second.clone()]).with_multicall(multicall),
    );
    let items = (0_u8..64)
        .map(|index| {
            let read = RpcRead::eth_call(
                Address::from([index; 20]),
                Bytes::from_static(b"\x70\xa0\x82\x31"),
            );
            WorkItem {
                key: WorkKey {
                    identity: read.identity_for_route(&route),
                    route: route.chain_route().clone(),
                    nonce: u64::from(index),
                    latest_epoch: None,
                },
                execution_route: route.clone(),
                read,
                origins: vec![test_origin()],
                waiters: WaiterState::new(WaiterPolicy {
                    deadline: None,
                    attempt_timeout: route.attempt_timeout,
                }),
            }
        })
        .collect::<Vec<_>>();
    let output = execute_aggregate_with_failover(
        reqwest::Client::new(),
        Arc::new(Semaphore::new(4)),
        route,
        &items,
    )
    .await;
    assert_eq!(output.values.len(), 64);
    assert!(output.values.iter().all(Result::is_ok));
    assert_eq!(
        output
            .attempts
            .iter()
            .filter(|attempt| attempt.endpoint == first)
            .count(),
        MAX_REDUCTION_ATTEMPTS_PER_ENDPOINT
    );
    assert!(
        output
            .attempts
            .iter()
            .any(|attempt| attempt.endpoint == second)
    );
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn singleton_leaf_fails_over_after_individual_retry() {
    let multicall = Address::from([14_u8; 20]);
    let first_responder: RpcResponder = Arc::new(|request| rpc_error(&request, -32000));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_calls_for_responder = second_calls.clone();
    let second_responder: RpcResponder = Arc::new(move |request| {
        second_calls_for_responder.fetch_add(1, Ordering::SeqCst);
        if request["params"][0]["to"] == json!(format!("{multicall:#x}")) {
            rpc_error(&request, -32000)
        } else {
            rpc_result(&request, &json!("0x42"))
        }
    });
    let first_active = Arc::new(AtomicUsize::new(0));
    let first_maximum = Arc::new(AtomicUsize::new(0));
    let second_active = Arc::new(AtomicUsize::new(0));
    let second_maximum = Arc::new(AtomicUsize::new(0));
    let (first, first_server) = spawn_rpc_mock(first_responder, first_active, first_maximum).await;
    let (second, second_server) =
        spawn_rpc_mock(second_responder, second_active, second_maximum).await;
    let route =
        RpcRoute::new(RpcChainRoute::new(1, vec![first.clone(), second]).with_multicall(multicall));
    let read = RpcRead::eth_call(
        Address::from([1_u8; 20]),
        Bytes::from_static(b"\x70\xa0\x82\x31"),
    );
    let item = WorkItem {
        key: WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce: 1,
            latest_epoch: None,
        },
        execution_route: route.clone(),
        read,
        origins: vec![test_origin()],
        waiters: WaiterState::new(WaiterPolicy {
            deadline: None,
            attempt_timeout: route.attempt_timeout,
        }),
    };
    let output = execute_aggregate_with_failover(
        reqwest::Client::new(),
        Arc::new(Semaphore::new(2)),
        route,
        &[item],
    )
    .await;
    assert_eq!(
        output.values,
        vec![Ok(data_result(Bytes::from_static(b"\x42")))]
    );
    assert_eq!(
        output
            .attempts
            .iter()
            .filter(|attempt| attempt.endpoint == first)
            .count(),
        2
    );
    // The second endpoint repeats the aggregate probe before its own singleton retry.
    assert_eq!(second_calls.load(Ordering::SeqCst), 2);
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn individual_read_fails_over_on_recoverable_error() {
    let first_responder: RpcResponder = Arc::new(|request| rpc_error(&request, -32000));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_calls_for_responder = second_calls.clone();
    let second_responder: RpcResponder = Arc::new(move |request| {
        second_calls_for_responder.fetch_add(1, Ordering::SeqCst);
        rpc_result(&request, &json!("0x42"))
    });
    let first_active = Arc::new(AtomicUsize::new(0));
    let first_maximum = Arc::new(AtomicUsize::new(0));
    let second_active = Arc::new(AtomicUsize::new(0));
    let second_maximum = Arc::new(AtomicUsize::new(0));
    let (first, first_server) = spawn_rpc_mock(first_responder, first_active, first_maximum).await;
    let (second, second_server) =
        spawn_rpc_mock(second_responder, second_active, second_maximum).await;
    let route = RpcRoute::new(RpcChainRoute::new(1, vec![first.clone(), second]));
    let read = RpcRead::eth_call(
        Address::from([1_u8; 20]),
        Bytes::from_static(b"\x70\xa0\x82\x31"),
    );
    let item = WorkItem {
        key: WorkKey {
            identity: read.identity_for_route(&route),
            route: route.chain_route().clone(),
            nonce: 1,
            latest_epoch: None,
        },
        execution_route: route.clone(),
        read,
        origins: vec![test_origin()],
        waiters: WaiterState::new(WaiterPolicy {
            deadline: Some(Instant::now() + Duration::from_secs(5)),
            attempt_timeout: route.attempt_timeout,
        }),
    };
    let endpoints = item.execution_route.endpoints().to_vec();
    let output = run_job(
        test_resources(),
        ExecutionJob::Individual(Box::new(item)),
        endpoints,
        JobControl::granted(),
    )
    .await;
    assert_eq!(
        output.completions[0].1,
        Ok(data_result(Bytes::from_static(b"\x42")))
    );
    assert_eq!(output.requests.len(), 2);
    assert_eq!(output.requests[0].endpoint, first);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn production_path_fans_out_unrecoverable_outer_failure() {
    let multicall = Address::from([12_u8; 20]);
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let responder: RpcResponder = Arc::new(move |request| rpc_error(&request, -32099));
    let (endpoint, server) = spawn_rpc_mock(responder, active, maximum).await;
    let broker = test_broker(Duration::from_millis(1), 2);
    let reads = (0_u8..3)
        .map(|index| {
            RpcRead::eth_call(
                Address::from([index + 1; 20]),
                Bytes::from_static(b"\x70\xa0\x82\x31"),
            )
        })
        .collect();
    let result = time::timeout(
        Duration::from_secs(1),
        broker.submit(RpcSubmission::new(
            RpcRoute::new(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall)),
            reads,
            test_origin(),
        )),
    )
    .await
    .expect("unrecoverable request")
    .unwrap();
    assert_eq!(result.len(), 3);
    for value in result {
        assert!(matches!(
            value,
            Err(RpcBrokerError::Remote(remote)) if remote.code() == -32099
        ));
    }
    drop(broker);
    server.abort();
}
