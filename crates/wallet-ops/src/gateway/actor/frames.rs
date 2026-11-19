#[allow(clippy::wildcard_imports)]
use super::*;

impl Actor {
    pub(super) fn accept(&mut self, stream: TcpStream) {
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
                if !head
                    .as_deref()
                    .is_some_and(install::is_allowed_upgrade_origin)
                {
                    tracing::debug!(target: LOG_TARGET, "gateway refused a browser-page websocket upgrade");
                    return None;
                }
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
                tracing::debug!(target: LOG_TARGET, "gateway install request without local address");
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
                tracing::debug!(target: LOG_TARGET, "gateway install response not delivered");
            }
            None
        });
    }

    pub(super) const fn state(&self) -> GatewayServerMessage {
        GatewayServerMessage::State {
            version: 1,
            locked: self.locked,
            generation: self.generation,
            wallet_transition: self.provider.wallet_transition(),
            client_info_supported: true,
            unlock_supported: true,
        }
    }

    pub(super) fn frame(
        &mut self,
        id: u64,
        session: &mut Session,
        bytes: &[u8],
    ) -> Result<(), GatewayError> {
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
                    tracing::debug!(target: LOG_TARGET,
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
                                crate::gateway::GatewayUiEventKind::Unlock { request },
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

                    Ok(GatewayClientMessage::ChainEditor {
                        version: 1,
                        generation,
                        command,
                    }) => {
                        if let Some(request) = self.provider.chain_editor_command(
                            id,
                            session.peer().ok_or(GatewayError::Unavailable)?,
                            generation,
                            command,
                        ) {
                            self.emit_ui_event(
                                id,
                                generation,
                                crate::gateway::GatewayUiEventKind::ChainEditor { request },
                            );
                        }
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
                                crate::gateway::GatewayUiEventKind::Network { request },
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
                                crate::gateway::GatewayUiEventKind::PrivateView { command },
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
                                crate::gateway::GatewayUiEventKind::PublicView { peer_id, command },
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
                            crate::gateway::GatewayUiEventKind::SummonDesktop,
                        );
                    }
                    Ok(GatewayClientMessage::UserActivity {
                        version: 1,
                        generation,
                    }) => {
                        self.emit_ui_event(
                            id,
                            generation,
                            crate::gateway::GatewayUiEventKind::UserActivity,
                        );
                    }
                    Ok(GatewayClientMessage::ResolveConnect {
                        version: 1,
                        request_id,
                        public_account_uuid,
                        chain_id,
                    }) => {
                        tracing::debug!(target: LOG_TARGET,
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
            tracing::debug!(target: LOG_TARGET, session_id = id, "gateway session authenticated");
            session.application(self.state())?;
            self.provider
                .attach_ui_peer(id, session.peer().ok_or(GatewayError::Unavailable)?);
            session.heartbeat = Instant::now() + HEARTBEAT;
        }
        Ok(())
    }

    fn trace_extension_command(session_id: u64, message: Option<&GatewayClientMessage>) {
        let Some(message) = message else {
            tracing::debug!(target: LOG_TARGET,
                session_id,
                command = "invalid",
                "gateway extension command received"
            );
            return;
        };
        let (command, version) = match message {
            GatewayClientMessage::GetUnlockState { version } => ("get_unlock_state", version),
            GatewayClientMessage::Unlock { version, .. } => ("unlock", version),
            GatewayClientMessage::ChainEditor { version, .. } => ("chain_editor", version),
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
                tracing::debug!(target: LOG_TARGET,
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
        tracing::debug!(target: LOG_TARGET,
            session_id,
            command,
            version = *version,
            "gateway extension command received"
        );
    }

    fn emit_ui_event(
        &mut self,
        id: u64,
        generation: u64,
        kind: crate::gateway::GatewayUiEventKind,
    ) {
        let live = self
            .ui_sessions
            .entry(id)
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicBool::new(true)));
        if let Some(event) = self.provider.ui_event(generation, kind, live) {
            let _ = self.ui_events.send(event);
        }
    }
}
