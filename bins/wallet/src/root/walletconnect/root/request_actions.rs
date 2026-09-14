use super::super::account_select::public_account_walletconnect_label;
use super::super::fee::{
    WalletConnectFeeState, WalletConnectFeeStatus, WalletConnectReviewedFeeProjection,
    validate_walletconnect_reviewed_fee_pairing, walletconnect_fee_retry_action_enabled,
    walletconnect_fee_retrying, walletconnect_fee_state_projection,
    walletconnect_request_can_simulate, walletconnect_request_fee_eligible,
    walletconnect_request_raw_gas,
};
use super::super::helpers::{
    WALLETCONNECT_NETWORK_FEE_LABEL, WALLETCONNECT_RAW_REQUEST_LABEL,
    WALLETCONNECT_TRANSACTION_DETAILS_LABEL, WalletConnectRequestExpiryStatus,
    parse_caip2_chain_id, walletconnect_transaction_selector,
};
use super::super::intent::{
    WalletConnectAmount, WalletConnectHeroSummary, WalletConnectIntentAction,
    WalletConnectIntentContext, WalletConnectIntentView, WalletConnectParty,
    WalletConnectPartyRole, WalletConnectRisk, build_walletconnect_intent,
    classify_walletconnect_fee_significance, walletconnect_approximate_usd_label,
    walletconnect_moving_usd_micro_value, walletconnect_party_address_label,
    walletconnect_party_badge_label, walletconnect_party_role_label,
    walletconnect_selected_account_provenance_visible, walletconnect_should_render_token_contract,
    walletconnect_simulation_risk, walletconnect_token_contract_recognition,
};
use super::super::render::walletconnect_disclosure_content;
use super::*;
use crate::root::gas_fee::{
    Eip1559GasFeeEditorState, Eip1559GasFeeTarget, format_gwei, render_eip1559_gas_fee_editor,
};
use crate::root::public_action::format_gas_limit;
use crate::root::tokens::format_native_token_amount_for_display;
use alloy::primitives::Address;
use gpui::relative;
use gpui_component::collapsible::Collapsible;
use railgun_ui::format_usd_micro_value;
use ui::controls::app_button_label;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WalletConnectRequestFooterActionState {
    reject_enabled: bool,
    approve_enabled: bool,
    approve_danger: bool,
}

const fn walletconnect_request_footer_action_state(
    in_flight: bool,
    locally_expired: bool,
    intent_context_available: bool,
    unlimited_allowance: bool,
    hardware_request: bool,
    hardware_typed_data_hash_fallback: bool,
) -> WalletConnectRequestFooterActionState {
    WalletConnectRequestFooterActionState {
        reject_enabled: !in_flight,
        approve_enabled: intent_context_available && !in_flight && !locally_expired,
        approve_danger: intent_context_available
            && unlimited_allowance
            && !in_flight
            && !hardware_request
            && !hardware_typed_data_hash_fallback,
    }
}

pub(in crate::root::walletconnect) fn gateway_pending_request(
    approval: &wallet_ops::gateway::GatewayApprovalRequest,
    rpc_reads: wallet_ops::DappRpcReadClient,
) -> Option<WalletConnectRequestUi> {
    approval.control.ensure_current().ok()?;
    let key = format!("gateway:{}", approval.id);
    let chain_id = format!("eip155:{}", approval.chain_id);
    let remaining = approval
        .deadline
        .saturating_duration_since(tokio::time::Instant::now());
    let expiry = current_unix_seconds()
        .saturating_add(remaining.as_secs())
        .saturating_add(u64::from(remaining.subsec_nanos() != 0));
    let item = approval.parsed.desktop_approval(
        0,
        &key,
        "Browser dapp",
        &chain_id,
        approval.account.address,
        Some(expiry),
    );
    Some(WalletConnectRequestUi {
        key,
        review_token: walletconnect_request_id_seed(),
        binding: DappRequestBinding {
            public_account_uuid: approval.permission.public_account_uuid.clone(),
            public_account_scope: approval.permission.public_account_scope.clone(),
            owning_private_wallet_uuid: approval.permission.owning_private_wallet_uuid.clone(),
            peer_name: approval.origin.paired_peer_id()?.to_owned(),
            peer_url: approval.origin.web_origin()?.as_str().to_owned(),
        },
        session_identity: DappSessionIdentity {
            transport: "gateway",
            id: approval.id.clone(),
        },
        parsed: approval.parsed.clone(),
        item,
        account_source: approval.account.source,
        request_control: Some(approval.control.clone()),
        rpc_reads: Some(rpc_reads),
    })
}

impl WalletRoot {
    pub(in crate::root) fn reconcile_gateway_approvals(
        &mut self,
        handle: &wallet_ops::gateway::GatewayHandle,
        ready: &[Arc<wallet_ops::gateway::GatewayApprovalRequest>],
        cx: &mut Context<'_, Self>,
    ) {
        let removed: Vec<_> = self
            .walletconnect
            .pending_requests
            .iter()
            .filter(|(_, request)| request.request_control.is_some())
            .filter(|(key, request)| {
                !request.is_current()
                    || (!self.walletconnect.request_actions.contains(key.as_str())
                        && !ready
                            .iter()
                            .any(|item| request.key == format!("gateway:{}", item.id)))
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in removed {
            self.walletconnect.remove_pending_request(&key);
            self.walletconnect.request_actions.remove(&key);
            if self.walletconnect.request_dialog_key.as_deref() == Some(key.as_str()) {
                self.clear_spend_authorization(cx);
                self.discard_walletconnect_fee_for_request_replacement();
            }
        }
        if let (Some(store), Some(view)) = (&self.vault_store, &self.view_session) {
            for approval in ready {
                let key = format!("gateway:{}", approval.id);
                if !walletconnect_request_should_queue(
                    &self.walletconnect.pending_requests,
                    &self.walletconnect.handled_request_keys,
                    &key,
                ) {
                    continue;
                }
                let Some(request) =
                    gateway_pending_request(approval, handle.approval_reads(approval.id.clone()))
                else {
                    continue;
                };
                let resolution = store.resolve_dapp_session_account(
                    view,
                    &request.binding.public_account_uuid,
                    &request.binding.public_account_scope,
                    request.binding.owning_private_wallet_uuid.as_deref(),
                );
                if !matches!(resolution, Ok(WalletConnectSessionAccountResolution::Usable(account)) if account == approval.account)
                {
                    continue;
                }
                self.walletconnect.request_routes.insert(
                    request.key.clone(),
                    DappRequestRoute::Gateway {
                        handle: handle.clone(),
                        approval_id: approval.id.clone(),
                    },
                );
                self.walletconnect
                    .pending_requests
                    .insert(request.key.clone(), request);
            }
        }
        self.publish_gateway_summaries(self.gateway_request_summaries());
        self.load_gateway_asset_metadata(cx);
        self.ensure_walletconnect_pending_request_expiry_timer(cx);
        self.sync_walletconnect_attention();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_request(
        &self,
        root: &Entity<Self>,
        request: &WalletConnectRequestUi,
        content_width: Pixels,
    ) -> gpui::Div {
        let intent_context = match self.walletconnect_intent_context(request) {
            Ok(context) => context,
            Err(message) => {
                return div().w_full().child(
                    Alert::error(
                        SharedString::from(format!(
                            "walletconnect-request-intent-context-error-{}",
                            request.key
                        )),
                        message,
                    )
                    .small(),
                );
            }
        };
        let public_accounts = intent_context.public_accounts;
        let fee_state = self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .filter(|state| state.request_key.as_ref() == request.key);
        let mut intent = build_walletconnect_intent(request, intent_context);
        append_walletconnect_simulation_risk(request.key.as_ref(), fee_state, &mut intent.risks);
        let fee_significance = fee_state
            .and_then(|state| {
                walletconnect_fee_state_projection(
                    state,
                    self.walletconnect.walletconnect_gas_fee.refreshing,
                )
            })
            .map(|projection| {
                classify_walletconnect_fee_significance(
                    projection.expected_native_usd_micro_value,
                    walletconnect_moving_usd_micro_value(
                        parse_display_chain_id(&request.item.chain_id),
                        intent.action,
                        &intent.amount,
                        intent.attached_native.as_ref(),
                        intent_context.anchor_rates,
                    ),
                )
            });
        let disclosure_state = self
            .walletconnect
            .request_disclosure_state(request.key.as_str());
        let hardware_typed_data_hash_fallback =
            walletconnect_request_uses_hardware_typed_data_hash_fallback(
                request,
                self.walletconnect_request_hardware_typed_data_mode(request),
            );
        let mut content = div().w_full().min_w(px(0.0)).flex().flex_col().gap_2();
        match &request.parsed {
            WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id } => {
                let description = if self.selected_chain == *chain_id {
                    "Switch this website to your wallet's current network."
                } else {
                    "Switch the network for this website and your wallet."
                };
                content = content.child(app_muted_text(description));
            }
            WalletConnectParsedRequest::WalletAddEthereumChain { chain_id, .. } => {
                content = content.child(walletconnect_kv_row("Configured chain", chain_id.to_string()))
                    .child(app_muted_text("This network is already configured. This confirms that the network is available with the wallet's saved settings. Supplied network metadata is ignored."));
            }
            WalletConnectParsedRequest::WalletWatchAsset { address, .. } => {
                content = content
                    .child(walletconnect_kv_row(
                        "Token chain",
                        request.item.chain_id.clone(),
                    ))
                    .child(walletconnect_kv_row("Token address", address.to_string()));
                content = match self.walletconnect.watch_asset_metadata.get(&request.key).and_then(Option::as_ref) {
                    Some(Ok(token)) => content.child(walletconnect_kv_row("Token symbol", token.symbol.clone()))
                        .child(walletconnect_kv_row("Token decimals", token.decimals.to_string()))
                        .child(app_muted_text("Confirm adding this token to the wallet. Website metadata and images are ignored.")),
                    Some(Err(message)) => content.child(app_muted_text(message.clone())),
                    None => content.child(app_muted_text("Loading token metadata from the saved network...")),
                };
            }
            _ => {}
        }

        for (risk_index, risk) in intent.risks.iter().enumerate() {
            content = content.child(render_walletconnect_intent_risk(
                &request.key,
                risk_index,
                risk,
            ));
        }
        content = content
            .child(render_walletconnect_intent_card(
                &request.key,
                &intent,
                content_width,
            ))
            .child(render_walletconnect_request_provenance(
                request,
                &intent,
                public_accounts,
            ))
            .when(
                walletconnect_request_fee_eligible(&request.parsed),
                |this| {
                    this.child(render_walletconnect_network_fee(
                        root,
                        request,
                        fee_state,
                        self.walletconnect.walletconnect_gas_fee.refreshing,
                        disclosure_state.network_fee_open,
                        &self.walletconnect.walletconnect_gas_fee,
                        fee_significance,
                    ))
                },
            )
            .when_some(intent.transaction.as_ref(), |this, transaction| {
                this.child(render_walletconnect_request_disclosure(
                    root,
                    &request.key,
                    WalletConnectRequestDisclosure::TransactionDetails,
                    WALLETCONNECT_TRANSACTION_DETAILS_LABEL,
                    disclosure_state.transaction_details_open,
                    render_walletconnect_transaction_details(
                        transaction,
                        &request.key,
                        content_width,
                    ),
                ))
            })
            .child(render_walletconnect_request_disclosure(
                root,
                &request.key,
                WalletConnectRequestDisclosure::RawRequest,
                WALLETCONNECT_RAW_REQUEST_LABEL,
                disclosure_state.raw_request_open,
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(request.request_control.is_some(), |this| {
                        this.child(walletconnect_kv_row(
                            "Paired browser",
                            request.binding.peer_name.clone(),
                        ))
                    })
                    .child(walletconnect_raw_details(&request.key, intent.raw_request)),
            ));
        if hardware_typed_data_hash_fallback {
            content = content.child(
                Alert::warning(
                    SharedString::from(format!(
                        "walletconnect-hardware-typed-data-fallback-{}",
                        request.key
                    )),
                    "This hardware session will use the device's EIP-712 hash-signing fallback. RailOxide computed the typed-data hashes from the validated request and will verify the signature before responding, but the device may show hashes instead of structured fields. Continue only if you accept this reduced device visibility.",
                )
                .small(),
            );
        }
        if matches!(request.account_source, PublicAccountSource::HardwareDerived)
            && !request.parsed.desktop_policy()
        {
            content = content.child(
                Alert::warning(
                    SharedString::from(format!(
                        "walletconnect-hardware-request-warning-{}",
                        request.key
                    )),
                    hardware_walletconnect_notice(request.item.method),
                )
                .small(),
            );
            #[cfg(feature = "hardware")]
            {
                if self.current_session_needs_trezor_app_passphrase() {
                    content = content.child(walletconnect_trezor_app_passphrase_input(
                        &self.trezor_app_passphrase_input,
                        self.walletconnect
                            .request_actions
                            .contains(request.key.as_str()),
                    ));
                }
            }
        }
        if let Some(progress) = self
            .walletconnect
            .request_approval_progress
            .get(request.key.as_str())
        {
            content = content.child(render_walletconnect_approval_stepper(progress));
        }
        if let Some(error) = self.walletconnect.error.as_ref() {
            content = content.child(
                Alert::error("walletconnect-request-dialog-error", error.to_string()).small(),
            );
        }
        content
    }

    pub(in crate::root::walletconnect) fn render_walletconnect_request_footer(
        &self,
        root: &Entity<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(request_key) = self.walletconnect.request_dialog_key.as_deref() else {
            return Vec::new();
        };
        if self
            .walletconnect
            .completed_request_dialogs
            .contains_key(request_key)
        {
            return Vec::new();
        }
        let Some(request) = self.walletconnect.pending_requests.get(request_key) else {
            return Vec::new();
        };
        let in_flight = self.walletconnect.request_actions.contains(request_key);
        let locally_expired = !request.approval_admitted(current_unix_seconds());
        let (intent_context_available, unlimited_allowance) =
            match self.walletconnect_intent_context(request) {
                Ok(context) => {
                    let intent = build_walletconnect_intent(request, context);
                    (
                        true,
                        intent.risks.iter().any(|risk| {
                            matches!(risk, &WalletConnectRisk::UnlimitedAllowance { .. })
                        }),
                    )
                }
                Err(_) => (false, false),
            };
        let hardware_request = request.account_source == PublicAccountSource::HardwareDerived
            && !request.parsed.desktop_policy();
        let hardware_typed_data_hash_fallback =
            walletconnect_request_uses_hardware_typed_data_hash_fallback(
                request,
                self.walletconnect_request_hardware_typed_data_mode(request),
            );
        let action_state = walletconnect_request_footer_action_state(
            in_flight,
            locally_expired,
            intent_context_available
                && (!matches!(
                    request.parsed,
                    WalletConnectParsedRequest::WalletWatchAsset { .. }
                ) || matches!(
                    self.walletconnect.watch_asset_metadata.get(request_key),
                    Some(Some(Ok(_)))
                )),
            unlimited_allowance,
            hardware_request,
            hardware_typed_data_hash_fallback,
        );
        let approve_label = walletconnect_request_approve_label(
            in_flight,
            hardware_request,
            hardware_typed_data_hash_fallback,
            unlimited_allowance,
        );
        let reject_root = root.clone();
        let reject_key = Arc::<str>::from(request_key);
        let approve_root = root.clone();
        let approve_key = Arc::clone(&reject_key);
        let reject = app_button(
            SharedString::from(format!("walletconnect-request-reject-{request_key}")),
            "Reject",
        )
        .outline()
        .small()
        .disabled(!action_state.reject_enabled)
        .on_click(move |_event, window, cx| {
            let key = Arc::clone(&reject_key);
            reject_root.update(cx, |root, cx| {
                root.reject_walletconnect_request(key.as_ref(), window, cx);
            });
        })
        .into_any_element();
        let approve = app_button(
            SharedString::from(format!("walletconnect-request-approve-{request_key}")),
            if request.parsed.desktop_policy() {
                "Confirm"
            } else {
                approve_label
            },
        )
        .primary()
        .small()
        .loading(in_flight)
        .disabled(!action_state.approve_enabled);
        let approve = if action_state.approve_danger {
            approve.danger()
        } else {
            approve
        }
        .on_click(move |_event, window, cx| {
            let key = Arc::clone(&approve_key);
            approve_root.update(cx, |root, cx| {
                root.approve_walletconnect_request(key.as_ref(), window, cx);
            });
        })
        .into_any_element();
        vec![reject, approve]
    }

    fn walletconnect_request_hardware_typed_data_mode(
        &self,
        request: &WalletConnectRequestUi,
    ) -> HardwareTypedDataSigningMode {
        walletconnect_hardware_typed_data_mode_for_request(
            request,
            &self.public_accounts,
            self.view_session.as_deref(),
        )
    }

    pub(in crate::root) fn gateway_request_summaries(&self) -> Vec<(String, String)> {
        self.walletconnect
            .pending_requests
            .values()
            .filter_map(|request| {
                let Some(DappRequestRoute::Gateway { approval_id, .. }) =
                    self.walletconnect.request_routes.get(&request.key)
                else {
                    return None;
                };
                if !request.is_current() {
                    return None;
                }
                let context = self.walletconnect_intent_context(request).ok()?;
                Some((
                    approval_id.clone(),
                    build_walletconnect_intent(request, context).authorization,
                ))
            })
            .collect()
    }

    fn walletconnect_intent_context(
        &self,
        request: &WalletConnectRequestUi,
    ) -> Result<WalletConnectIntentContext<'_>, &'static str> {
        let chain_id = parse_caip2_chain_id(&request.item.chain_id)
            .ok_or("This request does not identify a supported EVM chain.")?;
        let chain = self
            .effective_chain_configs
            .get(&chain_id)
            .ok_or("The request chain is not available in the current wallet settings.")?;
        Ok(WalletConnectIntentContext {
            chain,
            selected_chain_id: self.selected_chain,
            token_registry: &self.effective_token_registry,
            anchor_rates: &self.public_broadcaster_anchor_cache,
            public_accounts: &self.public_accounts,
            public_address_book: &self.public_address_book,
        })
    }

    pub(in crate::root::walletconnect) fn approve_walletconnect_request(
        &mut self,
        request_key: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.request_actions.contains(request_key) {
            return;
        }
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key)
            .cloned()
        else {
            return;
        };
        if !request.approval_admitted(current_unix_seconds()) {
            self.walletconnect.error = Some(Arc::from(
                "WalletConnect request expired before approval could start.",
            ));
            cx.notify();
            return;
        }
        if request.request_control.is_some() && request.parsed.desktop_policy() {
            self.approve_gateway_policy(request, window, cx);
            return;
        }
        let reviewed_fee = if walletconnect_request_fee_eligible(&request.parsed) {
            match self.capture_walletconnect_reviewed_fee(&request, cx) {
                Ok(reviewed_fee) => Some(reviewed_fee),
                Err(error) => {
                    self.walletconnect.error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        tracing::info!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(request_key),
            method = request.item.method.as_str(),
            chain_id = request.item.chain_id.as_str(),
            dapp = request.item.dapp_name.as_str(),
            hardware = request.account_source == PublicAccountSource::HardwareDerived,
            "walletconnect request approval selected"
        );
        if request.account_source == PublicAccountSource::HardwareDerived {
            self.submit_walletconnect_request_authorized(
                request_key,
                request.review_token,
                reviewed_fee,
                Zeroizing::new(String::new()),
                None,
                window,
                cx,
            );
        } else {
            let intent = SpendAuthorizationIntent::WalletConnectRequest {
                request_key: request_key.to_owned(),
                review_token: request.review_token,
                reviewed_fee: reviewed_fee.clone(),
            };
            let summary = {
                let context = match self.walletconnect_intent_context(&request) {
                    Ok(context) => context,
                    Err(message) => {
                        self.walletconnect.error = Some(Arc::from(message));
                        cx.notify();
                        return;
                    }
                };
                let mut intent = build_walletconnect_intent(&request, context);
                append_walletconnect_simulation_risk(
                    request.key.as_ref(),
                    self.walletconnect.walletconnect_fee_state.as_ref(),
                    &mut intent.risks,
                );
                walletconnect_request_authorization_summary_with_fee(
                    &request,
                    &intent,
                    reviewed_fee.as_ref(),
                )
            };
            self.request_spend_authorization(intent, summary, window, cx);
        }
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn submit_walletconnect_request_authorized(
        &mut self,
        request_key: &str,
        review_token: u64,
        reviewed_fee: Option<WalletConnectReviewedFeeProjection>,
        vault_password: Zeroizing<String>,
        protected_software_seed_session: Option<
            Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        >,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.request_actions.contains(request_key) {
            return;
        }
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key)
            .cloned()
        else {
            return;
        };
        if !walletconnect_request_matches_review_token(&request, review_token) {
            self.walletconnect.error = Some(Arc::from(
                "WalletConnect request changed while authorization was open; review the current request before approving.",
            ));
            cx.notify();
            return;
        }
        if let Err(error) =
            validate_walletconnect_reviewed_fee_pairing(&request.parsed, reviewed_fee.as_ref())
        {
            self.walletconnect.error = Some(Arc::from(error));
            self.clear_spend_authorization(cx);
            cx.notify();
            return;
        }
        if walletconnect_request_fee_eligible(&request.parsed)
            && let Some(reviewed_fee) = reviewed_fee.as_ref()
            && let Err(error) = self.validate_walletconnect_reviewed_fee(&request, reviewed_fee, cx)
        {
            self.walletconnect.error = Some(Arc::from(error));
            self.clear_spend_authorization(cx);
            cx.notify();
            return;
        }
        let (Some(vault_store), Some(view_session)) =
            (self.vault_store.clone(), self.view_session.clone())
        else {
            self.walletconnect.error = Some(Arc::from("Unlock a wallet before approving requests"));
            cx.notify();
            return;
        };
        let effective_chain = parse_caip2_chain_id(&request.item.chain_id)
            .and_then(|chain_id| self.walletconnect_effective_chain_config(chain_id));
        let request = match self.revalidate_walletconnect_pending_request(
            &request,
            vault_store.as_ref(),
            view_session.as_ref(),
            current_unix_seconds(),
        ) {
            Ok((request, route)) => {
                self.walletconnect
                    .request_routes
                    .insert(request.key.clone(), route);
                request
            }
            Err(error) => {
                let response_sender = match self.walletconnect_response_sender(&request, cx) {
                    Ok(context) => context,
                    Err(context_error) => {
                        self.walletconnect.error = Some(context_error);
                        cx.notify();
                        return;
                    }
                };
                self.publish_invalid_walletconnect_pending_request(
                    request_key,
                    &error,
                    response_sender,
                    window,
                    cx,
                );
                return;
            }
        };
        let response_sender = match self.walletconnect_response_sender(&request, cx) {
            Ok(context) => context,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let transaction_tracking = if matches!(
            request.parsed,
            WalletConnectParsedRequest::EthSendTransaction { .. }
        ) {
            let tracking = parse_caip2_chain_id(&request.item.chain_id)
                .ok_or_else(|| "WalletConnect request chain is not EIP-155".to_owned())
                .and_then(|chain_id| {
                    self.public_transaction_tracking_context(
                        chain_id,
                        &request.binding.public_account_uuid,
                    )
                });
            match tracking {
                Ok(context) => Some(context),
                Err(error) => {
                    self.walletconnect.error = Some(Arc::from(error));
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase =
            if request.account_source == PublicAccountSource::HardwareDerived {
                view_session.hardware_profile_session().and_then(|session| {
                    self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
                })
            } else {
                None
            };
        #[cfg(not(feature = "hardware"))]
        let trezor_app_passphrase = None;
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider =
            if request.account_source == PublicAccountSource::HardwareDerived {
                Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
            } else {
                None
            };
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_provider = None;
        let http = self.http.clone();
        let hardware_request = request.account_source == PublicAccountSource::HardwareDerived;
        let hash_fallback_confirmed = walletconnect_request_uses_hardware_typed_data_hash_fallback(
            &request,
            self.walletconnect_request_hardware_typed_data_mode(&request),
        );
        let progress_generation = hardware_request.then(|| {
            self.walletconnect
                .start_request_approval_progress(request_key, &request)
        });
        let approval_event_tx = progress_generation.map(|generation| {
            let (event_tx, event_rx) = mpsc::unbounded_channel();
            Self::spawn_walletconnect_approval_session_event_listener(
                request_key.to_owned(),
                generation,
                event_rx,
                cx,
            );
            event_tx
        });
        if self
            .walletconnect
            .walletconnect_fee_state
            .as_ref()
            .is_some_and(|state| {
                state.request_key.as_ref() == request_key && state.review_token == review_token
            })
        {
            self.walletconnect.walletconnect_fee_quote_task = None;
            self.walletconnect.walletconnect_gas_fee.refreshing = false;
        }
        self.walletconnect
            .request_actions
            .insert(request_key.to_owned());
        self.walletconnect.error = None;
        tracing::info!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(request_key),
            method = request.item.method.as_str(),
            chain_id = request.item.chain_id.as_str(),
            dapp = request.item.dapp_name.as_str(),
            "submitting authorized walletconnect request"
        );
        let request_key = request_key.to_owned();
        let native_control = request.request_control.clone();
        let join = self.spawn_public_transaction_submission(async move {
            Box::pin(approve_walletconnect_request_task(
                request,
                vault_store,
                view_session,
                vault_password,
                protected_software_seed_session,
                trezor_app_passphrase,
                trezor_pin_matrix_provider,
                effective_chain,
                response_sender,
                http,
                hash_fallback_confirmed,
                reviewed_fee,
                approval_event_tx,
                transaction_tracking,
            ))
            .await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.request_actions.remove(&request_key);
                if native_control.as_ref().is_some_and(|control| control.ensure_current().is_err()) {
                    root.walletconnect.remove_pending_request(&request_key);
                    root.sync_walletconnect_attention();
                    cx.notify();
                    return;
                }
                match result {
                    Ok(Ok(outcome)) => {
                        tracing::info!(
                            target: "wallet::root::walletconnect",
                            request_key = %walletconnect_request_key_log_label(&request_key),
                            authorization_failed = outcome.authorization_failed,
                            response_published = outcome.response_published,
                            tx_submitted = outcome.submitted_tx_hash.is_some(),
                            "walletconnect request approval handled"
                        );
                        if outcome.hash_fallback_confirmation_required {
                            #[cfg(feature = "hardware")]
                            if let Some(session) = outcome.refreshed_hardware_session {
                                root.refresh_active_hardware_profile_session(session, cx);
                            }
                            root.walletconnect.request_approval_progress.remove(&request_key);
                            root.walletconnect.status = Some(Arc::from(
                                "Review the EIP-712 hash fallback warning, then continue if you still want to approve on device.",
                            ));
                            cx.notify();
                            return;
                        }
                        let completed_request = root.walletconnect.remove_pending_request(&request_key);
                        let show_completion = completed_request.is_some();
                        if let Some(request) = completed_request {
                            root.walletconnect.completed_request_dialogs.insert(
                                request_key.clone(),
                                WalletConnectCompletedRequestUi::from_outcome(request, &outcome),
                            );
                        }
                        if !show_completion {
                            root.stop_walletconnect_request_dialog_refresh();
                        }
                        if root.walletconnect.request_dialog_key.as_deref()
                            == Some(request_key.as_str())
                        {
                            root.clear_trezor_app_passphrase_input(window, cx);
                            if show_completion {
                                root.walletconnect.request_dialog_open = true;
                            } else {
                                root.clear_walletconnect_request_dialog_state(window, cx);
                                window.close_dialog(cx);
                            }
                        }
                        if let Some(error) = outcome.relay_error {
                            let status = outcome.submitted_tx_hash.as_ref().map_or_else(
                                || "WalletConnect request was handled locally, but the relay response publish failed.".to_owned(),
                                |tx_hash| format!(
                                    "WalletConnect transaction was submitted ({tx_hash}), but the relay response publish failed. The request was removed to avoid rebroadcasting."
                                ),
                            );
                            root.walletconnect.status = Some(Arc::from(status));
                            root.walletconnect.error = Some(Arc::from(error));
                        } else if outcome.authorization_failed {
                            root.clear_spend_authorization(cx);
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect request was not authorized; error response published.",
                            ));
                        } else if outcome.expired {
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect request expired before approval completed.",
                            ));
                        } else {
                            root.walletconnect.status = Some(Arc::from(
                                "WalletConnect request approved and response published.",
                            ));
                        }
                    }
                    Ok(Err(error)) => {
                        if native_control.is_some() && !matches!(error, DappApprovalTaskError::AdmissionBusy) {
                            root.walletconnect.remove_pending_request(&request_key);
                        }
                        let error = error.to_string();
                        tracing::warn!(
                            target: "wallet::root::walletconnect",
                            request_key = %walletconnect_request_key_log_label(&request_key),
                            error = %error,
                            "walletconnect request approval failed"
                        );
                        if let Some(generation) = progress_generation {
                            root.walletconnect.fail_request_approval_progress(
                                &request_key,
                                generation,
                                error.clone(),
                            );
                        }
                        root.walletconnect.error = Some(Arc::from(error));
                    }
                    Err(error) => {
                        if native_control.is_some() {
                            root.walletconnect.remove_pending_request(&request_key);
                        }
                        let message = format!("Dapp approval task failed: {error}");
                        if let Some(generation) = progress_generation {
                            root.walletconnect.fail_request_approval_progress(
                                &request_key,
                                generation,
                                message.clone(),
                            );
                        }
                        root.walletconnect.error = Some(Arc::from(message));
                    }
                }
                root.refresh_walletconnect_gas_fee_quote(Arc::from(request_key.as_str()), cx);
                root.sync_walletconnect_attention();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn spawn_walletconnect_approval_session_event_listener(
        request_key: String,
        generation: u64,
        mut event_rx: mpsc::UnboundedReceiver<PublicActionSessionEvent>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                let _ = this.update(cx, |root, cx| {
                    root.apply_walletconnect_approval_session_event(
                        &request_key,
                        generation,
                        event,
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    pub(in crate::root::walletconnect) fn apply_walletconnect_approval_session_event(
        &mut self,
        request_key: &str,
        generation: u64,
        event: PublicActionSessionEvent,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            PublicActionSessionEvent::AttemptHandoff { .. } => {
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::PrepareRequest,
                    PublicActionStepStatus::Done,
                    None,
                    None,
                );
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::ApproveOnDevice,
                    PublicActionStepStatus::Pending,
                    None,
                    None,
                );
            }
            PublicActionSessionEvent::AttemptSubmitted { attempt, .. } => {
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::ApproveOnDevice,
                    PublicActionStepStatus::Done,
                    None,
                    None,
                );
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::BroadcastTransaction,
                    PublicActionStepStatus::Done,
                    Some(attempt.tx_hash),
                    None,
                );
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::RespondToDapp,
                    PublicActionStepStatus::Pending,
                    None,
                    None,
                );
            }
            PublicActionSessionEvent::StepFailed { message, .. }
            | PublicActionSessionEvent::AttemptRejected { message, .. }
            | PublicActionSessionEvent::HardwareApprovalFailed { message } => {
                self.discard_active_trezor_session_if_stale(&message, cx);
                self.walletconnect
                    .fail_request_approval_progress(request_key, generation, message);
            }
            PublicActionSessionEvent::HardwareApprovalStarted => {
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::ApproveOnDevice,
                    PublicActionStepStatus::Pending,
                    None,
                    None,
                );
            }
            PublicActionSessionEvent::HardwareApprovalCompleted => {
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::ApproveOnDevice,
                    PublicActionStepStatus::Done,
                    None,
                    None,
                );
                self.walletconnect.apply_request_approval_progress_update(
                    request_key,
                    generation,
                    WalletConnectApprovalProgressStep::RespondToDapp,
                    PublicActionStepStatus::Pending,
                    None,
                    None,
                );
            }
            PublicActionSessionEvent::HardwareProfileSessionRefreshed { session } => {
                #[cfg(feature = "hardware")]
                self.refresh_active_hardware_profile_session(session, cx);
                #[cfg(not(feature = "hardware"))]
                let _ = session;
            }
            PublicActionSessionEvent::FeeAuthorizationRequired { .. } => {}
        }
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn revalidate_walletconnect_pending_request(
        &self,
        request: &WalletConnectRequestUi,
        store: &DesktopVaultStore,
        view_session: &DesktopViewSession,
        now: u64,
    ) -> Result<
        (WalletConnectRequestUi, WalletConnectRequestRoute),
        WalletConnectSessionRequestFailure,
    > {
        if let Some(control) = &request.request_control {
            let failure = || WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Unauthorized,
                message: "Dapp approval is no longer current".to_owned(),
            };
            control.ensure_current().map_err(|_| failure())?;
            let route = self
                .walletconnect
                .request_routes
                .get(&request.key)
                .cloned()
                .ok_or_else(failure)?;
            let DappRequestRoute::Gateway {
                handle,
                approval_id,
            } = &route
            else {
                return Err(failure());
            };
            let approvals = handle.approval_requests();
            let approvals = approvals.borrow();
            let ready = approvals
                .iter()
                .find(|ready| &ready.id == approval_id)
                .ok_or_else(failure)?;
            let account = store
                .resolve_dapp_session_account(
                    view_session,
                    &request.binding.public_account_uuid,
                    &request.binding.public_account_scope,
                    request.binding.owning_private_wallet_uuid.as_deref(),
                )
                .map_err(|_| failure())?;
            let WalletConnectSessionAccountResolution::Usable(account) = account else {
                return Err(failure());
            };
            if account != ready.account
                || request.item.account != account.address
                || request.item.chain_id != format!("eip155:{}", ready.chain_id)
            {
                return Err(failure());
            }
            wallet_ops::walletconnect::validate_dapp_request_account(
                &request.parsed,
                &account,
                ready.chain_id,
                walletconnect_namespace_account_support(&account, Some(view_session)),
            )
            .map_err(|error| WalletConnectSessionRequestFailure {
                kind: match error {
                    wallet_ops::walletconnect::DappRequestValidationError::UnsupportedMethod => {
                        WalletConnectRequestErrorKind::UnsupportedMethod
                    }
                    wallet_ops::walletconnect::DappRequestValidationError::AccountMismatch => {
                        WalletConnectRequestErrorKind::Unauthorized
                    }
                    _ => WalletConnectRequestErrorKind::MalformedParams,
                },
                message: "Dapp request is not supported by the current account".to_owned(),
            })?;
            self.ensure_walletconnect_chain_enabled(&request.item.chain_id)?;
            control.ensure_current().map_err(|_| failure())?;
            return Ok((request.clone(), route));
        }
        walletconnect_validate_pending_request_expiry(request.item.expiry_timestamp, now)?;
        let session_uuid = request
            .session_identity
            .walletconnect_session_id()
            .ok_or_else(|| WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Unauthorized,
                message: "Request does not belong to a WalletConnect session".to_owned(),
            })?;
        let session = store
            .load_walletconnect_session(view_session, session_uuid)
            .map_err(|error| WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Internal,
                message: format!("Could not reload WalletConnect session: {error}"),
            })?;
        let binding = DappRequestBinding::from_walletconnect_session(&session);
        let resolution = store
            .resolve_dapp_session_account(
                view_session,
                &binding.public_account_uuid,
                &binding.public_account_scope,
                binding.owning_private_wallet_uuid.as_deref(),
            )
            .map_err(|error| WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Internal,
                message: format!("Could not resolve WalletConnect Public account: {error}"),
            })?;
        let account_source = match &resolution {
            WalletConnectSessionAccountResolution::Usable(account) => account.source,
            WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet {
                ..
            } => {
                return Err(WalletConnectSessionRequestFailure {
                    kind: WalletConnectRequestErrorKind::Unauthorized,
                    message:
                        "WalletConnect session is paused for a different selected Private wallet"
                            .to_owned(),
                });
            }
            WalletConnectSessionAccountResolution::InvalidPublicAccount => {
                return Err(WalletConnectSessionRequestFailure {
                    kind: WalletConnectRequestErrorKind::Unauthorized,
                    message: "WalletConnect session Public account is invalid".to_owned(),
                });
            }
        };
        let selected_account_support = match &resolution {
            WalletConnectSessionAccountResolution::Usable(account) => {
                walletconnect_namespace_account_support(account, Some(view_session))
            }
            WalletConnectSessionAccountResolution::TemporarilyPausedWrongPrivateWallet {
                ..
            }
            | WalletConnectSessionAccountResolution::InvalidPublicAccount => {
                WalletConnectNamespaceAccountSupport::for_account_source(
                    PublicAccountSource::Derived,
                )
            }
        };
        let validation = validate_walletconnect_session_request_with_account_support(
            &session,
            &resolution,
            selected_account_support,
            &request.item.topic,
            request.item.id,
            &request.item.chain_id,
            request.parsed.clone(),
            None,
            now,
        )
        .map_err(|error| walletconnect_session_request_failure_from_error(&error))?;
        self.ensure_walletconnect_chain_enabled(&request.item.chain_id)?;
        if let WalletConnectParsedRequest::WalletSwitchEthereumChain { chain_id } =
            &validation.request
        {
            self.ensure_walletconnect_chain_enabled(&format!("eip155:{chain_id}"))?;
        }
        let Some(mut item) = validation.approval_item else {
            return Err(WalletConnectSessionRequestFailure {
                kind: WalletConnectRequestErrorKind::Internal,
                message: "WalletConnect request no longer requires approval".to_owned(),
            });
        };
        item.expiry_timestamp = request.item.expiry_timestamp;
        let route = WalletConnectRequestRoute::from_session(&session);
        Ok((
            WalletConnectRequestUi {
                request_control: None,
                rpc_reads: None,
                key: request.key.clone(),
                review_token: request.review_token,
                binding,
                session_identity: request.session_identity.clone(),
                parsed: request.parsed.clone(),
                item,
                account_source,
            },
            route,
        ))
    }

    pub(in crate::root::walletconnect) fn publish_invalid_walletconnect_pending_request(
        &mut self,
        request_key: &str,
        failure: &WalletConnectSessionRequestFailure,
        response_sender: DappResponseSender,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let response = Err(DappRequestError {
            provider_failure: None,
            kind: failure.kind,
            message: failure.message.clone(),
        });
        let request_key = request_key.to_owned();
        self.walletconnect
            .request_actions
            .insert(request_key.clone());
        tracing::warn!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(&request_key),
            error = %failure.message,
            "walletconnect pending request failed revalidation"
        );
        let join = self.runtime.spawn(async move {
            response_sender
                .begin_approval()
                .await
                .map_err(|_| "Dapp approval is unavailable".to_owned())?;
            response_sender.send(response).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.request_actions.remove(&request_key);
                root.walletconnect.remove_pending_request(&request_key);
                if root.walletconnect.request_dialog_key.as_deref() == Some(request_key.as_str()) {
                    root.clear_walletconnect_request_dialog_state(window, cx);
                    window.close_dialog(cx);
                }
                root.walletconnect.status = Some(Arc::from(
                    "WalletConnect request is no longer valid; error response published.",
                ));
                if let Ok(Err(error)) = result {
                    root.walletconnect.error = Some(Arc::from(format!(
                        "Request was removed locally, but relay error response failed: {error}"
                    )));
                }
                root.sync_walletconnect_attention();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root::walletconnect) fn reject_walletconnect_request(
        &mut self,
        request_key: &str,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.walletconnect.request_actions.contains(request_key) {
            return;
        }
        let Some(request) = self
            .walletconnect
            .pending_requests
            .get(request_key)
            .cloned()
        else {
            return;
        };
        let response_sender = match self.walletconnect_response_sender(&request, cx) {
            Ok(context) => context,
            Err(error) => {
                self.walletconnect.error = Some(error);
                cx.notify();
                return;
            }
        };
        let response = Err(DappRequestError {
            provider_failure: None,
            kind: WalletConnectRequestErrorKind::UserRejected,
            message: "User rejected WalletConnect request".to_owned(),
        });
        let native = request.request_control.is_some();
        let request_key = request_key.to_owned();
        self.walletconnect
            .request_actions
            .insert(request_key.clone());
        tracing::info!(
            target: "wallet::root::walletconnect",
            request_key = %walletconnect_request_key_log_label(&request_key),
            method = request.item.method.as_str(),
            chain_id = request.item.chain_id.as_str(),
            dapp = request.item.dapp_name.as_str(),
            "rejecting walletconnect request"
        );
        let join = self
            .runtime
            .spawn(async move { response_sender.send(response).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                root.walletconnect.request_actions.remove(&request_key);
                tracing::info!(
                    target: "wallet::root::walletconnect",
                    request_key = %walletconnect_request_key_log_label(&request_key),
                    relay_failed = matches!(&result, Ok(Err(_))),
                    relay_not_sent = matches!(&result, Ok(Err(error)) if !native && walletconnect_relay_request_was_not_sent(error)),
                    "walletconnect request rejection handled"
                );
                match result {
                    Ok(Ok(())) => {
                        root.walletconnect.remove_pending_request(&request_key);
                        if root.walletconnect.request_dialog_key.as_deref()
                            == Some(request_key.as_str())
                        {
                            root.clear_walletconnect_request_dialog_state(window, cx);
                            window.close_dialog(cx);
                        }
                        root.walletconnect.status = Some(Arc::from("WalletConnect request rejected."));
                    }
                    Ok(Err(error)) if !native && walletconnect_relay_request_was_not_sent(&error) => {
                        root.walletconnect.status = Some(Arc::from(
                            "WalletConnect relay is reconnecting; rejection was not sent. The request remains pending so you can retry.",
                        ));
                        root.walletconnect.error = Some(Arc::from(error));
                    }
                    Ok(Err(error)) => {
                        root.walletconnect.remove_pending_request(&request_key);
                        if root.walletconnect.request_dialog_key.as_deref()
                            == Some(request_key.as_str())
                        {
                            root.clear_walletconnect_request_dialog_state(window, cx);
                            window.close_dialog(cx);
                        }
                        root.walletconnect.status = Some(Arc::from("WalletConnect request rejected."));
                        root.walletconnect.error = Some(Arc::from(format!(
                            "Request was removed locally, but relay rejection failed: {error}"
                        )));
                    }
                    Err(error) => {
                        root.walletconnect.status = Some(Arc::from(
                            "WalletConnect rejection task failed. The request remains pending so you can retry.",
                        ));
                        root.walletconnect.error = Some(Arc::from(format!(
                            "WalletConnect rejection task failed: {error}"
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
}

fn append_walletconnect_simulation_risk(
    request_key: &str,
    fee_state: Option<&WalletConnectFeeState>,
    risks: &mut Vec<WalletConnectRisk>,
) {
    if let Some(state) = fee_state.filter(|state| state.request_key.as_ref() == request_key)
        && let Some(risk) = walletconnect_simulation_risk(&state.status, state.error.as_deref())
    {
        risks.push(risk);
    }
}

fn render_walletconnect_intent_risk(
    request_key: &str,
    risk_index: usize,
    risk: &WalletConnectRisk,
) -> impl IntoElement {
    let message = match risk {
        WalletConnectRisk::UnlimitedAllowance { spender } => format!(
            "Unlimited allowance: {} can keep spending this token until the allowance is revoked.",
            spender.to_checksum(None)
        ),
        WalletConnectRisk::ForeignTransferSource { source } => format!(
            "This transferFrom call attempts to move funds from {}.",
            source.to_checksum(None)
        ),
        WalletConnectRisk::AttachedNativeValue(amount) => format!(
            "This token operation also sends {} to the contract.",
            amount.display
        ),
        WalletConnectRisk::UndecodedContractCall { selector } => format!(
            "This contract call could not be decoded{}. Inspect the raw request before approving.",
            selector.map_or_else(String::new, |selector| {
                format!(" (selector 0x{})", alloy::hex::encode(selector))
            })
        ),
        WalletConnectRisk::WouldRevert { reason } => format!("Would revert: {reason}"),
    };
    let id = SharedString::from(format!(
        "walletconnect-request-risk-{request_key}-{risk_index}"
    ));
    match risk {
        WalletConnectRisk::UnlimitedAllowance { .. } | WalletConnectRisk::WouldRevert { .. } => {
            Alert::error(id, message).small()
        }
        WalletConnectRisk::ForeignTransferSource { .. }
        | WalletConnectRisk::AttachedNativeValue(..)
        | WalletConnectRisk::UndecodedContractCall { .. } => Alert::warning(id, message).small(),
    }
}

fn render_walletconnect_intent_card(
    request_key: &str,
    intent: &WalletConnectIntentView<'_>,
    content_width: Pixels,
) -> gpui::Div {
    let mut card = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_ELEVATED))
        .p(px(12.0))
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    app_strong_text(intent.hero.verb)
                        .text_size(px(18.0))
                        .whitespace_normal(),
                )
                .when(
                    intent.action != WalletConnectIntentAction::ChainSwitch,
                    |this| {
                        this.child(walletconnect_approved_chain_chip(
                            &approved_chain_display_item(&format!("eip155:{}", intent.chain_id)),
                        ))
                    },
                ),
        )
        .when_some(render_walletconnect_intent_hero(intent), |this, hero| {
            this.child(hero)
        });

    if walletconnect_should_render_token_contract(intent.action)
        && let Some(token) = walletconnect_intent_token_contract(&intent.amount)
    {
        card = card.child(render_walletconnect_address_row(
            "Token contract",
            None,
            token,
            Some(walletconnect_token_contract_recognition(&intent.amount)),
            request_key,
            content_width,
        ));
    }

    let party_count = intent.parties.len();
    let mut party_flow = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_3()
        .pt(px(6.0));
    for (index, party) in intent.parties.iter().enumerate() {
        party_flow = party_flow.child(render_walletconnect_party_row(
            party,
            request_key,
            content_width,
        ));
        if intent.action.allows_party_connector() && index + 1 < party_count {
            party_flow = party_flow.child(
                div()
                    .w_full()
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(gpui_component::Icon::new(IconName::ArrowDown)),
            );
        }
    }
    if party_count > 0 {
        card = card.child(party_flow);
    }
    if let Some(attached_native) = intent.attached_native.as_ref()
        && !matches!(
            &intent.hero.summary,
            WalletConnectHeroSummary::UndecodedCall { .. }
        )
    {
        card = card.child(
            app_muted_text(format!(
                "Attached native value: {}",
                attached_native.display
            ))
            .whitespace_normal(),
        );
    }
    if let Some(effect) = intent.hero.approval_effect.as_deref() {
        card = card.child(
            Alert::new(
                SharedString::from(format!("walletconnect-approval-effect-{request_key}")),
                effect.to_owned(),
            )
            .small()
            .text_size(APP_TEXT_SIZE)
            .text_color(rgb(theme::TEXT_MUTED)),
        );
    }
    card
}

fn render_walletconnect_intent_hero(intent: &WalletConnectIntentView<'_>) -> Option<gpui::Div> {
    let mut hero = div().w_full().min_w(px(0.0)).flex().items_center().gap_3();
    if let Some(path) = intent.icon.clone() {
        hero = hero.child(img(path).size(px(34.0)).rounded_full().flex_none());
    }
    let mut body = div().min_w(px(0.0)).flex_1().flex().flex_col().gap_1();
    match &intent.hero.summary {
        WalletConnectHeroSummary::Amount => {
            let (label, danger) = walletconnect_intent_amount_label(&intent.amount);
            body = body.child(
                app_strong_text(label)
                    .text_size(px(22.0))
                    .text_color(rgb(if danger { theme::DANGER } else { theme::TEXT }))
                    .whitespace_normal(),
            );
            if let Some(context) = intent.usd_context.as_deref() {
                body = body.child(app_muted_text(walletconnect_approximate_usd_label(context)));
            }
        }
        WalletConnectHeroSummary::PersonalMessage(summary) => {
            body = body.child(app_strong_text(format!("{} bytes", summary.bytes)));
            body = body.child(app_muted_text(summary.preview.as_deref().map_or_else(
                || "Binary or control-heavy message; inspect raw request".to_owned(),
                |preview| format!("Message: {preview}"),
            )));
        }
        WalletConnectHeroSummary::TypedData(summary) => {
            if matches!(intent.action, WalletConnectIntentAction::Approve) {
                let (label, danger) = walletconnect_intent_amount_label(&intent.amount);
                body = body.child(
                    app_strong_text(label)
                        .text_size(px(22.0))
                        .text_color(rgb(if danger { theme::DANGER } else { theme::TEXT }))
                        .whitespace_normal(),
                );
                if let Some(context) = intent.usd_context.as_deref() {
                    body = body.child(app_muted_text(walletconnect_approximate_usd_label(context)));
                }
            }
            body = body.child(app_strong_text(
                summary
                    .domain_name
                    .as_deref()
                    .unwrap_or("No domain name supplied")
                    .to_owned(),
            ));
            body = body.child(app_muted_text(format!(
                "Primary type: {}",
                summary.primary_type
            )));
        }
        WalletConnectHeroSummary::UndecodedCall { .. } => {
            let attached_native = intent.attached_native.as_ref()?;
            body = body.child(
                app_strong_text(attached_native.display.clone())
                    .text_size(px(22.0))
                    .whitespace_normal(),
            );
            if let Some(usd) = attached_native.usd.as_deref() {
                body = body.child(app_muted_text(walletconnect_approximate_usd_label(usd)));
            }
        }
        WalletConnectHeroSummary::Policy(detail) => {
            body = body.child(app_muted_text(detail.clone()));
        }
        WalletConnectHeroSummary::ChainSwitch {
            from_chain_id,
            to_chain_id,
        } => {
            body = body.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(walletconnect_approved_chain_chip(
                        &approved_chain_display_item(&format!("eip155:{from_chain_id}")),
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(app_muted_text("→"))
                            .child(walletconnect_approved_chain_chip(
                                &approved_chain_display_item(&format!("eip155:{to_chain_id}")),
                            )),
                    ),
            );
        }
        WalletConnectHeroSummary::None => {
            body = body.child(app_strong_text("Review request details"));
        }
    }
    Some(hero.child(body))
}

fn walletconnect_intent_amount_label(amount: &WalletConnectAmount) -> (String, bool) {
    match amount {
        WalletConnectAmount::KnownToken { display, .. }
        | WalletConnectAmount::Native { display, .. }
        | WalletConnectAmount::Unlimited { display, .. }
        | WalletConnectAmount::RawToken { display, .. } => (
            display.clone(),
            matches!(amount, &WalletConnectAmount::Unlimited { .. }),
        ),
        WalletConnectAmount::None => ("No amount specified".to_owned(), false),
    }
}

const fn walletconnect_intent_token_contract(amount: &WalletConnectAmount) -> Option<Address> {
    match amount {
        WalletConnectAmount::KnownToken { token, .. }
        | WalletConnectAmount::Unlimited { token, .. }
        | WalletConnectAmount::RawToken { token, .. } => Some(*token),
        WalletConnectAmount::Native { .. } | WalletConnectAmount::None => None,
    }
}

fn render_walletconnect_party_row(
    party: &WalletConnectParty,
    request_key: &str,
    content_width: Pixels,
) -> gpui::Div {
    let badge = walletconnect_party_badge_label(party.role, &party.badge);
    render_walletconnect_address_row(
        walletconnect_party_role_label(party.role),
        Some(party.role),
        party.address,
        badge.as_deref(),
        request_key,
        content_width,
    )
}

fn render_walletconnect_address_row(
    role: &'static str,
    party_role: Option<WalletConnectPartyRole>,
    address: Address,
    badge: Option<&str>,
    request_key: &str,
    content_width: Pixels,
) -> gpui::Div {
    let full_address = address.to_checksum(None);
    let copy_id = SharedString::from(format!("walletconnect-request-{request_key}-{role}-copy"));
    let body = div()
        .min_w(px(0.0))
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            app_muted_text(role)
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::TEXT_SUBTLE)),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    app_strong_text(walletconnect_party_address_label(
                        party_role.unwrap_or(WalletConnectPartyRole::Contract),
                        &address,
                        content_width,
                    ))
                    .font_family(APP_MONO_FONT_FAMILY),
                )
                .when_some(badge, |this, badge| {
                    this.child(walletconnect_party_badge_chip(badge))
                })
                .child(clipboard_with_toast(copy_id, full_address)),
        );
    div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .items_start()
        .gap_2()
        .child(body)
}

fn walletconnect_party_badge_chip(label: &str) -> gpui::Div {
    div()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::BORDER_SUBTLE))
        .bg(rgb(theme::SURFACE_HOVER_SUBTLE))
        .px(px(5.0))
        .py(px(2.0))
        .text_size(px(11.0))
        .text_color(rgb(theme::TEXT_MUTED))
        .whitespace_normal()
        .child(SharedString::from(label.to_owned()))
}

fn render_walletconnect_request_provenance(
    request: &WalletConnectRequestUi,
    intent: &WalletConnectIntentView<'_>,
    public_accounts: &[PublicAccountMetadata],
) -> gpui::Div {
    let (expiry, expiry_color) = match walletconnect_request_expiry_status(
        request.item.expiry_timestamp,
        current_unix_seconds(),
    ) {
        WalletConnectRequestExpiryStatus::Missing => {
            ("No request expiry supplied".to_owned(), theme::TEXT)
        }
        WalletConnectRequestExpiryStatus::Remaining(seconds) => (
            format!("Expires in {}", walletconnect_duration_label(seconds)),
            if seconds < 30 {
                theme::WARNING
            } else {
                theme::TEXT
            },
        ),
        WalletConnectRequestExpiryStatus::Expired => {
            ("Expired at deadline".to_owned(), theme::TEXT)
        }
    };
    div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .px(px(2.0))
        .child(walletconnect_kv_row(
            "Site",
            if request.request_control.is_some() {
                request.binding.peer_url.clone()
            } else {
                intent.provenance.site.clone()
            },
        ))
        .when(request.request_control.is_none(), |this| {
            this.when_some(intent.provenance.dapp_name.as_ref(), |this, name| {
                this.child(walletconnect_provenance_dapp_row(name))
            })
        })
        .when(
            walletconnect_selected_account_provenance_visible(
                request.item.account,
                &intent.parties,
            ),
            |this| {
                this.child(walletconnect_kv_row(
                    "Public account",
                    walletconnect_public_account_provenance_label(
                        request.item.account,
                        public_accounts,
                    ),
                ))
            },
        )
        .child(walletconnect_kv_element_row(
            "Request expiry",
            div()
                .min_w(px(0.0))
                .text_align(gpui::TextAlign::Right)
                .text_color(rgb(expiry_color))
                .whitespace_normal()
                .child(SharedString::from(expiry)),
        ))
}

fn walletconnect_public_account_provenance_label(
    selected_account: Address,
    public_accounts: &[PublicAccountMetadata],
) -> String {
    public_accounts
        .iter()
        .find(|account| account.address == selected_account)
        .and_then(|account| {
            account
                .label
                .as_deref()
                .filter(|label| !label.trim().is_empty())
                .map(|_| public_account_walletconnect_label(account))
        })
        .unwrap_or_else(|| short_address(&selected_account))
}

fn walletconnect_provenance_dapp_row(name: &str) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_start()
        .justify_between()
        .gap_3()
        .text_size(APP_TEXT_SIZE)
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::TEXT_MUTED))
                .child("Dapp"),
        )
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .text_align(gpui::TextAlign::Right)
                .truncate()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(name.to_owned())),
        )
}

fn walletconnect_duration_label(seconds: u64) -> String {
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds:02}s")
    }
}

fn render_walletconnect_request_disclosure(
    root: &Entity<WalletRoot>,
    request_key: &str,
    disclosure: WalletConnectRequestDisclosure,
    label: &'static str,
    open: bool,
    content: gpui::Div,
) -> Collapsible {
    let toggle_root = root.clone();
    let toggle_key = request_key.to_owned();
    let id = format!("walletconnect-request-{request_key}-{label}-disclosure");
    let toggle = app_button_base(SharedString::from(id))
        .text()
        .small()
        .compact()
        .icon(if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .text_color(rgb(theme::TEXT_MUTED))
        .child(app_button_label(label))
        .on_click(move |_event, _window, cx| {
            cx.stop_propagation();
            toggle_root.update(cx, |root, cx| {
                root.walletconnect
                    .toggle_request_disclosure(&toggle_key, disclosure);
                cx.notify();
            });
        });
    Collapsible::new()
        .open(open)
        .w_full()
        .child(
            div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(toggle),
        )
        .content(content)
}

fn render_walletconnect_transaction_details(
    details: &super::super::intent::WalletConnectTransactionDetails<'_>,
    request_key: &str,
    content_width: Pixels,
) -> gpui::Div {
    let transaction = details.transaction;
    let mut content = div().w_full().min_w(px(0.0)).flex().flex_col().gap_1();
    if let Some(target) = transaction.to {
        content = content.child(render_walletconnect_address_row(
            "Target",
            None,
            target,
            Some("Contract target"),
            request_key,
            content_width,
        ));
    } else {
        content = content.child(walletconnect_kv_row(
            "Target",
            "Contract creation".to_owned(),
        ));
    }
    content = content
        .child(walletconnect_kv_row(
            "Chain ID",
            details.chain_id.to_string(),
        ))
        .child(walletconnect_kv_row(
            "Native value",
            transaction
                .value
                .map_or_else(|| "0".to_owned(), |value| value.to_string()),
        ))
        .child(walletconnect_kv_row(
            "Calldata selector",
            walletconnect_transaction_selector(transaction).map_or_else(
                || "None".to_owned(),
                |selector| format!("0x{}", alloy::hex::encode(selector)),
            ),
        ));
    content = content.child(walletconnect_kv_row(
        "Calldata",
        walletconnect_calldata_context(transaction),
    ));
    if let Some(value) = transaction.gas {
        content = content.child(walletconnect_kv_row(
            "Dapp gas (not used)",
            value.to_string(),
        ));
    }
    if let Some(value) = transaction.gas_price {
        content = content.child(walletconnect_kv_row(
            "Dapp gas price (not used)",
            walletconnect_dapp_gas_price(value),
        ));
    }
    if let Some(value) = transaction.max_fee_per_gas {
        content = content.child(walletconnect_kv_row(
            "Dapp max fee (not used)",
            walletconnect_dapp_gas_price(value),
        ));
    }
    if let Some(value) = transaction.max_priority_fee_per_gas {
        content = content.child(walletconnect_kv_row(
            "Dapp max tip (not used)",
            walletconnect_dapp_gas_price(value),
        ));
    }
    if let Some(value) = transaction.nonce {
        content = content.child(walletconnect_kv_row(
            "Dapp nonce (not used)",
            value.to_string(),
        ));
    }
    if let Some(value) = transaction.transaction_type {
        content = content.child(walletconnect_kv_row(
            "Dapp transaction type (not used)",
            value.to_string(),
        ));
    }
    walletconnect_disclosure_content(content)
}

fn render_walletconnect_network_fee(
    root: &Entity<WalletRoot>,
    request: &WalletConnectRequestUi,
    fee_state: Option<&super::super::fee::WalletConnectFeeState>,
    refreshing: bool,
    open: bool,
    fee_editor: &Eip1559GasFeeEditorState,
    significance: Option<super::super::intent::WalletConnectFeeSignificance>,
) -> gpui::Div {
    let request_key = request.key.clone();
    let status = fee_state.map(|state| state.status);
    let error = fee_state
        .and_then(|state| state.error.as_ref())
        .map(ToString::to_string);
    let mut row = div().w_full().min_w(px(0.0)).flex().flex_col().gap_1();
    let mut value = div()
        .min_w(px(0.0))
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_2()
        .text_align(gpui::TextAlign::Right)
        .text_size(APP_TEXT_SIZE)
        .line_height(relative(1.0));
    let simulation_disclosure = status == Some(WalletConnectFeeStatus::AwaitingSimulation);
    let simulation_in_flight = fee_state.is_some_and(|state| state.simulation_requested);
    if let Some(projection) =
        fee_state.and_then(|state| walletconnect_fee_state_projection(state, refreshing))
    {
        value = value.child(walletconnect_fee_cost_value(
            parse_display_chain_id(&request.item.chain_id),
            projection.expected_gas_cost,
            projection.expected_native_usd_micro_value,
            significance,
            false,
        ));
    } else {
        match status {
            Some(WalletConnectFeeStatus::WouldRevert) => {
                value = value.child(
                    div()
                        .text_size(APP_TEXT_SIZE)
                        .line_height(relative(1.0))
                        .text_color(rgb(theme::DANGER))
                        .child("Would revert"),
                );
            }
            Some(WalletConnectFeeStatus::AwaitingSimulation) => {}
            Some(WalletConnectFeeStatus::UnavailableFailed) => {
                value = value.child(app_muted_text("Unavailable"));
            }
            Some(WalletConnectFeeStatus::Fetching) | None => {
                value = value.child(app_muted_text("Fetching fee data..."));
            }
            Some(
                WalletConnectFeeStatus::EstimatedFromOperation(_)
                | WalletConnectFeeStatus::Simulated(_),
            ) => unreachable!("typed fee projections are handled above"),
        }
    }
    let can_simulate = walletconnect_request_can_simulate(request)
        && !matches!(
            status,
            Some(WalletConnectFeeStatus::Simulated(_) | WalletConnectFeeStatus::WouldRevert)
        );
    let toggle_root = root.clone();
    let toggle_key = request_key.clone();
    let toggle = app_button_base(SharedString::from(format!(
        "walletconnect-network-fee-toggle-{request_key}"
    )))
    .text()
    .small()
    .compact()
    .icon(if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .text_color(rgb(theme::TEXT_MUTED))
    .child(app_button_label(WALLETCONNECT_NETWORK_FEE_LABEL))
    .on_click(move |_event, _window, cx| {
        cx.stop_propagation();
        toggle_root.update(cx, |root, cx| {
            root.walletconnect
                .toggle_request_disclosure(&toggle_key, WalletConnectRequestDisclosure::NetworkFee);
            cx.notify();
        });
    });
    let mut right_group = div()
        .min_w(px(0.0))
        .flex_1()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_2()
        .child(value);
    let simulation_action_visible = can_simulate
        && (simulation_disclosure || fee_state.is_none_or(|state| !state.simulation_retryable));
    if simulation_action_visible {
        let simulate_root = root.clone();
        let key = request_key.clone();
        right_group = right_group.child(
            app_button(
                SharedString::from(format!("walletconnect-simulate-{request_key}")),
                "Simulate on network",
            )
            .outline()
            .small()
            .loading(simulation_in_flight)
            .child(gpui_component::Icon::new(IconName::Info).with_size(px(14.0)))
            .tooltip("Simulation sends transaction details to the configured RPC.")
            .disabled(simulation_in_flight)
            .on_click(move |_event, _window, cx| {
                cx.stop_propagation();
                simulate_root.update(cx, |root, cx| {
                    root.simulate_current_walletconnect_transaction(&key, cx);
                });
            }),
        );
    }
    if matches!(status, Some(WalletConnectFeeStatus::UnavailableFailed)) {
        let retry_root = root.clone();
        let key = Arc::<str>::from(request_key.as_str());
        let retrying = fee_state.is_some_and(|state| {
            walletconnect_fee_retrying(state.retry_attempt, refreshing, state.simulation_requested)
        });
        let retry_blocked = !fee_state.is_some_and(|state| {
            walletconnect_fee_retry_action_enabled(
                state.status,
                refreshing,
                state.simulation_requested,
            )
        });
        right_group = right_group.child(
            app_button(
                SharedString::from(format!("walletconnect-retry-fee-{request_key}")),
                "Retry",
            )
            .outline()
            .small()
            .loading(retrying)
            .disabled(retry_blocked)
            .on_click(move |_event, _window, cx| {
                cx.stop_propagation();
                let key = Arc::clone(&key);
                retry_root.update(cx, |root, cx| {
                    root.retry_walletconnect_fee(key.as_ref(), cx);
                });
            }),
        );
    }
    let simulation_failed = fee_state.is_some_and(|state| {
        matches!(state.status, WalletConnectFeeStatus::UnavailableFailed)
            && state.simulation_retryable
    });
    row = row.child(
        Collapsible::new()
            .open(open)
            .w_full()
            .child(
                div()
                    .w_full()
                    .min_w(px(0.0))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(toggle)
                    .child(right_group),
            )
            .content(render_walletconnect_fee_details(
                root.clone(),
                request,
                fee_state,
                refreshing,
                fee_editor,
                significance,
            )),
    );
    if simulation_failed && let Some(error) = error.filter(|error| !error.is_empty()) {
        row = row.child(
            Alert::error(
                SharedString::from(format!("walletconnect-simulation-error-{request_key}")),
                error,
            )
            .small(),
        );
    }
    if let Some(message) =
        significance.and_then(super::super::intent::WalletConnectFeeSignificance::warning_message)
    {
        row = row.child(
            Alert::warning(
                SharedString::from(format!("walletconnect-fee-significance-{request_key}")),
                message,
            )
            .small(),
        );
    }
    row
}

fn render_walletconnect_fee_details(
    root: Entity<WalletRoot>,
    request: &WalletConnectRequestUi,
    fee_state: Option<&super::super::fee::WalletConnectFeeState>,
    refreshing: bool,
    fee_editor: &Eip1559GasFeeEditorState,
    significance: Option<super::super::intent::WalletConnectFeeSignificance>,
) -> gpui::Div {
    let chain_id = parse_display_chain_id(&request.item.chain_id);
    let raw_gas_limit = walletconnect_request_raw_gas(request);
    let mut details = div().w_full().min_w(px(0.0)).flex().flex_col().gap_1();
    if let Some(projection) =
        fee_state.and_then(|state| walletconnect_fee_state_projection(state, refreshing))
    {
        details = details
            .child(walletconnect_kv_element_row(
                "Gas usage",
                app_muted_text(format_gas_limit(projection.raw_gas_limit)),
            ))
            .child(walletconnect_kv_element_row(
                "Expected cost",
                walletconnect_fee_cost_value(
                    chain_id,
                    projection.expected_gas_cost,
                    projection.expected_native_usd_micro_value,
                    significance,
                    false,
                ),
            ))
            .when(
                wallet_ops::public_action_maximum_gas_cost_is_significant(
                    projection.expected_gas_cost,
                    projection.maximum_gas_cost,
                ),
                |this| {
                    this.child(walletconnect_kv_element_row(
                        "Maximum cost",
                        walletconnect_fee_cost_value(
                            chain_id,
                            projection.maximum_gas_cost,
                            projection.maximum_native_usd_micro_value,
                            None,
                            true,
                        ),
                    ))
                },
            );
    } else {
        details = details
            .child(walletconnect_kv_element_row(
                "Gas usage",
                app_muted_text(
                    raw_gas_limit
                        .map_or_else(|| "Requires simulation".to_owned(), format_gas_limit),
                ),
            ))
            .child(walletconnect_kv_element_row(
                "Expected cost",
                app_muted_text("Needs gas usage"),
            ));
    }
    details = details.child(div().pt(px(6.0)).child(render_eip1559_gas_fee_editor(
        root,
        &Eip1559GasFeeTarget::WalletConnect {
            request_key: Arc::from(request.key.as_str()),
        },
        fee_editor,
        false,
    )));
    walletconnect_disclosure_content(details)
}

fn walletconnect_dapp_gas_price(value: U256) -> String {
    if value <= U256::from(u128::MAX) {
        format!("{} gwei (not used)", format_gwei(value.to::<u128>()))
    } else {
        format!("{value} wei (not used)")
    }
}

fn walletconnect_fee_cost_value(
    chain_id: u64,
    cost: alloy::primitives::U256,
    usd: Option<alloy::primitives::U256>,
    significance: Option<super::super::intent::WalletConnectFeeSignificance>,
    muted_all: bool,
) -> gpui::Div {
    let native = format_native_token_amount_for_display(chain_id, cost);
    let warning = significance.is_some_and(|significance| {
        matches!(
            significance,
            super::super::intent::WalletConnectFeeSignificance::High
                | super::super::intent::WalletConnectFeeSignificance::ExceedsAmount
        )
    });
    let usd_color = if muted_all {
        theme::TEXT_MUTED
    } else if warning {
        theme::WARNING
    } else {
        theme::TEXT
    };
    let native_color = if muted_all || warning {
        if warning {
            theme::WARNING
        } else {
            theme::TEXT_MUTED
        }
    } else {
        theme::TEXT_MUTED
    };
    let mut value = div()
        .min_w(px(0.0))
        .flex()
        .flex_wrap()
        .justify_end()
        .text_align(gpui::TextAlign::Right)
        .text_size(APP_TEXT_SIZE)
        .line_height(relative(1.0))
        .whitespace_normal();
    if let Some(usd) = usd {
        value = value
            .child(
                div()
                    .text_color(rgb(usd_color))
                    .child(SharedString::from(format!(
                        "≈ {}",
                        format_usd_micro_value(usd)
                    ))),
            )
            .child(
                div()
                    .text_color(rgb(native_color))
                    .child(SharedString::from(format!(" · {native}"))),
            );
    } else {
        value = value.child(
            div()
                .text_color(rgb(usd_color))
                .child(SharedString::from(native)),
        );
    }
    value
}

fn parse_display_chain_id(value: &str) -> u64 {
    parse_caip2_chain_id(value).unwrap_or_default()
}

fn walletconnect_calldata_context(transaction: &WalletConnectEvmTransaction) -> String {
    let Some(data) = transaction.data.as_deref() else {
        return "None supplied".to_owned();
    };
    let data = data.strip_prefix("0x").unwrap_or(data);
    alloy::hex::decode(data).map_or_else(
        |_| "Supplied (invalid encoding)".to_owned(),
        |bytes| format!("{} bytes supplied", bytes.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use std::sync::Arc;

    fn fee_state(
        request_key: &str,
        status: WalletConnectFeeStatus,
        error: Option<&str>,
    ) -> WalletConnectFeeState {
        WalletConnectFeeState {
            request_key: Arc::from(request_key),
            review_token: 0,
            payload_fingerprint: B256::ZERO,
            request_generation: 0,
            dialog_generation: 0,
            editor_generation: 0,
            expiry_timestamp: None,
            status,
            error: error.map(Arc::from),
            retry_attempt: 0,
            generation: 0,
            simulation_requested: false,
            simulation_retryable: false,
            last_successful_display_projection: None,
        }
    }

    #[test]
    fn dapp_gas_price_formats_safe_values_as_gwei_without_truncating_large_values() {
        assert_eq!(
            walletconnect_dapp_gas_price(U256::from(1_500_000_000_u64)),
            "1.5 gwei (not used)"
        );
        let oversized = U256::from(u128::MAX) + U256::from(1_u8);
        assert_eq!(
            walletconnect_dapp_gas_price(oversized),
            format!("{oversized} wei (not used)")
        );
    }

    #[test]
    fn footer_disables_approve_for_expiry_or_missing_intent_context() {
        let expired =
            walletconnect_request_footer_action_state(false, true, true, false, false, false);
        assert!(expired.reject_enabled);
        assert!(!expired.approve_enabled);

        let missing_context =
            walletconnect_request_footer_action_state(false, false, false, false, false, false);
        assert!(missing_context.reject_enabled);
        assert!(!missing_context.approve_enabled);
    }

    #[test]
    fn footer_disables_both_actions_while_request_is_in_flight() {
        let state =
            walletconnect_request_footer_action_state(true, false, true, true, false, false);

        assert!(!state.reject_enabled);
        assert!(!state.approve_enabled);
        assert!(!state.approve_danger);
    }

    #[test]
    fn request_scoped_fee_risk_enrichment_adds_revert_but_not_unavailable() {
        let revert = fee_state(
            "request-key",
            WalletConnectFeeStatus::WouldRevert,
            Some("execution reverted"),
        );
        let mut risks = Vec::new();
        append_walletconnect_simulation_risk("request-key", Some(&revert), &mut risks);
        assert_eq!(
            risks,
            vec![WalletConnectRisk::WouldRevert {
                reason: "execution reverted".to_owned(),
            }]
        );

        let other_request = fee_state(
            "other-request-key",
            WalletConnectFeeStatus::WouldRevert,
            Some("execution reverted"),
        );
        risks.clear();
        append_walletconnect_simulation_risk("request-key", Some(&other_request), &mut risks);
        assert!(risks.is_empty());

        let unavailable = fee_state(
            "request-key",
            WalletConnectFeeStatus::UnavailableFailed,
            Some("provider unavailable"),
        );
        risks.clear();
        append_walletconnect_simulation_risk("request-key", Some(&unavailable), &mut risks);
        assert!(risks.is_empty());
    }
}
