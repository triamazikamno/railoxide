#[allow(clippy::wildcard_imports)]
use super::*;

impl Actor {
    pub(super) fn publish(&self) {
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

    pub(super) async fn shutdown(&mut self) {
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
        tracing::debug!(target: LOG_TARGET,
            reason = "gateway_retired",
            sessions = self.sessions.len(),
            "gateway sessions retired"
        );
        self.sessions.clear();
        self.retire_provider_sessions();
        self.upgrades.abort_all();
        while self.upgrades.join_next().await.is_some() {}
    }

    pub(super) async fn bind(&mut self) {
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

    pub(super) async fn command(&mut self, command: Command) {
        match command {
            Command::ChainEditorResult(request, outcome, done) => {
                self.provider
                    .complete_chain_editor_request(request, outcome);
                self.flush_provider();
                let _ = done.send(());
            }
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
                        .update_wallet(crate::gateway::GatewayWalletState::default(), generation);
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
                // Multiple publishers may queue the same epoch around a hot chain edit.
                // A delayed snapshot cannot replace the synchronously published authority.
                let result = if !self.provider.is_current_wallet_state(&wallet) {
                    Err(GatewayError::Unavailable)
                } else if generation > self.generation
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
            Command::ApprovalRead(id, endpoint, rpc, entered, client, reply) => {
                self.provider
                    .approval_read(&id, endpoint, rpc, entered, client, reply);
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

    pub(super) fn unlock_allowed(&self, peer: PeerId) -> bool {
        self.registry
            .peers
            .iter()
            .any(|entry| entry.id == peer.to_bytes() && entry.allow_unlock)
    }

    pub(super) fn publish_unlocks(&mut self) {
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
}
