use alloy::providers::{Provider, ProviderBuilder};
use broadcaster_core::query_rpc_pool::{QueryRpcPool, RpcAdmission};
use futures_util::future::{self, Join, Ready, join_all};
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use poi::SensitiveUrl;
use rand::seq::SliceRandom as _;
use std::fmt::{self, Display};
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{
    ClassRoute, RpcBrokerError, RpcChainRoute, RpcClients, RpcEndpointRoles, RpcTransport,
};

const IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on concurrent identity probes for one pool admission or candidate source.
pub(super) const MAX_IDENTITY_PROBES: usize = 8;
/// How often a background admission checks that its pool is still owned.
const POOL_LIVENESS_INTERVAL: Duration = Duration::from_secs(1);
const SYNC_ADMISSION_BACKOFF: AdmissionBackoff = AdmissionBackoff {
    initial: Duration::from_secs(30),
    max: Duration::from_mins(5),
};

/// Orders candidate endpoint indices in place before probing or selection.
pub(crate) type CandidateOrder = fn(&mut [usize]);

/// Shuffles candidates so no configured position gets a fixed preference.
pub(crate) fn random_candidate_order(indices: &mut [usize]) {
    indices.shuffle(&mut rand::rng());
}

/// Runs at most `limit` probes at once and yields each keyed result as it completes. The owner
/// decides when work becomes eligible; the runner never spawns tasks or retries.
pub(super) struct ProbeRunner<K, F: Future> {
    limit: usize,
    in_flight: FuturesUnordered<Join<Ready<K>, F>>,
}

impl<K, F: Future> ProbeRunner<K, F> {
    pub(super) fn new(limit: usize) -> Self {
        debug_assert!(limit > 0, "a probe runner needs at least one slot");
        Self {
            limit: limit.max(1),
            in_flight: FuturesUnordered::new(),
        }
    }

    pub(super) fn has_capacity(&self) -> bool {
        self.in_flight.len() < self.limit
    }

    pub(super) fn is_empty(&self) -> bool {
        self.in_flight.is_empty()
    }

    /// Callers check `has_capacity` first.
    pub(super) fn push(&self, key: K, probe: F) {
        debug_assert!(self.has_capacity(), "probe runner is full");
        self.in_flight.push(future::join(future::ready(key), probe));
    }

    /// Returns the next finished probe, or `None` when nothing is in flight.
    pub(super) async fn next(&mut self) -> Option<(K, F::Output)> {
        self.in_flight.next().await
    }
}

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
            .endpoints
            .iter()
            .find(|(url, _)| url == endpoint)
            .expect("endpoint belongs to this route")
            .1
    }

    pub(super) fn endpoint_identity_verified(&self, endpoint: &SensitiveUrl) -> bool {
        !self.requires_identity_verification() || self.endpoint_verification(endpoint).initialized()
    }

    /// Whether `other` shares this route's identity cache, as its clones do.
    pub(super) fn shares_identity_cache(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.verified_identities, &other.verified_identities)
    }

    /// Completes after a later successful identity check on this route or a clone. Create it
    /// before reading the cache so a check that finishes in between still wakes it.
    pub(super) fn identity_checked(&self) -> tokio::sync::futures::Notified<'_> {
        self.verified_identities.checked.notified()
    }

    /// Successful checks belong to this immutable route and its clones. Failed checks
    /// remain retryable; replacing the configuration starts with no verified endpoints.
    pub(super) async fn verify_endpoint_identity(
        &self,
        clients: &RpcClients,
        endpoint: &SensitiveUrl,
        timeout: Duration,
    ) -> Result<(), RpcBrokerError> {
        if !self.requires_identity_verification() {
            return Ok(());
        }
        let result = tokio::time::timeout(
            timeout,
            self.endpoint_verification(endpoint)
                .get_or_try_init(|| self.check_chain_id(clients, endpoint)),
        )
        .await
        .map_err(|_| RpcBrokerError::Timeout)?
        .copied();
        if result.is_ok() {
            // Wakes parked jobs on this route, whichever clone verified the endpoint.
            self.verified_identities.checked.notify_waiters();
        }
        result
    }

    /// Logs the outcome under `label` (a configured index or `archive`), never the URL.
    pub(super) async fn verify_labeled_endpoint_identity(
        &self,
        clients: &RpcClients,
        endpoint: &SensitiveUrl,
        label: &(dyn Display + Sync),
    ) -> Result<(), RpcBrokerError> {
        let started = tokio::time::Instant::now();
        let result = self
            .verify_endpoint_identity(clients, endpoint, IDENTITY_TIMEOUT)
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

    /// One bounded `eth_chainId` request to an endpoint of a trusted route, for background
    /// warmup. It neither reads nor fills the identity cache, so the route stays trusted for
    /// admission. Logs the outcome under `label`, never the URL.
    pub(super) async fn probe_labeled_endpoint_responsiveness(
        &self,
        clients: &RpcClients,
        endpoint: &SensitiveUrl,
        label: &(dyn Display + Sync),
    ) -> Result<(), RpcBrokerError> {
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(IDENTITY_TIMEOUT, self.check_chain_id(clients, endpoint))
            .await
            .unwrap_or(Err(RpcBrokerError::Timeout));
        tracing::debug!(
            chain_id = self.chain_id(),
            endpoint = %label,
            outcome = identity_outcome(&result),
            elapsed_ms = started.elapsed().as_millis(),
            "RPC endpoint responsiveness check finished"
        );
        result
    }

    /// Asks `endpoint` over its transport's client for its chain ID and compares it with this
    /// route's, without a timeout.
    async fn check_chain_id(
        &self,
        clients: &RpcClients,
        endpoint: &SensitiveUrl,
    ) -> Result<(), RpcBrokerError> {
        let url = endpoint.expose_url();
        let client = clients.get(self.endpoint_transport(url)).clone();
        let provider = ProviderBuilder::new().connect_reqwest(client, url.clone());
        let id = provider
            .get_chain_id()
            .await
            .map_err(|_| RpcBrokerError::Transport)?;
        if id != self.chain_id() {
            return Err(RpcBrokerError::InvalidResponse);
        }
        Ok(())
    }

    /// Checks all endpoints concurrently, so slow endpoints delay the caller by at most one
    /// timeout. The returned route keeps the configured order of the endpoints that passed.
    pub(crate) async fn verify_identity(
        &self,
        clients: &RpcClients,
    ) -> Result<Self, RpcBrokerError> {
        let checks = self.configured_settings().enumerate().map(
            |(index, (endpoint, transport, roles))| async move {
                self.verify_labeled_endpoint_identity(clients, endpoint, &index)
                    .await
                    .is_ok()
                    .then(|| (endpoint.clone(), transport, roles))
            },
        );
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
        let mut route = Self::new_with_transports(self.chain_id(), endpoints);
        if let Some(multicall) = self.multicall() {
            route = route.with_multicall(multicall);
        }
        // This snapshot is used only by the operation that awaited verification.
        Ok(route)
    }

    /// A pool over this route's endpoints in configured order, each sending through its
    /// transport's client.
    pub(super) fn query_rpc_pool(&self, clients: &RpcClients, cooldown: Duration) -> QueryRpcPool {
        QueryRpcPool::with_http_clients(
            self.transport_endpoints()
                .map(|(endpoint, transport)| {
                    (
                        endpoint.expose_url().clone(),
                        clients.get(transport).clone(),
                    )
                })
                .collect(),
            cooldown,
        )
    }

    /// Admits `pool` endpoints in the background as their identity checks pass. `pool` must be
    /// built by `query_rpc_pool` with pending admission, and with a pending archive when
    /// `archive` is set. The archive is checked over its own transport. Returns once any regular
    /// endpoint is admitted, or fails once every regular endpoint's first check failed; the
    /// archive never affects the result.
    pub(super) async fn admit_sync_pool(
        &self,
        clients: &RpcClients,
        pool: &Arc<QueryRpcPool>,
        archive: Option<(SensitiveUrl, RpcTransport)>,
    ) -> Result<(), RpcBrokerError> {
        self.admit_pool(
            clients,
            pool,
            archive,
            SYNC_ADMISSION_BACKOFF,
            random_candidate_order,
        )
        .await
    }

    async fn admit_pool(
        &self,
        clients: &RpcClients,
        pool: &Arc<QueryRpcPool>,
        archive: Option<(SensitiveUrl, RpcTransport)>,
        backoff: AdmissionBackoff,
        order: CandidateOrder,
    ) -> Result<(), RpcBrokerError> {
        let chain_id = self.chain_id();
        if self.endpoints().is_empty() {
            return Err(RpcBrokerError::NoEndpoint { chain_id });
        }
        let (report, admitted) = oneshot::channel();
        tokio::spawn(self.clone().run_pool_admission(
            clients.clone(),
            Arc::downgrade(pool),
            archive,
            backoff,
            order,
            report,
        ));
        if admitted.await.unwrap_or(false) {
            Ok(())
        } else {
            Err(RpcBrokerError::NoEndpoint { chain_id })
        }
    }

    /// Runs until every endpoint is admitted or excluded, or until the pool is dropped. Only a
    /// weak pool reference is held, so dropping the pool stops all probing. `order` arranges the
    /// initial regular probes once; retries and the archive are not reordered.
    async fn run_pool_admission(
        self,
        clients: RpcClients,
        pool: Weak<QueryRpcPool>,
        archive: Option<(SensitiveUrl, RpcTransport)>,
        backoff: AdmissionBackoff,
        order: CandidateOrder,
        report: oneshot::Sender<bool>,
    ) {
        let archive = archive.map(|(url, transport)| {
            let route = Self::new_with_transports(
                self.chain_id(),
                vec![(url.clone(), transport, RpcEndpointRoles::default())],
            )
            .with_identity_verification();
            (route, url)
        });
        let regular_count = self.endpoints().len();
        let mut regular = (0..regular_count).collect::<Vec<_>>();
        order(&mut regular);
        let now = Instant::now();
        let mut queued = regular
            .into_iter()
            .map(AdmissionSlot::Provider)
            .chain(archive.is_some().then_some(AdmissionSlot::Archive))
            .map(|slot| AdmissionProbe {
                slot,
                due: now,
                retry_delay: backoff.initial,
                first: true,
            })
            .collect::<Vec<_>>();
        let clients = &clients;
        let start_probe = |slot: AdmissionSlot| {
            let (route, endpoint) = match slot {
                AdmissionSlot::Provider(index) => (&self, &self.endpoints()[index]),
                AdmissionSlot::Archive => {
                    let (route, url) = archive.as_ref().expect("archive probes need an archive");
                    (route, url)
                }
            };
            async move {
                route
                    .verify_labeled_endpoint_identity(clients, endpoint, &slot)
                    .await
            }
        };
        let mut report = Some(report);
        let mut first_failures = 0;
        let mut probes = ProbeRunner::new(MAX_IDENTITY_PROBES);
        loop {
            let now = Instant::now();
            while probes.has_capacity() {
                let Some(position) = queued.iter().position(|probe| probe.due <= now) else {
                    break;
                };
                if pool.strong_count() == 0 {
                    return;
                }
                let probe = queued.remove(position);
                probes.push(probe, start_probe(probe.slot));
            }
            if probes.is_empty() && queued.is_empty() {
                return;
            }
            let mut wake = now + POOL_LIVENESS_INTERVAL;
            if probes.has_capacity()
                && let Some(due) = queued.iter().map(|probe| probe.due).min()
            {
                wake = wake.min(due);
            }
            let finished = if probes.is_empty() {
                tokio::time::sleep_until(wake).await;
                None
            } else {
                tokio::time::timeout_at(wake, probes.next())
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

impl ClassRoute {
    /// A pool over this class's endpoints in configured order, each sending through its
    /// transport's client.
    pub(crate) fn query_rpc_pool(&self, clients: &RpcClients, cooldown: Duration) -> QueryRpcPool {
        self.route().query_rpc_pool(clients, cooldown)
    }

    /// Admits the slots of a pool built by [`Self::query_rpc_pool`] as this class's endpoints
    /// pass their identity checks, reading and filling the full route's shared identity cache.
    /// See [`RpcChainRoute::admit_sync_pool`].
    pub(crate) async fn admit_sync_pool(
        &self,
        clients: &RpcClients,
        pool: &Arc<QueryRpcPool>,
        archive: Option<(SensitiveUrl, RpcTransport)>,
    ) -> Result<(), RpcBrokerError> {
        self.route().admit_sync_pool(clients, pool, archive).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        RecordedRequests, RpcMockGate, RpcResponder, spawn_gated_rpc_mock,
        spawn_recording_rpc_mock, spawn_rpc_mock, tagged_reqwest_client,
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
        clients: &RpcClients,
        archive: bool,
    ) -> Arc<QueryRpcPool> {
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
        assert!(available(&pool).is_empty());
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
        let route =
            RpcChainRoute::new(999_999, vec![endpoint.clone()]).with_identity_verification();
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
        let replacement =
            RpcChainRoute::new(999_999, vec![replacement]).with_identity_verification();
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
        let rebuilt =
            RpcChainRoute::new(999_999, route.endpoint_urls()).with_identity_verification();
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
}
