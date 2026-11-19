#[allow(clippy::wildcard_imports)]
use super::*;

impl Actor {
    pub(super) fn retire_provider_sessions(&mut self) {
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

    pub(super) fn broadcast_state(&mut self) {
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

    pub(super) fn flush_provider(&mut self) {
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
                    tracing::debug!(target: LOG_TARGET,
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

    pub(super) fn record_peer_activity(&mut self, id: PeerId) {
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
            tracing::warn!(target: LOG_TARGET, "gateway peer activity could not be saved");
        }
    }

    pub(super) fn tick(&mut self) {
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
                        tracing::debug!(target: LOG_TARGET,
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
                    tracing::debug!(target: LOG_TARGET,
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
                        tracing::debug!(target: LOG_TARGET,
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
