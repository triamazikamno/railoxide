use super::*;

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
        Arc::new(move |_resources, group, _endpoints, _control| {
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
    let route = RpcRoute::new(RpcChainRoute::new(
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
        Arc::new(move |_resources, group, _endpoints, _control| {
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
        RpcRoute::new(RpcChainRoute::new(
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
            let executor: JobExecutor = Arc::new(move |_resources, group, _endpoints, _control| {
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
