#[allow(clippy::wildcard_imports)]
use super::*;

impl DappProvider {
    pub(in crate::gateway) fn resolve_connect(
        &mut self,
        session: u64,
        peer: PeerId,
        approval_id: &str,
        account_uuid: Option<&str>,
        chain_id: u64,
        now: Instant,
    ) {
        let peer_id = alloy::hex::encode(peer.to_bytes());
        let Some(index) = self.pending.iter().position(|pending| {
            pending.session == session
                && pending.approval_id == approval_id
                && pending.origin.paired_peer_id() == Some(peer_id.as_str())
        }) else {
            return;
        };
        let pending = self.pending.remove(index);
        let outcome = if now >= pending.deadline {
            Err(if self.wallet.view.is_none() {
                4100
            } else {
                -32002
            })
        } else if account_uuid.is_none() {
            Err(4001)
        } else if let Some(view) = self.wallet.view.as_ref() {
            if self.wallet.chain_ids.contains(&chain_id) {
                self.store
                    .grant_gateway_permission(
                        view,
                        &pending.origin,
                        account_uuid.expect("selected account"),
                        chain_id,
                    )
                    .map(|_| {
                        tracing::info!(target: LOG_TARGET, "gateway connection permission saved");
                    })
                    .map_err(|error| {
                        let error_kind = match error {
                            VaultError::Decrypt => "decrypt",
                            VaultError::Encrypt => "encrypt",
                            VaultError::Decode(_) => "decode",
                            VaultError::Encode(_) => "encode",
                            VaultError::Db(_) => "database",
                            VaultError::InvalidGatewayPermission => "invalid_gateway_permission",
                            VaultError::PublicAccountNotFound => "public_account_not_found",
                            VaultError::InvalidPublicAccountOperation => {
                                "invalid_public_account_operation"
                            }
                            VaultError::Random => "random",
                            _ => "other",
                        };
                        tracing::warn!(target: LOG_TARGET, error_kind, "gateway connection permission save failed");
                        4100
                    })
            } else {
                Err(4901)
            }
        } else {
            Err(4100)
        };
        self.reload_permissions();
        // Permission state precedes the successful connect response on every owning document.
        self.refresh_documents(false);
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        self.push_pending_ui();
        self.push_ui(session);
        match outcome {
            Ok(()) => self.admit_read(
                pending.session,
                pending.document,
                pending.request_id,
                "eth_requestAccounts",
                json!([]),
                Instant::now(),
            ),
            Err(code) => self.respond(
                pending.session,
                &pending.document,
                &pending.request_id,
                Err(code),
            ),
        }
    }
    pub(in crate::gateway) fn revoke(&mut self, permission_id: &str) -> Result<(), GatewayError> {
        let view = self.wallet.view.as_ref().ok_or(GatewayError::Unavailable)?;
        let permission = self
            .permissions
            .iter()
            .find(|permission| permission.permission_id == permission_id)
            .ok_or(GatewayError::Unavailable)?
            .clone();
        self.store
            .delete_gateway_permission(view, permission_id)
            .map_err(|_| GatewayError::Storage)?;
        self.reload_permissions();
        self.refresh_documents(false);
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        let mut retained = Vec::new();
        let mut changed = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            if pending.origin == permission.origin {
                self.respond(
                    pending.session,
                    &pending.document,
                    &pending.request_id,
                    Err(4100),
                );
                changed.push(pending.session);
            } else {
                retained.push(pending);
            }
        }
        self.pending = retained;
        changed.sort_unstable();
        changed.dedup();
        for session in changed {
            self.push_ui(session);
        }
        self.push_pending_ui();
        Ok(())
    }
    pub(in crate::gateway) fn tick(&mut self, now: Instant) {
        self.reload_permissions();
        self.refresh_wallet_switches(now);
        self.refresh_documents(false);
        self.refresh_approvals(now);
        self.invalidate_reads(now);
        let expired = self.admission.expire_queued(now);
        self.apply_updates(ReadAdmissionUpdates {
            ready: Vec::new(),
            expired,
        });
        let mut retained = Vec::new();
        let mut changed = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            if now >= pending.deadline {
                self.respond(
                    pending.session,
                    &pending.document,
                    &pending.request_id,
                    Err(if self.wallet.view.is_none() {
                        4100
                    } else {
                        -32002
                    }),
                );
                changed.push(pending.session);
            } else {
                retained.push(pending);
            }
        }
        self.pending = retained;
        for session in changed {
            self.push_ui(session);
        }
    }
    pub(super) fn reject_registration(&mut self, session: u64, document: &str, code: i32) {
        self.respond(session, document, "", Err(code));
        if let Some((_, delivery)) = self.outbox.last_mut() {
            // Existing document IDs keep their captured incarnation guard.
            delivery.registration_rejection = true;
        }
    }
    pub(super) fn respond(
        &mut self,
        session: u64,
        document: &str,
        request_id: &str,
        outcome: Result<Value, i32>,
    ) {
        let local_error = outcome.is_err();
        self.respond_outcome(session, document, request_id, outcome.map_err(safe_error));
        if let Some((_, delivery)) = self.outbox.last_mut() {
            delivery.local_error = local_error;
        }
    }
    pub(super) fn respond_error(
        &mut self,
        session: u64,
        document: &str,
        request_id: &str,
        error: ProviderRpcError,
    ) {
        self.respond_outcome(session, document, request_id, Err(error));
    }
    pub(super) fn respond_outcome(
        &mut self,
        session: u64,
        document: &str,
        request_id: &str,
        outcome: Result<Value, ProviderRpcError>,
    ) {
        let document_generation = self
            .documents
            .get(&(session, document.to_owned()))
            .map_or(0, |owner| owner.generation);
        self.outbox.push((
            session,
            Delivery {
                incarnation: self
                    .documents
                    .get(&(session, document.to_owned()))
                    .map(|doc| doc.incarnation),
                chain_editor: None,
                local_error: false,
                registration_rejection: false,
                read: None,
                approval: None,
                message: GatewayServerMessage::ProviderResponse {
                    version: 1,
                    document: document.to_owned(),
                    generation: self.generation,
                    document_generation,
                    request_id: request_id.to_owned(),
                    outcome: match outcome {
                        Ok(result) => GatewayProviderOutcome::Success { result },
                        Err(error) => GatewayProviderOutcome::Failure { error },
                    },
                },
            },
        ));
    }
    pub(super) fn push_pending_ui(&mut self) {
        self.publish_wallet_switches();
        let mut sessions: Vec<_> = self
            .pending
            .iter()
            .map(|pending| pending.session)
            .chain(self.approvals.iter().map(|pending| pending.session))
            .chain(self.ui_snapshots.keys().copied())
            .collect();
        sessions.sort_unstable();
        sessions.dedup();
        for session in sessions {
            self.push_ui(session);
        }
    }
    pub(in crate::gateway) fn push_ui(&mut self, session: u64) {
        self.publish_wallet_switches();
        let message = self.ui(session);
        let serialized = serde_json::to_value(&message).expect("serializable UI snapshot");
        if self.ui_snapshots.get(&session) != Some(&serialized) {
            self.ui_snapshots.insert(session, serialized);
            self.outbox.push((session, Delivery::control(message)));
        }
    }
    pub(super) fn ui(&self, session: u64) -> GatewayServerMessage {
        let accounts = if self
            .pending
            .iter()
            .any(|pending| pending.session == session)
        {
            self.wallet
                .view
                .as_ref()
                .and_then(|view| {
                    self.store
                        .list_active_public_accounts_for_session(view)
                        .ok()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let pending_connects = self.pending.iter().filter(|pending| pending.session == session).map(|pending| {
            let wrong_wallet = self.wallet.view.as_ref().is_some_and(|view| self.permissions.iter().find(|permission| permission.origin == pending.origin).is_some_and(|permission| matches!(self.store.resolve_dapp_session_account(view, &permission.public_account_uuid, &permission.public_account_scope, permission.owning_private_wallet_uuid.as_deref()), Ok(WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet { .. }))));
            GatewayConnectPrompt {
                request_id: pending.approval_id.clone(), url: pending.origin.web_origin().expect("dapp origin").as_str().to_owned(), paired_peer_id: pending.origin.paired_peer_id().expect("dapp peer").to_owned(),
                needs_unlock: self.wallet.view.is_none(), wrong_wallet,
                accounts: accounts.iter().map(|account| GatewayAccountChoice { uuid: account.public_account_uuid.clone(), label: account.label.clone(), address: account.address.to_string() }).collect(),
                chains: self.wallet.chain_ids.iter().map(|&id| self.wallet.chain_choice(id)).collect(), default_chain_id: self.wallet.default_chain_id,
            }
        }).collect();
        // The unlocked wallet's own accounts, so the peer can list them without a prompt.
        let wallet_accounts = self.wallet.view.as_ref().map_or_else(Vec::new, |view| {
            self.wallet
                .public_accounts
                .iter()
                .filter(|account| {
                    account.is_active_for_wallet(view.wallet_id())
                        && self
                            .wallet
                            .public_view
                            .selected_chain
                            .is_none_or(|chain| account.is_available_on_chain(chain))
                })
                .map(|account| GatewayAccountChoice {
                    uuid: account.public_account_uuid.clone(),
                    label: account.label.clone(),
                    address: account.address.to_string(),
                })
                .collect()
        });
        let mut public_view = self.wallet.public_view.clone();
        let peer_id = self
            .ui_peers
            .get(&session)
            .map(|peer| alloy::hex::encode(peer.to_bytes()));
        public_view
            .drafts
            .retain(|draft| peer_id.as_deref() == Some(draft.peer_id.as_str()));
        let mut message = GatewayServerMessage::UiSnapshot {
            version: 1,
            generation: self.generation,
            locked: self.wallet.view.is_none(),
            accounts: wallet_accounts,
            public_view,
            private_view_supported: self.wallet.private_view_supported,
            private_actions_supported: self.wallet.private_actions_supported,
            private_self_broadcast_supported: self.wallet.private_self_broadcast_supported,
            chain_management_supported: peer_id.is_some() && self.wallet.view.is_some(),
            native_usd_pricing: if peer_id.is_some() && self.wallet.view.is_some() {
                self.wallet.native_usd_pricing.clone()
            } else {
                BTreeMap::new()
            },
            network_control_supported: self.wallet.network_view.is_some() && peer_id.is_some(),
            network_view: peer_id
                .as_ref()
                .and_then(|_| self.wallet.network_view.clone().map(Box::new)),
            network_results: if peer_id.is_some() {
                self.network_results(session)
            } else {
                Vec::new()
            },
            private_view: peer_id.and_then(|_| self.wallet.private_view.clone().map(Box::new)),
            wallet_transition: self.wallet.wallet_transition,
            chains: self
                .wallet
                .chain_ids
                .iter()
                .map(|&id| self.wallet.chain_choice(id))
                .collect(),
            permissions: self.ui_peers.get(&session).map_or_else(Vec::new, |peer| {
                let peer = alloy::hex::encode(peer.to_bytes());
                self.permissions()
                    .into_iter()
                    .filter(|permission| permission.paired_peer_id == peer)
                    .map(|permission| crate::gateway::GatewaySitePermission {
                        permission_id: permission.permission_id,
                        origin: permission.url,
                        account_uuid: permission.public_account_uuid,
                        chain_id: permission.chain_id,
                    })
                    .collect()
            }),
            ui_error: self.ui_errors.get(&session).cloned(),
            pending_connects,
            pending_requests: self.pending_request_summaries(session),
        };
        if self.wallet.private_view.is_some()
            && serde_json::to_vec(&message)
                .is_ok_and(|bytes| bytes.len() > dapp_gateway_protocol::MAX_MESSAGE_LEN)
            && let GatewayServerMessage::UiSnapshot {
                private_view,
                ui_error,
                ..
            } = &mut message
        {
            // Retire the entire private observation. A truncated asset list would imply a false total.
            *private_view = None;
            *ui_error = Some(
                "Private wallet data is too large to display. Open the desktop app to continue."
                    .into(),
            );
        }
        message
    }
}
