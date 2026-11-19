#[allow(clippy::wildcard_imports)]
use super::*;

impl DappProvider {
    pub(in crate::gateway) fn register(
        &mut self,
        session: u64,
        peer: PeerId,
        document: String,
        url: &str,
    ) -> Result<(), GatewayError> {
        if !valid_id(&document) {
            return Err(GatewayError::Unavailable);
        }
        let Ok(origin) = RpcOrigin::dapp_web_origin(alloy::hex::encode(peer.to_bytes()), url)
        else {
            self.reject_registration(session, &document, -32602);
            return Ok(());
        };
        let key = (session, document);
        self.ui_peers.insert(session, peer);
        if let Some(existing) = self.documents.get(&key) {
            if existing.origin != origin {
                return Err(GatewayError::Unavailable);
            }
            self.push_state(&key, None);
            return Ok(());
        }
        if self.documents.len() >= MAX_DOCUMENTS
            || self
                .documents
                .keys()
                .filter(|(owner, _)| *owner == session)
                .count()
                >= MAX_SESSION_DOCUMENTS
        {
            self.reject_registration(session, &key.1, -32005);
            return Ok(());
        }
        let disclosure = self.disclosure(&origin);
        self.next_document += 1;
        self.documents.insert(
            key.clone(),
            Document {
                incarnation: self.next_document,
                origin,
                generation: 1,
                disclosure,
            },
        );
        self.push_state(&key, None);
        Ok(())
    }
    pub(in crate::gateway) fn unregister(&mut self, session: u64, document: &str) {
        self.documents.remove(&(session, document.to_owned()));
        self.retain_origin_routes();
        self.pending
            .retain(|pending| pending.session != session || pending.document != document);
        self.outbox.retain(|(owner, message)| *owner != session || !matches!(&message.message, GatewayServerMessage::ProviderResponse { document: target, .. } | GatewayServerMessage::ProviderState { document: target, .. } if target == document));
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        self.push_ui(session);
    }
    pub(in crate::gateway) fn request(
        &mut self,
        session: u64,
        document: String,
        request_id: String,
        method: &str,
        params: Value,
        now: Instant,
    ) -> Result<(), GatewayError> {
        self.tick(now);
        if !valid_id(&document) || !valid_id(&request_id) {
            return Err(GatewayError::Unavailable);
        }
        let Some(owner) = self.documents.get(&(session, document.clone())) else {
            self.respond(session, &document, &request_id, Err(4100));
            return Ok(());
        };
        let origin = owner.origin.clone();
        if self.pending.iter().any(|pending| pending.session == session && pending.document == document && pending.request_id == request_id)
            || self.approvals.iter().any(|pending| pending.session == session && pending.document == document && pending.request_id == request_id)
            || self.reads.values().any(|read| matches!(read.response, ReadResponse::Provider) && read.owner.session == session && read.owner.document == document && read.owner.request_id == request_id)
            || self.outbox.iter().any(|(owner, message)| *owner == session && matches!(&message.message, GatewayServerMessage::ProviderResponse { document: target, request_id: id, .. } if target == &document && id == &request_id))
        { return Err(GatewayError::Unavailable); }
        let local = matches!(
            method,
            "eth_accounts" | "eth_chainId" | "eth_requestAccounts"
        );
        if matches!(
            method,
            "personal_sign"
                | "eth_signTypedData"
                | "eth_signTypedData_v4"
                | "eth_sendTransaction"
                | "wallet_switchEthereumChain"
                | "wallet_addEthereumChain"
                | "wallet_watchAsset"
        ) {
            return self.request_approval(session, document, request_id, method, params, now);
        }
        if !local && !remote_method(method) {
            self.respond(session, &document, &request_id, Err(4200));
            return Ok(());
        }
        if local && !params.is_null() && !params.as_array().is_some_and(Vec::is_empty) {
            self.respond(session, &document, &request_id, Err(-32602));
            return Ok(());
        }
        if method == "eth_requestAccounts" {
            self.reload_permissions();
        }
        if method == "eth_requestAccounts" && self.resolve(&origin).is_err() {
            if self
                .pending
                .iter()
                .filter(|pending| pending.origin == origin)
                .count()
                + self
                    .approvals
                    .iter()
                    .filter(|pending| pending.origin == origin)
                    .count()
                >= MAX_APPROVALS
            {
                self.respond(session, &document, &request_id, Err(-32005));
                return Ok(());
            }
            let mut id = [0; 16];
            getrandom::fill(&mut id).map_err(|_| GatewayError::Unavailable)?;
            self.pending.push(PendingConnect {
                approval_id: alloy::hex::encode(id),
                session,
                document,
                request_id,
                origin,
                deadline: now + APPROVAL_WINDOW,
                wallet_switch: None,
            });
            self.push_ui(session);
            return Ok(());
        }
        self.admit_read(session, document, request_id, method, params, now);
        Ok(())
    }
    pub(super) fn admit_read(
        &mut self,
        session: u64,
        document: String,
        request_id: String,
        method: &str,
        params: Value,
        now: Instant,
    ) {
        self.reload_permissions();
        self.refresh_documents(false);
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        let Some(doc) = self.documents.get(&(session, document.clone())) else {
            return;
        };
        let origin = doc.origin.clone();
        let permission = match self.resolve(&origin) {
            Ok((permission, _)) => Some(permission),
            Err(_) if matches!(method, "eth_accounts" | "eth_chainId") => None,
            Err(code) => {
                self.respond(session, &document, &request_id, Err(code));
                return;
            }
        };
        let unconnected_chain = if method == "eth_chainId" && permission.is_none() {
            if self.wallet.view.is_none() {
                self.respond(session, &document, &request_id, Err(4100));
                return;
            }
            let Some(chain_id) = self
                .wallet
                .default_chain_id
                .filter(|chain| self.wallet.chain_ids.contains(chain))
            else {
                self.respond(session, &document, &request_id, Err(4901));
                return;
            };
            Some(chain_id)
        } else {
            None
        };
        if permission
            .as_ref()
            .is_some_and(|grant| !self.wallet.chain_ids.contains(&grant.chain_id))
        {
            self.respond(session, &document, &request_id, Err(4901));
            return;
        }
        let work = match method {
            "eth_accounts" | "eth_requestAccounts" => ReadWork::Accounts,
            "eth_chainId" => ReadWork::Chain,
            _ => {
                let chain = permission
                    .as_ref()
                    .expect("authorized remote read")
                    .chain_id;
                if !self.wallet.routes.contains_key(&chain) {
                    self.respond(session, &document, &request_id, Err(4901));
                    return;
                }
                if self.wallet.http.is_none() {
                    self.respond(session, &document, &request_id, Err(4900));
                    return;
                }
                let balance_shortcut = permits_balance_shortcut(method, &params);
                match crate::gateway::reads::parse_read(method, params, chain) {
                    Ok(rpc) => ReadWork::Remote {
                        rpc,
                        balance_shortcut,
                    },
                    Err(error) => {
                        self.respond_error(session, &document, &request_id, error);
                        return;
                    }
                }
            }
        };
        let owner = ReadOwner {
            incarnation: doc.incarnation,
            session,
            document,
            request_id,
            origin: origin.clone(),
            document_generation: doc.generation,
            generation: self.generation,
            wallet: self.wallet.clone(),
            permission,
            local_balance: None,
            unconnected_chain,
        };
        let (ticket, phase) = match self.admission.admit(origin, now) {
            Ok(ReadAdmissionDecision::Ready(ticket)) => (ticket, ReadPhase::Delivery),
            Ok(ReadAdmissionDecision::Queued(ticket)) => (ticket, ReadPhase::Queued),
            Err(_) => {
                self.respond(
                    owner.session,
                    &owner.document,
                    &owner.request_id,
                    Err(-32005),
                );
                return;
            }
        };
        let id = ticket.id;
        let remote = matches!(work, ReadWork::Remote { .. });
        self.reads.insert(
            id,
            PendingRead {
                response: ReadResponse::Provider,
                ticket,
                owner,
                work: Some(work),
                phase,
                retired: false,
                remote,
                enqueued: false,
                delivered: false,
            },
        );
        if phase != ReadPhase::Queued {
            self.dispatch_read(id);
        }
    }
    pub(super) fn validate_read(&self, read: &PendingRead, now: Instant) -> Result<(), i32> {
        self.validate_owner(&read.owner, read.remote, read.ticket.deadline, now)?;
        if let ReadResponse::Native {
            approval_id,
            reply,
            control,
            ..
        } = &read.response
        {
            if reply.as_ref().is_some_and(oneshot::Sender::is_closed) {
                return Err(-32002);
            }
            if !self
                .approvals
                .iter()
                .any(|pending| &pending.id == approval_id)
            {
                return Err(4100);
            }
            control
                .ensure_current()
                .map_err(|error| approvals::broker_code(&error))?;
        }
        Ok(())
    }
    pub(super) fn validate_owner(
        &self,
        owner: &ReadOwner,
        remote: bool,
        deadline: Instant,
        now: Instant,
    ) -> Result<(), i32> {
        let doc = self
            .documents
            .get(&(owner.session, owner.document.clone()))
            .ok_or(4900)?;
        if doc.origin != owner.origin || doc.incarnation != owner.incarnation {
            return Err(4900);
        }
        let authority = self.authority.borrow();
        if !authority.same_authority(&owner.wallet) {
            return Err(if authority.view.is_none() && owner.wallet.view.is_some() {
                4100
            } else {
                -32002
            });
        }
        if owner.unconnected_chain.is_some_and(|chain| {
            self.wallet.default_chain_id != Some(chain)
                || authority.default_chain_id != Some(chain)
                || !self.wallet.chain_ids.contains(&chain)
                || !authority.chain_ids.contains(&chain)
        }) {
            return Err(-32002);
        }
        drop(authority);
        let resolved = self.resolve(&owner.origin).ok();
        if resolved.as_ref().map(|(permission, _)| permission) != owner.permission.as_ref() {
            return Err(4100);
        }
        if let Some(permission) = &owner.permission {
            if !self.wallet.chain_ids.contains(&permission.chain_id) {
                return Err(4901);
            }
            if !self
                .wallet
                .same_chain_authority(&owner.wallet, permission.chain_id)
                || !self
                    .authority
                    .borrow()
                    .same_chain_authority(&owner.wallet, permission.chain_id)
            {
                return Err(-32002);
            }
            if remote {
                if self.wallet.http.is_none() {
                    return Err(4900);
                }
                if !self.wallet.routes.contains_key(&permission.chain_id) {
                    return Err(4901);
                }
            }
        }
        if (self.generation, doc.generation) != (owner.generation, owner.document_generation)
            || !self.wallet.same_authority(&owner.wallet)
            || now >= deadline
        {
            return Err(
                if self.wallet.view.is_none() && owner.wallet.view.is_some() {
                    4100
                } else {
                    -32002
                },
            );
        }
        if let Some(answer) = &owner.local_balance {
            let account = &resolved.as_ref().ok_or(4100)?.1;
            if !answer.is_current(&self.wallet, account, now) {
                return Err(-32002);
            }
        }
        Ok(())
    }
}
