use super::*;

#[test]
fn spawn_requires_tokio_runtime() {
    assert!(matches!(
        RpcBroker::spawn(reqwest::Client::new()),
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
        RpcBroker::spawn_on(reqwest::Client::new(), &handle),
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
        RpcBroker::spawn(reqwest::Client::new()),
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
    let broker = RpcBroker::spawn_on(reqwest::Client::new(), &handle)
        .expect("live runtime should schedule the broker actor");
    assert_eq!(handle.metrics().num_alive_tasks(), before + 2);

    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn actor_keeps_collection_window_across_later_arrival_and_deadline_wake() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
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
        Arc::new(move |_client, _semaphore, group, _endpoints| {
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
    let lenient =
        submit(RpcRoute::from(chain.clone()).with_attempt_timeout(Duration::from_secs(5)));
    let strict = submit(RpcRoute::from(chain).with_attempt_timeout(Duration::from_millis(100)));
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
        Arc::new(move |_client, _semaphore, group, _endpoints| {
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
        Arc::new(move |_client, _semaphore, group, _endpoints| {
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
    let route = RpcRoute::from(RpcChainRoute::new(1, Vec::<Url>::new()));
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
        Arc::new(move |_client, _semaphore, group, _endpoints| {
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

#[test]
fn partition_work_chunks_by_count_and_by_gas_without_wrapping() {
    let work_items = |route: &RpcRoute, reads: Vec<RpcRead>| -> Vec<WorkItem> {
        reads
            .into_iter()
            .enumerate()
            .map(|(nonce, read)| WorkItem {
                key: WorkKey {
                    identity: read.identity_for_route(route),
                    route: route.chain_route().clone(),
                    nonce: nonce as u64,
                    latest_epoch: None,
                },
                execution_route: route.clone(),
                read,
                origins: vec![test_origin()],
                waiters: WaiterState::new(WaiterPolicy {
                    deadline: None,
                    attempt_timeout: route.attempt_timeout,
                }),
            })
            .collect()
    };

    let counted = test_route().with_test_thresholds(2, 200_000);
    let chunks = partition_work(work_items(
        &counted,
        (0_u8..5)
            .map(|marker| RpcRead::eth_call(Address::ZERO, Bytes::from(vec![marker])))
            .collect(),
    ));
    let markers = chunks
        .iter()
        .map(|chunk| {
            chunk
                .iter()
                .map(|item| read_calldata(&item.read).as_ref()[0])
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(markers, vec![vec![0, 1], vec![2, 3], vec![4]]);

    let overflowing = test_route().with_test_thresholds(100, u64::MAX);
    let gas_read = |calldata: &'static [u8], gas: u64| {
        rpc_read_from_request(
            TransactionRequest {
                to: Some(Address::ZERO.into()),
                gas: Some(gas),
                input: TransactionInput::new(Bytes::from_static(calldata)),
                ..TransactionRequest::default()
            },
            BlockId::latest(),
            None,
        )
    };
    let chunks = partition_work(work_items(
        &overflowing,
        vec![
            gas_read(b"first", u64::MAX),
            gas_read(b"second", 1),
            gas_read(b"third", 2),
        ],
    ));
    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(
        chunks
            .iter()
            .flat_map(|chunk| chunk.iter().map(|item| read_calldata(&item.read)))
            .collect::<Vec<_>>(),
        vec![
            Bytes::from_static(b"first"),
            Bytes::from_static(b"second"),
            Bytes::from_static(b"third"),
        ]
    );
}

#[tokio::test]
async fn individual_ready_work_is_bounded_by_top_level_concurrency() {
    let started = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let executor: JobExecutor = {
        let started = started.clone();
        let gate = gate.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
            assert!(matches!(&group, ExecutionJob::Individual(_)));
            started.fetch_add(1, Ordering::SeqCst);
            let gate = gate.clone();
            Box::pin(async move {
                let _permit = gate.acquire_owned().await.expect("execution gate permit");
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
    let broker = test_broker_with_executor(Duration::from_millis(1), 4, executor);
    let reads = (0_u8..64)
        .map(|marker| RpcRead::eth_call(Address::ZERO, Bytes::from(vec![marker])))
        .collect::<Vec<_>>();
    let submission = tokio::spawn({
        let broker = broker.clone();
        async move {
            broker
                .submit(RpcSubmission::new(test_route(), reads, test_origin()))
                .await
        }
    });
    time::sleep(Duration::from_millis(20)).await;
    assert_eq!(started.load(Ordering::SeqCst), 4);

    gate.add_permits(64);
    let results = submission.await.unwrap().expect("bounded submission");
    assert_eq!(results.len(), 64);
    assert!(results.iter().all(Result::is_ok));
    drop(broker);
}

#[tokio::test]
async fn aggregate_completion_is_delivered_while_individual_job_is_blocked() {
    let gate = Arc::new(Semaphore::new(0));
    let executor: JobExecutor = {
        let gate = gate.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
            let aggregate = group.first().is_some_and(|item| {
                item.execution_route.multicall().is_some() && item.read.is_multicall_eligible()
            });
            let gate = gate.clone();
            Box::pin(async move {
                if !aggregate {
                    let _permit = gate.acquire_owned().await.expect("execution gate permit");
                }
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
    let broker = test_broker_with_executor(Duration::from_millis(1), 2, executor);
    let route = test_route_with_multicall(Address::from([7_u8; 20]));
    let individual = tokio::spawn({
        let broker = broker.clone();
        let route = route.clone();
        async move {
            broker
                .submit(RpcSubmission::new(
                    route,
                    vec![rpc_read_from_request(
                        TransactionRequest {
                            to: Some(Address::ZERO.into()),
                            value: Some(U256::from(1)),
                            input: TransactionInput::new(Bytes::from_static(b"individual")),
                            ..TransactionRequest::default()
                        },
                        BlockId::latest(),
                        None,
                    )],
                    test_origin(),
                ))
                .await
        }
    });
    let aggregate = tokio::spawn({
        let broker = broker.clone();
        async move {
            broker
                .submit(RpcSubmission::new(
                    route,
                    vec![RpcRead::eth_call(
                        Address::ZERO,
                        Bytes::from_static(b"aggregate"),
                    )],
                    test_origin(),
                ))
                .await
        }
    });

    let delivered = time::timeout(Duration::from_secs(1), aggregate)
        .await
        .expect("aggregate completion must not wait for the blocked individual job")
        .unwrap()
        .expect("aggregate submission");
    assert_eq!(
        delivered,
        vec![Ok(data_result(Bytes::from_static(b"aggregate")))]
    );
    gate.add_permits(1);
    assert!(individual.await.unwrap().is_ok());
    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn queued_duplicate_reads_expire_without_dispatch() {
    let executions = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Semaphore::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        let gate = gate.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
            executions.fetch_add(1, Ordering::SeqCst);
            let gate = gate.clone();
            Box::pin(async move {
                let _permit = gate.acquire_owned().await.expect("execution gate permit");
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
    let queued_route = test_route().with_request_timeout(Duration::from_millis(500));
    let submit = |route: RpcRoute, calldata: &'static [u8], origin: RpcOrigin| {
        let broker = broker.clone();
        tokio::spawn(async move {
            broker
                .submit(RpcSubmission::new(
                    route,
                    vec![
                        RpcRead::eth_call(Address::ZERO, Bytes::from_static(calldata))
                            .with_test_block(BlockNumberOrTag::Number(1)),
                    ],
                    origin,
                ))
                .await
        })
    };
    let blocking = submit(
        test_route(),
        b"blocking",
        WalletRpcOrigin::PublicWallet.into(),
    );
    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    let first = submit(
        queued_route.clone(),
        b"queued",
        WalletRpcOrigin::PublicWallet.into(),
    );
    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(2)).await;
    tokio::task::yield_now().await;
    let second = submit(queued_route, b"queued", WalletRpcOrigin::Staking.into());
    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(2)).await;
    tokio::task::yield_now().await;
    time::advance(Duration::from_millis(600)).await;

    for queued in [first, second] {
        match queued.await.unwrap() {
            Ok(results) => assert!(matches!(
                results.as_slice(),
                [Err(RpcBrokerError::TimeoutBeforeDispatch)]
            )),
            Err(RpcBrokerError::Timeout) => {}
            Err(error) => panic!("unexpected queued waiter error: {error}"),
        }
    }
    gate.add_permits(1);
    assert!(blocking.await.unwrap().is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    drop(broker);
}

#[tokio::test]
async fn duplicate_submissions_attach_to_an_in_flight_execution() {
    let selected_endpoints = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executions = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let started = Arc::new(tokio::sync::Notify::new());
    let executor: JobExecutor = {
        let selected_endpoints = selected_endpoints.clone();
        let executions = executions.clone();
        let gate = gate.clone();
        let started = started.clone();
        Arc::new(move |_client, _semaphore, group, endpoints| {
            executions.fetch_add(1, Ordering::SeqCst);
            let selected_endpoints = selected_endpoints.clone();
            let gate = gate.clone();
            started.notify_one();
            Box::pin(async move {
                gate.notified().await;
                let mut completions = Vec::new();
                let mut requests = Vec::new();
                for item in group {
                    let endpoint = endpoints
                        .first()
                        .expect("selected endpoint")
                        .expose_url()
                        .clone();
                    selected_endpoints.lock().unwrap().push(endpoint.clone());
                    requests.push(RequestEvent {
                        chain_id: item.execution_route.chain_id(),
                        endpoint,
                        health_outcome: EndpointHealthOutcome::Healthy,
                    });
                    completions.push((item.key, Err(remote_error(-32042))));
                }
                JobOutput {
                    completions,
                    requests,
                }
            })
        })
    };
    let broker = test_broker_with_executor(Duration::from_millis(1), 1, executor);
    broker.notify_block(1, 1);
    time::sleep(Duration::from_millis(5)).await;
    let route = RpcRoute::from(RpcChainRoute::new(
        1,
        vec![
            Url::parse("https://first-selected.invalid").unwrap(),
            Url::parse("https://second-selected.invalid").unwrap(),
        ],
    ));
    let warmup = tokio::spawn({
        let broker = broker.clone();
        let route = route.clone();
        async move {
            broker
                .submit(RpcSubmission::new(
                    route,
                    vec![RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[13]))],
                    WalletRpcOrigin::PublicWallet.into(),
                ))
                .await
        }
    });
    started.notified().await;
    gate.notify_one();
    assert!(warmup.await.unwrap().is_ok());

    let submission = RpcSubmission::new(
        route.clone(),
        vec![RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[14]))],
        WalletRpcOrigin::PublicWallet.into(),
    );
    let first = tokio::spawn({
        let broker = broker.clone();
        let submission = submission.clone();
        async move { broker.submit(submission).await }
    });
    started.notified().await;
    let equivalent_route = RpcRoute::from(RpcChainRoute::new(
        route.chain_id(),
        route.chain_route().endpoint_urls(),
    ));
    let equivalent_submission = RpcSubmission::new(
        equivalent_route,
        vec![RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[14]))],
        WalletRpcOrigin::Staking.into(),
    );
    let second = tokio::spawn({
        let broker = broker.clone();
        async move { broker.submit(equivalent_submission).await }
    });
    time::sleep(Duration::from_millis(10)).await;
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    gate.notify_one();
    let expected = Err(remote_error(-32042));
    assert_eq!(first.await.unwrap(), Ok(vec![expected.clone()]));
    assert_eq!(second.await.unwrap(), Ok(vec![expected]));
    assert_eq!(
        selected_endpoints.lock().unwrap().as_slice(),
        &[
            Url::parse("https://first-selected.invalid").unwrap(),
            Url::parse("https://second-selected.invalid").unwrap(),
        ]
    );
    drop(broker);
}

#[tokio::test]
async fn unknown_block_latest_reads_do_not_deduplicate_in_flight() {
    let executions = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let executor: JobExecutor = {
        let executions = executions.clone();
        let gate = gate.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
            executions.fetch_add(execution_job_len(&group), Ordering::SeqCst);
            let gate = gate.clone();
            Box::pin(async move {
                gate.notified().await;
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
    let broker = test_broker_with_executor(Duration::from_millis(1), 2, executor);
    let submission = RpcSubmission::new(
        test_route(),
        vec![RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[16]))],
        test_origin(),
    );
    let first = tokio::spawn({
        let broker = broker.clone();
        let submission = submission.clone();
        async move { broker.submit(submission).await }
    });
    let second = tokio::spawn({
        let broker = broker.clone();
        async move { broker.submit(submission).await }
    });
    time::sleep(Duration::from_millis(10)).await;
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    gate.notify_waiters();
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    drop(broker);
}

#[tokio::test]
async fn eth_call_and_balance_reads_do_not_share_cache_or_dedup_identity() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
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
    let broker = test_broker_with_executor(Duration::from_millis(1), 1, executor);
    broker.notify_block(1, 10);
    time::sleep(Duration::from_millis(5)).await;
    let route = test_route();
    let target = Address::from([6_u8; 20]);
    let eth_call = RpcRead::eth_call(target, Bytes::new());
    let balance = RpcRead::get_balance(target);
    for read in [eth_call.clone(), balance.clone(), eth_call, balance] {
        assert!(
            broker
                .submit(RpcSubmission::new(route.clone(), vec![read], test_origin(),))
                .await
                .unwrap()[0]
                .is_ok()
        );
    }
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    drop(broker);
}

#[tokio::test]
async fn late_waiter_requeues_an_aggregate_member_expired_while_waiting_for_a_permit() {
    for shutdown in [false, true] {
        aggregate_member_expiry_with_late_waiter(shutdown).await;
    }
}

async fn aggregate_member_expiry_with_late_waiter(shutdown: bool) {
    time::pause();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let calls = observed.clone();
    let responder: RpcResponder = Arc::new(move |request| {
        let data = hex::decode(request["params"][0]["input"].as_str().unwrap()).unwrap();
        let decoded = IMulticall3::tryAggregateCall::abi_decode(&data).unwrap();
        calls.lock().unwrap().push(
            decoded
                .calls
                .iter()
                .map(|call| call.target)
                .collect::<Vec<_>>(),
        );
        aggregate_response(
            &request,
            decoded
                .calls
                .iter()
                .map(|_| (true, Bytes::from_static(b"ok")))
                .collect(),
        )
    });
    let (endpoint, server) = spawn_rpc_mock(
        responder,
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let semaphore = Arc::new(Semaphore::new(0));
    let (polled, mut polls) = mpsc::unbounded_channel();
    let executor: JobExecutor = {
        let semaphore = semaphore.clone();
        Arc::new(move |client, _semaphore, job, endpoints| {
            let mut future =
                super::super::execution::run_job(client, semaphore.clone(), job, endpoints);
            let polled = polled.clone();
            Box::pin(std::future::poll_fn(move |cx| {
                let result = future.as_mut().poll(cx);
                if result.is_pending() {
                    let _ = polled.send(());
                }
                result
            }))
        })
    };
    let (tx, rx) = mpsc::channel(8);
    let (_block_tx, block_rx) = mpsc::unbounded_channel();
    let actor = Actor::new(
        reqwest::Client::new(),
        rx,
        Duration::from_secs(30),
        1,
        executor,
    );
    let actor_task = tokio::spawn(actor.run(block_rx));
    let route = RpcRoute::from(
        RpcChainRoute::new(1, vec![endpoint]).with_multicall(Address::from([15_u8; 20])),
    )
    .with_test_thresholds(2, DEFAULT_MAX_ESTIMATED_GAS);
    let admission = Arc::new(Semaphore::new(4));
    let read = |index| {
        RpcRead::eth_call(Address::from([index; 20]), Bytes::new())
            .with_test_block(BlockNumberOrTag::Number(1))
    };
    let now = Instant::now();
    let mut results = Vec::new();
    for (index, seconds) in [(1, 1), (2, 30)] {
        let (sender, receiver) = oneshot::channel();
        results.push(receiver);
        tx.send(Command::Submit {
            submission: Box::new(RpcSubmission::new(
                route.clone(),
                vec![read(index)],
                test_origin(),
            )),
            replies: vec![resolution::ReadReply::new(
                sender,
                Arc::new(admission.clone().acquire_owned().await.unwrap()),
            )],
            deadline: Some(now + Duration::from_secs(seconds)),
        })
        .await
        .unwrap();
    }
    polls.recv().await.expect("aggregate waiting for a permit");
    while polls.try_recv().is_ok() {}
    time::advance(Duration::from_secs(2)).await;
    polls
        .recv()
        .await
        .expect("aggregate filtered the expired member and resumed waiting");

    let (sender, late_result) = oneshot::channel();
    tx.send(Command::Submit {
        submission: Box::new(RpcSubmission::new(
            route
                .clone()
                .with_test_thresholds(1, DEFAULT_MAX_ESTIMATED_GAS),
            vec![read(1)],
            RpcOrigin::from(WalletRpcOrigin::Governance),
        )),
        replies: vec![resolution::ReadReply::new(
            sender,
            Arc::new(admission.clone().acquire_owned().await.unwrap()),
        )],
        deadline: Some(now + Duration::from_mins(1)),
    })
    .await
    .unwrap();
    let (sender, barrier) = oneshot::channel();
    tx.send(Command::Submit {
        submission: Box::new(RpcSubmission::new(
            test_route(),
            vec![read(3)],
            test_origin(),
        )),
        replies: vec![resolution::ReadReply::new(
            sender,
            Arc::new(admission.clone().acquire_owned().await.unwrap()),
        )],
        deadline: Some(Instant::now()),
    })
    .await
    .unwrap();
    assert_eq!(
        barrier.await.unwrap(),
        Err(RpcBrokerError::TimeoutBeforeDispatch)
    );
    assert_eq!(admission.available_permits(), 1);
    let tx = if shutdown {
        let (sender, ready_result) = oneshot::channel();
        tx.send(Command::Submit {
            submission: Box::new(RpcSubmission::new(
                route.with_test_thresholds(1, DEFAULT_MAX_ESTIMATED_GAS),
                vec![read(3)],
                test_origin(),
            )),
            replies: vec![resolution::ReadReply::new(
                sender,
                Arc::new(admission.clone().acquire_owned().await.unwrap()),
            )],
            deadline: None,
        })
        .await
        .unwrap();
        drop(tx);
        // Ready work is failed before draining, proving channel closure was consumed
        // while the aggregate still waits for its physical permit.
        assert_eq!(ready_result.await.unwrap(), Err(RpcBrokerError::Shutdown));
        None
    } else {
        Some(tx)
    };
    time::resume();
    semaphore.add_permits(1);
    assert_eq!(
        time::timeout(Duration::from_secs(5), late_result)
            .await
            .expect("late waiter must be completed or shut down")
            .unwrap(),
        if shutdown {
            Err(RpcBrokerError::Shutdown)
        } else {
            Ok(data_result(Bytes::from_static(b"ok")))
        }
    );
    assert_eq!(
        results.pop().unwrap().await.unwrap(),
        Ok(data_result(Bytes::from_static(b"ok")))
    );
    drop(results);
    let mut expected = vec![vec![Address::from([2_u8; 20])]];
    if !shutdown {
        expected.push(vec![Address::from([1_u8; 20])]);
    }
    assert_eq!(*observed.lock().unwrap(), expected);
    drop(tx);
    actor_task.await.unwrap();
    assert_eq!(admission.available_permits(), 4);
    server.abort();
}

#[tokio::test]
async fn shutdown_drains_an_accepted_execution() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let executor: JobExecutor = {
        let started = started.clone();
        let release = release.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
            let started = started.clone();
            let release = release.clone();
            Box::pin(async move {
                started.notify_one();
                release.notified().await;
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
    let (tx, rx) = mpsc::channel(8);
    let (_block_tx, block_rx) = mpsc::unbounded_channel();
    let actor = Actor::new(
        reqwest::Client::new(),
        rx,
        Duration::from_secs(30),
        1,
        executor,
    );
    let actor_task = tokio::spawn(actor.run(block_rx));
    let route = test_route().with_test_thresholds(1, DEFAULT_MAX_ESTIMATED_GAS);
    let origin = test_origin();
    let admission = Arc::new(Semaphore::new(2));
    let (started_reply, started_result) = oneshot::channel();
    let started_reply = resolution::ReadReply::new(
        started_reply,
        Arc::new(admission.clone().acquire_owned().await.unwrap()),
    );
    tx.send(Command::Submit {
        submission: Box::new(RpcSubmission::new(
            route.clone(),
            vec![RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[15]))],
            origin.clone(),
        )),
        replies: vec![started_reply],
        deadline: None,
    })
    .await
    .expect("started submission should be accepted");
    started.notified().await;

    let (ready_reply, ready_result) = oneshot::channel();
    let ready_reply = resolution::ReadReply::new(
        ready_reply,
        Arc::new(admission.acquire_owned().await.unwrap()),
    );
    tx.send(Command::Submit {
        submission: Box::new(RpcSubmission::new(
            route,
            vec![RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[16]))],
            origin,
        )),
        replies: vec![ready_reply],
        deadline: None,
    })
    .await
    .expect("ready submission should be accepted");
    let (barrier, barrier_result) = oneshot::channel();
    let barrier = resolution::ReadReply::new(
        barrier,
        Arc::new(Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap()),
    );
    tx.send(Command::Submit {
        submission: Box::new(RpcSubmission::new(
            test_route(),
            vec![RpcRead::eth_call(Address::ZERO, Bytes::new())],
            test_origin(),
        )),
        replies: vec![barrier],
        deadline: Some(Instant::now()),
    })
    .await
    .expect("expired submission barrier should be accepted");
    assert_eq!(
        barrier_result
            .await
            .expect("actor should process the barrier"),
        Err(RpcBrokerError::TimeoutBeforeDispatch)
    );
    drop(tx);

    let ready = time::timeout(Duration::from_secs(1), ready_result)
        .await
        .expect("ready work must fail without waiting for the drain")
        .expect("ready reply");
    assert_eq!(ready, Err(RpcBrokerError::Shutdown));
    release.notify_one();
    assert_eq!(
        started_result.await.expect("started reply"),
        Ok(data_result(Bytes::from_static(&[15])))
    );
    actor_task
        .await
        .expect("actor should exit after channel closure");
}

#[tokio::test]
async fn block_notification_flushes_pending_work_after_other_branch_wins() {
    let broker = test_broker_with_executor(Duration::from_secs(30), 1, test_executor());
    let submission = RpcSubmission::new(
        test_route(),
        vec![RpcRead::eth_call(Address::ZERO, Bytes::from_static(&[8]))],
        test_origin(),
    );
    let pending = tokio::spawn({
        let broker = broker.clone();
        async move { broker.submit(submission).await }
    });
    time::sleep(Duration::from_millis(5)).await;
    broker.notify_block(1, 42);
    let result = time::timeout(Duration::from_secs(1), pending)
        .await
        .expect("block flush")
        .unwrap()
        .unwrap();
    assert_eq!(result, vec![Ok(data_result(Bytes::from_static(&[8])))]);
    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn stale_head_notification_does_not_flush_pending_work() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
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
    let broker = test_broker_with_executor(Duration::from_hours(1), 1, executor);
    broker.notify_block(1, 10);
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let pending = tokio::spawn({
        let broker = broker.clone();
        async move {
            broker
                .submit(RpcSubmission::new(
                    test_route(),
                    vec![RpcRead::eth_call(
                        Address::ZERO,
                        Bytes::from_static(b"pending"),
                    )],
                    test_origin(),
                ))
                .await
        }
    });
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }

    broker.notify_block(1, 9);
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(!pending.is_finished());

    broker.notify_block(1, 10);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        pending.await.unwrap().expect("pending submission"),
        vec![Ok(data_result(Bytes::from_static(b"pending")))]
    );
    drop(broker);
}

#[tokio::test]
async fn latest_cache_hits_within_a_block_and_invalidates_on_advance() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
            executions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let mut completions = Vec::new();
                let mut requests = Vec::new();
                for item in group {
                    let endpoint = item.execution_route.endpoints()[0].expose_url().clone();
                    requests.push(RequestEvent {
                        chain_id: item.execution_route.chain_id(),
                        endpoint,
                        health_outcome: EndpointHealthOutcome::Healthy,
                    });
                    completions.push((item.key, Ok(data_result(Bytes::from_static(b"cached")))));
                }
                JobOutput {
                    completions,
                    requests,
                }
            })
        })
    };
    let broker = test_broker_with_executor(Duration::from_millis(1), 1, executor);
    broker.notify_block(1, 10);
    time::sleep(Duration::from_millis(5)).await;
    let route = RpcRoute::from(RpcChainRoute::new(
        1,
        vec![Url::parse("https://cache.invalid").unwrap()],
    ));
    let read = RpcRead::eth_call(Address::from([4_u8; 20]), Bytes::from_static(b"read"));
    let make_submission = || RpcSubmission::new(route.clone(), vec![read.clone()], test_origin());
    assert_eq!(
        broker.submit(make_submission()).await.unwrap()[0],
        Ok(data_result(Bytes::from_static(b"cached")))
    );
    // Session installation and the sync-tip stream both seed the epoch, so the
    // same head can be reported more than once and a stale head can still
    // arrive. Neither may discard results observed at the recorded head.
    broker.notify_block(1, 10);
    time::sleep(Duration::from_millis(5)).await;
    broker.notify_block(1, 9);
    time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        broker.submit(make_submission()).await.unwrap()[0],
        Ok(data_result(Bytes::from_static(b"cached")))
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    broker.notify_block(1, 11);
    time::sleep(Duration::from_millis(5)).await;
    assert!(broker.submit(make_submission()).await.unwrap()[0].is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    let target = Address::from([5_u8; 20]);
    let tagged = RpcRead::eth_call(target, Bytes::from_static(b"tag"))
        .with_test_block(BlockNumberOrTag::Safe);
    assert!(!tagged.is_cacheable());
    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn latest_cache_lease_and_invalidation_preserve_numbered_entries() {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = executions.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
            executions.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                JobOutput {
                    completions: group
                        .into_iter()
                        .map(|item| (item.key, Ok(data_result(Bytes::from_static(b"cached")))))
                        .collect(),
                    requests: Vec::new(),
                }
            })
        })
    };
    let broker = test_broker_with_executor(Duration::from_mins(1), 1, executor);
    broker.notify_block(1, 10);
    broker.notify_block(2, 20);
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    let route = |chain_id| {
        RpcRoute::from(RpcChainRoute::new(
            chain_id,
            vec![Url::parse("https://lease-cache.invalid").unwrap()],
        ))
        .with_test_thresholds(1, DEFAULT_MAX_ESTIMATED_GAS)
    };
    let latest = || RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"latest"));
    let numbered = || {
        let mut read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"numbered"));
        read = read.with_test_block(BlockNumberOrTag::Number(7));
        read
    };
    let origin = || test_origin();
    let submit = |route: RpcRoute, read: RpcRead| async {
        broker
            .submit(RpcSubmission::new(route, vec![read], origin()))
            .await
            .expect("cache submission")
    };

    assert!(submit(route(1), latest()).await[0].is_ok());
    assert!(submit(route(2), latest()).await[0].is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    assert!(submit(route(1), latest()).await[0].is_ok());
    assert!(submit(route(2), latest()).await[0].is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 2);

    let before_lease = HEAD_FRESHNESS_LEASE
        .checked_sub(Duration::from_secs(1))
        .expect("freshness lease exceeds one second");
    time::advance(before_lease).await;
    broker.notify_block(1, 10);
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    time::advance(Duration::from_secs(2)).await;
    assert!(submit(route(1), latest()).await[0].is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    // Renewal must notice expiry even when no intervening read reconciled the cache.
    broker.notify_block(2, 20);
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    assert!(submit(route(2), latest()).await[0].is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 3);

    assert!(submit(route(1), numbered()).await[0].is_ok());
    assert!(submit(route(1), numbered()).await[0].is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 4);
    broker.invalidate_block(1);
    broker.notify_block(1, 10);
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    assert!(submit(route(1), latest()).await[0].is_ok());
    assert!(submit(route(1), numbered()).await[0].is_ok());
    assert_eq!(executions.load(Ordering::SeqCst), 5);
    drop(broker);
}

#[tokio::test(start_paused = true)]
async fn renewed_latest_head_isolates_active_reads_across_validity_boundaries() {
    #[derive(Clone, Copy, Debug)]
    enum Boundary {
        Invalidation,
        Expiry,
        ReconciledExpiry,
    }

    for boundary in [
        Boundary::Invalidation,
        Boundary::Expiry,
        Boundary::ReconciledExpiry,
    ] {
        for old_finishes_first in [true, false] {
            let (started_tx, mut started_rx) = mpsc::unbounded_channel();
            let executor: JobExecutor = Arc::new(move |_client, _semaphore, group, _endpoints| {
                let (release, response) = oneshot::channel::<Bytes>();
                started_tx
                    .send(release)
                    .expect("test receives each dispatch");
                Box::pin(async move {
                    let value = response.await.expect("test releases each dispatch");
                    JobOutput {
                        completions: group
                            .into_iter()
                            .map(|item| (item.key, Ok(data_result(value.clone()))))
                            .collect(),
                        requests: Vec::new(),
                    }
                })
            });
            let broker = test_broker_with_executor(Duration::from_millis(1), 3, executor);
            let route = test_route()
                .with_request_timeout(HEAD_FRESHNESS_LEASE * 10)
                .with_test_thresholds(1, DEFAULT_MAX_ESTIMATED_GAS);
            let submit = |marker: &'static [u8]| {
                let broker = broker.clone();
                let route = route.clone();
                tokio::spawn(async move {
                    broker
                        .submit(RpcSubmission::new(
                            route,
                            vec![RpcRead::eth_call(Address::ZERO, Bytes::from_static(marker))],
                            test_origin(),
                        ))
                        .await
                        .unwrap()
                })
            };
            broker.notify_block(1, 10);
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            let old = submit(b"shared");
            let old_release = started_rx.recv().await.expect("old dispatch");

            // A fresh equal-height notification must still share active work.
            broker.notify_block(1, 10);
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            let duplicate = submit(b"shared");
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert!(started_rx.try_recv().is_err());

            match boundary {
                Boundary::Invalidation => broker.invalidate_block(1),
                Boundary::Expiry | Boundary::ReconciledExpiry => {
                    time::advance(HEAD_FRESHNESS_LEASE + Duration::from_secs(1)).await;
                    if matches!(boundary, Boundary::ReconciledExpiry) {
                        let probe = submit(b"reconcile");
                        started_rx
                            .recv()
                            .await
                            .expect("reconciling dispatch")
                            .send(Bytes::from_static(b"probe"))
                            .unwrap();
                        assert_eq!(
                            probe.await.unwrap(),
                            vec![Ok(data_result(Bytes::from_static(b"probe")))]
                        );
                    }
                }
            }
            broker.notify_block(1, 10);
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            let new = submit(b"shared");
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            let new_release = started_rx
                .try_recv()
                .unwrap_or_else(|_| panic!("{boundary:?} must dispatch a new latest read"));
            let old_value = Bytes::from_static(b"old");
            let new_value = Bytes::from_static(b"new");
            if old_finishes_first {
                old_release.send(old_value.clone()).unwrap();
                assert_eq!(old.await.unwrap(), vec![Ok(data_result(old_value.clone()))]);
                assert_eq!(duplicate.await.unwrap(), vec![Ok(data_result(old_value))]);
                let later = submit(b"shared");
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                assert!(
                    !later.is_finished(),
                    "old completion must not seed the cache"
                );
                assert!(started_rx.try_recv().is_err());
                new_release.send(new_value.clone()).unwrap();
                assert_eq!(new.await.unwrap(), vec![Ok(data_result(new_value.clone()))]);
                assert_eq!(
                    later.await.unwrap(),
                    vec![Ok(data_result(new_value.clone()))]
                );
            } else {
                new_release.send(new_value.clone()).unwrap();
                assert_eq!(new.await.unwrap(), vec![Ok(data_result(new_value.clone()))]);
                old_release.send(old_value.clone()).unwrap();
                assert_eq!(old.await.unwrap(), vec![Ok(data_result(old_value.clone()))]);
                assert_eq!(duplicate.await.unwrap(), vec![Ok(data_result(old_value))]);
            }
            let cached = submit(b"shared");
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert!(
                cached.is_finished(),
                "{boundary:?} must cache the new result"
            );
            assert_eq!(cached.await.unwrap(), vec![Ok(data_result(new_value))]);
            assert!(started_rx.try_recv().is_err());
            drop(broker);
        }
    }
}
