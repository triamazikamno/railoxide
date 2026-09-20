use super::*;

impl WalletRoot {
    pub(in crate::root) fn start_walletconnect_pairing_from_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.pairing_in_progress {
            return;
        }
        let uri = self
            .walletconnect
            .uri_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if uri.is_empty() {
            self.walletconnect.error = Some(Arc::from("Paste a WalletConnect wc: URI first"));
            cx.notify();
            return;
        }
        if self.selected_walletconnect_public_account().is_none() {
            self.walletconnect.error = Some(Arc::from(
                "Select an active Public account before connecting a dapp",
            ));
            cx.notify();
            return;
        }
        let pairing_start = match start_walletconnect_pairing(&uri, current_unix_seconds()) {
            Ok(start) => start,
            Err(error) => {
                self.walletconnect.error = Some(Arc::from(walletconnect_error_message(&error)));
                cx.notify();
                return;
            }
        };
        let context = match self.walletconnect_client_context(cx) {
            Ok(context) => context,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let pairing = pairing_start.uri.clone();
        let steps = pairing_start.relay_steps;
        tracing::info!(
            target: "wallet::root::walletconnect",
            pairing_topic = %walletconnect_topic_log_label(&pairing.topic),
            step_count = steps.len(),
            "starting walletconnect pairing"
        );
        self.walletconnect.pairing_in_progress = true;
        self.walletconnect.error = None;
        self.walletconnect.status = Some(Arc::from("Subscribing pairing topic..."));
        self.walletconnect
            .pending_pairings
            .insert(pairing.topic.clone(), pairing.clone());
        self.walletconnect
            .uri_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let join = self
            .runtime
            .spawn(async move { execute_walletconnect_relay_steps(&context.worker, steps).await });
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |root, cx| {
                root.walletconnect.pairing_in_progress = false;
                match result {
                    Ok(Ok(output)) => {
                        root.walletconnect
                            .subscriptions
                            .extend(output.subscriptions);
                        root.ingest_walletconnect_pairing_messages(&pairing, output.messages);
                        if root.walletconnect.pending_proposal.is_none() {
                            root.walletconnect.status = Some(Arc::from(
                                "Pairing topic subscribed. Waiting for the dapp proposal...",
                            ));
                        }
                        root.ensure_walletconnect_relay_processing(cx);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            target: "wallet::root::walletconnect",
                            error = %error,
                            "walletconnect pairing relay step failed"
                        );
                        root.walletconnect.error = Some(Arc::from(error));
                        root.ensure_walletconnect_relay_processing(cx);
                    }
                    Err(error) => {
                        root.walletconnect.error = Some(Arc::from(format!(
                            "WalletConnect pairing task failed: {error}"
                        )));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn ingest_walletconnect_pairing_messages(
        &mut self,
        pairing: &WalletConnectPairingUri,
        messages: Vec<WalletConnectRelayMessage>,
    ) {
        if walletconnect_pairing_expired(pairing, current_unix_seconds()) {
            self.walletconnect.pending_pairings.remove(&pairing.topic);
            self.walletconnect.status = Some(Arc::from(
                "WalletConnect pairing expired before the dapp proposal arrived.",
            ));
            self.sync_walletconnect_attention();
            return;
        }
        for message in messages
            .into_iter()
            .filter(|message| message.topic == pairing.topic)
        {
            match decode_walletconnect_session_proposal(pairing, &message.message) {
                Ok(proposal) => {
                    tracing::info!(
                        target: "wallet::root::walletconnect",
                        pairing_topic = %walletconnect_topic_log_label(&pairing.topic),
                        proposal_id = proposal.id,
                        dapp = proposal.peer_metadata.name.as_str(),
                        "received walletconnect session proposal"
                    );
                    self.walletconnect.pending_proposal = Some(WalletConnectProposalUi {
                        pairing: pairing.clone(),
                        proposal,
                    });
                    self.walletconnect.status = Some(Arc::from(
                        "Review the WalletConnect session proposal before connecting.",
                    ));
                    self.walletconnect.error = None;
                    self.sync_walletconnect_attention();
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "wallet::root::walletconnect",
                        pairing_topic = %walletconnect_topic_log_label(&pairing.topic),
                        error = %walletconnect_error_message(&error),
                        "could not decode walletconnect proposal"
                    );
                    self.walletconnect.error = Some(Arc::from(format!(
                        "Could not decode WalletConnect proposal: {}",
                        walletconnect_error_message(&error)
                    )));
                }
            }
        }
    }

    pub(in crate::root::walletconnect) fn supported_walletconnect_chain_ids(
        &self,
    ) -> BTreeSet<u64> {
        walletconnect_enabled_chain_ids(&self.effective_chain_configs)
    }

    pub(in crate::root::walletconnect) fn walletconnect_effective_chain_config(
        &self,
        chain_id: u64,
    ) -> Option<EffectiveChainConfig> {
        self.effective_chain_configs.enabled(chain_id).ok().cloned()
    }

    pub(in crate::root::walletconnect) fn ensure_walletconnect_chain_enabled(
        &self,
        chain_id: &str,
    ) -> Result<(), WalletConnectSessionRequestFailure> {
        ensure_walletconnect_chain_id_enabled(chain_id, &self.supported_walletconnect_chain_ids())
    }

    pub(in crate::root::walletconnect) fn walletconnect_proposal_negotiation(
        &self,
        proposal: &WalletConnectSessionProposal,
        selected_account: &PublicAccountMetadata,
    ) -> Result<WalletConnectNamespaceNegotiation, WalletConnectError> {
        let account_support =
            walletconnect_namespace_account_support(selected_account, self.view_session.as_deref());
        negotiate_walletconnect_namespaces_with_account_support(
            &proposal.required_namespaces,
            &proposal.optional_namespaces,
            &self.supported_walletconnect_chain_ids(),
            selected_account.address,
            account_support,
        )
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_proposal(
        &self,
        root: &Entity<Self>,
        proposal: &WalletConnectProposalUi,
    ) -> gpui::Div {
        let now = current_unix_seconds();
        let expired = proposal.proposal.is_expired(now);
        let selected_account = self.selected_walletconnect_public_account();
        let negotiation = selected_account
            .map(|account| self.walletconnect_proposal_negotiation(&proposal.proposal, account));
        let typed_data_probe_can_satisfy = selected_account.is_some_and(|account| {
            #[cfg(feature = "hardware")]
            {
                account.source == PublicAccountSource::HardwareDerived
                    && walletconnect_proposal_requests_hardware_typed_data(&proposal.proposal)
                    && !walletconnect_namespace_account_support(
                        account,
                        self.view_session.as_deref(),
                    )
                    .hardware_typed_data_signing_mode
                    .is_supported()
                    && negotiate_walletconnect_namespaces_with_account_support(
                        &proposal.proposal.required_namespaces,
                        &proposal.proposal.optional_namespaces,
                        &self.supported_walletconnect_chain_ids(),
                        account.address,
                        WalletConnectNamespaceAccountSupport::hardware(
                            HardwareTypedDataSigningMode::ClearSign,
                        ),
                    )
                    .is_ok()
            }
            #[cfg(not(feature = "hardware"))]
            {
                let _ = account;
                false
            }
        });
        let can_approve = !expired
            && selected_account
                .is_some_and(|account| account.status == PublicAccountStatus::Active)
            && (matches!(&negotiation, Some(Ok(_))) || typed_data_probe_can_satisfy)
            && !self.walletconnect.approving_proposal;
        let approve_root = root.clone();
        let reject_root = root.clone();
        let selected_label = selected_account.map_or_else(
            || "No active Public account selected".to_owned(),
            public_account_walletconnect_label,
        );
        let mut card = walletconnect_subpanel("Session proposal")
            .child(walletconnect_metadata_block(
                &proposal.proposal.peer_metadata,
            ))
            .child(walletconnect_kv_row(
                "Selected Public account",
                selected_label,
            ))
            .child(walletconnect_privacy_notices());
        if let Some(Err(error)) = &negotiation
            && !typed_data_probe_can_satisfy
        {
            card = card.child(
                Alert::warning(
                    "walletconnect-unsupported-required",
                    format!("Required namespaces cannot be satisfied: {error}"),
                )
                .small(),
            );
        }
        if expired {
            card = card.child(
                Alert::warning(
                    "walletconnect-proposal-expired",
                    "This WalletConnect proposal expired. Reject it and paste a fresh wc: URI.",
                )
                .small(),
            );
        }
        card.child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    app_button("walletconnect-proposal-reject", "Reject")
                        .outline()
                        .small()
                        .disabled(self.walletconnect.approving_proposal)
                        .on_click(move |_event, _window, cx| {
                            reject_root.update(cx, |root, cx| {
                                root.reject_walletconnect_proposal(cx);
                            });
                        }),
                )
                .child(
                    app_button(
                        "walletconnect-proposal-approve",
                        if self.walletconnect.approving_proposal {
                            "Connecting..."
                        } else {
                            "Connect dapp"
                        },
                    )
                    .primary()
                    .small()
                    .loading(self.walletconnect.approving_proposal)
                    .disabled(!can_approve)
                    .on_click(move |_event, window, cx| {
                        approve_root.update(cx, |root, cx| {
                            root.approve_walletconnect_proposal(window, cx);
                        });
                    }),
                ),
        )
    }

    pub(in crate::root::walletconnect) fn approve_walletconnect_proposal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.approving_proposal {
            return;
        }
        let Some(proposal_ui) = self.walletconnect.pending_proposal.clone() else {
            return;
        };
        let Some(selected_account) = self.selected_walletconnect_public_account().cloned() else {
            self.walletconnect.error = Some(Arc::from("Select an active Public account first"));
            cx.notify();
            return;
        };
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            self.walletconnect.error = Some(Arc::from("Unlock a wallet before connecting"));
            cx.notify();
            return;
        };
        if self.walletconnect_proposal_needs_hardware_typed_data_probe(
            &proposal_ui.proposal,
            &selected_account,
            view_session.as_ref(),
        ) {
            self.probe_hardware_typed_data_for_proposal_then_approve(
                proposal_ui,
                selected_account,
                store,
                view_session,
                window,
                cx,
            );
            return;
        }
        self.approve_walletconnect_proposal_with_current_support(window, cx);
    }

    fn approve_walletconnect_proposal_with_current_support(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.approving_proposal {
            return;
        }
        let Some(proposal_ui) = self.walletconnect.pending_proposal.clone() else {
            return;
        };
        let Some(selected_account) = self.selected_walletconnect_public_account().cloned() else {
            self.walletconnect.error = Some(Arc::from("Select an active Public account first"));
            cx.notify();
            return;
        };
        let (Some(store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            self.walletconnect.error = Some(Arc::from("Unlock a wallet before connecting"));
            cx.notify();
            return;
        };
        let context = match self.walletconnect_client_context(cx) {
            Ok(context) => context,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let relay_identity =
            match store.load_or_create_walletconnect_relay_identity(view_session.as_ref()) {
                Ok(identity) => identity,
                Err(error) => {
                    self.walletconnect.error = Some(Arc::from(format!(
                        "Could not load WalletConnect relay identity: {error}"
                    )));
                    cx.notify();
                    return;
                }
            };
        let session_uuid = walletconnect_session_uuid(&proposal_ui.proposal);
        let selected_account_support =
            walletconnect_namespace_account_support(&selected_account, Some(view_session.as_ref()));
        let approval = match approve_walletconnect_session_with_account_support(
            &proposal_ui.proposal,
            &proposal_ui.pairing.sym_key,
            &relay_identity,
            &selected_account,
            selected_account_support,
            &self.supported_walletconnect_chain_ids(),
            session_uuid,
            current_unix_seconds(),
        ) {
            Ok(approval) => approval,
            Err(error) => {
                self.walletconnect.error = Some(Arc::from(walletconnect_error_message(&error)));
                cx.notify();
                return;
            }
        };
        let session = approval.session.clone();
        let steps = approval.relay_steps;
        let enabled_chain_ids = self.supported_walletconnect_chain_ids();
        tracing::info!(
            target: "wallet::root::walletconnect",
            session_uuid = %walletconnect_request_key_log_label(&session.session_uuid),
            pairing_topic = %walletconnect_topic_log_label(&session.pairing_topic),
            session_topic = %walletconnect_topic_log_label(&session.session_topic),
            dapp = session.peer_metadata.name.as_str(),
            "approving walletconnect session proposal"
        );
        self.walletconnect.approving_proposal = true;
        self.walletconnect.error = None;
        self.walletconnect.status = Some(Arc::from("Publishing WalletConnect session approval..."));
        let handoff_topic = session.session_topic.clone();
        self.walletconnect
            .approval_handoff_sessions
            .insert(handoff_topic.clone(), session.clone());
        let join = self.runtime.spawn(async move {
            let relay_result = execute_walletconnect_approval_relay_steps(
                &context.worker,
                &store,
                view_session.as_ref(),
                &session,
                steps,
            )
            .await?;
            let mut processing_result = process_walletconnect_relay_output(
                &context.worker,
                &store,
                &view_session,
                &[],
                std::slice::from_ref(&session),
                &enabled_chain_ids,
                relay_result.output,
                current_unix_seconds(),
            )
            .await;
            if let Some(error) = relay_result.post_persist_error {
                processing_result.error.get_or_insert(error);
            }
            Ok::<WalletConnectRelayProcessingResult, String>(processing_result)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.approving_proposal = false;
                match result {
                    Ok(Ok(processing_result)) => {
                        let approval_relay_error = processing_result.error.clone();
                        tracing::info!(
                            target: "wallet::root::walletconnect",
                            "walletconnect session approval persisted"
                        );
                        root.walletconnect
                            .pending_pairings
                            .remove(&proposal_ui.pairing.topic);
                        root.walletconnect.pending_proposal = None;
                        root.reload_walletconnect_sessions(cx);
                        root.walletconnect
                            .approval_handoff_sessions
                            .remove(&handoff_topic);
                        root.apply_walletconnect_relay_processing_result(Ok(processing_result), cx);
                        if let Some(error) = approval_relay_error {
                            root.walletconnect.retain_pending_requests(|_, request| {
                                request.session_identity.walletconnect_session_id().is_none() || request.item.topic.as_str() != handoff_topic.as_str()
                            });
                            root.walletconnect.subscriptions.remove(&handoff_topic);
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect approval publish failed; the local session was removed.",
                            ));
                            root.walletconnect.error = Some(Arc::from(error));
                        } else {
                            root.walletconnect.status =
                                Some(Arc::from("WalletConnect session connected."));
                            root.walletconnect.connection_dialog_open = false;
                            window.close_all_dialogs(cx);
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            target: "wallet::root::walletconnect",
                            error = %error,
                            "walletconnect session approval failed"
                        );
                        root.walletconnect
                            .approval_handoff_sessions
                            .remove(&handoff_topic);
                        root.walletconnect.retain_pending_requests(|_, request| {
                            request.session_identity.walletconnect_session_id().is_none() || request.item.topic.as_str() != handoff_topic.as_str()
                        });
                        root.walletconnect.error = Some(Arc::from(error));
                    }
                    Err(error) => {
                        root.walletconnect
                            .approval_handoff_sessions
                            .remove(&handoff_topic);
                        root.walletconnect.retain_pending_requests(|_, request| {
                            request.session_identity.walletconnect_session_id().is_none() || request.item.topic.as_str() != handoff_topic.as_str()
                        });
                        root.walletconnect.error = Some(Arc::from(format!(
                            "WalletConnect approval task failed: {error}"
                        )));
                    }
                }
                root.sync_walletconnect_attention();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    #[cfg(feature = "hardware")]
    #[allow(clippy::unused_self)]
    fn walletconnect_proposal_needs_hardware_typed_data_probe(
        &self,
        proposal: &WalletConnectSessionProposal,
        selected_account: &PublicAccountMetadata,
        view_session: &DesktopViewSession,
    ) -> bool {
        selected_account.source == PublicAccountSource::HardwareDerived
            && walletconnect_proposal_requests_hardware_typed_data(proposal)
            && !walletconnect_namespace_account_support(selected_account, Some(view_session))
                .hardware_typed_data_signing_mode
                .is_supported()
    }

    #[cfg(not(feature = "hardware"))]
    #[allow(clippy::unused_self)]
    const fn walletconnect_proposal_needs_hardware_typed_data_probe(
        &self,
        proposal: &WalletConnectSessionProposal,
        selected_account: &PublicAccountMetadata,
        view_session: &DesktopViewSession,
    ) -> bool {
        let _ = (proposal, selected_account, view_session);
        false
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn probe_hardware_typed_data_for_proposal_then_approve(
        &mut self,
        proposal_ui: WalletConnectProposalUi,
        selected_account: PublicAccountMetadata,
        vault_store: Arc<DesktopVaultStore>,
        view_session: Arc<DesktopViewSession>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase = view_session.hardware_profile_session().and_then(|session| {
            self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
        });
        #[cfg(not(feature = "hardware"))]
        let trezor_app_passphrase = None;
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider =
            Some(self.trezor_pin_matrix_provider_for_operation(window, cx));
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_provider = None;
        self.walletconnect.approving_proposal = true;
        self.walletconnect.error = None;
        self.walletconnect.status = Some(Arc::from(
            "Checking WalletConnect hardware typed-data support...",
        ));
        let typed_data_required =
            walletconnect_proposal_requests_required_typed_data(&proposal_ui.proposal);
        let selected_account_uuid = selected_account.public_account_uuid;
        let public_account_uuid = selected_account_uuid.clone();
        let join = self.runtime.spawn(async move {
            walletconnect_probe_hardware_typed_data_signing_mode(
                WalletConnectHardwareTypedDataCapabilityRequest {
                    view_session,
                    vault_store,
                    trezor_app_passphrase,
                    trezor_pin_matrix_provider,
                    public_account_uuid,
                },
            )
            .await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.approving_proposal = false;
                let pending_matches = root
                    .walletconnect
                    .pending_proposal
                    .as_ref()
                    .is_some_and(|pending| {
                        pending.proposal.id == proposal_ui.proposal.id
                            && pending.proposal.pairing_topic == proposal_ui.proposal.pairing_topic
                    });
                let account_matches = root
                    .selected_walletconnect_public_account()
                    .is_some_and(|account| {
                        account.public_account_uuid.as_str() == selected_account_uuid.as_str()
                    });
                if !pending_matches || !account_matches {
                    cx.notify();
                    return;
                }
                let mut continue_approval = true;
                match result {
                    Ok(Ok(result)) if result.mode.is_supported() => {
                        if let Some(session) = result.refreshed_hardware_session {
                            #[cfg(feature = "hardware")]
                            root.refresh_active_hardware_profile_session(session, cx);
                            #[cfg(not(feature = "hardware"))]
                            let _ = session;
                        }
                        root.walletconnect.status = Some(Arc::from(
                            "WalletConnect hardware typed-data support verified.",
                        ));
                    }
                    Ok(Ok(_)) => {
                        if typed_data_required {
                            root.walletconnect.error = Some(Arc::from(
                                "The connected hardware session does not support WalletConnect typed-data signing.",
                            ));
                            continue_approval = false;
                        } else {
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect hardware typed-data support is unavailable; connecting without typed-data.",
                            ));
                        }
                    }
                    Ok(Err(error)) => {
                        root.discard_active_trezor_session_if_stale(&format_report_chain(&error), cx);
                        if typed_data_required {
                            root.walletconnect.error = Some(Arc::from(format!(
                                "Could not check hardware typed-data support: {}",
                                format_report_chain(&error)
                            )));
                            continue_approval = false;
                        } else {
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect hardware typed-data support could not be checked; connecting without typed-data.",
                            ));
                        }
                    }
                    Err(error) => {
                        if typed_data_required {
                            root.walletconnect.error = Some(Arc::from(format!(
                                "Hardware typed-data support check failed: {error}"
                            )));
                            continue_approval = false;
                        } else {
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect hardware typed-data support check failed; connecting without typed-data.",
                            ));
                        }
                    }
                }
                if continue_approval {
                    root.approve_walletconnect_proposal_with_current_support(window, cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn reject_walletconnect_proposal(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(proposal_ui) = self.walletconnect.pending_proposal.clone() else {
            return;
        };
        let context = match self.walletconnect_client_context(cx) {
            Ok(context) => context,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let now = current_unix_seconds();
        let supported_chain_ids = self.supported_walletconnect_chain_ids();
        let reason = walletconnect_proposal_rejection_reason(
            &proposal_ui.proposal,
            self.selected_walletconnect_public_account(),
            self.selected_walletconnect_public_account().map(|account| {
                walletconnect_namespace_account_support(account, self.view_session.as_deref())
            }),
            &supported_chain_ids,
            now,
        );
        let response = reject_walletconnect_session_proposal(proposal_ui.proposal.id, reason);
        let message =
            match encode_walletconnect_response_message(&proposal_ui.pairing.sym_key, &response) {
                Ok(message) => message,
                Err(error) => {
                    self.walletconnect.error = Some(Arc::from(format!(
                        "Could not encrypt WalletConnect rejection: {error}"
                    )));
                    cx.notify();
                    return;
                }
            };
        let steps = vec![WalletConnectRelayStep::Publish(
            WalletConnectRelayRpc::Publish {
                topic: proposal_ui.proposal.pairing_topic.clone(),
                message,
                ttl: WALLETCONNECT_RELAY_TTL_SECS,
                tag: WC_SESSION_PROPOSE_REJECT_TAG,
            },
        )];
        self.walletconnect.approving_proposal = true;
        let join = self
            .runtime
            .spawn(async move { execute_walletconnect_relay_steps(&context.worker, steps).await });
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |root, cx| {
                root.walletconnect.approving_proposal = false;
                root.walletconnect
                    .pending_pairings
                    .remove(&proposal_ui.pairing.topic);
                root.walletconnect
                    .subscriptions
                    .remove(&proposal_ui.pairing.topic);
                root.walletconnect.pending_proposal = None;
                root.ensure_walletconnect_relay_processing(cx);
                root.walletconnect.status = Some(Arc::from("WalletConnect proposal rejected."));
                if let Ok(Err(error)) = result {
                    root.walletconnect.error = Some(Arc::from(format!(
                        "Proposal was removed locally, but relay rejection failed: {error}"
                    )));
                }
                root.sync_walletconnect_attention();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
