//! Background identity and responsiveness warmup for one installation's routes.
//!
//! Checks use the route's shared identity cache; trusted routes instead get an `eth_chainId`
//! responsiveness probe without changing their trust. Failed endpoints stay out of fee snapshots
//! until a background retry succeeds, using the broker's endpoint cooldowns. The pass shares the
//! verification budget with job probes but holds no read HTTP slots or job probe runners.

use super::RpcChainRoute;
use super::actor::{EndpointHealthOutcome, RequestEvent};
use super::capacity::JobResources;
use super::identity::{CandidateOrder, ProbeRunner};
use super::profile::EndpointProfile;
use futures_util::future::BoxFuture;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Weak};
use tokio::sync::watch;
use tokio::time::{self, Instant};
use url::Url;

/// Leave half of the broker's eight verification permits for pending requests.
const MAX_BACKGROUND_PROBES: usize = 4;

/// Owns one installation's warmup pass. Clones share the pass, and dropping the last clone stops
/// it. Checks that already succeeded stay cached on the routes.
#[derive(Clone)]
pub struct RpcIdentityWarmup(pub(super) Arc<WarmupOwner>);

impl fmt::Debug for RpcIdentityWarmup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RpcIdentityWarmup").finish_non_exhaustive()
    }
}

impl RpcIdentityWarmup {
    /// A new owner and the request that starts its pass.
    pub(super) fn start(
        routes: Vec<RpcChainRoute>,
        preferred_chain: Option<u64>,
    ) -> (Self, WarmupRequest) {
        let (priority, preferred_chain) = watch::channel(preferred_chain);
        let (routes, activated_routes) = watch::channel(routes);
        (
            Self(Arc::new(WarmupOwner { priority, routes })),
            WarmupRequest {
                activated_routes,
                preferred_chain,
            },
        )
    }

    /// Prioritizes checks not yet started for this chain without restarting the pass.
    pub fn prioritize_chain(&self, chain_id: u64) {
        let _ = self.0.priority.send_replace(Some(chain_id));
    }

    /// Adds an installed route without restarting existing checks or recovery. Replace the
    /// owner when route configuration changes; repeated activation of the same route is inert.
    pub fn activate_route(&self, route: RpcChainRoute) {
        self.0.routes.send_if_modified(|routes| {
            if routes
                .iter()
                .any(|current| current == &route && current.shares_identity_cache(&route))
            {
                return false;
            }
            routes.push(route);
            true
        });
    }
}

/// Dropping the owner closes both watch channels, which stops the pass; never clone these senders.
pub(super) struct WarmupOwner {
    priority: watch::Sender<Option<u64>>,
    routes: watch::Sender<Vec<RpcChainRoute>>,
}

/// The installation whose pass a broker started last, so registering it again joins that pass.
pub(super) type InstalledWarmup = Option<(Vec<RpcChainRoute>, Weak<WarmupOwner>)>;

pub(super) struct WarmupRequest {
    activated_routes: watch::Receiver<Vec<RpcChainRoute>>,
    preferred_chain: watch::Receiver<Option<u64>>,
}

/// Owns initial probes, failed endpoints and their pending recovery until the owner drops.
pub(super) struct WarmupPass {
    resources: JobResources,
    routes: Vec<RpcChainRoute>,
    /// `(route position, endpoint index)` targets not yet started, including pending retries.
    queued: Vec<(usize, usize)>,
    failed: HashSet<(usize, usize)>,
    probes: ProbeRunner<(usize, usize), BoxFuture<'static, Option<RequestEvent>>>,
    preferred_chain: watch::Receiver<Option<u64>>,
    activated_routes: watch::Receiver<Vec<RpcChainRoute>>,
    order: CandidateOrder,
}

/// Whether `current` and `next` are the same route objects. Equal routes rebuilt from the same
/// URLs have their own caches, so they are a new installation.
pub(super) fn same_installation(current: &[RpcChainRoute], next: &[RpcChainRoute]) -> bool {
    current.len() == next.len()
        && current
            .iter()
            .zip(next)
            .all(|(current, next)| current == next && current.shares_identity_cache(next))
}

/// Builds the pass for `request`. Newly activated targets are shuffled once and run at most
/// four at a time, so the bound does not grow with the route count. Queued targets for the
/// preferred chain run first; other targets retain their shuffled order.
pub(super) fn pass(
    resources: JobResources,
    request: WarmupRequest,
    order: CandidateOrder,
) -> WarmupPass {
    let WarmupRequest {
        activated_routes,
        preferred_chain,
    } = request;
    WarmupPass {
        probes: ProbeRunner::new(MAX_BACKGROUND_PROBES),
        resources,
        routes: Vec::new(),
        queued: Vec::new(),
        failed: HashSet::new(),
        preferred_chain,
        activated_routes,
        order,
    }
}

/// The next failure or successful recovery; pending while there is none.
pub(super) async fn next_event(
    pass: Option<&mut WarmupPass>,
    profiles: &HashMap<(u64, Url), EndpointProfile>,
) -> Option<RequestEvent> {
    match pass {
        Some(pass) => pass.next_event(profiles).await,
        None => std::future::pending().await,
    }
}

impl WarmupPass {
    fn append_activated_routes(&mut self) {
        let activated = self.activated_routes.borrow_and_update();
        let start = self.routes.len();
        if activated.len() == start {
            return;
        }
        self.routes.extend_from_slice(&activated[start..]);
        // Only new routes add work; completed checks and recovery keep their current state.
        let targets = self.routes[start..]
            .iter()
            .enumerate()
            .flat_map(|(position, route)| {
                route
                    .endpoints()
                    .iter()
                    .enumerate()
                    .filter(|(_, endpoint)| {
                        !(route.requires_identity_verification()
                            && route.endpoint_identity_verified(endpoint))
                    })
                    .map(move |(index, _)| (start + position, index))
            })
            .collect::<Vec<_>>();
        let mut ordered = (0..targets.len()).collect::<Vec<_>>();
        (self.order)(&mut ordered);
        self.queued
            .extend(ordered.into_iter().map(|target| targets[target]));
    }

    /// A failed endpoint stays excluded while its recovery is queued or in flight.
    pub(super) fn excludes(&self, route: &RpcChainRoute, endpoint: &poi::SensitiveUrl) -> bool {
        self.failed.iter().any(|&(position, index)| {
            let installed = &self.routes[position];
            installed.chain_id() == route.chain_id()
                && installed.shares_identity_cache(route)
                && installed.endpoints()[index] == *endpoint
        })
    }

    fn retry_at(
        &self,
        target: (usize, usize),
        profiles: &HashMap<(u64, Url), EndpointProfile>,
    ) -> Option<Instant> {
        if !self.failed.contains(&target) {
            return None;
        }
        let (position, index) = target;
        let route = &self.routes[position];
        profiles
            .get(&(
                route.chain_id(),
                route.endpoints()[index].expose_url().clone(),
            ))
            .and_then(|profile| profile.withdrawn_until)
    }

    async fn next_event(
        &mut self,
        profiles: &HashMap<(u64, Url), EndpointProfile>,
    ) -> Option<RequestEvent> {
        loop {
            self.append_activated_routes();
            // Only due targets enter the runner, so backoff holds neither permits nor slots.
            while self.probes.has_capacity() {
                let now = Instant::now();
                let preferred_chain = *self.preferred_chain.borrow();
                let position = self
                    .queued
                    .iter()
                    .enumerate()
                    .filter(|(_, target)| {
                        self.retry_at(**target, profiles).is_none_or(|at| at <= now)
                    })
                    .min_by_key(|(_, (route, _))| {
                        Some(self.routes[*route].chain_id()) != preferred_chain
                    })
                    .map(|(position, _)| position);
                let Some(position) = position else {
                    break;
                };
                let target @ (route, index) = self.queued.remove(position);
                let probe = warm_endpoint(
                    self.resources.clone(),
                    self.routes[route].clone(),
                    index,
                    self.failed.contains(&target),
                );
                self.probes.push(target, Box::pin(probe));
            }
            let retry_at = if self.probes.has_capacity() {
                self.queued
                    .iter()
                    .filter_map(|&target| self.retry_at(target, profiles))
                    .min()
            } else {
                None
            };
            tokio::select! {
                biased;
                changed = self.activated_routes.changed() => {
                    changed.ok()?;
                }
                changed = self.preferred_chain.changed() => {
                    changed.ok()?;
                }
                finished = self.probes.next(), if !self.probes.is_empty() => {
                    let (target, event) = finished?;
                    if let Some(event) = event {
                        if event.health_outcome == EndpointHealthOutcome::Strike {
                            self.failed.insert(target);
                            self.queued.push(target);
                        } else if event.health_outcome == EndpointHealthOutcome::Healthy {
                            self.failed.remove(&target);
                        }
                        return Some(event);
                    }
                }
                () = async {
                    if let Some(at) = retry_at {
                        time::sleep_until(at).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
            }
        }
    }
}

/// Initial successes need no event; a recovery success restores snapshot eligibility.
async fn warm_endpoint(
    resources: JobResources,
    route: RpcChainRoute,
    index: usize,
    recovery: bool,
) -> Option<RequestEvent> {
    let endpoint = &route.endpoints()[index];
    let verify = route.requires_identity_verification();
    if verify && route.endpoint_identity_verified(endpoint) {
        return recovery.then(|| {
            RequestEvent::new(
                route.chain_id(),
                endpoint.expose_url().clone(),
                EndpointHealthOutcome::Healthy,
            )
        });
    }
    let _budget = resources.probes.acquire().await.ok()?;
    let result = if verify {
        route
            .verify_labeled_endpoint_identity(&resources.clients, endpoint, &index)
            .await
    } else {
        // A trusted route counts as verified, so its identity check would send nothing.
        route
            .probe_labeled_endpoint_responsiveness(&resources.clients, endpoint, &index)
            .await
    };
    if result.is_ok() && !recovery {
        return None;
    }
    Some(RequestEvent::new(
        route.chain_id(),
        endpoint.expose_url().clone(),
        EndpointHealthOutcome::for_result(&result),
    ))
}
