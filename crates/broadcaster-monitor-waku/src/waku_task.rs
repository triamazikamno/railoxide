use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eyre::{Result, WrapErr};
use public_broadcaster_protocol::Payload as FeePayload;
use tokio::sync::{mpsc, watch};
use waku::PeerSnapshot;
use waku::proto::WakuMessage;
pub use waku::{DEFAULT_CLEARNET_DOH_ENDPOINT as DEFAULT_DOH_ENDPOINT, DEFAULT_TOR_DOH_ENDPOINT};
use waku_relay::client::{
    AdditionalPeer, Client, ClientConfig, RelayNetworkConfig, RelayNetworkMode,
};
pub use waku_relay::client::{DEFAULT_CLUSTER_ID, DEFAULT_SHARD_ID};
use waku_relay::msg::ContentTopic;

use broadcaster_monitor::{EventTx, FeeRow, PeerRow, PeerSummary, Shared, publish_revision};

pub const DEFAULT_MAX_PEERS: usize = 10;
pub const DEFAULT_PEER_CONNECTION_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, Default)]
pub struct WakuMonitorConfig {
    pub chain_ids: Vec<u64>,
    pub cluster_id: Option<u32>,
    pub shard_id: Option<u32>,
    pub dns_enr_trees: Option<Vec<String>>,
    pub direct_peers: Vec<WakuMonitorDirectPeer>,
    pub backup_peers: Vec<WakuMonitorDirectPeer>,
    pub doh_endpoint: Option<String>,
    pub doh_fallback_endpoints: Option<Vec<String>>,
    pub max_peers: Option<usize>,
    pub peer_connection_timeout: Option<Duration>,
    pub nwaku_url: Option<String>,
    pub network: RelayNetworkConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakuMonitorDirectPeer {
    pub peer_id: String,
    pub addr: String,
}

impl WakuMonitorConfig {
    /// Build a Waku relay client config from explicit monitor settings. The standalone
    /// wallet monitor fills these from the active wallet network context.
    #[must_use]
    pub fn to_waku_config(&self) -> ClientConfig {
        let doh = self
            .doh_endpoint
            .clone()
            .unwrap_or_else(|| default_doh_endpoint(self.network.mode).to_string());
        let timeout = self
            .peer_connection_timeout
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_PEER_CONNECTION_TIMEOUT_SECS));

        ClientConfig {
            nwaku_url: self.nwaku_url.clone(),
            direct_peers: grouped_direct_peers(&self.direct_peers),
            backup_peers: grouped_direct_peers(&self.backup_peers),
            dns_enr_trees: self.dns_enr_trees.clone(),
            doh_endpoint: Some(doh),
            doh_fallback_endpoints: self.doh_fallback_endpoints.clone(),
            cluster_id: Some(self.cluster_id.unwrap_or(DEFAULT_CLUSTER_ID)),
            shard_id: Some(self.shard_id.unwrap_or(DEFAULT_SHARD_ID)),
            max_peers: Some(self.max_peers.unwrap_or(DEFAULT_MAX_PEERS)),
            peer_connection_timeout: Some(timeout),
        }
    }

    /// Construct and start a Waku client from monitor settings.
    pub fn build_client(&self) -> Result<Arc<Client>> {
        let cfg = self.to_waku_config();
        let client = Client::new_with_network(&cfg, self.network.clone())
            .wrap_err("construct waku relay client")?;
        Ok(Arc::new(client))
    }
}

fn grouped_direct_peers(peers: &[WakuMonitorDirectPeer]) -> Vec<AdditionalPeer> {
    let mut grouped: Vec<AdditionalPeer> = Vec::new();
    for peer in peers {
        if let Some(existing) = grouped
            .iter_mut()
            .find(|existing| existing.peer_id == peer.peer_id)
        {
            if !existing.addrs.iter().any(|addr| addr == &peer.addr) {
                existing.addrs.push(peer.addr.clone());
            }
        } else {
            grouped.push(AdditionalPeer {
                peer_id: peer.peer_id.clone(),
                addrs: vec![peer.addr.clone()],
            });
        }
    }
    grouped
}

const fn default_doh_endpoint(network_mode: RelayNetworkMode) -> &'static str {
    match network_mode {
        RelayNetworkMode::Tor => DEFAULT_TOR_DOH_ENDPOINT,
        RelayNetworkMode::Direct | RelayNetworkMode::Proxy => DEFAULT_DOH_ENDPOINT,
    }
}

/// Interval for refreshing peer snapshots from the Waku node.
const PEER_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Spawn the monitor's background Waku + fees workers on the current runtime.
pub async fn spawn_workers(
    opts: WakuMonitorConfig,
    waku: Arc<Client>,
    shared: Shared,
    events: EventTx,
) -> Result<()> {
    spawn_workers_inner(opts, waku, shared, events, None).await
}

pub async fn spawn_workers_until_shutdown(
    opts: WakuMonitorConfig,
    waku: Arc<Client>,
    shared: Shared,
    events: EventTx,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    spawn_workers_inner(opts, waku, shared, events, Some(shutdown)).await
}

async fn spawn_workers_inner(
    opts: WakuMonitorConfig,
    waku: Arc<Client>,
    shared: Shared,
    events: EventTx,
    mut shutdown: Option<watch::Receiver<bool>>,
) -> Result<()> {
    let chain_ids = opts.chain_ids;
    let content_topics: Vec<String> = chain_ids
        .iter()
        .map(|chain_id| ContentTopic::fees_topic(*chain_id))
        .collect();

    tracing::info!(
        chains = ?chain_ids,
        topics = ?content_topics,
        "subscribing to broadcaster fees content topics"
    );

    if let Some(reason) = waku.disabled_reason() {
        tracing::warn!(%reason, "Waku fee subscription disabled by network policy");
        if shutdown.is_some() {
            run_peer_poll_loop(waku, shared, events, shutdown).await;
        } else {
            spawn_peer_poll_worker(waku, shared, events, None);
        }
        return Ok(());
    }

    let msg_rx = if let Some(shutdown) = shutdown.as_mut() {
        let Some(result) =
            await_until_shutdown(waku.subscribe_with_fee_history(content_topics), shutdown).await
        else {
            tracing::debug!("fees subscription startup shutting down");
            return Ok(());
        };
        result.wrap_err("subscribe to fees content topics")?
    } else {
        waku.subscribe_with_fee_history(content_topics)
            .await
            .wrap_err("subscribe to fees content topics")?
    };

    if let Some(shutdown) = shutdown {
        let peer_shutdown = shutdown.clone();
        tokio::join!(
            run_fees_loop(msg_rx, shared.clone(), events.clone(), Some(shutdown)),
            run_peer_poll_loop(waku, shared, events, Some(peer_shutdown)),
        );
    } else {
        spawn_peer_poll_worker(Arc::clone(&waku), shared.clone(), events.clone(), None);
        tokio::spawn(async move {
            run_fees_loop(msg_rx, shared, events, None).await;
        });
    }

    Ok(())
}

fn spawn_peer_poll_worker(
    waku: Arc<Client>,
    shared: Shared,
    events: EventTx,
    shutdown: Option<watch::Receiver<bool>>,
) {
    tokio::spawn(async move {
        run_peer_poll_loop(waku, shared, events, shutdown).await;
    });
}

async fn run_fees_loop(
    mut msg_rx: mpsc::Receiver<WakuMessage>,
    shared: Shared,
    events: EventTx,
    mut shutdown: Option<watch::Receiver<bool>>,
) {
    #[allow(clippy::large_enum_variant)]
    enum LoopEvent {
        Message(Option<WakuMessage>),
        PromotePending,
        Shutdown,
    }

    let pending_timer = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(pending_timer);
    loop {
        let pending_deadline = shared.read().next_pending_fee_announcement_deadline();
        if let Some(deadline) = pending_deadline {
            pending_timer
                .as_mut()
                .reset(tokio::time::Instant::from_std(deadline));
        }

        let event = if let Some(shutdown) = shutdown.as_mut() {
            tokio::select! {
                biased;
                should_shutdown = shutdown_changed_or_requested(shutdown) => {
                    if should_shutdown {
                        LoopEvent::Shutdown
                    } else {
                        continue;
                    }
                }
                () = pending_timer.as_mut(), if pending_deadline.is_some() => LoopEvent::PromotePending,
                msg = msg_rx.recv() => LoopEvent::Message(msg),
            }
        } else {
            tokio::select! {
                msg = msg_rx.recv() => LoopEvent::Message(msg),
                () = pending_timer.as_mut(), if pending_deadline.is_some() => LoopEvent::PromotePending,
            }
        };

        match event {
            LoopEvent::Message(Some(msg)) => {
                let Some(chain_id) = extract_fees_chain_id(&msg.content_topic) else {
                    tracing::trace!(topic = %msg.content_topic, "ignoring non-fees content topic");
                    continue;
                };
                handle_fees_message(chain_id, &msg.payload, &shared, &events);
            }
            LoopEvent::Message(None) => break,
            LoopEvent::PromotePending => {
                let revisions = shared
                    .write()
                    .promote_due_fee_announcements(Instant::now(), SystemTime::now());
                for revision in revisions {
                    publish_revision(&events, revision);
                }
            }
            LoopEvent::Shutdown => {
                tracing::debug!("fees subscription worker shutting down");
                return;
            }
        }
    }
    tracing::warn!("fees subscription channel closed");
}

/// Decode one fees `WakuMessage` payload and emit one row per token fee.
/// Returns the number of rows produced (for testability).
pub fn handle_fees_message(
    chain_id: u64,
    payload: &[u8],
    shared: &Shared,
    events: &EventTx,
) -> usize {
    handle_fees_message_at(
        chain_id,
        payload,
        shared,
        events,
        SystemTime::now(),
        Instant::now(),
    )
}

fn handle_fees_message_at(
    chain_id: u64,
    payload: &[u8],
    shared: &Shared,
    events: &EventTx,
    wall_now: SystemTime,
    monotonic_now: Instant,
) -> usize {
    let payload: FeePayload = match serde_json::from_slice(payload) {
        Ok(p) => p,
        Err(error) => {
            tracing::warn!(%error, chain_id, "failed to decode fees envelope");
            return 0;
        }
    };

    let (body, signature_valid) = match payload.decode_and_verify() {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, chain_id, "failed to verify fees payload");
            return 0;
        }
    };

    let railgun_address: Arc<str> = Arc::from(body.railgun_address.as_ref());
    let fees_id: Arc<str> = Arc::from(body.fees_id.as_str());
    let identifier: Option<Arc<str>> = body.identifier.map(|s| Arc::from(s.as_str()));
    let version: Arc<str> = Arc::from(body.version.as_str());
    let required_poi_list_keys: Vec<Arc<str>> = body
        .required_poi_list_keys
        .into_iter()
        .map(|key| Arc::from(key.as_str()))
        .collect();
    let fee_expiration = SystemTime::UNIX_EPOCH + Duration::from_millis(body.fee_expiration);
    let rows = body
        .fees
        .into_iter()
        .map(|(token_address, fee)| FeeRow {
            chain_id,
            railgun_address: railgun_address.clone(),
            token_address,
            fee,
            signature_valid,
            fees_id: fees_id.clone(),
            fee_expiration,
            available_wallets: body.available_wallets,
            version: version.clone(),
            relay_adapt: body.relay_adapt,
            relay_adapt_7702: body.relay_adapt_7702,
            required_poi_list_keys: required_poi_list_keys.clone(),
            identifier: identifier.clone(),
            last_seen: wall_now,
            reliability: body.reliability,
        })
        .collect();
    let outcome = shared.write().admit_fee_announcement(
        chain_id,
        &railgun_address,
        fee_expiration,
        wall_now,
        monotonic_now,
        signature_valid,
        rows,
    );
    if let Some(rev) = outcome.revision() {
        publish_revision(events, rev);
    }
    outcome.visible_rows()
}

/// Extract the chain id from a `/railgun/v2/0-{chain_id}-fees/json` topic.
/// Returns `None` for non-fees topics.
#[must_use]
pub fn extract_fees_chain_id(topic: &str) -> Option<u64> {
    match ContentTopic::parse(topic) {
        ContentTopic::Fees(chain_id) => Some(chain_id),
        _ => None,
    }
}

async fn run_peer_poll_loop(
    waku: Arc<Client>,
    shared: Shared,
    events: EventTx,
    mut shutdown: Option<watch::Receiver<bool>>,
) {
    let mut ticker = tokio::time::interval(PEER_POLL_INTERVAL);
    loop {
        if let Some(shutdown) = shutdown.as_mut() {
            tokio::select! {
                _ = ticker.tick() => {}
                should_shutdown = shutdown_changed_or_requested(shutdown) => {
                    if should_shutdown {
                        tracing::debug!("peer polling worker shutting down");
                        break;
                    }
                    continue;
                }
            }
        } else {
            ticker.tick().await;
        }
        publish_peer_summary(&waku, &shared, &events);
    }
}

async fn shutdown_changed_or_requested(shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    shutdown.changed().await.is_err() || *shutdown.borrow()
}

async fn await_until_shutdown<F>(
    future: F,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<F::Output>
where
    F: Future,
{
    tokio::select! {
        result = future => Some(result),
        _ = shutdown_changed_or_requested(shutdown) => None,
    }
}

fn publish_peer_summary(waku: &Client, shared: &Shared, events: &EventTx) {
    let (summary, rows) = peer_state_for_client(waku);
    if let Some(rev) = shared.write().set_peers(summary, rows) {
        publish_revision(events, rev);
    }
}

fn peer_state_for_client(waku: &Client) -> (PeerSummary, Vec<PeerRow>) {
    let stats = waku.peer_stats();
    let snapshots = waku.peer_snapshots();
    let network_degraded = match waku.network_mode() {
        RelayNetworkMode::Tor => stats.connected_peers.is_empty(),
        RelayNetworkMode::Proxy => true,
        RelayNetworkMode::Direct => false,
    } || waku.disabled_reason().is_some();
    let network_label: Arc<str> = if let Some(reason) = waku.disabled_reason() {
        Arc::from(format!("{}: {reason}", waku.network_status_label()))
    } else if waku.network_mode() == RelayNetworkMode::Tor && stats.connected_peers.is_empty() {
        Arc::from("Tor-safe Waku: degraded (no safe peers)")
    } else {
        Arc::from(waku.network_status_label())
    };
    let summary = PeerSummary {
        connected: stats.connected_peers.len(),
        known: stats.known_peers,
        dialing: stats.dialing_count,
        lightpush_capable: stats.lightpush_capable,
        peer_exchange_capable: stats.peer_exchange_capable,
        network_label,
        network_degraded,
    };
    let rows: Vec<PeerRow> = snapshots.iter().map(peer_row_from_snapshot).collect();
    (summary, rows)
}

#[must_use]
pub fn peer_row_from_snapshot(snapshot: &PeerSnapshot) -> PeerRow {
    PeerRow {
        peer_id: Arc::from(snapshot.peer_id.to_string().as_str()),
        addrs: snapshot
            .addrs
            .iter()
            .map(|a| Arc::from(a.to_string().as_str()))
            .collect(),
        connected: snapshot.connected,
        dialing: snapshot.dialing,
        supports_lightpush_v3: snapshot.supports_lightpush_v3,
        supports_peer_exchange: snapshot.supports_peer_exchange,
        supports_filter: snapshot.supports_filter,
        dial_failures: snapshot.dial_failures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use alloy::primitives::{U256, address};
    use alloy::uint;
    use broadcaster_monitor::{event_channel, shared};
    use public_broadcaster_protocol::Body as FeeBody;

    #[test]
    fn extract_fees_chain_id_matches_valid_topic() {
        assert_eq!(extract_fees_chain_id("/railgun/v2/0-1-fees/json"), Some(1));
        assert_eq!(
            extract_fees_chain_id("/railgun/v2/0-42161-fees/json"),
            Some(42161)
        );
    }

    #[test]
    fn extract_fees_chain_id_rejects_non_fees_topics() {
        assert_eq!(extract_fees_chain_id("/railgun/v2/0-1-transact/json"), None);
        assert_eq!(extract_fees_chain_id("/other/v2/0-1-fees/json"), None);
        assert_eq!(extract_fees_chain_id("/railgun/v2/0-NaN-fees/json"), None);
    }

    #[test]
    fn peer_row_is_derived_from_snapshot_fields() {
        use libp2p::PeerId;
        let pid = PeerId::random();
        let snap = PeerSnapshot {
            peer_id: pid,
            addrs: Vec::new(),
            connected: true,
            dialing: false,
            supports_lightpush_v3: true,
            supports_peer_exchange: false,
            supports_filter: true,
            dial_failures: 2,
        };
        let row = peer_row_from_snapshot(&snap);
        assert_eq!(row.peer_id.as_ref(), pid.to_string());
        assert!(row.connected);
        assert!(!row.dialing);
        assert!(row.supports_lightpush_v3);
        assert!(!row.supports_peer_exchange);
        assert!(row.supports_filter);
        assert_eq!(row.dial_failures, 2);
    }

    #[test]
    fn waku_config_defaults_apply_when_flags_are_absent() {
        let opts = WakuMonitorConfig::default();
        let cfg = opts.to_waku_config();
        assert_eq!(cfg.cluster_id, Some(DEFAULT_CLUSTER_ID));
        assert_eq!(cfg.shard_id, Some(DEFAULT_SHARD_ID));
        assert_eq!(cfg.max_peers, Some(DEFAULT_MAX_PEERS));
        assert_eq!(cfg.doh_endpoint.as_deref(), Some(DEFAULT_DOH_ENDPOINT));
        assert!(cfg.doh_fallback_endpoints.is_none());
        assert_eq!(
            cfg.peer_connection_timeout,
            Some(Duration::from_secs(DEFAULT_PEER_CONNECTION_TIMEOUT_SECS))
        );
        assert!(cfg.direct_peers.is_empty());
        assert!(cfg.dns_enr_trees.is_none());
    }

    #[test]
    fn waku_config_uses_tor_doh_default_in_tor_mode() {
        let opts = WakuMonitorConfig {
            network: RelayNetworkConfig {
                mode: RelayNetworkMode::Tor,
                http_client: None,
                tor_client: None,
            },
            ..WakuMonitorConfig::default()
        };
        let cfg = opts.to_waku_config();
        assert_eq!(cfg.doh_endpoint.as_deref(), Some(DEFAULT_TOR_DOH_ENDPOINT));
    }

    #[test]
    fn waku_config_overrides_apply_when_flags_present() {
        const PEER_ID: &str = "16Uiu2HAkwNeQVY32bUrL1eM68ryMa48PXY5Bhfxfg9e2byYcc46m";
        let opts = WakuMonitorConfig {
            chain_ids: Vec::new(),
            cluster_id: Some(7),
            shard_id: Some(3),
            dns_enr_trees: Some(vec!["enrtree://custom@example.invalid".to_string()]),
            direct_peers: vec![
                WakuMonitorDirectPeer {
                    peer_id: PEER_ID.to_string(),
                    addr: format!("/dns4/example.invalid/tcp/30304/p2p/{PEER_ID}"),
                },
                WakuMonitorDirectPeer {
                    peer_id: PEER_ID.to_string(),
                    addr: format!("/dns4/example.invalid/tcp/8000/wss/p2p/{PEER_ID}"),
                },
            ],
            backup_peers: vec![
                WakuMonitorDirectPeer {
                    peer_id: PEER_ID.to_string(),
                    addr: "/dns4/backup.example.invalid/tcp/8000/wss".to_string(),
                };
                2
            ],
            max_peers: Some(42),
            doh_endpoint: Some("https://example.invalid/dns-query".to_string()),
            doh_fallback_endpoints: Some(vec![
                "https://fallback.example.invalid/dns-query".to_string(),
            ]),
            peer_connection_timeout: Some(Duration::from_secs(3)),
            nwaku_url: Some("http://127.0.0.1:8645".to_string()),
            network: RelayNetworkConfig::direct(),
        };
        let cfg = opts.to_waku_config();
        assert_eq!(cfg.cluster_id, Some(7));
        assert_eq!(cfg.shard_id, Some(3));
        assert_eq!(cfg.max_peers, Some(42));
        assert_eq!(
            cfg.dns_enr_trees.as_deref(),
            Some(["enrtree://custom@example.invalid".to_string()].as_slice())
        );
        assert_eq!(cfg.direct_peers.len(), 1);
        assert_eq!(cfg.direct_peers[0].peer_id, PEER_ID);
        assert_eq!(cfg.direct_peers[0].addrs.len(), 2);
        assert_eq!(cfg.backup_peers.len(), 1);
        assert_eq!(cfg.backup_peers[0].peer_id, PEER_ID);
        assert_eq!(
            cfg.backup_peers[0].addrs,
            vec!["/dns4/backup.example.invalid/tcp/8000/wss"]
        );
        assert!(
            !cfg.direct_peers[0]
                .addrs
                .contains(&cfg.backup_peers[0].addrs[0])
        );
        assert_eq!(
            cfg.doh_endpoint.as_deref(),
            Some("https://example.invalid/dns-query")
        );
        assert_eq!(
            cfg.doh_fallback_endpoints.as_deref(),
            Some(["https://fallback.example.invalid/dns-query".to_string()].as_slice())
        );
        assert_eq!(cfg.peer_connection_timeout, Some(Duration::from_secs(3)));
        assert_eq!(cfg.nwaku_url.as_deref(), Some("http://127.0.0.1:8645"));
    }

    #[test]
    fn waku_config_doh_override_wins_in_tor_mode() {
        let opts = WakuMonitorConfig {
            doh_endpoint: Some("https://example.invalid/dns-query".to_string()),
            network: RelayNetworkConfig {
                mode: RelayNetworkMode::Tor,
                http_client: None,
                tor_client: None,
            },
            ..WakuMonitorConfig::default()
        };
        let cfg = opts.to_waku_config();
        assert_eq!(
            cfg.doh_endpoint.as_deref(),
            Some("https://example.invalid/dns-query")
        );
    }

    #[tokio::test]
    async fn proxy_mode_worker_publishes_disabled_waku_status() {
        let opts = WakuMonitorConfig {
            chain_ids: vec![1],
            network: RelayNetworkConfig::proxy(reqwest::Client::new()),
            ..WakuMonitorConfig::default()
        };
        let waku = opts.build_client().expect("proxy Waku client");
        let shared = shared();
        let (events, mut event_rx) = event_channel(16);

        spawn_workers(opts, Arc::clone(&waku), shared.clone(), events)
            .await
            .expect("proxy workers start");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let summary = shared.read().peer_summary();
                if summary.network_degraded && summary.network_label.contains("Waku disabled") {
                    break;
                }
                event_rx.changed().await.expect("peer summary event");
            }
        })
        .await
        .expect("disabled Waku status published");
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_startup_work() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        shutdown_tx.send(true).expect("request shutdown");

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            await_until_shutdown(std::future::pending::<()>(), &mut shutdown_rx),
        )
        .await
        .expect("pending startup was cancelled");

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn shutdown_stops_workers_and_releases_client() {
        let opts = WakuMonitorConfig {
            chain_ids: vec![1],
            network: RelayNetworkConfig::proxy(reqwest::Client::new()),
            ..WakuMonitorConfig::default()
        };
        let waku = opts.build_client().expect("proxy Waku client");
        let weak_waku = Arc::downgrade(&waku);
        let shared = shared();
        let (events, _event_rx) = event_channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        shutdown_tx.send(true).expect("request shutdown");

        spawn_workers_until_shutdown(opts, Arc::clone(&waku), shared, events, shutdown_rx)
            .await
            .expect("workers observe shutdown");
        drop(waku);

        tokio::time::timeout(Duration::from_secs(1), async {
            while weak_waku.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker-held client clones released");
    }

    #[test]
    fn handle_fees_message_rejects_invalid_json_without_producing_rows() {
        let shared = shared();
        let (tx, _rx) = event_channel(16);
        let produced = handle_fees_message(1, b"not-json", &shared, &tx);
        assert_eq!(produced, 0);
        assert!(shared.read().fee_rows().is_empty());
    }

    #[test]
    fn handle_fees_message_retains_fee_metadata() {
        const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";

        let token = address!("0000000000000000000000000000000000000001");
        let relay_adapt = address!("0000000000000000000000000000000000000002");
        let relay_adapt_7702 = address!("0000000000000000000000000000000000000003");
        let body = FeeBody {
            fees: HashMap::from([(token, uint!(42_U256))]),
            fee_expiration: 1_900_000_000_000,
            fees_id: "fees-id".to_string(),
            railgun_address: RAILGUN_ADDRESS.into(),
            available_wallets: 3,
            version: "8.2.3".to_string(),
            relay_adapt,
            relay_adapt_7702: Some(relay_adapt_7702),
            required_poi_list_keys: vec!["poi-list".to_string()],
            reliability: 0.91,
            identifier: Some("broadcaster-one".to_string()),
        };
        let payload = FeePayload {
            data: serde_json::to_vec(&body).expect("serialize fees body"),
            signature: vec![0; 64],
        };
        let payload = serde_json::to_vec(&payload).expect("serialize fees payload");
        let shared = shared();
        let (tx, _rx) = event_channel(16);

        let produced = handle_fees_message(1, &payload, &shared, &tx);

        assert_eq!(produced, 1);
        let rows = shared.read().fee_rows();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.available_wallets, 3);
        assert_eq!(row.version.as_ref(), "8.2.3");
        assert_eq!(row.relay_adapt, relay_adapt);
        assert_eq!(row.relay_adapt_7702, Some(relay_adapt_7702));
        assert_eq!(row.required_poi_list_keys, vec![Arc::from("poi-list")]);
        assert_eq!(row.identifier.as_deref(), Some("broadcaster-one"));
        assert!(!row.signature_valid);
    }

    #[test]
    fn handle_fees_message_emits_one_revision_for_all_token_rows() {
        const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";

        let body = FeeBody {
            fees: HashMap::from([
                (
                    address!("0000000000000000000000000000000000000001"),
                    uint!(42_U256),
                ),
                (
                    address!("0000000000000000000000000000000000000002"),
                    uint!(84_U256),
                ),
            ]),
            fee_expiration: 1_900_000_000_000,
            fees_id: "fees-id".to_string(),
            railgun_address: RAILGUN_ADDRESS.into(),
            available_wallets: 3,
            version: "8.2.3".to_string(),
            relay_adapt: address!("0000000000000000000000000000000000000003"),
            relay_adapt_7702: None,
            required_poi_list_keys: Vec::new(),
            reliability: 0.91,
            identifier: None,
        };
        let payload = FeePayload {
            data: serde_json::to_vec(&body).expect("serialize fees body"),
            signature: vec![0; 64],
        };
        let payload = serde_json::to_vec(&payload).expect("serialize fees payload");
        let shared = shared();
        let (tx, rx) = event_channel(16);

        assert_eq!(handle_fees_message(1, &payload, &shared, &tx), 2);
        assert_eq!(shared.read().fee_rows().len(), 2);
        assert_eq!(shared.read().rev(), 1);
        assert_eq!(*rx.borrow(), 1);
    }

    #[test]
    fn handle_fees_message_skips_unverified_replacement_after_verified_generation() {
        const RAILGUN_ADDRESS: &str = "0zk1qy4v02p5zkq0zfpaxhz79j5tslrv8c44d80d8jr2fuecrtxlp8lemrv7j6fe3z53ll0jm7u592n0hr8elesd0xzv6y9jpdvsyln80m95jcxhvnmagfqg5p6e9mp";

        let now = SystemTime::now();
        let expiration = now + Duration::from_mins(5);
        let broadcaster: Arc<str> = Arc::from(RAILGUN_ADDRESS);
        let token = address!("0000000000000000000000000000000000000001");
        let cached_row = FeeRow {
            chain_id: 1,
            railgun_address: broadcaster.clone(),
            token_address: token,
            fee: uint!(42_U256),
            signature_valid: true,
            fees_id: Arc::from("cached-fees-id"),
            fee_expiration: expiration,
            available_wallets: 3,
            version: Arc::from("8.2.3"),
            relay_adapt: address!("0000000000000000000000000000000000000002"),
            relay_adapt_7702: None,
            required_poi_list_keys: Vec::new(),
            identifier: None,
            last_seen: now,
            reliability: 0.91,
        };
        let shared = shared();
        shared.write().admit_fee_announcement(
            1,
            &broadcaster,
            expiration,
            now,
            Instant::now(),
            true,
            vec![cached_row],
        );

        let body = FeeBody {
            fees: HashMap::from([(token, uint!(84_U256))]),
            fee_expiration: 1_900_000_000_000,
            fees_id: "replacement-fees-id".to_string(),
            railgun_address: RAILGUN_ADDRESS.into(),
            available_wallets: 3,
            version: "8.2.3".to_string(),
            relay_adapt: address!("0000000000000000000000000000000000000002"),
            relay_adapt_7702: None,
            required_poi_list_keys: Vec::new(),
            reliability: 0.91,
            identifier: None,
        };
        let payload = FeePayload {
            data: serde_json::to_vec(&body).expect("serialize fees body"),
            signature: vec![0; 64],
        };
        let payload = serde_json::to_vec(&payload).expect("serialize fees payload");
        let (tx, rx) = event_channel(16);

        assert_eq!(handle_fees_message(1, &payload, &shared, &tx), 0);
        let rows = shared.read().fee_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fees_id.as_ref(), "cached-fees-id");
        assert_eq!(*rx.borrow(), 0);
    }

    #[test]
    fn monitor_revisions_never_regress() {
        let (events, rx) = event_channel(16);
        publish_revision(&events, 2);
        publish_revision(&events, 1);
        assert_eq!(*rx.borrow(), 2);
    }

    #[tokio::test]
    async fn fees_worker_promotes_pending_and_then_shuts_down_promptly() {
        let wall = SystemTime::now();
        let monotonic = Instant::now()
            .checked_sub(Duration::from_secs(41))
            .expect("test monotonic timestamp supports subtraction");
        let expiration = wall + Duration::from_mins(2);
        let broadcaster: Arc<str> = Arc::from("0zk-worker");
        let token = address!("0000000000000000000000000000000000000001");
        let row = |fee: u64,
                   fees_id: &'static str,
                   received_at: SystemTime,
                   fee_expiration: SystemTime| FeeRow {
            chain_id: 1,
            railgun_address: broadcaster.clone(),
            token_address: token,
            fee: U256::from(fee),
            signature_valid: true,
            fees_id: Arc::from(fees_id),
            fee_expiration,
            available_wallets: 1,
            version: Arc::from("8.2.3"),
            relay_adapt: address!("0000000000000000000000000000000000000002"),
            relay_adapt_7702: None,
            required_poi_list_keys: Vec::new(),
            identifier: None,
            last_seen: received_at,
            reliability: 1.0,
        };
        let shared = shared();
        shared.write().admit_fee_announcement(
            1,
            &broadcaster,
            expiration,
            wall,
            monotonic,
            true,
            vec![row(100, "active", wall, expiration)],
        );
        let pending_wall = wall + Duration::from_secs(1);
        let pending_expiration = expiration + Duration::from_secs(1);
        shared.write().admit_fee_announcement(
            1,
            &broadcaster,
            pending_expiration,
            pending_wall,
            monotonic + Duration::from_secs(1),
            true,
            vec![row(200, "pending", pending_wall, pending_expiration)],
        );

        let (_msg_tx, msg_rx) = mpsc::channel(1);
        let (events, mut event_rx) = event_channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker_shared = shared.clone();
        let worker = tokio::spawn(async move {
            run_fees_loop(msg_rx, worker_shared, events, Some(shutdown_rx)).await;
        });

        tokio::time::timeout(Duration::from_secs(1), event_rx.changed())
            .await
            .expect("pending fee promotion timed out")
            .expect("fee event channel closed");
        assert_eq!(shared.read().fee_rows()[0].fees_id.as_ref(), "pending");

        shutdown_tx
            .send(true)
            .expect("request fees worker shutdown");
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("fees worker shutdown timed out")
            .expect("fees worker task failed");
    }
}
