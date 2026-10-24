use super::*;
use crate::http::{OperationHttpClient, OperationNetworkIsolation};

const MULTICALL: Address = Address::repeat_byte(0x33);

/// An operation client whose requests carry `x-route: <tag>`.
fn tagged_client(tag: &'static str) -> OperationHttpClient {
    OperationHttpClient::for_tests(
        tagged_reqwest_client(tag),
        OperationNetworkIsolation::Dedicated,
    )
}

/// Answers an aggregate with each call's calldata and an individual call with its input.
fn echo_responder() -> RpcResponder {
    Arc::new(|request| {
        let input = request["params"][0]["input"].as_str().unwrap().to_owned();
        match aggregated_calls(&request) {
            Some(calls) => aggregate_response(
                &request,
                calls
                    .into_iter()
                    .map(|call| (true, call.callData))
                    .collect(),
            ),
            None => rpc_result(&request, &json!(input)),
        }
    })
}

fn aggregated_calls(request: &Value) -> Option<Vec<IMulticall3::Call>> {
    let input = hex::decode(request["params"][0]["input"].as_str()?).ok()?;
    IMulticall3::tryAggregateCall::abi_decode(&input)
        .ok()
        .map(|decoded| decoded.calls)
}

fn read_at(target: u8, block: u64) -> RpcRead {
    RpcRead::eth_call(Address::repeat_byte(target), Bytes::from(vec![target]))
        .with_test_block(BlockNumberOrTag::Number(block))
}

#[tokio::test]
async fn isolated_routes_aggregate_apart_over_their_own_clients() {
    let (endpoint, recorded, server) = spawn_recording_rpc_mock(echo_responder()).await;
    let chain = RpcChainRoute::new(1, vec![endpoint]).with_multicall(MULTICALL);
    let first = RpcRoute::from(chain.isolated(tagged_client("first")));
    let second = RpcRoute::from(chain.isolated(tagged_client("second")));
    let broker = test_broker(Duration::from_millis(20), 4);
    let submit = |route: &RpcRoute| {
        broker.submit(RpcSubmission::new(
            route.clone(),
            vec![read_at(1, 1), read_at(2, 1)],
            test_origin(),
        ))
    };

    let (first_results, second_results) = tokio::join!(submit(&first), submit(&second));
    for results in [first_results.unwrap(), second_results.unwrap()] {
        assert_eq!(
            results,
            vec![
                Ok(data_result(Bytes::from(vec![1]))),
                Ok(data_result(Bytes::from(vec![2])))
            ]
        );
    }
    // One multicall per route, each sent by that route's client.
    let mut requests = recorded
        .lock()
        .unwrap()
        .iter()
        .map(|(tag, body)| (tag.clone(), aggregated_calls(body).map(|calls| calls.len())))
        .collect::<Vec<_>>();
    requests.sort();
    assert_eq!(
        requests,
        vec![
            (Some("first".to_owned()), Some(2)),
            (Some("second".to_owned()), Some(2))
        ]
    );
    drop(broker);
    server.abort();
}

#[tokio::test]
async fn isolated_and_shared_reads_share_neither_requests_nor_cached_results() {
    let (endpoint, recorded, server) = spawn_recording_rpc_mock(echo_responder()).await;
    let shared = RpcRoute::from(RpcChainRoute::new(1, vec![endpoint]));
    let isolated = RpcRoute::from(shared.chain_route().isolated(tagged_client("isolated")));
    let broker = test_broker(Duration::from_millis(20), 4);
    let submit = |route: &RpcRoute, block| {
        broker.submit(RpcSubmission::new(
            route.clone(),
            vec![read_at(1, block)],
            test_origin(),
        ))
    };
    let tags = || {
        recorded
            .lock()
            .unwrap()
            .iter()
            .map(|(tag, _)| tag.clone())
            .collect::<Vec<_>>()
    };
    let isolated_tag = Some("isolated".to_owned());

    // Submitted in one window, the identical reads are not deduplicated.
    let (isolated_results, shared_results) = tokio::join!(submit(&isolated, 1), submit(&shared, 1));
    assert!(isolated_results.unwrap()[0].is_ok());
    assert!(shared_results.unwrap()[0].is_ok());
    let mut together = tags();
    together.sort();
    assert_eq!(together, vec![None, isolated_tag.clone()]);
    // The shared result cached at block 1 is not served to the isolated route.
    assert!(submit(&isolated, 1).await.unwrap()[0].is_ok());
    // The isolated result at block 2 is not cached for the shared route.
    assert!(submit(&isolated, 2).await.unwrap()[0].is_ok());
    assert!(submit(&shared, 2).await.unwrap()[0].is_ok());
    // The shared route's own result is cached.
    assert!(submit(&shared, 2).await.unwrap()[0].is_ok());
    assert_eq!(tags()[2..], [isolated_tag.clone(), isolated_tag, None]);
    drop(broker);
    server.abort();
}

#[tokio::test]
async fn failover_sends_each_endpoint_over_its_own_transport() {
    let failing: RpcResponder = Arc::new(|request| rpc_error(&request, -32000));
    let (direct_down, direct_down_requests, direct_down_server) =
        spawn_recording_rpc_mock(failing.clone()).await;
    let (wallet_up, wallet_up_requests, wallet_up_server) =
        spawn_recording_rpc_mock(echo_responder()).await;
    let (other_down, other_down_requests, other_down_server) =
        spawn_recording_rpc_mock(failing).await;
    let (direct_up, direct_up_requests, direct_up_server) =
        spawn_recording_rpc_mock(echo_responder()).await;
    let broker = RpcBroker::spawn_with_candidate_order(
        tagged_reqwest_client("wallet"),
        tagged_reqwest_client("direct"),
        |_| {},
    )
    .expect("test broker requires an active Tokio runtime");
    let route = |endpoints: [(Url, RpcTransport); 2]| {
        RpcChainRoute::new_with_transports(
            1,
            endpoints
                .into_iter()
                .map(|(url, transport)| (url.into(), transport))
                .collect(),
        )
    };
    // On an isolated route only the wallet endpoint uses the isolated client.
    let mixed = route([
        (direct_down, RpcTransport::Direct),
        (wallet_up, RpcTransport::Wallet),
    ])
    .isolated(tagged_client("isolated"));
    let both_direct = route([
        (other_down, RpcTransport::Direct),
        (direct_up, RpcTransport::Direct),
    ]);

    for chain in [mixed, both_direct] {
        let results = broker
            .submit(RpcSubmission::new(
                RpcRoute::from(chain),
                vec![read_at(1, 1)],
                test_origin(),
            ))
            .await
            .unwrap();
        assert_eq!(results, vec![Ok(data_result(Bytes::from(vec![1])))]);
    }
    let tags = |requests: &RecordedRequests| {
        requests
            .lock()
            .unwrap()
            .iter()
            .map(|(tag, _)| tag.clone())
            .collect::<Vec<_>>()
    };
    let direct = vec![Some("direct".to_owned())];
    assert_eq!(tags(&direct_down_requests), direct);
    assert_eq!(tags(&wallet_up_requests), vec![Some("isolated".to_owned())]);
    assert_eq!(tags(&other_down_requests), direct);
    assert_eq!(tags(&direct_up_requests), direct);
    drop(broker);
    for server in [
        direct_down_server,
        wallet_up_server,
        other_down_server,
        direct_up_server,
    ] {
        server.abort();
    }
}

#[tokio::test]
async fn completed_isolated_reads_leave_no_route_clone_in_the_broker() {
    let (endpoint, _recorded, server) = spawn_recording_rpc_mock(echo_responder()).await;
    let route = RpcRoute::from(
        RpcChainRoute::new(1, vec![endpoint])
            .with_multicall(MULTICALL)
            .isolated(tagged_client("isolated")),
    );
    let probe = route
        .chain_route()
        .isolation_probe()
        .expect("isolated route");
    let broker = test_broker(Duration::from_millis(1), 1);

    let results = broker
        .submit(RpcSubmission::new(
            route,
            vec![read_at(1, 1), read_at(2, 1)],
            test_origin(),
        ))
        .await
        .unwrap();
    assert!(results.iter().all(Result::is_ok));
    assert!(probe.upgrade().is_none());
    drop(broker);
    server.abort();
}
