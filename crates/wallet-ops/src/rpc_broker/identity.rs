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
    /// route's, without a timeout. A Tron endpoint's HTTP API must also report mainnet's genesis.
    async fn check_chain_id(
        &self,
        clients: &RpcClients,
        endpoint: &SensitiveUrl,
    ) -> Result<(), RpcBrokerError> {
        let url = endpoint.expose_url();
        let client = clients.get(self.endpoint_transport(url)).clone();
        self.pace(url).await;
        let provider = ProviderBuilder::new().connect_reqwest(client.clone(), url.clone());
        let id = provider
            .get_chain_id()
            .await
            .map_err(|_| RpcBrokerError::Transport)?;
        if id != self.chain_id() {
            return Err(RpcBrokerError::InvalidResponse);
        }
        self.check_tron_genesis(&client, endpoint, IDENTITY_TIMEOUT)
            .await
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
        route.tron.clone_from(&self.tron);
        // This snapshot is used only by the operation that awaited verification.
        Ok(route)
    }

    /// A pool over this route's endpoints in configured order, each sending through the client
    /// [`Self::read_client`] picks for it.
    pub(super) fn query_rpc_pool(&self, clients: &RpcClients, cooldown: Duration) -> QueryRpcPool {
        QueryRpcPool::with_http_clients(
            self.endpoints()
                .iter()
                .map(|endpoint| {
                    let url = endpoint.expose_url();
                    (url.clone(), self.read_client(clients, url).clone())
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
    /// A pool over this class's endpoints in configured order. See
    /// [`RpcChainRoute::query_rpc_pool`].
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
mod tests;
