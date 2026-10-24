//! Background identity warmup through the real broker and the installed routes' shared caches.

use super::candidates::ready;
use super::{RpcMockGate, spawn_rpc_mock_with_gate};
use crate::HttpContext;
use crate::rpc_broker::model::RpcChainRoute;
use crate::rpc_broker::{RpcBrokerError, RpcCandidates};
use crate::settings::{EffectiveChainRegistry, WalletSettings, build_effective_chain_configs};
use alloy::primitives::U64;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time;
use url::Url;

const CHAIN_ID: u64 = 999_999;
const WAIT: Duration = Duration::from_secs(5);
/// Time for a pass to send another check if it would retry or run twice.
const SETTLE: Duration = Duration::from_millis(200);

/// An endpoint that counts answered `eth_chainId` requests and fails them while `failing` is set.
/// With a gate, each response waits until the gate releases it.
async fn counted_endpoint(
    failing: Arc<AtomicBool>,
    gate: Option<RpcMockGate>,
) -> (Url, JoinHandle<()>, Arc<AtomicUsize>) {
    counted_chain_endpoint(CHAIN_ID, failing, gate).await
}

async fn counted_chain_endpoint(
    chain_id: u64,
    failing: Arc<AtomicBool>,
    gate: Option<RpcMockGate>,
) -> (Url, JoinHandle<()>, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&requests);
    let (endpoint, server) = spawn_rpc_mock_with_gate(
        Arc::new(move |request| {
            counted.fetch_add(1, Ordering::SeqCst);
            if failing.load(Ordering::SeqCst) {
                json!({"jsonrpc":"2.0", "id":request["id"], "error": {"code": -32000, "message": "unavailable"}})
            } else {
                json!({"jsonrpc":"2.0", "id":request["id"], "result": U64::from(chain_id)})
            }
        }),
        Arc::default(),
        Arc::default(),
        gate,
    )
    .await;
    (endpoint, server, requests)
}

/// Releases every held response, including later ones, until aborted, so each request that
/// reached the endpoint is answered and counted.
fn release_continuously(gate: &RpcMockGate) -> JoinHandle<()> {
    let release = Arc::clone(&gate.release_response);
    tokio::spawn(async move {
        loop {
            release.notify_one();
            time::sleep(Duration::from_millis(5)).await;
        }
    })
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    time::timeout(WAIT, async {
        while !condition() {
            time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

#[tokio::test]
async fn activating_routes_preserves_inflight_checks_and_wakes_an_idle_pass_once() {
    let held = RpcMockGate::default();
    let (initial, initial_server, initial_requests) =
        counted_endpoint(Arc::default(), Some(held.clone())).await;
    let (second, second_server, second_requests) =
        counted_chain_endpoint(CHAIN_ID + 1, Arc::default(), None).await;
    let (later, later_server, later_requests) =
        counted_chain_endpoint(CHAIN_ID + 2, Arc::default(), None).await;
    // Trusted routes send real responsiveness probes even after a previous success.
    let initial = RpcChainRoute::new(CHAIN_ID, vec![initial]);
    let second = RpcChainRoute::new(CHAIN_ID + 1, vec![second]);
    let later = RpcChainRoute::new(CHAIN_ID + 2, vec![later]);
    let http = HttpContext::direct_for_tests();
    let warmup = http
        .rpc_broker()
        .warm_identities(vec![initial.clone()], None);
    time::timeout(WAIT, held.request_started.notified())
        .await
        .expect("the initial check starts");
    time::sleep(SETTLE).await;
    assert_eq!(second_requests.load(Ordering::SeqCst), 0);
    assert_eq!(later_requests.load(Ordering::SeqCst), 0);

    warmup.activate_route(second.clone());
    wait_until(
        "the added route warms while the first check is held",
        || second_requests.load(Ordering::SeqCst) == 1,
    )
    .await;
    assert_eq!(initial_requests.load(Ordering::SeqCst), 0);

    // Joining by the original registration still retains dynamically activated routes.
    let joined = http
        .rpc_broker()
        .warm_identities(vec![initial.clone()], None);
    drop(warmup);
    let releaser = release_continuously(&held);
    wait_until("the initial check completes without restarting", || {
        initial_requests.load(Ordering::SeqCst) == 1
    })
    .await;
    time::sleep(SETTLE).await;
    assert_eq!(later_requests.load(Ordering::SeqCst), 0);

    joined.activate_route(later.clone());
    wait_until("activation wakes the idle pass", || {
        later_requests.load(Ordering::SeqCst) == 1
    })
    .await;
    for route in [initial, second, later] {
        joined.activate_route(route);
    }
    time::sleep(SETTLE).await;
    assert_eq!(initial_requests.load(Ordering::SeqCst), 1);
    assert_eq!(second_requests.load(Ordering::SeqCst), 1);
    assert_eq!(later_requests.load(Ordering::SeqCst), 1);

    drop(joined);
    releaser.abort();
    initial_server.abort();
    second_server.abort();
    later_server.abort();
}

#[tokio::test]
async fn failed_warmup_recovers_without_foreground_requests_and_stops_with_its_owner() {
    // Custom identities and trusted-endpoint responsiveness use different probe paths.
    for verify_identity in [true, false] {
        check_failed_warmup_recovery(verify_identity).await;
    }
}

async fn advance_backoff(duration: Duration) {
    time::pause();
    time::advance(duration).await;
    // Let real socket I/O settle without automatically jumping to a probe timeout.
    time::resume();
}

async fn check_failed_warmup_recovery(verify_identity: bool) {
    use crate::rpc_broker::model::HEALTH_WITHDRAWAL_BASE;

    let failing = Arc::new(AtomicBool::new(true));
    let held = RpcMockGate::default();
    let (endpoint, server, requests) =
        counted_endpoint(Arc::clone(&failing), Some(held.clone())).await;
    let (offline, offline_server, offline_requests) =
        counted_endpoint(Arc::new(AtomicBool::new(true)), None).await;
    let http = HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, vec![endpoint, offline]);
    let route = if verify_identity {
        route.with_identity_verification()
    } else {
        route
    };
    let warmup = http.rpc_broker().warm_identities(vec![route.clone()], None);
    time::timeout(WAIT, held.request_started.notified())
        .await
        .expect("initial probe starts");
    held.release_response.notify_one();

    // Even when every endpoint failed, fee refreshes must not restart discovery.
    time::timeout(WAIT, async {
        loop {
            if matches!(
                http.rpc_broker().endpoint_candidates(&route, None).await,
                Err(RpcBrokerError::NoEndpoint { .. })
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed warmup excludes the only endpoint from snapshots");
    for _ in 0..3 {
        assert!(matches!(
            RpcCandidates::acquire(&http, &super::general(&route), None).await,
            Err(RpcBrokerError::NoEndpoint { .. })
        ));
    }
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    advance_backoff(HEALTH_WITHDRAWAL_BASE).await;
    time::timeout(WAIT, held.request_started.notified())
        .await
        .expect("background recovery starts without a foreground read");
    // Expired cooldown is not proof of recovery, even for a trusted endpoint.
    assert!(matches!(
        RpcCandidates::acquire(&http, &super::general(&route), None).await,
        Err(RpcBrokerError::NoEndpoint { .. })
    ));
    held.release_response.notify_one();
    wait_until("the recovery probe fails", || {
        requests.load(Ordering::SeqCst) == 2
    })
    .await;
    time::sleep(SETTLE).await;

    advance_backoff(HEALTH_WITHDRAWAL_BASE).await;
    assert!(
        time::timeout(SETTLE, held.request_started.notified())
            .await
            .is_err(),
        "the second failure must increase the retry delay"
    );
    advance_backoff(HEALTH_WITHDRAWAL_BASE).await;
    time::timeout(WAIT, held.request_started.notified())
        .await
        .expect("a later background retry starts");
    failing.store(false, Ordering::SeqCst);
    held.release_response.notify_one();
    time::timeout(WAIT, async {
        loop {
            if let Ok(snapshot) = http.rpc_broker().endpoint_candidates(&route, None).await {
                assert_eq!(snapshot, vec![route.endpoints()[0].clone()]);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("successful background recovery restores the endpoint");
    let candidates = RpcCandidates::acquire(&http, &super::general(&route), None)
        .await
        .expect("a new fee refresh uses the recovered endpoint");
    assert_eq!(ready(&candidates), vec![0]);
    assert!(candidates.subscribe().changed().await.is_err());
    assert_eq!(requests.load(Ordering::SeqCst), 3);

    // The still-offline endpoint must not keep retrying after installation retirement.
    drop(warmup);
    time::sleep(SETTLE).await;
    let offline_attempts = offline_requests.load(Ordering::SeqCst);
    advance_backoff(HEALTH_WITHDRAWAL_BASE * 20).await;
    time::sleep(SETTLE).await;
    assert_eq!(offline_requests.load(Ordering::SeqCst), offline_attempts);
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(
        http.rpc_broker()
            .endpoint_candidates(&route, None)
            .await
            .expect("retirement leaves no exclusion without a recovery owner")
            .len(),
        2
    );
    server.abort();
    offline_server.abort();
}

#[tokio::test]
async fn warmup_outlives_foreground_sources_and_checks_each_endpoint_once() {
    let (cached, cached_server, cached_requests) = counted_endpoint(Arc::default(), None).await;
    let held_gate = RpcMockGate::default();
    let (held, held_server, held_requests) =
        counted_endpoint(Arc::default(), Some(held_gate.clone())).await;
    let (failed, failed_server, failed_requests) =
        counted_endpoint(Arc::new(AtomicBool::new(true)), None).await;
    let http = HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, vec![cached.clone(), held, failed])
        .with_identity_verification();
    route
        .for_endpoint(cached.into())
        .verify_identity(&http.rpc_clients())
        .await
        .expect("a clone verifies the cached endpoint");

    // Registering the same route objects while their check is in flight joins that pass, so
    // dropping the first handle neither cancels nor restarts it.
    let first = http.rpc_broker().warm_identities(vec![route.clone()], None);
    time::timeout(WAIT, held_gate.request_started.notified())
        .await
        .expect("warmup checks the held endpoint");
    let warmup = http.rpc_broker().warm_identities(vec![route.clone()], None);
    drop(first);
    wait_until("warmup checks the failing endpoint", || {
        failed_requests.load(Ordering::SeqCst) == 1
    })
    .await;
    time::sleep(SETTLE).await;
    assert_eq!(failed_requests.load(Ordering::SeqCst), 1);

    // A foreground source uses the cached endpoint at once and only waits on the held check,
    // then ends with the source. That one failed check already withdrew the failing endpoint, so
    // the fresh snapshot leaves it out and nothing checks it again.
    let foreground = RpcCandidates::acquire(&http, &super::general(&route), None)
        .await
        .expect("candidates");
    assert_eq!(foreground.len(), 2);
    assert_eq!(ready(&foreground), vec![0]);
    drop(foreground);

    let releaser = release_continuously(&held_gate);
    wait_until("warmup verifies the held endpoint", || {
        route.endpoint_identity_verified(&route.endpoints()[1])
    })
    .await;
    time::sleep(SETTLE).await;
    assert_eq!(held_requests.load(Ordering::SeqCst), 1);

    // A later source reuses both checks without another request.
    let later = RpcCandidates::acquire(&http, &super::general(&route), None)
        .await
        .expect("candidates");
    assert_eq!(later.len(), 2);
    assert_eq!(ready(&later), vec![0, 1]);
    assert_eq!(held_requests.load(Ordering::SeqCst), 1);
    assert_eq!(failed_requests.load(Ordering::SeqCst), 1);
    assert_eq!(cached_requests.load(Ordering::SeqCst), 1);
    drop(warmup);
    releaser.abort();
    cached_server.abort();
    held_server.abort();
    failed_server.abort();
}

#[tokio::test]
async fn replacement_warms_rebuilt_routes_without_retiring_live_consumers() {
    let held_gate = RpcMockGate::default();
    let (held, held_server, held_requests) =
        counted_endpoint(Arc::default(), Some(held_gate.clone())).await;
    let http = HttpContext::direct_for_tests();
    let outgoing = RpcChainRoute::new(CHAIN_ID, vec![held]).with_identity_verification();
    let outgoing_warmup = http
        .rpc_broker()
        .warm_identities(vec![outgoing.clone()], None);
    time::timeout(WAIT, held_gate.request_started.notified())
        .await
        .expect("warmup checks the endpoint");
    // A live source on the outgoing route waits on the same check.
    let consumer = RpcCandidates::acquire(&http, &super::general(&outgoing), None)
        .await
        .expect("candidates");
    let consumer =
        tokio::spawn(async move { consumer.random_ready().await.map(|provider| provider.index) });

    // Settings rebuild the route from the same URL, with a fresh cache.
    let replacement =
        RpcChainRoute::new(CHAIN_ID, outgoing.endpoint_urls()).with_identity_verification();
    let replacement_warmup = http
        .rpc_broker()
        .warm_identities(vec![replacement.clone()], None);
    let releaser = release_continuously(&held_gate);

    assert_eq!(
        time::timeout(WAIT, consumer)
            .await
            .expect("the live consumer outlasts the replaced pass")
            .expect("consumer task"),
        Some(0)
    );
    wait_until("the replacement pass verifies its own route", || {
        replacement.endpoint_identity_verified(&replacement.endpoints()[0])
    })
    .await;
    // Each cache needed an answered check of its own.
    assert!(held_requests.load(Ordering::SeqCst) >= 2);
    drop((outgoing_warmup, replacement_warmup));
    releaser.abort();
    held_server.abort();
}

#[tokio::test]
async fn installed_registry_warms_privacy_and_selected_chains_and_defers_unused_public_chains() {
    let defaults =
        build_effective_chain_configs(&WalletSettings::default()).expect("default chains");
    let mut endpoints = Vec::new();
    let mut chains = Vec::new();
    // An unselected privacy chain, selected Public-only chain, unused Public-only chain,
    // and disabled chain exercise the startup policy against actual identity requests.
    for index in 0..4 {
        let chain_id = CHAIN_ID + index;
        let (endpoint, server, requests) =
            counted_chain_endpoint(chain_id, Arc::default(), None).await;
        let mut chain = defaults.get(1).expect("default chain").clone();
        chain.chain_id = chain_id;
        chain.enabled = index != 3;
        if index != 0 {
            chain.railgun = None;
        }
        chain.rpc_route = RpcChainRoute::new(chain_id, vec![endpoint]).with_identity_verification();
        chains.push(chain);
        endpoints.push((server, requests));
    }
    let registry: EffectiveChainRegistry = chains.into_iter().collect();
    let http = HttpContext::direct_for_tests();
    let warmup = crate::start_rpc_identity_warmup(&http, &registry, CHAIN_ID + 1);

    wait_until(
        "privacy and selected-chain checks reach their shared caches",
        || {
            [CHAIN_ID, CHAIN_ID + 1].into_iter().all(|chain_id| {
                let route = &registry.get(chain_id).expect("startup chain").rpc_route;
                route.endpoint_identity_verified(&route.endpoints()[0])
            })
        },
    )
    .await;
    time::sleep(SETTLE).await;
    let request_counts = || {
        endpoints
            .iter()
            .map(|(_, requests)| requests.load(Ordering::SeqCst))
            .collect::<Vec<_>>()
    };
    assert_eq!(request_counts(), vec![1, 1, 0, 0]);

    let unused = &registry.get(CHAIN_ID + 2).expect("unused chain").rpc_route;
    warmup.activate_route(unused.clone());
    wait_until("the newly used Public-only chain verifies", || {
        unused.endpoint_identity_verified(&unused.endpoints()[0])
    })
    .await;
    assert_eq!(request_counts(), vec![1, 1, 1, 0]);
    drop(warmup);
    for (server, _) in endpoints {
        server.abort();
    }
}

#[tokio::test]
async fn warmup_prioritizes_selected_chain_and_reorders_only_queued_checks() {
    let mut routes = Vec::new();
    let mut endpoints = Vec::new();
    for (chain_id, endpoint_count) in [(CHAIN_ID, 5), (CHAIN_ID + 1, 2)] {
        let mut urls = Vec::new();
        for _ in 0..endpoint_count {
            let gate = RpcMockGate::default();
            let active = Arc::new(AtomicUsize::new(0));
            let answered = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&answered);
            let (url, server) = spawn_rpc_mock_with_gate(
                Arc::new(move |request| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    json!({"jsonrpc":"2.0", "id":request["id"], "result": U64::from(chain_id)})
                }),
                Arc::clone(&active),
                Arc::default(),
                Some(gate.clone()),
            )
            .await;
            urls.push(url);
            endpoints.push((gate, active, answered, server));
        }
        routes.push(RpcChainRoute::new(chain_id, urls).with_identity_verification());
    }
    let http = HttpContext::direct_for_tests();
    // Put the other chain first in the base order so startup preference is deterministic.
    let broker = crate::rpc_broker::RpcBroker::spawn_with_candidate_order(
        http.rpc_client.clone(),
        http.direct_rpc_client.clone(),
        <[usize]>::reverse,
    )
    .expect("broker");
    let first = broker.warm_identities(routes.clone(), Some(CHAIN_ID));
    wait_until("four selected-chain checks start", || {
        endpoints[..5]
            .iter()
            .map(|(_, active, _, _)| active.load(Ordering::SeqCst))
            .sum::<usize>()
            == 4
    })
    .await;
    assert!(
        endpoints[5..]
            .iter()
            .all(|(_, active, _, _)| active.load(Ordering::SeqCst) == 0)
    );
    let held = endpoints[..5]
        .iter()
        .position(|(_, active, _, _)| active.load(Ordering::SeqCst) == 1)
        .expect("held check");
    let queued = endpoints[..5]
        .iter()
        .position(|(_, active, _, _)| active.load(Ordering::SeqCst) == 0)
        .expect("queued check");

    // Joining the same installation changes its preference without cancelling any sent check.
    let warmup = broker.warm_identities(routes.clone(), Some(CHAIN_ID + 1));
    drop(first);
    endpoints[held].0.release_response.notify_one();
    wait_until("newly selected chain gets the next free slot", || {
        endpoints[5..]
            .iter()
            .any(|(_, active, _, _)| active.load(Ordering::SeqCst) == 1)
    })
    .await;
    assert_eq!(endpoints[queued].1.load(Ordering::SeqCst), 0);
    for (index, (_, active, _, _)) in endpoints[..5].iter().enumerate() {
        if index != held && index != queued {
            assert_eq!(
                active.load(Ordering::SeqCst),
                1,
                "in-flight checks remain running"
            );
        }
    }

    // A later UI chain switch changes the same pass again. Completed checks are not repeated.
    warmup.prioritize_chain(CHAIN_ID);
    let second = endpoints[5..]
        .iter()
        .position(|(_, active, _, _)| active.load(Ordering::SeqCst) == 1)
        .expect("new chain check")
        + 5;
    let still_queued = endpoints[5..]
        .iter()
        .position(|(_, active, _, _)| active.load(Ordering::SeqCst) == 0)
        .expect("queued new-chain check")
        + 5;
    endpoints[second].0.release_response.notify_one();
    wait_until("the original chain's queued check starts", || {
        endpoints[queued].1.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(
        endpoints[still_queued].1.load(Ordering::SeqCst),
        0,
        "the updated preference precedes the other queued chain"
    );
    let releasers = endpoints
        .iter()
        .map(|(gate, _, _, _)| release_continuously(gate))
        .collect::<Vec<_>>();
    wait_until("all queued endpoints verify", || {
        routes.iter().all(|route| {
            route
                .endpoints()
                .iter()
                .all(|endpoint| route.endpoint_identity_verified(endpoint))
        })
    })
    .await;
    time::sleep(SETTLE).await;
    assert!(
        endpoints
            .iter()
            .all(|(_, _, answered, _)| answered.load(Ordering::SeqCst) == 1)
    );
    drop(warmup);
    for releaser in releasers {
        releaser.abort();
    }
    for (_, _, _, server) in endpoints {
        server.abort();
    }
}
