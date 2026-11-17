use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use super::*;
use crate::rpc_broker::tests::{
    MAX_IDENTITY_PROBES, RpcMockGate, general, spawn_rpc_mock_with_gate,
};
use serde_json::{Value, json};

#[tokio::test(start_paused = true)]
async fn receipt_polling_survives_absent_or_closed_self_broadcast_commands() {
    let (closed_tx, closed_rx) = mpsc::unbounded_channel();
    drop(closed_tx);
    for mut command_rx in [None, Some(closed_rx)] {
        let started = tokio::time::Instant::now();
        assert!(
            wait_for_self_broadcast_poll_or_command(&mut command_rx)
                .await
                .is_none()
        );
        assert_eq!(started.elapsed(), Duration::from_secs(3));
    }

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let mut command_rx = Some(command_rx);
    let replacement = SelfBroadcastCommand {
        kind: SelfBroadcastCommandKind::Replacement,
        gas_fee: SelfBroadcastGasFeeSelection::Auto,
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        command_tx.send(replacement).expect("queue replacement");
    });
    let started = tokio::time::Instant::now();
    assert_eq!(
        wait_for_self_broadcast_poll_or_command(&mut command_rx).await,
        Some(replacement),
    );
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    assert!(
        wait_for_self_broadcast_poll_or_command(&mut command_rx)
            .await
            .is_none()
    );
    assert_eq!(started.elapsed(), Duration::from_secs(4));
}

#[test]
fn self_broadcast_observation_preserves_unknown_hash_and_canonical_winner() {
    let attempts = [1_u64, 2].map(|id| SubmittedSelfBroadcastAttempt {
        info: SelfBroadcastAttemptInfo {
            tx_hash: format!("0x{id:064x}"),
            nonce: 7,
            gas_limit: 21_000 * id,
            max_fee_per_gas: u128::from(id) * 10,
            max_priority_fee_per_gas: u128::from(id),
        },
        rpc_gas_price: u128::from(id) * 5,
        estimated_native_gas_cost: U256::from(id * 210_000),
        live_native_balance: U256::from(1_000_000),
    });
    let pending = crate::block_observer::BlockObservation {
        receipt: None,
        head: Some((0, 10)),
    };
    assert!(
        self_broadcast_observation_outcome(Ok(pending), &attempts)
            .expect("pending observation")
            .is_none()
    );

    let (index, unknown) = self_broadcast_observation_outcome(
        Err(eyre!("privacy-preserving block observation is unavailable")),
        &attempts,
    )
    .expect("retain signed attempt after observation failure")
    .expect("terminal observation outcome");
    assert_eq!(unknown.tx_hash(), attempts[1].info.tx_hash);
    assert!(unknown.receipt().is_none());
    assert_eq!(
        self_broadcast_winner_output(&attempts, index).max_fee_per_gas,
        attempts[1].info.max_fee_per_gas,
    );

    for status in [true, false] {
        let mut receipt = receipt(status, 12);
        receipt.tx_hash.clone_from(&attempts[0].info.tx_hash);
        let observed = crate::block_observer::BlockObservation {
            receipt: Some((0, receipt.clone())),
            head: Some((0, 12)),
        };
        let (index, outcome) = self_broadcast_observation_outcome(Ok(observed), &attempts)
            .expect("canonical observation")
            .expect("receipt outcome");
        assert_eq!(outcome.receipt(), Some(&receipt));
        assert_eq!(
            self_broadcast_winner_output(&attempts, index).max_fee_per_gas,
            attempts[0].info.max_fee_per_gas,
        );
    }
    assert!(self_broadcast_observation_outcome(Err(eyre!("not submitted")), &[]).is_err());
}

#[tokio::test(start_paused = true)]
async fn admitted_fee_sampling_reaches_healthy_fifth_provider_before_quote_deadline() {
    let http = HttpContext::direct_for_tests();
    let urls: Vec<Url> = (0..5)
        .map(|index| {
            Url::parse(&format!("http://127.0.0.1:1/fee-provider-{index}"))
                .expect("synthetic provider URL")
        })
        .collect();
    let started = tokio::time::Instant::now();
    let candidates =
        RpcCandidates::acquire_for_gateway(&http, &general(&RpcChainRoute::new(42161, urls)), None)
            .await
            .expect("gateway candidates");
    let stalled_endpoints: Vec<_> = candidates
        .pool()
        .available_providers()
        .iter()
        .take(4)
        .map(|provider| provider.url.clone())
        .collect();
    let reads = crate::DappRpcReadClient::new(move |endpoint, _read| {
        let stalls = stalled_endpoints.contains(endpoint.expose_url());
        Box::pin(async move {
            if stalls {
                return std::future::pending().await;
            }
            // Gas price and priority fee suffice for a partial sample when
            // the provider's fee-history response cannot be decoded.
            Ok(json!("0x64"))
        })
    });

    let policy = SelfBroadcastFeeQuoteTimeoutPolicy::for_network_mode(WalletNetworkMode::Direct);
    let samples = self_broadcast_parallel_fee_samples_with_reads(
        &candidates,
        started,
        policy,
        Some((reads, 42161)),
    )
    .await
    .expect("the healthy fifth provider supplies fees while earlier providers remain pending")
    .expect("providers were sampled");

    assert!(self_broadcast_quote_from_fee_samples(&samples).is_some());
    assert!(started.elapsed() < policy.hard_deadline);
}

const FEE_CHAIN_ID: u64 = 999_999;
const WAIT: Duration = Duration::from_secs(5);

/// An endpoint reporting `chain_id` that logs each method it receives. It offers a nonzero
/// gas price without a usable tip, so its samples never start the grace period.
async fn fee_endpoint(
    chain_id: u64,
    gate: Option<RpcMockGate>,
) -> (Url, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    let methods = Arc::new(Mutex::new(Vec::new()));
    let logged = methods.clone();
    let responder: crate::rpc_broker::tests::RpcResponder = Arc::new(move |request: Value| {
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        logged.lock().expect("method log").push(method.clone());
        let result = match method.as_str() {
            "eth_chainId" => json!(alloy::primitives::U64::from(chain_id)),
            "eth_gasPrice" => json!("0x64"),
            "eth_maxPriorityFeePerGas" => json!("0x0"),
            _ => {
                return json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32601, "message": "unsupported"},
                });
            }
        };
        json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
    });
    let (url, server) =
        spawn_rpc_mock_with_gate(responder, Arc::default(), Arc::default(), gate).await;
    (url, server, methods)
}

fn received(methods: &Mutex<Vec<String>>, method: &str) -> usize {
    methods
        .lock()
        .expect("method log")
        .iter()
        .filter(|logged| *logged == method)
        .count()
}

/// Wrong-chain endpoints whose identity probes fill every probe slot until released.
async fn stalled_probe_window(
    count: usize,
) -> (Vec<Url>, Vec<tokio::task::JoinHandle<()>>, Vec<RpcMockGate>) {
    let (mut urls, mut servers, mut gates) = (Vec::new(), Vec::new(), Vec::new());
    for _ in 0..count {
        let gate = RpcMockGate::default();
        let (url, server, _) = fee_endpoint(1, Some(gate.clone())).await;
        urls.push(url);
        servers.push(server);
        gates.push(gate);
    }
    (urls, servers, gates)
}

#[tokio::test]
async fn cold_fee_sampling_collects_a_later_admitted_candidate_once() {
    let policy = SelfBroadcastFeeQuoteTimeoutPolicy {
        grace_after_first_usable: Duration::from_secs(1),
        hard_deadline: Duration::from_secs(20),
    };
    // The healthy endpoint shares the first probe window with stalled ones. The later
    // endpoint takes the slot the healthy probe frees, but its identity answer is held.
    let (healthy, healthy_server, healthy_methods) = fee_endpoint(FEE_CHAIN_ID, None).await;
    let (stalled, stalled_servers, gates) = stalled_probe_window(MAX_IDENTITY_PROBES - 1).await;
    let later_gate = RpcMockGate::default();
    let (later, later_server, later_methods) =
        fee_endpoint(FEE_CHAIN_ID, Some(later_gate.clone())).await;
    let mut endpoints = vec![healthy];
    endpoints.extend(stalled);
    endpoints.push(later);
    let http = HttpContext::direct_for_tests_with_candidate_order(|_| {});
    let route = RpcChainRoute::new(FEE_CHAIN_ID, endpoints).with_identity_verification();
    let started_at = tokio::time::Instant::now();
    let candidates = RpcCandidates::acquire(
        &http,
        &general(&route),
        Some(started_at + policy.hard_deadline),
    )
    .await
    .expect("candidates");
    let sampling = tokio::spawn({
        let candidates = candidates.clone();
        async move { self_broadcast_parallel_fee_samples(&candidates, started_at, policy).await }
    });
    for gate in &gates {
        tokio::time::timeout(WAIT, gate.request_started.notified())
            .await
            .expect("stalled probes fill the window");
    }
    tokio::time::timeout(WAIT, async {
        while received(&healthy_methods, "eth_gasPrice") == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the healthy endpoint is sampled");
    tokio::time::timeout(WAIT, later_gate.request_started.notified())
        .await
        .expect("the later endpoint's identity probe is held");
    // Let that sample settle so no sample job is running when the later endpoint arrives.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(received(&later_methods, "eth_gasPrice"), 0);
    assert!(!sampling.is_finished());

    // The later endpoint passes its check and answers every later request; the stalled
    // endpoints fail theirs, which ends discovery.
    let release_later = tokio::spawn(async move {
        loop {
            later_gate.release_response.notify_one();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    for gate in &gates {
        gate.release_response.notify_one();
    }
    let samples = tokio::time::timeout(WAIT, sampling)
        .await
        .expect("sampling ends once discovery and its jobs finish")
        .expect("sampling task")
        .expect("candidates were sampled");
    assert!(started_at.elapsed() < policy.hard_deadline / 2);
    assert_eq!(samples.len(), 2);
    assert_eq!(received(&healthy_methods, "eth_gasPrice"), 1);
    assert_eq!(received(&later_methods, "eth_gasPrice"), 1);
    release_later.abort();
    healthy_server.abort();
    later_server.abort();
    for server in stalled_servers {
        server.abort();
    }
}

#[tokio::test]
async fn stalled_cold_probes_leave_fee_sampling_unavailable_at_its_cutoff() {
    let policy = SelfBroadcastFeeQuoteTimeoutPolicy {
        grace_after_first_usable: Duration::from_millis(100),
        hard_deadline: Duration::from_millis(400),
    };
    let (mut endpoints, stalled_servers, _gates) = stalled_probe_window(MAX_IDENTITY_PROBES).await;
    let (queued, queued_server, _) = fee_endpoint(FEE_CHAIN_ID, None).await;
    endpoints.push(queued);
    let http = HttpContext::direct_for_tests_with_candidate_order(|_| {});
    let route = RpcChainRoute::new(FEE_CHAIN_ID, endpoints).with_identity_verification();
    let started_at = tokio::time::Instant::now();
    let candidates = RpcCandidates::acquire(
        &http,
        &general(&route),
        Some(started_at + policy.hard_deadline),
    )
    .await
    .expect("candidates");

    let samples = self_broadcast_parallel_fee_samples(&candidates, started_at, policy).await;
    assert!(samples.is_none());
    assert!(started_at.elapsed() >= policy.hard_deadline);
    assert!(started_at.elapsed() < policy.hard_deadline * 5);
    queued_server.abort();
    for server in stalled_servers {
        server.abort();
    }
}

/// Reports and lookups reach the actor on different channels, so this waits for the
/// reports to apply, well inside the shortest withdrawal.
async fn snapshots_become(http: &HttpContext, route: &ClassRoute, expected: &[&Url]) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let candidates = RpcCandidates::acquire(http, route, None)
                .await
                .expect("candidates");
            let ready = candidates.pool().available_providers();
            if ready
                .iter()
                .map(|provider| &provider.url)
                .eq(expected.iter().copied())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the reports applied");
}

#[tokio::test]
async fn timed_out_fee_samples_withdraw_their_endpoint_and_grace_cutoffs_report_nothing() {
    async fn sample(http: &HttpContext, route: &ClassRoute, grace: Duration) -> usize {
        let candidates = RpcCandidates::acquire(http, route, None)
            .await
            .expect("candidates");
        let policy = SelfBroadcastFeeQuoteTimeoutPolicy {
            grace_after_first_usable: grace,
            hard_deadline: Duration::from_secs(10),
        };
        self_broadcast_parallel_fee_samples(&candidates, tokio::time::Instant::now(), policy)
            .await
            .expect("candidates were sampled")
            .len()
    }
    let (hanging, hanging_server) = spawn_rpc_mock_with_gate(
        Arc::new(|request: Value| json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x64"})),
        Arc::default(),
        Arc::default(),
        Some(RpcMockGate::default()),
    )
    .await;
    // A usable tip starts the grace period; the fee history error is neutral.
    let (healthy, healthy_server) = spawn_rpc_mock_with_gate(
        Arc::new(|request: Value| match request["method"].as_str() {
            Some("eth_gasPrice" | "eth_maxPriorityFeePerGas") => {
                json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x64"})
            }
            _ => json!({"jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32000, "message": "unavailable"}}),
        }),
        Arc::default(),
        Arc::default(),
        None,
    )
    .await;
    // Only the operation's client times out a request to the hanging endpoint.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let route = general(&RpcChainRoute::new(
        FEE_CHAIN_ID,
        vec![hanging.clone(), healthy.clone()],
    ))
    .isolated(crate::http::OperationHttpClient::for_tests(
        client,
        crate::http::OperationNetworkIsolation::Dedicated,
    ));
    let http = HttpContext::direct_for_tests_with_candidate_order(|_| {});

    // The grace cutoff drops the hanging sample long before its request times out.
    assert_eq!(sample(&http, &route, Duration::from_millis(20)).await, 1);
    // Reports apply in order. Had the dropped sample reported a strike, both endpoints
    // would now be withdrawn, which leaves both eligible.
    let broker = http.rpc_broker();
    broker.report_endpoint_outcome(FEE_CHAIN_ID, &healthy, EndpointHealthOutcome::Strike);
    snapshots_become(&http, &route, &[&hanging]).await;
    broker.report_endpoint_outcome(FEE_CHAIN_ID, &healthy, EndpointHealthOutcome::Healthy);
    snapshots_become(&http, &route, &[&hanging, &healthy]).await;

    // With a longer grace period the hanging requests time out, and the next quote's
    // snapshot leaves that endpoint out.
    sample(&http, &route, Duration::from_secs(5)).await;
    snapshots_become(&http, &route, &[&healthy]).await;
    hanging_server.abort();
    healthy_server.abort();
}

#[tokio::test]
async fn a_failed_optional_replacement_fee_request_withdraws_its_endpoint() {
    // The gas price alone makes a quote; the rate-limited fee history is still a strike.
    let (failing, failing_server) = spawn_rpc_mock_with_gate(
        Arc::new(|request: Value| match request["method"].as_str() {
            Some("eth_gasPrice" | "eth_maxPriorityFeePerGas") => {
                json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x64"})
            }
            _ => json!({"jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32005, "message": "limit exceeded"}}),
        }),
        Arc::default(),
        Arc::default(),
        None,
    )
    .await;
    let (other, other_server) = spawn_rpc_mock_with_gate(
        Arc::new(|request: Value| json!({"jsonrpc": "2.0", "id": request["id"], "result": "0x64"})),
        Arc::default(),
        Arc::default(),
        None,
    )
    .await;
    let route = general(&RpcChainRoute::new(
        FEE_CHAIN_ID,
        vec![failing.clone(), other.clone()],
    ));
    let http = HttpContext::direct_for_tests_with_candidate_order(|_| {});
    let candidates = RpcCandidates::acquire(&http, &route, None)
        .await
        .expect("candidates");
    let provider = candidates
        .pool()
        .available_providers()
        .into_iter()
        .find(|provider| provider.url == failing)
        .expect("the failing endpoint is a candidate");

    self_broadcast_gas_fee_quote(&candidates, &provider)
        .await
        .expect("the gas price makes a quote");
    snapshots_become(&http, &route, &[&other]).await;
    failing_server.abort();
    other_server.abort();
}

#[test]
fn fee_history_sample_uses_current_and_next_block_base_fees() {
    let sample = SelfBroadcastFeeSample::from_parts(
        Some(120),
        Some(3),
        Some(FeeHistory {
            base_fee_per_gas: vec![70, 80, 90],
            ..FeeHistory::default()
        }),
    );

    assert_eq!(sample.current_base_fee_per_gas, Some(80));
    assert_eq!(sample.next_base_fee_per_gas, Some(90));
}

fn sponsored_input(tree: u32, position: u64, value: u64) -> Utxo {
    Utxo::new(
        Note::new_unshield(Address::ZERO, Address::from([0x44; 20]), U256::from(value)),
        tree,
        position,
        UtxoSource {
            tx_hash: FixedBytes::from([value as u8; 32]),
            block_number: value,
            block_timestamp: value,
        },
        UtxoCommitmentKind::Transact,
    )
}

#[test]
fn sponsored_pending_inputs_resolve_exact_pinned_identities() {
    let first = sponsored_input(1, 2, 3);
    let second = sponsored_input(4, 5, 6);
    let selected = [
        SelectedInputIdentity::from_utxo(&second),
        SelectedInputIdentity::from_utxo(&first),
    ];
    let resolved =
        resolve_sponsored_pending_spent_inputs(vec![first.clone(), second.clone()], &selected)
            .expect("resolve pinned inputs");
    assert_eq!(resolved[0].tree, second.tree);
    assert_eq!(resolved[0].position, second.position);
    assert_eq!(resolved[1].tree, first.tree);
    assert_eq!(resolved[1].position, first.position);

    let missing = SelectedInputIdentity {
        tree: 99,
        position: 100,
    };
    assert!(resolve_sponsored_pending_spent_inputs(vec![first], &[missing]).is_err());
}

fn spawn_query_rpc_response(
    result: std::result::Result<Value, Value>,
) -> (Url, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind test query RPC");
    let url = Url::parse(&format!(
        "http://{}",
        listener.local_addr().expect("test query RPC address")
    ))
    .expect("test query RPC URL");
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept query RPC request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set query RPC read timeout");
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut buffer).expect("read query RPC request");
            assert!(read != 0, "query RPC request ended before headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().expect("content length"))
                    })
                    .expect("query RPC content length");
                break (header_end, content_length);
            }
        };
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).expect("read query RPC body");
            assert!(read != 0, "query RPC request ended before body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        let request: Value =
            serde_json::from_slice(&bytes[header_end..header_end + content_length])
                .expect("query RPC JSON");
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let response = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        };
        let response = serde_json::to_string(&response).expect("serialize query RPC response");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .expect("write query RPC response");
    });
    (url, task)
}

#[tokio::test]
async fn sponsored_payer_mismatch_fails_through_active_query_rpc_pool() {
    let (url, task) = spawn_query_rpc_response(Ok(json!("0x")));
    let http = HttpContext::direct_for_tests();
    let pool = query_rpc_pool_for_route(&general(&RpcChainRoute::new(1, vec![url])), &http);
    let result = sponsored_code_preflight_from_rpc_pool(
        &pool,
        Address::from([0x13; 20]),
        Address::from([0x14; 20]),
    )
    .await;
    task.join().expect("query RPC task");
    assert!(matches!(
        result,
        Err(SponsorshipError::PayerRuntimeMismatch)
    ));
}

#[tokio::test]
async fn sponsored_final_payer_call_revert_blocks_preparation() {
    let reflected = "0x1234 https://user:password@example.invalid/private?token=secret";
    let (url, task) = spawn_query_rpc_response(Err(json!({
        "code": -32000,
        "message": reflected,
        "data": reflected,
    })));
    let http = HttpContext::direct_for_tests();
    let pool = query_rpc_pool_for_route(&general(&RpcChainRoute::new(1, vec![url])), &http);
    let provider_handle = pool
        .available_providers()
        .into_iter()
        .next()
        .expect("query RPC provider");
    let result = sponsored_exact_gas_estimate(
        &provider_handle,
        Address::from([0x15; 20]),
        Bytes::from_static(&[0x12, 0x34]),
    )
    .await;
    task.join().expect("query RPC task");
    let error = result.expect_err("payer call revert blocks preparation");
    let formatted = format!("{error} {error:?}");
    assert!(!formatted.contains(reflected));
}

struct MockSponsoredBackend {
    state: Mutex<MockSponsoredBackendState>,
    active_submissions: AtomicUsize,
    max_active_submissions: AtomicUsize,
    observation_establishments: AtomicUsize,
}

#[derive(Default)]
struct MockSponsoredBackendState {
    submit_results: VecDeque<bool>,
    submissions: Vec<String>,
    heads: VecDeque<Option<u64>>,
    fallback_head: Option<u64>,
    receipt: Option<TxReceiptOutput>,
    receipt_after_submissions: Option<usize>,
    marked_hashes: Vec<FixedBytes<32>>,
    submission_delay: Option<Duration>,
    canonical_never_completes: bool,
    mark_delay: Option<Duration>,
}

impl MockSponsoredBackend {
    fn new(state: MockSponsoredBackendState) -> Self {
        Self {
            state: Mutex::new(state),
            active_submissions: AtomicUsize::new(0),
            max_active_submissions: AtomicUsize::new(0),
            observation_establishments: AtomicUsize::new(0),
        }
    }

    fn submission_count(&self) -> usize {
        self.state.lock().expect("mock state").submissions.len()
    }

    fn marked_hashes(&self) -> Vec<FixedBytes<32>> {
        self.state.lock().expect("mock state").marked_hashes.clone()
    }

    fn submissions(&self) -> Vec<String> {
        self.state.lock().expect("mock state").submissions.clone()
    }

    fn observation_establishment_count(&self) -> usize {
        self.observation_establishments.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SponsoredSelfBroadcastBackend for MockSponsoredBackend {
    async fn establish_observation(&self) -> Result<()> {
        self.observation_establishments
            .fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn submit_bundle(&self, signed_raw_transaction: &str, _tx_hash: FixedBytes<32>) -> bool {
        assert_eq!(self.observation_establishment_count(), 1);
        let active = self.active_submissions.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active_submissions
            .fetch_max(active, Ordering::SeqCst);
        let submission_delay = self.state.lock().expect("mock state").submission_delay;
        if let Some(submission_delay) = submission_delay {
            tokio::time::sleep(submission_delay).await;
        } else {
            tokio::task::yield_now().await;
        }
        let result = {
            let mut state = self.state.lock().expect("mock state");
            state.submissions.push(signed_raw_transaction.to_owned());
            state.submit_results.pop_front().unwrap_or(false)
        };
        self.active_submissions.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn canonical_state(&self, _tx_hash: FixedBytes<32>) -> SponsoredCanonicalState {
        if self
            .state
            .lock()
            .expect("mock state")
            .canonical_never_completes
        {
            std::future::pending::<()>().await;
        }
        let mut state = self.state.lock().expect("mock state");
        let ready = state
            .receipt_after_submissions
            .is_some_and(|required| state.submissions.len() >= required);
        let receipt = ready.then(|| state.receipt.take()).flatten();
        let head = state
            .heads
            .pop_front()
            .unwrap_or(state.fallback_head)
            .map(|head| (0, head));
        SponsoredCanonicalState {
            receipt,
            head,
            ..SponsoredCanonicalState::default()
        }
    }

    async fn mark_inputs_pending_spent(&self, tx_hash: FixedBytes<32>) -> Result<()> {
        let delay = self.state.lock().expect("mock state").mark_delay;
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        self.state
            .lock()
            .expect("mock state")
            .marked_hashes
            .push(tx_hash);
        Ok(())
    }
}

async fn wait_for_submission_count(backend: &MockSponsoredBackend, expected: usize) {
    for _ in 0..100 {
        if backend.submission_count() >= expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "expected {expected} submissions, observed {}",
        backend.submission_count()
    );
}

async fn wait_for_active_submission(backend: &MockSponsoredBackend) {
    for _ in 0..100 {
        if backend.active_submissions.load(Ordering::SeqCst) != 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("expected an active submission");
}

async fn advance_and_yield(duration: Duration) {
    tokio::time::advance(duration).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

fn receipt(status: bool, block_number: u64) -> TxReceiptOutput {
    TxReceiptOutput {
        tx_hash: format!("0x{}", "11".repeat(32)),
        status,
        block_number,
        gas_used: 21_000,
        contract_address: None,
    }
}

#[test]
fn trezor_app_passphrase_self_broadcast_error_adds_restart_guidance() {
    let error = eyre!(TREZOR_APP_PASSPHRASE_ERROR_TEXT);
    let message = self_broadcast_signing_error_message(&error);

    assert!(is_trezor_app_passphrase_required_error(&error));
    assert!(message.contains(TREZOR_SELF_BROADCAST_RESTART_GUIDANCE));
}

#[test]
fn sponsored_head_poll_interval_clamps_sub_second_and_slow_chains() {
    assert_eq!(
        sponsored_self_broadcast_head_poll_interval(Duration::from_millis(450)),
        Duration::from_millis(500)
    );
    assert_eq!(
        sponsored_self_broadcast_head_poll_interval(Duration::from_secs(4)),
        Duration::from_secs(1)
    );
    assert_eq!(
        sponsored_self_broadcast_head_poll_interval(Duration::from_secs(20)),
        Duration::from_secs(3)
    );
}

#[test]
fn sponsored_cancel_grace_clamps_fast_and_slow_chains() {
    assert_eq!(
        sponsored_self_broadcast_cancel_grace(Duration::from_millis(500)),
        Duration::from_secs(15)
    );
    assert_eq!(
        sponsored_self_broadcast_cancel_grace(Duration::from_secs(12)),
        Duration::from_secs(24)
    );
    assert_eq!(
        sponsored_self_broadcast_cancel_grace(Duration::from_mins(1)),
        Duration::from_mins(1)
    );
}

#[tokio::test(start_paused = true)]
async fn sponsored_session_retries_on_heads_and_missed_slot_heartbeat_without_overlap() {
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([false, false, false, false]),
        heads: VecDeque::from([Some(10), Some(11), Some(12)]),
        fallback_head: Some(12),
        ..MockSponsoredBackendState::default()
    }));
    let (_command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            FixedBytes::from([0x11; 32]),
            "0xidentical-signed-transaction",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    assert_eq!(backend.observation_establishment_count(), 1);
    advance_and_yield(Duration::from_secs(1)).await;
    wait_for_submission_count(&backend, 2).await;
    advance_and_yield(Duration::from_secs(1)).await;
    wait_for_submission_count(&backend, 3).await;
    advance_and_yield(Duration::from_secs(4)).await;
    wait_for_submission_count(&backend, 4).await;

    assert_eq!(backend.submission_count(), 4);
    assert_eq!(backend.max_active_submissions.load(Ordering::SeqCst), 1);
    assert!(
        backend
            .submissions()
            .iter()
            .all(|raw| raw == "0xidentical-signed-transaction")
    );

    task.abort();
    let _ = task.await;
}

#[tokio::test(start_paused = true)]
async fn sponsored_cancellation_waits_for_head_newer_than_last_acceptance() {
    let tx_hash = FixedBytes::from([0x22; 32]);
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        heads: VecDeque::from([Some(20), Some(20), Some(21)]),
        fallback_head: Some(21),
        ..MockSponsoredBackendState::default()
    }));
    let (command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            tx_hash,
            "0xaccepted",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    tokio::task::yield_now().await;
    command_tx
        .send(SponsoredSelfBroadcastCommand::Cancel)
        .expect("cancel session");
    advance_and_yield(Duration::from_secs(1)).await;
    assert!(!task.is_finished());
    advance_and_yield(Duration::from_secs(1)).await;

    let outcome = task.await.expect("session task").expect("session outcome");
    assert_eq!(
        outcome,
        SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Cancelled,
            bundle_was_accepted: true,
        }
    );
    assert_eq!(backend.submission_count(), 1);
    assert_eq!(backend.marked_hashes(), vec![tx_hash]);
}

#[tokio::test(start_paused = true)]
async fn sponsored_cancellation_during_fanout_is_latched_while_submission_drains() {
    let tx_hash = FixedBytes::from([0x23; 32]);
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        heads: VecDeque::from([Some(20), Some(20), Some(21)]),
        fallback_head: Some(21),
        submission_delay: Some(Duration::from_secs(1)),
        ..MockSponsoredBackendState::default()
    }));
    let (command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            tx_hash,
            "0xaccepted-during-cancel",
            command_rx,
        )
        .await
    });

    wait_for_active_submission(&backend).await;
    command_tx
        .send(SponsoredSelfBroadcastCommand::Cancel)
        .expect("cancel session");
    advance_and_yield(Duration::from_secs(3)).await;

    let outcome = task.await.expect("session task").expect("session outcome");
    assert_eq!(
        outcome,
        SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Cancelled,
            bundle_was_accepted: true,
        }
    );
    assert_eq!(backend.submission_count(), 1);
    assert_eq!(backend.marked_hashes(), vec![tx_hash]);
    assert_eq!(backend.active_submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn sponsored_retry_acceptance_requires_a_fresh_post_acceptance_head() {
    let tx_hash = FixedBytes::from([0x25; 32]);
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([false, true]),
        heads: VecDeque::from([Some(100), Some(101), Some(101), Some(102)]),
        fallback_head: Some(102),
        ..MockSponsoredBackendState::default()
    }));
    let (command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            tx_hash,
            "0xaccepted-retry",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    advance_and_yield(Duration::from_secs(1)).await;
    wait_for_submission_count(&backend, 2).await;
    command_tx
        .send(SponsoredSelfBroadcastCommand::Cancel)
        .expect("cancel session");
    advance_and_yield(Duration::from_secs(1)).await;
    assert!(!task.is_finished());
    advance_and_yield(Duration::from_secs(1)).await;

    let outcome = task.await.expect("session task").expect("session outcome");
    assert_eq!(
        outcome,
        SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Cancelled,
            bundle_was_accepted: true,
        }
    );
}

#[tokio::test(start_paused = true)]
async fn sponsored_session_renews_pending_spent_locks_while_waiting_for_receipt() {
    let tx_hash = FixedBytes::from([0x24; 32]);
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        fallback_head: Some(20),
        ..MockSponsoredBackendState::default()
    }));
    let (_command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_mins(10),
            tx_hash,
            "0xaccepted",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    assert_eq!(backend.marked_hashes(), vec![tx_hash]);
    advance_and_yield(SPONSORED_PENDING_LOCK_RENEW_INTERVAL).await;
    assert_eq!(backend.marked_hashes(), vec![tx_hash, tx_hash]);

    task.abort();
    let _ = task.await;
}

#[tokio::test(start_paused = true)]
async fn sponsored_canonical_revert_is_authoritative_after_relay_acceptance() {
    let tx_hash = FixedBytes::from([0x33; 32]);
    let backend = MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        fallback_head: Some(30),
        receipt: Some(receipt(false, 31)),
        receipt_after_submissions: Some(1),
        ..MockSponsoredBackendState::default()
    });
    let (_command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);

    let outcome = run_sponsored_self_broadcast_session_with_backend(
        &backend,
        Duration::from_secs(4),
        tx_hash,
        "0xreverting",
        command_rx,
    )
    .await
    .expect("session outcome");

    let SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt) = outcome else {
        panic!("expected canonical receipt");
    };
    assert!(!receipt.status);
}

#[tokio::test(start_paused = true)]
async fn sponsored_shutdown_reports_accepted_session_stopped() {
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        heads: VecDeque::from([Some(50), Some(51)]),
        fallback_head: Some(51),
        ..MockSponsoredBackendState::default()
    }));
    let (command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            FixedBytes::from([0x55; 32]),
            "0xsession-only",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    tokio::task::yield_now().await;
    command_tx
        .send(SponsoredSelfBroadcastCommand::Shutdown)
        .expect("shutdown session");
    advance_and_yield(Duration::from_secs(1)).await;

    let outcome = task.await.expect("session task").expect("session outcome");
    assert_eq!(
        outcome,
        SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Shutdown,
            bundle_was_accepted: true,
        }
    );
}

#[tokio::test(start_paused = true)]
async fn sponsored_cancel_stops_at_grace_deadline_when_rpcs_never_advance() {
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        canonical_never_completes: true,
        ..MockSponsoredBackendState::default()
    }));
    let (command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            FixedBytes::from([0x56; 32]),
            "0xstale-rpc",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    command_tx
        .send(SponsoredSelfBroadcastCommand::Cancel)
        .expect("cancel session");
    drop(command_tx);
    advance_and_yield(Duration::from_secs(14)).await;
    assert!(!task.is_finished());
    advance_and_yield(Duration::from_secs(1)).await;

    assert_eq!(
        task.await.expect("session task").expect("session outcome"),
        SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Cancelled,
            bundle_was_accepted: true,
        }
    );
}

#[tokio::test(start_paused = true)]
async fn sponsored_shutdown_interrupts_a_pending_canonical_poll() {
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        canonical_never_completes: true,
        ..MockSponsoredBackendState::default()
    }));
    let (command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            FixedBytes::from([0x57; 32]),
            "0xhung-rpc",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    tokio::task::yield_now().await;
    command_tx
        .send(SponsoredSelfBroadcastCommand::Shutdown)
        .expect("shutdown session");
    tokio::task::yield_now().await;

    assert_eq!(
        task.await.expect("session task").expect("session outcome"),
        SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Shutdown,
            bundle_was_accepted: true,
        }
    );
}

#[tokio::test(start_paused = true)]
async fn sponsored_shutdown_overrides_an_active_cancel_grace() {
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        fallback_head: Some(50),
        ..MockSponsoredBackendState::default()
    }));
    let (command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            FixedBytes::from([0x59; 32]),
            "0xcancel-then-shutdown",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    command_tx
        .send(SponsoredSelfBroadcastCommand::Cancel)
        .expect("cancel session");
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    command_tx
        .send(SponsoredSelfBroadcastCommand::Shutdown)
        .expect("shutdown session");
    tokio::task::yield_now().await;

    assert_eq!(
        task.await.expect("session task").expect("session outcome"),
        SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Shutdown,
            bundle_was_accepted: true,
        }
    );
}

#[tokio::test(start_paused = true)]
async fn sponsored_shutdown_interrupts_pending_spend_bookkeeping() {
    let backend = Arc::new(MockSponsoredBackend::new(MockSponsoredBackendState {
        submit_results: VecDeque::from([true]),
        mark_delay: Some(Duration::from_mins(1)),
        ..MockSponsoredBackendState::default()
    }));
    let (command_tx, command_rx) = watch::channel(SponsoredSelfBroadcastCommand::Running);
    let task_backend = Arc::clone(&backend);
    let task = tokio::spawn(async move {
        run_sponsored_self_broadcast_session_with_backend(
            task_backend.as_ref(),
            Duration::from_secs(4),
            FixedBytes::from([0x58; 32]),
            "0xhung-actor",
            command_rx,
        )
        .await
    });

    wait_for_submission_count(&backend, 1).await;
    tokio::task::yield_now().await;
    command_tx
        .send(SponsoredSelfBroadcastCommand::Shutdown)
        .expect("shutdown session");
    tokio::task::yield_now().await;

    assert_eq!(
        task.await.expect("session task").expect("session outcome"),
        SponsoredSelfBroadcastSessionOutcome::Stopped {
            reason: SponsoredSelfBroadcastStopReason::Shutdown,
            bundle_was_accepted: true,
        }
    );
}
