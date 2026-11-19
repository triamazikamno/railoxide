#[allow(clippy::wildcard_imports)]
use super::*;

impl DappProvider {
    pub(in crate::gateway) fn new(store: DesktopVaultStore, generation: u64) -> Self {
        let (authority_fallback, authority) = watch::channel(GatewayWalletState::default());
        Self {
            store,
            wallet: GatewayWalletState::default(),
            generation,
            documents: HashMap::new(),
            permissions: Vec::new(),
            pending: Vec::new(),
            approvals: Vec::new(),
            approval_updates: watch::channel(Vec::new()).0,
            switch_updates: watch::channel(Vec::new()).0,
            authority,
            authority_fallback: Some(authority_fallback),
            gateway_lifetime: None,
            admission: ReadAdmission::new(),
            reads: HashMap::new(),
            origin_routes: HashMap::new(),
            jobs: JoinSet::new(),
            outbox: Vec::new(),
            next_document: 0,
            ui_snapshots: HashMap::new(),
            ui_peers: HashMap::new(),
            ui_errors: HashMap::new(),
            network_views: network::NetworkViews::default(),
            chain_editor_views: chain_editor::ChainEditorViews::default(),
        }
    }
    pub(in crate::gateway) const fn wallet_transition(&self) -> bool {
        self.wallet.wallet_transition
    }
    pub(in crate::gateway) fn retire_sessions(&mut self, live: impl Fn(u64) -> bool) {
        self.network_views.retire_sessions(&live);
        self.chain_editor_views.retire_sessions(&live);
        self.ui_snapshots.retain(|session, _| live(*session));
        self.ui_peers.retain(|session, _| live(*session));
        self.ui_errors.retain(|session, _| live(*session));
        self.documents.retain(|(session, _), _| live(*session));
        self.retain_origin_routes();
        self.pending.retain(|pending| live(pending.session));
        self.outbox.retain(|(session, _)| live(*session));
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        // Origin rate charges survive reconnect and document replacement.
    }
    pub(in crate::gateway) fn drain(&mut self) -> Vec<(u64, Delivery)> {
        let mut outbox = std::mem::take(&mut self.outbox);
        for (session, delivery) in &mut outbox {
            if let GatewayServerMessage::ProviderResponse {
                document,
                request_id,
                ..
            } = &delivery.message
                && let Some(read) = self.reads.values_mut().find(|read| {
                    matches!(read.response, ReadResponse::Provider)
                        && read.owner.session == *session
                        && read.owner.document == *document
                        && read.owner.request_id == *request_id
                })
            {
                delivery.read = Some((read.ticket.clone(), read.owner.clone(), read.remote));
                read.enqueued = true;
            }
        }
        outbox
    }
    pub(in crate::gateway) fn ui_event(
        &self,
        generation: u64,
        kind: crate::gateway::GatewayUiEventKind,
        live: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<crate::gateway::GatewayUiEvent> {
        if generation != self.generation || !self.authority.borrow().same_authority(&self.wallet) {
            return None;
        }
        let live = Arc::downgrade(live);
        let authority = self.authority.clone();
        let wallet = self.wallet.clone();
        let captured = wallet.clone();
        Some(crate::gateway::GatewayUiEvent {
            generation,
            kind,
            wallet,
            control: DappRequestControl::new(Instant::now() + Duration::from_secs(2), move || {
                if !live
                    .upgrade()
                    .is_some_and(|live| live.load(std::sync::atomic::Ordering::Acquire))
                    || authority.has_changed().is_err()
                {
                    return Err(RpcBrokerError::Shutdown);
                }
                if !authority.borrow().same_authority(&captured) {
                    return Err(RpcBrokerError::OriginRejected);
                }
                Ok(())
            }),
        })
    }
    pub(in crate::gateway) fn is_current_wallet_state(&self, wallet: &GatewayWalletState) -> bool {
        wallet.same_state(&self.authority.borrow())
    }

    pub(in crate::gateway) fn same_authority(&self, wallet: &GatewayWalletState) -> bool {
        self.wallet.same_authority(wallet)
    }
    pub(in crate::gateway) fn permissions(&self) -> Vec<GatewayPermissionSummary> {
        if self.wallet.view.is_none() {
            return Vec::new();
        }
        self.permissions
            .iter()
            .filter_map(|permission| {
                Some(GatewayPermissionSummary {
                    permission_id: permission.permission_id.clone(),
                    url: permission.origin.web_origin()?.as_str().to_owned(),
                    paired_peer_id: permission.origin.paired_peer_id()?.to_owned(),
                    public_account_uuid: permission.public_account_uuid.clone(),
                    chain_id: permission.chain_id,
                })
            })
            .collect()
    }
    pub(in crate::gateway) fn update_wallet(
        &mut self,
        mut wallet: GatewayWalletState,
        generation: u64,
    ) {
        if !wallet.private_view_supported {
            wallet.private_view = None;
        }
        if wallet.view.is_none() {
            wallet.network_view = None;
            wallet.public_view = crate::gateway::GatewayPublicView::default();
            wallet.private_view = None;
            self.ui_errors.clear();
            wallet.public_accounts.clear();
            wallet.token_registry = None;
            wallet.chain_ids.clear();
            wallet.default_chain_id = None;
            wallet.http = None;
            wallet.routes.clear();
        }
        self.retire_network_context(&wallet);
        if !self.wallet.same_authority(&wallet) {
            self.chain_editor_views.retire_context();
        }
        if let Some(authority) = &self.authority_fallback {
            authority.send_replace(wallet.clone());
        }
        self.wallet = wallet;
        let previous = std::mem::replace(&mut self.generation, generation);
        self.reload_permissions();
        self.refresh_wallet_switches(Instant::now());
        // Changed disclosures are pushed anyway; an unchanged same-generation state is redundant,
        // except while locked: the bridge resets documents on every locked State and needs it.
        self.refresh_documents(generation != previous || self.wallet.view.is_none());
        self.refresh_approvals(Instant::now());
        self.invalidate_reads(Instant::now());
        self.push_pending_ui();
    }
    pub(super) fn reload_permissions(&mut self) {
        self.permissions = self
            .wallet
            .view
            .as_ref()
            .and_then(|view| self.store.list_gateway_permissions(view).ok())
            .unwrap_or_default();
        self.retain_origin_routes();
    }
    /// Drops each origin route whose origin has no document left, or whose context or grant
    /// changed. The origin's next read opens a new one.
    pub(super) fn retain_origin_routes(&mut self) {
        let (documents, permissions) = (&self.documents, &self.permissions);
        let http = self.wallet.http.as_ref();
        self.origin_routes.retain(|origin, route| {
            documents
                .values()
                .any(|document| &document.origin == origin)
                && permissions
                    .iter()
                    .find(|permission| &permission.origin == origin)
                    .is_some_and(|permission| route.serves(http, permission))
        });
    }
    pub(super) fn resolve(
        &self,
        origin: &RpcOrigin,
    ) -> Result<(GatewayPermission, PublicAccountMetadata), i32> {
        let view = self.wallet.view.as_ref().ok_or(4100)?;
        let permission = self
            .permissions
            .iter()
            .find(|permission| &permission.origin == origin)
            .ok_or(4100)?;
        match self.store.resolve_dapp_session_account(
            view,
            &permission.public_account_uuid,
            &permission.public_account_scope,
            permission.owning_private_wallet_uuid.as_deref(),
        ) {
            Ok(WalletConnectSessionAccountResolution::Usable(account))
                if account.is_available_on_chain(permission.chain_id) =>
            {
                Ok((permission.clone(), account))
            }
            _ => Err(4100),
        }
    }
    pub(super) fn disclosure(&self, origin: &RpcOrigin) -> Disclosure {
        let Ok((permission, account)) = self.resolve(origin) else {
            return Disclosure::default();
        };
        Disclosure {
            accounts: vec![account.address.to_string()],
            chain_id: self
                .wallet
                .chain_ids
                .contains(&permission.chain_id)
                .then(|| chain_hex(permission.chain_id)),
            permission_id: Some(permission.permission_id),
        }
    }
    pub(super) fn push_state(&mut self, key: &(u64, String), invalidation_code: Option<i32>) {
        if let Some(document) = self.documents.get(key) {
            self.outbox.push((
                key.0,
                Delivery {
                    incarnation: Some(document.incarnation),
                    chain_editor: None,
                    local_error: false,
                    registration_rejection: false,
                    read: None,
                    approval: None,
                    message: GatewayServerMessage::ProviderState {
                        version: 1,
                        document: key.1.clone(),
                        generation: self.generation,
                        document_generation: document.generation,
                        accounts: document.disclosure.accounts.clone(),
                        chain_id: document.disclosure.chain_id.clone(),
                        invalidation_code,
                    },
                },
            ));
        }
    }
    pub(super) fn refresh_documents(&mut self, force: bool) {
        let keys: Vec<_> = self.documents.keys().cloned().collect();
        for key in keys {
            let disclosure = self.disclosure(&self.documents[&key].origin);
            let document = self.documents.get_mut(&key).expect("known document");
            let changed = document.disclosure != disclosure;
            let invalidation_code = changed.then(|| {
                if disclosure.accounts.is_empty()
                    || disclosure.accounts != document.disclosure.accounts
                {
                    4100
                } else if disclosure.chain_id.is_none() {
                    4901
                } else {
                    -32002
                }
            });
            if changed {
                document.generation += 1;
                document.disclosure = disclosure;
            }
            if changed || force {
                self.push_state(&key, invalidation_code);
            }
        }
    }
}
