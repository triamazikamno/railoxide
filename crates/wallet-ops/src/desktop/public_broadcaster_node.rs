//! A Waku node of its own for each public broadcaster submission.

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, WrapErr, eyre};
use rand::RngExt as _;
use rand::seq::IndexedRandom as _;
use tokio::sync::{mpsc, watch};
use waku::proto::WakuMessage;
use waku_relay::client::Client as WakuClient;

use crate::{HttpContext, OperationHttpClient, WalletNetworkMode};
use waku_relay::client::{ClientConfig, RelayNetworkConfig, RelayNetworkMode};

/// The most seeds, and the most peers, of one submission node.
const SUBMISSION_NODE_MAX_PEERS: usize = 3;
/// Bound for a fresh node to reach a `LightPush` peer and a filter peer. Over Tor its new
/// isolation has to build circuits before the first connection, which can take tens of seconds.
pub(crate) const SUBMISSION_NODE_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a waiting submission reads its node's route.
pub(crate) const SUBMISSION_ROUTE_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long a route must stay not ready before the wallet recovers it, so that a peer that
/// drops and redials on its own triggers nothing.
pub(crate) const SUBMISSION_ROUTE_LOSS_GRACE: Duration = Duration::from_secs(10);
/// The most route recoveries of one submission.
pub const SUBMISSION_ROUTE_RECOVERY_LIMIT: u32 = 2;
const SUBMISSION_NODE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Ends a wallet root's submission nodes. A closed scope builds no node and ends every
/// teardown delay at once.
#[derive(Clone, Debug)]
pub struct SubmissionNodeScope {
    closed: Arc<watch::Sender<bool>>,
}

impl Default for SubmissionNodeScope {
    fn default() -> Self {
        Self::new()
    }
}

impl SubmissionNodeScope {
    #[must_use]
    pub fn new() -> Self {
        Self {
            closed: Arc::new(watch::Sender::new(false)),
        }
    }

    pub fn close(&self) {
        self.closed.send_replace(true);
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    async fn closed(&self) {
        // `self` holds the sender, so the wait ends only on close.
        let _ = self.closed.subscribe().wait_for(|closed| *closed).await;
    }
}

/// A submission node's connected peers, and whether any of them serves `LightPush` and filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SubmissionRouteHealth {
    pub(crate) peers: usize,
    pub(crate) lightpush: bool,
    pub(crate) filter: bool,
}

impl SubmissionRouteHealth {
    /// The node can publish and receive responses.
    pub(crate) const fn ready(self) -> bool {
        self.lightpush && self.filter
    }
}

/// The Waku operations a submission runs on its node.
#[async_trait::async_trait]
pub(crate) trait SubmissionNode: Send + Sync {
    fn local_peer_id(&self) -> Option<waku::PeerId>;
    fn pubsub_path(&self) -> &str;
    fn route_health(&self) -> SubmissionRouteHealth;
    /// Drops the node's connections and redials. `false` when the node can't, outside Tor.
    fn refresh_route(&self) -> bool;
    async fn wait_ready(&self, bound: Duration) -> Result<()>;
    async fn subscribe(&self, content_topics: Vec<String>) -> Result<mpsc::Receiver<WakuMessage>>;
    async fn publish(&self, content_topic: &str, payload: &[u8]) -> Result<()>;
    async fn shutdown(&self, bound: Duration) -> Result<()>;
}

#[async_trait::async_trait]
impl SubmissionNode for WakuClient {
    fn local_peer_id(&self) -> Option<waku::PeerId> {
        Self::local_peer_id(self)
    }

    fn pubsub_path(&self) -> &str {
        Self::pubsub_path(self)
    }

    fn route_health(&self) -> SubmissionRouteHealth {
        let mut health = SubmissionRouteHealth::default();
        for peer in self.peer_snapshots().iter().filter(|peer| peer.connected) {
            health.peers += 1;
            health.lightpush |= peer.supports_lightpush_v3;
            health.filter |= peer.supports_filter;
        }
        health
    }

    fn refresh_route(&self) -> bool {
        self.refresh_network_session()
    }

    async fn wait_ready(&self, bound: Duration) -> Result<()> {
        Ok(Self::wait_ready(self, bound).await?)
    }

    async fn subscribe(&self, content_topics: Vec<String>) -> Result<mpsc::Receiver<WakuMessage>> {
        Ok(Self::subscribe(self, content_topics).await?)
    }

    async fn publish(&self, content_topic: &str, payload: &[u8]) -> Result<()> {
        Ok(Self::publish(self, content_topic, payload).await?)
    }

    async fn shutdown(&self, bound: Duration) -> Result<()> {
        Ok(Self::shutdown(self, bound).await?)
    }
}

/// What a submission node is built from.
pub(crate) struct SubmissionNodeSpec {
    pub(crate) config: ClientConfig,
    pub(crate) network: RelayNetworkConfig,
}

#[cfg(test)]
pub(crate) type SubmissionNodeFactory =
    Arc<dyn Fn(SubmissionNodeSpec) -> Result<Box<dyn SubmissionNode>> + Send + Sync>;

/// What a public broadcaster submission needs to run on a Waku node of its own.
#[derive(Clone)]
pub struct PublicBroadcasterWaku {
    /// The root-owned node. Submissions only read its seed peers.
    root: Arc<WakuClient>,
    /// The active settings' Waku configuration.
    config: ClientConfig,
    http: HttpContext,
    scope: SubmissionNodeScope,
    #[cfg(test)]
    node_factory: Option<SubmissionNodeFactory>,
}

impl PublicBroadcasterWaku {
    #[must_use]
    pub const fn new(
        root: Arc<WakuClient>,
        config: ClientConfig,
        http: HttpContext,
        scope: SubmissionNodeScope,
    ) -> Self {
        Self {
            root,
            config,
            http,
            scope,
            #[cfg(test)]
            node_factory: None,
        }
    }

    /// Builds nodes with `factory` instead of the Waku client.
    #[cfg(test)]
    pub(crate) fn with_node_factory_for_tests(
        mut self,
        factory: impl Fn(SubmissionNodeSpec) -> Result<Box<dyn SubmissionNode>> + Send + Sync + 'static,
    ) -> Self {
        self.node_factory = Some(Arc::new(factory));
        self
    }

    /// Builds a node for one submission and waits until it can publish and receive responses.
    ///
    /// The node has a fresh identity, up to three seeds sampled from the root's snapshot, and
    /// one operation client for its Tor dials, DNS discovery and nwaku requests, returned with
    /// it so that the submission can re-isolate its route. It publishes nothing.
    pub(crate) async fn start_node(
        &self,
    ) -> Result<(Box<dyn SubmissionNode>, OperationHttpClient)> {
        let network_mode = self.http.network_mode();
        if network_mode == WalletNetworkMode::Proxy {
            return Err(waku_unavailable("Proxy mode disables Waku"));
        }
        if self.scope.is_closed() {
            return Err(waku_unavailable("the wallet is locked"));
        }
        let seeds: Vec<_> = self
            .root
            .seed_peers()
            .sample(&mut rand::rng(), SUBMISSION_NODE_MAX_PEERS)
            .cloned()
            .collect();
        let mut config = self.config.clone();
        if seeds.is_empty() {
            // Without a seed the node runs DNS discovery with the configured trees.
            if config.dns_enr_trees.as_ref().is_some_and(Vec::is_empty) {
                return Err(waku_unavailable(
                    "no Waku peer is known yet and DNS discovery is disabled",
                ));
            }
        } else {
            config.dns_enr_trees = Some(Vec::new());
        }
        let seed_count = seeds.len();
        config.direct_peers = seeds;
        config.backup_peers = Vec::new();
        config.max_peers = Some(SUBMISSION_NODE_MAX_PEERS);

        let operation = self
            .http
            .operation_http_client()
            .await
            .wrap_err("Waku delivery network is unavailable")?;
        let http_client = operation.client().clone();
        let network = if network_mode == WalletNetworkMode::Tor {
            let provider = operation
                .tor_client_provider()
                .ok_or_else(|| waku_unavailable("the submission has no isolated Tor client"))?;
            RelayNetworkConfig::tor_with_client_provider(provider, http_client)
        } else {
            RelayNetworkConfig {
                mode: RelayNetworkMode::Direct,
                http_client: Some(http_client),
                tor_client: None,
            }
        };
        if self.scope.is_closed() {
            return Err(waku_unavailable("the wallet is locked"));
        }
        let node = self.build_node(SubmissionNodeSpec { config, network })?;
        tracing::info!(
            peer_id = ?node.local_peer_id(),
            seeds = seed_count,
            isolation = ?operation.isolation(),
            "started public broadcaster submission node"
        );
        // Dropping the node on failure stops it without waiting.
        node.wait_ready(SUBMISSION_NODE_READY_TIMEOUT)
            .await
            .wrap_err("Waku delivery network is unavailable: no broadcaster peer connected")?;
        Ok((node, operation))
    }

    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "tests replace the node factory")
    )]
    fn build_node(&self, spec: SubmissionNodeSpec) -> Result<Box<dyn SubmissionNode>> {
        #[cfg(test)]
        if let Some(factory) = &self.node_factory {
            return factory(spec);
        }
        let node = WakuClient::new_with_network(&spec.config, spec.network)
            .wrap_err("Waku delivery network is unavailable: submission node failed to start")?;
        Ok(Box::new(node))
    }

    /// Keeps `node` connected, without publishing, for a random 10 to 60 seconds so that its
    /// lifetime doesn't end at its response, then shuts it down and drops its `operation`
    /// client. A closed scope ends the delay at once.
    pub(crate) fn retire(&self, node: Box<dyn SubmissionNode>, operation: OperationHttpClient) {
        // A dropped submission retires its node from `Drop`, which can run off the runtime or
        // while unwinding. There, dropping the node signals its shutdown without blocking.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            drop((node, operation));
            return;
        };
        let scope = self.scope.clone();
        let tail = Duration::from_millis(rand::rng().random_range(10_000..=60_000));
        runtime.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(tail) => {}
                () = scope.closed() => {}
            }
            if let Err(error) = node.shutdown(SUBMISSION_NODE_SHUTDOWN_TIMEOUT).await {
                tracing::debug!(%error, "public broadcaster submission node shutdown did not complete");
            }
            drop(operation);
        });
    }
}

fn waku_unavailable(reason: &str) -> eyre::Report {
    eyre!("Waku delivery network is unavailable: {reason}")
}
