use super::*;
use crate::rpc_broker::execution::{MAX_RESPONSE_BYTES, parse_rpc_result, run_job};
use alloy::primitives::B256;

fn named(method: &str, params: Value) -> RpcRead {
    RpcRead::from_method_params(method, params, 1).expect("valid method read")
}

fn dapp() -> RpcOrigin {
    RpcOrigin::dapp("authenticated-peer", "https://dapp.example").unwrap()
}

#[test]
fn named_ingress_preserves_method_selectors_and_rejects_unsupported_forms() {
    let address = Address::from([1; 20]);
    let hash = B256::from([2; 32]);
    let state = json!({"blockHash": hash, "requireCanonical": true});
    let cases = [
        ("eth_getBalance", json!([address, state])),
        ("eth_blockNumber", json!([])),
        ("eth_getCode", json!([address, state])),
        ("eth_getStorageAt", json!([address, "0x1", state])),
        ("eth_getTransactionCount", json!([address, state])),
        ("eth_getBlockByHash", json!([hash, true])),
        ("eth_getBlockByNumber", json!(["pending", false])),
        ("eth_getBlockTransactionCountByHash", json!([hash])),
        ("eth_getBlockTransactionCountByNumber", json!(["safe"])),
        ("eth_getTransactionByHash", json!([hash])),
        ("eth_getTransactionReceipt", json!([hash])),
        (
            "eth_getTransactionByBlockHashAndIndex",
            json!([hash, "0x0"]),
        ),
        (
            "eth_getTransactionByBlockNumberAndIndex",
            json!(["0x1", "0x2"]),
        ),
        ("eth_getBlockReceipts", json!([hash])),
        ("eth_getBlockReceipts", json!(["finalized"])),
        (
            "eth_getLogs",
            json!([{"address": [address, address], "topics": [null, [hash, hash], hash], "fromBlock": "0x1", "toBlock": "latest"}]),
        ),
        ("eth_gasPrice", json!([])),
        ("eth_maxPriorityFeePerGas", json!([])),
        (
            "eth_feeHistory",
            json!(["0x2", "latest", [0, 50.5, 50.5, 100]]),
        ),
        ("eth_feeHistory", json!(["0x2", "latest"])),
        ("eth_estimateGas", json!([{"data": "0x0001"}])),
    ];
    for (method, params) in cases {
        let read = named(method, params.clone());
        let (actual_method, actual_params, _) = wire_request_for(&read);
        assert_eq!(actual_method, method);
        let expected = if method == "eth_getLogs" {
            json!([{"address": address, "topics": [null, hash, hash], "fromBlock": "0x1", "toBlock": "latest"}])
        } else {
            params.clone()
        };
        assert_eq!(actual_params, expected, "{method}");
        let mut extra = params.as_array().unwrap().clone();
        extra.extend([Value::Null, Value::Null, Value::Null]);
        assert!(RpcRead::from_method_params(method, json!(extra), 1).is_err());
    }
    for (method, input, expected) in [
        (
            "eth_getStorageAt",
            json!([address, "0x01", "0x00"]),
            json!([address, "0x1", "0x0"]),
        ),
        (
            "eth_getTransactionByBlockNumberAndIndex",
            json!(["latest", 1]),
            json!(["latest", "0x1"]),
        ),
        (
            "eth_feeHistory",
            json!([1, "latest", [101, 40]]),
            json!(["0x1", "latest", [101, 40]]),
        ),
    ] {
        assert_eq!(wire_request_for(&named(method, input)).1, expected);
    }
    let invalid = [
        ("eth_sendRawTransaction", json!(["0x01"])),
        ("eth_chainId", json!([])),
        (
            "eth_getCode",
            json!([address, {"blockHash": hash, "typo": true}]),
        ),
        (
            "eth_getCode",
            json!([address, {"blockNumber": "0x1", "requireCanonical": true}]),
        ),
        ("eth_getBlockByNumber", json!([state, true])),
        ("eth_getBlockReceipts", json!([state])),
        ("eth_getBlockTransactionCountByNumber", json!([hash])),
        (
            "eth_getLogs",
            json!([{"blockHash": hash, "fromBlock": "latest"}]),
        ),
        ("eth_getLogs", json!([{"topics": [[hash, "bad"]]}])),
        ("eth_getLogs", json!([{"unknown": true}])),
        ("eth_feeHistory", json!(["0x1", "latest", [null]])),
    ];
    for (method, params) in invalid {
        assert!(
            RpcRead::from_method_params(method, params, 1).is_err(),
            "{method}"
        );
    }
}

#[test]
fn estimation_reuses_call_validation_and_admission_checks_the_actual_route() {
    let transaction = json!({"from": Address::ZERO, "data": "0x0001", "input": "0x0001", "value": "0x1",
        "gas": "0x5208", "gasPrice": "0x2", "chainId": "0x1", "nonce": "0x0", "type": "0x0",
        "accessList": [{"address": Address::ZERO, "storageKeys": [B256::ZERO]}]});
    let read = named("eth_estimateGas", json!([transaction, "pending"]));
    assert_eq!(wire_request_for(&read).1, json!([transaction, "pending"]));
    for (field, value) in [("input", json!("0x02")), ("chainId", json!("0x2"))] {
        let mut invalid = transaction.clone();
        invalid[field] = value;
        assert!(RpcRead::from_method_params("eth_estimateGas", json!([invalid]), 1).is_err());
    }
    assert!(
        RpcRead::from_method_params("eth_estimateGas", json!([transaction, "latest", {}]), 1)
            .is_err()
    );
    let wrong_route = RpcChainRoute::new(2, Vec::<Url>::new()).into();
    assert_eq!(
        RpcSubmission::new(wrong_route, vec![read], dapp()).validate_admission(),
        Err(RpcBrokerError::InvalidRead(
            RpcReadValidationError::ChainIdMismatch
        ))
    );
}

fn authorization() -> Value {
    json!({"chainId": "0x1", "address": Address::ZERO, "nonce": "0x0", "yParity": "0x0", "r": "0x1", "s": "0x1"})
}

#[test]
fn method_variable_inputs_share_existing_byte_admission() {
    let log = |count| {
        named(
            "eth_getLogs",
            json!([{"address": vec![Address::ZERO; count]}]),
        )
    };
    assert!(
        RpcSubmission::new(test_route(), vec![log(6553)], dapp())
            .validate_admission()
            .is_ok()
    );
    assert_eq!(
        RpcSubmission::new(test_route(), vec![log(6554)], dapp()).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );
    let fee = |count| named("eth_feeHistory", json!(["0x1", "latest", vec![50; count]]));
    assert!(
        RpcSubmission::new(test_route(), vec![fee(16384)], dapp())
            .validate_admission()
            .is_ok()
    );
    assert_eq!(
        RpcSubmission::new(test_route(), vec![fee(16385)], dapp()).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );
    assert_eq!(
        RpcSubmission::new(test_route(), vec![fee(16384); 5], test_origin()).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );
    let topics = named("eth_getLogs", json!([{"topics": [vec![B256::ZERO; 4096]]}]));
    assert_eq!(
        RpcSubmission::new(test_route(), vec![topics], dapp()).validate_admission(),
        Err(RpcBrokerError::AdmissionRejected)
    );
    for method in ["eth_call", "eth_estimateGas"] {
        let extra_read = |length| {
            named(
                method,
                json!([{"data": "0x0001", "context": "x".repeat(length)}]),
            )
        };
        // Two calldata bytes plus the compact JSON object, including its key and quotes.
        let overhead = 2 + serde_json::to_vec(&json!({"context": ""})).unwrap().len();
        let exact = extra_read(128 * 1024 - overhead);
        assert!(
            RpcSubmission::new(test_route(), vec![exact.clone(); 4], dapp())
                .validate_admission()
                .is_ok()
        );
        assert_eq!(
            RpcSubmission::new(test_route(), vec![exact; 5], dapp()).validate_admission(),
            Err(RpcBrokerError::AdmissionRejected)
        );
        for length in [128 * 1024 - overhead + 1, 128 * 1024] {
            assert_eq!(
                RpcSubmission::new(test_route(), vec![extra_read(length)], dapp())
                    .validate_admission(),
                Err(RpcBrokerError::AdmissionRejected)
            );
        }
    }
    for transaction in [
        json!({"blobVersionedHashes": vec![B256::ZERO; 4097]}),
        json!({"authorizationList": vec![authorization(); 16384]}),
        json!({"blobs": [format!("0x{}", "00".repeat(131_072))], "commitments": [format!("0x{}", "00".repeat(48))], "proofs": [format!("0x{}", "00".repeat(48))]}),
    ] {
        for method in ["eth_call", "eth_estimateGas"] {
            let read = named(method, json!([transaction]));
            assert_eq!(
                RpcSubmission::new(test_route(), vec![read], dapp()).validate_admission(),
                Err(RpcBrokerError::AdmissionRejected)
            );
        }
    }
}

#[tokio::test]
async fn submission_preserves_mixed_results_and_rejects_wallet_hash_reads_before_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
    let mut block = serde_json::to_value(block).unwrap();
    block["extension"] = json!({"sensitive": ["secret-payload", 7]});
    let expected_block = block.clone();
    let expected_error = json!({
        "code": -32602,
        "message": "secret-message",
        "data": {"nested": ["secret-error", null]},
        "extension": {"detail": "secret-extension"}
    });
    let responder: RpcResponder = {
        let calls = calls.clone();
        let error = expected_error.clone();
        Arc::new(move |request| {
            calls.fetch_add(1, Ordering::SeqCst);
            if request["method"] == "eth_gasPrice" {
                return json!({"jsonrpc": "2.0", "id": request["id"], "error": error});
            }
            let value = match request["method"].as_str().unwrap() {
                "eth_getBlockByNumber" => block.clone(),
                "eth_getBalance" => json!("0x2"),
                "eth_call" => json!("0x0001"),
                "eth_getTransactionReceipt" => Value::Null,
                _ => panic!("unexpected method"),
            };
            rpc_result(&request, &value)
        })
    };
    let (url, server) = spawn_rpc_mock(responder, Arc::default(), Arc::default()).await;
    let route: RpcRoute = RpcChainRoute::new(1, vec![url]).into();
    let broker = test_broker(Duration::from_millis(1), 4);
    let reads = vec![
        named("eth_getBlockByNumber", json!(["latest", false])),
        RpcRead::get_balance(Address::ZERO),
        named("eth_gasPrice", json!([])),
        RpcRead::eth_call(Address::ZERO, Bytes::new()),
        named("eth_getTransactionReceipt", json!([B256::ZERO])),
    ];
    for method in ["eth_getTransactionReceipt", "eth_getTransactionByHash"] {
        assert_eq!(
            broker
                .submit(RpcSubmission::new(
                    route.clone(),
                    vec![
                        RpcRead::get_balance(Address::ZERO),
                        named(method, json!([B256::ZERO]))
                    ],
                    test_origin(),
                ))
                .await,
            Err(RpcBrokerError::OriginRejected)
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let values = broker
        .submit(RpcSubmission::new(route.clone(), reads, dapp()))
        .await
        .unwrap();
    assert_eq!(values.len(), 5);
    assert_eq!(values[0].as_ref().unwrap().expose_value(), &expected_block);
    assert_eq!(values[1].as_ref().unwrap().expose_value(), &json!("0x2"));
    let RpcBrokerError::Remote(remote) = values[2].as_ref().unwrap_err() else {
        panic!("structured member error at its input position")
    };
    assert_eq!(
        serde_json::to_value(remote.expose_payload()).unwrap(),
        expected_error
    );
    assert_eq!(values[3].as_ref().unwrap().expose_value(), &json!("0x0001"));
    assert_eq!(values[4].as_ref().unwrap().expose_value(), &Value::Null);
    assert!(!format!("{values:?}").contains("secret-"));
    assert!(!format!("{}", values[0].as_ref().unwrap()).contains("secret-"));
    assert!(!format!("{remote}").contains("secret-"));
    assert_eq!(values[0].clone().unwrap().into_value(), expected_block);
    server.abort();
}

#[tokio::test]
async fn repeated_nullable_logs_estimates_and_special_calls_execute_without_reuse() {
    let extras = json!({"nested": ["0x0001", null, {"enabled": true}]});
    let calldata = Bytes::from([b"\x70\xa0\x82\x31".as_slice(), &[0_u8; 32]].concat());
    let calls = Arc::new(AtomicUsize::new(0));
    let responder: RpcResponder = {
        let calls = calls.clone();
        let calldata = calldata.clone();
        let extras = extras.clone();
        Arc::new(move |request| {
            calls.fetch_add(1, Ordering::SeqCst);
            rpc_result(
                &request,
                &match request["method"].as_str().unwrap() {
                    "eth_getTransactionByHash" | "eth_getTransactionReceipt" => Value::Null,
                    "eth_getLogs" => json!([]),
                    "eth_estimateGas" => {
                        assert_eq!(request["params"][0]["context"], extras);
                        assert_eq!(request["params"][1], json!("pending"));
                        json!("0x5208")
                    }
                    "eth_call" => {
                        assert_eq!(request["params"][0]["to"], json!(Address::from([1; 20])));
                        let transaction = &request["params"][0];
                        assert_eq!(transaction["data"], json!(calldata));
                        if transaction.get("blobVersionedHashes").is_some() {
                            assert_eq!(transaction["blobVersionedHashes"], json!([B256::ZERO]));
                            assert_eq!(transaction["maxFeePerBlobGas"], json!("0x1"));
                        } else if transaction.get("authorizationList").is_some() {
                            assert_eq!(transaction["authorizationList"], json!([authorization()]));
                        } else {
                            assert_eq!(transaction["context"], extras);
                        }
                        json!("0x0001")
                    }
                    _ => panic!("unexpected method"),
                },
            )
        })
    };
    let (url, server) = spawn_rpc_mock(responder, Arc::default(), Arc::default()).await;
    let route: RpcRoute = RpcChainRoute::new(1, vec![url])
        .with_multicall(Address::ZERO)
        .into();
    let broker = test_broker(Duration::from_millis(1), 4);
    broker.notify_block(1, 100);
    let reads = vec![
        named("eth_getTransactionByHash", json!([B256::ZERO])),
        named("eth_getTransactionReceipt", json!([B256::ZERO])),
        named(
            "eth_getLogs",
            json!([{"fromBlock": "0x1", "toBlock": "0x1"}]),
        ),
        named(
            "eth_estimateGas",
            json!([{"data": "0x00", "context": extras}, "pending"]),
        ),
        named(
            "eth_call",
            json!([{"to": Address::from([1; 20]), "data": calldata, "blobVersionedHashes": [B256::ZERO], "maxFeePerBlobGas": "0x1"}]),
        ),
        named(
            "eth_call",
            json!([{"to": Address::from([1; 20]), "data": calldata, "authorizationList": [authorization()]}]),
        ),
        named(
            "eth_call",
            json!([{"to": Address::from([1; 20]), "data": calldata, "context": extras}]),
        ),
    ];
    for _ in 0..2 {
        let submission = RpcSubmission::new(route.clone(), reads.clone(), dapp());
        let (first, second) =
            tokio::join!(broker.submit(submission.clone()), broker.submit(submission));
        assert!(first.unwrap().iter().all(Result::is_ok));
        assert!(second.unwrap().iter().all(Result::is_ok));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 28);
    server.abort();
}

#[test]
fn nullable_shapes_remote_errors_and_extension_fields_preserve_response_semantics() {
    let validate = |method, params, value| {
        let read = named(method, params);
        let operation = read.operation();
        operation.validate_result(value)
    };
    assert_eq!(
        parse_rpc_result(&json!({"id": 1}), false),
        Err(RpcBrokerError::InvalidResponse)
    );
    assert_eq!(
        parse_rpc_result(&json!({"id": 1, "result": null}), false),
        Ok(Value::Null)
    );
    for (method, params, value) in [
        ("eth_getLogs", json!([{}]), Value::Null),
        ("eth_blockNumber", json!([]), json!("invalid")),
        (
            "eth_getStorageAt",
            json!([Address::ZERO, "0x0", "latest"]),
            json!("0x01"),
        ),
        ("eth_getTransactionReceipt", json!([B256::ZERO]), json!({})),
        ("eth_getBlockReceipts", json!(["latest"]), json!([{}])),
    ] {
        assert_eq!(
            validate(method, params, value),
            Err(RpcBrokerError::InvalidResponse)
        );
    }
    let log = json!({
        "address": Address::ZERO,
        "topics": [B256::ZERO],
        "data": "0x0001",
        "blockHash": B256::from([1; 32]),
        "blockNumber": "0x1",
        "transactionHash": B256::from([2; 32]),
        "transactionIndex": "0x0",
        "logIndex": "0x0",
        "extra": {"secret": "nested-result"}
    });
    assert_eq!(
        validate("eth_getLogs", json!([{}]), json!([log]))
            .unwrap()
            .into_value(),
        json!([log])
    );
    let fee = json!({"oldestBlock": "0x1", "baseFeePerGas": ["0x1", "0x2"], "gasUsedRatio": [0.5], "extension": [null, "secret"]});
    assert_eq!(
        validate("eth_feeHistory", json!(["0x1", "latest"]), fee.clone())
            .unwrap()
            .into_value(),
        fee
    );
    let raw = json!({"code": -32005, "message": "sensitive message", "data": {"secret": "private"}, "extra": "retained"});
    let error = parse_rpc_result(&json!({"id": 1, "error": raw}), false).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("private"));
    let RpcBrokerError::Remote(remote) = error else {
        panic!("structured remote error")
    };
    assert_eq!(remote.code(), -32005);
    assert_eq!(remote.expose_data(), Some(&raw["data"]));
}

async fn chunked_endpoint(body: Vec<u8>, delay: Duration) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).await;
                if stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.is_err() { return; }
                time::sleep(delay).await;
                for chunk in body.chunks(65536) {
                    let header = format!("{:x}\r\n", chunk.len());
                    if stream.write_all(header.as_bytes()).await.is_err()
                        || stream.write_all(chunk).await.is_err()
                        || stream.write_all(b"\r\n").await.is_err()
                    {
                        return;
                    }
                }
                let _ = stream.write_all(b"0\r\n\r\n").await;
            });
        }
    });
    (url, task)
}

async fn individual_job(read: RpcRead, endpoints: Vec<Url>, timeout: Duration) -> JobOutput {
    let route = RpcRoute::from(RpcChainRoute::new(1, endpoints));
    let key = WorkKey {
        identity: read.identity_for_route(&route),
        route: route.chain_route().clone(),
        nonce: 1,
        latest_epoch: None,
    };
    let waiters = WaiterState::new(WaiterPolicy {
        deadline: Some(Instant::now() + timeout),
        attempt_timeout: timeout,
    });
    let endpoints = route.endpoints().to_vec();
    let item = WorkItem {
        key,
        execution_route: route,
        read,
        origins: vec![dapp()],
        waiters,
    };
    run_job(
        reqwest::Client::new(),
        Arc::new(Semaphore::new(1)),
        ExecutionJob::Individual(Box::new(item)),
        endpoints,
    )
    .await
}

#[tokio::test]
async fn streamed_body_boundary_is_terminal_neutral_and_deadline_bounded() {
    // Whitespace counts toward the decoded body limit, even when the result is small.
    let mut body = br#"{"jsonrpc":"2.0","id":1,"result":"0x10"}"#.to_vec();
    body.resize(MAX_RESPONSE_BYTES, b' ');
    let (exact, exact_server) = chunked_endpoint(body.clone(), Duration::ZERO).await;
    body.push(b' ');
    let (oversized, oversized_server) = chunked_endpoint(body, Duration::ZERO).await;
    let (slow, slow_server) = chunked_endpoint(
        br#"{"id":1,"result":"0x10"}"#.to_vec(),
        Duration::from_millis(200),
    )
    .await;
    for read in [
        named("eth_blockNumber", json!([])),
        RpcRead::get_balance(Address::ZERO),
        RpcRead::eth_call(Address::ZERO, Bytes::new()),
    ] {
        let exact_output =
            individual_job(read.clone(), vec![exact.clone()], Duration::from_secs(5)).await;
        assert_eq!(
            exact_output.completions[0].1,
            Ok(RpcResult::new(json!("0x10")))
        );
        let output = individual_job(
            read.clone(),
            vec![oversized.clone(), exact.clone()],
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            output.completions[0].1,
            Err(RpcBrokerError::ResponseTooLarge)
        );
        assert_eq!(output.requests.len(), 1);
        assert_eq!(output.requests[0].endpoint, oversized);
        assert_eq!(
            output.requests[0].health_outcome,
            EndpointHealthOutcome::Neutral
        );
        let slow_output = individual_job(read, vec![slow.clone()], Duration::from_millis(50)).await;
        assert_eq!(slow_output.completions[0].1, Err(RpcBrokerError::Timeout));
        assert_eq!(
            slow_output.requests[0].health_outcome,
            EndpointHealthOutcome::Strike
        );
    }
    exact_server.abort();
    oversized_server.abort();
    slow_server.abort();
}

#[tokio::test]
async fn aggregate_body_cap_is_terminal_without_partial_results_or_reduction() {
    let response = aggregate_response(
        &json!({"id": 1}),
        vec![
            (
                true,
                Bytes::copy_from_slice(&U256::from(42).to_be_bytes::<32>()),
            ),
            (true, Bytes::from_static(&[0, 1])),
            (false, Bytes::from_static(&[0xde, 0xad])),
        ],
    );
    let mut body = serde_json::to_vec(&response).unwrap();
    body.resize(MAX_RESPONSE_BYTES, b' ');
    let (exact, exact_server) = chunked_endpoint(body.clone(), Duration::ZERO).await;
    body.push(b' ');
    let (oversized, oversized_server) = chunked_endpoint(body, Duration::ZERO).await;
    for oversized_first in [false, true] {
        let urls = if oversized_first {
            vec![oversized.clone(), exact.clone()]
        } else {
            vec![exact.clone()]
        };
        let route = RpcRoute::from(RpcChainRoute::new(1, urls).with_multicall(Address::ZERO));
        let reads = [
            RpcRead::get_balance(Address::ZERO),
            RpcRead::eth_call(Address::from([1; 20]), Bytes::new()),
            RpcRead::eth_call(Address::from([2; 20]), Bytes::new()),
        ];
        let items = reads
            .into_iter()
            .map(|read| WorkItem {
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
                    deadline: Some(Instant::now() + Duration::from_secs(5)),
                    attempt_timeout: Duration::from_secs(5),
                }),
            })
            .collect();
        let output = run_job(
            reqwest::Client::new(),
            Arc::new(Semaphore::new(1)),
            ExecutionJob::Aggregate(items),
            route.endpoints().to_vec(),
        )
        .await;
        assert_eq!(output.completions.len(), 3);
        assert_eq!(output.requests.len(), 1);
        if oversized_first {
            assert!(
                output
                    .completions
                    .iter()
                    .all(|(_, result)| *result == Err(RpcBrokerError::ResponseTooLarge))
            );
            assert_eq!(output.requests[0].endpoint, oversized);
            assert_eq!(
                output.requests[0].health_outcome,
                EndpointHealthOutcome::Neutral
            );
        } else {
            assert_eq!(output.completions[0].1, Ok(RpcResult::new(json!("0x2a"))));
            assert_eq!(output.completions[1].1, Ok(RpcResult::new(json!("0x0001"))));
            assert!(
                matches!(&output.completions[2].1, Err(RpcBrokerError::InnerRevert(revert)) if revert.expose_bytes() == &Bytes::from_static(&[0xde, 0xad]))
            );
            assert_eq!(
                output.requests[0].health_outcome,
                EndpointHealthOutcome::Healthy
            );
        }
    }
    exact_server.abort();
    oversized_server.abort();
}

#[tokio::test]
async fn malformed_and_remote_failures_keep_existing_endpoint_failover() {
    let (invalid, invalid_server) = spawn_rpc_mock(
        Arc::new(|request| rpc_result(&request, &json!({}))),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let (remote, remote_server) = spawn_rpc_mock(
        Arc::new(|request| rpc_error(&request, -32005)),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let (absent, absent_server) = spawn_rpc_mock(
        Arc::new(|request| rpc_result(&request, &Value::Null)),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let hunted_calls = Arc::new(AtomicUsize::new(0));
    let (hunted, hunted_server) = spawn_rpc_mock(
        {
            let calls = hunted_calls.clone();
            Arc::new(move |request| {
                calls.fetch_add(1, Ordering::SeqCst);
                rpc_result(&request, &Value::Null)
            })
        },
        Arc::default(),
        Arc::default(),
    )
    .await;
    let output = individual_job(
        named("eth_getTransactionReceipt", json!([B256::ZERO])),
        vec![invalid, remote, absent.clone(), hunted],
        Duration::from_secs(5),
    )
    .await;
    let value = output.completions[0].1.clone().unwrap();
    assert!(value.expose_value().is_null());
    assert_eq!(output.requests.len(), 3);
    assert_eq!(hunted_calls.load(Ordering::SeqCst), 0);
    assert_eq!(output.requests[2].endpoint, absent);
    assert_eq!(
        output.requests[2].health_outcome,
        EndpointHealthOutcome::Healthy
    );
    invalid_server.abort();
    remote_server.abort();
    absent_server.abort();
    hunted_server.abort();
}

fn custom_transaction() -> Value {
    json!({
        "type": "0x6a", "hash": B256::ZERO, "from": Address::ZERO,
        "to": null, "nonce": "0x0", "gas": "0x5208", "value": "0x0",
        "input": "0x0001", "chainId": "0xa4b1", "gasPrice": "0x1",
        "extension": {"chainSpecific": [null, "preserved"]}
    })
}

fn block_with_transactions(transactions: Value) -> Value {
    let block: alloy::rpc::types::Block = alloy::rpc::types::Block::default();
    let mut block = serde_json::to_value(block).unwrap();
    block["transactions"] = transactions;
    block
}

fn validate_method(read: &RpcRead, value: Value) -> Result<RpcResult, RpcBrokerError> {
    let operation = read.operation();
    operation.validate_result(value)
}

async fn assert_first_endpoint_preserves(read: RpcRead, value: Value) {
    let response = value.clone();
    let (first, first_server) = spawn_rpc_mock(
        Arc::new(move |request| rpc_result(&request, &response)),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let calls = fallback_calls.clone();
    let (fallback, fallback_server) = spawn_rpc_mock(
        Arc::new(move |request| {
            calls.fetch_add(1, Ordering::SeqCst);
            rpc_result(&request, &Value::Null)
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let output = individual_job(read, vec![first.clone(), fallback], Duration::from_secs(5)).await;
    let actual = output.completions[0].1.clone().unwrap();
    assert_eq!(actual.into_value(), value);
    assert_eq!(output.requests.len(), 1);
    assert_eq!(output.requests[0].endpoint, first);
    assert_eq!(
        output.requests[0].health_outcome,
        EndpointHealthOutcome::Healthy
    );
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    first_server.abort();
    fallback_server.abort();
}

#[tokio::test]
async fn alloy_envelopes_preserve_known_and_custom_transaction_results() {
    let transaction = custom_transaction();
    let receipt = json!({
        "type": "0x6a", "transactionHash": B256::ZERO, "from": Address::ZERO,
        "to": null, "gasUsed": "0x5208", "cumulativeGasUsed": "0x5208",
        "status": "0x1", "logs": [], "logsBloom": alloy::primitives::Bloom::ZERO,
        "extension": {"chainSpecific": [null, "preserved"]}
    });
    let tx_read = named("eth_getTransactionByHash", json!([B256::ZERO]));
    let block_read = named("eth_getBlockByNumber", json!(["latest", true]));
    let receipt_read = named("eth_getTransactionReceipt", json!([B256::ZERO]));
    let receipts_read = named("eth_getBlockReceipts", json!(["latest"]));
    for (read, value) in [
        (tx_read.clone(), transaction.clone()),
        (
            block_read.clone(),
            block_with_transactions(json!([transaction])),
        ),
        (receipt_read.clone(), receipt.clone()),
        (receipts_read.clone(), json!([receipt])),
    ] {
        assert_first_endpoint_preserves(read, value).await;
    }
    let mut legacy_receipt = receipt.clone();
    legacy_receipt.as_object_mut().unwrap().remove("type");
    assert_eq!(
        validate_method(&receipt_read, legacy_receipt.clone())
            .unwrap()
            .into_value(),
        legacy_receipt
    );
    let mut incomplete_known = transaction.clone();
    incomplete_known["type"] = json!("0x2");
    incomplete_known["nonce"] = json!("chain-specific-value");
    assert_first_endpoint_preserves(tx_read.clone(), incomplete_known.clone()).await;
    assert_first_endpoint_preserves(
        block_read.clone(),
        block_with_transactions(json!([incomplete_known])),
    )
    .await;
    let mut invalid = transaction.clone();
    invalid["hash"] = json!("bad");
    assert_eq!(
        validate_method(&tx_read, invalid.clone()),
        Err(RpcBrokerError::InvalidResponse)
    );
    assert_eq!(
        validate_method(&block_read, block_with_transactions(json!([invalid]))),
        Err(RpcBrokerError::InvalidResponse)
    );
    let mut invalid_receipt = receipt.clone();
    invalid_receipt["gasUsed"] = json!("bad");
    assert_eq!(
        validate_method(&receipt_read, invalid_receipt.clone()),
        Err(RpcBrokerError::InvalidResponse)
    );
    assert_eq!(
        validate_method(&receipts_read, json!([receipt, invalid_receipt])),
        Err(RpcBrokerError::InvalidResponse)
    );
}

#[tokio::test]
async fn pending_block_null_header_fields_preserve_raw_values_only_for_pending_selectors() {
    for full in [false, true] {
        let read = named("eth_getBlockByNumber", json!(["pending", full]));
        let mut block = block_with_transactions(if full {
            json!([custom_transaction()])
        } else {
            json!([B256::ZERO])
        });
        for field in ["hash", "miner", "nonce"] {
            block[field] = Value::Null;
        }
        assert_first_endpoint_preserves(read.clone(), block.clone()).await;
        let mut malformed = block.clone();
        malformed["parentHash"] = json!("bad");
        assert_eq!(
            validate_method(&read, malformed),
            Err(RpcBrokerError::InvalidResponse)
        );
        for field in ["hash", "miner", "nonce"] {
            let mut malformed = block.clone();
            malformed[field] = json!("bad");
            assert_eq!(
                validate_method(&read, malformed),
                Err(RpcBrokerError::InvalidResponse)
            );
        }
        let mut missing = block.clone();
        missing.as_object_mut().unwrap().remove("hash");
        assert_eq!(
            validate_method(&read, missing),
            Err(RpcBrokerError::InvalidResponse)
        );
        for mined in [
            named("eth_getBlockByNumber", json!(["latest", full])),
            named("eth_getBlockByHash", json!([B256::ZERO, full])),
        ] {
            assert_eq!(
                validate_method(&mined, block.clone()),
                Err(RpcBrokerError::InvalidResponse)
            );
        }
    }
}

#[tokio::test]
async fn defaulted_fee_history_is_healthy_and_malformed_fields_stay_validated() {
    for count in ["0x0", "0x1"] {
        let read = named("eth_feeHistory", json!([count, "latest"]));
        let empty =
            json!({"oldestBlock": "0x0", "gasUsedRatio": null, "extension": [null, "preserved"]});
        assert_first_endpoint_preserves(read.clone(), empty).await;
        for accepted in [
            json!({"gasUsedRatio": null}),
            json!({"oldestBlock": "0x0", "gasUsedRatio": [0.5]}),
        ] {
            assert_first_endpoint_preserves(read.clone(), accepted).await;
        }
        for invalid in [
            json!({}),
            json!({"oldestBlock": "0x0", "gasUsedRatio": "bad"}),
            json!({"oldestBlock": "0x0", "gasUsedRatio": ["bad"]}),
            json!({"oldestBlock": "0x0", "gasUsedRatio": null, "baseFeePerGas": ["bad"]}),
        ] {
            assert_eq!(
                validate_method(&read, invalid),
                Err(RpcBrokerError::InvalidResponse)
            );
        }
    }
}
