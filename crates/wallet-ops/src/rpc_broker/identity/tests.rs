use super::super::tests::{
    RecordedRequests, RpcMockGate, RpcResponder, spawn_gated_rpc_mock, spawn_recording_rpc_mock,
    spawn_rpc_mock, tagged_reqwest_client,
};
use super::*;
use alloy::primitives::U64;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use tokio::time::Instant;

const CHAIN_ID: u64 = 999_999;
const FAST_BACKOFF: AdmissionBackoff = AdmissionBackoff {
    initial: Duration::from_millis(20),
    max: Duration::from_millis(40),
};
/// Several `FAST_BACKOFF` retry periods.
const RETRY_WINDOW: Duration = Duration::from_millis(300);

fn chain_id_responder(chain_id: u64) -> RpcResponder {
    Arc::new(
        move |request| json!({"jsonrpc":"2.0", "id":request["id"], "result": U64::from(chain_id)}),
    )
}

/// An endpoint that answers `chain_id` only once the returned gate is released.
async fn held_chain_id_mock(chain_id: u64) -> (url::Url, tokio::task::JoinHandle<()>, RpcMockGate) {
    let gate = RpcMockGate {
        request_started: Arc::default(),
        release_response: Arc::default(),
    };
    let (endpoint, server) = spawn_gated_rpc_mock(
        chain_id_responder(chain_id),
        Arc::default(),
        Arc::default(),
        gate.clone(),
    )
    .await;
    (endpoint, server, gate)
}

/// An endpoint that counts `chain_id` requests and fails them while `failing` is set.
async fn counted_chain_id_mock(
    chain_id: u64,
    failing: Arc<AtomicBool>,
) -> (url::Url, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let (endpoint, server) = spawn_rpc_mock(
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
    )
    .await;
    (endpoint, server, requests)
}

fn pending_pool(route: &RpcChainRoute, clients: &RpcClients, archive: bool) -> Arc<QueryRpcPool> {
    let pool = route
        .query_rpc_pool(clients, Duration::from_secs(5))
        .with_pending_admission();
    Arc::new(if archive {
        pool.with_pending_archive()
    } else {
        pool
    })
}

fn available(pool: &QueryRpcPool) -> Vec<usize> {
    pool.available_providers()
        .iter()
        .map(|provider| provider.index)
        .collect()
}

async fn eventually(mut reached: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reached() {
        assert!(
            Instant::now() < deadline,
            "admission state not reached in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn identity_checks_run_concurrently_and_keep_configured_order() {
    let (slow, slow_server, slow_gate) = held_chain_id_mock(CHAIN_ID).await;
    let (stalled, stalled_server, stalled_gate) = held_chain_id_mock(CHAIN_ID).await;
    let (fast, fast_server) =
        spawn_rpc_mock(chain_id_responder(CHAIN_ID), Arc::default(), Arc::default()).await;
    let http = crate::HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, vec![slow.clone(), stalled, fast.clone()])
        .with_identity_verification();

    let started = Instant::now();
    // Both held requests are in flight only when checks run concurrently; sequential
    // checks would time out the slow endpoint before the stalled one is asked.
    tokio::spawn(async move {
        slow_gate.request_started.notified().await;
        stalled_gate.request_started.notified().await;
        slow_gate.release_response.notify_one();
        // Let the slow answer land in real time, then skip ahead to the stalled timeout.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::time::pause();
    });
    let verified = route
        .verify_identity(&http.rpc_clients())
        .await
        .expect("slow and fast endpoints verify");

    assert!(started.elapsed() <= IDENTITY_TIMEOUT + Duration::from_secs(1));
    assert_eq!(verified.endpoint_urls(), vec![slow, fast]);
    slow_server.abort();
    stalled_server.abort();
    fast_server.abort();
}

#[tokio::test]
async fn sync_pool_starts_on_first_admission_and_admits_the_rest_later() {
    let (held, held_server, held_gate) = held_chain_id_mock(CHAIN_ID).await;
    let (fast, fast_server) =
        spawn_rpc_mock(chain_id_responder(CHAIN_ID), Arc::default(), Arc::default()).await;
    let (archive, archive_server, archive_gate) = held_chain_id_mock(CHAIN_ID).await;
    let http = crate::HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, vec![held, fast]).with_identity_verification();
    let pool = pending_pool(&route, &http.rpc_clients(), true);

    // The held endpoint and archive answer only after the gates open below.
    tokio::time::timeout(
        Duration::from_secs(5),
        route.admit_sync_pool(
            &http.rpc_clients(),
            &pool,
            Some((archive.into(), RpcTransport::Wallet)),
        ),
    )
    .await
    .expect("admission does not wait for held endpoints")
    .expect("the fast endpoint admits the pool");
    assert_eq!(pool.provider_admission(0), Some(RpcAdmission::Pending));
    assert_eq!(pool.provider_admission(1), Some(RpcAdmission::Admitted));
    assert_eq!(available(&pool), vec![1]);
    assert!(!pool.archive_admitted());

    tokio::time::timeout(Duration::from_secs(5), async {
        held_gate.request_started.notified().await;
        archive_gate.request_started.notified().await;
    })
    .await
    .expect("held endpoint and archive are probed");
    held_gate.release_response.notify_one();
    archive_gate.release_response.notify_one();
    eventually(|| available(&pool) == vec![0, 1] && pool.archive_admitted()).await;
    held_server.abort();
    fast_server.abort();
    archive_server.abort();
}

#[tokio::test]
async fn wrong_chain_endpoints_are_excluded_without_retry() {
    let (wrong, wrong_server, wrong_requests) = counted_chain_id_mock(1, Arc::default()).await;
    let (good, good_server) =
        spawn_rpc_mock(chain_id_responder(CHAIN_ID), Arc::default(), Arc::default()).await;
    let (wrong_archive, wrong_archive_server, wrong_archive_requests) =
        counted_chain_id_mock(1, Arc::default()).await;
    let http = crate::HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, vec![wrong, good]).with_identity_verification();
    let pool = pending_pool(&route, &http.rpc_clients(), true);

    route
        .admit_pool(
            &http.rpc_clients(),
            &pool,
            Some((wrong_archive.into(), RpcTransport::Wallet)),
            FAST_BACKOFF,
            |_| {},
        )
        .await
        .expect("the good endpoint admits the pool");
    eventually(|| {
        pool.provider_admission(0) == Some(RpcAdmission::Excluded)
            && wrong_archive_requests.load(Ordering::SeqCst) == 1
    })
    .await;
    tokio::time::sleep(RETRY_WINDOW).await;

    assert_eq!(wrong_requests.load(Ordering::SeqCst), 1);
    assert_eq!(wrong_archive_requests.load(Ordering::SeqCst), 1);
    assert_eq!(pool.provider_admission(0), Some(RpcAdmission::Excluded));
    assert_eq!(available(&pool), vec![1]);
    assert!(!pool.archive_admitted());
    wrong_server.abort();
    good_server.abort();
    wrong_archive_server.abort();
}

#[tokio::test]
async fn sync_pool_fails_when_every_regular_first_check_fails() {
    let (wrong, wrong_server, _) = counted_chain_id_mock(1, Arc::default()).await;
    let refused = {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind unused port");
        let address = listener.local_addr().expect("unused port address");
        url::Url::parse(&format!("http://{address}")).expect("refused endpoint URL")
    };
    let http = crate::HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, vec![wrong, refused]).with_identity_verification();
    let pool = pending_pool(&route, &http.rpc_clients(), false);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        route.admit_sync_pool(&http.rpc_clients(), &pool, None),
    )
    .await
    .expect("admission reports after the first failed checks");
    assert!(matches!(
        result,
        Err(RpcBrokerError::NoEndpoint { chain_id: CHAIN_ID })
    ));
    assert_eq!(available(&pool), [] as [usize; 0]);
    wrong_server.abort();
}

#[tokio::test]
async fn initial_probe_order_fills_slots_and_keeps_configured_indices() {
    let mut endpoints = Vec::new();
    let mut held_servers = Vec::new();
    for _ in 0..MAX_IDENTITY_PROBES {
        let (endpoint, server, _) = held_chain_id_mock(CHAIN_ID).await;
        endpoints.push(endpoint);
        held_servers.push(server);
    }
    let (fast, fast_server) =
        spawn_rpc_mock(chain_id_responder(CHAIN_ID), Arc::default(), Arc::default()).await;
    endpoints.push(fast);
    let http = crate::HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(CHAIN_ID, endpoints).with_identity_verification();
    let pool = pending_pool(&route, &http.rpc_clients(), false);

    // In configured order the held prefix would occupy every probe slot until it times out.
    tokio::time::timeout(
        Duration::from_secs(5),
        route.admit_pool(&http.rpc_clients(), &pool, None, FAST_BACKOFF, |indices| {
            indices.reverse();
        }),
    )
    .await
    .expect("the reordered fast endpoint is probed in the first window")
    .expect("the fast endpoint admits the pool");
    assert_eq!(available(&pool), vec![MAX_IDENTITY_PROBES]);
    for server in held_servers {
        server.abort();
    }
    fast_server.abort();
}

#[tokio::test]
async fn failed_checks_retry_until_admitted_and_stop_with_the_pool() {
    let flaky_failing = Arc::new(AtomicBool::new(true));
    let (flaky, flaky_server, flaky_requests) =
        counted_chain_id_mock(CHAIN_ID, flaky_failing.clone()).await;
    let (good, good_server) =
        spawn_rpc_mock(chain_id_responder(CHAIN_ID), Arc::default(), Arc::default()).await;
    let (broken, broken_server, broken_requests) =
        counted_chain_id_mock(CHAIN_ID, Arc::new(AtomicBool::new(true))).await;
    let http = crate::HttpContext::direct_for_tests();
    let route =
        RpcChainRoute::new(CHAIN_ID, vec![flaky, good, broken]).with_identity_verification();
    let pool = pending_pool(&route, &http.rpc_clients(), false);

    route
        .admit_pool(&http.rpc_clients(), &pool, None, FAST_BACKOFF, |_| {})
        .await
        .expect("the good endpoint admits the pool");
    // A second request proves the failed check is retried; failures keep it pending.
    eventually(|| flaky_requests.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(pool.provider_admission(0), Some(RpcAdmission::Pending));
    flaky_failing.store(false, Ordering::SeqCst);
    eventually(|| available(&pool) == vec![0, 1]).await;
    eventually(|| broken_requests.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(pool.provider_admission(2), Some(RpcAdmission::Pending));

    // Dropping the pool ends the session, so the broken endpoint is no longer probed.
    drop(pool);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after_drop = broken_requests.load(Ordering::SeqCst);
    tokio::time::sleep(RETRY_WINDOW).await;
    assert_eq!(broken_requests.load(Ordering::SeqCst), after_drop);
    flaky_server.abort();
    good_server.abort();
    broken_server.abort();
}

#[tokio::test]
async fn endpoint_identity_does_not_transfer_to_replacement_configuration() {
    let returned_id = Arc::new(AtomicU64::new(1));
    let observed = returned_id.clone();
    let (endpoint, server) = super::super::tests::spawn_rpc_mock(
        Arc::new(move |request| {
            assert_eq!(request["method"], "eth_chainId");
            json!({"jsonrpc":"2.0", "id":request["id"], "result": U64::from(observed.load(Ordering::SeqCst))})
        }), Arc::default(), Arc::default(),
    ).await;
    let http = crate::HttpContext::direct_for_tests();
    let route = RpcChainRoute::new(999_999, vec![endpoint.clone()]).with_identity_verification();
    assert!(route.verify_identity(&http.rpc_clients()).await.is_err());
    returned_id.store(999_999, Ordering::SeqCst);
    let equivalent =
        RpcChainRoute::new(999_999, route.endpoint_urls()).with_identity_verification();
    let fingerprint = |route: &RpcChainRoute| {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        route.hash(&mut hasher);
        hasher.finish()
    };
    let before_verification = fingerprint(&route);
    assert_eq!(
        route
            .verify_identity(&http.rpc_clients())
            .await
            .unwrap()
            .endpoint_urls(),
        vec![endpoint]
    );
    let (replacement, replacement_server) = super::super::tests::spawn_rpc_mock(
        Arc::new(|request| json!({"jsonrpc":"2.0", "id":request["id"], "result":"0x1"})),
        Arc::default(),
        Arc::default(),
    )
    .await;
    let replacement = RpcChainRoute::new(999_999, vec![replacement]).with_identity_verification();
    assert!(
        replacement
            .verify_identity(&http.rpc_clients())
            .await
            .is_err()
    );
    // Populating a cache must not change broker grouping or authority equality.
    assert_eq!(route, equivalent);
    assert_eq!(fingerprint(&route), before_verification);
    assert_eq!(fingerprint(&equivalent), before_verification);
    // A rebuilt configuration must verify even a formerly correct endpoint.
    returned_id.store(1, Ordering::SeqCst);
    assert!(
        route
            .clone()
            .verify_identity(&http.rpc_clients())
            .await
            .is_ok()
    );
    let rebuilt = RpcChainRoute::new(999_999, route.endpoint_urls()).with_identity_verification();
    assert!(rebuilt.verify_identity(&http.rpc_clients()).await.is_err());
    server.abort();
    replacement_server.abort();
}

#[tokio::test]
async fn identity_checks_and_sync_pools_use_each_endpoint_transport_client() {
    let (wallet, wallet_requests, wallet_server) =
        spawn_recording_rpc_mock(chain_id_responder(CHAIN_ID)).await;
    let (direct, direct_requests, direct_server) =
        spawn_recording_rpc_mock(chain_id_responder(CHAIN_ID)).await;
    let (archive, archive_requests, archive_server) =
        spawn_recording_rpc_mock(chain_id_responder(CHAIN_ID)).await;
    let clients = RpcClients::new(
        tagged_reqwest_client("wallet"),
        tagged_reqwest_client("direct"),
    );
    let route = RpcChainRoute::new_with_transports(
        CHAIN_ID,
        vec![
            (
                wallet.clone().into(),
                RpcTransport::Wallet,
                RpcEndpointRoles::default(),
            ),
            (
                direct.clone().into(),
                RpcTransport::Direct,
                RpcEndpointRoles {
                    general: false,
                    ..RpcEndpointRoles::default()
                },
            ),
        ],
    )
    .with_identity_verification();

    let verified = route
        .verify_identity(&clients)
        .await
        .expect("both endpoints verify");
    assert_eq!(verified.endpoint_transport(&wallet), RpcTransport::Wallet);
    assert_eq!(verified.endpoint_transport(&direct), RpcTransport::Direct);
    // The verified route keeps each endpoint's roles.
    assert_eq!(
        verified
            .for_class(crate::RpcTrafficClass::General)
            .unwrap()
            .endpoint_urls(),
        vec![wallet.clone()]
    );
    // The checks above are cached, so admission sends only the direct archive's check.
    let pool = Arc::new(
        route
            .query_rpc_pool(&clients, Duration::from_secs(5))
            .with_pending_admission()
            .with_pending_archive(),
    );
    route
        .admit_sync_pool(
            &clients,
            &pool,
            Some((archive.into(), RpcTransport::Direct)),
        )
        .await
        .expect("verified endpoints admit the pool");
    eventually(|| available(&pool) == vec![0, 1] && pool.archive_admitted()).await;
    for provider in pool.available_providers() {
        assert_eq!(provider.provider.get_chain_id().await.unwrap(), CHAIN_ID);
    }

    let tags = |requests: &RecordedRequests| {
        requests
            .lock()
            .unwrap()
            .iter()
            .map(|(tag, _)| tag.clone())
            .collect::<Vec<_>>()
    };
    let wallet_tag = Some("wallet".to_owned());
    let direct_tag = Some("direct".to_owned());
    assert_eq!(tags(&wallet_requests), vec![wallet_tag.clone(), wallet_tag]);
    assert_eq!(
        tags(&direct_requests),
        vec![direct_tag.clone(), direct_tag.clone()]
    );
    assert_eq!(tags(&archive_requests), vec![direct_tag]);
    wallet_server.abort();
    direct_server.abort();
    archive_server.abort();
}

#[tokio::test]
async fn class_pool_admission_uses_identities_verified_on_the_full_route() {
    let (wrong, _wrong_requests, wrong_server) =
        spawn_recording_rpc_mock(chain_id_responder(CHAIN_ID + 1)).await;
    let (first, first_requests, first_server) =
        spawn_recording_rpc_mock(chain_id_responder(CHAIN_ID)).await;
    let (second, second_requests, second_server) =
        spawn_recording_rpc_mock(chain_id_responder(CHAIN_ID)).await;
    let clients = crate::HttpContext::direct_for_tests().rpc_clients();
    let general_only = RpcEndpointRoles {
        sensitive: false,
        ..RpcEndpointRoles::default()
    };
    let sensitive_only = RpcEndpointRoles {
        general: false,
        ..RpcEndpointRoles::default()
    };
    let route = RpcChainRoute::new_with_transports(
        CHAIN_ID,
        vec![
            (wrong.into(), RpcTransport::Wallet, general_only),
            (first.into(), RpcTransport::Wallet, sensitive_only),
            (second.into(), RpcTransport::Wallet, sensitive_only),
        ],
    )
    .with_identity_verification();
    route
        .verify_identity(&clients)
        .await
        .expect("the Sensitive endpoints verify");

    // Pool slots follow the class route, so slot 0 is the first Sensitive endpoint, not
    // the General one that serves another chain.
    let sensitive = route.for_class(crate::RpcTrafficClass::Sensitive).unwrap();
    let pool = Arc::new(
        sensitive
            .query_rpc_pool(&clients, Duration::from_secs(5))
            .with_pending_admission(),
    );
    sensitive
        .admit_sync_pool(&clients, &pool, None)
        .await
        .expect("cached identities admit the pool");
    eventually(|| available(&pool) == vec![0, 1]).await;
    let checks = |requests: &RecordedRequests| requests.lock().unwrap().len();
    assert_eq!((checks(&first_requests), checks(&second_requests)), (1, 1));
    wrong_server.abort();
    first_server.abort();
    second_server.abort();
}
