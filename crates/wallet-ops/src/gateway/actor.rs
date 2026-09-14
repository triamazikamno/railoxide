use super::{
    Command, GatewayClientMessage, GatewayConfig, GatewayError, GatewayInstallBundle,
    GatewayPairingOffer, GatewayPeerSummary, GatewayServerMessage, GatewaySnapshot, PairingCode,
    PeerId, install,
    provider::{DappProvider, Delivery, DeliveryStatus, ReadCompletion},
    storage::{MAX_PEERS, Peer, Registry, StoredSecret},
};
use dapp_gateway_protocol::{
    ClientHello, Connection, HandshakeEvent, MAX_HANDSHAKE_LEN, MAX_RECORD_LEN, ServerAuth,
    SessionSecret,
};
use futures_util::{Sink, Stream};
use local_db::DbStore;
use std::{
    collections::{HashMap, VecDeque},
    future::poll_fn,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, timeout_at},
};
use tokio_tungstenite::{
    WebSocketStream, accept_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

// Bound aggregate socket buffers and incomplete messages even for authenticated local peers.
const MAX_CONNECTIONS: usize = 64;
const MAX_UNAUTHENTICATED: usize = 8;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
// Response write for the install page and zip; the task holds a pending-upgrade slot meanwhile.
const INSTALL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const CODE_LIFETIME: Duration = Duration::from_mins(2);
const HEARTBEAT: Duration = Duration::from_secs(20);
const ACTIVITY_FLUSH_INTERVAL: Duration = Duration::from_mins(5);
type Socket = WebSocketStream<TcpStream>;
// Logical queued payloads are capped at 40 MiB per peer session (2.5 GiB at 64 sessions).
// One additional sealed copy is bounded by the protocol message limit.
const MAX_OUTBOUND_MESSAGES: usize = 64;
const MAX_OUTBOUND_BYTES: usize = 40 * 1024 * 1024;
// Includes queue residence and stays below the browser's ten-second assembly window.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

struct Outbound {
    delivery: Option<Delivery>,
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    sealed: bool,
    flushing: bool,
    deadline: Instant,
    enqueued_at: Instant,
    first_progress_at: Option<Instant>,
    flush_started_at: Option<Instant>,
    flush_pending_polls: u64,
}

impl Outbound {
    fn log_failure(&self, id: u64, reason: &'static str, phase: &'static str) {
        let message_kind = match self.delivery.as_ref().map(|delivery| &delivery.message) {
            None => "Handshake",
            Some(GatewayServerMessage::State { .. }) => "State",
            Some(GatewayServerMessage::UnlockState { .. }) => "UnlockState",
            Some(GatewayServerMessage::Heartbeat { .. }) => "Heartbeat",
            Some(GatewayServerMessage::Unsupported { .. }) => "Unsupported",
            Some(GatewayServerMessage::ProviderState { .. }) => "ProviderState",
            Some(GatewayServerMessage::ProviderResponse { .. }) => "ProviderResponse",
            Some(GatewayServerMessage::UiSnapshot { .. }) => "UiSnapshot",
        };
        let now = Instant::now();
        let age_ms = now.duration_since(self.enqueued_at).as_millis();
        let queued_ms = self
            .first_progress_at
            .unwrap_or(now)
            .duration_since(self.enqueued_at)
            .as_millis();
        let flush_age_ms = self
            .flush_started_at
            .map(|started| now.duration_since(started).as_millis());
        tracing::debug!(
            session_id = id,
            reason,
            phase,
            message_kind,
            sealed = self.sealed,
            flushing = self.flushing,
            message_bytes = self.bytes,
            frames_remaining = self.frames.len(),
            age_ms,
            queued_ms,
            output_started = self.first_progress_at.is_some(),
            ?flush_age_ms,
            flush_pending_polls = self.flush_pending_polls,
            "gateway output rejected"
        );
    }
}

struct Challenge {
    allow_unlock: bool,
    code: PairingCode,
    generation: u64,
    expires: Instant,
    reservations: u8,
}

impl Challenge {
    fn reserve(&mut self, now: Instant) -> Option<PairingCode> {
        if now >= self.expires || self.reservations >= 3 {
            return None;
        }
        self.reservations += 1;
        PairingCode::new(*self.code.expose_for_display()).ok()
    }
}

struct Bucket {
    tokens: u32,
    updated: Instant,
}
impl Bucket {
    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.duration_since(self.updated).as_secs() / 10;
        if elapsed > 0 {
            self.tokens = (u64::from(self.tokens) + elapsed).min(6) as u32;
            self.updated += Duration::from_secs(elapsed * 10);
        }
        if self.tokens == 0 {
            false
        } else {
            self.tokens -= 1;
            true
        }
    }
}

struct Session<S = TcpStream> {
    socket: WebSocketStream<S>,
    outbound: VecDeque<Outbound>,
    outbound_bytes: usize,
    protocol: Option<Connection>,
    challenge: Option<u64>,
    deadline: Instant,
    heartbeat: Instant,
}
impl<S: AsyncRead + AsyncWrite + Unpin> Session<S> {
    fn peer(&self) -> Option<PeerId> {
        self.protocol
            .as_ref()
            .and_then(Connection::authenticated_peer_id)
    }
    fn enqueue(&mut self, outbound: Outbound) -> Result<(), GatewayError> {
        if self.outbound.len() >= MAX_OUTBOUND_MESSAGES
            || self.outbound_bytes.saturating_add(outbound.bytes) > MAX_OUTBOUND_BYTES
        {
            tracing::debug!(
                reason = "queue_capacity",
                queued_messages = self.outbound.len(),
                "gateway output enqueue rejected"
            );
            return Err(GatewayError::Unavailable);
        }
        self.outbound_bytes += outbound.bytes;
        self.outbound.push_back(outbound);
        Ok(())
    }
    fn send(&mut self, bytes: Vec<u8>) -> Result<(), GatewayError> {
        self.enqueue(Outbound {
            bytes: bytes.len(),
            delivery: None,
            frames: VecDeque::from([bytes]),
            sealed: true,
            flushing: false,
            deadline: self.deadline,
            enqueued_at: Instant::now(),
            first_progress_at: None,
            flush_started_at: None,
            flush_pending_polls: 0,
        })
    }
    fn application(&mut self, message: GatewayServerMessage) -> Result<(), GatewayError> {
        self.deliver(Delivery::control(message))
    }
    fn deliver(&mut self, delivery: Delivery) -> Result<(), GatewayError> {
        let bytes = serde_json::to_vec(&delivery.message)
            .map_err(|_| GatewayError::Unavailable)?
            .len();
        let enqueued_at = Instant::now();
        self.enqueue(Outbound {
            delivery: Some(delivery),
            frames: VecDeque::new(),
            bytes,
            sealed: false,
            flushing: false,
            deadline: enqueued_at + WRITE_TIMEOUT,
            enqueued_at,
            first_progress_at: None,
            flush_started_at: None,
            flush_pending_polls: 0,
        })
    }
    fn current(&mut self, id: u64, provider: &DappProvider) -> bool {
        self.outbound.front_mut().is_none_or(|output| {
            if !output.sealed {
                return true;
            }
            if Instant::now() >= output.deadline {
                output.log_failure(id, "write_deadline", "current");
                return false;
            }
            let current = output
                .delivery
                .as_mut()
                .is_none_or(|delivery| provider.delivery(id, delivery) == DeliveryStatus::Current);
            if !current {
                output.log_failure(id, "delivery_invalidated", "current");
            }
            current
        })
    }
    /// Send and flush at most one frame per actor turn without awaiting readiness.
    fn poll_output(
        &mut self,
        id: u64,
        provider: &DappProvider,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<Option<u64>, GatewayError>> {
        if !self.current(id, provider) {
            return Poll::Ready(Err(GatewayError::Unavailable));
        }
        let Some(output) = self.outbound.front_mut() else {
            return Poll::Pending;
        };
        if Instant::now() >= output.deadline {
            output.log_failure(id, "write_deadline", "poll_output");
            return Poll::Ready(Err(GatewayError::Unavailable));
        }
        output.first_progress_at.get_or_insert_with(Instant::now);
        if !output.sealed
            && provider.delivery(id, output.delivery.as_mut().expect("application delivery"))
                == DeliveryStatus::Discard
        {
            let output = self.outbound.pop_front().expect("queued output");
            self.outbound_bytes -= output.bytes;
            return Poll::Ready(Ok(output
                .delivery
                .and_then(|delivery| delivery.ticket_id())));
        }
        // Approval controls were checked before this guard. Keep immediate wallet
        // authority stable through encryption, submission and the first flush.
        let authority = provider.authority().borrow();
        if let Some(delivery) = output.delivery.as_mut() {
            let status = provider.delivery_authority(id, delivery, &authority);
            if output.sealed && status != DeliveryStatus::Current {
                output.log_failure(id, "live_authority_invalidated", "poll_output");
                return Poll::Ready(Err(GatewayError::Unavailable));
            }
            if status == DeliveryStatus::Discard {
                let output = self.outbound.pop_front().expect("queued output");
                self.outbound_bytes -= output.bytes;
                return Poll::Ready(Ok(output
                    .delivery
                    .and_then(|delivery| delivery.ticket_id())));
            }
        }
        if !output.flushing {
            match Pin::new(&mut self.socket).poll_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(_)) => {
                    output.log_failure(id, "socket_ready_failed", "poll_output");
                    return Poll::Ready(Err(GatewayError::Unavailable));
                }
                Poll::Ready(Ok(())) => {}
            }
            if !output.sealed {
                let delivery = output.delivery.as_ref().expect("application delivery");
                let bytes =
                    serde_json::to_vec(&delivery.message).map_err(|_| GatewayError::Unavailable)?;
                output.frames = self
                    .protocol
                    .as_mut()
                    .ok_or(GatewayError::Unavailable)?
                    .seal_message(&bytes)
                    .map_err(|_| GatewayError::Unavailable)?
                    .into();
                output.sealed = true;
                if let Some(deadline) = delivery.deadline() {
                    output.deadline = output.deadline.min(deadline);
                }
            }
            if Instant::now() >= output.deadline {
                output.log_failure(id, "write_deadline", "poll_output");
                return Poll::Ready(Err(GatewayError::Unavailable));
            }
            let frame = output.frames.pop_front().expect("sealed frame");
            Pin::new(&mut self.socket)
                .start_send(Message::Binary(frame.into()))
                .map_err(|_| {
                    output.log_failure(id, "socket_send_failed", "poll_output");
                    GatewayError::Unavailable
                })?;
            output.flushing = true;
            output.flush_started_at = Some(Instant::now());
            output.flush_pending_polls = 0;
        }
        // A ready socket can finish small State/UI messages in this same turn.
        // Backpressure and additional fragments still yield with all fences intact.
        match Pin::new(&mut self.socket).poll_flush(cx) {
            Poll::Pending => {
                output.flush_pending_polls = output.flush_pending_polls.saturating_add(1);
                return Poll::Pending;
            }
            Poll::Ready(Err(_)) => {
                output.log_failure(id, "socket_flush_failed", "poll_output");
                return Poll::Ready(Err(GatewayError::Unavailable));
            }
            Poll::Ready(Ok(())) => {}
        }
        output.flushing = false;
        output.flush_started_at = None;
        output.flush_pending_polls = 0;
        if output.frames.is_empty() {
            let output = self.outbound.pop_front().expect("flushed output");
            self.outbound_bytes -= output.bytes;
            return Poll::Ready(Ok(output
                .delivery
                .and_then(|delivery| delivery.ticket_id())));
        }
        Poll::Ready(Ok(None))
    }
}

enum Progress {
    Incoming(
        u64,
        Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ),
    Read(Result<ReadCompletion, tokio::task::JoinError>),
    Write(u64, Result<Option<u64>, GatewayError>),
}

#[derive(Default)]
struct ProgressCursor {
    next_source: usize,
    last_input: u64,
    last_output: u64,
}
impl ProgressCursor {
    fn poll<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        sessions: &mut HashMap<u64, Session<S>>,
        provider: &mut DappProvider,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Progress> {
        // A ready input source cannot exclude completions or output. Each selected
        // source advances one event, with a separate rotation among its sessions.
        for offset in 0..3 {
            let source = (self.next_source + offset) % 3;
            let progress = match source {
                0 => {
                    let mut ids: Vec<_> = sessions.keys().copied().collect();
                    ids.sort_unstable_by_key(|id| (*id <= self.last_input, *id));
                    let mut progress = Poll::Pending;
                    for id in ids {
                        let session = sessions.get_mut(&id).expect("live session");
                        // Reading may flush queued pong/close frames and shared write buffers.
                        // Retire stale ciphertext before entering any socket operation.
                        if !session.current(id, provider) {
                            progress =
                                Poll::Ready(Progress::Write(id, Err(GatewayError::Unavailable)));
                            break;
                        }
                        let authority = provider.authority().borrow();
                        if session.outbound.front_mut().is_some_and(|output| {
                            output.sealed
                                && output.delivery.as_mut().is_some_and(|delivery| {
                                    provider.delivery_authority(id, delivery, &authority)
                                        != DeliveryStatus::Current
                                })
                        }) {
                            if let Some(output) = session.outbound.front() {
                                output.log_failure(id, "live_authority_invalidated", "poll_input");
                            }
                            progress =
                                Poll::Ready(Progress::Write(id, Err(GatewayError::Unavailable)));
                            break;
                        }
                        if let Poll::Ready(message) = Pin::new(&mut session.socket).poll_next(cx) {
                            self.last_input = id;
                            progress = Poll::Ready(Progress::Incoming(id, message));
                            break;
                        }
                    }
                    progress
                }
                1 => match provider.jobs.poll_join_next(cx) {
                    Poll::Ready(Some(completion)) => Poll::Ready(Progress::Read(completion)),
                    _ => Poll::Pending,
                },
                2 => {
                    let mut ids: Vec<_> = sessions.keys().copied().collect();
                    ids.sort_unstable_by_key(|id| (*id <= self.last_output, *id));
                    let mut progress = Poll::Pending;
                    for id in ids {
                        let session = sessions.get_mut(&id).expect("live session");
                        if let Poll::Ready(result) = session.poll_output(id, provider, cx) {
                            self.last_output = id;
                            progress = Poll::Ready(Progress::Write(id, result));
                            break;
                        }
                    }
                    progress
                }
                _ => unreachable!("three progress sources"),
            };
            if progress.is_ready() {
                self.next_source = (source + 1) % 3;
                return progress;
            }
        }
        Poll::Pending
    }
}

struct Actor {
    unlocks: super::unlock::Unlocks,
    provider: DappProvider,
    db: Arc<DbStore>,
    registry: Registry,
    activity_dirty: bool,
    next_activity_flush: Instant,
    storage_error: Option<GatewayError>,
    error: Option<GatewayError>,
    listener: Option<TcpListener>,
    upgrades: JoinSet<Option<(Socket, Instant)>>,
    sessions: HashMap<u64, Session>,
    next_session: u64,
    /// Versions reported by paired browsers this process has seen. Never persisted.
    extension_versions: HashMap<[u8; 16], String>,
    ui_sessions: HashMap<u64, Arc<std::sync::atomic::AtomicBool>>,
    ui_events: tokio::sync::broadcast::Sender<super::GatewayUiEvent>,
    progress: ProgressCursor,
    challenge: Option<Challenge>,
    challenge_generation: u64,
    bucket: Bucket,
    locked: bool,
    generation: u64,
    started: Instant,
    updates: watch::Sender<GatewaySnapshot>,
    bundle: GatewayInstallBundle,
}

pub(super) async fn run(
    db: Arc<DbStore>,
    mut commands: mpsc::Receiver<Command>,
    updates: watch::Sender<GatewaySnapshot>,
    locked: bool,
    generation: u64,
    approval_updates: watch::Sender<Vec<Arc<super::GatewayApprovalRequest>>>,
    authority: watch::Receiver<super::GatewayWalletState>,
    ui_events: tokio::sync::broadcast::Sender<super::GatewayUiEvent>,
    switch_updates: watch::Sender<Vec<Arc<super::GatewayWalletSwitchRequest>>>,
    bundle: GatewayInstallBundle,
) {
    let loaded = Registry::load(&db);
    let storage_error = loaded.as_ref().err().copied();
    let now = Instant::now();
    let mut provider = DappProvider::new(
        crate::vault::DesktopVaultStore::from_db(Arc::clone(&db)),
        generation,
    );
    provider.set_approval_channels(approval_updates, authority, updates.subscribe());
    provider.set_switch_channel(switch_updates);
    let mut actor = Actor {
        unlocks: super::unlock::Unlocks::default(),
        provider,
        db,
        registry: loaded.unwrap_or_default(),
        activity_dirty: false,
        next_activity_flush: now + ACTIVITY_FLUSH_INTERVAL,
        storage_error,
        error: storage_error,
        listener: None,
        upgrades: JoinSet::new(),
        sessions: HashMap::new(),
        next_session: 0,
        extension_versions: HashMap::new(),
        ui_sessions: HashMap::new(),
        ui_events,
        progress: ProgressCursor::default(),
        challenge: None,
        challenge_generation: 0,
        bucket: Bucket {
            tokens: 6,
            updated: now,
        },
        locked,
        generation,
        started: now,
        updates,
        bundle,
    };
    if storage_error.is_none() && actor.registry.config.enabled {
        actor.bind().await;
    }
    actor.publish();
    let mut ticks = tokio::time::interval(Duration::from_millis(100));
    loop {
        // Revalidate actor-observed invalidation before further affected output.
        // Keep outer selection fair even while the listener or commands remain ready.
        actor
            .sessions
            .retain(|id, session| session.current(*id, &actor.provider));
        actor.retire_provider_sessions();
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(Command::Shutdown(reply)) => { actor.shutdown().await; actor.publish(); let _ = reply.send(Ok(())); break; }
                    Some(command) => { actor.command(command).await; }
                    None => { actor.shutdown().await; break; }
                }
            }
            result = actor.upgrades.join_next(), if !actor.upgrades.is_empty() => {
                if let Some(Ok(Some((socket, deadline)))) = result
                    && deadline > Instant::now()
                {
                    actor.next_session += 1;
                    tracing::debug!(session_id = actor.next_session, "gateway transport accepted");
                    actor.sessions.insert(actor.next_session, Session { socket, outbound: VecDeque::new(), outbound_bytes: 0, protocol: None, challenge: None, deadline, heartbeat: Instant::now() + HEARTBEAT });
                }
            }
            accepted = async {
                match &actor.listener { Some(listener) => listener.accept().await, None => std::future::pending().await }
            } => {
                if let Ok((stream, _)) = accepted { actor.accept(stream); }
            }
            _ = ticks.tick() => { actor.tick(); }
            progress = poll_fn(|cx| actor.progress.poll(&mut actor.sessions, &mut actor.provider, cx)) => {
                let publish = matches!(&progress, Progress::Incoming(..) | Progress::Write(_, Err(_)));
                match progress {
                    Progress::Incoming(id, message) => {
                        if let Some(mut session) = actor.sessions.remove(&id) {
                            let phase = if session.peer().is_some() { "authenticated" } else { "handshake" };
                            let reason = match message {
                                Some(Ok(Message::Binary(bytes))) => {
                                    if actor.frame(id, &mut session, &bytes).is_ok() {
                                        actor.sessions.insert(id, session);
                                        None
                                    } else {
                                        Some("frame_processing_failed")
                                    }
                                }
                                Some(Ok(Message::Text(_))) => Some("unexpected_text"),
                                Some(Ok(Message::Ping(_))) => Some("unexpected_ping"),
                                Some(Ok(Message::Pong(_))) => Some("unexpected_pong"),
                                Some(Ok(Message::Close(_))) => Some("websocket_close"),
                                Some(Ok(Message::Frame(_))) => Some("unexpected_raw_frame"),
                                Some(Err(_)) => Some("socket_read_failed"),
                                None => Some("socket_eof"),
                            };
                            if let Some(reason) = reason {
                                tracing::debug!(session_id = id, reason, phase, "gateway session retired");
                            }
                        }
                    }
                    Progress::Read(Ok(completion)) => actor.provider.complete_read(completion),
                    Progress::Read(Err(_)) => {},
                    Progress::Write(id, result) => {
                        match result {
                            Ok(Some(ticket)) => actor.provider.delivered(ticket),
                            Ok(None) => {},
                            Err(_) => {
                                tracing::debug!(session_id = id, reason = "output_failed", "gateway session retired");
                                actor.sessions.remove(&id);
                            }
                        }
                    }
                }
                actor.retire_provider_sessions();
                actor.flush_provider();
                if publish { actor.publish(); }
            }
        }
    }
}

impl Actor {
    fn publish(&self) {
        let snapshot = GatewaySnapshot {
            config: self.registry.config,
            listener_addr: self
                .listener
                .as_ref()
                .and_then(|listener| listener.local_addr().ok()),
            pairing_active: self
                .challenge
                .as_ref()
                .is_some_and(|code| Instant::now() < code.expires && code.reservations < 3),
            peers: self
                .registry
                .peers
                .iter()
                .map(|peer| GatewayPeerSummary {
                    allow_unlock: peer.allow_unlock,
                    id: PeerId::from_bytes(peer.id),
                    label: peer.label.clone(),
                    paired_at: peer.paired_at,
                    last_active_at: peer.last_active_at,
                    connected_sessions: self
                        .sessions
                        .values()
                        .filter(|session| session.peer().is_some_and(|id| id.to_bytes() == peer.id))
                        .count(),
                    extension_version: self.extension_versions.get(&peer.id).cloned(),
                })
                .collect(),
            locked: self.locked,
            generation: self.generation,
            error: self.error,
            permissions: self.provider.permissions(),
        };
        self.updates.send_if_modified(|previous| {
            if *previous == snapshot {
                return false;
            }
            *previous = snapshot;
            true
        });
    }

    async fn shutdown(&mut self) {
        self.retire().await;
        // Destructive lifecycle cleanup waits for accepted broker submissions. Session
        // retirement removes delivery authority but cannot cancel shared broker work.
        while let Some(completion) = self.provider.jobs.join_next().await {
            if let Ok(completion) = completion {
                self.provider.complete_read(completion);
            }
        }
    }

    async fn retire(&mut self) {
        self.listener = None;
        self.challenge = None;
        tracing::debug!(
            reason = "gateway_retired",
            sessions = self.sessions.len(),
            "gateway sessions retired"
        );
        self.sessions.clear();
        self.retire_provider_sessions();
        self.upgrades.abort_all();
        while self.upgrades.join_next().await.is_some() {}
    }

    async fn bind(&mut self) {
        let config = self.registry.config;
        let port = config.port.get();
        let address = SocketAddr::new(config.bind_address, port);
        match TcpListener::bind(address).await {
            Ok(listener) => {
                self.listener = Some(listener);
                self.error = None;
            }
            Err(error) => {
                self.listener = None;
                self.error = Some(if error.kind() == std::io::ErrorKind::AddrInUse {
                    GatewayError::PortInUse(port)
                } else {
                    GatewayError::Listener
                });
            }
        }
    }

    async fn command(&mut self, command: Command) {
        match command {
            Command::NetworkResult(request, outcome) => {
                self.provider.complete_network_request(request, outcome);
                self.flush_provider();
            }
            Command::BeginWalletSwitch(id, reply) => {
                let _ = reply.send(self.provider.begin_wallet_switch(&id));
                self.flush_provider();
            }
            Command::RejectWalletSwitch(id) => {
                self.provider.reject_wallet_switch(&id);
                self.flush_provider();
            }
            Command::Summaries(wallet, generation, summaries) => {
                self.provider
                    .publish_summaries(&wallet, generation, summaries);
                self.flush_provider();
            }
            Command::Configure(config, reply) => {
                let result = self.configure(config).await;
                self.publish();
                let _ = reply.send(result);
            }
            Command::Pair(allow_unlock, reply) => {
                let result = self.issue_code(allow_unlock);
                self.publish();
                let _ = reply.send(result);
            }
            Command::AllowUnlock(peer, allowed, reply) => {
                let result = self.set_allow_unlock(peer, allowed);
                self.publish();
                let _ = reply.send(result);
            }
            Command::Revoke(peer, reply) => {
                let result = self.revoke(peer);
                self.publish();
                let _ = reply.send(result);
            }
            Command::State(locked, generation, reply) => {
                let result = if generation == self.generation && locked != self.locked {
                    Err(GatewayError::Unavailable)
                } else if generation >= self.generation {
                    self.locked = locked;
                    self.generation = generation;
                    // Legacy lock-only callers cannot retain an earlier view capability.
                    self.provider
                        .update_wallet(super::GatewayWalletState::default(), generation);
                    self.broadcast_state();
                    self.flush_provider();
                    Ok(())
                } else {
                    Ok(())
                };
                self.publish();
                let _ = reply.send(result);
            }
            Command::WalletState(wallet, generation, reply) => {
                let result = if generation > self.generation
                    || (generation == self.generation && self.provider.same_authority(&wallet))
                {
                    self.locked = wallet.view.is_none();
                    self.generation = generation;
                    self.provider.update_wallet(*wallet, generation);
                    self.broadcast_state();
                    self.flush_provider();
                    Ok(())
                } else if generation < self.generation {
                    Ok(())
                } else {
                    Err(GatewayError::Unavailable)
                };
                self.publish();
                let _ = reply.send(result);
            }
            Command::RevokePermission(permission, reply) => {
                let result = self.provider.revoke(&permission);
                self.flush_provider();
                self.publish();
                let _ = reply.send(result);
            }
            Command::ReturnApprovalToReview(id, reply) => {
                let _ = reply.send(self.provider.return_approval_to_review(&id));
                self.flush_provider();
            }
            Command::BeginApproval(id, reply) => {
                let _ = reply.send(self.provider.begin_approval(&id));
                self.flush_provider();
            }
            Command::CompleteApproval(id, result, reply) => {
                let _ = reply.send(self.provider.complete_approval(&id, result));
                self.flush_provider();
            }
            Command::ApprovalRead(id, endpoint, rpc, entered, reply) => {
                self.provider
                    .approval_read(&id, endpoint, rpc, entered, reply);
                self.flush_provider();
            }
            Command::Shutdown(_) => unreachable!("shutdown handled in actor loop"),
        }
    }

    async fn configure(&mut self, config: GatewayConfig) -> Result<(), GatewayError> {
        if let Some(error) = self.storage_error {
            return Err(error);
        }
        let mut candidate = self.registry.clone();
        candidate.config = config;
        candidate.save(&self.db)?;
        self.registry = candidate;
        self.activity_dirty = false;
        self.retire().await;
        self.error = None;
        if config.enabled {
            self.bind().await;
        }
        self.error.map_or(Ok(()), Err)
    }

    fn issue_code(&mut self, allow_unlock: bool) -> Result<GatewayPairingOffer, GatewayError> {
        if let Some(error) = self.storage_error {
            return Err(error);
        }
        if self.listener.is_none() {
            return Err(GatewayError::Disabled);
        }
        if self.registry.peers.len() >= MAX_PEERS {
            return Err(GatewayError::Unavailable);
        }
        let mut digits = [0; 6];
        // Rejection sampling avoids modulo bias and preserves leading zeros.
        for digit in &mut digits {
            loop {
                let mut byte = [0];
                getrandom::fill(&mut byte).map_err(|_| GatewayError::Unavailable)?;
                if byte[0] < 250 {
                    *digit = b'0' + byte[0] % 10;
                    break;
                }
            }
        }
        self.challenge_generation = self
            .challenge_generation
            .checked_add(1)
            .ok_or(GatewayError::Unavailable)?;
        let code = PairingCode::new(digits).map_err(|_| GatewayError::Unavailable)?;
        self.challenge = Some(Challenge {
            allow_unlock,
            code,
            generation: self.challenge_generation,
            expires: Instant::now() + CODE_LIFETIME,
            reservations: 0,
        });
        // Attempts admitted under earlier codes have no remaining authority.
        self.sessions
            .retain(|_, session| session.challenge.is_none() || session.peer().is_some());
        Ok(GatewayPairingOffer {
            code: PairingCode::new(digits).map_err(|_| GatewayError::Unavailable)?,
            expires_in_secs: CODE_LIFETIME.as_secs(),
        })
    }

    fn set_allow_unlock(&mut self, id: PeerId, allowed: bool) -> Result<(), GatewayError> {
        if let Some(error) = self.storage_error {
            return Err(error);
        }
        let mut candidate = self.registry.clone();
        let peer = candidate
            .peers
            .iter_mut()
            .find(|peer| peer.id == id.to_bytes())
            .ok_or(GatewayError::Unavailable)?;
        peer.allow_unlock = allowed;
        candidate.save(&self.db)?;
        self.registry = candidate;
        self.activity_dirty = false;
        self.retire_provider_sessions();
        self.publish_unlocks();
        Ok(())
    }

    fn unlock_allowed(&self, peer: PeerId) -> bool {
        self.registry
            .peers
            .iter()
            .any(|entry| entry.id == peer.to_bytes() && entry.allow_unlock)
    }

    fn publish_unlocks(&mut self) {
        for (id, session) in &mut self.sessions {
            if !self.unlocks.sent.contains_key(id) {
                continue;
            }
            let Some(peer) = session.peer() else {
                continue;
            };
            let allowed = self
                .registry
                .peers
                .iter()
                .any(|entry| entry.id == peer.to_bytes() && entry.allow_unlock);
            let view = self.unlocks.view(*id, allowed);
            if self.unlocks.sent.get(id) != Some(&view)
                && session
                    .application(GatewayServerMessage::UnlockState {
                        version: 1,
                        generation: self.generation,
                        response_to: None,
                        view: view.clone(),
                    })
                    .is_ok()
            {
                self.unlocks.sent.insert(*id, view);
            }
        }
    }

    fn revoke(&mut self, id: PeerId) -> Result<(), GatewayError> {
        if let Some(error) = self.storage_error {
            return Err(error);
        }
        let mut candidate = self.registry.clone();
        candidate.peers.retain(|peer| peer.id != id.to_bytes());
        candidate.save(&self.db)?;
        self.registry = candidate;
        self.activity_dirty = false;
        self.extension_versions.remove(&id.to_bytes());
        // This includes handshakes that looked up the old credential but have not confirmed.
        self.sessions.retain(|_, session| {
            session
                .protocol
                .as_ref()
                .is_none_or(|protocol| protocol.pending_peer_id() != id)
        });
        self.retire_provider_sessions();
        Ok(())
    }

    fn accept(&mut self, stream: TcpStream) {
        let unauthenticated = self
            .sessions
            .values()
            .filter(|session| session.peer().is_none())
            .count()
            + self.upgrades.len();
        if self.sessions.len() + self.upgrades.len() >= MAX_CONNECTIONS
            || unauthenticated >= MAX_UNAUTHENTICATED
        {
            return;
        }
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let bundle = self.bundle;
        self.upgrades.spawn(async move {
            // Browsers reach this port for the install page, so classify before the handshake.
            let head = timeout_at(deadline, install::peek_head(&stream))
                .await
                .ok()?;
            if head.as_deref().is_some_and(install::is_websocket_upgrade) {
                let config = WebSocketConfig::default()
                    .read_buffer_size(4096)
                    .write_buffer_size(4096)
                    .max_write_buffer_size(128 * 1024)
                    .max_frame_size(Some(MAX_RECORD_LEN))
                    .max_message_size(Some(MAX_RECORD_LEN))
                    .accept_unmasked_frames(false);
                return timeout_at(deadline, accept_async_with_config(stream, Some(config)))
                    .await
                    .ok()?
                    .ok()
                    .map(|socket| (socket, deadline));
            }
            // The page names the address the browser actually reached, not the one it asked for.
            let Ok(local) = stream.local_addr() else {
                tracing::debug!("gateway install request without local address");
                return None;
            };
            // A v4-mapped client on a dual-stack listener has to read as the IPv4 address it used.
            let local = SocketAddr::new(local.ip().to_canonical(), local.port());
            let head = head.unwrap_or_default();
            let response = install::respond(&head, bundle, local);
            let served = tokio::time::timeout(
                INSTALL_RESPONSE_TIMEOUT,
                install::serve(stream, head.len(), &response),
            )
            .await;
            if !matches!(served, Ok(Ok(()))) {
                tracing::debug!("gateway install response not delivered");
            }
            None
        });
    }

    const fn state(&self) -> GatewayServerMessage {
        GatewayServerMessage::State {
            version: 1,
            locked: self.locked,
            generation: self.generation,
            wallet_transition: self.provider.wallet_transition(),
            client_info_supported: true,
            unlock_supported: true,
        }
    }

    fn frame(&mut self, id: u64, session: &mut Session, bytes: &[u8]) -> Result<(), GatewayError> {
        let now = Instant::now();
        if session.peer().is_none() && (now >= session.deadline || bytes.len() > MAX_HANDSHAKE_LEN)
        {
            return Err(GatewayError::Unavailable);
        }
        if session.protocol.is_none() {
            if !self.bucket.take(now) {
                return Err(GatewayError::Unavailable);
            }
            let hello = ClientHello::decode(bytes).map_err(|_| GatewayError::Unavailable)?;
            let auth = if hello.is_pairing() {
                let challenge = self.challenge.as_mut().ok_or(GatewayError::Unavailable)?;
                let code = challenge.reserve(now).ok_or(GatewayError::Unavailable)?;
                session.challenge = Some(challenge.generation);
                let mut id = [0; 16];
                getrandom::fill(&mut id).map_err(|_| GatewayError::Unavailable)?;
                if self.registry.peers.iter().any(|peer| peer.id == id) {
                    return Err(GatewayError::Unavailable);
                }
                ServerAuth::Pair {
                    code,
                    peer_id: PeerId::from_bytes(id),
                }
            } else {
                let peer = self
                    .registry
                    .peers
                    .iter()
                    .find(|peer| peer.id == hello.claimed_peer_id().to_bytes())
                    .ok_or(GatewayError::Unavailable)?;
                ServerAuth::Reconnect(SessionSecret::from_storage(peer.secret.0))
            };
            let (protocol, output) =
                Connection::server(hello, auth).map_err(|_| GatewayError::Unavailable)?;
            session.protocol = Some(protocol);
            return session.send(output);
        }
        if session.peer().is_some() {
            let now_ms = self
                .started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            let complete = session
                .protocol
                .as_mut()
                .ok_or(GatewayError::Unavailable)?
                .receive_frame(bytes, now_ms)
                .map_err(|error| {
                    tracing::debug!(
                        session_id = id,
                        reason = "protocol_frame_rejected",
                        ?error,
                        "gateway frame rejected"
                    );
                    GatewayError::Unavailable
                })?;
            if let Some(message) = complete {
                self.record_peer_activity(session.peer().ok_or(GatewayError::Unavailable)?);
                let message = zeroize::Zeroizing::new(message);
                let command = serde_json::from_slice::<GatewayClientMessage>(&message);
                drop(message);
                Self::trace_extension_command(id, command.as_ref().ok());
                match command {
                    Ok(GatewayClientMessage::GetUnlockState { version: 1 }) => {
                        let view = self.unlocks.view(
                            id,
                            self.unlock_allowed(session.peer().ok_or(GatewayError::Unavailable)?),
                        );
                        session.application(GatewayServerMessage::UnlockState {
                            version: 1,
                            generation: self.generation,
                            response_to: None,
                            view: view.clone(),
                        })?;
                        self.unlocks.sent.insert(id, view);
                    }
                    Ok(GatewayClientMessage::Unlock {
                        version: 1,
                        generation,
                        attempt_id,
                        command,
                    }) => {
                        if attempt_id.is_empty() || attempt_id.len() > 128 {
                            return Err(GatewayError::Unavailable);
                        }
                        let peer = session.peer().ok_or(GatewayError::Unavailable)?;
                        let allowed = self.unlock_allowed(peer);
                        let response_to = Some(attempt_id.clone());
                        if allowed
                            && generation == self.generation
                            && self.locked
                            && let Some(request) =
                                self.unlocks.command(id, peer, &attempt_id, command)
                        {
                            self.emit_ui_event(
                                id,
                                generation,
                                super::GatewayUiEventKind::Unlock { request },
                            );
                        }
                        let view = self
                            .unlocks
                            .view(id, allowed)
                            .response(response_to.as_deref().expect("unlock response owner"));
                        session.application(GatewayServerMessage::UnlockState {
                            version: 1,
                            generation: self.generation,
                            response_to,
                            view: view.clone(),
                        })?;
                        self.unlocks.sent.insert(id, view);
                    }

                    Ok(GatewayClientMessage::Network {
                        version: 1,
                        generation,
                        context_revision,
                        command,
                    }) => {
                        if let Some(request) = self.provider.network_command(
                            id,
                            session.peer().ok_or(GatewayError::Unavailable)?,
                            generation,
                            &context_revision,
                            command,
                        ) {
                            self.emit_ui_event(
                                id,
                                generation,
                                super::GatewayUiEventKind::Network { request },
                            );
                        }
                    }
                    Ok(GatewayClientMessage::PrivateView {
                        version: 1,
                        generation,
                        command,
                    }) => {
                        if let Some(command) = self.provider.private_command(
                            id,
                            session.peer().ok_or(GatewayError::Unavailable)?,
                            generation,
                            command,
                        ) {
                            self.emit_ui_event(
                                id,
                                generation,
                                super::GatewayUiEventKind::PrivateView { command },
                            );
                        }
                    }
                    Ok(GatewayClientMessage::PublicView {
                        version: 1,
                        generation,
                        command,
                    }) => {
                        if let Some(command) = self.provider.public_command(
                            id,
                            session.peer().ok_or(GatewayError::Unavailable)?,
                            generation,
                            command,
                        ) {
                            let peer_id = alloy::hex::encode(
                                session.peer().ok_or(GatewayError::Unavailable)?.to_bytes(),
                            );
                            self.emit_ui_event(
                                id,
                                generation,
                                super::GatewayUiEventKind::PublicView { peer_id, command },
                            );
                        }
                    }
                    Ok(GatewayClientMessage::GetState { version: 1 }) => {
                        session.application(self.state())?;
                    }
                    Ok(GatewayClientMessage::ClientInfo {
                        version: 1,
                        extension_version,
                    }) => {
                        // Unusable text is dropped rather than shown or answered as unsupported.
                        if let Some(version) = accepted_extension_version(&extension_version) {
                            let peer = session.peer().ok_or(GatewayError::Unavailable)?;
                            self.extension_versions.insert(peer.to_bytes(), version);
                        }
                    }
                    Ok(GatewayClientMessage::Heartbeat { version: 1 }) => {
                        session.application(GatewayServerMessage::Heartbeat { version: 1 })?;
                    }
                    Ok(GatewayClientMessage::RegisterDocument {
                        version: 1,
                        document,
                        url,
                    }) => self.provider.register(
                        id,
                        session.peer().ok_or(GatewayError::Unavailable)?,
                        document,
                        &url,
                    )?,
                    Ok(GatewayClientMessage::UnregisterDocument {
                        version: 1,
                        document,
                    }) => self.provider.unregister(id, &document),
                    Ok(GatewayClientMessage::ProviderRequest {
                        version: 1,
                        document,
                        request_id,
                        method,
                        params,
                    }) => self
                        .provider
                        .request(id, document, request_id, &method, params, now)?,
                    Ok(GatewayClientMessage::RequestWalletSwitch {
                        version: 1,
                        generation,
                        request_id,
                    }) => {
                        self.provider.request_wallet_switch(
                            id,
                            session.peer().ok_or(GatewayError::Unavailable)?,
                            generation,
                            &request_id,
                            now,
                        );
                    }
                    Ok(GatewayClientMessage::SummonDesktop {
                        version: 1,
                        generation,
                    }) => {
                        self.emit_ui_event(
                            id,
                            generation,
                            super::GatewayUiEventKind::SummonDesktop,
                        );
                    }
                    Ok(GatewayClientMessage::UserActivity {
                        version: 1,
                        generation,
                    }) => {
                        self.emit_ui_event(id, generation, super::GatewayUiEventKind::UserActivity);
                    }
                    Ok(GatewayClientMessage::ResolveConnect {
                        version: 1,
                        request_id,
                        public_account_uuid,
                        chain_id,
                    }) => {
                        tracing::debug!(
                            session_id = id,
                            approved = public_account_uuid.is_some(),
                            "gateway connect decision received"
                        );
                        self.provider.resolve_connect(
                            id,
                            session.peer().ok_or(GatewayError::Unavailable)?,
                            &request_id,
                            public_account_uuid.as_deref(),
                            chain_id,
                            now,
                        );
                    }
                    _ => {
                        session.application(GatewayServerMessage::Unsupported { version: 1 })?;
                    }
                }
            }
            return Ok(());
        }
        if let Some(generation) = session.challenge
            && !self
                .challenge
                .as_ref()
                .is_some_and(|code| code.generation == generation && now < code.expires)
        {
            return Err(GatewayError::Unavailable);
        }
        let protocol = session.protocol.as_mut().ok_or(GatewayError::Unavailable)?;
        let mut step = protocol
            .receive_handshake(bytes)
            .map_err(|_| GatewayError::Unavailable)?;
        if step.event == Some(HandshakeEvent::PairingReadyToCommit) {
            // No await between revalidation, durable commit and invalidation of competing attempts.
            if !self.challenge.as_ref().is_some_and(|code| {
                Some(code.generation) == session.challenge && Instant::now() < code.expires
            }) {
                return Err(GatewayError::Unavailable);
            }
            let secret = protocol
                .pending_credential()
                .ok_or(GatewayError::Unavailable)?;
            let paired_at = epoch_secs();
            let mut candidate = self.registry.clone();
            candidate.peers.push(Peer {
                id: protocol.pending_peer_id().to_bytes(),
                secret: StoredSecret(secret.export_for_storage()),
                label: None,
                allow_unlock: self
                    .challenge
                    .as_ref()
                    .is_some_and(|code| code.allow_unlock),
                paired_at,
                last_active_at: paired_at,
            });
            candidate.save(&self.db)?;
            self.registry = candidate;
            self.activity_dirty = false;
            self.challenge = None;
            self.sessions
                .retain(|_, other| other.challenge.is_none() || other.peer().is_some());
            step = protocol
                .commit_pairing()
                .map_err(|_| GatewayError::Unavailable)?;
        }
        if let Some(output) = step.outbound {
            session.send(output)?;
        }
        if step.event == Some(HandshakeEvent::Authenticated) {
            self.record_peer_activity(session.peer().ok_or(GatewayError::Unavailable)?);
            tracing::debug!(session_id = id, "gateway session authenticated");
            session.application(self.state())?;
            self.provider
                .attach_ui_peer(id, session.peer().ok_or(GatewayError::Unavailable)?);
            session.heartbeat = Instant::now() + HEARTBEAT;
        }
        Ok(())
    }

    fn trace_extension_command(session_id: u64, message: Option<&GatewayClientMessage>) {
        let Some(message) = message else {
            tracing::debug!(
                session_id,
                command = "invalid",
                "gateway extension command received"
            );
            return;
        };
        let (command, version) = match message {
            GatewayClientMessage::GetUnlockState { version } => ("get_unlock_state", version),
            GatewayClientMessage::Unlock { version, .. } => ("unlock", version),
            GatewayClientMessage::Network { version, .. } => ("network", version),
            GatewayClientMessage::PrivateView { version, .. } => ("private_view", version),
            GatewayClientMessage::PublicView { version, .. } => ("public_view", version),
            GatewayClientMessage::GetState { version } => ("get_state", version),
            GatewayClientMessage::ClientInfo { version, .. } => ("client_info", version),
            GatewayClientMessage::Heartbeat { version } => ("heartbeat", version),
            GatewayClientMessage::RegisterDocument { version, .. } => {
                ("register_document", version)
            }
            GatewayClientMessage::UnregisterDocument { version, .. } => {
                ("unregister_document", version)
            }
            GatewayClientMessage::ProviderRequest {
                version, method, ..
            } => {
                // Bound diagnostic metadata without changing which methods the provider accepts.
                let rpc_method = if method.len() <= 128
                    && method
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.'))
                {
                    method.as_str()
                } else {
                    "<redacted>"
                };
                tracing::debug!(
                    session_id,
                    command = "provider_request",
                    version = *version,
                    rpc_method,
                    "gateway extension command received"
                );
                return;
            }
            GatewayClientMessage::RequestWalletSwitch { version, .. } => {
                ("request_wallet_switch", version)
            }
            GatewayClientMessage::SummonDesktop { version, .. } => ("summon_desktop", version),
            GatewayClientMessage::UserActivity { version, .. } => ("user_activity", version),
            GatewayClientMessage::ResolveConnect { version, .. } => ("resolve_connect", version),
        };
        tracing::debug!(
            session_id,
            command,
            version = *version,
            "gateway extension command received"
        );
    }

    fn emit_ui_event(&mut self, id: u64, generation: u64, kind: super::GatewayUiEventKind) {
        let live = self
            .ui_sessions
            .entry(id)
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicBool::new(true)));
        if let Some(event) = self.provider.ui_event(generation, kind, live) {
            let _ = self.ui_events.send(event);
        }
    }

    fn retire_provider_sessions(&mut self) {
        let sessions = &self.sessions;
        let peers = &self.registry.peers;
        self.unlocks.retire(|session, peer| {
            sessions
                .get(&session)
                .is_some_and(|session| session.peer() == Some(peer))
                && peers
                    .iter()
                    .any(|entry| entry.id == peer.to_bytes() && entry.allow_unlock)
        });
        self.unlocks.sent.retain(|id, _| sessions.contains_key(id));
        self.ui_sessions.retain(|id, live| {
            let current = self
                .sessions
                .get(id)
                .is_some_and(|session| session.peer().is_some());
            if !current {
                live.store(false, std::sync::atomic::Ordering::Release);
            }
            current
        });
        let sessions = &self.sessions;
        self.provider
            .retire_sessions(|id| sessions.contains_key(&id));
    }

    fn broadcast_state(&mut self) {
        let ids: Vec<_> = self.sessions.keys().copied().collect();
        for id in ids {
            if let Some(mut session) = self.sessions.remove(&id)
                && (session.peer().is_none() || session.application(self.state()).is_ok())
            {
                self.sessions.insert(id, session);
            }
        }
        self.retire_provider_sessions();
    }

    fn flush_provider(&mut self) {
        loop {
            let messages = self.provider.drain();
            if messages.is_empty() {
                break;
            }
            for (id, delivery) in messages {
                let ticket = delivery.ticket_id();
                let queued = self
                    .sessions
                    .get_mut(&id)
                    .is_some_and(|session| session.deliver(delivery).is_ok());
                if !queued {
                    tracing::debug!(
                        session_id = id,
                        reason = "provider_output_not_queued",
                        "gateway session unavailable"
                    );
                    self.sessions.remove(&id);
                    self.retire_provider_sessions();
                    if let Some(ticket) = ticket {
                        self.provider.delivered(ticket);
                    }
                }
            }
        }
    }

    fn record_peer_activity(&mut self, id: PeerId) {
        let now = epoch_secs();
        if let Some(peer) = self
            .registry
            .peers
            .iter_mut()
            .find(|peer| peer.id == id.to_bytes())
            && now > peer.last_active_at
        {
            peer.last_active_at = now;
            self.activity_dirty = true;
        }
    }

    fn flush_peer_activity(&mut self, now: Instant) {
        if !self.activity_dirty || now < self.next_activity_flush {
            return;
        }
        // Failed best-effort writes use the same cadence and never retire sessions.
        self.next_activity_flush = now + ACTIVITY_FLUSH_INTERVAL;
        if self.registry.save(&self.db).is_ok() {
            self.activity_dirty = false;
        } else {
            tracing::warn!("gateway peer activity could not be saved");
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        self.flush_peer_activity(now);
        if self
            .challenge
            .as_ref()
            .is_some_and(|code| now >= code.expires)
        {
            self.challenge = None;
        }
        let now_ms = self
            .started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let ids: Vec<_> = self.sessions.keys().copied().collect();
        for id in ids {
            if let Some(mut session) = self.sessions.remove(&id) {
                if session.peer().is_none() {
                    if now < session.deadline {
                        self.sessions.insert(id, session);
                    } else {
                        tracing::debug!(
                            session_id = id,
                            reason = "handshake_deadline",
                            "gateway session retired"
                        );
                    }
                    continue;
                }
                if session
                    .protocol
                    .as_mut()
                    .is_none_or(|protocol| protocol.expire_assembly(now_ms).is_err())
                {
                    tracing::debug!(
                        session_id = id,
                        reason = "assembly_expired_or_protocol_missing",
                        "gateway session retired"
                    );
                    continue;
                }
                if now >= session.heartbeat {
                    if session
                        .application(GatewayServerMessage::Heartbeat { version: 1 })
                        .is_err()
                    {
                        tracing::debug!(
                            session_id = id,
                            reason = "heartbeat_enqueue_failed",
                            "gateway session retired"
                        );
                        continue;
                    }
                    session.heartbeat = now + HEARTBEAT;
                }
                self.sessions.insert(id, session);
            }
        }
        self.retire_provider_sessions();
        self.publish_unlocks();
        self.provider.tick(now);
        self.flush_provider();
        self.publish();
    }
}

/// Keeps a browser-supplied version short and numeric before it reaches the desktop UI.
fn accepted_extension_version(value: &str) -> Option<String> {
    if !(1..=32).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    let parts: Vec<&str> = value.split('.').collect();
    if !(1..=4).contains(&parts.len()) || parts.iter().any(|part| part.parse::<u64>().is_err()) {
        return None;
    }
    Some(value.to_owned())
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_include_aborted_attempts_and_expire() {
        let now = Instant::now();
        let mut code = Challenge {
            allow_unlock: false,
            code: PairingCode::new(*b"000123").unwrap(),
            generation: 1,
            expires: now + CODE_LIFETIME,
            reservations: 0,
        };
        for _ in 0..3 {
            assert!(code.reserve(now).is_some());
        }
        assert!(code.reserve(now).is_none());
        let mut replacement = Challenge {
            allow_unlock: false,
            code: PairingCode::new(*b"123456").unwrap(),
            generation: 2,
            expires: now + CODE_LIFETIME,
            reservations: 0,
        };
        assert!(replacement.reserve(now + CODE_LIFETIME).is_none());
    }

    #[test]
    fn listener_bucket_refills_without_resetting_for_new_codes() {
        let now = Instant::now();
        let mut bucket = Bucket {
            tokens: 6,
            updated: now,
        };
        for _ in 0..6 {
            assert!(bucket.take(now));
        }
        assert!(!bucket.take(now));
        assert!(!bucket.take(now + Duration::from_secs(9)));
        assert!(bucket.take(now + Duration::from_secs(10)));
        assert!(!bucket.take(now + Duration::from_secs(10)));
        assert!(bucket.take(now + Duration::from_secs(20)));
    }
}

#[cfg(test)]
mod integration_tests {
    use super::super::GatewayHandle;
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use local_db::DbConfig;
    use std::net::{Ipv4Addr, Ipv6Addr};

    async fn available_port() -> std::num::NonZeroU16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        std::num::NonZeroU16::new(listener.local_addr().unwrap().port()).unwrap()
    }

    async fn socket(address: std::net::SocketAddr) -> Socket {
        let stream = TcpStream::connect(address).await.unwrap();
        tokio_tungstenite::client_async(format!("ws://{address}"), stream)
            .await
            .unwrap()
            .0
    }

    async fn read(socket: &mut Socket) -> Vec<u8> {
        match tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Binary(bytes) => bytes.to_vec(),
            _ => panic!("expected binary protocol frame"),
        }
    }

    async fn pair(socket: &mut Socket, code: PairingCode) -> (Connection, PeerId, SessionSecret) {
        let (mut protocol, hello) = Connection::client_pair(code);
        socket.send(Message::Binary(hello.into())).await.unwrap();
        let mut credential = None;
        loop {
            let frame = read(socket).await;
            let step = protocol.receive_handshake(&frame).unwrap();
            if let Some(output) = step.outbound {
                socket.send(Message::Binary(output.into())).await.unwrap();
            }
            if step.event == Some(HandshakeEvent::CredentialReceived) {
                credential = Some(SessionSecret::from_storage(
                    protocol.pending_credential().unwrap().export_for_storage(),
                ));
                let ack = protocol.acknowledge_credential().unwrap();
                socket.send(Message::Binary(ack.into())).await.unwrap();
            }
            if step.event == Some(HandshakeEvent::Authenticated) {
                let peer = protocol.authenticated_peer_id().unwrap();
                return (protocol, peer, credential.unwrap());
            }
        }
    }

    async fn state(socket: &mut Socket, protocol: &mut Connection) -> serde_json::Value {
        let bytes = read(socket).await;
        let message = protocol.receive_frame(&bytes, 0).unwrap().unwrap();
        serde_json::from_slice(&message).unwrap()
    }

    async fn idle_ui_snapshot(
        socket: &mut Socket,
        protocol: &mut Connection,
        generation: u64,
        locked: bool,
    ) {
        let mut snapshot = state(socket, protocol).await;
        // Unlocked transport fixtures can already have networks and saved grants.
        // Provider tests cover their contents; this helper verifies idle/redacted UI.
        if locked {
            assert_eq!(snapshot["chains"], serde_json::json!([]));
            assert_eq!(snapshot["permissions"], serde_json::json!([]));
        }
        snapshot.as_object_mut().unwrap().remove("chains");
        snapshot.as_object_mut().unwrap().remove("permissions");
        assert_eq!(
            snapshot,
            serde_json::json!({
                "type": "ui_snapshot",
                "version": 1,
                "generation": generation,
                "locked": locked,
                "accounts": [],
                "private_view_supported": false,
                "private_view": null,
                "public_view": {
                    "selected_account": null,
                    "selected_chain": null,
                    "balances": [],
                    "drafts": [],
                    "refreshing": false,
                    "balance_error": false,
                },
                "ui_error": null,
                "pending_connects": [],
                "pending_requests": [],
            })
        );
    }

    async fn application(
        socket: &mut Socket,
        protocol: &mut Connection,
        message: serde_json::Value,
    ) {
        for frame in protocol
            .seal_message(&serde_json::to_vec(&message).unwrap())
            .unwrap()
        {
            socket.send(Message::Binary(frame.into())).await.unwrap();
        }
    }

    #[tokio::test]
    async fn paired_unlock_permission_is_enforced_and_revocation_invalidates_native_work() {
        async fn unlock_state(socket: &mut Socket, protocol: &mut Connection) -> serde_json::Value {
            loop {
                let value = state(socket, protocol).await;
                if value["type"] == "unlock_state" {
                    return value;
                }
            }
        }
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-unlock-{}", alloy::hex::encode(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
        handle
            .configure(GatewayConfig {
                enabled: true,
                bind_address: Ipv4Addr::LOCALHOST.into(),
                port: available_port().await,
            })
            .await
            .unwrap();
        let code = handle.issue_pairing_code_with_unlock(true).await.unwrap();
        let mut socket = socket(handle.snapshots().borrow().listener_addr.unwrap()).await;
        let (mut protocol, peer, _) = pair(&mut socket, code.code).await;
        state(&mut socket, &mut protocol).await;
        assert!(handle.snapshots().borrow().peers[0].allow_unlock);
        let mut events = handle.ui_events();
        handle.set_peer_allow_unlock(peer, false).await.unwrap();
        application(&mut socket, &mut protocol, serde_json::json!({"type":"unlock", "version":1, "generation":1, "attempt_id":"denied", "command":{"action":"password", "password":"synthetic password"}})).await;
        assert_eq!(
            unlock_state(&mut socket, &mut protocol).await["view"]["allowed"],
            false
        );
        assert!(
            events.try_recv().is_err(),
            "unpermitted input never reaches the vault owner"
        );
        handle.set_peer_allow_unlock(peer, true).await.unwrap();
        assert!(Registry::load(&db).unwrap().peers[0].allow_unlock);
        application(&mut socket, &mut protocol, serde_json::json!({"type":"unlock", "version":1, "generation":1, "attempt_id":"accepted", "command":{"action":"password", "password":"synthetic password"}})).await;
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        let super::super::GatewayUiEventKind::Unlock { request } = event.kind else {
            panic!("unlock request");
        };
        let (_, guard) = request.take().unwrap();
        guard.finish(super::super::GatewayUnlockPhase::Passphrase);
        handle.set_peer_allow_unlock(peer, false).await.unwrap();
        assert!(!guard.is_current());
        guard.finish(super::super::GatewayUnlockPhase::Complete);
        assert!(
            !guard.is_current(),
            "revoked work cannot regain installation authority"
        );
        drop(guard);
        handle.shutdown().await.unwrap();
        drop(socket);
        drop(handle);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn pairing_attempt(address: std::net::SocketAddr, admitted: bool) {
        let mut connection = socket(address).await;
        connection
            .send(Message::Binary(ClientHello::pair().encode().into()))
            .await
            .unwrap();
        if admitted {
            // Abort after the first server flight, before proving knowledge of the code.
            read(&mut connection).await;
        } else {
            let rejected = tokio::time::timeout(Duration::from_secs(5), connection.next())
                .await
                .unwrap();
            assert!(!matches!(rejected, Some(Ok(Message::Binary(_)))));
        }
    }

    #[tokio::test]
    async fn aborted_pairing_attempts_exhaust_codes_and_survive_listener_rebind() {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        // Finish before the listener's first ten-second token refill.
        tokio::time::timeout(Duration::from_secs(8), async {
            let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
            let enabled = GatewayConfig {
                enabled: true,
                bind_address: Ipv4Addr::LOCALHOST.into(),
                port: available_port().await,
            };
            handle.configure(enabled).await.unwrap();
            let address = handle.snapshots().borrow().listener_addr.unwrap();
            handle.issue_pairing_code().await.unwrap();
            for _ in 0..3 {
                pairing_attempt(address, true).await;
            }
            // Rejection for the exhausted code spends the fourth listener token.
            pairing_attempt(address, false).await;

            handle.issue_pairing_code().await.unwrap();
            for _ in 0..2 {
                pairing_attempt(address, true).await;
            }
            pairing_attempt(address, false).await;
            handle.issue_pairing_code().await.unwrap();
            pairing_attempt(address, false).await;

            handle
                .configure(GatewayConfig {
                    enabled: false,
                    ..enabled
                })
                .await
                .unwrap();
            assert!(handle.snapshots().borrow().listener_addr.is_none());
            handle.configure(enabled).await.unwrap();
            let address = handle.snapshots().borrow().listener_addr.unwrap();
            handle.issue_pairing_code().await.unwrap();
            pairing_attempt(address, false).await;
            handle.shutdown().await.unwrap();
        })
        .await
        .expect("pairing admission scenario must finish before token refill");
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Sends one plain HTTP request on the gateway listener and reads the whole response.
    async fn install_request(address: std::net::SocketAddr, target: &str) -> (String, Vec<u8>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut stream = TcpStream::connect(address).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        (
            String::from_utf8(response[..split].to_vec()).unwrap(),
            response[split + 4..].to_vec(),
        )
    }

    #[tokio::test]
    async fn install_page_and_bundle_are_served_beside_the_transport() {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let bundle = GatewayInstallBundle {
            zip: b"PK-test",
            version: "9.9.9",
        };
        let handle = GatewayHandle::start(db.clone(), true, 1, bundle);
        handle
            .configure(GatewayConfig {
                enabled: true,
                bind_address: Ipv4Addr::LOCALHOST.into(),
                port: available_port().await,
            })
            .await
            .unwrap();
        let address = handle.snapshots().borrow().listener_addr.unwrap();

        let (head, body) = install_request(address, "/install").await;
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(
            head.contains("Content-Type: text/html; charset=utf-8"),
            "{head}"
        );
        assert!(String::from_utf8(body).unwrap().contains("9.9.9"));

        let (head, body) = install_request(address, "/install/RailOxide-Extension.zip").await;
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert_eq!(body, b"PK-test");

        // The same listener keeps upgrading protocol clients.
        let mut connection = socket(address).await;
        connection.close(None).await.unwrap();

        handle.shutdown().await.unwrap();
        drop(handle);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reported_extension_versions_reach_the_snapshot_and_ignore_invalid_reports() {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
        handle
            .configure(GatewayConfig {
                enabled: true,
                bind_address: Ipv4Addr::LOCALHOST.into(),
                port: available_port().await,
            })
            .await
            .unwrap();
        let address = handle.snapshots().borrow().listener_addr.unwrap();
        let offer = handle.issue_pairing_code().await.unwrap();
        let mut connection = socket(address).await;
        let (mut protocol, peer, _secret) = pair(&mut connection, offer.code).await;
        state(&mut connection, &mut protocol).await;
        idle_ui_snapshot(&mut connection, &mut protocol, 1, true).await;

        application(
            &mut connection,
            &mut protocol,
            serde_json::json!({"type":"client_info","version":1,"extension_version":"0.0.9"}),
        )
        .await;
        let mut snapshots = handle.snapshots();
        tokio::time::timeout(
            Duration::from_secs(5),
            snapshots.wait_for(|snapshot| {
                snapshot.peers.first().is_some_and(|reported| {
                    reported.id == peer && reported.extension_version.as_deref() == Some("0.0.9")
                })
            }),
        )
        .await
        .unwrap()
        .unwrap();

        // An unusable report is dropped: it neither replaces the known version nor retires
        // the session, so the next command is still answered.
        application(
            &mut connection,
            &mut protocol,
            serde_json::json!({"type":"client_info","version":1,"extension_version":"abc"}),
        )
        .await;
        application(
            &mut connection,
            &mut protocol,
            serde_json::json!({"type":"heartbeat","version":1}),
        )
        .await;
        assert_eq!(
            state(&mut connection, &mut protocol).await["type"],
            "heartbeat"
        );
        assert_eq!(
            snapshots.borrow().peers[0].extension_version.as_deref(),
            Some("0.0.9")
        );

        handle.shutdown().await.unwrap();
        drop(connection);
        drop(handle);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn oversized_fragmented_upgrade_still_handshakes() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
        handle
            .configure(GatewayConfig {
                enabled: true,
                bind_address: Ipv4Addr::LOCALHOST.into(),
                port: available_port().await,
            })
            .await
            .unwrap();
        let address = handle.snapshots().borrow().listener_addr.unwrap();

        // A browser can carry kilobytes of cookies, arriving in pieces; the classifier has to wait
        // for the whole head and still hand a request this size to the handshake.
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Cookie: {}\r\n\r\n",
            "a".repeat(9 * 1024)
        );
        let request = request.as_bytes();
        let mut stream = TcpStream::connect(address).await.unwrap();
        let split = request.len() / 2;
        stream.write_all(&request[..split]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        stream.write_all(&request[split..]).await.unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut byte = [0; 1];
            while !response.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).await.unwrap() == 0 {
                    break;
                }
                response.extend_from_slice(&byte);
            }
        })
        .await
        .expect("the upgrade response must arrive");
        let head = String::from_utf8(response).unwrap();
        assert_eq!(
            head.lines().next(),
            Some("HTTP/1.1 101 Switching Protocols"),
            "{head}"
        );

        drop(stream);
        handle.shutdown().await.unwrap();
        drop(handle);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn ipv6_listener_binds_and_persists_selected_address() {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let reserved = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
        let address = reserved.local_addr().unwrap();
        let config = GatewayConfig {
            enabled: true,
            bind_address: address.ip(),
            port: std::num::NonZeroU16::new(address.port()).unwrap(),
        };
        drop(reserved);
        let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
        handle.configure(config).await.unwrap();
        assert_eq!(handle.snapshots().borrow().listener_addr, Some(address));
        let mut connection = socket(address).await;
        connection.close(None).await.unwrap();
        assert_eq!(Registry::load(&db).unwrap().config, config);
        handle.shutdown().await.unwrap();
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn authenticated_peer_activity_is_batched_between_registry_writes() {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let mut registry = Registry {
            config: GatewayConfig {
                enabled: true,
                port: available_port().await,
                ..GatewayConfig::default()
            },
            ..Registry::default()
        };
        let initial_timestamp = 1;
        for id in [1, 2] {
            registry.peers.push(Peer {
                id: [id; 16],
                secret: StoredSecret([id; 32]),
                label: None,
                allow_unlock: false,
                paired_at: initial_timestamp,
                last_active_at: initial_timestamp,
            });
        }
        registry.save(&db).unwrap();
        let started = Instant::now();
        let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
        let mut snapshots = handle.snapshots();
        snapshots
            .wait_for(|snapshot| snapshot.listener_addr.is_some())
            .await
            .unwrap();
        let address = snapshots.borrow().listener_addr.unwrap();
        let mut connections = Vec::new();
        for peer in &registry.peers {
            let mut connection = socket(address).await;
            let (mut protocol, hello) = Connection::client_reconnect(
                PeerId::from_bytes(peer.id),
                SessionSecret::from_storage(peer.secret.0),
            );
            connection
                .send(Message::Binary(hello.into()))
                .await
                .unwrap();
            loop {
                let step = protocol
                    .receive_handshake(&read(&mut connection).await)
                    .unwrap();
                if let Some(output) = step.outbound {
                    connection
                        .send(Message::Binary(output.into()))
                        .await
                        .unwrap();
                }
                if step.event == Some(HandshakeEvent::Authenticated) {
                    break;
                }
            }
            state(&mut connection, &mut protocol).await;
            idle_ui_snapshot(&mut connection, &mut protocol, 1, true).await;
            application(
                &mut connection,
                &mut protocol,
                serde_json::json!({"type":"heartbeat","version":1}),
            )
            .await;
            assert_eq!(
                state(&mut connection, &mut protocol).await["type"],
                "heartbeat"
            );
            connections.push(connection);
        }
        snapshots
            .wait_for(|snapshot| {
                snapshot
                    .peers
                    .iter()
                    .all(|peer| peer.last_active_at > initial_timestamp)
            })
            .await
            .unwrap();
        let active: Vec<_> = snapshots
            .borrow()
            .peers
            .iter()
            .map(|peer| peer.last_active_at)
            .collect();
        assert!(
            Registry::load(&db)
                .unwrap()
                .peers
                .iter()
                .all(|peer| peer.last_active_at == initial_timestamp)
        );
        // Complete real socket I/O before pausing; bound the deadline on both sides of setup.
        let authenticated = Instant::now();
        tokio::time::pause();
        let before_flush = started + Duration::from_mins(4);
        tokio::time::advance(before_flush.saturating_duration_since(Instant::now())).await;
        assert!(
            Registry::load(&db)
                .unwrap()
                .peers
                .iter()
                .all(|peer| peer.last_active_at == initial_timestamp)
        );
        let after_flush = authenticated + ACTIVITY_FLUSH_INTERVAL;
        tokio::time::advance(after_flush.saturating_duration_since(Instant::now())).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let persisted = Registry::load(&db).unwrap();
        assert_eq!(
            persisted
                .peers
                .iter()
                .map(|peer| peer.last_active_at)
                .collect::<Vec<_>>(),
            active
        );
        assert!(
            persisted
                .peers
                .iter()
                .all(|peer| peer.paired_at == initial_timestamp)
        );
        handle.shutdown().await.unwrap();
        drop(connections);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn pairing_lock_revocation_and_registry_restart() {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("gateway-test-{}", u128::from_le_bytes(random)));
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();
        let handle = GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
        let conflicting = GatewayConfig {
            enabled: true,
            bind_address: Ipv4Addr::LOCALHOST.into(),
            port: std::num::NonZeroU16::new(occupied_port).unwrap(),
        };
        assert_eq!(
            handle.configure(conflicting).await,
            Err(GatewayError::PortInUse(occupied_port))
        );
        assert!(handle.snapshots().borrow().listener_addr.is_none());
        assert_eq!(
            handle.snapshots().borrow().error,
            Some(GatewayError::PortInUse(occupied_port))
        );
        assert_eq!(Registry::load(&db).unwrap().config, conflicting);
        let config = GatewayConfig {
            port: available_port().await,
            ..conflicting
        };
        handle.configure(config).await.unwrap();
        let mut snapshots = handle.snapshots();
        snapshots
            .wait_for(|snapshot| snapshot.listener_addr.is_some())
            .await
            .unwrap();
        let address = snapshots.borrow().listener_addr.unwrap();
        assert_eq!(
            address,
            SocketAddr::new(config.bind_address, config.port.get())
        );
        assert!(snapshots.borrow().error.is_none());
        let mut unauthenticated = socket(address).await;
        unauthenticated
            .send(Message::Binary(
                br#"{"type":"get_state","version":1}"#.to_vec().into(),
            ))
            .await
            .unwrap();
        let rejected = tokio::time::timeout(Duration::from_secs(5), unauthenticated.next())
            .await
            .unwrap();
        assert!(!matches!(rejected, Some(Ok(Message::Binary(_)))));
        let offer = handle.issue_pairing_code().await.unwrap();
        let mut connection = socket(address).await;
        let pairing_started = epoch_secs();
        let (mut protocol, peer, secret) = pair(&mut connection, offer.code).await;
        let persisted = Registry::load(&db).unwrap();
        let paired_at = persisted.peers[0].paired_at;
        assert!((pairing_started..=epoch_secs()).contains(&paired_at));
        assert_eq!(persisted.peers[0].last_active_at, paired_at);
        let first = state(&mut connection, &mut protocol).await;
        assert_eq!(first["locked"], true);
        assert_eq!(first["generation"], 1);
        idle_ui_snapshot(&mut connection, &mut protocol, 1, true).await;
        // Successful pairing consumes the code even with reservations remaining.
        pairing_attempt(address, false).await;
        let store = crate::vault::DesktopVaultStore::from_db(Arc::clone(&db));
        let view = super::super::provider::tests::initialize(&store);
        handle
            .update_wallet_state(super::super::provider::tests::state(&view), 2)
            .await
            .unwrap();
        assert_eq!(state(&mut connection, &mut protocol).await["locked"], false);
        idle_ui_snapshot(&mut connection, &mut protocol, 2, false).await;
        handle.update_desktop_state(true, 3).await.unwrap();
        let locked = state(&mut connection, &mut protocol).await;
        assert_eq!(locked["locked"], true);
        assert_eq!(locked["generation"], 3);
        idle_ui_snapshot(&mut connection, &mut protocol, 3, true).await;
        // The authenticated transport owns the peer identity and document registration.
        handle
            .update_wallet_state(super::super::provider::tests::state(&view), 4)
            .await
            .unwrap();
        assert_eq!(state(&mut connection, &mut protocol).await["generation"], 4);
        idle_ui_snapshot(&mut connection, &mut protocol, 4, false).await;
        application(&mut connection, &mut protocol, serde_json::json!({"type":"register_document","version":1,"document":"invalid","url":"not a URL"})).await;
        let invalid = state(&mut connection, &mut protocol).await;
        assert_eq!(invalid["error"]["code"], -32602);
        assert!(!invalid.to_string().contains("not a URL"));
        application(&mut connection, &mut protocol, serde_json::json!({"type":"register_document","version":1,"document":"page","url":"https://example.invalid/path?q=one#fragment"})).await;
        assert_eq!(
            state(&mut connection, &mut protocol).await["accounts"],
            serde_json::json!([])
        );
        application(&mut connection, &mut protocol, serde_json::json!({"type":"provider_request","version":1,"document":"page","request_id":"connect","method":"eth_requestAccounts","params":[]})).await;
        let prompt = state(&mut connection, &mut protocol).await;
        let pending = &prompt["pending_connects"][0];
        assert_eq!(pending["url"], "https://example.invalid/");
        assert_eq!(
            pending["paired_peer_id"],
            alloy::hex::encode(peer.to_bytes())
        );
        let address = pending["accounts"][0]["address"].clone();
        application(&mut connection, &mut protocol, serde_json::json!({"type":"resolve_connect","version":1,"request_id":pending["request_id"],"public_account_uuid":pending["accounts"][0]["uuid"],"chain_id":1})).await;
        let granted_state = state(&mut connection, &mut protocol).await;
        assert_eq!(granted_state["type"], "provider_state");
        assert_eq!(granted_state["accounts"], serde_json::json!([address]));
        assert_eq!(
            state(&mut connection, &mut protocol).await["pending_connects"],
            serde_json::json!([])
        );
        let connected = state(&mut connection, &mut protocol).await;
        assert_eq!(connected["result"], serde_json::json!([address]));
        assert_eq!(
            connected["document_generation"],
            granted_state["document_generation"]
        );
        let permission = store.list_gateway_permissions(&view).unwrap().remove(0);
        handle
            .revoke_permission(permission.permission_id)
            .await
            .unwrap();
        let revoked = state(&mut connection, &mut protocol).await;
        assert_eq!(revoked["accounts"], serde_json::json!([]));
        assert_eq!(revoked["invalidation_code"], 4100);
        assert!(
            revoked["document_generation"].as_u64().unwrap()
                > granted_state["document_generation"].as_u64().unwrap()
        );
        let revoked_ui = state(&mut connection, &mut protocol).await;
        assert_eq!(revoked_ui["type"], "ui_snapshot");
        assert_eq!(revoked_ui["permissions"], serde_json::json!([]));
        application(&mut connection, &mut protocol, serde_json::json!({"type":"register_document","version":1,"document":"page","url":"https://other.invalid/"})).await;
        let retired = tokio::time::timeout(Duration::from_secs(5), connection.next())
            .await
            .unwrap();
        assert!(!matches!(retired, Some(Ok(Message::Binary(_)))));
        drop(view);
        drop(store);
        handle.shutdown().await.unwrap();
        drop(db);
        let db = Arc::new(
            DbStore::open(DbConfig {
                root_dir: root.clone(),
            })
            .unwrap(),
        );
        let restored = GatewayHandle::start(db.clone(), true, 4, GatewayInstallBundle::default());
        let mut snapshots = restored.snapshots();
        snapshots
            .wait_for(|snapshot| snapshot.listener_addr.is_some())
            .await
            .unwrap();
        assert_eq!(snapshots.borrow().peers[0].id, peer);
        assert_eq!(snapshots.borrow().config, config);
        let address = snapshots.borrow().listener_addr.unwrap();
        assert_eq!(
            address,
            SocketAddr::new(config.bind_address, config.port.get())
        );
        let mut connection = socket(address).await;
        let (mut protocol, hello) = Connection::client_reconnect(peer, secret);
        connection
            .send(Message::Binary(hello.into()))
            .await
            .unwrap();
        loop {
            let step = protocol
                .receive_handshake(&read(&mut connection).await)
                .unwrap();
            if let Some(output) = step.outbound {
                connection
                    .send(Message::Binary(output.into()))
                    .await
                    .unwrap();
            }
            if step.event == Some(HandshakeEvent::Authenticated) {
                break;
            }
        }
        let restored_state = state(&mut connection, &mut protocol).await;
        assert_eq!(restored_state["generation"], 4);
        assert_eq!(restored_state["locked"], true);
        idle_ui_snapshot(&mut connection, &mut protocol, 4, true).await;
        restored.revoke_peer(peer).await.unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(5), connection.next())
            .await
            .unwrap();
        assert!(!matches!(closed, Some(Ok(Message::Binary(_)))));
        assert!(Registry::load(&db).unwrap().peers.is_empty());
        let mut revoked = socket(address).await;
        revoked
            .send(Message::Binary(
                ClientHello::reconnect(peer).encode().into(),
            ))
            .await
            .unwrap();
        let rejected = tokio::time::timeout(Duration::from_secs(5), revoked.next())
            .await
            .unwrap();
        assert!(!matches!(rejected, Some(Ok(Message::Binary(_)))));
        restored
            .configure(GatewayConfig {
                bind_address: Ipv4Addr::UNSPECIFIED.into(),
                ..config
            })
            .await
            .unwrap();
        snapshots
            .wait_for(|snapshot| {
                snapshot
                    .listener_addr
                    .is_some_and(|address| address.ip().is_unspecified())
            })
            .await
            .unwrap();
        restored.shutdown().await.unwrap();
        // Unknown future storage must not be overwritten by an enable operation.
        let future = br#"{"version":3,"config":{"enabled":true,"bind_address":"127.0.0.1","port":43110},"peers":[]}"#;
        db.put_app_settings_record("gateway-state", future).unwrap();
        let blocked = GatewayHandle::start(db.clone(), true, 5, GatewayInstallBundle::default());
        assert_eq!(
            blocked.configure(config).await,
            Err(GatewayError::InvalidStorage)
        );
        assert_eq!(
            db.get_app_settings_record("gateway-state")
                .unwrap()
                .unwrap(),
            future
        );
        assert!(blocked.snapshots().borrow().listener_addr.is_none());
        blocked.shutdown().await.unwrap();
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn encrypted_reads_suppress_retired_payloads_and_shutdown_waits_for_broker_drain() {
        use super::super::provider::tests::{held_rpc, initialize, state as wallet_state};
        use crate::{HttpContext, RpcChainRoute, RpcOrigin};
        use serde_json::json;
        for remote_error in [false, true] {
            for invalidation in ["lock", "revoke", "restart"] {
                let mut random = [0; 16];
                getrandom::fill(&mut random).unwrap();
                let root = std::env::temp_dir()
                    .join(format!("gateway-read-ws-{}", alloy::hex::encode(random)));
                let db = Arc::new(
                    DbStore::open(DbConfig {
                        root_dir: root.clone(),
                    })
                    .unwrap(),
                );
                let store = crate::vault::DesktopVaultStore::from_db(db.clone());
                let view = initialize(&store);
                let (endpoint, started, release, server) = held_rpc(remote_error).await;
                let handle =
                    GatewayHandle::start(db.clone(), true, 1, GatewayInstallBundle::default());
                let config = GatewayConfig {
                    enabled: true,
                    bind_address: Ipv4Addr::LOCALHOST.into(),
                    port: available_port().await,
                };
                handle.configure(config).await.unwrap();
                let address = handle.snapshots().borrow().listener_addr.unwrap();
                let offer = handle.issue_pairing_code().await.unwrap();
                let mut connection = socket(address).await;
                let (mut protocol, peer, secret) = pair(&mut connection, offer.code).await;
                state(&mut connection, &mut protocol).await;
                idle_ui_snapshot(&mut connection, &mut protocol, 1, true).await;
                let origin = RpcOrigin::dapp(
                    alloy::hex::encode(peer.to_bytes()),
                    "https://wire.invalid/path?q=1#doc",
                )
                .unwrap();
                let account = store
                    .list_active_public_accounts_for_session(&view)
                    .unwrap()
                    .remove(0);
                store
                    .grant_gateway_permission(&view, &origin, &account.public_account_uuid, 1)
                    .unwrap();
                let permission = store.list_gateway_permissions(&view).unwrap().remove(0);
                let mut wallet = wallet_state(&view);
                wallet.http = Some(HttpContext::direct_for_tests());
                wallet
                    .routes
                    .insert(1, RpcChainRoute::new(1, vec![endpoint]));
                handle.update_wallet_state(wallet, 2).await.unwrap();
                state(&mut connection, &mut protocol).await;
                idle_ui_snapshot(&mut connection, &mut protocol, 2, false).await;
                application(&mut connection, &mut protocol, json!({"type":"register_document","version":1,"document":"doc","url":origin.web_origin().unwrap().as_str()})).await;
                assert_eq!(
                    state(&mut connection, &mut protocol).await["accounts"],
                    json!([account.address.to_string()])
                );
                application(&mut connection, &mut protocol, json!({"type":"provider_request","version":1,"document":"doc","request_id":"held","method":"eth_blockNumber","params":[]})).await;
                tokio::time::timeout(Duration::from_secs(5), started.notified())
                    .await
                    .unwrap();
                match invalidation {
                    "lock" => handle
                        .update_wallet_state(super::super::GatewayWalletState::default(), 3)
                        .await
                        .unwrap(),
                    "revoke" => handle
                        .revoke_permission(permission.permission_id)
                        .await
                        .unwrap(),
                    _ => handle.configure(config).await.unwrap(),
                }
                if invalidation == "restart" {
                    drop(connection);
                    let address = handle.snapshots().borrow().listener_addr.unwrap();
                    connection = socket(address).await;
                    let (mut fresh, hello) = Connection::client_reconnect(peer, secret);
                    connection
                        .send(Message::Binary(hello.into()))
                        .await
                        .unwrap();
                    loop {
                        let step = fresh
                            .receive_handshake(&read(&mut connection).await)
                            .unwrap();
                        if let Some(output) = step.outbound {
                            connection
                                .send(Message::Binary(output.into()))
                                .await
                                .unwrap();
                        }
                        if step.event == Some(HandshakeEvent::Authenticated) {
                            break;
                        }
                    }
                    protocol = fresh;
                    state(&mut connection, &mut protocol).await;
                    idle_ui_snapshot(&mut connection, &mut protocol, 2, false).await;
                    application(&mut connection, &mut protocol, json!({"type":"register_document","version":1,"document":"doc","url":origin.web_origin().unwrap().as_str()})).await;
                    state(&mut connection, &mut protocol).await;
                    application(&mut connection, &mut protocol, json!({"type":"provider_request","version":1,"document":"doc","request_id":"fresh","method":"eth_accounts","params":[]})).await;
                    let response = state(&mut connection, &mut protocol).await;
                    assert_eq!(response["request_id"], "fresh");
                    assert_eq!(response["result"], json!([account.address.to_string()]));
                } else {
                    loop {
                        let message = state(&mut connection, &mut protocol).await;
                        assert!(!message.to_string().contains("synthetic-owner-payload"));
                        assert_ne!(message["result"], "0xfeed");
                        if message["request_id"] == "held" {
                            assert_eq!(message["error"]["code"], 4100);
                            break;
                        }
                    }
                }
                // Session retirement cannot release the accepted broker future. Destructive
                // shutdown acknowledgement remains pending until the held HTTP response settles.
                let mut shutdown = tokio::spawn({
                    let handle = handle.clone();
                    async move { handle.shutdown().await }
                });
                assert!(
                    tokio::time::timeout(Duration::from_millis(30), &mut shutdown)
                        .await
                        .is_err()
                );
                release.notify_one();
                shutdown.await.unwrap().unwrap();
                server.await.unwrap();
                drop(connection);
                drop(handle);
                drop(store);
                drop(view);
                drop(db);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod outbound_tests;
