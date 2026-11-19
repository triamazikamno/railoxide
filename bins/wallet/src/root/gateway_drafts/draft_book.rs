#[allow(clippy::wildcard_imports)]
use super::*;

impl GatewayDraftBook {
    pub(in crate::root) fn retire_chain(&mut self, chain_id: u64) {
        self.records.retain(|_, record| {
            if record.view.input.chain_id() == chain_id {
                retire_record(record);
                false
            } else {
                true
            }
        });
    }

    pub(super) fn create(
        &mut self,
        peer_id: &str,
        request_id: String,
        input: GatewayDraftPayload,
        wallet: Arc<DesktopViewSession>,
        wallet_generation: u64,
    ) -> bool {
        if request_id.is_empty() || request_id.len() > 64 || !valid_input_size(&input) {
            return false;
        }
        if let Some(existing) = self.records.get(peer_id) {
            // A repeated Create cannot replace work or restart an estimate after reconnect.
            if existing.view.request_id == request_id || existing.execution.is_some() {
                return false;
            }
        } else if self.records.len() >= MAX_DRAFT_PEERS {
            return false;
        }
        if let Some(old) = self.records.remove(peer_id)
            && let Some(job) = old.estimation
        {
            job.abort();
        }
        let draft_id = alloy::hex::encode(rand::random::<[u8; 16]>());
        self.records.insert(
            peer_id.to_owned(),
            DraftRecord {
                view: GatewayDraftView {
                    peer_id: peer_id.to_owned(),
                    draft_id,
                    request_id,
                    revision: 0,
                    input,
                    status: GatewayDraftStatus::Editing,
                    estimate: None,
                    private_progress: None,
                    private_options: None,
                    gas_quote: None,
                    recipients: Vec::new(),
                    step_label: String::new(),
                    message: String::new(),
                    warning: false,
                    can_cancel: true,
                    can_retry: false,
                },
                wallet,
                wallet_generation,
                prepared: None,
                context_binding: None,
                estimated_at: None,
                estimation: None,
                execution: None,
                picker: None,
                operation: None,
            },
        );
        true
    }

    pub(super) fn update(
        &mut self,
        peer_id: &str,
        draft_id: &str,
        revision: u64,
        input: GatewayDraftPayload,
        wallet: &Arc<DesktopViewSession>,
        wallet_generation: u64,
    ) -> bool {
        if !valid_input_size(&input) {
            return false;
        }
        let Some(record) = self.records.get_mut(peer_id).filter(|record| {
            record.view.draft_id == draft_id
                && record.execution.is_none()
                && revision > record.view.revision
                && Arc::ptr_eq(wallet, &record.wallet)
                && wallet_generation == record.wallet_generation
                && std::mem::discriminant(&record.view.input) == std::mem::discriminant(&input)
        }) else {
            return false;
        };
        if let Some(job) = record.estimation.take() {
            job.abort();
        }
        if quote_identity(&record.view.input) != quote_identity(&input) {
            record.view.gas_quote = None;
        }
        record.picker = None;
        if record.view.input.public().map(|input| &input.account)
            != input.public().map(|input| &input.account)
        {
            record.operation = None;
        }
        record.view.input = input;
        record.view.revision = revision;
        record.view.estimate = None;
        record.prepared = None;
        record.estimated_at = None;
        true
    }

    pub(super) fn dismiss(&mut self, peer_id: &str, draft_id: &str) -> bool {
        if !self.records.get(peer_id).is_some_and(|record| {
            record.view.draft_id == draft_id
                && record.execution.as_ref().is_none_or(|execution| {
                    matches!(
                        execution.snapshot().status,
                        GatewayDraftStatus::Done | GatewayDraftStatus::Failed
                    )
                })
        }) {
            return false;
        }
        self.records.remove(peer_id).is_some_and(|record| {
            if let Some(job) = record.estimation {
                job.abort();
            }
            true
        })
    }

    #[cfg(feature = "hardware")]
    pub(in crate::root) fn refresh_hardware_session(
        &mut self,
        previous: &Arc<DesktopViewSession>,
        refreshed: &Arc<DesktopViewSession>,
        generation: u64,
    ) {
        for record in self.records.values_mut() {
            if record.wallet_generation == generation
                && Arc::ptr_eq(&record.wallet, previous)
                && record.execution.as_ref().is_some_and(|execution| {
                    execution.generation().is_some() || execution.has_review_approval()
                })
            {
                // Only the known hardware-profile refresh may carry running progress, or an
                // approved execution (private or public) that has not bound its generation yet,
                // to its replacement view. Unapproved drafts keep their original authority binding.
                record.wallet = refreshed.clone();
            }
        }
    }

    pub(in crate::root) fn stopped(&self, generation: u64) {
        for execution in self
            .records
            .values()
            .filter_map(|record| record.execution.as_ref())
        {
            if execution.snapshot().private.is_none() && execution.generation() == Some(generation)
            {
                execution.stopped();
            }
        }
    }
    pub(in crate::root) fn reconcile_wallet(
        &mut self,
        wallet: Option<&Arc<DesktopViewSession>>,
        generation: u64,
    ) {
        self.records.retain(|_, record| {
            let keep = wallet.is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
                && generation == record.wallet_generation;
            if !keep {
                retire_record(record);
            }
            keep
        });
    }

    pub(in crate::root) fn retain_peers(
        &mut self,
        peers: &[wallet_ops::gateway::GatewayPeerSummary],
    ) {
        self.records.retain(|id, record| {
            let keep = peers
                .iter()
                .any(|peer| alloy::hex::encode(peer.id.to_bytes()) == *id);
            if !keep {
                retire_record(record);
            }
            keep
        });
    }
    pub(in crate::root) fn retire(&mut self) {
        for (_, record) in self.records.drain() {
            if let Some(job) = record.estimation {
                job.abort();
            }
            if let Some(execution) = record.execution {
                execution.cancel_review();
            }
        }
    }

    pub(in crate::root) fn views(&self, root: &WalletRoot) -> Vec<GatewayDraftView> {
        let mut views: Vec<_> = self
            .records
            .values()
            .filter(|record| {
                root.view_session
                    .as_ref()
                    .is_some_and(|view| Arc::ptr_eq(view, &record.wallet))
                    && root.active_wallet_generation == record.wallet_generation
            })
            .map(|record| {
                let mut view = record.view.clone();
                if let GatewayDraftPayload::Private(input) = &record.view.input {
                    let prepared = match &record.prepared {
                        Some(PreparedDraft::Private(prepared)) => Some(prepared.as_ref()),
                        _ => None,
                    };
                    view.private_options = (record.execution.is_none()).then(|| {
                        root.gateway_private_draft_options(input, prepared, record.picker.as_ref())
                    });
                }
                if record.execution.is_none()
                    && let Some(PreparedDraft::Private(prepared)) = &record.prepared
                    && (!prepared.is_current(root, &record.view.input)
                        || record
                            .estimated_at
                            .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME))
                {
                    view.status = GatewayDraftStatus::Editing;
                    view.message =
                        "Private funds or fee estimate changed. Refresh the estimate.".into();
                }
                if let Some(execution) = &record.execution {
                    let progress = execution.snapshot();
                    view.private_progress.clone_from(&progress.private);
                    view.status = progress.status;
                    view.step_label = progress.step_label;
                    view.message = progress.message;
                    view.warning = progress.warning;
                    view.can_retry = progress.can_retry;
                    view.can_cancel = matches!(
                        view.status,
                        GatewayDraftStatus::Attention | GatewayDraftStatus::InProgress
                    ) && execution.generation().is_none_or(|generation| {
                        if let Some(private) = &view.private_progress {
                            return private.stop || private.stop_waiting;
                        }
                        generation == root.public_form.action_generation
                            && root.public_form.action_stop_available
                    });
                }
                if record.execution.is_none()
                    && let Some(PreparedDraft::PrivateSelfBroadcast(prepared)) = &record.prepared
                    && (!prepared.is_current(root, &record.view.input)
                        || (!prepared.retains_background_estimate()
                            && record
                                .estimated_at
                                .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME)))
                {
                    view.status = GatewayDraftStatus::Editing;
                    view.message =
                        "Private funds or self-broadcast estimate changed. Refresh the estimate."
                            .into();
                }
                view
            })
            .collect();
        views.sort_by(|a, b| a.draft_id.cmp(&b.draft_id));
        views
    }
}
