use crate::rpc_broker::actor::{Command, JobExecutor, JobOutput};
use crate::rpc_broker::broker::{RpcBroker, SUBMISSION_CAPACITY};
use crate::rpc_broker::model::{
    DEFAULT_MAX_ESTIMATED_GAS, RpcBrokerError, RpcBrokerSpawnError, RpcOrigin, RpcRead, RpcRoute,
    RpcSubmission,
};
use crate::rpc_broker::tests::{data_result, read_calldata, test_origin, test_route};
use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, Bytes};
use futures_util::poll;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::time;

async fn await_actor(broker: &RpcBroker) {
    let (reply, receiver) = oneshot::channel();
    let reply = crate::rpc_broker::resolution::ReadReply::new(
        reply,
        Arc::new(Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap()),
    );
    broker
        .tx
        .send(Command::Submit {
            submission: Box::new(RpcSubmission::new(
                test_route(),
                vec![RpcRead::eth_call(Address::ZERO, Bytes::new())],
                test_origin(),
            )),
            replies: vec![reply],
            deadline: Some(time::Instant::now()),
        })
        .await
        .expect("expired submission barrier");
    assert_eq!(
        receiver.await.expect("actor processed the barrier"),
        Err(RpcBrokerError::TimeoutBeforeDispatch)
    );
}

impl RpcBroker {
    pub(in crate::rpc_broker) fn spawn_with_test_config(
        client: reqwest::Client,
        interval: Duration,
        max_in_flight: usize,
        executor: JobExecutor,
    ) -> Result<Arc<Self>, RpcBrokerSpawnError> {
        Self::spawn_with_test_config_and_capacity(
            client,
            interval,
            max_in_flight,
            SUBMISSION_CAPACITY,
            executor,
        )
    }

    pub(in crate::rpc_broker) fn spawn_with_test_config_and_capacity(
        client: reqwest::Client,
        interval: Duration,
        max_in_flight: usize,
        submission_capacity: NonZeroUsize,
        executor: JobExecutor,
    ) -> Result<Arc<Self>, RpcBrokerSpawnError> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(RpcBrokerSpawnError::NoRuntime)?;
        Self::spawn_primitive(
            client,
            &handle,
            interval,
            max_in_flight,
            submission_capacity,
            executor,
        )
    }
}

#[tokio::test]
async fn oversized_submission_capacity_is_rejected_before_spawn() {
    let result = RpcBroker::spawn_with_test_config_and_capacity(
        reqwest::Client::new(),
        Duration::ZERO,
        1,
        NonZeroUsize::new(Semaphore::MAX_PERMITS + 1).unwrap(),
        crate::rpc_broker::broker::default_executor(),
    );
    assert!(matches!(
        result,
        Err(RpcBrokerSpawnError::InvalidSubmissionCapacity)
    ));
}

#[tokio::test]
async fn oversized_submission_is_rejected_before_command_send() {
    let (tx, mut commands) = mpsc::channel(1);
    let (block_tx, _block_rx) = mpsc::unbounded_channel();
    let broker = RpcBroker {
        tx,
        block_tx,
        admission: Arc::new(Semaphore::new(256)),
    };
    let reads = (0..65)
        .map(|_| RpcRead::get_balance(Address::ZERO))
        .collect();
    let origin = RpcOrigin::dapp("peer", "https://example.com").expect("dapp origin");
    let result = broker
        .submit(RpcSubmission::new(test_route(), reads, origin))
        .await;
    assert_eq!(result, Err(RpcBrokerError::AdmissionRejected));
    assert!(matches!(
        commands.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn admission_credit_covers_batch_replies_and_pre_enqueue_paths() {
    let (tx, mut commands) = mpsc::channel(4);
    let (block_tx, _block_rx) = mpsc::unbounded_channel();
    let admission = Arc::new(Semaphore::new(1));
    let broker = Arc::new(RpcBroker {
        tx,
        block_tx,
        admission: admission.clone(),
    });
    let origin = test_origin();
    let batch = |marker: &'static [u8], route: RpcRoute| {
        RpcSubmission::new(
            route,
            vec![
                RpcRead::eth_call(Address::ZERO, Bytes::from_static(marker)),
                RpcRead::eth_call(Address::ZERO, Bytes::from_static(marker)),
            ],
            origin.clone(),
        )
    };

    let first_submission = batch(b"first", test_route());
    let first = tokio::spawn({
        let broker = broker.clone();
        async move { broker.submit(first_submission).await }
    });
    let Command::Submit { replies, .. } = commands.recv().await.expect("first command");
    let mut replies = replies.into_iter();
    assert!(admission.try_acquire().is_err());

    let second_submission = batch(b"first", test_route());
    let second = tokio::spawn({
        let broker = broker.clone();
        async move { broker.submit(second_submission).await }
    });
    tokio::task::yield_now().await;
    assert!(matches!(
        commands.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    replies
        .next()
        .expect("first member reply")
        .send(Err(RpcBrokerError::TimeoutBeforeDispatch));
    assert!(admission.try_acquire().is_err());
    replies
        .next()
        .expect("last member reply")
        .send(Ok(data_result(Bytes::from_static(b"last"))));
    assert_eq!(
        first.await.expect("first task").expect("first result"),
        vec![
            Err(RpcBrokerError::TimeoutBeforeDispatch),
            Ok(data_result(Bytes::from_static(b"last"))),
        ]
    );

    let Command::Submit { replies, .. } = time::timeout(Duration::from_secs(1), commands.recv())
        .await
        .expect("second command should be released")
        .expect("second command");
    for reply in replies {
        reply.send(Ok(data_result(Bytes::from_static(b"second"))));
    }
    assert_eq!(
        second.await.expect("second task").expect("second result"),
        vec![Ok(data_result(Bytes::from_static(b"second"))); 2]
    );

    let held = admission
        .clone()
        .acquire_owned()
        .await
        .expect("hold credit");
    assert_eq!(
        broker
            .submit(RpcSubmission::new(test_route(), Vec::new(), origin.clone(),))
            .await,
        Ok(Vec::new())
    );
    let cancelled_submission = batch(
        b"cancelled",
        test_route().with_request_timeout(Duration::from_millis(20)),
    );
    let cancelled = tokio::spawn({
        let broker = broker.clone();
        async move { broker.submit(cancelled_submission).await }
    });
    assert_eq!(
        cancelled.await.expect("cancelled task"),
        Err(RpcBrokerError::Timeout)
    );
    assert!(matches!(
        commands.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    let mut waiting_route = test_route();
    waiting_route.request_timeout = None;
    let waiting_submission = batch(b"waiting", waiting_route);
    let waiting = broker.submit(waiting_submission);
    tokio::pin!(waiting);
    assert!(matches!(poll!(&mut waiting), std::task::Poll::Pending));
    drop(commands);
    assert_eq!(
        time::timeout(Duration::from_secs(1), &mut waiting)
            .await
            .expect("waiting task should wake"),
        Err(RpcBrokerError::Shutdown)
    );
    drop(held);

    let (failed_tx, failed_rx) = mpsc::channel(1);
    let (failed_block_tx, _failed_block_rx) = mpsc::unbounded_channel();
    let failed_admission = Arc::new(Semaphore::new(1));
    let failed_broker = Arc::new(RpcBroker {
        tx: failed_tx.clone(),
        block_tx: failed_block_tx,
        admission: failed_admission.clone(),
    });
    failed_tx
        .send(Command::Submit {
            submission: Box::new(batch(b"fill", test_route())),
            replies: Vec::new(),
            deadline: None,
        })
        .await
        .expect("fill failed-send channel");
    let failed_submission = batch(b"failed-send", test_route());
    let failed = failed_broker.submit(failed_submission);
    tokio::pin!(failed);
    assert!(matches!(poll!(&mut failed), std::task::Poll::Pending));
    assert_eq!(failed_admission.available_permits(), 0);
    drop(failed_rx);
    assert_eq!(
        time::timeout(Duration::from_secs(1), &mut failed)
            .await
            .expect("failed-send should wake"),
        Err(RpcBrokerError::Shutdown)
    );
    assert!(failed_admission.try_acquire().is_ok());
    assert_eq!(
        broker
            .submit(RpcSubmission::new(test_route(), Vec::new(), origin.clone(),))
            .await,
        Err(RpcBrokerError::Shutdown)
    );
}

#[tokio::test]
async fn duplicate_callers_require_distinct_admission_credits() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor: JobExecutor = {
        let started = started.clone();
        let release = release.clone();
        let executions = executions.clone();
        Arc::new(move |_client, _semaphore, group, _endpoints| {
            let started = started.clone();
            let release = release.clone();
            let first = executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            Box::pin(async move {
                started.notify_one();
                if first {
                    release.notified().await;
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
    let broker = RpcBroker::spawn_with_test_config_and_capacity(
        reqwest::Client::new(),
        Duration::ZERO,
        1,
        NonZeroUsize::new(2).unwrap(),
        executor,
    )
    .expect("test broker requires an active Tokio runtime");
    let mut route = test_route().with_test_thresholds(1, DEFAULT_MAX_ESTIMATED_GAS);
    route.request_timeout = None;
    let read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"duplicate"))
        .with_test_block(BlockNumberOrTag::Number(1));
    let origin = test_origin();
    let first = tokio::spawn({
        let broker = broker.clone();
        let route = route.clone();
        let read = read.clone();
        let origin = origin.clone();
        async move {
            broker
                .submit(RpcSubmission::new(route, vec![read], origin))
                .await
        }
    });
    started.notified().await;

    let second = broker.submit(RpcSubmission::new(
        route.clone(),
        vec![read.clone()],
        origin.clone(),
    ));
    tokio::pin!(second);
    assert!(matches!(poll!(&mut second), std::task::Poll::Pending));
    await_actor(&broker).await;
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(broker.admission.available_permits(), 0);
    first.abort();
    assert!(
        first
            .await
            .expect_err("aborted first caller")
            .is_cancelled()
    );
    assert_eq!(broker.admission.available_permits(), 0);

    let third_read = RpcRead::eth_call(Address::ZERO, Bytes::from_static(b"third"))
        .with_test_block(BlockNumberOrTag::Number(1));
    let third = broker.submit(RpcSubmission::new(
        route.clone(),
        vec![third_read],
        origin.clone(),
    ));
    tokio::pin!(third);
    assert!(matches!(poll!(&mut third), std::task::Poll::Pending));
    assert_eq!(broker.admission.available_permits(), 0);
    await_actor(&broker).await;

    release.notify_one();
    assert_eq!(
        second.await.expect("second result"),
        vec![Ok(data_result(Bytes::from_static(b"duplicate")))]
    );
    assert_eq!(
        third.await.expect("third result"),
        vec![Ok(data_result(Bytes::from_static(b"third")))]
    );
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(broker.admission.available_permits(), 2);

    let cached = broker
        .submit(RpcSubmission::new(route, vec![read], origin))
        .await
        .expect("cache hit");
    assert_eq!(
        cached,
        vec![Ok(data_result(Bytes::from_static(b"duplicate")))]
    );
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(broker.admission.available_permits(), 2);
}
