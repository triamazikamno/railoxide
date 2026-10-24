use std::time::Duration;

use std::future::Future;

use futures_util::stream::{FuturesUnordered, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::SensitiveUrl;
use crate::http::{OperationHttpClient, redact_url_for_display};

const SPONSORED_BUNDLE_REQUEST_ID: u64 = 1;
const SPONSORED_BUNDLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SponsoredBundleRelayFailureKind {
    Transport,
    HttpStatus(u16),
    InvalidResponse,
    JsonRpc { code: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredBundleRelayFailure {
    pub relay: String,
    pub kind: SponsoredBundleRelayFailureKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsoredBundleFanout {
    pub accepted_relays: Vec<String>,
    pub failures: Vec<SponsoredBundleRelayFailure>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("all sponsored bundle relays failed: {failures:?}")]
pub struct SponsoredBundleAllRelaysFailed {
    pub failures: Vec<SponsoredBundleRelayFailure>,
}

#[derive(Serialize)]
struct SponsoredBundleRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: [SponsoredBundleParams<'a>; 1],
}

#[derive(Serialize)]
struct SponsoredBundleParams<'a> {
    txs: [&'a str; 1],
}

#[derive(Deserialize)]
struct SponsoredBundleResponse {
    jsonrpc: String,
    id: u64,
    result: Option<Value>,
    error: Option<SponsoredBundleJsonRpcError>,
}

#[derive(Deserialize)]
struct SponsoredBundleJsonRpcError {
    code: i64,
}

enum RelayAttemptError {
    Transport,
    HttpStatus(u16),
    InvalidResponse,
    JsonRpc(i64),
}

/// Submits one signed transaction as a pending-block bundle to every relay.
///
/// All requests use the operation's client; relay errors deliberately omit
/// response messages because providers can reflect signed bytes or endpoint data.
pub async fn submit_sponsored_bundle(
    client: &OperationHttpClient,
    relays: &[SensitiveUrl],
    signed_raw_transaction: &str,
) -> Result<SponsoredBundleFanout, SponsoredBundleAllRelaysFailed> {
    submit_sponsored_bundle_with_acceptance(client, relays, signed_raw_transaction, || async {})
        .await
}

pub(crate) async fn submit_sponsored_bundle_with_acceptance<F, Fut>(
    client: &OperationHttpClient,
    relays: &[SensitiveUrl],
    signed_raw_transaction: &str,
    on_first_acceptance: F,
) -> Result<SponsoredBundleFanout, SponsoredBundleAllRelaysFailed>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut attempts = relays
        .iter()
        .map(|relay| async move {
            let redacted_relay = redact_url_for_display(relay.expose_url());
            let result =
                submit_sponsored_bundle_to_relay(client, relay, signed_raw_transaction).await;
            (redacted_relay, result)
        })
        .collect::<FuturesUnordered<_>>();

    let mut accepted_relays = Vec::new();
    let mut failures = Vec::new();
    let mut on_first_acceptance = Some(on_first_acceptance);
    while let Some((relay, result)) = attempts.next().await {
        match result {
            Ok(()) => {
                if let Some(on_first_acceptance) = on_first_acceptance.take() {
                    on_first_acceptance().await;
                }
                accepted_relays.push(relay);
            }
            Err(error) => failures.push(SponsoredBundleRelayFailure {
                relay,
                kind: match error {
                    RelayAttemptError::Transport => SponsoredBundleRelayFailureKind::Transport,
                    RelayAttemptError::HttpStatus(status) => {
                        SponsoredBundleRelayFailureKind::HttpStatus(status)
                    }
                    RelayAttemptError::InvalidResponse => {
                        SponsoredBundleRelayFailureKind::InvalidResponse
                    }
                    RelayAttemptError::JsonRpc(code) => {
                        SponsoredBundleRelayFailureKind::JsonRpc { code }
                    }
                },
            }),
        }
    }

    if accepted_relays.is_empty() {
        Err(SponsoredBundleAllRelaysFailed { failures })
    } else {
        Ok(SponsoredBundleFanout {
            accepted_relays,
            failures,
        })
    }
}

async fn submit_sponsored_bundle_to_relay(
    client: &OperationHttpClient,
    relay: &SensitiveUrl,
    signed_raw_transaction: &str,
) -> Result<(), RelayAttemptError> {
    let mut request_url = relay.expose_url().clone();
    let _ = request_url.set_username("");
    let _ = request_url.set_password(None);
    let request = SponsoredBundleRequest {
        jsonrpc: "2.0",
        id: SPONSORED_BUNDLE_REQUEST_ID,
        method: "eth_sendBundle",
        params: [SponsoredBundleParams {
            txs: [signed_raw_transaction],
        }],
    };
    let response = client
        .client()
        .post(request_url)
        .timeout(SPONSORED_BUNDLE_REQUEST_TIMEOUT)
        .json(&request)
        .send()
        .await
        .map_err(|_| RelayAttemptError::Transport)?;
    if !response.status().is_success() {
        return Err(RelayAttemptError::HttpStatus(response.status().as_u16()));
    }
    let response = response
        .json::<SponsoredBundleResponse>()
        .await
        .map_err(|_| RelayAttemptError::InvalidResponse)?;
    if response.jsonrpc != "2.0" || response.id != SPONSORED_BUNDLE_REQUEST_ID {
        return Err(RelayAttemptError::InvalidResponse);
    }
    if let Some(error) = response.error {
        return Err(RelayAttemptError::JsonRpc(error.code));
    }
    if response.result.is_none() {
        return Err(RelayAttemptError::InvalidResponse);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;
    use std::time::Duration;

    use serde_json::{Value, json};
    use url::Url;

    use super::*;
    use crate::http::{OperationNetworkIsolation, WalletNetworkMode};

    fn operation_client(client: reqwest::Client) -> OperationHttpClient {
        OperationHttpClient::for_tests(
            client,
            OperationNetworkIsolation::Unavailable(WalletNetworkMode::Direct),
        )
    }

    struct CapturedRequest {
        headers: String,
        body: Value,
    }

    struct ResponseGate {
        ready: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    fn spawn_relay(
        status: u16,
        response_body: &str,
        gate: Option<ResponseGate>,
    ) -> (Url, mpsc::Receiver<CapturedRequest>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind test relay");
        let url = Url::parse(&format!(
            "http://{}",
            listener.local_addr().expect("test relay address")
        ))
        .expect("test relay URL");
        let response_body = response_body.to_owned();
        let (request_tx, request_rx) = mpsc::channel();
        let task = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept relay request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set relay read timeout");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            let (header_end, content_length) = loop {
                let read = stream.read(&mut buffer).expect("read relay request");
                assert!(read != 0, "relay request ended before headers");
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let header_end = header_end + 4;
                    let headers = String::from_utf8_lossy(&bytes[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("content length"))
                        })
                        .expect("request content length");
                    break (header_end, content_length);
                }
            };
            while bytes.len() < header_end + content_length {
                let read = stream.read(&mut buffer).expect("read relay request body");
                assert!(read != 0, "relay request ended before body");
                bytes.extend_from_slice(&buffer[..read]);
            }
            let _ = request_tx.send(CapturedRequest {
                headers: String::from_utf8(bytes[..header_end].to_vec())
                    .expect("request headers UTF-8"),
                body: serde_json::from_slice(&bytes[header_end..header_end + content_length])
                    .expect("request JSON"),
            });
            if let Some(gate) = gate {
                gate.ready.send(()).expect("signal relay ready");
                gate.release.recv().expect("wait to release relay");
            }
            let reason = if status == 200 {
                "OK"
            } else {
                "Internal Server Error"
            };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            )
            .expect("write relay response");
        });
        (url, request_rx, task)
    }

    fn success_response() -> &'static str {
        r#"{"jsonrpc":"2.0","id":1,"result":"0xbundle"}"#
    }

    fn rpc_error_response(message: &str) -> String {
        serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": { "code": -32000, "message": message },
        }))
        .expect("serialize test RPC error")
    }

    #[tokio::test]
    async fn request_shape_is_exact_and_unauthenticated() {
        let (mut url, request_rx, task) = spawn_relay(200, success_response(), None);
        url.set_username("relay-user").expect("set URL username");
        url.set_password(Some("relay-password"))
            .expect("set URL password");
        let relays = [SensitiveUrl::from(url)];
        let raw_transaction = "0x010203";

        let client = operation_client(crate::rpc_broker::tests::tagged_reqwest_client("operation"));

        let result = submit_sponsored_bundle(&client, &relays, raw_transaction)
            .await
            .expect("bundle accepted");
        let request = request_rx.recv().expect("captured relay request");
        task.join().expect("relay task");

        assert_eq!(result.accepted_relays.len(), 1);
        assert_eq!(
            request.body,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_sendBundle",
                "params": [{ "txs": [raw_transaction] }],
            })
        );
        let headers = request.headers.to_ascii_lowercase();
        assert!(!headers.contains("authorization:"));
        assert!(headers.contains("x-route: operation"));
    }

    #[tokio::test]
    async fn identical_bundle_fanout_is_concurrent_and_marks_first_acceptance() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_one_tx, release_one_rx) = mpsc::channel();
        let (release_two_tx, release_two_rx) = mpsc::channel();
        let (url_one, request_one_rx, task_one) = spawn_relay(
            200,
            success_response(),
            Some(ResponseGate {
                ready: ready_tx.clone(),
                release: release_one_rx,
            }),
        );
        let (url_two, request_two_rx, task_two) = spawn_relay(
            200,
            success_response(),
            Some(ResponseGate {
                ready: ready_tx,
                release: release_two_rx,
            }),
        );
        let concurrent = Arc::new(AtomicBool::new(false));
        let observed_concurrent = Arc::clone(&concurrent);
        let callback_before_second = Arc::new(AtomicBool::new(false));
        let observed_callback = Arc::clone(&callback_before_second);
        let callback_count = Arc::new(AtomicUsize::new(0));
        let counted_callbacks = Arc::clone(&callback_count);
        let (callback_tx, callback_rx) = mpsc::channel();
        let coordinator = thread::spawn(move || {
            let first = ready_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            let second = ready_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            observed_concurrent.store(first && second, Ordering::SeqCst);
            let _ = release_one_tx.send(());
            observed_callback.store(
                callback_rx.recv_timeout(Duration::from_secs(2)).is_ok(),
                Ordering::SeqCst,
            );
            let _ = release_two_tx.send(());
        });
        let relays = [SensitiveUrl::from(url_one), SensitiveUrl::from(url_two)];
        let raw_transaction = "0xidentical-signed-transaction";

        let result = submit_sponsored_bundle_with_acceptance(
            &operation_client(reqwest::Client::new()),
            &relays,
            raw_transaction,
            move || async move {
                counted_callbacks.fetch_add(1, Ordering::SeqCst);
                let _ = callback_tx.send(());
            },
        )
        .await
        .expect("both bundles accepted");
        let request_one = request_one_rx.recv().expect("first relay request");
        let request_two = request_two_rx.recv().expect("second relay request");
        coordinator.join().expect("coordinator task");
        task_one.join().expect("first relay task");
        task_two.join().expect("second relay task");

        assert!(concurrent.load(Ordering::SeqCst));
        assert!(callback_before_second.load(Ordering::SeqCst));
        assert_eq!(callback_count.load(Ordering::SeqCst), 1);
        assert_eq!(result.accepted_relays.len(), 2);
        assert_eq!(request_one.body, request_two.body);
        assert_eq!(
            request_one.body["params"][0]["txs"],
            json!([raw_transaction])
        );
    }

    #[tokio::test]
    async fn partial_success_isolates_relay_failure() {
        let rpc_error = rpc_error_response("relay rejected bundle");
        let (success_url, _, success_task) = spawn_relay(200, success_response(), None);
        let (failure_url, _, failure_task) = spawn_relay(200, &rpc_error, None);
        let relays = [
            SensitiveUrl::from(success_url),
            SensitiveUrl::from(failure_url),
        ];

        let client = operation_client(reqwest::Client::new());
        let result = submit_sponsored_bundle(&client, &relays, "0xsigned")
            .await
            .expect("one relay acceptance is success");
        success_task.join().expect("success relay task");
        failure_task.join().expect("failure relay task");

        assert_eq!(result.accepted_relays.len(), 1);
        assert_eq!(result.failures.len(), 1);
        assert_eq!(
            result.failures[0].kind,
            SponsoredBundleRelayFailureKind::JsonRpc { code: -32000 }
        );
    }

    #[tokio::test]
    async fn all_relay_failures_are_structured() {
        let rpc_error = rpc_error_response("rejected");
        let (http_error_url, _, http_error_task) = spawn_relay(500, "{}", None);
        let (rpc_error_url, _, rpc_error_task) = spawn_relay(200, &rpc_error, None);
        let relays = [
            SensitiveUrl::from(http_error_url),
            SensitiveUrl::from(rpc_error_url),
        ];

        let client = operation_client(reqwest::Client::new());
        let error = submit_sponsored_bundle(&client, &relays, "0xsigned")
            .await
            .expect_err("all relays fail");
        http_error_task.join().expect("HTTP error relay task");
        rpc_error_task.join().expect("RPC error relay task");

        assert_eq!(error.failures.len(), 2);
        assert!(
            error.failures.iter().any(|failure| {
                failure.kind == SponsoredBundleRelayFailureKind::HttpStatus(500)
            })
        );
        assert!(error.failures.iter().any(|failure| {
            failure.kind == SponsoredBundleRelayFailureKind::JsonRpc { code: -32000 }
        }));
    }

    #[tokio::test]
    async fn failures_redact_endpoint_credentials_and_signed_bytes() {
        let signed_transaction = "0xsigned-transaction-sentinel";
        let response = rpc_error_response(&format!(
            "reflected {signed_transaction} https://relay-user:relay-password@example.invalid/private-path?api-key=relay-query"
        ));
        let (mut url, _, task) = spawn_relay(200, &response, None);
        url.set_username("relay-user").expect("set URL username");
        url.set_password(Some("relay-password"))
            .expect("set URL password");
        url.set_path("/private-path");
        url.set_query(Some("api-key=relay-query"));
        let relay_host = url.host_str().expect("relay host").to_owned();
        let relays = [SensitiveUrl::from(url)];

        let client = operation_client(reqwest::Client::new());
        let error = submit_sponsored_bundle(&client, &relays, signed_transaction)
            .await
            .expect_err("relay rejects bundle");
        task.join().expect("relay task");
        let formatted = format!("{error} {error:?}");

        assert!(formatted.contains(&relay_host));
        for secret in [
            signed_transaction,
            "relay-user",
            "relay-password",
            "private-path",
            "relay-query",
        ] {
            assert!(!formatted.contains(secret), "error leaked {secret}");
        }
    }
}
