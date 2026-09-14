//! A desktop-confirmed continuation of one existing account-disclosure request.
use super::{
    DappProvider, DappRequestControl, DesktopVaultStore, GatewayError, GatewayPermission,
    GatewayWalletState, PeerId, RpcBrokerError, WalletConnectSessionAccountResolution, same_http,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::{sync::watch, time::Instant};

/// Published only by the desktop lifecycle that dispatched this confirmed switch.
#[derive(Clone, PartialEq, Eq)]
pub struct GatewayWalletSwitchTransition {
    pub request_id: String,
    pub installed: bool,
}

pub struct GatewayWalletSwitchRequest {
    pub id: String,
    pub target_wallet_uuid: String,
    pub url: String,
    pub generation: u64,
    pub control: DappRequestControl,
    source: GatewayWalletState,
    permission: GatewayPermission,
    incarnation: u64,
    approved: Arc<AtomicBool>,
}
impl GatewayWalletSwitchRequest {
    #[must_use]
    pub fn awaiting_confirmation(&self) -> bool {
        !self.approved.load(Ordering::Acquire) && self.control.ensure_current().is_ok()
    }

    /// Before and after awaiting native admission, GPUI must still own this source state.
    #[must_use]
    pub fn source_is_current(&self, wallet: &GatewayWalletState, generation: u64) -> bool {
        generation == self.generation
            && self.source.same_authority(wallet)
            && self.control.ensure_current().is_ok()
    }

    #[must_use]
    pub fn source_wallet_is_current(&self, wallet: &GatewayWalletState) -> bool {
        self.source.active_wallet_generation == wallet.active_wallet_generation
            && self.source.same_accounts(wallet)
            && match (&self.source.view, &wallet.view) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }

    /// Networking may not change during an explicitly approved wallet transition.
    #[must_use]
    pub fn same_network(&self, wallet: &GatewayWalletState) -> bool {
        same_network(&self.source, wallet)
    }
}

fn same_network(source: &GatewayWalletState, current: &GatewayWalletState) -> bool {
    source.chain_ids == current.chain_ids
        && source.routes == current.routes
        && same_http(source.http.as_ref(), current.http.as_ref())
}

impl DappProvider {
    pub(in crate::gateway) fn set_switch_channel(
        &mut self,
        updates: watch::Sender<Vec<Arc<GatewayWalletSwitchRequest>>>,
    ) {
        self.switch_updates = updates;
    }

    pub(in crate::gateway) fn request_wallet_switch(
        &mut self,
        session: u64,
        peer: PeerId,
        generation: u64,
        id: &str,
        now: Instant,
    ) {
        if generation != self.generation || !self.authority.borrow().same_authority(&self.wallet) {
            return;
        }
        let peer = alloy::hex::encode(peer.to_bytes());
        let Some(index) = self.pending.iter().position(|pending| {
            pending.session == session
                && pending.approval_id == id
                && pending.deadline > now
                && pending.origin.paired_peer_id() == Some(peer.as_str())
        }) else {
            return;
        };
        let pending = &self.pending[index];
        if pending.wallet_switch.is_some() {
            return;
        }
        let Some(view) = &self.wallet.view else {
            return;
        };
        let Some(permission) = self
            .permissions
            .iter()
            .find(|permission| permission.origin == pending.origin)
            .cloned()
        else {
            return;
        };
        let Ok(WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet {
            owning_wallet_uuid,
        }) = self.store.resolve_dapp_session_account(
            view,
            &permission.public_account_uuid,
            &permission.public_account_scope,
            permission.owning_private_wallet_uuid.as_deref(),
        )
        else {
            return;
        };
        if !self.wallet.chain_ids.contains(&permission.chain_id) {
            return;
        }
        let Some(doc) = self.documents.get(&(session, pending.document.clone())) else {
            return;
        };
        let source = self.wallet.clone();
        let authority = self.authority.clone();
        let captured = source.clone();
        let target = owning_wallet_uuid.clone();
        let request_id = id.to_owned();
        let approved = Arc::new(AtomicBool::new(false));
        let approval = approved.clone();
        let store = DesktopVaultStore::from_db(self.store.db());
        let saved_permission = permission.clone();
        let lifetime = self.gateway_lifetime.clone();
        let control = DappRequestControl::new(pending.deadline, move || {
            if authority.has_changed().is_err()
                || lifetime.as_ref().is_some_and(|lifetime| {
                    lifetime.has_changed().is_err() || !lifetime.borrow().config.enabled
                })
            {
                return Err(RpcBrokerError::Shutdown);
            }
            let current = authority.borrow();
            let continuation = approval.load(Ordering::Acquire)
                && current
                    .wallet_switch
                    .as_ref()
                    .is_some_and(|transition| transition.request_id == request_id)
                && current.view.as_ref().is_none_or(|view| {
                    (view.wallet_id() == target
                        || captured
                            .view
                            .as_ref()
                            .is_some_and(|source| Arc::ptr_eq(source, view)))
                        && same_network(&captured, &current)
                });
            if !captured.same_authority(&current) && !continuation {
                return Err(RpcBrokerError::OriginRejected);
            }
            if !captured.view.as_ref().is_some_and(|view| {
                store
                    .list_gateway_permissions(view)
                    .is_ok_and(|permissions| permissions.contains(&saved_permission))
            }) {
                return Err(RpcBrokerError::OriginRejected);
            }
            Ok(())
        });
        let url = pending
            .origin
            .web_origin()
            .expect("dapp origin")
            .as_str()
            .to_owned();
        let incarnation = doc.incarnation;
        self.pending[index].wallet_switch = Some(Arc::new(GatewayWalletSwitchRequest {
            id: id.to_owned(),
            target_wallet_uuid: owning_wallet_uuid,
            url,
            generation,
            control,
            source,
            permission,
            incarnation,
            approved,
        }));
        self.publish_wallet_switches();
    }

    pub(in crate::gateway) fn begin_wallet_switch(
        &mut self,
        id: &str,
    ) -> Result<Arc<GatewayWalletSwitchRequest>, GatewayError> {
        self.tick(Instant::now());
        let request = self
            .pending
            .iter()
            .find(|pending| pending.approval_id == id)
            .and_then(|pending| pending.wallet_switch.as_ref())
            .ok_or(GatewayError::Unavailable)?;
        if !request.source_is_current(&self.wallet, self.generation)
            || request.approved.swap(true, Ordering::AcqRel)
        {
            return Err(GatewayError::Unavailable);
        }
        let request = request.clone();
        self.publish_wallet_switches();
        Ok(request)
    }

    pub(in crate::gateway) fn reject_wallet_switch(&mut self, id: &str) {
        let Some(index) = self
            .pending
            .iter()
            .position(|pending| pending.approval_id == id && pending.wallet_switch.is_some())
        else {
            return;
        };
        let pending = self.pending.remove(index);
        if let Some(request) = &pending.wallet_switch {
            request.control.invalidate(&RpcBrokerError::OriginRejected);
        }
        self.respond(
            pending.session,
            &pending.document,
            &pending.request_id,
            Err(4001),
        );
        self.push_ui(pending.session);
    }

    pub(super) fn publish_wallet_switches(&self) {
        let requests: Vec<_> = self
            .pending
            .iter()
            .filter_map(|pending| pending.wallet_switch.clone())
            .collect();
        // A dialog/event retaining an Arc must lose authority as soon as its original request ends.
        for previous in self.switch_updates.borrow().iter() {
            if !requests
                .iter()
                .any(|request| Arc::ptr_eq(request, previous))
            {
                previous.control.invalidate(&RpcBrokerError::OriginRejected);
            }
        }
        self.switch_updates.send_if_modified(|current| {
            if current.len() == requests.len()
                && current
                    .iter()
                    .zip(&requests)
                    .all(|(left, right)| Arc::ptr_eq(left, right))
            {
                false
            } else {
                current.clone_from(&requests);
                true
            }
        });
    }

    pub(super) fn refresh_wallet_switches(&mut self, now: Instant) {
        let mut retained = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            let Some(request) = &pending.wallet_switch else {
                retained.push(pending);
                continue;
            };
            let document_current = self
                .documents
                .get(&(pending.session, pending.document.clone()))
                .is_some_and(|doc| {
                    doc.incarnation == request.incarnation && doc.origin == pending.origin
                });
            if now >= pending.deadline
                || !document_current
                || request.control.ensure_current().is_err()
            {
                request.control.invalidate(&RpcBrokerError::OriginRejected);
                if document_current {
                    self.respond(
                        pending.session,
                        &pending.document,
                        &pending.request_id,
                        Err(if now >= pending.deadline {
                            -32002
                        } else {
                            4100
                        }),
                    );
                }
                continue;
            }
            let installed = request.approved.load(Ordering::Acquire)
                && self
                    .wallet
                    .wallet_switch
                    .as_ref()
                    .is_some_and(|transition| {
                        transition.request_id == request.id && transition.installed
                    })
                && self
                    .wallet
                    .view
                    .as_ref()
                    .is_some_and(|view| view.wallet_id() == request.target_wallet_uuid)
                && self.authority.borrow().same_authority(&self.wallet);
            if !installed {
                retained.push(pending);
                continue;
            }
            let allowed = request.same_network(&self.wallet)
                && self
                    .resolve(&pending.origin)
                    .is_ok_and(|(permission, _)| permission == request.permission);
            if allowed {
                // Re-enter ordinary read admission only after the intended view was installed.
                self.refresh_documents(false);
                self.admit_read(
                    pending.session,
                    pending.document.clone(),
                    pending.request_id.clone(),
                    "eth_requestAccounts",
                    json!([]),
                    now,
                );
                for read in self.reads.values_mut().filter(|read| {
                    read.owner.session == pending.session
                        && read.owner.document == pending.document
                        && read.owner.request_id == pending.request_id
                }) {
                    read.ticket.deadline = read.ticket.deadline.min(pending.deadline);
                }
            } else {
                self.respond(
                    pending.session,
                    &pending.document,
                    &pending.request_id,
                    Err(4100),
                );
            }
        }
        self.pending = retained;
        self.publish_wallet_switches();
    }
}
