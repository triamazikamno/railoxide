use super::*;

impl WalletRoot {
    pub(in crate::root::gateway_drafts) fn gateway_private_form(
        &self,
        execution: &GatewayDraftExecution,
    ) -> Option<(DeliveryFormKind, crate::root::UnshieldAssetKey)> {
        self.send_forms
            .iter()
            .find(|(_, form)| {
                form.gateway_execution
                    .as_ref()
                    .is_some_and(|owner| owner.same_execution(execution))
            })
            .map(|(key, _)| (DeliveryFormKind::Send, *key))
            .or_else(|| {
                self.unshield_forms
                    .iter()
                    .find(|(_, form)| {
                        form.gateway_execution
                            .as_ref()
                            .is_some_and(|owner| owner.same_execution(execution))
                    })
                    .map(|(key, _)| (DeliveryFormKind::Unshield, *key))
            })
    }

    pub(in crate::root) fn release_gateway_private_form(
        &mut self,
        execution: &GatewayDraftExecution,
        cx: &mut Context<'_, Self>,
    ) {
        if execution.generation().is_some()
            || execution.snapshot().status != GatewayDraftStatus::Failed
        {
            return;
        }
        match self.gateway_private_form(execution) {
            Some((DeliveryFormKind::Send, key)) => self.close_send_form(key, cx),
            Some((DeliveryFormKind::Unshield, key)) => self.close_unshield_form(key, cx),
            None => {}
        }
    }

    pub(in crate::root) fn reject_gateway_private_authorization(
        &mut self,
        execution: &GatewayDraftExecution,
        cx: &mut Context<'_, Self>,
    ) {
        if execution.generation().is_some() {
            return;
        }
        let error = self
            .gateway_private_form(execution)
            .and_then(|(kind, key)| match kind {
                DeliveryFormKind::Send => self.send_forms.get(&key)?.error.as_deref(),
                DeliveryFormKind::Unshield => self.unshield_forms.get(&key)?.error.as_deref(),
            });
        execution.reject_private_review(
            error
                .unwrap_or("Transaction inputs changed. Update the draft and submit again.")
                .to_owned(),
        );
        self.release_gateway_private_form(execution, cx);
    }

    pub(in crate::root::gateway_drafts) fn submit_gateway_private_draft(
        &mut self,
        peer_id: &str,
        draft_id: &str,
        revision: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        use crate::root::{DeliveryMode, UnshieldAssetKey};
        let mut book = self.gateway.drafts.borrow_mut();
        let Some(record) = book.records.get_mut(peer_id).filter(|record| {
            record.view.draft_id == draft_id
                && record.view.revision == revision
                && record.execution.is_none()
                && record.view.status == GatewayDraftStatus::Ready
                && record.wallet_generation == self.active_wallet_generation
                && self
                    .view_session
                    .as_ref()
                    .is_some_and(|wallet| Arc::ptr_eq(wallet, &record.wallet))
        }) else {
            return;
        };
        if window.has_active_dialog(cx)
            || !self.send_forms.is_empty()
            || !self.unshield_forms.is_empty()
            || self.private_broadcaster_progress.is_some()
            || self.public_form.sending
            || self.public_form.shielding
        {
            record.view.message =
                "Finish or close the current action in the desktop app first.".into();
            return;
        }
        if let Some(PreparedDraft::PrivateSelfBroadcast(prepared)) = &record.prepared {
            if !prepared.is_current(self, &record.view.input)
                || (!prepared.retains_background_estimate()
                    && record
                        .estimated_at
                        .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME))
            {
                record.view.status = GatewayDraftStatus::Editing;
                drop(book);
                self.estimate_gateway_private_draft(peer_id, cx);
                return;
            }
            let prepared = prepared.clone();
            let estimated_at = record.estimated_at;
            let execution =
                GatewayDraftExecution::private(alloy::hex::encode(rand::random::<[u8; 16]>()))
                    .with_origin(self.gateway_peer_name(peer_id));
            record.execution = Some(execution.clone());
            record.picker = None;
            drop(book);
            self.begin_gateway_self_broadcast(*prepared, &execution, estimated_at, window, cx);
            return;
        }
        let Some(PreparedDraft::Private(prepared)) = &record.prepared else {
            return;
        };
        if !prepared.is_current(self, &record.view.input)
            || record
                .estimated_at
                .is_none_or(|at| at.elapsed() > ESTIMATE_LIFETIME)
        {
            record.view.status = GatewayDraftStatus::Editing;
            drop(book);
            self.estimate_gateway_private_draft(peer_id, cx);
            return;
        }
        let Some(PreparedDraft::Private(prepared)) = record.prepared.take() else {
            return;
        };
        let estimated_at = record.estimated_at;
        let execution =
            GatewayDraftExecution::private(alloy::hex::encode(rand::random::<[u8; 16]>()))
                .with_origin(self.gateway_peer_name(peer_id));
        record.execution = Some(execution.clone());
        record.picker = None;
        drop(book);
        let input = prepared.input;
        let key = UnshieldAssetKey::from_asset(&input.asset);
        let kind = match &input.output {
            PrivateEstimateOutput::Send => DeliveryFormKind::Send,
            PrivateEstimateOutput::Unshield { .. } => DeliveryFormKind::Unshield,
        };
        window.activate_window();
        match &input.output {
            PrivateEstimateOutput::Send => {
                self.initialize_send_form(input.asset, window, cx);
                let form = self
                    .send_forms
                    .get_mut(&key)
                    .expect("new private submission state");
                form.gateway_execution = Some(execution.clone());
                form.delivery_mode = DeliveryMode::PublicBroadcaster;
                form.selected_fee_token = input.fee_token;
                form.custom_fee_amount = None;
                form.broadcaster_choice = input.broadcaster;
                form.fee_mode = input.fee_mode;
                form.allow_suspicious_broadcasters = input.allow_out_of_range;
                form.favorites_only_broadcasters = input.favorites_only;
            }
            PrivateEstimateOutput::Unshield {
                unwrap,
                native_top_up,
            } => {
                self.initialize_unshield_form(input.asset, window, cx);
                let form = self
                    .unshield_forms
                    .get_mut(&key)
                    .expect("new private submission state");
                form.gateway_execution = Some(execution.clone());
                form.delivery_mode = DeliveryMode::PublicBroadcaster;
                form.selected_fee_token = input.fee_token;
                form.custom_fee_amount = None;
                form.broadcaster_choice = input.broadcaster;
                form.fee_mode = input.fee_mode;
                form.allow_suspicious_broadcasters = input.allow_out_of_range;
                form.favorites_only_broadcasters = input.favorites_only;
                form.unwrap = *unwrap;
                form.native_top_up_enabled = native_top_up.is_some();
                form.native_top_up.clone_from(native_top_up);
            }
        }
        self.set_private_action_recipient(kind, key, &input.recipient, window, cx);
        self.set_programmatic_amount_input(kind, key, prepared.estimate.entered_amount, window, cx);
        match kind {
            DeliveryFormKind::Send => {
                let form = self
                    .send_forms
                    .get_mut(&key)
                    .expect("private submission state");
                if form.broadcaster_choice == BroadcasterChoice::Random {
                    form.shown_broadcaster =
                        Some(prepared.estimate.broadcaster.railgun_address.clone());
                }
                form.latest_estimate = Some(prepared.estimate.clone());
                form.cost_estimate = Some(prepared.estimate);
                form.gateway_estimated_at = estimated_at;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.estimate_id = 0;
                self.generate_send_calldata_from_form(key, window, cx);
            }
            DeliveryFormKind::Unshield => {
                let form = self
                    .unshield_forms
                    .get_mut(&key)
                    .expect("private submission state");
                if form.broadcaster_choice == BroadcasterChoice::Random {
                    form.shown_broadcaster =
                        Some(prepared.estimate.broadcaster.railgun_address.clone());
                }
                form.latest_estimate = Some(prepared.estimate.clone());
                form.cost_estimate = Some(prepared.estimate);
                form.gateway_estimated_at = estimated_at;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                form.estimate_id = 0;
                self.generate_unshield_calldata_from_form(key, window, cx);
            }
        }
        if !window.has_active_dialog(cx) {
            self.reject_gateway_private_authorization(&execution, cx);
        }
        self.watch_gateway_drafts(cx);
    }
}
