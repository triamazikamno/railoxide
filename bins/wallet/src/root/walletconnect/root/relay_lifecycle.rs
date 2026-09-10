use super::*;

impl WalletRoot {
    pub(in crate::root::walletconnect) fn walletconnect_client_context(
        &mut self,
        cx: &Context<'_, Self>,
    ) -> Result<WalletConnectClientContext, Arc<str>> {
        let Some(store) = self.vault_store.as_ref() else {
            return Err(Arc::from("Wallet vault storage is unavailable"));
        };
        let Some(view_session) = self.view_session.as_ref() else {
            return Err(Arc::from("Unlock a wallet before using WalletConnect"));
        };
        let identity = store
            .load_or_create_walletconnect_relay_identity(view_session.as_ref())
            .map_err(|error| {
                Arc::from(format!(
                    "Could not load WalletConnect relay identity: {error}"
                ))
            })?;
        let client = walletconnect_client_from_identity(
            self.walletconnect_effective_project_id(cx),
            &identity,
        );
        let http = self.http.clone();
        let worker = self.ensure_walletconnect_relay_worker(client, http, cx);
        Ok(WalletConnectClientContext { worker })
    }

    pub(in crate::root::walletconnect) fn walletconnect_client_context_for_session(
        &mut self,
        session: &WalletConnectSessionRecord,
        cx: &Context<'_, Self>,
    ) -> Result<WalletConnectClientContext, Arc<str>> {
        self.walletconnect_client_context_for_relay_client(&session.relay_client_id, cx)
    }

    fn walletconnect_client_context_for_relay_client(
        &mut self,
        relay_client_id: &str,
        cx: &Context<'_, Self>,
    ) -> Result<WalletConnectClientContext, Arc<str>> {
        let Some(store) = self.vault_store.as_ref() else {
            return Err(Arc::from("Wallet vault storage is unavailable"));
        };
        let Some(view_session) = self.view_session.as_ref() else {
            return Err(Arc::from("Unlock a wallet before using WalletConnect"));
        };
        let identity = store
            .load_walletconnect_relay_identity_for_client_id(view_session.as_ref(), relay_client_id)
            .map_err(|error| {
                Arc::from(format!(
                    "Could not load WalletConnect relay identity: {error}"
                ))
            })?
            .ok_or_else(|| {
                Arc::from("Could not find the relay identity for this WalletConnect session")
            })?;
        let client = walletconnect_client_from_identity(
            self.walletconnect_effective_project_id(cx),
            &identity,
        );
        let http = self.http.clone();
        let worker = self.ensure_walletconnect_relay_worker(client, http, cx);
        Ok(WalletConnectClientContext { worker })
    }

    pub(in crate::root::walletconnect) fn walletconnect_response_sender(
        &mut self,
        request: &WalletConnectRequestUi,
        cx: &Context<'_, Self>,
    ) -> Result<DappResponseSender, Arc<str>> {
        let route = self
            .walletconnect
            .request_routes
            .get(&request.key)
            .cloned()
            .ok_or_else(|| Arc::from("WalletConnect request response route is unavailable"))?;
        match &route {
            DappRequestRoute::Gateway {
                handle,
                approval_id,
            } => Ok(DappResponseSender::gateway(
                handle.clone(),
                approval_id.clone(),
            )),
            DappRequestRoute::WalletConnect {
                relay_client_id, ..
            } => {
                let context =
                    self.walletconnect_client_context_for_relay_client(relay_client_id, cx)?;
                Ok(route.response_sender(context.worker, request.item.id))
            }
        }
    }

    pub(in crate::root::walletconnect) fn walletconnect_effective_project_id(
        &self,
        cx: &Context<'_, Self>,
    ) -> String {
        self.settings_editor.as_ref().map_or_else(
            || WALLETCONNECT_DEFAULT_PROJECT_ID.to_owned(),
            |editor| {
                editor
                    .read(cx)
                    .saved
                    .walletconnect
                    .effective_project_id()
                    .to_owned()
            },
        )
    }

    pub(in crate::root::walletconnect) fn ensure_walletconnect_relay_worker(
        &mut self,
        client: WalletConnectRelayClient,
        http: HttpContext,
        cx: &Context<'_, Self>,
    ) -> WalletConnectRelayWorkerHandle {
        let client_id = client.auth().client_id.clone();
        let project_id = client.project_id().to_owned();
        if let Some(worker) = self.walletconnect.relay_workers.get(&client_id) {
            if worker.project_id == project_id {
                return worker.clone();
            }
            tracing::debug!(
                target: "wallet::root::walletconnect",
                relay_client_id = %walletconnect_request_key_log_label(&client_id),
                old_project_id = %walletconnect_request_key_log_label(&worker.project_id),
                new_project_id = %walletconnect_request_key_log_label(&project_id),
                "restarting walletconnect relay worker for project id change"
            );
            if let Some(worker) = self.walletconnect.relay_workers.remove(&client_id) {
                worker.stop();
            }
        }
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let worker = WalletConnectRelayWorkerHandle {
            worker_id: walletconnect_request_id_seed(),
            project_id,
            command_tx,
        };
        self.walletconnect
            .relay_workers
            .insert(client_id.clone(), worker.clone());
        tracing::debug!(
            target: "wallet::root::walletconnect",
            relay_client_id = %walletconnect_request_key_log_label(&client_id),
            worker_id = worker.worker_id,
            "starting walletconnect relay worker"
        );
        let runtime_join = self.runtime.spawn(walletconnect_relay_worker_loop(
            client, http, command_rx, event_tx,
        ));
        let event_client_id = client_id.clone();
        let event_worker_id = worker.worker_id;
        cx.spawn(async move |this, cx| {
            let mut event_rx = event_rx;
            while let Some(event) = event_rx.recv().await {
                match event {
                    WalletConnectRelayWorkerEvent::Output(output) => {
                        let plan = this
                            .update(cx, |root, _cx| {
                                root.walletconnect_relay_processing_plan(
                                    &event_client_id,
                                    event_worker_id,
                                )
                            })
                            .ok()
                            .flatten();
                        let Some(plan) = plan else {
                            continue;
                        };
                        let result = process_walletconnect_relay_output(
                            &plan.worker,
                            &plan.store,
                            &plan.view_session,
                            &plan.pairings,
                            &plan.sessions,
                            &plan.enabled_chain_ids,
                            output,
                            current_unix_seconds(),
                        )
                        .await;
                        let _ = this.update(cx, |root, cx| {
                            if !root.walletconnect_worker_matches(&event_client_id, event_worker_id)
                            {
                                return;
                            }
                            root.apply_walletconnect_relay_processing_result(Ok(result), cx);
                            cx.notify();
                        });
                    }
                    WalletConnectRelayWorkerEvent::Reconnecting(error) => {
                        let _ = this.update(cx, |root, cx| {
                            if !root.walletconnect_worker_matches(&event_client_id, event_worker_id)
                            {
                                return;
                            }
                            root.apply_walletconnect_relay_or_error(error);
                            cx.notify();
                        });
                    }
                    WalletConnectRelayWorkerEvent::Reconnected => {
                        let _ = this.update(cx, |root, cx| {
                            if !root.walletconnect_worker_matches(&event_client_id, event_worker_id)
                            {
                                return;
                            }
                            if root.walletconnect.relay_reconnecting {
                                root.walletconnect.relay_reconnecting = false;
                                root.walletconnect.error = None;
                                root.walletconnect.status =
                                    Some(Arc::from("WalletConnect relay reconnected."));
                                cx.notify();
                            }
                        });
                    }
                }
            }
            let _ = runtime_join.await;
        })
        .detach();
        worker
    }

    pub(in crate::root::walletconnect) fn stop_walletconnect_relay_workers_except(
        &mut self,
        active_client_ids: &BTreeSet<String>,
    ) {
        self.stop_walletconnect_relay_workers_except_with(active_client_ids, false);
    }

    pub(in crate::root::walletconnect) fn stop_terminal_walletconnect_relay_workers_except(
        &mut self,
        active_client_ids: &BTreeSet<String>,
    ) {
        self.stop_walletconnect_relay_workers_except_with(active_client_ids, true);
    }

    pub(in crate::root::walletconnect) fn stop_walletconnect_relay_workers_except_with(
        &mut self,
        active_client_ids: &BTreeSet<String>,
        unsubscribe_first: bool,
    ) {
        stop_stale_walletconnect_relay_workers(
            &mut self.walletconnect.relay_workers,
            active_client_ids,
            unsubscribe_first,
        );
    }

    pub(in crate::root) fn restart_walletconnect_relay_workers_for_network_session(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> usize {
        let restarted = self.walletconnect.relay_workers.len();
        if restarted == 0 {
            return 0;
        }
        for worker in self.walletconnect.relay_workers.values() {
            worker.stop();
        }
        self.walletconnect.relay_workers.clear();
        self.walletconnect.relay_reconnecting = false;
        self.ensure_walletconnect_relay_processing(cx);
        restarted
    }

    pub(in crate::root::walletconnect) fn walletconnect_worker_matches(
        &self,
        client_id: &str,
        worker_id: u64,
    ) -> bool {
        self.walletconnect
            .relay_workers
            .get(client_id)
            .is_some_and(|worker| worker.worker_id == worker_id)
    }

    pub(in crate::root::walletconnect) fn walletconnect_relay_processing_plan(
        &self,
        client_id: &str,
        worker_id: u64,
    ) -> Option<WalletConnectRelayProcessingPlan> {
        if !self.walletconnect_worker_matches(client_id, worker_id) {
            return None;
        }
        let store = self.vault_store.clone()?;
        let view_session = self.view_session.clone()?;
        let worker = self.walletconnect.relay_workers.get(client_id)?.clone();
        let mut pairings = Vec::new();
        if !self.walletconnect.pending_pairings.is_empty() {
            match store.load_or_create_walletconnect_relay_identity(view_session.as_ref()) {
                Ok(identity) if identity.client_id == client_id => {
                    pairings.extend(self.walletconnect.pending_pairings.values().cloned());
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(
                        target: "wallet::root::walletconnect",
                        error = %error,
                        "walletconnect worker processing skipped pending pairings; relay identity unavailable"
                    );
                }
            }
        }
        let sessions = walletconnect_active_sessions_for_relay_client(
            &self.walletconnect.sessions,
            &self.walletconnect.approval_handoff_sessions,
            client_id,
        );
        if pairings.is_empty() && sessions.is_empty() {
            return None;
        }
        Some(WalletConnectRelayProcessingPlan {
            store,
            view_session,
            worker,
            pairings,
            sessions,
            enabled_chain_ids: self.supported_walletconnect_chain_ids(),
        })
    }

    pub(in crate::root) fn ensure_walletconnect_relay_processing(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        self.expire_walletconnect_sessions(cx);
        self.expire_walletconnect_pairings(cx);
        self.sync_walletconnect_relay_workers(cx);
        self.ensure_walletconnect_pending_request_expiry_timer(cx);
        self.ensure_walletconnect_session_expiry_timer(cx);
        self.sync_walletconnect_attention();
    }

    pub(in crate::root::walletconnect) fn sync_walletconnect_relay_workers(
        &mut self,
        cx: &Context<'_, Self>,
    ) -> bool {
        let Some(store) = self.vault_store.clone() else {
            tracing::debug!(
                target: "wallet::root::walletconnect",
                "walletconnect relay processing unavailable; vault store is missing"
            );
            self.stop_walletconnect_relay_workers_except(&BTreeSet::new());
            return false;
        };
        let Some(view_session) = self.view_session.clone() else {
            tracing::debug!(
                target: "wallet::root::walletconnect",
                "walletconnect relay processing unavailable; wallet is locked"
            );
            self.stop_walletconnect_relay_workers_except(&BTreeSet::new());
            return false;
        };
        let pairings = self
            .walletconnect
            .pending_pairings
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let sessions = walletconnect_active_sessions(
            &self.walletconnect.sessions,
            &self.walletconnect.approval_handoff_sessions,
        );
        if pairings.is_empty() && sessions.is_empty() {
            self.stop_terminal_walletconnect_relay_workers_except(&BTreeSet::new());
            tracing::debug!(
                target: "wallet::root::walletconnect",
                "walletconnect relay processing unavailable; no active pairings or sessions"
            );
            return false;
        }
        let project_id = self.walletconnect_effective_project_id(cx);
        let mut sessions_by_client_id = BTreeMap::<String, Vec<WalletConnectSessionRecord>>::new();
        for session in sessions {
            sessions_by_client_id
                .entry(session.relay_client_id.clone())
                .or_default()
                .push(session);
        }
        let mut active_client_ids = BTreeSet::new();
        if !pairings.is_empty() {
            let identity = match store
                .load_or_create_walletconnect_relay_identity(view_session.as_ref())
            {
                Ok(identity) => identity,
                Err(error) => {
                    tracing::debug!(
                        target: "wallet::root::walletconnect",
                        error = %error,
                        "walletconnect relay processing unavailable; current relay identity is missing"
                    );
                    return false;
                }
            };
            let client_id = identity.client_id.clone();
            let sessions = sessions_by_client_id.remove(&client_id).unwrap_or_default();
            let client = walletconnect_client_from_identity(project_id.clone(), &identity);
            let worker = self.ensure_walletconnect_relay_worker(client, self.http.clone(), cx);
            active_client_ids.insert(client_id);
            worker.set_topics(walletconnect_relay_target_topics(&pairings, &sessions));
        }
        for (client_id, sessions) in sessions_by_client_id {
            let identity = match store
                .load_walletconnect_relay_identity_for_client_id(view_session.as_ref(), &client_id)
            {
                Ok(Some(identity)) => identity,
                Ok(None) => {
                    tracing::warn!(
                        target: "wallet::root::walletconnect",
                        relay_client_id = %walletconnect_request_key_log_label(&client_id),
                        session_count = sessions.len(),
                        "walletconnect relay processing skipped sessions with missing relay identity"
                    );
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "wallet::root::walletconnect",
                        relay_client_id = %walletconnect_request_key_log_label(&client_id),
                        session_count = sessions.len(),
                        error = %error,
                        "walletconnect relay processing could not load relay identity"
                    );
                    continue;
                }
            };
            let client = walletconnect_client_from_identity(project_id.clone(), &identity);
            let worker =
                self.ensure_walletconnect_relay_worker(client.clone(), self.http.clone(), cx);
            active_client_ids.insert(client_id);
            worker.set_topics(walletconnect_relay_target_topics(&[], &sessions));
        }
        if active_client_ids.is_empty() {
            self.stop_terminal_walletconnect_relay_workers_except(&BTreeSet::new());
            tracing::debug!(
                target: "wallet::root::walletconnect",
                "walletconnect relay processing unavailable; no relay identities matched active work"
            );
            return false;
        }
        self.stop_terminal_walletconnect_relay_workers_except(&active_client_ids);
        true
    }

    pub(in crate::root::walletconnect) fn ensure_walletconnect_pending_request_expiry_timer(
        &mut self,
        cx: &Context<'_, Self>,
    ) {
        if self.walletconnect.pending_requests.is_empty()
            || self.walletconnect.request_expiry_timer_active
        {
            return;
        }
        self.walletconnect.request_expiry_timer_active = true;
        self.walletconnect.request_expiry_generation =
            self.walletconnect.request_expiry_generation.wrapping_add(1);
        let generation = self.walletconnect.request_expiry_generation;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let keep_running = this
                    .update(cx, |root, cx| {
                        if root.walletconnect.request_expiry_generation != generation {
                            return false;
                        }
                        if root.walletconnect.pending_requests.is_empty() {
                            root.walletconnect.request_expiry_timer_active = false;
                            return false;
                        }
                        root.expire_walletconnect_pending_requests(cx);
                        if root.walletconnect.pending_requests.is_empty() {
                            root.walletconnect.request_expiry_timer_active = false;
                            false
                        } else {
                            true
                        }
                    })
                    .unwrap_or(false);
                if !keep_running {
                    return;
                }
            }
        })
        .detach();
    }

    pub(in crate::root::walletconnect) fn ensure_walletconnect_session_expiry_timer(
        &mut self,
        cx: &Context<'_, Self>,
    ) {
        let now = current_unix_seconds();
        let deadline = self
            .walletconnect
            .sessions
            .iter()
            .chain(self.walletconnect.approval_handoff_sessions.values())
            .filter(|session| {
                walletconnect_session_has_expiring_lifecycle(session)
                    && session.expiry_timestamp > now
            })
            .map(|session| session.expiry_timestamp)
            .chain(
                self.walletconnect
                    .pending_pairings
                    .values()
                    .filter_map(|pairing| pairing.expiry_timestamp)
                    .filter(|expiry| *expiry > now),
            )
            .min();
        let Some(deadline) = deadline else {
            if self.walletconnect.session_expiry_timer_active {
                self.walletconnect.session_expiry_generation =
                    self.walletconnect.session_expiry_generation.wrapping_add(1);
            }
            self.walletconnect.session_expiry_timer_active = false;
            self.walletconnect.session_expiry_deadline = None;
            return;
        };
        if self.walletconnect.session_expiry_timer_active
            && self.walletconnect.session_expiry_deadline == Some(deadline)
        {
            return;
        }
        self.walletconnect.session_expiry_timer_active = true;
        self.walletconnect.session_expiry_deadline = Some(deadline);
        self.walletconnect.session_expiry_generation =
            self.walletconnect.session_expiry_generation.wrapping_add(1);
        let generation = self.walletconnect.session_expiry_generation;
        let delay = Duration::from_secs(deadline.saturating_sub(now).max(1));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |root, cx| {
                if root.walletconnect.session_expiry_generation != generation {
                    return;
                }
                root.walletconnect.session_expiry_timer_active = false;
                root.walletconnect.session_expiry_deadline = None;
                root.expire_walletconnect_sessions(cx);
                root.expire_walletconnect_pairings(cx);
                root.ensure_walletconnect_relay_processing(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::root::walletconnect) fn expire_walletconnect_sessions(
        &mut self,
        _cx: &mut Context<'_, Self>,
    ) {
        let now = current_unix_seconds();
        let expired_session_uuids = self
            .walletconnect
            .sessions
            .iter()
            .filter(|session| {
                walletconnect_session_has_expiring_lifecycle(session)
                    && walletconnect_session_expired(session, now)
            })
            .map(|session| session.session_uuid.clone())
            .collect::<Vec<_>>();
        if expired_session_uuids.is_empty() {
            return;
        }
        let store = self.vault_store.clone();
        let view_session = self.view_session.clone();
        for session_uuid in expired_session_uuids {
            let Some(updated_session) = self
                .walletconnect
                .sessions
                .iter_mut()
                .find(|session| session.session_uuid == session_uuid)
                .map(|session| {
                    session.lifecycle_state = WalletConnectSessionLifecycleState::Expired;
                    session.clone()
                })
            else {
                continue;
            };
            self.walletconnect
                .subscriptions
                .remove(&updated_session.session_topic);
            self.walletconnect.retain_pending_requests(|_, request| {
                request.session_identity.walletconnect_session_id() != Some(session_uuid.as_str())
            });
            if let (Some(store), Some(view_session)) = (store.as_ref(), view_session.as_ref())
                && let Err(error) =
                    store.update_walletconnect_session(view_session.as_ref(), &updated_session)
            {
                tracing::warn!(
                    target: "wallet::root::walletconnect",
                    session_uuid = %walletconnect_request_key_log_label(&updated_session.session_uuid),
                    error = %error,
                    "could not persist expired walletconnect session state"
                );
            }
        }
        self.walletconnect.session_expiry_generation =
            self.walletconnect.session_expiry_generation.wrapping_add(1);
        self.walletconnect.session_expiry_timer_active = false;
        self.walletconnect.session_expiry_deadline = None;
    }

    pub(in crate::root::walletconnect) fn expire_walletconnect_pairings(
        &mut self,
        _cx: &mut Context<'_, Self>,
    ) -> bool {
        let now = current_unix_seconds();
        let expired_topics =
            expired_walletconnect_pairing_topics(&self.walletconnect.pending_pairings, now);
        if expired_topics.is_empty() {
            return false;
        }
        let mut removed_pending_proposal = false;
        for topic in expired_topics {
            self.walletconnect.pending_pairings.remove(&topic);
            self.walletconnect.subscriptions.remove(&topic);
            if !self.walletconnect.approving_proposal
                && self
                    .walletconnect
                    .pending_proposal
                    .as_ref()
                    .is_some_and(|proposal| proposal.pairing.topic == topic)
            {
                self.walletconnect.pending_proposal = None;
                removed_pending_proposal = true;
            }
        }
        self.walletconnect.status = Some(Arc::from(if removed_pending_proposal {
            "WalletConnect proposal expired. Paste a fresh wc: URI to reconnect."
        } else {
            "WalletConnect pairing expired before the dapp proposal arrived."
        }));
        self.walletconnect.session_expiry_generation =
            self.walletconnect.session_expiry_generation.wrapping_add(1);
        self.walletconnect.session_expiry_timer_active = false;
        self.walletconnect.session_expiry_deadline = None;
        true
    }

    pub(in crate::root::walletconnect) fn apply_walletconnect_relay_processing_result(
        &mut self,
        result: Result<WalletConnectRelayProcessingResult, String>,
        cx: &mut Context<'_, Self>,
    ) {
        match result {
            Ok(result) => {
                tracing::debug!(
                    target: "wallet::root::walletconnect",
                    proposal_count = result.proposals.len(),
                    pending_request_count = result.pending_requests.len(),
                    removed_session_count = result.removed_sessions.len(),
                    subscription_count = result.subscriptions.len(),
                    relay_error = result.error.is_some(),
                    "applying walletconnect relay processing result"
                );
                let reconnecting = self.walletconnect.relay_reconnecting;
                let relay_error = result.error.clone();
                for pairing_topic in result.removed_pairings {
                    self.walletconnect.pending_pairings.remove(&pairing_topic);
                }
                for proposal in result.proposals {
                    self.walletconnect.pending_proposal = Some(proposal);
                    self.walletconnect.status = Some(Arc::from(
                        "Review the WalletConnect session proposal before connecting.",
                    ));
                    self.walletconnect.error = None;
                }
                for (request, route) in result.pending_requests {
                    if self
                        .walletconnect
                        .pending_requests
                        .get(&request.key)
                        .is_some_and(|existing| {
                            existing.review_token != request.review_token
                                || existing.item.raw_details != request.item.raw_details
                        })
                    {
                        let request_key = request.key.clone();
                        self.walletconnect
                            .request_routes
                            .insert(request_key.clone(), route);
                        self.walletconnect
                            .pending_requests
                            .insert(request_key.clone(), request);
                        self.walletconnect
                            .request_disclosure_states
                            .remove(&request_key);
                        if self.walletconnect.request_dialog_key.as_deref()
                            == Some(request_key.as_str())
                        {
                            self.discard_walletconnect_fee_for_request_replacement();
                        }
                        continue;
                    }
                    if !walletconnect_request_should_queue(
                        &self.walletconnect.pending_requests,
                        &self.walletconnect.handled_request_keys,
                        &request.key,
                    ) {
                        if self
                            .walletconnect
                            .handled_request_keys
                            .contains(&request.key)
                        {
                            tracing::info!(
                                target: "wallet::root::walletconnect",
                                request_key = %walletconnect_request_key_log_label(&request.key),
                                method = request.item.method.as_str(),
                                chain_id = request.item.chain_id.as_str(),
                                dapp = request.item.dapp_name.as_str(),
                                "ignored replayed walletconnect request"
                            );
                        }
                        continue;
                    }
                    self.walletconnect
                        .dismissed_request_dialog_keys
                        .remove(&request.key);
                    tracing::info!(
                        target: "wallet::root::walletconnect",
                        request_key = %walletconnect_request_key_log_label(&request.key),
                        method = request.item.method.as_str(),
                        chain_id = request.item.chain_id.as_str(),
                        dapp = request.item.dapp_name.as_str(),
                        "queued walletconnect request"
                    );
                    self.walletconnect
                        .request_routes
                        .insert(request.key.clone(), route);
                    self.walletconnect
                        .pending_requests
                        .insert(request.key.clone(), request);
                }
                for session_uuid in result.removed_sessions {
                    tracing::info!(
                        target: "wallet::root::walletconnect",
                        session_uuid = %walletconnect_request_key_log_label(&session_uuid),
                        "walletconnect session removed by lifecycle message"
                    );
                    self.walletconnect
                        .sessions
                        .retain(|session| session.session_uuid != session_uuid);
                    self.walletconnect.retain_pending_requests(|_, request| {
                        request.session_identity.walletconnect_session_id()
                            != Some(session_uuid.as_str())
                    });
                }
                self.walletconnect
                    .subscriptions
                    .extend(result.subscriptions);
                if let Some(error) = relay_error {
                    self.apply_walletconnect_relay_or_error(error);
                } else if reconnecting {
                    self.walletconnect.relay_reconnecting = false;
                    self.walletconnect.error = None;
                    self.walletconnect.status = Some(Arc::from("WalletConnect relay reconnected."));
                    tracing::info!(
                        target: "wallet::root::walletconnect",
                        "walletconnect relay reconnected"
                    );
                }
            }
            Err(error) => {
                self.apply_walletconnect_relay_or_error(error);
            }
        }
        self.expire_walletconnect_pending_requests(cx);
        self.ensure_walletconnect_relay_processing(cx);
    }

    pub(in crate::root::walletconnect) fn expire_walletconnect_pending_requests(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        let expired_keys = expired_walletconnect_request_keys(
            &self.walletconnect.pending_requests,
            &self.walletconnect.request_actions,
            current_unix_seconds(),
        );
        let mut changed = false;
        for request_key in expired_keys {
            let Some(request) = self
                .walletconnect
                .pending_requests
                .get(&request_key)
                .cloned()
            else {
                continue;
            };
            let response_sender = self.walletconnect_response_sender(&request, cx);
            self.walletconnect.remove_pending_request(&request_key);
            changed = true;
            let response_sender = match response_sender {
                Ok(sender) => sender,
                Err(error) => {
                    self.walletconnect.error = Some(error);
                    continue;
                }
            };
            let response = Err(DappRequestError {
                provider_failure: None,
                kind: WalletConnectRequestErrorKind::ExpiredRequest,
                message: "WalletConnect request expired before approval".to_owned(),
            });
            self.walletconnect
                .request_actions
                .insert(request_key.clone());
            tracing::info!(
                target: "wallet::root::walletconnect",
                request_key = %walletconnect_request_key_log_label(&request_key),
                method = request.item.method.as_str(),
                chain_id = request.item.chain_id.as_str(),
                dapp = request.item.dapp_name.as_str(),
                "expiring walletconnect pending request"
            );
            let join = self
                .runtime
                .spawn(async move { response_sender.send(response).await });
            cx.spawn(async move |this, cx| {
                let result = join.await;
                let _ = this.update(cx, |root, cx| {
                    root.walletconnect.request_actions.remove(&request_key);
                    root.walletconnect.status = Some(Arc::from(
                        "Expired WalletConnect request removed; error response published.",
                    ));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            root.walletconnect.error = Some(Arc::from(format!(
                                "Expired request was removed locally, but relay error response failed: {error}"
                            )));
                        }
                        Err(error) => {
                            root.walletconnect.error = Some(Arc::from(format!(
                                "Expired request relay task failed: {error}"
                            )));
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        if changed {
            self.sync_walletconnect_attention();
            cx.notify();
        }
    }

    pub(in crate::root::walletconnect) fn apply_walletconnect_relay_or_error(
        &mut self,
        error: String,
    ) {
        if walletconnect_is_transient_relay_error(&error) {
            tracing::warn!(
                target: "wallet::root::walletconnect",
                error = %error,
                "walletconnect relay unavailable; worker will reconnect"
            );
            self.walletconnect.relay_reconnecting = true;
            self.walletconnect.error = None;
            self.walletconnect.status = Some(Arc::from(
                "WalletConnect relay reconnecting... Requests will resume when the relay connection recovers.",
            ));
        } else {
            tracing::warn!(
                target: "wallet::root::walletconnect",
                error = %error,
                "walletconnect error"
            );
            self.walletconnect.error = Some(Arc::from(error));
        }
    }
}
