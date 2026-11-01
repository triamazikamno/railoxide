//! Tron's additions to a broker route.
//!
//! Each JSON-RPC endpoint may have an HTTP API, which serves what JSON-RPC can't. Identity checks
//! also ask that HTTP API for block 0, so a mainnet JSON-RPC URL can't be paired with another
//! network's HTTP API. Requests to each endpoint, on either API, are spaced by a minimum interval
//! to stay under keyless rate limits.

use super::actor::EndpointHealthOutcome;
use super::execution::{read_capped_body, send_json_post, should_failover_error};
use super::model::DEFAULT_ATTEMPT_TIMEOUT;
use super::{ClassRoute, RpcBrokerError, RpcChainRoute, RpcEndpointRoles, RpcTransport};
use crate::HttpContext;
use alloy::primitives::{B256, b256};
use poi::SensitiveUrl;
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::time::{self, Instant};
use url::Url;

/// Mainnet's block 0. Tron's chain id is its last four bytes.
const TRON_GENESIS_BLOCK_ID: B256 =
    b256!("00000000000000001ebf88508a03865c71d452e25f4d51194196a1d22b6653dc");
// TronGrid allows 3 keyless requests per second; Pocket and PublicNode publish no limit.
const TRON_REQUEST_INTERVAL: Duration = Duration::from_millis(400);

/// Every configured Tron endpoint, looked up by JSON-RPC URL so that class-restricted clones
/// share it.
#[derive(Debug)]
pub(crate) struct TronEndpoints(Box<[TronEndpoint]>);

#[derive(Debug)]
struct TronEndpoint {
    json_rpc: SensitiveUrl,
    http_api: Option<SensitiveUrl>,
    /// When the next request to this endpoint may start.
    next_request: Mutex<Option<Instant>>,
}

impl PartialEq for TronEndpoints {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self.0.iter().zip(other.0.iter()).all(|(left, right)| {
                left.json_rpc == right.json_rpc && left.http_api == right.http_api
            })
    }
}

impl Eq for TronEndpoints {}

impl Hash for TronEndpoints {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for endpoint in &self.0 {
            endpoint.json_rpc.expose_url().hash(state);
            endpoint
                .http_api
                .as_ref()
                .map(SensitiveUrl::expose_url)
                .hash(state);
        }
    }
}

impl TronEndpoints {
    fn get(&self, endpoint: &Url) -> Option<&TronEndpoint> {
        self.0
            .iter()
            .find(|configured| configured.json_rpc.expose_url() == endpoint)
    }
}

impl RpcChainRoute {
    /// Tron's route: each JSON-RPC endpoint with its optional HTTP API, transport and roles.
    /// Every endpoint is identity-checked on both APIs before use, and requests are paced.
    #[must_use]
    pub(crate) fn new_tron(
        endpoints: Vec<(
            SensitiveUrl,
            Option<SensitiveUrl>,
            RpcTransport,
            RpcEndpointRoles,
        )>,
    ) -> Self {
        let tron = TronEndpoints(
            endpoints
                .iter()
                .map(|(json_rpc, http_api, _, _)| TronEndpoint {
                    json_rpc: json_rpc.clone(),
                    http_api: http_api.clone(),
                    next_request: Mutex::new(None),
                })
                .collect(),
        );
        let mut route = Self::new_with_transports(
            crate::tron::TRON_CHAIN_ID,
            endpoints
                .into_iter()
                .map(|(json_rpc, _, transport, roles)| (json_rpc, transport, roles))
                .collect(),
        )
        .with_identity_verification();
        route.tron = Some(Arc::new(tron));
        route
    }

    /// The HTTP API paired with the Tron JSON-RPC `endpoint`, if any.
    pub(crate) fn tron_http_api(&self, endpoint: &SensitiveUrl) -> Option<&SensitiveUrl> {
        self.tron
            .as_ref()?
            .get(endpoint.expose_url())?
            .http_api
            .as_ref()
    }

    /// Waits for the next request slot of a Tron `endpoint`; other routes never wait. Each call
    /// reserves its own slot, so concurrent requests are spaced too.
    pub(super) async fn pace(&self, endpoint: &Url) {
        let Some(slot) = self.tron.as_ref().and_then(|tron| tron.get(endpoint)) else {
            return;
        };
        let start = {
            let mut next = slot
                .next_request
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let start = next.map_or(now, |next| next.max(now));
            *next = Some(start + TRON_REQUEST_INTERVAL);
            start
        };
        time::sleep_until(start).await;
    }

    /// Checks that a Tron `endpoint`'s HTTP API, if any, reports mainnet's block 0. A different
    /// block id fails as `InvalidResponse`, like a wrong chain id; a failed request is retryable.
    pub(super) async fn check_tron_genesis(
        &self,
        client: &reqwest::Client,
        endpoint: &SensitiveUrl,
        timeout: Duration,
    ) -> Result<(), RpcBrokerError> {
        let Some(http_api) = self.tron_http_api(endpoint) else {
            return Ok(());
        };
        self.pace(endpoint.expose_url()).await;
        let block = post_http_api(
            client,
            http_api,
            "getblockbynum",
            &json!({"num": 0}),
            timeout,
        )
        .await
        .map_err(|_| RpcBrokerError::Transport)?;
        let id = block
            .get("blockID")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<B256>().ok())
            .ok_or(RpcBrokerError::Transport)?;
        if id == TRON_GENESIS_BLOCK_ID {
            Ok(())
        } else {
            Err(RpcBrokerError::InvalidResponse)
        }
    }
}

impl ClassRoute {
    /// POSTs `body` to `/wallet/{method}` of the HTTP API of this class's Tron endpoints, trying
    /// the health-eligible ones in the broker's candidate order until one answers. Only endpoints
    /// with an HTTP API that passed the identity check are used, each over its transport or this
    /// route's isolated client.
    pub(crate) async fn tron_http_api_call(
        &self,
        http: &HttpContext,
        method: &str,
        body: &Value,
    ) -> Result<Value, RpcBrokerError> {
        let route = self.route();
        let broker = http.rpc_broker();
        let clients = http.rpc_clients();
        let mut last_error = RpcBrokerError::NoEndpoint {
            chain_id: route.chain_id(),
        };
        for endpoint in broker.endpoint_candidates(route, None).await? {
            let Some(http_api) = route.tron_http_api(&endpoint) else {
                continue;
            };
            let label = route
                .endpoints()
                .iter()
                .position(|configured| *configured == endpoint)
                .expect("candidates belong to the route");
            if let Err(error) = route
                .verify_labeled_endpoint_identity(&clients, &endpoint, &label)
                .await
            {
                last_error = error;
                continue;
            }
            let url = endpoint.expose_url();
            route.pace(url).await;
            let result = post_http_api(
                route.read_client(&clients, url),
                http_api,
                method,
                body,
                DEFAULT_ATTEMPT_TIMEOUT,
            )
            .await;
            broker.report_endpoint_outcome(
                route.chain_id(),
                url,
                EndpointHealthOutcome::for_result(&result),
            );
            match result {
                Ok(value) => return Ok(value),
                Err(error) if should_failover_error(&error) => last_error = error,
                Err(error) => return Err(error),
            }
        }
        Err(last_error)
    }
}

/// One POST of `body` to `{http_api}/wallet/{method}`, with the broker's response cap. Tron's HTTP
/// API reports failures, including a keyless rate limit, as HTTP 200 with an `Error` member; such
/// a body fails as `InvalidResponse`, which fails over like a malformed JSON-RPC answer.
async fn post_http_api(
    client: &reqwest::Client,
    http_api: &SensitiveUrl,
    method: &str,
    body: &Value,
    timeout: Duration,
) -> Result<Value, RpcBrokerError> {
    let mut url = http_api.expose_url().clone();
    url.path_segments_mut()
        .map_err(|()| RpcBrokerError::Transport)?
        .pop_if_empty()
        .extend(["wallet", method]);
    let request = async {
        let mut response = send_json_post(client, url, body).await?;
        let bytes = read_capped_body(&mut response).await?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| RpcBrokerError::InvalidResponse)?;
        if value.get("Error").is_some() {
            return Err(RpcBrokerError::InvalidResponse);
        }
        Ok(value)
    };
    time::timeout(timeout, request)
        .await
        .map_err(|_| RpcBrokerError::Timeout)?
}

#[cfg(test)]
#[path = "tests/tron.rs"]
mod tests;
