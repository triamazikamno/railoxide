//! Endpoint candidates for one operation whose callers send their own RPC requests.
//!
//! One broker lookup captures the route's health-eligible endpoints. Identity progress then
//! changes readiness locally, without contacting the actor; a health refresh is another
//! acquisition. Neither the lookup nor requests through the pool touch broker request counts,
//! health or permits.

use super::identity::{MAX_IDENTITY_PROBES, ProbeRunner};
use super::{RpcBrokerError, RpcChainRoute, RpcClients};
use crate::HttpContext;
use crate::desktop::DEFAULT_QUERY_RPC_COOLDOWN;
use broadcaster_core::query_rpc_pool::{ProviderHandle, QueryRpcPool, RpcAdmission};
use poi::SensitiveUrl;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;

/// Eligible endpoints captured for one quote, estimate or preflight and shared by its nested
/// fee reads. Clones share discovery, which stops when the last clone drops.
#[derive(Clone)]
pub(crate) struct RpcCandidates(Arc<Inner>);

struct Inner {
    /// Built from the snapshot in its order; only admitted endpoints are ready.
    pool: Arc<QueryRpcPool>,
    /// Bumped after each admission change and closed when discovery ends.
    progress: watch::Receiver<()>,
    discovery: Option<AbortHandle>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(discovery) = &self.discovery {
            discovery.abort();
        }
    }
}

impl RpcCandidates {
    /// Captures `route`'s eligible endpoints for native direct reads. Trusted and already
    /// verified endpoints are ready at once. The rest are probed in the background once each, in
    /// snapshot order and at most `MAX_IDENTITY_PROBES` at a time, and become ready as they
    /// pass. Any failed probe excludes its endpoint from this source only.
    pub(crate) async fn acquire(
        http: &HttpContext,
        route: &RpcChainRoute,
        deadline: Option<Instant>,
    ) -> Result<Self, RpcBrokerError> {
        let snapshot = http
            .rpc_broker()
            .endpoint_candidates(route, deadline)
            .await?;
        let pool = Arc::new(snapshot_pool(http, route, &snapshot).with_pending_admission());
        let mut unverified = Vec::new();
        for (slot, endpoint) in snapshot.iter().enumerate() {
            if route.endpoint_identity_verified(endpoint) {
                pool.set_provider_admission(slot, RpcAdmission::Admitted);
            } else {
                let configured = route
                    .endpoints()
                    .iter()
                    .position(|configured| configured == endpoint)
                    .expect("candidates belong to the route");
                unverified.push((slot, configured));
            }
        }
        // Without discovery the sender drops on return, so the source starts finished.
        let (progress, receiver) = watch::channel(());
        let discovery = if unverified.is_empty() {
            None
        } else {
            let task = tokio::spawn(discover(
                route.clone(),
                http.rpc_clients(),
                Arc::clone(&pool),
                unverified,
                progress,
            ));
            Some(task.abort_handle())
        };
        Ok(Self(Arc::new(Inner {
            pool,
            progress: receiver,
            discovery,
        })))
    }

    /// Captures `route`'s eligible endpoints with all of them ready and no local probes. Use only
    /// for `DappRpcReadClient` reads, whose gateway and broker admission verify each endpoint's
    /// identity per read.
    pub(crate) async fn acquire_for_gateway(
        http: &HttpContext,
        route: &RpcChainRoute,
        deadline: Option<Instant>,
    ) -> Result<Self, RpcBrokerError> {
        let snapshot = http
            .rpc_broker()
            .endpoint_candidates(route, deadline)
            .await?;
        let (_, progress) = watch::channel(());
        Ok(Self(Arc::new(Inner {
            pool: Arc::new(snapshot_pool(http, route, &snapshot)),
            progress,
            discovery: None,
        })))
    }

    /// Ready providers are the admitted ones. Identity progress only admits endpoints, so it
    /// never clears a caller's own `mark_bad_provider` cooldown.
    pub(crate) fn pool(&self) -> &QueryRpcPool {
        &self.0.pool
    }

    /// Eligible endpoints in the snapshot, ready or not.
    pub(crate) fn len(&self) -> usize {
        self.0.pool.len()
    }

    /// One consumer's view of admission progress. Every change since the source was acquired
    /// starts unobserved, so a consumer can inspect the pool and then await `changed` without
    /// missing one. `changed` errs once discovery has ended and every change has been observed.
    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.0.progress.clone()
    }

    /// Returns the first ready provider in snapshot order that is not cooling down and not in
    /// `tried`, and adds it to `tried`. Waits for discovery while none qualifies; `None` once
    /// discovery has ended without one.
    pub(crate) async fn next_untried(&self, tried: &mut HashSet<usize>) -> Option<ProviderHandle> {
        let provider = self
            .wait_for(|pool| {
                pool.available_providers()
                    .into_iter()
                    .find(|provider| !tried.contains(&provider.index))
            })
            .await?;
        tried.insert(provider.index);
        Some(provider)
    }

    /// Like `QueryRpcPool::random_provider`, but waits for discovery while none is ready; `None`
    /// once discovery has ended without one.
    pub(crate) async fn random_ready(&self) -> Option<ProviderHandle> {
        self.wait_for(QueryRpcPool::random_provider).await
    }

    async fn wait_for<T>(&self, mut find: impl FnMut(&QueryRpcPool) -> Option<T>) -> Option<T> {
        let mut progress = self.subscribe();
        loop {
            if let Some(found) = find(&self.0.pool) {
                return Some(found);
            }
            if progress.changed().await.is_err() {
                return None;
            }
        }
    }
}

/// Each snapshot endpoint sends through its transport's client on `route`.
fn snapshot_pool(
    http: &HttpContext,
    route: &RpcChainRoute,
    snapshot: &[SensitiveUrl],
) -> QueryRpcPool {
    QueryRpcPool::with_http_clients(
        snapshot
            .iter()
            .map(|endpoint| {
                let url = endpoint.expose_url();
                (
                    url.clone(),
                    http.rpc_client_for(route.endpoint_transport(url)).clone(),
                )
            })
            .collect(),
        DEFAULT_QUERY_RPC_COOLDOWN,
    )
}

/// Probes each `(pool slot, configured index)` once, then returns, which closes `progress`.
/// Probes log the configured index, never the URL.
async fn discover(
    route: RpcChainRoute,
    clients: RpcClients,
    pool: Arc<QueryRpcPool>,
    unverified: Vec<(usize, usize)>,
    progress: watch::Sender<()>,
) {
    let (route, clients) = (&route, &clients);
    let start_probe = |configured: usize| async move {
        route
            .verify_labeled_endpoint_identity(clients, &route.endpoints()[configured], &configured)
            .await
    };
    let mut queued = unverified.into_iter();
    let mut probes = ProbeRunner::new(MAX_IDENTITY_PROBES);
    loop {
        while probes.has_capacity() {
            let Some((slot, configured)) = queued.next() else {
                break;
            };
            probes.push(slot, start_probe(configured));
        }
        let Some((slot, result)) = probes.next().await else {
            return;
        };
        // Transient failures are not retried here; a later operation may try again.
        let admission = if result.is_ok() {
            RpcAdmission::Admitted
        } else {
            RpcAdmission::Excluded
        };
        pool.set_provider_admission(slot, admission);
        // Published after the pool update, so a woken consumer sees the new admission.
        progress.send_modify(|()| {});
    }
}
