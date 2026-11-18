use super::*;

impl WalletRoot {
    pub(in crate::root) fn begin_blocked_shield_refund(
        &mut self,
        row: &UtxoDisplayRow,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(utxo_id) = row.utxo_id else {
            return;
        };
        if self.blocked_shield_refunds_in_flight.contains(&utxo_id) {
            return;
        }
        let Some(rescue) = row.blocked_shield_rescue.as_ref() else {
            return;
        };
        if !rescue.eligible {
            if can_start_blocked_shield_origin_resolution(row, rescue) {
                // The dialog opens inside the click and shows the lookup until it answers.
                self.resolve_blocked_shield_refund_authorization(utxo_id, cx);
                Self::open_blocked_shield_refund_dialog(utxo_id, window, cx);
            }
            return;
        }

        self.open_blocked_shield_refund_authorization(utxo_id, row, rescue, window, cx);
    }

    fn open_blocked_shield_refund_authorization(
        &mut self,
        utxo_id: BlockedShieldRescueUtxoId,
        row: &UtxoDisplayRow,
        rescue: &BlockedShieldRescueInfo,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(origin_address) = rescue.origin_address.clone() else {
            return;
        };
        let intent = self.blocked_shield_refund_authorization_intent(utxo_id);
        let device_only = self.selected_wallet_source().is_hardware_derived()
            && matches!(intent, SpendAuthorizationIntent::BlockedShieldRefund(_));
        let summary =
            blocked_shield_refund_authorization_summary(row, rescue, &origin_address, device_only);
        self.request_spend_authorization(intent, summary, window, cx);
    }

    /// The gas approval for a refund whose matched origin is a stealth account in Public.
    /// A hardware wallet derives that account's signer on the device, so no vault password
    /// can stand in for it.
    pub(in crate::root) fn blocked_shield_refund_executor_gas_payment(
        &self,
        utxo_id: BlockedShieldRescueUtxoId,
    ) -> Option<wallet_ops::HardwareExecutorAction> {
        let rescue = self
            .blocked_shield_rescue_rows
            .get(&utxo_id)
            .map(BlockedShieldRescueRowState::info)
            .filter(|rescue| rescue.eligible)?;
        let account =
            self.selected_self_broadcast_gas_payer_account(rescue.public_account_uuid.as_deref())?;
        let wallet_ops::vault::PublicAccountSource::ExecutorDerived(source) = account.source else {
            return None;
        };
        Some(wallet_ops::HardwareExecutorAction::GasPayment {
            account: account.public_account_uuid.clone(),
            operation: source.operation(),
        })
    }

    pub(in crate::root) fn blocked_shield_refund_authorization_intent(
        &self,
        utxo_id: BlockedShieldRescueUtxoId,
    ) -> SpendAuthorizationIntent {
        if self.selected_wallet_source().is_hardware_derived()
            && self
                .blocked_shield_refund_executor_gas_payment(utxo_id)
                .is_none()
        {
            SpendAuthorizationIntent::BlockedShieldRefundGasPassword(utxo_id)
        } else {
            SpendAuthorizationIntent::BlockedShieldRefund(utxo_id)
        }
    }

    pub(in crate::root) fn request_blocked_shield_refund_hardware_authorization(
        &mut self,
        utxo_id: BlockedShieldRescueUtxoId,
        vault_password: zeroize::Zeroizing<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(row) = self.active_blocked_shield_rescue_display_row(utxo_id) else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield hardware refund requested without display row");
            return;
        };
        let Some(rescue) = self
            .blocked_shield_rescue_rows
            .get(&utxo_id)
            .map(BlockedShieldRescueRowState::info)
            .filter(|rescue| rescue.eligible)
        else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield hardware refund requested for ineligible UTXO");
            return;
        };
        let Some(origin_address) = rescue.origin_address.clone() else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield hardware refund requested without origin address");
            return;
        };
        let summary =
            blocked_shield_refund_authorization_summary(&row, rescue, &origin_address, false);
        self.open_hardware_spend_authorization_dialog(
            HardwareSpendAuthorizationCompletion::BlockedShieldRefund {
                utxo_id,
                vault_password,
            },
            summary,
            window,
            cx,
        );
    }

    /// Look up the origin of a blocked Shield. The result is stored as the row's state, which
    /// the table and the open refund dialog draw. It opens nothing.
    fn resolve_blocked_shield_refund_authorization(
        &mut self,
        utxo_id: BlockedShieldRescueUtxoId,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .blocked_shield_rescue_rows
            .get(&utxo_id)
            .is_some_and(BlockedShieldRescueRowState::is_resolving)
        {
            return;
        }
        let Some(session) = self.selected_chain_session() else {
            tracing::warn!(target: LOG_TARGET,
                "blocked Shield refund origin resolution requested without selected chain session"
            );
            return;
        };
        let Some(view_session) = self.view_session.clone() else {
            tracing::warn!(target: LOG_TARGET,
                "blocked Shield refund origin resolution requested without unlocked wallet"
            );
            return;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield refund origin resolution requested without vault store");
            return;
        };
        let Ok(effective_chain) = self
            .effective_chain_configs
            .enabled(self.last_evm_chain)
            .cloned()
        else {
            return;
        };
        let lookup_generation = self.next_blocked_shield_rescue_lookup_generation();
        self.blocked_shield_rescue_rows.insert(
            utxo_id,
            BlockedShieldRescueRowState::resolving(lookup_generation),
        );
        self.sync_utxo_table(cx);

        let http = self.http.clone();
        let request = BlockedShieldRescueEligibilityRequest {
            chain_id: self.last_evm_chain,
            effective_chain,
            view_session,
            session,
            vault_store,
            utxo_id,
        };
        let resolve = self.runtime.spawn(async move {
            let client = http.operation_http_client().await?;
            wallet_ops::resolve_blocked_shield_rescue_eligibility(request, &http, &client).await
        });
        cx.spawn(async move |this, cx| {
            let (info, blocker) = match resolve.await {
                Ok(Ok(mut eligibility)) => {
                    let blocker = eligibility.blocker.take();
                    (
                        blocked_shield_rescue_info_from_eligibility(eligibility),
                        blocker,
                    )
                }
                Ok(Err(error)) => (blocked_shield_rescue_error_info(error.to_string()), None),
                Err(error) => (blocked_shield_rescue_error_info(error.to_string()), None),
            };
            let _ = this.update(cx, |root, cx| {
                let accepts_result = root
                    .blocked_shield_rescue_rows
                    .get(&utxo_id)
                    .is_some_and(|state| state.accepts_lookup_result(lookup_generation));
                if !accepts_result {
                    return;
                }
                root.blocked_shield_rescue_rows.insert(
                    utxo_id,
                    BlockedShieldRescueRowState::from_lookup(info, blocker),
                );
                root.sync_utxo_table(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// What the refund dialog of the blocked Shield `utxo_id` shows now. A Shield whose row
    /// state was invalidated or pruned shows its row's own rescue info.
    fn blocked_shield_refund_dialog_state(
        &self,
        utxo_id: BlockedShieldRescueUtxoId,
    ) -> BlockedShieldRefundDialogState {
        // The debug UI fixture answers for its synthesized Shield. No release build has one.
        if let Some(state) = crate::root::private_swap::ui_fixture_refund_dialog_state(utxo_id) {
            return state;
        }
        let Some(state) = self.blocked_shield_rescue_rows.get(&utxo_id) else {
            let info = self
                .active_blocked_shield_rescue_display_row(utxo_id)
                .and_then(|row| row.blocked_shield_rescue)
                .unwrap_or(BlockedShieldRescueInfo {
                    eligible: false,
                    disabled_reason: None,
                    origin_address: None,
                    public_account_uuid: None,
                    public_account_label: None,
                });
            return BlockedShieldRefundDialogState::Unavailable {
                info,
                blocker: None,
            };
        };
        if state.is_resolving() {
            BlockedShieldRefundDialogState::Checking
        } else if state.info.eligible {
            BlockedShieldRefundDialogState::Available(state.info.clone())
        } else {
            BlockedShieldRefundDialogState::Unavailable {
                info: state.info.clone(),
                blocker: state.blocker.clone(),
            }
        }
    }

    /// Open the refund dialog of the blocked Shield `utxo_id`. Its builder reads the
    /// Shield's state on every frame, so the dialog shows the origin check while it runs,
    /// then the refund it found or why there is none.
    pub(in crate::root) fn open_blocked_shield_refund_dialog(
        utxo_id: BlockedShieldRescueUtxoId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        window.open_wallet_dialog(cx, move |dialog, window, cx| {
            // The dialog layer is drawn outside the root's lease, so the root can be read.
            let wallet = root.read(cx);
            let state = wallet.blocked_shield_refund_dialog_state(utxo_id);
            let (title, content, footer) = match state {
                BlockedShieldRefundDialogState::Checking => (
                    "Checking origin",
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Spinner::new()
                                .icon(IconName::LoaderCircle)
                                .color(rgb(theme::TEXT_MUTED).into())
                                .with_size(px(14.0)),
                        )
                        .child(
                            app_text(
                                "Reading the Shield transaction to find the account that made it.",
                            )
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal(),
                        ),
                    blocked_shield_refund_close_button().into_any_element(),
                ),
                BlockedShieldRefundDialogState::Available(info) => {
                    let message = if let Some(label) = &info.public_account_label {
                        format!("The refund goes back to \"{label}\", which pays the gas.")
                    } else {
                        "The refund goes back to the account that made the Shield, which pays \
                         the gas."
                            .to_owned()
                    };
                    (
                        "Refund available",
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(app_text(message).whitespace_normal())
                            .when_some(info.origin_address, |this, address| {
                                this.child(blocked_shield_refund_origin_row(address))
                            }),
                        blocked_shield_refund_available_footer(&root, utxo_id),
                    )
                }
                BlockedShieldRefundDialogState::Unavailable { info, blocker } => {
                    let BlockedShieldRefundGuidance { title, fix, action } =
                        blocked_shield_refund_guidance(
                            blocker.as_ref(),
                            wallet.stealth_session().is_some(),
                            native_wrapped_output_labels(wallet.last_evm_chain)
                                .map(|(native, _)| native),
                        );
                    let message = info
                        .disabled_reason
                        .unwrap_or_else(|| "Blocked Shield refund is unavailable.".to_owned());
                    (
                        title,
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(app_text(message).whitespace_normal())
                            .when_some(fix, |this, fix| {
                                this.child(app_text(fix).whitespace_normal())
                            })
                            .when_some(info.origin_address, |this, address| {
                                this.child(blocked_shield_refund_origin_row(address))
                            }),
                        blocked_shield_refund_unavailable_footer(&root, action, cx),
                    )
                }
            };
            dialog
                .w((window.viewport_size().width * 0.92).min(window.rem_size() * 30.0))
                .max_h(dialog_max_height(window))
                .title(app_strong_text(title))
                .child(content)
                .footer(footer)
        });
    }

    /// Whether `action` of the refund dialog can be followed now. A stealth account can't be
    /// shown while Stealth accounts is busy.
    pub(super) fn can_follow_blocked_shield_refund_action(
        &self,
        action: BlockedShieldRefundAction,
        cx: &App,
    ) -> bool {
        match action {
            BlockedShieldRefundAction::OpenStealthAccounts
            | BlockedShieldRefundAction::OpenPublic => true,
            BlockedShieldRefundAction::ShowStealthAccount(operation) => {
                self.stealth_session().is_some_and(|session| {
                    let target = StealthAccountTarget::new(&session, operation);
                    self.can_open_stealth_account(&target, cx)
                })
            }
        }
    }

    /// Go where `action` of the refund dialog leads. The dialog has closed.
    pub(super) fn follow_blocked_shield_refund_action(
        &mut self,
        action: BlockedShieldRefundAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match action {
            BlockedShieldRefundAction::OpenStealthAccounts => {
                self.select_wallet_tab(WalletTab::Public, cx);
                // Stealth accounts owns focus, rather than the ordinary account search.
                self.focus_public_account_search_on_render = false;
                self.open_stealth_accounts(window, cx);
            }
            BlockedShieldRefundAction::ShowStealthAccount(operation) => {
                if let Some(session) = self.stealth_session() {
                    let target = StealthAccountTarget::new(&session, operation);
                    self.open_stealth_account(&target, window, cx);
                }
            }
            BlockedShieldRefundAction::OpenPublic => {
                self.select_wallet_tab(WalletTab::Public, cx);
                // Put the inactive origin in sight. Stealth accounts may be open in place of
                // the account list, a kept search may filter the origin out, and the list
                // opens on its Active section.
                self.close_stealth_accounts(cx);
                self.public_form
                    .search_input
                    .update(cx, |input, cx| input.replace_all("", window, cx));
                self.set_public_account_section_open(PublicAccountStatus::Inactive, window, cx);
            }
        }
    }

    pub(in crate::root) fn submit_blocked_shield_refund_authorized(
        &mut self,
        utxo_id: BlockedShieldRescueUtxoId,
        spend_authorization: DesktopPrivateSpendAuthorization,
        vault_password: Option<zeroize::Zeroizing<String>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Ok(resolved_chain) = self
            .effective_chain_configs
            .railgun(self.last_evm_chain)
            .cloned()
        else {
            return;
        };
        let protected_seed_session = spend_authorization.protected_seed_session();
        let Some(session) = self.selected_chain_session() else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield refund requested without selected chain session");
            return;
        };
        if self.blocked_shield_refunds_in_flight.contains(&utxo_id) {
            tracing::warn!(target: LOG_TARGET, "duplicate blocked Shield refund request ignored");
            return;
        }
        let Some(view_session) = self.view_session.clone() else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield refund requested without unlocked wallet");
            return;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield refund requested without vault store");
            return;
        };
        let Some(rescue) = self
            .blocked_shield_rescue_rows
            .get(&utxo_id)
            .map(BlockedShieldRescueRowState::info)
            .filter(|rescue| rescue.eligible)
        else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield refund requested for ineligible UTXO");
            return;
        };
        let Some(public_account_uuid) = rescue.public_account_uuid.clone() else {
            tracing::warn!(target: LOG_TARGET, "blocked Shield refund requested without origin public account");
            return;
        };
        let password = if let Some(password) = vault_password {
            Some(password)
        } else {
            match &spend_authorization {
                DesktopPrivateSpendAuthorization::VaultPassword(password)
                | DesktopPrivateSpendAuthorization::ProtectedSoftwareSeed { password, .. } => {
                    Some(password.clone())
                }
                // Only the device approval scoped to the origin account signs its gas.
                DesktopPrivateSpendAuthorization::HardwareExecutor(hardware)
                    if hardware.is_gas_payment_for(&public_account_uuid) =>
                {
                    None
                }
                DesktopPrivateSpendAuthorization::PreauthorizedSigner(_)
                | DesktopPrivateSpendAuthorization::HardwareExecutor(_)
                | DesktopPrivateSpendAuthorization::HardwarePublic => {
                    tracing::warn!(target: LOG_TARGET,
                        "blocked Shield refund self-broadcast requested without gas-payer password"
                    );
                    self.set_vault_error(
                    "Blocked Shield refund self-broadcast requires the vault password for the public gas payer.",
                    cx,
                );
                    return;
                }
            }
        };
        let transaction_tracking = match self
            .public_transaction_tracking_context(self.last_evm_chain, &public_account_uuid)
        {
            Ok(context) => context,
            Err(error) => {
                self.set_vault_error(error, cx);
                return;
            }
        };
        self.blocked_shield_refunds_in_flight.insert(utxo_id);
        self.sync_utxo_table(cx);
        let http = self.http.clone();
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider = view_session
            .hardware_profile_session()
            .filter(|session| session.device_kind == HardwareDeviceKind::Trezor)
            .map(|_| self.trezor_pin_matrix_provider_for_operation(window, cx));
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_provider = None;
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        Self::watch_blocked_shield_refund_events(utxo_id, event_rx, window, cx);
        Self::show_blocked_shield_refund_progress_dialog(utxo_id, window, cx);
        let request = BlockedShieldRescueSelfBroadcastRequest {
            transaction_tracking: Some(transaction_tracking),
            chain_id: self.last_evm_chain,
            effective_chain: resolved_chain,
            view_session,
            session,
            vault_store,
            spend_authorization,
            vault_password: password,
            protected_software_seed_session: protected_seed_session,
            trezor_pin_matrix_provider,
            utxo_id,
            requested_public_account_uuid: Some(public_account_uuid),
            verify_proof: true,
            gas_fee: SelfBroadcastGasFeeSelection::Auto,
            progress_tx: None,
            command_rx: None,
            event_tx: Some(event_tx),
        };
        let submit = self.spawn_public_transaction_submission(request.chain_id, async move {
            let client = http.operation_http_client().await?;
            wallet_ops::submit_blocked_shield_rescue_self_broadcast(request, &http, &client).await
        });
        cx.spawn(async move |this, cx| {
            let result = submit.await;
            let _ = this.update(cx, |root, cx| {
                root.blocked_shield_refunds_in_flight.remove(&utxo_id);
                match result {
                    Ok(Ok(_result)) => {
                        root.blocked_shield_rescue_rows.insert(
                            utxo_id,
                            BlockedShieldRescueRowState::from_info(
                                blocked_shield_rescue_submitted_info(),
                            ),
                        );
                    }
                    Ok(Err(error)) => {
                        let message = error.to_string();
                        if crate::root::spend_authorization::is_spend_authorization_failure_error(
                            &message,
                        ) {
                            root.clear_spend_authorization(cx);
                        }
                        root.discard_active_trezor_session_if_stale(&message, cx);
                        root.blocked_shield_rescue_rows.insert(
                            utxo_id,
                            BlockedShieldRescueRowState::from_info(
                                blocked_shield_rescue_error_info(message.clone()),
                            ),
                        );
                        tracing::warn!(target: LOG_TARGET, %message, "blocked Shield refund submission failed");
                    }
                    Err(error) => {
                        tracing::warn!(target: LOG_TARGET, %error, "blocked Shield refund task failed");
                    }
                }
                root.sync_utxo_table(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn watch_blocked_shield_refund_events(
        utxo_id: BlockedShieldRescueUtxoId,
        mut event_rx: tokio::sync::mpsc::UnboundedReceiver<SelfBroadcastSessionEvent>,
        window: &Window,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                let _ = this.update_in(cx, |root, _window, cx| {
                    if !root.blocked_shield_refunds_in_flight.contains(&utxo_id) {
                        return;
                    }
                    match event {
                        SelfBroadcastSessionEvent::HardwareProfileSessionRefreshed { session } => {
                            #[cfg(feature = "hardware")]
                            root.refresh_active_hardware_profile_session(session, cx);
                            #[cfg(not(feature = "hardware"))]
                            let _ = session;
                        }
                        SelfBroadcastSessionEvent::StepFailed { message, .. }
                        | SelfBroadcastSessionEvent::AttemptRejected { message, .. } => {
                            root.discard_active_trezor_session_if_stale(&message, cx);
                            root.blocked_shield_rescue_rows.insert(
                                utxo_id,
                                BlockedShieldRescueRowState::from_info(
                                    blocked_shield_rescue_error_info(message),
                                ),
                            );
                        }
                        SelfBroadcastSessionEvent::PendingOutputPoiProofsRequired { .. }
                        | SelfBroadcastSessionEvent::AttemptSubmitted(_) => {}
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn show_blocked_shield_refund_progress_dialog(
        utxo_id: BlockedShieldRescueUtxoId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(px(460.0));
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(app_strong_text("Blocked Shield refund"))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.clear_trezor_pin_matrix_prompt(cx);
                    });
                })
                .child(
                    content_root
                        .read(cx)
                        .render_blocked_shield_refund_progress_dialog_content(
                            &content_root,
                            utxo_id,
                            content_width,
                        ),
                )
        });
    }

    fn render_blocked_shield_refund_progress_dialog_content(
        &self,
        root: &Entity<Self>,
        utxo_id: BlockedShieldRescueUtxoId,
        content_width: Pixels,
    ) -> gpui::Div {
        let in_flight = self.blocked_shield_refunds_in_flight.contains(&utxo_id);
        let status = self
            .blocked_shield_rescue_rows
            .get(&utxo_id)
            .and_then(|state| state.info().disabled_reason.as_deref())
            .map_or_else(
                || {
                    if in_flight {
                        "Submitting the blocked Shield refund. Keep this window open for hardware prompts."
                    } else {
                        "No blocked Shield refund is currently in progress."
                    }
                    .to_owned()
                },
                ToOwned::to_owned,
            );
        let content = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .child(app_muted_text(status).whitespace_normal())
            .when(in_flight, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Spinner::new()
                                .icon(IconName::LoaderCircle)
                                .color(rgb(theme::TEXT_MUTED).into())
                                .with_size(px(14.0)),
                        )
                        .child(app_muted_shimmer_text(
                            "Waiting for self-broadcast confirmation...",
                        )),
                )
            });
        #[cfg(feature = "hardware")]
        let mut content = content;
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
        #[cfg(not(feature = "hardware"))]
        let _ = root;
        content
    }
}
