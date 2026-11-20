#[allow(clippy::wildcard_imports)]
use super::*;

impl TorBridgeSession {
    fn isolate(&mut self, generation: u64) {
        *self = Self {
            client: self.client.isolated_client(),
            generation,
            activity: TorBridgeActivity::new(generation),
        };
    }
}

impl ArtiSocksBridge {
    pub(super) async fn start(arti_client: WalletTorClient, generation: u64) -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .wrap_err("bind internal Arti SOCKS bridge")?;
        let local_addr = listener
            .local_addr()
            .wrap_err("read internal Arti SOCKS bridge address")?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let session = Arc::new(RwLock::new(TorBridgeSession {
            client: arti_client,
            generation,
            activity: TorBridgeActivity::new(generation),
        }));
        let task = tokio::spawn(run_arti_socks_bridge(
            listener,
            Arc::clone(&session),
            shutdown_rx,
        ));
        Ok(Self {
            local_addr,
            session,
            shutdown_tx,
            task,
        })
    }

    pub(super) fn capture_session(
        session: &Arc<RwLock<TorBridgeSession>>,
    ) -> Result<CapturedTorBridgeSession> {
        session
            .read()
            .map(|session| CapturedTorBridgeSession {
                client: session.client.clone(),
                generation: session.generation,
                activity: Arc::clone(&session.activity),
            })
            .map_err(|_| eyre!("active Tor session client lock is poisoned"))
    }

    /// Yields the session's current client, so a new Tor session re-isolates later dials.
    pub(super) fn session_client_provider(
        session: &Arc<RwLock<TorBridgeSession>>,
    ) -> WalletTorClientProvider {
        let session: Weak<RwLock<TorBridgeSession>> = Arc::downgrade(session);
        Arc::new(move || {
            session.upgrade().and_then(|session| {
                Self::capture_session(&session)
                    .ok()
                    .map(|captured| captured.client)
            })
        })
    }

    pub(super) const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub(super) fn active_client(&self) -> Result<WalletTorClient> {
        Self::capture_session(&self.session).map(|captured| captured.client)
    }

    pub(super) fn new_isolated_session(&self) -> Result<u64> {
        let mut session = self.write_session()?;
        let generation = session.generation.saturating_add(1);
        session.isolate(generation);
        Ok(generation)
    }

    /// Moves this bridge onto a fresh isolated client numbered `generation`.
    pub(super) fn join_session(&self, generation: u64) -> Result<()> {
        self.write_session()?.isolate(generation);
        Ok(())
    }

    /// Moves this bridge onto a fresh isolated client in its current session.
    pub(super) fn reisolate(&self) -> Result<()> {
        let mut session = self.write_session()?;
        let generation = session.generation;
        session.isolate(generation);
        Ok(())
    }

    fn write_session(&self) -> Result<std::sync::RwLockWriteGuard<'_, TorBridgeSession>> {
        self.session
            .write()
            .map_err(|_| eyre!("active Tor session client lock is poisoned"))
    }

    pub(super) fn session_generation(&self) -> u64 {
        self.session.read().map_or(0, |session| session.generation)
    }

    pub(super) fn activity_snapshot(&self) -> Option<TorBridgeActivitySnapshot> {
        self.session
            .read()
            .ok()
            .map(|session| session.activity.snapshot())
    }

    pub(super) fn runtime_health_report(&self) -> TorRuntimeHealthReport {
        let default_report = TorRuntimeHealthReport {
            health: TorRuntimeHealth::Ready,
            recent_attempt_count: 0,
            recent_failed_attempt_count: 0,
            recent_successful_sample_count: 0,
            recent_successful_setup_p75: None,
        };
        self.session.read().map_or(default_report, |session| {
            session.activity.runtime_health_report()
        })
    }
}

impl Drop for ArtiSocksBridge {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.task.abort();
    }
}

async fn run_arti_socks_bridge(
    listener: TcpListener,
    session: Arc<RwLock<TorBridgeSession>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let captured = match ArtiSocksBridge::capture_session(&session) {
                            Ok(captured) => captured,
                            Err(error) => {
                                tracing::warn!(target: LOG_TARGET,%error, "active Tor session client lock is poisoned");
                                continue;
                            }
                        };
                        tokio::spawn(async move {
                            if let Err(error) = handle_socks_connection(stream, captured).await {
                                tracing::debug!(target: LOG_TARGET,?error, "internal Arti SOCKS connection failed");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(target: LOG_TARGET,%error, "internal Arti SOCKS accept failed; retrying");
                        if !wait_after_socks_accept_error(&mut shutdown_rx).await {
                            break;
                        }
                    }
                }
            }
        }
    }
}

pub(super) async fn wait_after_socks_accept_error(shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        changed = shutdown_rx.changed() => changed.is_ok() && !*shutdown_rx.borrow(),
        () = tokio::time::sleep(SOCKS_ACCEPT_ERROR_BACKOFF) => true,
    }
}

async fn handle_socks_connection(
    mut inbound: TcpStream,
    captured: CapturedTorBridgeSession,
) -> Result<()> {
    let CapturedTorBridgeSession {
        client: arti_client,
        generation,
        activity,
    } = captured;
    debug_assert_eq!(generation, activity.generation);
    negotiate_socks_no_auth(&mut inbound).await?;
    let target = read_socks_connect_target(&mut inbound).await?;
    let connecting = activity.begin_connection();
    let outbound = match arti_client
        .connect((target.host.as_str(), target.port))
        .await
    {
        Ok(outbound) => outbound,
        Err(_error) => {
            connecting.finish_failure();
            send_socks_reply(&mut inbound, SOCKS_REPLY_GENERAL_FAILURE).await?;
            return Err(eyre!("Tor connection failed"));
        }
    };
    let active_stream = connecting.finish_success();
    send_socks_reply(&mut inbound, SOCKS_REPLY_SUCCEEDED).await?;
    relay_socks_stream(inbound, outbound, active_stream).await
}

pub(super) async fn relay_socks_stream<Inbound, Outbound>(
    mut inbound: Inbound,
    outbound: Outbound,
    active_stream: TorActiveStreamGuard,
) -> Result<()>
where
    Inbound: AsyncRead + AsyncWrite + Unpin,
    Outbound: AsyncRead + AsyncWrite + Unpin,
{
    let mut outbound = DownloadCounter::new(outbound, Arc::clone(&active_stream.activity));
    match tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await {
        Ok(_) => Ok(()),
        Err(error) if is_expected_socks_stream_close(&error) => {
            tracing::trace!(target: LOG_TARGET,%error, "internal Arti SOCKS stream closed");
            Ok(())
        }
        Err(error) => Err(error).wrap_err("relay SOCKS stream through Arti"),
    }
}

impl TorConnectingGuard {
    pub(super) fn finish_success(self) -> TorActiveStreamGuard {
        let activity = Arc::clone(&self.activity);
        activity.record_connection(true, self.started_at.elapsed());
        TorActiveStreamGuard { activity }
    }

    pub(super) fn finish_failure(self) {
        self.activity
            .record_connection(false, self.started_at.elapsed());
    }
}

impl Drop for TorConnectingGuard {
    fn drop(&mut self) {
        let _ = self.activity.connecting_streams.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |count| count.checked_sub(1),
        );
        self.activity.touch();
    }
}

impl Drop for TorActiveStreamGuard {
    fn drop(&mut self) {
        self.activity.finish_active_stream();
    }
}

impl<S> DownloadCounter<S> {
    const fn new(stream: S, activity: Arc<TorBridgeActivity>) -> Self {
        Self { stream, activity }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for DownloadCounter<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        if matches!(&result, Poll::Ready(Ok(()))) {
            self.activity
                .add_downloaded_bytes(buf.filled().len().saturating_sub(before));
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for DownloadCounter<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

fn is_expected_socks_stream_close(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
    )
}

async fn negotiate_socks_no_auth(inbound: &mut TcpStream) -> Result<()> {
    let mut header = [0_u8; 2];
    inbound
        .read_exact(&mut header)
        .await
        .wrap_err("read SOCKS greeting")?;
    if header[0] != SOCKS_VERSION {
        return Err(eyre!("unsupported SOCKS version {}", header[0]));
    }
    let mut methods = vec![0_u8; usize::from(header[1])];
    inbound
        .read_exact(&mut methods)
        .await
        .wrap_err("read SOCKS auth methods")?;
    if !methods.contains(&SOCKS_NO_AUTH) {
        inbound
            .write_all(&[SOCKS_VERSION, SOCKS_NO_ACCEPTABLE_METHODS])
            .await
            .wrap_err("write SOCKS auth rejection")?;
        return Err(eyre!("SOCKS client did not offer no-auth mode"));
    }
    inbound
        .write_all(&[SOCKS_VERSION, SOCKS_NO_AUTH])
        .await
        .wrap_err("write SOCKS auth selection")?;
    Ok(())
}

async fn read_socks_connect_target(inbound: &mut TcpStream) -> Result<SocksTarget> {
    let mut header = [0_u8; 4];
    inbound
        .read_exact(&mut header)
        .await
        .wrap_err("read SOCKS connect header")?;
    if header[0] != SOCKS_VERSION {
        send_socks_reply(inbound, SOCKS_REPLY_GENERAL_FAILURE).await?;
        return Err(eyre!("unsupported SOCKS request version {}", header[0]));
    }
    if header[1] != SOCKS_CMD_CONNECT {
        send_socks_reply(inbound, SOCKS_REPLY_COMMAND_NOT_SUPPORTED).await?;
        return Err(eyre!("unsupported SOCKS command {}", header[1]));
    }
    let host = match header[3] {
        SOCKS_ADDR_IPV4 => {
            let mut addr = [0_u8; 4];
            inbound
                .read_exact(&mut addr)
                .await
                .wrap_err("read SOCKS IPv4 address")?;
            Ipv4Addr::from(addr).to_string()
        }
        SOCKS_ADDR_DOMAIN => {
            let len = inbound
                .read_u8()
                .await
                .wrap_err("read SOCKS domain length")?;
            let mut domain = vec![0_u8; usize::from(len)];
            inbound
                .read_exact(&mut domain)
                .await
                .wrap_err("read SOCKS domain")?;
            String::from_utf8(domain).wrap_err("SOCKS domain is not UTF-8")?
        }
        SOCKS_ADDR_IPV6 => {
            let mut addr = [0_u8; 16];
            inbound
                .read_exact(&mut addr)
                .await
                .wrap_err("read SOCKS IPv6 address")?;
            Ipv6Addr::from(addr).to_string()
        }
        other => {
            send_socks_reply(inbound, SOCKS_REPLY_ADDR_NOT_SUPPORTED).await?;
            return Err(eyre!("unsupported SOCKS address type {other}"));
        }
    };
    let port = inbound
        .read_u16()
        .await
        .wrap_err("read SOCKS target port")?;
    Ok(SocksTarget { host, port })
}

async fn send_socks_reply(inbound: &mut TcpStream, reply: u8) -> Result<()> {
    inbound
        .write_all(&[
            SOCKS_VERSION,
            reply,
            0x00,
            SOCKS_ADDR_IPV4,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ])
        .await
        .wrap_err("write SOCKS reply")
}
