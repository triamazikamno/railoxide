use super::*;

impl WalletRoot {
    pub(in crate::root::walletconnect) fn render_walletconnect_request(
        &self,
        root: &Entity<Self>,
        request: &WalletConnectRequestUi,
        content_width: Pixels,
    ) -> gpui::Div {
        if let WalletConnectParsedRequest::WalletAddEthereumChain {
            chain_id,
            definition,
            ..
        } = &request.parsed
        {
            let mut content = div()
                .w_full()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap_2()
                .child(walletconnect_kv_row(
                    "Site",
                    request.binding.peer_url.clone(),
                ))
                .child(
                    Alert::info(
                        "gateway-chain-add-account-access",
                        "This confirmation grants no account access.",
                    )
                    .small(),
                );
            if let Some(chain) = definition {
                content = content
                    .child(walletconnect_kv_row(
                        "Network",
                        format!("{} ({chain_id})", chain.name),
                    ))
                    .child(walletconnect_kv_row(
                        "Native currency",
                        format!(
                            "{} ({}, {} decimals)",
                            chain.native_currency.name,
                            chain.native_currency.symbol,
                            chain.native_currency.decimals
                        ),
                    ));
                for endpoint in &chain.rpc_endpoints {
                    if let Ok(url) = reqwest::Url::parse(&endpoint.url) {
                        content = content.child(walletconnect_kv_row(
                            "RPC destination",
                            url.origin().ascii_serialization(),
                        ));
                    }
                }
            } else {
                content = content.child(walletconnect_kv_row("Configured chain", chain_id.to_string()))
                    .child(app_muted_text("This network is already configured. This confirms that the network is available with the wallet's saved settings. Supplied network metadata is ignored."));
            }

            if let Some(error) = &self.walletconnect.error {
                content = content
                    .child(Alert::error("gateway-chain-add-error", error.to_string()).small());
            }
            return content;
        }
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
                let description = if self.last_evm_chain == *chain_id {
                    "Switch this website to your wallet's current network."
                } else {
                    "Switch the network for this website and your wallet."
                };
                content = content.child(app_muted_text(description));
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
                    None => content.child(app_muted_shimmer_text("Loading token metadata from the saved network...")),
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
                &self.effective_chain_configs,
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
                        &intent.native_currency,
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
        if matches!(
            request.account_source,
            Some(PublicAccountSource::HardwareDerived)
        ) && !request.parsed.desktop_policy()
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
                        &self.hardware_profile.trezor_app_passphrase_input,
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
        let (intent_context_available, unlimited_allowance) = match &request.parsed {
            WalletConnectParsedRequest::WalletAddEthereumChain { .. } => (true, false),
            _ => match self.walletconnect_intent_context(request) {
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
            },
        };
        let hardware_request = request.account_source == Some(PublicAccountSource::HardwareDerived)
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

    pub(super) fn walletconnect_request_hardware_typed_data_mode(
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
                if let WalletConnectParsedRequest::WalletAddEthereumChain {
                    chain_id,
                    definition,
                    ..
                } = &request.parsed
                {
                    return Some((
                        approval_id.clone(),
                        definition.as_ref().map_or_else(
                            || format!("Confirm configured chain {chain_id}. No account access."),
                            |chain| format!("Add {} ({chain_id}). No account access.", chain.name),
                        ),
                    ));
                }
                let context = self.walletconnect_intent_context(request).ok()?;
                Some((
                    approval_id.clone(),
                    build_walletconnect_intent(request, context).authorization,
                ))
            })
            .collect()
    }

    pub(super) fn walletconnect_intent_context(
        &self,
        request: &WalletConnectRequestUi,
    ) -> Result<WalletConnectIntentContext<'_>, &'static str> {
        let chain_id = parse_caip2_chain_id(&request.item.chain_id)
            .ok_or("This request does not identify a supported EVM chain.")?;
        let chain = self
            .effective_chain_configs
            .get(chain_id)
            .ok_or("The request chain is not available in the current wallet settings.")?;
        Ok(WalletConnectIntentContext {
            chain,
            selected_chain_id: self.last_evm_chain,
            token_registry: &self.effective_token_registry,
            anchor_rates: &self.public_broadcaster_anchor_cache,
            public_accounts: &self.public_accounts,
            public_address_book: &self.public_address_book,
        })
    }
}
