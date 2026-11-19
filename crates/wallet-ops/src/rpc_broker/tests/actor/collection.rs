use super::*;

#[test]
fn spawn_requires_tokio_runtime() {
    assert!(matches!(
        RpcBroker::spawn(reqwest::Client::new(), reqwest::Client::new()),
        Err(RpcBrokerSpawnError::NoRuntime(_))
    ));
}

#[test]
fn spawn_on_rejects_stale_explicit_handle() {
    let handle = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        handle
    };

    assert!(matches!(
        RpcBroker::spawn_on(reqwest::Client::new(), reqwest::Client::new(), &handle),
        Err(RpcBrokerSpawnError::ActorStartup)
    ));
}

#[test]
fn spawn_rejects_stale_entered_handle() {
    let handle = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        handle
    };
    let _entered = handle.enter();

    assert!(matches!(
        RpcBroker::spawn(reqwest::Client::new(), reqwest::Client::new()),
        Err(RpcBrokerSpawnError::ActorStartup)
    ));
}

#[test]
fn spawn_on_schedules_actor_eagerly() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = runtime.handle().clone();
    let before = handle.metrics().num_alive_tasks();
    let broker = RpcBroker::spawn_on(reqwest::Client::new(), reqwest::Client::new(), &handle)
        .expect("live runtime should schedule the broker actor");
    assert_eq!(handle.metrics().num_alive_tasks(), before + 2);

    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn actor_keeps_collection_window_across_later_arrival_and_deadline_wake() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_resources, group, _endpoints, _control| {
            executions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                JobOutput {
                    completions: group
                        .into_iter()
                        .map(|item| (item.key, Ok(data_result(read_calldata(&item.read)))))
                        .collect(),
                    requests: Vec::new(),
                }
            })
        })
    };
    let broker = test_broker_with_executor(Duration::from_secs(10), 1, executor);
    let first_route = test_route().with_request_timeout(Duration::from_secs(2));
    let first = tokio::spawn({
        let broker = broker.clone();
        async move {
            broker
                .submit(RpcSubmission::new(
                    first_route,
                    vec![RpcRead::eth_call(
                        Address::ZERO,
                        Bytes::from_static(b"first"),
                    )],
                    test_origin(),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;
    time::advance(Duration::from_secs(1)).await;
    let second = tokio::spawn({
        let broker = broker.clone();
        async move {
            broker
                .submit(RpcSubmission::new(
                    test_route(),
                    vec![RpcRead::eth_call(
                        Address::ZERO,
                        Bytes::from_static(b"second"),
                    )],
                    test_origin(),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;
    time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    match first.await.unwrap() {
        Ok(first_result) => {
            assert_eq!(first_result.len(), 1);
            assert!(matches!(
                first_result[0],
                Err(RpcBrokerError::TimeoutBeforeDispatch)
            ));
        }
        Err(RpcBrokerError::Timeout) => {}
        Err(error) => panic!("unexpected first submission error: {error}"),
    }
    time::advance(Duration::from_secs(8)).await;
    let second_result = second.await.unwrap().expect("later submission");
    assert_eq!(
        second_result,
        vec![Ok(data_result(Bytes::from_static(b"second")))]
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn duplicate_waiters_share_one_execution_and_keep_independent_deadlines() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_resources, group, _endpoints, _control| {
            executions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                time::sleep(Duration::from_millis(150)).await;
                JobOutput {
                    completions: group
                        .into_iter()
                        .map(|item| (item.key, Ok(data_result(read_calldata(&item.read)))))
                        .collect(),
                    requests: Vec::new(),
                }
            })
        })
    };
    let broker = test_broker_with_executor(Duration::from_millis(1), 1, executor);
    let submit = |route: RpcRoute| {
        let broker = broker.clone();
        tokio::spawn(async move {
            broker
                .submit(RpcSubmission::new(
                    route,
                    vec![
                        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"shared"))
                            .with_test_block(BlockNumberOrTag::Number(1)),
                    ],
                    test_origin(),
                ))
                .await
        })
    };
    let early = submit(test_route().with_request_timeout(Duration::from_millis(100)));
    let late = submit(test_route().with_request_timeout(Duration::from_millis(300)));

    match early.await.unwrap() {
        Ok(results) => assert!(matches!(
            results.as_slice(),
            [Err(RpcBrokerError::TimeoutBeforeDispatch)]
        )),
        Err(RpcBrokerError::Timeout) => {}
        Err(error) => panic!("unexpected early waiter error: {error}"),
    }
    assert_eq!(
        late.await.unwrap().expect("later waiter"),
        vec![Ok(data_result(Bytes::from_static(b"shared")))]
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn duplicate_reads_share_one_request_under_the_strictest_attempt_cap() {
    let gate = RpcMockGate {
        request_started: Arc::new(tokio::sync::Notify::new()),
        release_response: Arc::new(tokio::sync::Notify::new()),
    };
    let responder: RpcResponder = Arc::new(|request| rpc_result(&request, &json!("0x01")));
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) =
        spawn_gated_rpc_mock(responder, active, maximum.clone(), gate.clone()).await;
    let broker = test_broker(Duration::from_millis(1), 2);
    let chain = RpcChainRoute::new(1, vec![endpoint]);
    let submit = |route: RpcRoute| {
        let broker = broker.clone();
        tokio::spawn(async move {
            broker
                .submit(RpcSubmission::new(
                    route,
                    vec![
                        RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"capped"))
                            .with_test_block(BlockNumberOrTag::Number(4)),
                    ],
                    test_origin(),
                ))
                .await
        })
    };
    let lenient = submit(RpcRoute::new(chain.clone()).with_attempt_timeout(Duration::from_secs(5)));
    let strict = submit(RpcRoute::new(chain).with_attempt_timeout(Duration::from_millis(100)));
    gate.request_started.notified().await;
    time::advance(Duration::from_millis(200)).await;
    for _ in 0..64 {
        if lenient.is_finished() && strict.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        lenient.is_finished() && strict.is_finished(),
        "the strictest waiter cap must end the shared request"
    );

    assert_eq!(
        lenient.await.unwrap().expect("lenient submission"),
        vec![Err(RpcBrokerError::Timeout)]
    );
    assert_eq!(
        strict.await.unwrap().expect("strict submission"),
        vec![Err(RpcBrokerError::Timeout)]
    );
    assert_eq!(maximum.load(Ordering::SeqCst), 1);
    gate.release_response.notify_waiters();
    drop(broker);
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn aggregated_reads_keep_distinct_policies_and_per_member_deadlines() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let executor: JobExecutor = {
        let observed = observed.clone();
        Arc::new(move |_resources, group, _endpoints, _control| {
            observed
                .lock()
                .expect("aggregate policy observation lock")
                .push(execution_job_len(&group));
            Box::pin(async move {
                time::sleep(Duration::from_millis(200)).await;
                let now = Instant::now();
                JobOutput {
                    completions: group
                        .into_iter()
                        .map(|item| {
                            let value = if item
                                .waiters
                                .merged_deadline()
                                .is_some_and(|deadline| deadline <= now)
                            {
                                Err(RpcBrokerError::TimeoutBeforeDispatch)
                            } else {
                                Ok(data_result(read_calldata(&item.read)))
                            };
                            (item.key, value)
                        })
                        .collect(),
                    requests: Vec::new(),
                }
            })
        })
    };
    let broker = test_broker_with_executor(Duration::from_millis(1), 1, executor);
    let multicall = Address::from([61_u8; 20]);
    let submission = |route: RpcRoute, calldata: &'static [u8]| {
        RpcSubmission::new(
            route,
            vec![RpcRead::eth_call(
                Address::from([1_u8; 20]),
                Bytes::from_static(calldata),
            )],
            test_origin(),
        )
    };
    let strict = test_route_with_multicall(multicall)
        .with_attempt_timeout(Duration::from_millis(100))
        .with_request_timeout(Duration::from_millis(150));
    let lenient = test_route_with_multicall(multicall)
        .with_attempt_timeout(Duration::from_secs(5))
        .with_request_timeout(Duration::from_secs(1));
    let (strict_result, lenient_result) = tokio::join!(
        broker.submit(submission(strict, b"strict")),
        broker.submit(submission(lenient, b"lenient")),
    );

    match strict_result {
        Ok(results) => assert!(matches!(
            results.as_slice(),
            [Err(RpcBrokerError::TimeoutBeforeDispatch)]
        )),
        Err(RpcBrokerError::Timeout) => {}
        Err(error) => panic!("unexpected strict member error: {error}"),
    }
    assert_eq!(
        lenient_result.expect("lenient member"),
        vec![Ok(data_result(Bytes::from_static(b"lenient")))]
    );
    assert_eq!(
        observed
            .lock()
            .expect("aggregate policy observations")
            .as_slice(),
        [2]
    );
    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn a_queued_read_keeps_its_own_deadline_when_the_running_read_expires() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_resources, group, _endpoints, _control| {
            executions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                time::sleep(Duration::from_millis(150)).await;
                JobOutput {
                    completions: group
                        .into_iter()
                        .map(|item| (item.key, Ok(data_result(read_calldata(&item.read)))))
                        .collect(),
                    requests: Vec::new(),
                }
            })
        })
    };
    let broker = test_broker_with_executor(Duration::from_millis(1), 1, executor);
    let submit = |route: RpcRoute, calldata: &'static [u8]| {
        let broker = broker.clone();
        tokio::spawn(async move {
            broker
                .submit(RpcSubmission::new(
                    route,
                    vec![RpcRead::eth_call(
                        Address::ZERO,
                        Bytes::from_static(calldata),
                    )],
                    test_origin(),
                ))
                .await
        })
    };
    let running = submit(
        test_route().with_request_timeout(Duration::from_millis(100)),
        b"running",
    );
    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let queued = submit(
        test_route().with_request_timeout(Duration::from_millis(500)),
        b"queued",
    );
    tokio::task::yield_now().await;

    match running.await.unwrap() {
        Ok(results) => assert!(matches!(
            results.as_slice(),
            [Err(RpcBrokerError::TimeoutBeforeDispatch)]
        )),
        Err(RpcBrokerError::Timeout) => {}
        Err(error) => panic!("unexpected running waiter error: {error}"),
    }
    assert_eq!(
        queued.await.unwrap().expect("queued waiter"),
        vec![Ok(data_result(Bytes::from_static(b"queued")))]
    );
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    drop(broker);
}

#[test]
fn waiter_policies_expire_independently_and_select_shortest_live_cap() {
    let now = Instant::now();
    let state = WaiterState::new(WaiterPolicy {
        deadline: Some(now + Duration::from_secs(1)),
        attempt_timeout: Duration::from_secs(5),
    });
    state.add(WaiterPolicy {
        deadline: Some(now + Duration::from_secs(3)),
        attempt_timeout: Duration::from_millis(100),
    });
    let snapshot = state.snapshot(now);
    assert!(
        snapshot
            .deadline
            .is_some_and(|deadline| deadline >= now + Duration::from_secs(3))
    );
    assert_eq!(snapshot.attempt_timeout, Duration::from_millis(100));
    assert!(state.snapshot(now).has_live);
    assert!(state.snapshot(now + Duration::from_secs(2)).has_live);
    let snapshot = state.snapshot(now + Duration::from_secs(2));
    assert!(
        snapshot
            .deadline
            .is_some_and(|deadline| deadline >= now + Duration::from_secs(3))
    );
    assert_eq!(snapshot.attempt_timeout, Duration::from_millis(100));
    assert!(!state.snapshot(now + Duration::from_secs(4)).has_live);
    assert_eq!(
        state.snapshot(now + Duration::from_secs(4)),
        WaiterSnapshot {
            has_live: false,
            deadline: None,
            attempt_timeout: Duration::ZERO,
        }
    );

    let unbounded_first = WaiterState::new(WaiterPolicy {
        deadline: None,
        attempt_timeout: Duration::from_secs(4),
    });
    unbounded_first.add(WaiterPolicy {
        deadline: Some(now + Duration::from_secs(2)),
        attempt_timeout: Duration::from_millis(200),
    });
    let bounded_first = WaiterState::new(WaiterPolicy {
        deadline: Some(now + Duration::from_secs(2)),
        attempt_timeout: Duration::from_millis(200),
    });
    bounded_first.add(WaiterPolicy {
        deadline: None,
        attempt_timeout: Duration::from_secs(4),
    });
    assert_eq!(
        unbounded_first.snapshot(now).deadline,
        bounded_first.snapshot(now).deadline
    );
    assert_eq!(
        unbounded_first.snapshot(now).attempt_timeout,
        Duration::from_millis(200)
    );
}

#[tokio::test]
async fn actor_resolves_each_read_and_does_not_leave_waiters_on_empty_route() {
    let broker = test_broker(Duration::from_millis(1), 1);
    let route = RpcRoute::new(RpcChainRoute::new(1, Vec::<Url>::new()));
    let submission = RpcSubmission::new(
        route,
        vec![RpcRead::eth_call(Address::ZERO, Bytes::new())],
        test_origin(),
    );
    let result = broker.submit(submission).await.unwrap();
    assert!(matches!(
        result.as_slice(),
        [Err(RpcBrokerError::NoEndpoint { chain_id: 1 })]
    ));
    drop(broker);
}

#[tokio::test]
async fn callers_keep_their_ordered_results_when_collected_together() {
    let broker = test_broker_with_executor(Duration::from_millis(5), 2, test_executor());
    let first = RpcSubmission::new(
        test_route(),
        vec![
            RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[1])),
            RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[2])),
        ],
        test_origin(),
    );
    let second = RpcSubmission::new(
        test_route(),
        vec![
            RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[3])),
            RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[4])),
        ],
        test_origin(),
    );
    let (first, second) = tokio::join!(broker.submit(first), broker.submit(second));
    assert_eq!(
        first.unwrap(),
        vec![
            Ok(data_result(Bytes::from_static(&[1]))),
            Ok(data_result(Bytes::from_static(&[2])))
        ]
    );
    assert_eq!(
        second.unwrap(),
        vec![
            Ok(data_result(Bytes::from_static(&[3]))),
            Ok(data_result(Bytes::from_static(&[4])))
        ]
    );
    drop(broker);
}

#[tokio::test]
async fn route_threshold_flushes_before_admitting_the_next_read() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_resources, group, _endpoints, _control| {
            executions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                JobOutput {
                    completions: group
                        .into_iter()
                        .map(|item| (item.key, Ok(data_result(read_calldata(&item.read)))))
                        .collect(),
                    requests: Vec::new(),
                }
            })
        })
    };
    let broker = test_broker_with_executor(Duration::from_millis(10), 1, executor);
    let route =
        test_route_with_multicall(Address::from([5_u8; 20])).with_test_thresholds(2, 1_000_000);
    let make_submission = |value| {
        RpcSubmission::new(
            route.clone(),
            vec![RpcRead::eth_call(Address::ZERO, Bytes::from(vec![value]))],
            test_origin(),
        )
    };
    let (first, second, third) = tokio::join!(
        broker.submit(make_submission(11)),
        broker.submit(make_submission(12)),
        broker.submit(make_submission(13)),
    );
    assert!(first.is_ok() && second.is_ok() && third.is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    drop(broker);
}
