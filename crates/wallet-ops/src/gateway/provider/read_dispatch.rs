#[allow(clippy::wildcard_imports)]
use super::*;

impl DappProvider {
    pub(super) fn dispatch_read(&mut self, id: u64) {
        let Some(mut read) = self.reads.remove(&id) else {
            return;
        };
        if let Err(code) = self.validate_read(&read, Instant::now()) {
            if let ReadResponse::Native { reply, .. } = &mut read.response {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(approvals::code_broker(code, &read.owner)));
                }
                self.finish_ticket(id);
                return;
            }
            read.phase = ReadPhase::Delivery;
            read.retired = true;
            self.respond(
                read.owner.session,
                &read.owner.document,
                &read.owner.request_id,
                Err(code),
            );
            self.reads.insert(id, read);
            return;
        }
        match read.work.take().expect("undispatched read") {
            ReadWork::Remote {
                rpc,
                balance_shortcut,
            } => {
                let answer = balance_shortcut
                    .then(|| {
                        let candidate = rpc.balance_candidate()?;
                        let (permission, account) = self.resolve(&read.owner.origin).ok()?;
                        LocalBalanceAnswer::prepare(
                            &self.wallet,
                            permission.chain_id,
                            account,
                            candidate,
                            Instant::now(),
                        )
                    })
                    .flatten();
                if let Some(answer) = answer {
                    let value = answer.value();
                    read.owner.local_balance = Some(answer);
                    read.phase = ReadPhase::Delivery;
                    self.respond(
                        read.owner.session,
                        &read.owner.document,
                        &read.owner.request_id,
                        Ok(value),
                    );
                    self.reads.insert(id, read);
                    return;
                }
                let permission = read
                    .owner
                    .permission
                    .as_ref()
                    .expect("authorized remote read");
                let http = read.owner.wallet.http.clone().expect("available context");
                let (route, origin_route) = match &read.response {
                    ReadResponse::Provider => {
                        let current = self
                            .origin_routes
                            .get(&read.owner.origin)
                            .filter(|route| route.serves(Some(&http), permission))
                            .cloned();
                        let origin_route = current.unwrap_or_else(|| {
                            let route = OriginRoute::new(http.clone(), permission);
                            self.origin_routes
                                .insert(read.owner.origin.clone(), route.clone());
                            route
                        });
                        (
                            read.owner.wallet.routes[&permission.chain_id].clone(),
                            Some(origin_route),
                        )
                    }
                    ReadResponse::Native { route, .. } => (route.clone(), None),
                };
                let origin = match &read.response {
                    ReadResponse::Provider => read.owner.origin.clone(),
                    ReadResponse::Native { .. } => WalletRpcOrigin::PublicWallet.into(),
                };
                let deadline = read.ticket.deadline;
                let lookup = matches!(read.response, ReadResponse::Provider)
                    .then(|| rpc.transaction_hash_lookup())
                    .flatten()
                    .map(|(kind, hash)| {
                        (
                            kind,
                            hash,
                            read.owner
                                .wallet
                                .public_transaction_tracker
                                .lookup_for_account(
                                    permission.chain_id,
                                    &permission.public_account_uuid,
                                    hash,
                                ),
                        )
                    });
                read.phase = ReadPhase::Running;
                self.jobs.spawn(async move {
                    let outcome = std::panic::AssertUnwindSafe(async move {
                        // Dapp reads never fall back to the shared route.
                        let route = match origin_route {
                            Some(origin_route) => origin_route.route(&route).await?,
                            None => route,
                        };
                        crate::gateway::reads::submit_read(
                            http, route, origin, rpc, lookup, deadline,
                        )
                        .await
                    })
                    .catch_unwind()
                    .await
                    .unwrap_or(Err(crate::gateway::reads::ReadError::Broker(
                        RpcBrokerError::InvalidResponse,
                    )));
                    (id, outcome)
                });
            }
            local => {
                let outcome = match local {
                    ReadWork::Accounts => Ok(json!(self.disclosure(&read.owner.origin).accounts)),
                    ReadWork::Chain => read
                        .owner
                        .permission
                        .as_ref()
                        .map(|permission| permission.chain_id)
                        .or(read.owner.unconnected_chain)
                        .map(|chain| json!(chain_hex(chain)))
                        .ok_or(4901),
                    ReadWork::Remote { .. } => unreachable!(),
                };
                read.phase = ReadPhase::Delivery;
                self.respond(
                    read.owner.session,
                    &read.owner.document,
                    &read.owner.request_id,
                    outcome,
                );
            }
        }
        self.reads.insert(id, read);
    }
    pub(in crate::gateway) fn complete_read(&mut self, (id, result): ReadCompletion) {
        self.reload_permissions();
        self.refresh_documents(false);
        let Some(mut read) = self.reads.remove(&id) else {
            return;
        };
        if matches!(read.response, ReadResponse::Native { .. }) {
            let validation = self
                .validate_read(&read, Instant::now())
                .map_err(|code| approvals::code_broker(code, &read.owner));
            if let ReadResponse::Native {
                reply: Some(reply), ..
            } = read.response
            {
                let outcome = validation.and_then(|()| {
                    result.map_err(|error| match error {
                        crate::gateway::reads::ReadError::Broker(error) => error,
                        crate::gateway::reads::ReadError::Unavailable => RpcBrokerError::Timeout,
                    })
                });
                let _ = reply.send(outcome);
            }
            self.finish_ticket(id);
            return;
        }
        if read.retired {
            // Broker capacity is released only now. Retain unsent terminal metadata.
            self.finish_ticket(id);
            if !read.delivered
                && self
                    .documents
                    .get(&(read.owner.session, read.owner.document.clone()))
                    .is_some_and(|doc| doc.incarnation == read.owner.incarnation)
            {
                read.phase = ReadPhase::Delivery;
                self.reads.insert(id, read);
            }
            return;
        }
        read.phase = ReadPhase::Delivery;
        if let Err(code) = self.validate_read(&read, Instant::now()) {
            self.respond(
                read.owner.session,
                &read.owner.document,
                &read.owner.request_id,
                Err(code),
            );
        } else if matches!(&result, Err(crate::gateway::reads::ReadError::Unavailable)) {
            self.respond(
                read.owner.session,
                &read.owner.document,
                &read.owner.request_id,
                Err(-32002),
            );
        } else {
            let availability = self.availability(&read.owner);
            let outcome = result.map_err(|error| match error {
                crate::gateway::reads::ReadError::Broker(error) => {
                    ProviderRpcError::from_broker(error, availability)
                }
                crate::gateway::reads::ReadError::Unavailable => unreachable!(),
            });
            self.respond_outcome(
                read.owner.session,
                &read.owner.document,
                &read.owner.request_id,
                outcome,
            );
        }
        self.reads.insert(id, read);
    }
    pub(super) fn availability(&self, owner: &ReadOwner) -> ProviderAvailability {
        if self.wallet.http.is_none()
            || !self
                .documents
                .contains_key(&(owner.session, owner.document.clone()))
        {
            ProviderAvailability::Disconnected
        } else if owner
            .permission
            .as_ref()
            .is_none_or(|permission| !self.wallet.routes.contains_key(&permission.chain_id))
        {
            ProviderAvailability::ChainUnavailable
        } else {
            ProviderAvailability::Available
        }
    }
    pub(super) fn finish_ticket(&mut self, id: u64) {
        let updates = self.admission.complete(id, Instant::now());
        self.apply_updates(updates);
    }
    pub(super) fn apply_updates(&mut self, updates: ReadAdmissionUpdates) {
        for ticket in updates.expired {
            if let Some(mut read) = self.reads.remove(&ticket.id) {
                if let ReadResponse::Native { reply, .. } = &mut read.response {
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(Err(RpcBrokerError::Timeout));
                    }
                    continue;
                }
                self.respond(
                    read.owner.session,
                    &read.owner.document,
                    &read.owner.request_id,
                    Err(-32002),
                );
                read.phase = ReadPhase::Delivery;
                read.retired = true;
                self.reads.insert(ticket.id, read);
            }
        }
        for ticket in updates.ready {
            self.dispatch_read(ticket.id);
        }
    }
    pub(super) fn invalidate_reads(&mut self, now: Instant) {
        let invalid: Vec<_> = self
            .reads
            .iter()
            .filter_map(|(&id, read)| {
                (!read.retired
                    || (read.phase == ReadPhase::Delivery
                        && !self
                            .documents
                            .contains_key(&(read.owner.session, read.owner.document.clone()))))
                .then(|| self.validate_read(read, now).err().map(|code| (id, code)))
                .flatten()
            })
            .collect();
        for (id, _) in &invalid {
            self.admission.cancel_queued(*id);
        }
        for (id, code) in invalid {
            let Some(mut read) = self.reads.remove(&id) else {
                continue;
            };
            let owner = &read.owner;
            if let ReadResponse::Native { reply, .. } = &mut read.response {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(approvals::code_broker(code, owner)));
                }
                if read.phase == ReadPhase::Running {
                    read.retired = true;
                    self.reads.insert(id, read);
                } else {
                    self.finish_ticket(id);
                }
                continue;
            }
            self.outbox.retain(|(session, message)| *session != owner.session || !matches!(&message.message,
                GatewayServerMessage::ProviderResponse { document, request_id, .. } if document == &owner.document && request_id == &owner.request_id));
            if !read.enqueued
                && self
                    .documents
                    .contains_key(&(owner.session, owner.document.clone()))
            {
                self.respond(owner.session, &owner.document, &owner.request_id, Err(code));
            }
            match read.phase {
                ReadPhase::Queued => {
                    self.admission.cancel_queued(id);
                    if self
                        .documents
                        .contains_key(&(read.owner.session, read.owner.document.clone()))
                    {
                        read.phase = ReadPhase::Delivery;
                        read.retired = true;
                        self.reads.insert(id, read);
                    }
                }
                ReadPhase::Running => {
                    read.retired = true;
                    self.reads.insert(id, read);
                }
                ReadPhase::Delivery => {
                    if self
                        .documents
                        .contains_key(&(read.owner.session, read.owner.document.clone()))
                    {
                        read.retired = true;
                        self.reads.insert(id, read);
                    } else {
                        self.finish_ticket(id);
                    }
                }
            }
        }
    }
}
