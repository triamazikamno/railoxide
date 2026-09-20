use super::*;
use crate::rpc_broker::actor::JobExecutor;
use crate::rpc_broker::model::{
    DEFAULT_ATTEMPT_TIMEOUT, RpcChainRoute, RpcSubmission, WalletRpcOrigin,
};
use crate::rpc_broker::resolution::{WaiterPolicy, WorkKey};
use crate::rpc_broker::tests::{
    RpcMockGate, RpcResponder, aggregate_response, data_result, read_calldata, remote_error,
    rpc_error, rpc_read_from_request, rpc_result, spawn_gated_rpc_mock, spawn_rpc_mock,
    spawn_status_rpc_mock, test_broker, test_broker_with_executor, test_origin, test_route,
};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, U256, hex};
use alloy::rpc::types::eth::transaction::{TransactionInput, TransactionRequest};
use alloy::sol_types::SolType;
use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

async fn execute_aggregate_with_failover(
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    route: RpcRoute,
    items: &[WorkItem],
) -> AggregateFailoverOutput {
    let endpoints = route.endpoints().to_vec();
    let output =
        execute_aggregate_with_failover_attempt(client, semaphore, route, endpoints, items).await;
    AggregateFailoverOutput {
        values: output
            .values
            .into_iter()
            .map(AttemptOutcome::into_result)
            .collect(),
        attempts: output.attempts,
    }
}

struct AggregateFailoverOutput {
    values: Vec<Result<RpcResult, RpcBrokerError>>,
    attempts: Vec<PhysicalAttempt>,
}

async fn execute_single_with_shared_waiters(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    read: &RpcRead,
    chain_id: u64,
    endpoints: &[SensitiveUrl],
    waiters: Arc<WaiterState>,
    fallback_attempt_timeout: Duration,
) -> (
    Result<RpcResult, RpcBrokerError>,
    Vec<(Url, EndpointHealthOutcome)>,
) {
    let (result, attempted) = execute_single_with_shared_waiters_attempt(
        client,
        semaphore,
        read,
        chain_id,
        endpoints,
        waiters,
        fallback_attempt_timeout,
    )
    .await;
    (result.into_result(), attempted)
}

async fn rpc_request(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    endpoint: &Url,
    method: &str,
    params: Value,
    preserve_revert: bool,
    deadline: Option<Instant>,
    attempt_timeout: Duration,
) -> Result<Value, RpcBrokerError> {
    rpc_request_attempt(
        client,
        semaphore,
        endpoint,
        method,
        params,
        preserve_revert,
        deadline,
        attempt_timeout,
    )
    .await
    .into_result()
}

async fn rpc_request_attempt(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    endpoint: &Url,
    method: &str,
    params: Value,
    preserve_revert: bool,
    deadline: Option<Instant>,
    attempt_timeout: Duration,
) -> AttemptOutcome<Value> {
    let body = wire_request(method, params);
    let value =
        execute_wire_request(client, semaphore, endpoint, body, deadline, attempt_timeout).await;
    value.and_then(|value| AttemptOutcome::from(parse_rpc_result(&value, preserve_revert)))
}

async fn execute_wire_request(
    client: &reqwest::Client,
    semaphore: &Arc<Semaphore>,
    endpoint: &Url,
    body: Request<Value>,
    deadline: Option<Instant>,
    attempt_timeout: Duration,
) -> AttemptOutcome<Value> {
    let permit = match acquire_permit(semaphore, deadline).await {
        PermitAcquisition::Acquired(permit) => permit,
        PermitAcquisition::DeadlineElapsed => return AttemptOutcome::NotDispatched,
        PermitAcquisition::Closed => return AttemptOutcome::CallerError(RpcBrokerError::Shutdown),
    };
    execute_wire_request_with_acquired_permit(
        client,
        endpoint,
        permit,
        body,
        deadline,
        None,
        attempt_timeout,
    )
    .await
}

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
            let route = RpcRoute::from(
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
            reqwest::Client::new(),
            Arc::new(Semaphore::new(0)),
            ExecutionJob::Aggregate(Vec::new()),
            Vec::new(),
        ),
    )
    .await
    .expect("empty aggregate should complete without acquiring a permit");

    assert!(output.completions.is_empty());
    assert!(output.requests.is_empty());
}

alloy::sol! {
    interface TypedHelperTest {
        function read() external view returns (uint256);
    }
}

#[tokio::test]
async fn typed_submit_helper_decodes_members_and_preserves_errors() {
    let executor: JobExecutor = Arc::new(|_client, _semaphore, group, _endpoints| {
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
            ],
            test_origin(),
        )
        .await
        .expect("outer submission should succeed");
    assert_eq!(results[0], Ok(U256::from(7)));
    assert_eq!(results[1], Err(RpcBrokerError::InvalidResponse));
    assert_eq!(results[2], Err(RpcBrokerError::Transport));
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
        RpcRoute::from(RpcChainRoute::new(1, vec![endpoint.clone()]).with_multicall(multicall)),
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
        RpcRoute::from(RpcChainRoute::new(1, vec![endpoint])),
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
            RpcRoute::from(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall)),
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
            RpcRoute::from(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall)),
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
    let route = RpcRoute::from(
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
        RpcRoute::from(RpcChainRoute::new(1, vec![endpoint.clone()]).with_multicall(multicall));
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
    let route = RpcRoute::from(
        RpcChainRoute::new(1, vec![first.clone(), second]).with_multicall(multicall),
    );
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
            deadline: Some(Instant::now() + Duration::from_secs(2)),
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
    first_gate.request_started.notified().await;
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
    tokio::time::timeout(Duration::from_secs(5), blocker_acquired.notified())
        .await
        .expect("blocker acquired");
    tokio::task::yield_now().await;
    let output = time::timeout(Duration::from_secs(3), request)
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
        RpcRoute::from(RpcChainRoute::new(1, vec![endpoint.clone()]).with_multicall(multicall));
    let reads = [
        RpcRead::eth_call(Address::from([1_u8; 20]), Bytes::from_static(b"first")),
        RpcRead::eth_call(Address::from([2_u8; 20]), Bytes::from_static(b"second")),
    ];
    let deadline = Instant::now() + Duration::from_secs(2);
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
    first_gate.request_started.notified().await;
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
    tokio::time::timeout(Duration::from_secs(5), blocker_acquired.notified())
        .await
        .expect("blocker acquired");
    tokio::task::yield_now().await;
    let output = time::timeout(Duration::from_secs(3), request)
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
        RpcRoute::from(RpcChainRoute::new(1, vec![endpoint.clone()]).with_multicall(multicall));
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
            RpcRoute::from(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall)),
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
    let route = RpcRoute::from(
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
    let route = RpcRoute::from(
        RpcChainRoute::new(1, vec![first.clone(), second]).with_multicall(multicall),
    );
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
    let route = RpcRoute::from(RpcChainRoute::new(1, vec![first.clone(), second]));
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
        reqwest::Client::new(),
        Arc::new(Semaphore::new(1)),
        ExecutionJob::Individual(Box::new(item)),
        endpoints,
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
            RpcRoute::from(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall)),
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
    let route = RpcRoute::from(RpcChainRoute::new(1, vec![endpoint.clone()]));
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
        reqwest::Client::new(),
        Arc::new(Semaphore::new(1)),
        ExecutionJob::Individual(Box::new(item)),
        endpoints,
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
    let route = RpcRoute::from(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall));
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
    let route = RpcRoute::from(
        RpcChainRoute::new(1, vec![first.clone(), second]).with_multicall(multicall),
    );
    let read = |index| {
        RpcRead::eth_call(
            Address::from([index; 20]),
            Bytes::from_static(b"\x70\xa0\x82\x31"),
        )
    };
    let early = Instant::now() + Duration::from_secs(1);
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
        Duration::from_secs(5),
        first_gate.request_started.notified(),
    )
    .await
    .expect("request started");

    late_waiter.add(WaiterPolicy {
        deadline: Some(Instant::now() + Duration::from_secs(100)),
        attempt_timeout: route.attempt_timeout,
    });
    late_waiter.add(WaiterPolicy {
        deadline: Some(Instant::now() + Duration::from_secs(1)),
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
    time::timeout(Duration::from_secs(5), blocker_acquired.notified())
        .await
        .expect("blocker acquired permit");
    time::pause();
    time::advance(Duration::from_secs(2)).await;
    time::resume();
    blocker_release.notify_one();
    time::timeout(
        Duration::from_secs(5),
        second_gate.request_started.notified(),
    )
    .await
    .expect("failover request started");
    second_gate.release_response.notify_one();

    let output = time::timeout(Duration::from_secs(5), request)
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
    let route = RpcRoute::from(RpcChainRoute::new(1, vec![endpoint]).with_multicall(multicall));
    let read = |index| {
        RpcRead::eth_call(
            Address::from([index; 20]),
            Bytes::from_static(b"\x70\xa0\x82\x31"),
        )
    };
    let early = Instant::now() + Duration::from_secs(1);
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
        Duration::from_secs(5),
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
    time::timeout(Duration::from_secs(5), blocker_acquired.notified())
        .await
        .expect("blocker acquired permit");
    time::pause();
    time::advance(Duration::from_secs(2)).await;
    time::resume();
    blocker_release.notify_one();
    time::timeout(
        Duration::from_secs(5),
        first_gate.request_started.notified(),
    )
    .await
    .expect("request started");
    first_gate.release_response.notify_one();

    let output = time::timeout(Duration::from_secs(5), request)
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
            RpcRoute::from(RpcChainRoute::new(1, vec![endpoint])),
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
            RpcRoute::from(RpcChainRoute::new(1, vec![first, second]).with_multicall(multicall)),
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
            RpcRoute::from(RpcChainRoute::new(1, vec![first, second]).with_multicall(multicall)),
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
async fn verified_routes_reuse_identity_and_withdraw_failed_identity_endpoints() {
    let rejected_checks = Arc::new(AtomicUsize::new(0));
    let rejected = rejected_checks.clone();
    let (bad, bad_server) = spawn_rpc_mock(
        Arc::new(move |request| {
            assert_eq!(request["method"], "eth_chainId");
            rejected.fetch_add(1, Ordering::SeqCst);
            rpc_result(&request, &json!("0x2"))
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
    let route = RpcRoute::from(RpcChainRoute::new(1, vec![bad, good]).with_identity_verification());
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
    assert_eq!(rejected_checks.load(Ordering::SeqCst), 3);
    bad_server.abort();
    good_server.abort();
}
