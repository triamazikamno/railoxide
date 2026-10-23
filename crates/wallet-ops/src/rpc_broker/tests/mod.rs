use super::actor::{Actor, Command, EndpointHealthOutcome, JobExecutor, JobOutput, RequestEvent};
use super::broker::{RpcBroker, default_executor};
use super::capacity::{JobControl, JobResources};
use super::execution::wire_request_for;
use super::model::*;
use super::operation::RpcOperation;
use super::profile::*;
use super::resolution;
use super::resolution::{WaiterPolicy, WaiterSnapshot, WaiterState, WorkItem, WorkKey};
use super::scheduler::{Claim, ExecutionJob, ReadyScheduler, partition_work};
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, Bytes, U256, hex};
use alloy::providers::bindings::IMulticall3;
use alloy::rpc::json_rpc::ErrorPayload;
use alloy::rpc::types::eth::state::StateOverride;
use alloy::rpc::types::eth::transaction::{TransactionInput, TransactionRequest};
use alloy::serde::WithOtherFields;
use alloy::sol_types::SolCall;
use serde_json::{Value, json};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::time::{self, Instant};
use url::Url;

/// Identity probes a candidate source runs at once, for tests outside the broker.
pub(crate) const MAX_IDENTITY_PROBES: usize = super::identity::MAX_IDENTITY_PROBES;

pub(crate) fn data_result(bytes: Bytes) -> RpcResult {
    RpcResult::new(serde_json::to_value(bytes).expect("Bytes serialize"))
}

pub(super) fn test_route() -> RpcRoute {
    RpcRoute::from(RpcChainRoute::new(
        1,
        vec![Url::parse("https://test.invalid").unwrap()],
    ))
}

pub(super) fn test_origin() -> RpcOrigin {
    RpcOrigin::from(WalletRpcOrigin::PublicWallet)
}

fn test_route_with_multicall(address: Address) -> RpcRoute {
    RpcRoute::from(
        RpcChainRoute::new(1, vec![Url::parse("https://test.invalid").unwrap()])
            .with_multicall(address),
    )
}

pub(super) fn read_calldata(read: &RpcRead) -> Bytes {
    match read.operation() {
        RpcOperation::EthCall { request, .. } => request.input.input().cloned().unwrap_or_default(),
        _ => Bytes::new(),
    }
}

pub(super) fn rpc_read_from_request(
    request: TransactionRequest,
    block: BlockId,
    state_overrides: Option<StateOverride>,
) -> RpcRead {
    RpcRead::from_rpc(WithOtherFields::new(request), block, state_overrides, 1)
        .expect("typed test eth_call")
}

pub(super) fn test_executor() -> JobExecutor {
    Arc::new(|_resources, group, _endpoints, _control| {
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
}

impl ReadyScheduler {
    /// Claims the next queued job as if every job could read now.
    pub(in crate::rpc_broker) fn next_ready(&mut self) -> Option<ExecutionJob> {
        match self.next(|_| true)? {
            Claim::Queued(job) => Some(job),
            Claim::Resume(_) => panic!("only queued work was admitted"),
        }
    }
}

fn execution_job_len(job: &ExecutionJob) -> usize {
    match job {
        ExecutionJob::Aggregate(items) => items.len(),
        ExecutionJob::Individual(_) => 1,
    }
}

pub(super) fn test_broker(interval: Duration, max_in_flight: usize) -> Arc<RpcBroker> {
    RpcBroker::spawn_with_test_config(
        reqwest::Client::new(),
        interval,
        max_in_flight,
        default_executor(),
    )
    .expect("test broker requires an active Tokio runtime")
}

pub(super) fn test_broker_with_executor(
    interval: Duration,
    max_in_flight: usize,
    executor: JobExecutor,
) -> Arc<RpcBroker> {
    RpcBroker::spawn_with_test_config(reqwest::Client::new(), interval, max_in_flight, executor)
        .expect("test broker requires an active Tokio runtime")
}

/// Broker whose executor answers every read with `cached` and counts its own invocations, so
/// callers outside this module can assert whether a latest read was served from the cache.
pub(crate) fn spawn_counting_test_broker() -> (Arc<RpcBroker>, Arc<AtomicUsize>) {
    let executions = Arc::new(AtomicUsize::new(0));
    let executor: JobExecutor = {
        let executions = Arc::clone(&executions);
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
    (broker, executions)
}

pub(super) fn rpc_result(request: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": request["id"], "result": result})
}

pub(super) fn rpc_error(request: &Value, code: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "error": {"code": code, "message": "mock RPC failure"}
    })
}

pub(super) fn remote_error(code: i64) -> RpcBrokerError {
    RpcBrokerError::Remote(RpcRemoteError::from(
        serde_json::from_value::<WithOtherFields<ErrorPayload<Value>>>(json!({
            "code": code,
            "message": "mock RPC failure"
        }))
        .expect("typed mock RPC error"),
    ))
}

pub(super) fn aggregate_response(request: &Value, results: Vec<(bool, Bytes)>) -> Value {
    let returns = results
        .into_iter()
        .map(|(success, return_data)| IMulticall3::Result {
            success,
            returnData: return_data,
        })
        .collect::<Vec<_>>();
    let encoded = IMulticall3::tryAggregateCall::abi_encode_returns(&returns);
    rpc_result(request, &json!(format!("0x{}", hex::encode(encoded))))
}

pub(crate) type RpcResponder = Arc<dyn Fn(Value) -> Value + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct RpcMockGate {
    pub(crate) request_started: Arc<tokio::sync::Notify>,
    pub(crate) release_response: Arc<tokio::sync::Notify>,
}

pub(crate) async fn spawn_rpc_mock(
    responder: RpcResponder,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
) -> (Url, tokio::task::JoinHandle<()>) {
    spawn_rpc_mock_with_gate(responder, active, maximum, None).await
}

pub(crate) async fn spawn_gated_rpc_mock(
    responder: RpcResponder,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    gate: RpcMockGate,
) -> (Url, tokio::task::JoinHandle<()>) {
    spawn_rpc_mock_with_gate(responder, active, maximum, Some(gate)).await
}

pub(crate) async fn spawn_rpc_mock_with_gate(
    responder: RpcResponder,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    gate: Option<RpcMockGate>,
) -> (Url, tokio::task::JoinHandle<()>) {
    serve_rpc_mock(responder, active, maximum, gate, None).await
}

/// Each request's `x-route` header, naming the client that sent it, and JSON body.
pub(super) type RecordedRequests = Arc<Mutex<Vec<(Option<String>, Value)>>>;

/// Like [`spawn_rpc_mock`], also recording every request in arrival order.
pub(super) async fn spawn_recording_rpc_mock(
    responder: RpcResponder,
) -> (Url, RecordedRequests, tokio::task::JoinHandle<()>) {
    let recorded = RecordedRequests::default();
    let (url, task) = serve_rpc_mock(
        responder,
        Arc::default(),
        Arc::default(),
        None,
        Some(recorded.clone()),
    )
    .await;
    (url, recorded, task)
}

async fn serve_rpc_mock(
    responder: RpcResponder,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    gate: Option<RpcMockGate>,
    recorded: Option<RecordedRequests>,
) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind RPC mock");
    let address = listener.local_addr().expect("RPC mock address");
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let responder = responder.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            let gate = gate.clone();
            let recorded = recorded.clone();
            tokio::spawn(handle_rpc_mock(
                stream, responder, active, maximum, gate, recorded,
            ));
        }
    });
    (
        Url::parse(&format!("http://{address}")).expect("RPC mock URL"),
        task,
    )
}

pub(super) async fn spawn_status_rpc_mock(status: u16) -> (Url, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind status RPC mock");
    let address = listener.local_addr().expect("status RPC mock address");
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                let body = br#"{"jsonrpc":"2.0","id":1,"result":"0xdeadbeef"}"#;
                let headers = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(headers.as_bytes()).await;
                let _ = stream.write_all(body).await;
            });
        }
    });
    (
        Url::parse(&format!("http://{address}")).expect("status RPC mock URL"),
        task,
    )
}

async fn handle_rpc_mock(
    mut stream: TcpStream,
    responder: RpcResponder,
    active: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    gate: Option<RpcMockGate>,
    recorded: Option<RecordedRequests>,
) -> std::io::Result<()> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (body_start, content_length, route_tag) = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..body_start]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .starts_with("content-length:")
                    .then(|| line.split_once(':'))
                    .flatten()
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let route_tag = headers.lines().find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("x-route:")
                .map(|tag| tag.trim().to_owned())
        });
        break (body_start, content_length, route_tag);
    };
    while request.len() < body_start + content_length {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let body: Value = serde_json::from_slice(&request[body_start..body_start + content_length])
        .expect("RPC mock JSON request");
    if let Some(recorded) = recorded {
        recorded.lock().unwrap().push((route_tag, body.clone()));
    }
    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
    maximum.fetch_max(current, Ordering::SeqCst);
    if let Some(gate) = gate {
        gate.request_started.notify_one();
        gate.release_response.notified().await;
    }
    let response = responder(body);
    time::sleep(Duration::from_millis(2)).await;
    active.fetch_sub(1, Ordering::SeqCst);
    let response = serde_json::to_vec(&response).expect("RPC mock JSON response");
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(&response).await
}

mod actor;
mod candidates;
mod isolation;
mod methods;
mod model;
mod profile;
mod progressive;
mod warmup;
