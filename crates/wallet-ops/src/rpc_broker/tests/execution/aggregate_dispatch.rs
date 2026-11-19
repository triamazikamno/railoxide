use super::*;

#[tokio::test]
async fn malformed_aggregate_response_strikes_without_reduction_and_can_fail_over() {
    for malformed in ["hex", "abi", "count", "balance"] {
        let first_hits = Arc::new(AtomicUsize::new(0));
        let hits = first_hits.clone();
        let first_responder: RpcResponder = Arc::new(move |request| {
            hits.fetch_add(1, Ordering::SeqCst);
            match malformed {
                "hex" => rpc_result(&request, &json!("0xzz")),
                "abi" => rpc_result(&request, &json!("0x00")),
                "balance" => aggregate_response(
                    &request,
                    vec![
                        (true, Bytes::from_static(b"short")),
                        (true, Bytes::from_static(b"second")),
                    ],
                ),
                _ => aggregate_response(&request, Vec::new()),
            }
        });
        let second_responder: RpcResponder = Arc::new(move |request| {
            aggregate_response(
                &request,
                vec![
                    (
                        true,
                        if malformed == "balance" {
                            Bytes::copy_from_slice(&U256::from(42).to_be_bytes::<32>())
                        } else {
                            Bytes::from_static(b"first")
                        },
                    ),
                    (true, Bytes::from_static(b"second")),
                ],
            )
        });
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (first, first_server) =
            spawn_rpc_mock(first_responder, active.clone(), maximum.clone()).await;
        let (second, second_server) = spawn_rpc_mock(second_responder, active, maximum).await;
        for failover in [false, true] {
            let endpoints = if failover {
                vec![first.clone(), second.clone()]
            } else {
                vec![first.clone()]
            };
            let route = RpcRoute::new(
                RpcChainRoute::new(1, endpoints).with_multicall(Address::from([10_u8; 20])),
            );
            let items = (1..=2)
                .map(|index| {
                    let read = if malformed == "balance" && index == 1 {
                        RpcRead::get_balance(Address::ZERO)
                    } else {
                        RpcRead::eth_call(Address::from([index; 20]), Bytes::new())
                    };
                    WorkItem {
                        key: WorkKey {
                            identity: read.identity_for_route(&route),
                            route: route.chain_route().clone(),
                            nonce: 0,
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
                Arc::new(Semaphore::new(1)),
                route,
                &items,
            )
            .await;
            assert_eq!(output.attempts.len(), if failover { 2 } else { 1 });
            assert_eq!(output.attempts[0].endpoint, first);
            assert_eq!(
                output.attempts[0].health_outcome,
                EndpointHealthOutcome::Strike
            );
            assert_eq!(output.attempts[0].indices, vec![0, 1]);
            if failover {
                assert_eq!(output.attempts[1].endpoint, second);
                assert_eq!(output.attempts[1].indices, vec![0, 1]);
                assert_eq!(
                    output.values,
                    vec![
                        Ok(if malformed == "balance" {
                            RpcResult::new(json!("0x2a"))
                        } else {
                            data_result(Bytes::from_static(b"first"))
                        }),
                        Ok(data_result(Bytes::from_static(b"second")))
                    ]
                );
            } else {
                assert_eq!(output.values, vec![Err(RpcBrokerError::InvalidResponse); 2]);
            }
        }
        assert_eq!(first_hits.load(Ordering::SeqCst), 2);
        first_server.abort();
        second_server.abort();
    }
}

#[tokio::test]
async fn empty_aggregate_job_completes_without_waiting_for_a_permit() {
    let output = time::timeout(
        Duration::from_secs(1),
        run_job(
            JobResources {
                http: Arc::new(Semaphore::new(0)),
                ..test_resources()
            },
            ExecutionJob::Aggregate(Vec::new()),
            Vec::new(),
            JobControl::granted(),
        ),
    )
    .await
    .expect("empty aggregate should complete without acquiring a permit");

    assert!(output.completions.is_empty());
    assert!(output.requests.is_empty());
}

#[tokio::test]
async fn typed_submit_helper_decodes_members_and_preserves_errors() {
    let executor: JobExecutor = Arc::new(|_resources, group, _endpoints, _control| {
        Box::pin(async move {
            let completions = group
                .into_iter()
                .map(|item| {
                    let marker = read_calldata(&item.read).first().copied();
                    let result = match marker {
                        Some(1) => Ok(data_result(Bytes::from(<alloy::sol_types::sol_data::Uint<
                            256,
                        > as SolType>::abi_encode(
                            &U256::from(7)
                        )))),
                        Some(2) => Ok(data_result(Bytes::from_static(&[1, 2, 3]))),
                        Some(4) => Ok(RpcResult::new(json!(
                            U256::from(7).to_be_bytes::<32>().to_vec()
                        ))),
                        _ => Err(RpcBrokerError::Transport),
                    };
                    (item.key, result)
                })
                .collect();
            JobOutput {
                completions,
                requests: Vec::new(),
            }
        })
    });
    let broker = test_broker_with_executor(Duration::from_millis(1), 1, executor);
    let route = test_route();
    let results = broker
        .submit_calls_decoded_as::<TypedHelperTest::readCall>(
            route,
            vec![
                (Address::ZERO, Bytes::from_static(&[1])),
                (Address::ZERO, Bytes::from_static(&[2])),
                (Address::ZERO, Bytes::from_static(&[3])),
                (Address::ZERO, Bytes::from_static(&[4])),
            ],
            test_origin(),
        )
        .await
        .expect("outer submission should succeed");
    assert_eq!(results[0], Ok(U256::from(7)));
    assert_eq!(results[1], Err(RpcBrokerError::InvalidResponse));
    assert_eq!(results[2], Err(RpcBrokerError::Transport));
    // A JSON byte array decodes like hex, as operation validation accepts it.
    assert_eq!(results[3], Ok(U256::from(7)));
    drop(broker);
}

#[tokio::test]
async fn production_path_reassembles_mixed_and_individual_reads() {
    let multicall = Address::from([9_u8; 20]);
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let responder: RpcResponder = Arc::new(move |request| {
        let aggregate = request["method"] == "eth_call"
            && request["params"][0]["to"] == json!(format!("{multicall:#x}"));
        if aggregate {
            let data = hex::decode(
                request["params"][0]["input"]
                    .as_str()
                    .expect("aggregate calldata"),
            )
            .expect("aggregate calldata hex");
            let decoded =
                IMulticall3::tryAggregateCall::abi_decode(&data).expect("aggregate calldata ABI");
            assert_eq!(decoded.calls.len(), 1);
            assert_eq!(decoded.calls[0].target, Address::from([1_u8; 20]));
            assert_eq!(
                decoded.calls[0].callData,
                Bytes::from_static(b"\x70\xa0\x82\x31")
            );
            aggregate_response(&request, vec![(true, Bytes::from_static(&[0x11]))])
        } else {
            rpc_result(&request, &json!("0x22"))
        }
    });
    let (endpoint, server) = spawn_rpc_mock(responder, active, maximum).await;
    let broker = test_broker(Duration::from_millis(1), 2);
    let ineligible = rpc_read_from_request(
        TransactionRequest {
            to: Some(Address::from([2_u8; 20]).into()),
            value: Some(U256::from(1)),
            input: TransactionInput::new(Bytes::from_static(b"\x12\x34\x56\x78")),
            ..TransactionRequest::default()
        },
        BlockId::latest(),
        None,
    );
    let mixed = RpcSubmission::new(
        RpcRoute::new(RpcChainRoute::new(1, vec![endpoint.clone()]).with_multicall(multicall)),
        vec![
            rpc_read_from_request(
                TransactionRequest {
                    to: Some(Address::from([1_u8; 20]).into()),
                    input: TransactionInput::new(Bytes::from_static(b"\x70\xa0\x82\x31")),
                    ..TransactionRequest::default()
                },
                BlockId::latest(),
                None,
            ),
            ineligible,
        ],
        test_origin(),
    );
    let mixed_result = time::timeout(Duration::from_secs(1), broker.submit(mixed))
        .await
        .expect("mixed production request")
        .unwrap();
    assert_eq!(
        mixed_result,
        vec![
            Ok(data_result(Bytes::from_static(&[0x11]))),
            Ok(data_result(Bytes::from_static(&[0x22])))
        ]
    );

    let individual = RpcSubmission::new(
        RpcRoute::new(RpcChainRoute::new(1, vec![endpoint])),
        vec![RpcRead::eth_call(
            Address::from([3_u8; 20]),
            Bytes::from_static(b"\x12\x34\x56\x78"),
        )],
        test_origin(),
    );
    let individual_result = broker.submit(individual).await.unwrap();
    assert_eq!(
        individual_result,
        vec![Ok(data_result(Bytes::from_static(&[0x22])))]
    );
    drop(broker);
    server.abort();
}

#[tokio::test]
async fn production_path_isolates_inner_revert_payloads() {
    let multicall = Address::from([10_u8; 20]);
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let responder: RpcResponder = Arc::new(move |request| {
        aggregate_response(
            &request,
            vec![
                (true, Bytes::from_static(&[0xaa])),
                (false, Bytes::from_static(b"\xde\xad\xbe\xef")),
            ],
        )
    });
    let (endpoint, server) = spawn_rpc_mock(responder, active, maximum).await;
    let broker = test_broker(Duration::from_millis(1), 2);
    let reads = (0_u8..2)
        .map(|index| {
            RpcRead::eth_call(
                Address::from([index + 1; 20]),
                Bytes::from_static(b"\x70\xa0\x82\x31"),
            )
        })
        .collect();
    let result = broker
        .submit(RpcSubmission::new(
            RpcRoute::new(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall)),
            reads,
            test_origin(),
        ))
        .await
        .unwrap();
    assert_eq!(result[0], Ok(data_result(Bytes::from_static(&[0xaa]))));
    assert!(matches!(
        &result[1],
        Err(RpcBrokerError::InnerRevert(revert))
            if revert.expose_bytes() == &Bytes::from_static(b"\xde\xad\xbe\xef")
                && revert.source().is_none()
    ));
    drop(broker);
    server.abort();
}

#[tokio::test]
async fn production_path_reduces_recoverable_aggregates_concurrently() {
    let multicall = Address::from([11_u8; 20]);
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let sizes_for_responder = sizes.clone();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let responder: RpcResponder = Arc::new(move |request| {
        let aggregate = request["method"] == "eth_call"
            && request["params"][0]["to"] == json!(format!("{multicall:#x}"));
        if aggregate {
            let data = hex::decode(
                request["params"][0]["input"]
                    .as_str()
                    .expect("aggregate calldata"),
            )
            .expect("aggregate calldata hex");
            let decoded =
                IMulticall3::tryAggregateCall::abi_decode(&data).expect("aggregate calldata ABI");
            sizes_for_responder
                .lock()
                .expect("sizes lock")
                .push(decoded.calls.len());
            rpc_error(&request, -32000)
        } else {
            rpc_result(&request, &json!("0x42"))
        }
    });
    let (endpoint, server) = spawn_rpc_mock(responder, active.clone(), maximum.clone()).await;
    let broker = test_broker(Duration::from_millis(1), 4);
    let reads = (0_u8..4)
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
    .expect("reduction request")
    .unwrap();
    assert_eq!(
        result,
        vec![Ok(data_result(Bytes::from_static(&[0x42]))); 4]
    );
    let mut observed = sizes.lock().expect("sizes lock").clone();
    observed.sort_unstable();
    assert_eq!(observed, vec![1, 1, 1, 1, 2, 2, 4]);
    assert!(maximum.load(Ordering::SeqCst) >= 2);
    drop(broker);
    server.abort();
}

#[tokio::test]
async fn aggregate_attempt_timeout_fails_over_before_total_deadline() {
    let multicall = Address::from([10_u8; 20]);
    let first_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let first_responder: RpcResponder = Arc::new(|request| {
        aggregate_response(&request, vec![(true, Bytes::from_static(b"first"))])
    });
    let second_responder: RpcResponder = Arc::new(|request| {
        aggregate_response(&request, vec![(true, Bytes::from_static(b"second"))])
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (first, first_server) = spawn_gated_rpc_mock(
        first_responder,
        active.clone(),
        maximum.clone(),
        first_gate.clone(),
    )
    .await;
    let (second, second_server) = spawn_rpc_mock(second_responder, active, maximum).await;
    let route = RpcRoute::new(
        RpcChainRoute::new(1, vec![first.clone(), second.clone()]).with_multicall(multicall),
    )
    .with_request_timeout(Duration::from_secs(2))
    .with_attempt_timeout(Duration::from_millis(100));
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
            deadline: Some(Instant::now() + Duration::from_secs(2)),
            attempt_timeout: route.attempt_timeout,
        }),
    };
    let items = vec![item];
    let output = tokio::spawn(async move {
        execute_aggregate_with_failover(
            reqwest::Client::new(),
            Arc::new(Semaphore::new(2)),
            route,
            &items,
        )
        .await
    });
    time::timeout(
        Duration::from_secs(3),
        first_gate.request_started.notified(),
    )
    .await
    .expect("first aggregate request should start");
    let output = time::timeout(Duration::from_secs(3), output)
        .await
        .expect("attempt failover")
        .expect("aggregate task");
    assert_eq!(
        output.values,
        vec![Ok(data_result(Bytes::from_static(b"second")))]
    );
    assert!(
        output
            .attempts
            .iter()
            .any(|attempt| attempt.endpoint == first)
    );
    assert!(
        output
            .attempts
            .iter()
            .any(|attempt| attempt.endpoint == second)
    );
    first_gate.release_response.notify_waiters();
    first_server.abort();
    second_server.abort();
}

#[tokio::test(start_paused = true)]
async fn aggregate_uses_the_strictest_cap_selected_at_dispatch() {
    let multicall = Address::from([11_u8; 20]);
    let gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let responder: RpcResponder = Arc::new(|request| {
        aggregate_response(
            &request,
            vec![
                (true, Bytes::from_static(b"first")),
                (true, Bytes::from_static(b"second")),
            ],
        )
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = spawn_gated_rpc_mock(responder, active, maximum, gate.clone()).await;
    let route =
        RpcRoute::new(RpcChainRoute::new(1, vec![endpoint.clone()]).with_multicall(multicall));
    let read = RpcRead::eth_call(
        Address::from([1_u8; 20]),
        Bytes::from_static(b"\x70\xa0\x82\x31"),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let waiter = WaiterState::new(WaiterPolicy {
        deadline: Some(deadline),
        attempt_timeout: Duration::from_secs(5),
    });
    let client = reqwest::Client::new();
    let semaphore = Arc::new(Semaphore::new(0));
    let dispatch_semaphore = semaphore.clone();
    let indices = vec![0_usize, 1];
    let members: Arc<[AggregateMember]> = vec![
        AggregateMember {
            read,
            waiters: WaiterState::new(WaiterPolicy {
                deadline: Some(deadline),
                attempt_timeout: Duration::from_secs(5),
            }),
        },
        AggregateMember {
            read: RpcRead::eth_call(
                Address::from([2_u8; 20]),
                Bytes::from_static(b"\x70\xa0\x82\x31"),
            ),
            waiters: waiter.clone(),
        },
    ]
    .into();
    let context = ReductionContext {
        client,
        semaphore: dispatch_semaphore,
        route,
        endpoint,
        members,
        reduction_budget: Arc::new(AtomicUsize::new(0)),
    };
    let request = tokio::spawn(async move { execute_aggregate(&context, &indices, None).await });
    tokio::task::yield_now().await;
    waiter.add(WaiterPolicy {
        deadline: Some(deadline),
        attempt_timeout: Duration::from_millis(100),
    });
    semaphore.add_permits(1);
    gate.request_started.notified().await;
    time::advance(Duration::from_millis(200)).await;
    let output = time::timeout(Duration::from_millis(100), request)
        .await
        .expect("dispatch honors shortest live member cap")
        .expect("aggregate dispatch task");
    assert_eq!(output.result.into_result(), Err(RpcBrokerError::Timeout));
    gate.release_response.notify_waiters();
    server.abort();
}

#[tokio::test]
async fn late_waiter_extends_only_future_shared_failover_attempts() {
    let first_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let second_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let first_responder: RpcResponder = Arc::new(|request| rpc_error(&request, -32005));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_calls_for_responder = second_calls.clone();
    let second_responder: RpcResponder = Arc::new(move |request| {
        second_calls_for_responder.fetch_add(1, Ordering::SeqCst);
        rpc_result(&request, &json!("0x02"))
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (first, first_server) = spawn_gated_rpc_mock(
        first_responder,
        active.clone(),
        maximum.clone(),
        first_gate.clone(),
    )
    .await;
    let (second, second_server) =
        spawn_gated_rpc_mock(second_responder, active, maximum, second_gate.clone()).await;
    let endpoints = [
        SensitiveUrl::from(first.clone()),
        SensitiveUrl::from(second.clone()),
    ];
    let first_deadline = Instant::now() + Duration::from_secs(10);
    let waiters = WaiterState::new(WaiterPolicy {
        deadline: Some(first_deadline),
        attempt_timeout: Duration::from_secs(10),
    });
    let read = RpcRead::eth_call(Address::ZERO, Bytes::new());
    let request_waiters = waiters.clone();
    let client = reqwest::Client::new();
    let semaphore = Arc::new(Semaphore::new(1));
    let request = tokio::spawn(async move {
        execute_single_with_shared_waiters(
            &client,
            &semaphore,
            &read,
            1,
            &endpoints,
            request_waiters,
            Duration::from_secs(5),
        )
        .await
    });
    time::timeout(
        Duration::from_secs(5),
        first_gate.request_started.notified(),
    )
    .await
    .expect("first physical request should start");
    waiters.add(WaiterPolicy {
        deadline: Some(Instant::now() + Duration::from_mins(1)),
        attempt_timeout: Duration::from_secs(30),
    });
    time::pause();
    time::advance(Duration::from_secs(11)).await;
    time::resume();
    time::timeout(
        Duration::from_secs(5),
        second_gate.request_started.notified(),
    )
    .await
    .expect("late waiter should permit failover after the original deadline");
    time::pause();
    time::advance(Duration::from_secs(11)).await;
    tokio::task::yield_now().await;
    time::resume();
    assert!(
        !request.is_finished(),
        "future attempt uses the late waiter's longer cap"
    );
    second_gate.release_response.notify_one();
    let (result, attempts) = time::timeout(Duration::from_secs(5), request)
        .await
        .expect("shared failover should complete")
        .expect("shared failover task");
    assert_eq!(result, Ok(data_result(Bytes::from_static(b"\x02"))));
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].0, second);
    first_gate.release_response.notify_waiters();
    first_server.abort();
    second_server.abort();
}

#[tokio::test(start_paused = true)]
async fn shared_waiter_resnapshots_after_semaphore_wait() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = spawn_rpc_mock(
        Arc::new(|request| rpc_result(&request, &json!("0x02"))),
        active,
        maximum,
    )
    .await;
    let endpoints = [SensitiveUrl::from(endpoint)];
    let initial_deadline = Instant::now() + Duration::from_secs(1);
    let waiters = WaiterState::new(WaiterPolicy {
        deadline: Some(initial_deadline),
        attempt_timeout: Duration::from_secs(5),
    });
    let client = reqwest::Client::new();
    let semaphore = Arc::new(Semaphore::new(0));
    let request = tokio::spawn({
        let waiters = waiters.clone();
        let semaphore = semaphore.clone();
        async move {
            execute_single_with_shared_waiters(
                &client,
                &semaphore,
                &RpcRead::eth_call(Address::ZERO, Bytes::new()),
                1,
                &endpoints,
                waiters,
                Duration::from_secs(5),
            )
            .await
        }
    });
    tokio::task::yield_now().await;
    time::advance(Duration::from_secs(2)).await;
    waiters.add(WaiterPolicy {
        deadline: Some(Instant::now() + Duration::from_secs(10)),
        attempt_timeout: Duration::from_secs(5),
    });
    time::resume();
    semaphore.add_permits(1);
    let (result, attempts) = request.await.expect("shared semaphore request");
    assert_eq!(result, Ok(data_result(Bytes::from_static(b"\x02"))));
    assert_eq!(attempts.len(), 1);
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn shared_failover_stops_without_live_waiters() {
    let first_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let first_responder: RpcResponder = Arc::new(|request| rpc_error(&request, -32005));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_calls_for_responder = second_calls.clone();
    let second_responder: RpcResponder = Arc::new(move |request| {
        second_calls_for_responder.fetch_add(1, Ordering::SeqCst);
        rpc_result(&request, &json!("0x02"))
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (first, first_server) = spawn_gated_rpc_mock(
        first_responder,
        active.clone(),
        maximum.clone(),
        first_gate.clone(),
    )
    .await;
    let (second, second_server) = spawn_rpc_mock(second_responder, active, maximum).await;
    let endpoints = [SensitiveUrl::from(first), SensitiveUrl::from(second)];
    let waiters = WaiterState::new(WaiterPolicy {
        deadline: Some(Instant::now() + Duration::from_millis(100)),
        attempt_timeout: Duration::from_secs(5),
    });
    let read = RpcRead::eth_call(Address::ZERO, Bytes::new());
    let client = reqwest::Client::new();
    let semaphore = Arc::new(Semaphore::new(1));
    let request = tokio::spawn({
        let waiters = waiters.clone();
        async move {
            execute_single_with_shared_waiters(
                &client,
                &semaphore,
                &read,
                1,
                &endpoints,
                waiters,
                Duration::from_secs(5),
            )
            .await
        }
    });
    first_gate.request_started.notified().await;
    time::advance(Duration::from_millis(200)).await;
    first_gate.release_response.notify_waiters();
    let (result, attempts) = request.await.expect("shared no-continuation task");
    assert!(matches!(result, Err(RpcBrokerError::Timeout)));
    assert_eq!(attempts.len(), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn aggregate_predispatch_timeout_does_not_replace_prior_endpoint_error() {
    let multicall = Address::from([10_u8; 20]);
    let first_gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let first_responder: RpcResponder = Arc::new(|request| rpc_error(&request, -32005));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_calls_for_responder = second_calls.clone();
    let second_responder: RpcResponder = Arc::new(move |request| {
        second_calls_for_responder.fetch_add(1, Ordering::SeqCst);
        aggregate_response(&request, vec![(true, Bytes::from_static(b"second"))])
    });
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (first, first_server) = spawn_gated_rpc_mock(
        first_responder,
        active.clone(),
        maximum.clone(),
        first_gate.clone(),
    )
    .await;
    let (second, second_server) = spawn_rpc_mock(second_responder, active, maximum).await;
    let route =
        RpcRoute::new(RpcChainRoute::new(1, vec![first.clone(), second]).with_multicall(multicall));
    let read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"\x70\xa0\x82\x31"));
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
            // Leaves real time to dispatch under load; the test then skips past it.
            deadline: Some(Instant::now() + Duration::from_secs(30)),
            attempt_timeout: route.attempt_timeout,
        }),
    };
    let items = vec![item];
    let semaphore = Arc::new(Semaphore::new(1));
    let request_semaphore = semaphore.clone();
    let request = tokio::spawn(async move {
        execute_aggregate_with_failover(reqwest::Client::new(), request_semaphore, route, &items)
            .await
    });
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
        .expect("aggregate failover deadline")
        .expect("aggregate failover task");
    assert_eq!(output.values, vec![Err(remote_error(-32005))]);
    assert_eq!(output.attempts.len(), 1);
    assert_eq!(output.attempts[0].endpoint, first);
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    blocker_release.notify_one();
    blocker.await.expect("blocker task");
    first_server.abort();
    second_server.abort();
}
