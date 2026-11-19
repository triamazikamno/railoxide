use super::*;

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
        Arc::new(move |_resources, group, _endpoints, _control| {
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
    time::timeout(Duration::from_secs(5), async {
        while started.load(Ordering::SeqCst) < 4 {
            time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("four individual jobs start up to the concurrency bound");
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
        Arc::new(move |_resources, group, _endpoints, _control| {
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
        Arc::new(move |_resources, group, _endpoints, _control| {
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

#[tokio::test(start_paused = true)]
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
        Arc::new(move |_resources, group, endpoints, _control| {
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
    let route = RpcRoute::new(RpcChainRoute::new(
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
    let equivalent_route = RpcRoute::new(RpcChainRoute::new(
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
    // Paused time lets the broker's earlier flush finish before this sleep ends,
    // so the duplicate attaches before the first execution is released.
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
    // A flag rather than a notification, so a job that parks after the release still runs.
    let (release, gate) = tokio::sync::watch::channel(false);
    let executor: JobExecutor = {
        let executions = executions.clone();
        let gate = gate.clone();
        Arc::new(move |_resources, group, _endpoints, _control| {
            executions.fetch_add(execution_job_len(&group), Ordering::SeqCst);
            let mut gate = gate.clone();
            Box::pin(async move {
                gate.wait_for(|released| *released).await.unwrap();
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
    // Both reads must start while the first is still held. A fixed sleep raced the broker's
    // dispatch; merging them would keep the count at one and time out here.
    time::timeout(Duration::from_secs(5), async {
        while executions.load(Ordering::SeqCst) < 2 {
            time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("both latest reads execute without deduplication");
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    release.send_replace(true);
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    drop(broker);
}

#[tokio::test]
async fn eth_call_and_balance_reads_do_not_share_cache_or_dedup_identity() {
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
        Arc::new(move |resources, job, endpoints, control| {
            let resources = JobResources {
                http: semaphore.clone(),
                ..resources
            };
            let mut future =
                crate::rpc_broker::execution::run_job(resources, job, endpoints, control);
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
        test_clients(),
        rx,
        Duration::from_secs(30),
        1,
        executor,
        |_| {},
    );
    let actor_task = tokio::spawn(actor.run(block_rx));
    let route = RpcRoute::new(
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
        Arc::new(move |_resources, group, _endpoints, _control| {
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
        test_clients(),
        rx,
        Duration::from_secs(30),
        1,
        executor,
        |_| {},
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
