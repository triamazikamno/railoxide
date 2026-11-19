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

mod commands;
mod frames;
mod upkeep;

/// Keeps the tracing target of the moved methods at this module's path.
const LOG_TARGET: &str = module_path!();

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
// One state per document of a full session (provider MAX_SESSION_DOCUMENTS) plus control traffic.
const MAX_OUTBOUND_MESSAGES: usize = 320;
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
            Some(GatewayServerMessage::ChainEditor { .. }) => "ChainEditor",
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
mod integration_tests;

#[cfg(test)]
mod outbound_tests;
