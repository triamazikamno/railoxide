use super::*;

impl WalletRoot {
    pub(super) const fn public_action_has_active_progress(&self) -> bool {
        !self.public_form.action_progress.is_empty()
    }

    /// Every public action shares one progress slot, so any running action blocks the others.
    pub(in crate::root) const fn public_action_in_flight(&self) -> bool {
        self.public_form.sending || self.public_form.shielding
    }

    pub(in crate::root) fn clear_public_action_progress_state(&mut self) {
        if let Some(handle) = self.public_form.action_task_abort_handle.take() {
            handle.abort();
        }
        self.drop_trezor_pin_matrix_prompt();
        self.public_form.action_progress.clear();
        self.public_form.action_progress_lifecycle = PublicActionProgressLifecycle::Clear;
        self.public_form.expanded_action_error_steps.clear();
        self.public_form.action_progress_dialog_open = false;
        self.public_form.action_requires_device_approval = false;
        self.public_form.action_progress_asset_label = Arc::from("");
        self.public_form.action_progress_icon_path = None;
        self.public_form.action_stop_available = false;
        self.public_form.action_stopped = false;
        self.public_form.action_command_tx = None;
        self.public_form.action_attempts.clear();
        self.public_form.action_current_gas_fee = None;
        self.public_form.action_fee_authorization_review = None;
        self.public_form.action_fees_authorized = false;
        self.public_form.action_action_error = None;
        self.public_form.action_error_summary = None;
        self.public_form.action_contract_address = None;
        self.public_form.action_progress_mode = PublicActionMode::Shield;
        self.public_form.action_progress_title_override = None;
    }

    pub(in crate::root) const fn replace_public_action_dialog_for_confirmed_history(
        &mut self,
        confirmed_history_remains: bool,
    ) {
        let Some(lifecycle) = public_action_progress_handoff_lifecycle(confirmed_history_remains)
        else {
            return;
        };
        self.public_form.action_progress_dialog_open = false;
        self.public_form.action_progress_lifecycle = lifecycle;
    }

    pub(in crate::root) fn start_public_action_progress(
        &mut self,
        mode: PublicActionMode,
        asset: PublicAssetId,
        asset_label: String,
        icon_path: Option<WalletIconSource>,
        public_account_source: PublicAccountSource,
        command_tx: Option<PublicActionCommandSender>,
        initial_gas_fee: Option<(u128, u128)>,
        fees_authorized: bool,
    ) -> u64 {
        let steps =
            public_action_progress_steps_for_source(mode, asset, public_account_source, false);
        self.start_public_action_progress_with_steps(
            mode,
            steps,
            asset_label,
            icon_path,
            public_account_source,
            command_tx,
            initial_gas_fee,
            fees_authorized,
        )
    }

    pub(in crate::root) fn start_public_action_progress_with_steps(
        &mut self,
        mode: PublicActionMode,
        steps: Vec<PublicActionProgressStep>,
        asset_label: String,
        icon_path: Option<WalletIconSource>,
        public_account_source: PublicAccountSource,
        command_tx: Option<PublicActionCommandSender>,
        initial_gas_fee: Option<(u128, u128)>,
        fees_authorized: bool,
    ) -> u64 {
        let states = steps
            .into_iter()
            .map(|step| PublicActionStepState {
                step,
                status: PublicActionStepStatus::NotStarted,
                tx_hash: None,
                message: None,
                interval: None,
            })
            .collect();
        self.start_public_action_progress_with_states(
            mode,
            states,
            asset_label,
            icon_path,
            public_account_source,
            command_tx,
            initial_gas_fee,
            fees_authorized,
        )
    }

    pub(in crate::root) fn start_public_action_progress_with_states(
        &mut self,
        mode: PublicActionMode,
        mut states: Vec<PublicActionStepState>,
        asset_label: String,
        icon_path: Option<WalletIconSource>,
        public_account_source: PublicAccountSource,
        command_tx: Option<PublicActionCommandSender>,
        initial_gas_fee: Option<(u128, u128)>,
        fees_authorized: bool,
    ) -> u64 {
        self.public_form.action_generation = self.public_form.action_generation.wrapping_add(1);
        self.public_form.action_progress_mode = mode;
        let generation = self.public_form.action_generation;
        self.public_form.action_progress_lifecycle = PublicActionProgressLifecycle::Active;
        self.public_form.expanded_action_error_steps.clear();
        self.public_form.action_progress_asset_label = Arc::from(asset_label);
        self.public_form.action_progress_icon_path = icon_path;
        self.public_form.action_progress_dialog_open = false;
        self.public_form.action_requires_device_approval =
            public_account_source == PublicAccountSource::HardwareDerived;
        self.public_form.action_task_abort_handle = None;
        self.public_form.action_stop_available = true;
        self.public_form.action_stopped = false;
        self.public_form.action_command_tx = command_tx;
        self.public_form.action_attempts.clear();
        self.public_form.action_current_gas_fee = initial_gas_fee;
        self.public_form.action_fee_authorization_review = None;
        self.public_form.action_fees_authorized = fees_authorized;
        self.public_form.action_action_error = None;
        self.public_form.action_error_summary = None;
        self.public_form.action_contract_address = None;
        self.public_form.action_progress_title_override = None;
        if states
            .iter()
            .all(|step| step.status == PublicActionStepStatus::NotStarted)
            && let Some(first) = states.first_mut()
        {
            first.status = PublicActionStepStatus::Pending;
        }
        self.public_form.action_progress = states;
        generation
    }

    pub(in crate::root) fn apply_public_action_progress_update(
        &mut self,
        generation: u64,
        update: PublicActionProgressUpdate,
        cx: &mut Context<'_, Self>,
    ) {
        if !public_action_accepts_update(
            self.public_form.action_generation,
            generation,
            self.public_form.action_stopped,
        ) {
            return;
        }
        let Some(step) = self
            .public_form
            .action_progress
            .iter_mut()
            .find(|step| step.step == update.step)
        else {
            return;
        };
        step.status = match update.status {
            PublicActionProgressStatus::Pending => PublicActionStepStatus::Pending,
            PublicActionProgressStatus::Done => PublicActionStepStatus::Done,
            PublicActionProgressStatus::Error => PublicActionStepStatus::Error,
        };
        if let Some(tx_hash) = update.tx_hash {
            step.tx_hash = Some(Arc::from(tx_hash));
        }
        if let Some(message) = update.message {
            step.message = Some(Arc::from(message));
        } else if update.status != PublicActionProgressStatus::Error {
            step.message = None;
        }
        cx.notify();
    }

    pub(in crate::root) fn fail_public_action_progress(
        &mut self,
        generation: u64,
        message: String,
        cx: &mut Context<'_, Self>,
    ) {
        if !public_action_accepts_update(
            self.public_form.action_generation,
            generation,
            self.public_form.action_stopped,
        ) {
            return;
        }
        if let Some(step) = self
            .public_form
            .action_progress
            .iter_mut()
            .find(|step| step.status == PublicActionStepStatus::Error)
        {
            let replace_message = match step.message.as_ref() {
                Some(existing) => message.len() > existing.len(),
                None => true,
            };
            if replace_message {
                step.message = Some(Arc::from(message));
            }
            cx.notify();
            return;
        }
        let step_index = self
            .public_form
            .action_progress
            .iter()
            .position(|step| step.status == PublicActionStepStatus::Pending)
            .or_else(|| {
                self.public_form
                    .action_progress
                    .iter()
                    .position(|step| step.status == PublicActionStepStatus::NotStarted)
            })
            .or_else(|| self.public_form.action_progress.len().checked_sub(1));
        if let Some(step_index) = step_index {
            let step = &mut self.public_form.action_progress[step_index];
            step.status = PublicActionStepStatus::Error;
            step.message = Some(Arc::from(message));
            self.public_form.action_command_tx = None;
            self.public_form.action_action_error = None;
            cx.notify();
        }
    }

    fn spawn_public_action_progress_listener(
        generation: u64,
        chain_id: u64,
        active_wallet_id: Option<Arc<str>>,
        mut progress_rx: mpsc::UnboundedReceiver<PublicActionProgressUpdate>,
        gateway_execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(update) = progress_rx.recv().await {
                if let Some(execution) = &gateway_execution {
                    execution.progress(&update);
                }
                let _ = this.update(cx, |root, cx| {
                    // A Tron Send's updates follow it to its outcome on any EVM chain.
                    if root.selected_wallet_id != active_wallet_id
                        || (root.last_evm_chain != chain_id
                            && root.tron_public.send.generation != Some(generation))
                    {
                        return;
                    }
                    root.apply_public_action_progress_update(generation, update, cx);
                });
            }
        })
        .detach();
    }

    fn spawn_public_action_session_event_listener(
        generation: u64,
        chain_id: u64,
        active_wallet_id: Option<Arc<str>>,
        mut event_rx: mpsc::UnboundedReceiver<PublicActionSessionEvent>,
        gateway_execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                if let Some(execution) = &gateway_execution {
                    execution.event(&event);
                }
                let _ = this.update(cx, |root, cx| {
                    if root.selected_wallet_id != active_wallet_id
                        || (root.last_evm_chain != chain_id
                            && root.tron_public.send.generation != Some(generation))
                    {
                        return;
                    }
                    root.apply_public_action_session_event(generation, event, cx);
                });
            }
        })
        .detach();
    }

    /// Routes a started action's progress and session events to the shared slot and opens
    /// its progress dialog.
    pub(in crate::root) fn follow_public_action_progress(
        generation: u64,
        chain_id: u64,
        active_wallet_id: Option<Arc<str>>,
        event_rx: mpsc::UnboundedReceiver<PublicActionSessionEvent>,
        gateway_execution: Option<wallet_ops::gateway::GatewayDraftExecution>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) -> mpsc::UnboundedSender<PublicActionProgressUpdate> {
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        Self::spawn_public_action_progress_listener(
            generation,
            chain_id,
            active_wallet_id.clone(),
            progress_rx,
            gateway_execution.clone(),
            cx,
        );
        Self::spawn_public_action_session_event_listener(
            generation,
            chain_id,
            active_wallet_id,
            event_rx,
            gateway_execution,
            cx,
        );
        Self::show_public_action_progress_dialog_after_close(window, cx);
        progress_tx
    }

    pub(in crate::root) fn apply_public_action_session_event(
        &mut self,
        generation: u64,
        event: PublicActionSessionEvent,
        cx: &mut Context<'_, Self>,
    ) {
        if !public_action_accepts_update(
            self.public_form.action_generation,
            generation,
            self.public_form.action_stopped,
        ) {
            return;
        }
        match event {
            PublicActionSessionEvent::StepFailed { step, message } => {
                self.discard_active_trezor_session_if_stale(&message, cx);
                self.public_form.action_action_error = None;
                if let Some(progress_step) = self
                    .public_form
                    .action_progress
                    .iter_mut()
                    .find(|progress_step| progress_step.step == step)
                {
                    progress_step.status = PublicActionStepStatus::Error;
                    progress_step.message = Some(Arc::from(message));
                }
            }
            PublicActionSessionEvent::AttemptHandoff { step } => {
                if public_action_step_is_final_handoff_for_steps(
                    self.public_form.action_progress_mode,
                    step,
                    &self.public_form.action_progress,
                ) {
                    self.public_form.action_stop_available = false;
                }
            }
            PublicActionSessionEvent::AttemptSubmitted { step, attempt } => {
                if public_action_step_is_final_handoff_for_steps(
                    self.public_form.action_progress_mode,
                    step,
                    &self.public_form.action_progress,
                ) {
                    self.public_form.action_stop_available = false;
                }
                self.public_form.action_current_gas_fee =
                    Some((attempt.max_fee_per_gas, attempt.max_priority_fee_per_gas));
                self.public_form.action_action_error = None;
                self.public_form.action_attempts.push(attempt);
            }
            PublicActionSessionEvent::AttemptRejected { message, .. }
            | PublicActionSessionEvent::HardwareApprovalFailed { message } => {
                self.discard_active_trezor_session_if_stale(&message, cx);
                self.public_form.action_action_error = Some(Arc::from(message));
            }
            PublicActionSessionEvent::FeeAuthorizationRequired {
                step,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                message,
            } => {
                self.public_form.action_fee_authorization_review =
                    Some(public_action_fee_authorization_review(
                        step,
                        max_fee_per_gas,
                        max_priority_fee_per_gas,
                        message.clone(),
                    ));
                self.public_form.action_action_error = None;
                if let Some(progress_step) = self
                    .public_form
                    .action_progress
                    .iter_mut()
                    .find(|progress_step| progress_step.step == step)
                {
                    progress_step.status = PublicActionStepStatus::Pending;
                    progress_step.message = Some(Arc::from(message));
                }
            }
            PublicActionSessionEvent::HardwareApprovalStarted
            | PublicActionSessionEvent::HardwareApprovalCompleted => {}
            PublicActionSessionEvent::HardwareProfileSessionRefreshed { session } => {
                #[cfg(feature = "hardware")]
                self.refresh_active_hardware_profile_session(session, cx);
                #[cfg(not(feature = "hardware"))]
                let _ = session;
            }
        }
        cx.notify();
    }

    pub(in crate::root) fn show_public_action_progress_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.action_progress_dialog_open {
            return;
        }
        self.public_form.action_progress_dialog_open = true;
        self.public_form.action_progress_lifecycle = PublicActionProgressLifecycle::Active;
        let generation = self.public_form.action_generation;
        let mode = self.public_form.action_progress_mode;
        let asset_label = Arc::clone(&self.public_form.action_progress_asset_label);
        let icon_path = self.public_form.action_progress_icon_path.clone();
        let title = self
            .public_form
            .action_progress_title_override
            .clone()
            .map_or_else(
                || format!("{} {}", public_action_mode_verb(mode), asset_label.as_ref()),
                |title| title.to_string(),
            );
        let root = cx.entity();
        let viewport_size = window.viewport_size();
        let dialog_width = (viewport_size.width * 0.92).min(PUBLIC_ACTION_DIALOG_WIDTH);
        let dialog_max_height = viewport_size.height * 0.84;
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(public_action_title_row(title.clone(), icon_path.clone()))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        if root.public_form.action_generation == generation {
                            root.apply_public_action_progress_dialog_close(window, cx, false);
                            cx.notify();
                        }
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_public_action_progress_dialog_content(&content_root, content_width),
                )
        });
    }

    pub(in crate::root) fn show_public_action_progress_dialog_after_close(
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        cx.defer_in(window, |root, window, cx| {
            root.show_public_action_progress_dialog(window, cx);
        });
    }

    pub(in crate::root) fn stop_public_action_progress(&mut self, cx: &mut Context<'_, Self>) {
        if public_action_progress_footer_action(
            self.public_form.action_stop_available,
            self.public_form.action_command_tx.is_some(),
            &self.public_form.action_progress,
        ) != ProgressFooterAction::Stop
        {
            return;
        }
        // A Tron Send's handoff may win before its event arrives; then only its outcome ends it.
        if self.claim_tron_send_stop_unless_handed_off() {
            return;
        }
        if let Some(handle) = self.public_form.action_task_abort_handle.take() {
            handle.abort();
        }
        self.clear_trezor_pin_matrix_prompt(cx);
        self.public_form.action_command_tx = None;
        self.public_form.action_action_error = None;
        self.public_form.action_fee_authorization_review = None;
        self.public_form.action_stop_available = false;
        self.public_form.action_stopped = true;
        self.gateway
            .drafts
            .borrow()
            .stopped(self.public_form.action_generation);
        match self.public_form.action_progress_mode {
            PublicActionMode::Shield => self.public_form.shielding = false,
            PublicActionMode::Send | PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                self.public_form.sending = false;
            }
        }
        mark_public_action_active_step_stopped(&mut self.public_form.action_progress);
        // Submission consumed the native preparation and stopping bypasses the completion
        // handler that would restore it.
        self.refresh_native_shield_preparation(cx);
        cx.notify();
    }

    pub(in crate::root) fn discard_public_action_attempt(&mut self, cx: &mut Context<'_, Self>) {
        if self.public_form.action_command_tx.is_none() {
            return;
        }
        self.public_form.action_generation = self.public_form.action_generation.wrapping_add(1);
        match self.public_form.action_progress_mode {
            PublicActionMode::Shield => self.public_form.shielding = false,
            PublicActionMode::Send | PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                self.public_form.sending = false;
            }
        }
        self.clear_public_action_progress_state();
        // Submission consumed the native preparation and discarding bypasses the completion
        // handler that would restore it.
        self.refresh_native_shield_preparation(cx);
        cx.notify();
    }

    pub(in crate::root) fn close_public_action_progress_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.apply_public_action_progress_dialog_close(window, cx, true);
        cx.notify();
    }

    fn apply_public_action_progress_dialog_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        close_top_dialog: bool,
    ) {
        if self.public_form.action_progress_lifecycle
            == PublicActionProgressLifecycle::DialogReplacedWhileConfirmedHistoryRemains
        {
            self.clear_trezor_pin_matrix_prompt(cx);
            self.public_form.action_progress_dialog_open = false;
            return;
        }
        match progress_dialog_close_behavior(
            public_action_progress_is_successful(&self.public_form.action_progress),
            self.public_form.action_stopped,
        ) {
            ProgressDialogCloseBehavior::AllAndClear => {
                self.public_form.sending = false;
                self.public_form.shielding = false;
                self.clear_public_action_dialog_inputs(window, cx);
                self.clear_public_action_progress_state();
                window.close_all_dialogs(cx);
            }
            ProgressDialogCloseBehavior::TopAndClear => {
                self.clear_public_action_progress_state();
                if close_top_dialog {
                    window.close_dialog(cx);
                }
            }
            ProgressDialogCloseBehavior::TopOnly => {
                self.clear_trezor_pin_matrix_prompt(cx);
                self.public_form.action_progress_dialog_open = false;
                if close_top_dialog {
                    window.close_dialog(cx);
                }
            }
        }
    }

    pub(in crate::root) fn render_public_action_progress_dialog_content(
        &self,
        root: &Entity<Self>,
        content_width: Pixels,
    ) -> gpui::Div {
        if self.public_form.action_progress.is_empty() {
            return div()
                .w(content_width)
                .child(app_muted_text("No active Public action."));
        }
        let mut content = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .child(render_public_action_stepper(
                root,
                &self.public_form.action_progress,
                &self.public_form.expanded_action_error_steps,
                self.public_form.action_progress_asset_label.as_ref(),
                self.public_form.action_requires_device_approval,
                self.public_form.action_command_tx.is_some(),
                self.public_form.action_current_gas_fee,
                self.public_form.action_fees_authorized,
                self.public_form.action_action_error.as_deref(),
                self.public_form.action_fee_authorization_review.as_ref(),
                self.public_form.action_error_summary.as_deref(),
                self.tron_send_pending_detail(),
                self.public_form.action_generation,
            ))
            .children(self.tron_send_progress_note());

        if let Some(contract_address) = self.public_form.action_contract_address.as_ref() {
            content = content.child(copyable_mono_field(
                "Created contract",
                contract_address.to_string(),
                "wallet-public-action-created-contract-copy",
            ));
        }

        if let Some((max_fee, max_tip)) = self.public_form.action_current_gas_fee {
            content = content.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p(px(12.0))
                    .rounded_md()
                    .bg(rgb(theme::SURFACE_ELEVATED))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .child(app_strong_text("Gas fee"))
                    .child(public_action_context_row(
                        "Max fee",
                        format!("{} gwei", format_gwei(max_fee)),
                    ))
                    .child(public_action_context_row(
                        "Max tip",
                        format!("{} gwei", format_gwei(max_tip)),
                    )),
            );
        }
        #[cfg(feature = "hardware")]
        if let Some(prompt) = self
            .hardware_profile_unlock
            .trezor_pin_matrix_prompt
            .as_ref()
        {
            content = content.child(crate::root::vault_ui::render_trezor_pin_matrix_prompt(
                root, prompt,
            ));
        }
        content = content.child(render_public_action_progress_footer(
            root.clone(),
            public_action_progress_footer_action(
                self.public_form.action_stop_available,
                self.public_form.action_command_tx.is_some(),
                &self.public_form.action_progress,
            ),
        ));
        content
    }
}
