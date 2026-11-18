use super::*;

impl WalletRoot {
    pub(in crate::root) fn clear_private_broadcaster_progress_state(&mut self) {
        self.publish_gateway_private_progress();
        if let Some(execution) = self
            .private_broadcaster_progress
            .as_ref()
            .and_then(|progress| progress.gateway_execution.as_ref())
        {
            execution.retire_private_owner();
        }
        let sponsored_handle =
            cancel_private_broadcaster_progress(&mut self.private_broadcaster_progress);
        if let Some(sponsored_handle) = sponsored_handle {
            self.public_broadcaster_task_abort_handles
                .retain(|handle| handle.id() != sponsored_handle.id());
            spawn_sponsored_abort_watchdog(
                &self.runtime,
                sponsored_handle,
                SPONSORED_SHUTDOWN_ABORT_GRACE,
            );
        }
        cancel_public_broadcaster_tasks(&mut self.public_broadcaster_task_abort_handles);
        self.drop_trezor_pin_matrix_prompt();
    }

    pub(in crate::root) fn set_private_broadcaster_task_abort_handle(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        handle: tokio::task::AbortHandle,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if progress.kind == kind && progress.key == key && progress.generation_id == generation_id {
            self.public_broadcaster_task_abort_handles
                .retain(|handle| !handle.is_finished());
            self.public_broadcaster_task_abort_handles
                .push(handle.clone());
            progress.task_abort_handle = Some(handle);
        }
    }

    /// Start the progress of a public broadcaster submission, and return the sender its
    /// delivery reports the submission's route through.
    pub(in crate::root) fn start_private_broadcaster_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        asset_label: String,
        icon_path: Option<WalletIconSource>,
        recipient: String,
        estimate: Option<PublicBroadcasterCostEstimate>,
        response_timeout: Duration,
        cx: &Context<'_, Self>,
    ) -> SubmissionRouteSender {
        // The route's chip and the re-sends it counts are redrawn as its delivery reports them.
        let (route_tx, mut route) = watch::channel(SubmissionRouteProgress::default());
        let route_rx = route.clone();
        cx.spawn(async move |this, cx| {
            while route.changed().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        let asset_label = Arc::<str>::from(asset_label);
        let dialog_open = self
            .private_broadcaster_progress
            .as_ref()
            .is_some_and(|progress| progress.dialog_open);
        let gateway_execution = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .and_then(|form| form.gateway_execution.clone()),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .and_then(|form| form.gateway_execution.clone()),
        };
        self.private_broadcaster_progress = Some(PrivateBroadcasterProgressState {
            gateway_execution,
            stealth_account: None,
            flow: PrivateSubmissionProgressFlow::PublicBroadcaster,
            kind,
            key,
            generation_id,
            asset_label: Arc::clone(&asset_label),
            icon_path,
            recipient: Arc::from(recipient),
            recipient_output: None,
            gas_payer: None,
            requires_device_approval: false,
            sponsored_funding: false,
            steps: private_broadcaster_progress_steps(),
            estimate,
            result: None,
            self_broadcast_result: None,
            self_broadcast_command_tx: None,
            sponsored_self_broadcast_command_tx: None,
            sponsored_self_broadcast_outcome: None,
            self_broadcast_attempts: Vec::new(),
            self_broadcast_current_gas_fee: None,
            self_broadcast_action_error: None,
            refusal_retry_pending: false,
            refusal_retry_error: None,
            input_pin_conflict: false,
            public_broadcaster_response_timeout: Some(response_timeout),
            public_broadcaster_wait_started_at: None,
            route: Some(route_rx),
            route_isolated: self.http.network_mode() != WalletNetworkMode::Direct,
            task_abort_handle: None,
            payload_inputs_rx: None,
            stop_available: true,
            stopped: false,
            error: None,
            dialog_open,
            stage_seen: false,
        });
        route_tx
    }

    pub(in crate::root) fn start_private_self_broadcast_progress(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        asset_label: String,
        icon_path: Option<WalletIconSource>,
        recipient: String,
        recipient_output: Option<String>,
        gas_payer: String,
        requires_device_approval: bool,
        command_tx: Option<SelfBroadcastCommandSender>,
        current_gas_fee: Option<(u128, u128)>,
    ) {
        let asset_label = Arc::<str>::from(asset_label);
        let dialog_open = self
            .private_broadcaster_progress
            .as_ref()
            .is_some_and(|progress| progress.dialog_open);
        let gateway_execution = match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get(&key)
                .and_then(|form| form.gateway_execution.clone()),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get(&key)
                .and_then(|form| form.gateway_execution.clone()),
        };
        self.private_broadcaster_progress = Some(PrivateBroadcasterProgressState {
            gateway_execution,
            stealth_account: None,
            flow: PrivateSubmissionProgressFlow::SelfBroadcast,
            kind,
            key,
            generation_id,
            asset_label: Arc::clone(&asset_label),
            icon_path,
            recipient: Arc::from(recipient),
            recipient_output: recipient_output.map(Arc::from),
            gas_payer: Some(Arc::from(gas_payer)),
            requires_device_approval,
            sponsored_funding: false,
            steps: self_broadcast_progress_steps(kind),
            estimate: None,
            result: None,
            self_broadcast_result: None,
            self_broadcast_command_tx: command_tx,
            sponsored_self_broadcast_command_tx: None,
            sponsored_self_broadcast_outcome: None,
            self_broadcast_attempts: Vec::new(),
            self_broadcast_current_gas_fee: current_gas_fee,
            self_broadcast_action_error: None,
            refusal_retry_pending: false,
            refusal_retry_error: None,
            input_pin_conflict: false,
            public_broadcaster_response_timeout: None,
            public_broadcaster_wait_started_at: None,
            route: None,
            route_isolated: false,
            task_abort_handle: None,
            payload_inputs_rx: None,
            stop_available: true,
            stopped: false,
            error: None,
            dialog_open,
            stage_seen: false,
        });
    }

    pub(in crate::root) fn show_private_broadcaster_progress_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_mut() else {
            return;
        };
        if progress.dialog_open {
            return;
        }
        progress.dialog_open = true;
        let flow = progress.flow;
        let kind = progress.kind;
        let key = progress.key;
        let generation_id = progress.generation_id;
        let asset_label = Arc::clone(&progress.asset_label);
        let icon_path = progress.icon_path.clone();
        let root = cx.entity();
        let viewport_size = window.viewport_size();
        let dialog_width =
            (viewport_size.width * 0.92).min(PRIVATE_BROADCASTER_PROGRESS_DIALOG_WIDTH);
        let dialog_max_height = viewport_size.height * 0.84;
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            // A public broadcaster submission's route chip ends the title, which truncates
            // before it.
            let route = root
                .read(cx)
                .private_broadcaster_progress
                .as_ref()
                .and_then(private_broadcaster_route_card)
                .map(|card| {
                    render_submission_route_indicator(
                        delivery_element_id(key, kind, "progress-route"),
                        &card,
                        &root,
                        private_broadcaster_root_route_card,
                    )
                });
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(
                    div()
                        .w(content_width)
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            private_action_title_row(
                                private_submission_dialog_title_action(flow, kind),
                                asset_label.as_ref(),
                                icon_path.clone(),
                                None,
                                false,
                            )
                            .flex_1()
                            .min_w_0()
                            .truncate(),
                        )
                        .children(route),
                )
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        let matches_progress = root
                            .private_broadcaster_progress
                            .as_ref()
                            .is_some_and(|progress| {
                                progress.kind == kind
                                    && progress.key == key
                                    && progress.generation_id == generation_id
                            });
                        if !matches_progress {
                            return;
                        }
                        root.apply_private_broadcaster_progress_dialog_close(window, cx, false);
                        cx.notify();
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_private_broadcaster_progress_dialog_content(
                            &content_root,
                            content_width,
                            cx,
                        ),
                )
        });
    }

    pub(in crate::root) fn stop_private_broadcaster_progress(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        let (
            kind,
            key,
            generation_id,
            sponsored_stopping,
            sponsored_watchdog_handle,
            task_abort_handle,
        ) = {
            let Some(progress) = self.private_broadcaster_progress.as_mut() else {
                return;
            };
            if private_broadcaster_progress_footer_action(progress) != ProgressFooterAction::Stop {
                return;
            }
            let kind = progress.kind;
            let key = progress.key;
            let generation_id = progress.generation_id;
            let sponsored_session_active = progress.steps.iter().any(|step| {
                step.status == PublicActionStepStatus::Pending
                    && sponsored_stop_uses_session_command(step.stage)
            });
            let mut task_abort_handle = None;
            let sponsored_stopping = if sponsored_session_active {
                request_sponsored_session_cancel(progress)
            } else {
                progress.sponsored_self_broadcast_command_tx.take();
                task_abort_handle = progress.task_abort_handle.take();
                false
            };
            progress.self_broadcast_command_tx = None;
            progress.self_broadcast_action_error = None;
            progress.stop_available = false;
            progress.error = None;
            if !sponsored_stopping {
                progress.stopped = true;
                mark_private_broadcaster_active_step_stopped(&mut progress.steps);
            }
            let sponsored_watchdog_handle = sponsored_stopping
                .then(|| progress.task_abort_handle.clone())
                .flatten();
            (
                kind,
                key,
                generation_id,
                sponsored_stopping,
                sponsored_watchdog_handle,
                task_abort_handle,
            )
        };
        if !sponsored_stopping {
            // The broadcaster may already hold the payload, so pin it before abandoning it.
            self.pin_abandoned_private_broadcaster_payload();
        }
        if let Some(handle) = task_abort_handle {
            handle.abort();
        }
        self.clear_trezor_pin_matrix_prompt(cx);

        if sponsored_stopping {
            if let Some(handle) = sponsored_watchdog_handle {
                spawn_sponsored_abort_watchdog(&self.runtime, handle, SPONSORED_CANCEL_ABORT_GRACE);
            }
            cx.notify();
            return;
        }

        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && form.generation_id == generation_id
                {
                    form.generating = false;
                    form.error = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && form.generation_id == generation_id
                {
                    form.generating = false;
                    form.error = None;
                }
            }
        }
        cx.notify();
    }

    pub(super) fn discard_private_submission_attempt(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let matches_progress = self
            .private_broadcaster_progress
            .as_ref()
            .is_some_and(|progress| {
                progress.kind == kind
                    && progress.key == key
                    && progress.generation_id == generation_id
                    && progress
                        .steps
                        .iter()
                        .any(|step| matches!(step.status, PublicActionStepStatus::Error))
            });
        if !matches_progress {
            return;
        }
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && form.generation_id == generation_id
                {
                    form.generating = false;
                    form.error = None;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && form.generation_id == generation_id
                {
                    form.generating = false;
                    form.error = None;
                }
            }
        }
        self.clear_private_broadcaster_progress_state();
        cx.notify();
    }

    pub(super) fn close_private_broadcaster_progress_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.apply_private_broadcaster_progress_dialog_close(window, cx, true);
        cx.notify();
    }

    fn apply_private_broadcaster_progress_dialog_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        close_top_dialog: bool,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            if close_top_dialog {
                window.close_dialog(cx);
            }
            return;
        };
        let kind = progress.kind;
        let key = progress.key;
        let behavior = private_broadcaster_progress_dialog_close_behavior(progress);
        self.publish_gateway_private_progress();
        match behavior {
            ProgressDialogCloseBehavior::AllAndClear => {
                match kind {
                    DeliveryFormKind::Send => self.close_send_form(key, cx),
                    DeliveryFormKind::Unshield => self.close_unshield_form(key, cx),
                }
                window.close_all_dialogs(cx);
            }
            ProgressDialogCloseBehavior::TopAndClear => {
                self.clear_private_broadcaster_progress_state();
                if close_top_dialog {
                    window.close_dialog(cx);
                }
            }
            ProgressDialogCloseBehavior::TopOnly => {
                self.clear_trezor_pin_matrix_prompt(cx);
                if let Some(progress) = self.private_broadcaster_progress.as_mut() {
                    progress.dialog_open = false;
                }
                if close_top_dialog {
                    window.close_dialog(cx);
                }
            }
        }
    }
}
