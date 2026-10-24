use super::{RpcMockGate, spawn_gated_rpc_mock, spawn_rpc_mock};
use crate::HttpContext;
use crate::rpc_broker::RpcCandidates;
use crate::rpc_broker::identity::MAX_IDENTITY_PROBES;
use crate::rpc_broker::model::RpcChainRoute;
use alloy::primitives::U64;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time;
use url::Url;

const CHAIN_ID: u64 = 999_999;
const WAIT: Duration = Duration::from_secs(5);

/// An endpoint that counts `eth_chainId` requests and answers `chain_id`, or fails for `None`.
async fn counted_endpoint(chain_id: Option<u64>) -> (Url, JoinHandle<()>, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let (endpoint, server) = spawn_rpc_mock(
        Arc::new(move |request| {
            counted.fetch_add(1, Ordering::SeqCst);
            match chain_id {
                Some(chain_id) => {
                    json!({"jsonrpc":"2.0", "id":request["id"], "result": U64::from(chain_id)})
                }
                None => {
                    json!({"jsonrpc":"2.0", "id":request["id"], "error": {"code": -32000, "message": "unavailable"}})
                }
            }
        }),
        Arc::default(),
        Arc::default(),
    )
    .await;
    (endpoint, server, requests)
}

/// A valid endpoint that answers only once the returned gate is released.
async fn held_endpoint() -> (Url, JoinHandle<()>, RpcMockGate) {
    let gate = RpcMockGate::default();
    let (endpoint, server) = spawn_gated_rpc_mock(
        Arc::new(
            |request| json!({"jsonrpc":"2.0", "id":request["id"], "result": U64::from(CHAIN_ID)}),
        ),
        Arc::default(),
        Arc::default(),
        gate.clone(),
    )
    .await;
    (endpoint, server, gate)
}

pub(super) fn ready(candidates: &RpcCandidates) -> Vec<usize> {
    candidates
        .pool()
        .available_providers()
        .iter()
        .map(|provider| provider.index)
        .collect()
}

#[tokio::test]
async fn cached_and_gateway_candidates_are_ready_without_new_probes() {
    let (cached, cached_server, cached_requests) = counted_endpoint(Some(CHAIN_ID)).await;
    let (held, held_server, held_gate) = held_endpoint().await;
    let http = HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, vec![cached.clone(), held.clone()])
        .with_identity_verification();
    route
        .for_endpoint(cached.clone().into())
        .verify_identity(&http.rpc_clients())
        .await
        .expect("a clone verifies the cached endpoint");

    // The cached endpoint is ready at once while the other endpoint's probe is held.
    let candidates = RpcCandidates::acquire(&http, &super::general(&route), None)
        .await
        .expect("candidates");
    assert_eq!(ready(&candidates), vec![0]);
    assert_eq!(candidates.len(), 2);
    time::timeout(WAIT, held_gate.request_started.notified())
        .await
        .expect("the unverified endpoint is probed");
    assert_eq!(cached_requests.load(Ordering::SeqCst), 1);

    // Gateway reads verify per read, so even an unverified configuration is ready unprobed.
    let unverified = RpcChainRoute::new(CHAIN_ID, vec![cached, held]).with_identity_verification();
    let gateway = RpcCandidates::acquire_for_gateway(&http, &super::general(&unverified), None)
        .await
        .expect("gateway candidates");
    assert_eq!(ready(&gateway), vec![0, 1]);
    assert!(gateway.subscribe().changed().await.is_err());
    assert_eq!(cached_requests.load(Ordering::SeqCst), 1);
    drop(candidates);
    cached_server.abort();
    held_server.abort();
}

#[tokio::test]
async fn discovery_probes_each_endpoint_once_and_ends_after_failures() {
    let (wrong, wrong_server, wrong_requests) = counted_endpoint(Some(1)).await;
    let (failing, failing_server, failing_requests) = counted_endpoint(None).await;
    let (held, held_server, gate) = held_endpoint().await;
    let http = HttpContext::direct_for_tests();
    let route =
        RpcChainRoute::new(CHAIN_ID, vec![wrong, failing, held]).with_identity_verification();
    let candidates = RpcCandidates::acquire(&http, &super::general(&route), None)
        .await
        .expect("candidates");

    // A waiter parked before the valid endpoint's admission is woken by it.
    let waiter = tokio::spawn({
        let candidates = candidates.clone();
        async move {
            candidates
                .random_ready()
                .await
                .map(|provider| provider.index)
        }
    });
    time::timeout(WAIT, gate.request_started.notified())
        .await
        .expect("the valid endpoint is probed");
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    gate.release_response.notify_one();
    assert_eq!(
        time::timeout(WAIT, waiter)
            .await
            .expect("the admission wakes the waiter")
            .expect("waiter task"),
        Some(2)
    );

    let mut tried = HashSet::new();
    let mut served = Vec::new();
    while let Some(provider) = time::timeout(WAIT, candidates.next_untried(&mut tried))
        .await
        .expect("the search ends once discovery does")
    {
        served.push(provider.index);
    }
    // The wrong chain and the failed check never become ready, and no endpoint is retried.
    assert_eq!(served, vec![2]);
    for requests in [&wrong_requests, &failing_requests] {
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
    assert!(
        time::timeout(Duration::from_millis(50), gate.request_started.notified())
            .await
            .is_err()
    );

    // Once the caller excludes the only ready endpoint, nothing more can arrive.
    let provider = candidates
        .random_ready()
        .await
        .expect("the valid endpoint is ready");
    candidates.pool().mark_bad_provider(&provider);
    assert!(
        time::timeout(WAIT, candidates.random_ready())
            .await
            .expect("finished discovery does not wait")
            .is_none()
    );
    wrong_server.abort();
    failing_server.abort();
    held_server.abort();
}

#[tokio::test]
async fn dropping_the_last_candidates_handle_stops_discovery() {
    let mut endpoints = Vec::new();
    let mut servers = Vec::new();
    let mut gates = Vec::new();
    for _ in 0..MAX_IDENTITY_PROBES {
        let (endpoint, server, gate) = held_endpoint().await;
        endpoints.push(endpoint);
        servers.push(server);
        gates.push(gate);
    }
    let (queued, queued_server, queued_requests) = counted_endpoint(Some(CHAIN_ID)).await;
    endpoints.push(queued);
    let http = HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, endpoints).with_identity_verification();
    let candidates = RpcCandidates::acquire(&http, &super::general(&route), None)
        .await
        .expect("candidates");
    let clone = candidates.clone();
    for gate in &gates {
        time::timeout(WAIT, gate.request_started.notified())
            .await
            .expect("held endpoints fill every probe slot");
    }

    // The last endpoint is still queued; a surviving clone keeps discovery alive until dropped.
    drop(candidates);
    assert!(
        time::timeout(Duration::from_millis(50), clone.subscribe().changed())
            .await
            .is_err()
    );
    drop(clone);
    for gate in &gates {
        gate.release_response.notify_one();
    }
    time::sleep(Duration::from_millis(200)).await;
    assert_eq!(queued_requests.load(Ordering::SeqCst), 0);
    for server in servers {
        server.abort();
    }
    queued_server.abort();
}
