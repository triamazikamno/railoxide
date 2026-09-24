use alloy::providers::{Provider, ProviderBuilder};
use broadcaster_core::query_rpc_pool::{QueryRpcPool, RpcAdmission};
use futures_util::future::join_all;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use poi::SensitiveUrl;
use std::fmt::{self, Display};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{RpcBrokerError, RpcChainRoute};

const IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on concurrent identity probes for one pool admission.
const MAX_IDENTITY_PROBES: usize = 8;
/// How often a background admission checks that its pool is still owned.
const POOL_LIVENESS_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_ADMISSION_BACKOFF: AdmissionBackoff = AdmissionBackoff {
    initial: Duration::from_secs(30),
    max: Duration::from_mins(5),
};

/// Per-endpoint retry delay after a failed probe; it doubles up to `max`.
#[derive(Clone, Copy)]
struct AdmissionBackoff {
    initial: Duration,
    max: Duration,
}

/// A pool slot; displays as the configured index or `archive`, never the URL.
#[derive(Clone, Copy)]
enum AdmissionSlot {
    Provider(usize),
    Archive,
}

impl Display for AdmissionSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider(index) => Display::fmt(index, f),
            Self::Archive => f.write_str("archive"),
        }
    }
}

#[derive(Clone, Copy)]
struct AdmissionProbe {
    slot: AdmissionSlot,
    due: Instant,
    retry_delay: Duration,
    first: bool,
}

impl AdmissionProbe {
    fn retry(self, backoff: AdmissionBackoff) -> Self {
        Self {
            due: Instant::now() + self.retry_delay,
            retry_delay: self.retry_delay.saturating_mul(2).min(backoff.max),
            first: false,
            ..self
        }
    }
}

/// Records a finished probe in the pool, or returns `None` once the pool is gone. The pool is
/// upgraded only for this synchronous update, never across an await. Wrong chains are
/// excluded for good; other failures stay pending for a retry.
fn apply_probe_result(
    pool: &Weak<QueryRpcPool>,
    slot: AdmissionSlot,
    result: &Result<(), RpcBrokerError>,
) -> Option<RpcAdmission> {
    let admission = match result {
        Ok(()) => RpcAdmission::Admitted,
        Err(RpcBrokerError::InvalidResponse) => RpcAdmission::Excluded,
        Err(_) => RpcAdmission::Pending,
    };
    if admission != RpcAdmission::Pending {
        let pool = pool.upgrade()?;
        match slot {
            AdmissionSlot::Provider(index) => pool.set_provider_admission(index, admission),
            AdmissionSlot::Archive => pool.set_archive_admission(admission),
        }
    }
    Some(admission)
}

/// Outcome class for identity diagnostics; error `Display` text is never logged.
const fn identity_outcome(result: &Result<(), RpcBrokerError>) -> &'static str {
    match result {
        Ok(()) => "verified",
        Err(RpcBrokerError::Timeout) => "timeout",
        Err(RpcBrokerError::Transport) => "transport",
        Err(RpcBrokerError::InvalidResponse) => "invalid_response",
        Err(_) => "other",
    }
}

impl RpcChainRoute {
    fn endpoint_verification(&self, endpoint: &SensitiveUrl) -> &tokio::sync::OnceCell<()> {
        &self
            .verified_identities
            .iter()
            .find(|(url, _)| url == endpoint)
            .expect("endpoint belongs to this route")
            .1
    }

    pub(super) fn endpoint_identity_verified(&self, endpoint: &SensitiveUrl) -> bool {
        !self.requires_identity_verification() || self.endpoint_verification(endpoint).initialized()
    }

    /// Successful checks belong to this immutable route and its clones. Failed checks
    /// remain retryable; replacing the configuration starts with no verified endpoints.
    pub(super) async fn verify_endpoint_identity(
        &self,
        client: &reqwest::Client,
        endpoint: &SensitiveUrl,
        timeout: Duration,
    ) -> Result<(), RpcBrokerError> {
        if !self.requires_identity_verification() {
            return Ok(());
        }
        tokio::time::timeout(
            timeout,
            self.endpoint_verification(endpoint)
                .get_or_try_init(|| async {
                    let provider = ProviderBuilder::new()
                        .connect_reqwest(client.clone(), endpoint.expose_url().clone());
                    let id = provider
                        .get_chain_id()
                        .await
                        .map_err(|_| RpcBrokerError::Transport)?;
                    if id != self.chain_id() {
                        return Err(RpcBrokerError::InvalidResponse);
                    }
                    Ok(())
                }),
        )
        .await
        .map_err(|_| RpcBrokerError::Timeout)?
        .copied()
    }

    /// Logs the outcome under `label` (a configured index or `archive`), never the URL.
    async fn verify_labeled_endpoint_identity(
        &self,
        client: &reqwest::Client,
        endpoint: &SensitiveUrl,
        label: &(dyn Display + Sync),
    ) -> Result<(), RpcBrokerError> {
        let started = tokio::time::Instant::now();
        let result = self
            .verify_endpoint_identity(client, endpoint, IDENTITY_TIMEOUT)
            .await;
        if self.requires_identity_verification() {
            tracing::debug!(
                chain_id = self.chain_id(),
                endpoint = %label,
                outcome = identity_outcome(&result),
                elapsed_ms = started.elapsed().as_millis(),
                "RPC endpoint identity check finished"
            );
        }
        result
    }

    /// Checks all endpoints concurrently, so slow endpoints delay the caller by at most one
    /// timeout. The returned route keeps the configured order of the endpoints that passed.
    pub(crate) async fn verify_identity(
        &self,
        client: &reqwest::Client,
    ) -> Result<Self, RpcBrokerError> {
        let checks = self
            .endpoints()
            .iter()
            .enumerate()
            .map(|(index, endpoint)| async move {
                self.verify_labeled_endpoint_identity(client, endpoint, &index)
                    .await
                    .is_ok()
                    .then(|| endpoint.clone())
            });
        let endpoints = join_all(checks)
            .await
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if endpoints.is_empty() {
            return Err(RpcBrokerError::NoEndpoint {
                chain_id: self.chain_id(),
            });
        }
        let mut route = Self::new(self.chain_id(), endpoints);
        if let Some(multicall) = self.multicall() {
            route = route.with_multicall(multicall);
        }
        // This snapshot is used only by the operation that awaited verification.
        Ok(route)
    }

    /// Admits `pool` endpoints in the background as their identity checks pass. `pool` must be
    /// built from `endpoint_urls()` in order with pending admission, and with a pending archive
    /// when `archive` is set. Returns once any regular endpoint is admitted, or fails once every
    /// regular endpoint's first check failed; the archive never affects the result.
    pub(crate) async fn admit_sync_pool(
        &self,
        client: &reqwest::Client,
        pool: &Arc<QueryRpcPool>,
        archive: Option<SensitiveUrl>,
    ) -> Result<(), RpcBrokerError> {
        self.admit_pool(client, pool, archive, SYNC_ADMISSION_BACKOFF)
            .await
    }

    async fn admit_pool(
        &self,
        client: &reqwest::Client,
        pool: &Arc<QueryRpcPool>,
        archive: Option<SensitiveUrl>,
        backoff: AdmissionBackoff,
    ) -> Result<(), RpcBrokerError> {
        let chain_id = self.chain_id();
        if self.endpoints().is_empty() {
            return Err(RpcBrokerError::NoEndpoint { chain_id });
        }
        let (report, admitted) = oneshot::channel();
        tokio::spawn(self.clone().run_pool_admission(
            client.clone(),
            Arc::downgrade(pool),
            archive,
            backoff,
            report,
        ));
        if admitted.await.unwrap_or(false) {
            Ok(())
        } else {
            Err(RpcBrokerError::NoEndpoint { chain_id })
        }
    }

    /// Runs until every endpoint is admitted or excluded, or until the pool is dropped. Only a
    /// weak pool reference is held, so dropping the pool stops all probing.
    async fn run_pool_admission(
        self,
        client: reqwest::Client,
        pool: Weak<QueryRpcPool>,
        archive: Option<SensitiveUrl>,
        backoff: AdmissionBackoff,
        report: oneshot::Sender<bool>,
    ) {
        let archive = archive.map(|url| {
            let route = Self::new(self.chain_id(), vec![url.clone()]).with_identity_verification();
            (route, url)
        });
        let regular_count = self.endpoints().len();
        let now = Instant::now();
        let mut queued = (0..regular_count)
            .map(AdmissionSlot::Provider)
            .chain(archive.is_some().then_some(AdmissionSlot::Archive))
            .map(|slot| AdmissionProbe {
                slot,
                due: now,
                retry_delay: backoff.initial,
                first: true,
            })
            .collect::<Vec<_>>();
        let client = &client;
        let start_probe = |probe: AdmissionProbe| {
            let (route, endpoint) = match probe.slot {
                AdmissionSlot::Provider(index) => (&self, &self.endpoints()[index]),
                AdmissionSlot::Archive => {
                    let (route, url) = archive.as_ref().expect("archive probes need an archive");
                    (route, url)
                }
            };
            async move {
                let result = route
                    .verify_labeled_endpoint_identity(client, endpoint, &probe.slot)
                    .await;
                (probe, result)
            }
        };
        let mut report = Some(report);
        let mut first_failures = 0;
        let mut in_flight = FuturesUnordered::new();
        loop {
            let now = Instant::now();
            while in_flight.len() < MAX_IDENTITY_PROBES {
                let Some(position) = queued.iter().position(|probe| probe.due <= now) else {
                    break;
                };
                if pool.strong_count() == 0 {
                    return;
                }
                in_flight.push(start_probe(queued.remove(position)));
            }
            if in_flight.is_empty() && queued.is_empty() {
                return;
            }
            let mut wake = now + POOL_LIVENESS_INTERVAL;
            if in_flight.len() < MAX_IDENTITY_PROBES
                && let Some(due) = queued.iter().map(|probe| probe.due).min()
            {
                wake = wake.min(due);
            }
            let finished = if in_flight.is_empty() {
                tokio::time::sleep_until(wake).await;
                None
            } else {
                tokio::time::timeout_at(wake, in_flight.next())
                    .await
                    .ok()
                    .flatten()
            };
            if pool.strong_count() == 0 {
                return;
            }
            let Some((probe, result)) = finished else {
                continue;
            };
            let Some(admission) = apply_probe_result(&pool, probe.slot, &result) else {
                return;
            };
            if matches!(probe.slot, AdmissionSlot::Provider(_)) {
                let admitted = admission == RpcAdmission::Admitted;
                if !admitted && probe.first {
                    first_failures += 1;
                }
                if (admitted || first_failures == regular_count)
                    && let Some(report) = report.take()
                {
                    let _ = report.send(admitted);
                }
            }
            if admission == RpcAdmission::Pending {
                queued.push(probe.retry(backoff));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{RpcMockGate, RpcResponder, spawn_gated_rpc_mock, spawn_rpc_mock};
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
    async fn held_chain_id_mock(
        chain_id: u64,
    ) -> (url::Url, tokio::task::JoinHandle<()>, RpcMockGate) {
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

    fn pending_pool(
        route: &RpcChainRoute,
        client: &reqwest::Client,
        archive: bool,
    ) -> Arc<QueryRpcPool> {
        let pool = QueryRpcPool::with_http_client(
            route.endpoint_urls(),
            Duration::from_secs(5),
            client.clone(),
        )
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
            .verify_identity(&http.rpc_client)
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
        let pool = pending_pool(&route, &http.rpc_client, true);

        // The held endpoint and archive answer only after the gates open below.
        tokio::time::timeout(
            Duration::from_secs(5),
            route.admit_sync_pool(&http.rpc_client, &pool, Some(archive.into())),
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
        let pool = pending_pool(&route, &http.rpc_client, true);

        route
            .admit_pool(
                &http.rpc_client,
                &pool,
                Some(wrong_archive.into()),
                FAST_BACKOFF,
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
        let pool = pending_pool(&route, &http.rpc_client, false);

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            route.admit_sync_pool(&http.rpc_client, &pool, None),
        )
        .await
        .expect("admission reports after the first failed checks");
        assert!(matches!(
            result,
            Err(RpcBrokerError::NoEndpoint { chain_id: CHAIN_ID })
        ));
        assert!(available(&pool).is_empty());
        wrong_server.abort();
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
        let pool = pending_pool(&route, &http.rpc_client, false);

        route
            .admit_pool(&http.rpc_client, &pool, None, FAST_BACKOFF)
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
        let route =
            RpcChainRoute::new(999_999, vec![endpoint.clone()]).with_identity_verification();
        assert!(route.verify_identity(&http.rpc_client).await.is_err());
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
                .verify_identity(&http.rpc_client)
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
        let replacement =
            RpcChainRoute::new(999_999, vec![replacement]).with_identity_verification();
        assert!(replacement.verify_identity(&http.rpc_client).await.is_err());
        // Populating a cache must not change broker grouping or authority equality.
        assert_eq!(route, equivalent);
        assert_eq!(fingerprint(&route), before_verification);
        assert_eq!(fingerprint(&equivalent), before_verification);
        // A rebuilt configuration must verify even a formerly correct endpoint.
        returned_id.store(1, Ordering::SeqCst);
        assert!(
            route
                .clone()
                .verify_identity(&http.rpc_client)
                .await
                .is_ok()
        );
        let rebuilt =
            RpcChainRoute::new(999_999, route.endpoint_urls()).with_identity_verification();
        assert!(rebuilt.verify_identity(&http.rpc_client).await.is_err());
        server.abort();
        replacement_server.abort();
    }
}
