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
    shared.write().start_fee_subscription();

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

const MAX_FEE_ANNOUNCEMENT_TOKENS: usize = 256;
const MAX_FEE_ANNOUNCEMENT_POI_LIST_KEYS: usize = 16;

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
    if !signature_valid {
        tracing::debug!(chain_id, "rejected fees payload with invalid signature");
        return 0;
    }

    let token_count = body.fees.len();
    let poi_list_key_count = body.required_poi_list_keys.len();
    if token_count > MAX_FEE_ANNOUNCEMENT_TOKENS
        || poi_list_key_count > MAX_FEE_ANNOUNCEMENT_POI_LIST_KEYS
    {
        tracing::warn!(
            chain_id,
            token_count,
            poi_list_key_count,
            "rejected fees payload over count limits"
        );
        return 0;
    }

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
mod tests;
