//! Desktop-only approval ownership. Raw request parameters never enter peer UI snapshots.
use super::{
    APPROVAL_WINDOW, DappProvider, GatewayError, GatewayPermission, GatewayWalletState,
    LocalProviderFailure, MAX_APPROVALS, PendingRead, ProviderRpcError, PublicAccountMetadata,
    ReadAdmissionDecision, ReadOwner, ReadPhase, ReadResponse, ReadWork,
};
use crate::SensitiveUrl;
use crate::dapp_request::DappRequestControl;
use crate::gateway::{GatewayApprovalFailure, admission::READ_TIMEOUT};
use crate::vault::PublicAccountSource;
use crate::walletconnect::{
    DappRequestValidationError, WalletConnectNamespaceAccountSupport, WalletConnectParsedRequest,
    parse_dapp_request_for_account, validate_dapp_request_account,
};
use crate::{RpcBrokerError, RpcOrigin, RpcRead, WalletRpcOrigin};
use serde_json::Value;
use std::sync::Arc;
use tokio::{
    sync::{oneshot, watch},
    time::Instant,
};

pub struct GatewayApprovalRequest {
    pub id: String,
    pub deadline: Instant,
    pub origin: RpcOrigin,
    /// Absent only for chain additions, which require no account grant.
    pub authorization: Option<GatewayApprovalAccount>,
    /// The proposed chain for additions; the granted chain for account-bound requests.
    pub chain_id: u64,
    pub parsed: WalletConnectParsedRequest,
    pub control: DappRequestControl,
}
pub struct GatewayApprovalAccount {
    pub permission: GatewayPermission,
    pub account: PublicAccountMetadata,
}
pub(super) struct PendingApproval {
    pub(super) id: String,
    pub(super) session: u64,
    pub(super) document: String,
    pub(super) request_id: String,
    pub(super) origin: RpcOrigin,
    incarnation: u64,
    waiting_unlock: Option<u64>,
    deadline: Instant,
    method: String,
    params: Option<Value>,
    ready: Option<(Arc<GatewayApprovalRequest>, ReadOwner)>,
    executing: bool,
    acknowledged: bool,
    summary: Option<String>,
}
pub(super) struct ApprovalDelivery {
    pub(super) owner: ReadOwner,
    pub(super) deadline: Instant,
    pub(super) control: DappRequestControl,
}
pub(super) const fn broker_code(error: &RpcBrokerError) -> i32 {
    match error {
        RpcBrokerError::OriginRejected => 4100,
        RpcBrokerError::Shutdown => 4900,
        RpcBrokerError::NoEndpoint { .. } => 4901,
        _ => -32002,
    }
}
pub(super) fn code_broker(code: i32, owner: &ReadOwner) -> RpcBrokerError {
    match code {
        4100 => RpcBrokerError::OriginRejected,
        4900 => RpcBrokerError::Shutdown,
        4901 => RpcBrokerError::NoEndpoint {
            chain_id: owner.permission.as_ref().map_or(0, |p| p.chain_id),
        },
        _ => RpcBrokerError::Timeout,
    }
}
const fn local_failure(code: i32) -> LocalProviderFailure {
    match code {
        4100 => LocalProviderFailure::Unauthorized,
        4900 => LocalProviderFailure::Disconnected,
        4901 => LocalProviderFailure::ChainUnavailable,
        4200 => LocalProviderFailure::Unsupported,
        -32602 => LocalProviderFailure::InvalidParams,
        _ => LocalProviderFailure::Unavailable,
    }
}
impl DappProvider {
    pub(in crate::gateway) fn set_approval_channels(
        &mut self,
        updates: watch::Sender<Vec<Arc<GatewayApprovalRequest>>>,
        authority: watch::Receiver<GatewayWalletState>,
        lifetime: watch::Receiver<crate::gateway::GatewaySnapshot>,
    ) {
        self.approval_updates = updates;
        self.authority = authority;
        self.authority_fallback = None;
        self.gateway_lifetime = Some(lifetime);
    }
    pub(super) fn request_approval(
        &mut self,
        session: u64,
        document: String,
        request_id: String,
        method: &str,
        params: Value,
        now: Instant,
    ) -> Result<(), GatewayError> {
        // Never assign current desktop authority to a request admitted against stale actor state.
        if !self.authority.borrow().same_authority(&self.wallet) {
            self.respond(session, &document, &request_id, Err(4100));
            return Ok(());
        }
        let waiting_unlock = if self.wallet.view.is_none() {
            if self.wallet.waiting_unlock.completed {
                self.respond(session, &document, &request_id, Err(4100));
                return Ok(());
            }
            Some(self.wallet.waiting_unlock.cohort)
        } else {
            None
        };
        let doc = &self.documents[&(session, document.clone())];
        let origin = doc.origin.clone();
        if method != "wallet_addEthereumChain"
            && self.wallet.view.is_some()
            && self.resolve(&origin).is_err()
        {
            self.respond(session, &document, &request_id, Err(4100));
            return Ok(());
        }
        if self.pending.iter().filter(|p| p.origin == origin).count()
            + self.approvals.iter().filter(|p| p.origin == origin).count()
            >= MAX_APPROVALS
        {
            self.respond(session, &document, &request_id, Err(-32005));
            return Ok(());
        }
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|_| GatewayError::Unavailable)?;
        self.approvals.push(PendingApproval {
            id: alloy::hex::encode(id),
            session,
            document,
            request_id,
            origin,
            incarnation: doc.incarnation,
            waiting_unlock,
            deadline: now + APPROVAL_WINDOW,
            method: method.to_owned(),
            params: Some(params),
            ready: None,
            executing: false,
            acknowledged: false,
            summary: None,
        });
        self.refresh_approvals(now);
        Ok(())
    }
    fn waiting_approval_is_current(&self, pending: &PendingApproval) -> Result<(), i32> {
        let current = self.authority.borrow();
        if let Some(cohort) = pending.waiting_unlock {
            if current.waiting_unlock.cohort != cohort
                || self.wallet.waiting_unlock.cohort != cohort
            {
                return Err(4100);
            }
            if self.wallet.view.is_some()
                && (!current.waiting_unlock.completed || !self.wallet.waiting_unlock.completed)
            {
                return Err(4100);
            }
        }
        if self.wallet.view.is_some() && !current.same_authority(&self.wallet) {
            return Err(4100);
        }
        Ok(())
    }
    fn prepare_approval(&self, pending: &mut PendingApproval) -> Result<(), i32> {
        self.waiting_approval_is_current(pending)?;
        let authorization = if pending.method == "wallet_addEthereumChain" {
            None
        } else {
            let (permission, account) = self.resolve(&pending.origin)?;
            if !self.wallet.chain_ids.contains(&permission.chain_id)
                || !self.wallet.routes.contains_key(&permission.chain_id)
            {
                return Err(4901);
            }
            Some(GatewayApprovalAccount {
                permission,
                account,
            })
        };
        if self.wallet.http.is_none() {
            return Err(4900);
        }
        let mut parsed = if let Some(authorization) = &authorization {
            parse_dapp_request_for_account(
                0,
                &pending.method,
                pending.params.as_ref().expect("waiting request"),
                authorization.account.address,
            )
        } else {
            crate::walletconnect::parse_dapp_chain_request(
                &pending.method,
                pending.params.as_ref().expect("waiting request"),
            )
        }
        .map_err(|error| match error {
            crate::walletconnect::WalletConnectError::UnsupportedMethod(_) => 4200,
            _ => -32602,
        })?;
        match &mut parsed {
            WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id } => {
                if !self.wallet.chain_ids.contains(chain_id)
                    || !self.wallet.routes.contains_key(chain_id)
                {
                    return Err(4901);
                }
            }
            WalletConnectParsedRequest::WalletAddEthereumChain {
                chain_id,
                raw,
                definition,
            } => {
                if !self.wallet.routes.contains_key(chain_id) {
                    if self.wallet.configured_chain_ids.contains(chain_id)
                        || self.wallet.chain_ids.contains(chain_id)
                    {
                        return Err(4901);
                    }
                    *definition = Some(Box::new(
                        super::super::policy::proposed_chain(*chain_id, raw).map_err(|_| -32602)?,
                    ));
                }
            }
            WalletConnectParsedRequest::WalletWatchAsset {
                chain_id: Some(chain_id),
                ..
            } if authorization
                .as_ref()
                .is_none_or(|authorization| *chain_id != authorization.permission.chain_id) =>
            {
                return Err(4901);
            }
            _ => {}
        }
        if let Some(GatewayApprovalAccount {
            permission,
            account,
        }) = &authorization
        {
            let support = if account.source == PublicAccountSource::HardwareDerived {
                let mode = account.hardware_descriptor.as_ref().and_then(|descriptor| {
                    self.wallet
                        .view
                        .as_ref()
                        .and_then(|view| view.hardware_profile_session())
                        .and_then(|session| session.typed_data_signing_mode(descriptor))
                });
                mode.map_or_else(
                    WalletConnectNamespaceAccountSupport::hardware_typed_data_capability_unknown,
                    WalletConnectNamespaceAccountSupport::hardware,
                )
            } else {
                WalletConnectNamespaceAccountSupport::for_account_source(account.source)
            };
            validate_dapp_request_account(&parsed, account, permission.chain_id, support).map_err(
                |error| match error {
                    DappRequestValidationError::UnsupportedMethod => 4200,
                    DappRequestValidationError::AccountMismatch => 4100,
                    DappRequestValidationError::TransactionChainMismatch
                    | DappRequestValidationError::TypedDataChainMismatch => -32602,
                },
            )?;
        }
        let doc = &self.documents[&(pending.session, pending.document.clone())];
        let wallet = self.wallet.clone();
        let live = self.authority.clone();
        let account_chain = authorization
            .as_ref()
            .map(|authorization| authorization.permission.chain_id);
        let chain_id = match &parsed {
            WalletConnectParsedRequest::WalletAddEthereumChain { chain_id, .. } => *chain_id,
            _ => account_chain.ok_or(4100)?,
        };
        let switch_target = match &parsed {
            WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id } => Some(*chain_id),
            _ => None,
        };
        let lifetime = self.gateway_lifetime.clone();
        let control = DappRequestControl::new(pending.deadline, move || {
            if lifetime.as_ref().is_some_and(|lifetime| {
                lifetime.has_changed().is_err() || !lifetime.borrow().config.enabled
            }) {
                return Err(RpcBrokerError::Shutdown);
            }
            if live.has_changed().is_err() {
                return Err(RpcBrokerError::Shutdown);
            }
            let current = live.borrow();
            if current.view.is_none()
                || current.active_wallet_generation != wallet.active_wallet_generation
                || !current.same_accounts(&wallet)
            {
                return Err(RpcBrokerError::OriginRejected);
            }
            if account_chain.is_some()
                && (!current.chain_ids.contains(&chain_id)
                    || !current.routes.contains_key(&chain_id))
            {
                return Err(RpcBrokerError::NoEndpoint { chain_id });
            }
            if current.http.is_none() {
                return Err(RpcBrokerError::Shutdown);
            }
            if !current.same_authority(&wallet)
                || (account_chain.is_some() && !current.same_chain_authority(&wallet, chain_id))
            {
                return Err(RpcBrokerError::Timeout);
            }
            if switch_target.is_some_and(|target| !current.same_chain_authority(&wallet, target)) {
                return Err(RpcBrokerError::Timeout);
            }
            Ok(())
        });
        control
            .ensure_current()
            .map_err(|error| broker_code(&error))?;
        let owner = ReadOwner {
            incarnation: pending.incarnation,
            session: pending.session,
            document: pending.document.clone(),
            request_id: pending.request_id.clone(),
            origin: pending.origin.clone(),
            document_generation: doc.generation,
            generation: self.generation,
            wallet: self.wallet.clone(),
            // Retain any existing grant only as an invalidation boundary. Adding a chain
            // never creates or changes it and never needs an account to approve.
            permission: self
                .resolve(&pending.origin)
                .ok()
                .map(|(permission, _)| permission),
            local_balance: None,
            unconnected_chain: None,
        };
        pending.ready = Some((
            Arc::new(GatewayApprovalRequest {
                id: pending.id.clone(),
                deadline: pending.deadline,
                origin: pending.origin.clone(),
                authorization,
                chain_id,
                parsed,
                control,
            }),
            owner,
        ));
        pending.params = None;
        Ok(())
    }
    pub(super) fn refresh_approvals(&mut self, now: Instant) {
        for mut pending in std::mem::take(&mut self.approvals) {
            let status = if !self
                .documents
                .get(&(pending.session, pending.document.clone()))
                .is_some_and(|doc| {
                    doc.incarnation == pending.incarnation && doc.origin == pending.origin
                }) {
                Err(4900)
            } else if let Some((request, owner)) = &pending.ready {
                request
                    .control
                    .ensure_current()
                    .map_err(|error| broker_code(&error))
                    .and_then(|()| self.validate_owner(owner, true, pending.deadline, now))
            } else if let Err(code) = self.waiting_approval_is_current(&pending) {
                Err(code)
            } else if now >= pending.deadline {
                Err(if self.wallet.view.is_none() {
                    4100
                } else {
                    -32002
                })
            } else if self.wallet.view.is_some() {
                self.prepare_approval(&mut pending)
            } else {
                Ok(())
            };
            if let Err(code) = status {
                if let Some((request, owner)) = &pending.ready {
                    request.control.invalidate(&code_broker(code, owner));
                }
                if !pending.acknowledged
                    && self
                        .documents
                        .get(&(pending.session, pending.document.clone()))
                        .is_some_and(|doc| doc.incarnation == pending.incarnation)
                {
                    self.respond(
                        pending.session,
                        &pending.document,
                        &pending.request_id,
                        Err(code),
                    );
                }
            } else if !pending.executing
                && let Some((request, owner)) = &pending.ready
                && let WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id } =
                    request.parsed
                && chain_id == request.chain_id
                && self.authority.borrow().default_chain_id == Some(chain_id)
            {
                self.respond_outcome(
                    pending.session,
                    &pending.document,
                    &pending.request_id,
                    Ok(Value::Null),
                );
                if let Some((_, delivery)) = self.outbox.last_mut() {
                    delivery.approval = Some(ApprovalDelivery {
                        owner: owner.clone(),
                        deadline: pending.deadline,
                        control: request.control.clone(),
                    });
                }
            } else {
                if !pending.acknowledged
                    && let Some((request, owner)) = &pending.ready
                    && matches!(
                        request.parsed,
                        WalletConnectParsedRequest::WalletWatchAsset { .. }
                    )
                {
                    self.respond_outcome(
                        pending.session,
                        &pending.document,
                        &pending.request_id,
                        Ok(Value::Bool(true)),
                    );
                    if let Some((_, delivery)) = self.outbox.last_mut() {
                        delivery.approval = Some(ApprovalDelivery {
                            owner: owner.clone(),
                            deadline: pending.deadline,
                            control: request.control.clone(),
                        });
                    }
                    pending.acknowledged = true;
                }
                self.approvals.push(pending);
            }
        }
        self.publish_approvals();
    }
    fn publish_approvals(&mut self) {
        self.push_pending_ui();
        let requests: Vec<_> = self
            .approvals
            .iter()
            .filter_map(|pending| {
                pending
                    .ready
                    .as_ref()
                    .map(|(request, _)| Arc::clone(request))
            })
            .collect();
        self.approval_updates.send_if_modified(|current| {
            if current.len() == requests.len()
                && current
                    .iter()
                    .zip(&requests)
                    .all(|(a, b)| Arc::ptr_eq(a, b))
            {
                false
            } else {
                current.clone_from(&requests);
                true
            }
        });
    }
    pub(in crate::gateway) fn publish_summaries(
        &mut self,
        wallet: &GatewayWalletState,
        generation: u64,
        summaries: Vec<(String, String)>,
    ) {
        self.refresh_approvals(Instant::now());
        if generation != self.generation
            || !wallet.same_state(&self.wallet)
            || !self.authority.borrow().same_state(wallet)
        {
            return;
        }
        for (id, summary) in summaries {
            if let Some(pending) = self.approvals.iter_mut().find(|pending| pending.id == id)
                && let Some((request, _)) = &pending.ready
                && request.control.ensure_current().is_ok()
            {
                pending.summary = Some(summary);
            }
        }
        self.push_pending_ui();
    }

    pub(super) fn pending_request_summaries(
        &self,
        session: u64,
    ) -> Vec<crate::gateway::GatewayPendingRequest> {
        self.approvals
            .iter()
            .filter(|pending| pending.session == session)
            .filter(|pending| pending.deadline > Instant::now())
            .filter(|pending| {
                pending.ready.as_ref().map_or_else(
                    || self.waiting_approval_is_current(pending).is_ok(),
                    |(request, owner)| {
                        request.control.ensure_current().is_ok()
                            && self
                                .validate_owner(owner, true, pending.deadline, Instant::now())
                                .is_ok()
                    },
                )
            })
            .map(|pending| crate::gateway::GatewayPendingRequest {
                request_id: pending.id.clone(),
                url: pending
                    .origin
                    .web_origin()
                    .expect("dapp origin")
                    .as_str()
                    .to_owned(),
                needs_unlock: pending.ready.is_none(),
                summary: pending.summary.clone(),
            })
            .collect()
    }

    pub(in crate::gateway) fn begin_approval(
        &mut self,
        id: &str,
    ) -> Result<Arc<GatewayApprovalRequest>, LocalProviderFailure> {
        self.tick(Instant::now());
        let pending = self
            .approvals
            .iter_mut()
            .find(|pending| pending.id == id)
            .ok_or(LocalProviderFailure::Unavailable)?;
        if pending.executing {
            return Err(LocalProviderFailure::Unavailable);
        }
        let (request, _) = pending
            .ready
            .as_ref()
            .ok_or(LocalProviderFailure::Unauthorized)?;
        request
            .control
            .ensure_current()
            .map_err(|error| local_failure(broker_code(&error)))?;
        pending.executing = true;
        Ok(Arc::clone(request))
    }
    pub(in crate::gateway) fn return_approval_to_review(
        &mut self,
        id: &str,
    ) -> Result<(), LocalProviderFailure> {
        self.tick(Instant::now());
        let pending = self
            .approvals
            .iter_mut()
            .find(|pending| pending.id == id)
            .ok_or(LocalProviderFailure::Unavailable)?;
        let (request, _) = pending
            .ready
            .as_ref()
            .ok_or(LocalProviderFailure::Unauthorized)?;
        request
            .control
            .ensure_current()
            .map_err(|error| local_failure(broker_code(&error)))?;
        if !pending.executing || request.control.handed_off() {
            return Err(LocalProviderFailure::Unavailable);
        }
        pending.executing = false;
        Ok(())
    }
    pub(in crate::gateway) fn complete_approval(
        &mut self,
        id: &str,
        result: Result<Value, GatewayApprovalFailure>,
    ) -> Result<(), LocalProviderFailure> {
        let approving = result.is_ok();
        self.tick(Instant::now());
        let index = self
            .approvals
            .iter()
            .position(|pending| pending.id == id)
            .ok_or(LocalProviderFailure::Unavailable)?;
        if !self.approvals[index].executing
            && !matches!(
                &result,
                Err(GatewayApprovalFailure::Local(
                    LocalProviderFailure::UserRejected
                ))
            )
        {
            return Err(LocalProviderFailure::Unavailable);
        }
        let pending = self.approvals.remove(index);
        let (request, mut owner) = pending.ready.ok_or(LocalProviderFailure::Unauthorized)?;
        let authorization = request
            .control
            .ensure_current()
            .map_err(|error| local_failure(broker_code(&error)))
            .and_then(|()| {
                self.validate_owner(&owner, true, pending.deadline, Instant::now())
                    .map_err(local_failure)
            });
        let result = authorization
            .map_err(GatewayApprovalFailure::Local)
            .and(result);
        let result = if result.is_ok() {
            match &request.parsed {
                WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id } => self
                    .commit_chain_switch(&request, &mut owner, *chain_id)
                    .map(|()| Value::Null)
                    .map_err(GatewayApprovalFailure::Local),
                WalletConnectParsedRequest::WalletAddEthereumChain { chain_id, .. } => {
                    // The desktop commits authority synchronously; its watch publication
                    // may still be queued when this completion reaches the actor.
                    let authority = self.authority.borrow();
                    if authority.chain_ids.contains(chain_id)
                        && authority.routes.contains_key(chain_id)
                    {
                        Ok(Value::Null)
                    } else {
                        Err(GatewayApprovalFailure::Local(
                            LocalProviderFailure::ChainUnavailable,
                        ))
                    }
                }
                _ => result,
            }
        } else {
            result
        };
        // Desktop follow-up actions require a committed approval, even when we
        // successfully queued a failure response for the website.
        let completion = match &result {
            Err(GatewayApprovalFailure::Local(failure)) if approving => Err(*failure),
            _ => Ok(()),
        };
        if pending.acknowledged {
            self.publish_approvals();
            self.invalidate_reads(Instant::now());
            return completion;
        }
        if let Err(failure) = &result {
            tracing::warn!(
                method = ?request.parsed.method(),
                chain_id = request.chain_id,
                ?failure,
                "dapp approval failed"
            );
        }
        let outcome = result.map_err(|failure| match failure {
            GatewayApprovalFailure::Local(failure) => ProviderRpcError::local(failure),
            GatewayApprovalFailure::Broker(error) => {
                ProviderRpcError::from_broker(error, self.availability(&owner))
            }
        });
        self.respond_outcome(
            pending.session,
            &pending.document,
            &pending.request_id,
            outcome,
        );
        if let Some((_, delivery)) = self.outbox.last_mut() {
            delivery.approval = Some(ApprovalDelivery {
                owner,
                deadline: pending.deadline,
                control: request.control.clone(),
            });
        }
        self.publish_approvals();
        self.invalidate_reads(Instant::now());
        completion
    }
    fn commit_chain_switch(
        &mut self,
        request: &GatewayApprovalRequest,
        owner: &mut ReadOwner,
        chain_id: u64,
    ) -> Result<(), LocalProviderFailure> {
        if !self.wallet.chain_ids.contains(&chain_id) || !self.wallet.routes.contains_key(&chain_id)
        {
            return Err(LocalProviderFailure::ChainUnavailable);
        }
        let view = self
            .wallet
            .view
            .as_ref()
            .ok_or(LocalProviderFailure::Unauthorized)?;
        request
            .control
            .ensure_current()
            .map_err(|error| local_failure(broker_code(&error)))?;
        // Keep immediate desktop authority stable across the synchronous permission write.
        let authority = self.authority.clone();
        let current = authority.borrow();
        if !current.same_authority(&self.wallet) {
            return Err(LocalProviderFailure::Unauthorized);
        }
        let authorization = request
            .authorization
            .as_ref()
            .ok_or(LocalProviderFailure::Unauthorized)?;
        self.store
            .grant_gateway_permission(
                view,
                &request.origin,
                &authorization.permission.public_account_uuid,
                chain_id,
            )
            .map_err(|_| LocalProviderFailure::Internal)?;
        drop(current);
        self.reload_permissions();
        self.refresh_documents(false);
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        let (permission, _) = self.resolve(&request.origin).map_err(local_failure)?;
        owner.permission = Some(permission);
        owner.document_generation =
            self.documents[&(owner.session, owner.document.clone())].generation;
        self.push_pending_ui();
        Ok(())
    }
    pub(in crate::gateway) fn approval_read(
        &mut self,
        id: &str,
        endpoint: SensitiveUrl,
        rpc: RpcRead,
        entered: Instant,
        reply: oneshot::Sender<Result<Value, RpcBrokerError>>,
    ) {
        self.tick(Instant::now());
        let Some((request, owner)) = self
            .approvals
            .iter()
            .find(|pending| pending.id == id)
            .and_then(|pending| pending.ready.as_ref())
        else {
            let _ = reply.send(Err(RpcBrokerError::OriginRejected));
            return;
        };
        if let Err(error) = request.control.ensure_current() {
            let _ = reply.send(Err(error));
            return;
        }
        if request.authorization.is_none() {
            let _ = reply.send(Err(RpcBrokerError::OriginRejected));
            return;
        }
        if !owner.wallet.routes[&request.chain_id]
            .endpoints()
            .contains(&endpoint)
        {
            let _ = reply.send(Err(RpcBrokerError::OriginRejected));
            return;
        }
        let deadline = entered + READ_TIMEOUT;
        let now = Instant::now();
        if now >= deadline {
            let _ = reply.send(Err(RpcBrokerError::TimeoutBeforeDispatch));
            return;
        }
        let decision =
            self.admission
                .admit_with_deadline(WalletRpcOrigin::PublicWallet.into(), now, deadline);
        let (ticket, phase) = match decision {
            Ok(ReadAdmissionDecision::Ready(ticket)) => (ticket, ReadPhase::Delivery),
            Ok(ReadAdmissionDecision::Queued(ticket)) => (ticket, ReadPhase::Queued),
            Err(limit) => {
                tracing::warn!(
                    chain_id = request.chain_id,
                    ?limit,
                    "dapp approval RPC read admission rejected"
                );
                let _ = reply.send(Err(RpcBrokerError::AdmissionRejected));
                return;
            }
        };
        let read_id = ticket.id;
        let route = owner.wallet.routes[&request.chain_id].for_endpoint(endpoint);
        self.reads.insert(
            read_id,
            PendingRead {
                response: ReadResponse::Native {
                    approval_id: id.to_owned(),
                    reply: Some(reply),
                    control: request.control.clone(),
                    route,
                },
                ticket,
                owner: owner.clone(),
                work: Some(ReadWork::Remote {
                    rpc,
                    balance_shortcut: false,
                }),
                phase,
                retired: false,
                remote: true,
                enqueued: false,
                delivered: false,
            },
        );
        if phase != ReadPhase::Queued {
            self.dispatch_read(read_id);
        }
    }
}
