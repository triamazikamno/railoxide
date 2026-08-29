use super::*;

impl WalletRoot {
    pub(in crate::root) fn open_public_action_dialog(
        &mut self,
        public_account_uuid: Arc<str>,
        asset: PublicAssetId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.set_public_selected_balance(public_account_uuid, asset, window, cx);
        self.public_form.action_mode = PublicActionMode::Shield;
        self.reset_public_action_gas_fee_quotes();
        self.clear_public_action_dialog_inputs(window, cx);
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(PUBLIC_ACTION_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let asset_label = public_asset_label(
            self.selected_chain,
            asset,
            Some(&self.effective_token_registry),
        );
        let icon_path = public_asset_icon_path(
            self.selected_chain,
            asset,
            Some(&self.effective_token_registry),
        );
        window.open_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(public_action_title_row(
                    asset_label.clone(),
                    icon_path.clone(),
                ))
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.clear_public_action_dialog_inputs(window, cx);
                    });
                })
                .child(scrollable_dialog_content(
                    content_max_height,
                    content_root.read(cx).render_public_action_dialog_content(
                        content_root.clone(),
                        content_width,
                        cx,
                    ),
                ))
        });
        self.refresh_public_action_gas_fee_quote(PublicActionMode::Shield, cx);
        cx.defer_in(window, |root, window, cx| {
            root.focus_public_action_dialog_input(window, cx);
        });
    }

    pub(in crate::root) fn focus_public_action_dialog_input(
        &self,
        window: &mut Window,
        cx: &Context<'_, Self>,
    ) {
        match self.public_form.action_mode {
            PublicActionMode::Shield => self
                .public_form
                .shield_amount_input
                .read(cx)
                .focus_handle(cx)
                .focus(window),
            PublicActionMode::Send => {
                let input = match self.public_form.public_send_kind {
                    PublicSendKind::Transfer => &self.public_form.send_recipient_input,
                    PublicSendKind::ContractCall => &self.public_form.advanced_send_to_input,
                    PublicSendKind::Deploy => &self.public_form.advanced_send_value_input,
                };
                input.read(cx).focus_handle(cx).focus(window);
            }
        }
    }

    pub(in crate::root) fn render_public_action_dialog_content(
        &self,
        root: Entity<Self>,
        content_width: Pixels,
        cx: &App,
    ) -> gpui::Div {
        let mode = self.public_form.action_mode;
        let account = self.selected_public_account();
        let selected_asset = self.public_form.selected_asset;
        let balance_entry = self.selected_public_balance_entry();
        let asset_label = selected_asset.map_or_else(
            || "selected asset".to_string(),
            |asset| {
                public_asset_label(
                    self.selected_chain,
                    asset,
                    Some(&self.effective_token_registry),
                )
            },
        );
        let disabled = account.is_none() || selected_asset.is_none();
        let submitting = self.public_form.sending || self.public_form.shielding;
        let mode_root = root.clone();
        let submit_root = root.clone();
        let gas_fee_root = root.clone();
        let progress_root = root.clone();
        let advanced_mode_root = root.clone();
        let advanced_estimate_root = root.clone();
        let advanced_submit_root = root.clone();
        let max_root = root;
        let show_form_errors = !self.public_action_has_active_progress();
        let amount_hint = format!("{asset_label} amount");
        let send_amount_hint = selected_asset.map_or_else(
            || "Amount".to_string(),
            |asset| {
                format!(
                    "Amount ({})",
                    public_action_asset_label(
                        self.selected_chain,
                        asset,
                        Some(&self.effective_token_registry),
                    )
                )
            },
        );
        let max_gas_reserve = selected_asset
            .and_then(|asset| self.public_action_estimated_gas_cost(mode, asset, cx))
            .map(|projection| projection.maximum_cost);
        let max_label = balance_entry
            .as_ref()
            .and_then(|entry| public_action_max_label(entry, max_gas_reserve));
        let fee_amount = selected_asset.and_then(|asset| {
            let input = match mode {
                PublicActionMode::Shield => &self.public_form.shield_amount_input,
                PublicActionMode::Send => &self.public_form.send_amount_input,
            };
            let decimals = public_asset_decimals(
                self.selected_chain,
                asset,
                Some(&self.effective_token_registry),
            );
            parse_send_amount(input.read(cx).value().as_ref(), decimals)
                .ok()
                .filter(|amount| !amount.is_zero())
        });
        let fee_display = self.public_action_fee_display(mode, fee_amount, cx);
        let gas_pending = match mode {
            PublicActionMode::Shield => self.public_form.shield_gas_fee.refreshing,
            PublicActionMode::Send => self.public_form.send_gas_fee.refreshing,
        };
        let fee_ready = fee_display.expected_gas_cost.is_some();
        let mut content = div()
            .w(content_width)
            .flex()
            .flex_col()
            .gap_3()
            .children(account.map(|account| {
                app_muted_text(format!("From {}", short_address(&account.address)))
                    .font_family(APP_MONO_FONT_FAMILY)
            }))
            .child(
                ButtonGroup::new("wallet-public-action-mode-toggle")
                    .w_full()
                    .outline()
                    .disabled(submitting)
                    .child(public_action_segment_button(
                        "wallet-public-action-mode-shield".into(),
                        "Shield",
                        Icon::new(RailgunActionIcon::Shield),
                        mode == PublicActionMode::Shield,
                    ))
                    .child(public_action_segment_button(
                        "wallet-public-action-mode-send".into(),
                        "Send",
                        Icon::new(RailgunActionIcon::Send),
                        mode == PublicActionMode::Send,
                    ))
                    .on_click(move |selected, window, cx| {
                        let Some(index) = selected.first() else {
                            return;
                        };
                        let mode = if *index == 0 {
                            PublicActionMode::Shield
                        } else {
                            PublicActionMode::Send
                        };
                        mode_root.update(cx, |root, cx| {
                            root.set_public_action_mode(mode, window, cx);
                        });
                    }),
            );

        match mode {
            PublicActionMode::Shield => {
                content = content
                    .child(render_public_action_amount_input(
                        max_root,
                        PublicActionMode::Shield,
                        &self.public_form.shield_amount_input,
                        amount_hint,
                        max_label,
                        disabled || self.public_form.shielding,
                    ))
                    .child(render_mimic_railway_shield_control(
                        submit_root.clone(),
                        self.public_form.mimic_railway_shield,
                        self.public_form.shielding,
                    ))
                    .child(render_eip1559_gas_fee_editor(
                        gas_fee_root,
                        &Eip1559GasFeeTarget::Public {
                            mode: PublicActionMode::Shield,
                        },
                        &self.public_form.shield_gas_fee,
                        disabled || self.public_form.shielding,
                    ))
                    .child(render_public_action_fee_estimate(&fee_display, gas_pending))
                    .child(
                        app_button(
                            "wallet-public-shield",
                            if self.public_form.shielding {
                                "Shielding..."
                            } else {
                                "Shield"
                            },
                        )
                        .primary()
                        .small()
                        .loading(self.public_form.shielding)
                        .disabled(disabled || self.public_form.shielding || !fee_ready)
                        .on_click(move |_event, window, cx| {
                            submit_root.update(cx, |root, cx| {
                                root.submit_public_shield_from_form(window, cx);
                            });
                        }),
                    );
                if show_form_errors && let Some(error) = self.public_form.shield_error.as_ref() {
                    content = content.child(
                        Alert::error("wallet-public-shield-error", error.to_string()).small(),
                    );
                }
            }
            PublicActionMode::Send => {
                let send_kind = self.public_form.public_send_kind;
                if selected_asset == Some(PublicAssetId::Native) {
                    content = content.child(
                        div().flex().justify_end().child(
                            ButtonGroup::new("wallet-public-send-kind-toggle")
                                .outline()
                                .compact()
                                .disabled(self.public_form.sending)
                                .child(public_send_kind_segment_button(
                                    "wallet-public-send-transfer".into(),
                                    "Transfer",
                                    send_kind == PublicSendKind::Transfer,
                                ))
                                .child(public_send_kind_segment_button(
                                    "wallet-public-send-contract-call".into(),
                                    "Contract call",
                                    send_kind == PublicSendKind::ContractCall,
                                ))
                                .child(public_send_kind_segment_button(
                                    "wallet-public-send-deploy".into(),
                                    "Deploy",
                                    send_kind == PublicSendKind::Deploy,
                                ))
                                .on_click(move |selected, window, cx| {
                                    let Some(index) = selected.first() else {
                                        return;
                                    };
                                    let kind = match *index {
                                        0 => PublicSendKind::Transfer,
                                        1 => PublicSendKind::ContractCall,
                                        _ => PublicSendKind::Deploy,
                                    };
                                    advanced_mode_root.update(cx, |root, cx| {
                                        root.set_public_send_kind(kind, window, cx);
                                    });
                                }),
                        ),
                    );
                }
                if send_kind.is_advanced() && selected_asset == Some(PublicAssetId::Native) {
                    if send_kind == PublicSendKind::ContractCall {
                        content = content.child(
                            labeled_field(
                                "To",
                                app_input(&self.public_form.advanced_send_to_input)
                                    .disabled(disabled || self.public_form.sending),
                            )
                            .children(
                                self.public_form
                                    .advanced_send_to_error
                                    .as_ref()
                                    .map(|error| {
                                        Alert::error(
                                            "wallet-public-send-advanced-to-error",
                                            error.to_string(),
                                        )
                                        .small()
                                    }),
                            ),
                        );
                    }
                    let data_label = if send_kind == PublicSendKind::Deploy {
                        "Init code"
                    } else {
                        "Calldata"
                    };
                    content = content
                        .child(
                            labeled_field(
                                format!(
                                    "Value ({})",
                                    native_token_display_label(self.selected_chain)
                                ),
                                app_input(&self.public_form.advanced_send_value_input)
                                    .disabled(disabled || self.public_form.sending),
                            )
                            .children(
                                self.public_form
                                    .advanced_send_value_error
                                    .as_ref()
                                    .map(|error| {
                                        Alert::error(
                                            "wallet-public-send-advanced-value-error",
                                            error.to_string(),
                                        )
                                        .small()
                                    }),
                            ),
                        )
                        .child(
                            labeled_field(
                                data_label,
                                app_input(&self.public_form.advanced_send_data_input)
                                    .w_full()
                                    .min_w(px(0.0))
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(APP_TEXT_SIZE)
                                    .disabled(disabled || self.public_form.sending),
                            )
                            .w_full()
                            .min_w(px(0.0))
                            .children(
                                self.public_form
                                    .advanced_send_data_error
                                    .as_ref()
                                    .map(|error| {
                                        Alert::error(
                                            "wallet-public-send-advanced-data-error",
                                            error.to_string(),
                                        )
                                        .small()
                                    }),
                            ),
                        )
                        .child(render_eip1559_gas_fee_editor(
                            gas_fee_root,
                            &Eip1559GasFeeTarget::Public {
                                mode: PublicActionMode::Send,
                            },
                            &self.public_form.send_gas_fee,
                            disabled || self.public_form.sending,
                        ))
                        .child(match self.public_form.advanced_send_estimate.as_ref() {
                            Some(estimate) => render_public_advanced_transaction_estimate(
                                self.selected_chain,
                                estimate,
                                self.public_broadcaster_anchor_cache
                                    .cached_native_usd_micro_value(
                                        self.selected_chain,
                                        estimate.expected_gas_cost,
                                    ),
                                self.public_broadcaster_anchor_cache
                                    .cached_native_usd_micro_value(
                                        self.selected_chain,
                                        estimate.max_gas_cost,
                                    ),
                            ),
                            None => Alert::info(
                                "wallet-public-send-advanced-estimate-required",
                                advanced_public_send_estimate_required_message(
                                    self.public_form.advanced_send_estimate_invalidated,
                                ),
                            )
                            .small()
                            .into_any_element(),
                        })
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_wrap()
                                .justify_end()
                                .gap_2()
                                .child(
                                    app_button(
                                        "wallet-public-send-advanced-estimate",
                                        if self.public_form.advanced_send_estimate_pending {
                                            "Estimating..."
                                        } else {
                                            "Estimate gas"
                                        },
                                    )
                                    .outline()
                                    .small()
                                    .loading(self.public_form.advanced_send_estimate_pending)
                                    .disabled(
                                        disabled
                                            || self.public_form.sending
                                            || self.public_form.advanced_send_estimate_pending,
                                    )
                                    .on_click(
                                        move |_event, _window, cx| {
                                            advanced_estimate_root.update(cx, |root, cx| {
                                                root.estimate_advanced_public_send(cx);
                                            });
                                        },
                                    ),
                                )
                                .child(
                                    app_button(
                                        "wallet-public-send-advanced-authorize",
                                        "Review and authorize",
                                    )
                                    .primary()
                                    .small()
                                    .disabled(
                                        disabled
                                            || self.public_form.sending
                                            || self.public_form.advanced_send_estimate_pending
                                            || self.public_form.advanced_send_estimate.is_none(),
                                    )
                                    .on_click(
                                        move |_event, window, cx| {
                                            advanced_submit_root.update(cx, |root, cx| {
                                                root.submit_public_send_from_form(window, cx);
                                            });
                                        },
                                    ),
                                ),
                        );
                } else {
                    content = content
                        .child(labeled_field(
                            "To",
                            app_input(&self.public_form.send_recipient_input)
                                .disabled(disabled || self.public_form.sending),
                        ))
                        .child(render_public_action_amount_input(
                            max_root,
                            PublicActionMode::Send,
                            &self.public_form.send_amount_input,
                            send_amount_hint,
                            max_label,
                            disabled || self.public_form.sending,
                        ))
                        .child(render_eip1559_gas_fee_editor(
                            gas_fee_root,
                            &Eip1559GasFeeTarget::Public {
                                mode: PublicActionMode::Send,
                            },
                            &self.public_form.send_gas_fee,
                            disabled || self.public_form.sending,
                        ))
                        .child(render_public_action_fee_estimate(&fee_display, gas_pending))
                        .child(
                            app_button(
                                "wallet-public-send",
                                if self.public_form.sending {
                                    "Sending..."
                                } else {
                                    "Send publicly"
                                },
                            )
                            .primary()
                            .small()
                            .loading(self.public_form.sending)
                            .disabled(disabled || self.public_form.sending || !fee_ready)
                            .on_click(move |_event, window, cx| {
                                submit_root.update(cx, |root, cx| {
                                    root.submit_public_send_from_form(window, cx);
                                });
                            }),
                        );
                }
                if show_form_errors && let Some(error) = self.public_form.send_error.as_ref() {
                    content = content
                        .child(Alert::error("wallet-public-send-error", error.to_string()).small());
                }
            }
        }

        if !self.public_form.action_progress_dialog_open
            && let Some(active_step) = public_action_closed_status_step(
                &self.public_form.action_progress,
                self.public_form.action_progress_lifecycle,
            )
        {
            content = content.child(render_public_action_active_status_notice(
                progress_root,
                self.public_form.action_progress_mode,
                self.public_form.action_progress_title_override.as_deref(),
                active_step,
                self.public_form.action_requires_device_approval,
                self.public_form.action_command_tx.is_some(),
            ));
        }

        content
    }

    pub(in crate::root) fn set_public_send_kind(
        &mut self,
        kind: PublicSendKind,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.sending
            || self.public_form.selected_asset != Some(PublicAssetId::Native)
            || self.public_form.public_send_kind == kind
        {
            return;
        }
        self.public_form.public_send_kind = kind;
        self.public_form.send_error = None;
        self.invalidate_advanced_public_send_estimate();
        cx.defer_in(window, |root, window, cx| {
            root.focus_public_action_dialog_input(window, cx);
        });
        cx.notify();
    }

    pub(in crate::root) fn clear_public_action_dialog_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        for input in [
            &self.public_form.send_recipient_input,
            &self.public_form.send_amount_input,
            &self.public_form.advanced_send_to_input,
            &self.public_form.advanced_send_value_input,
            &self.public_form.advanced_send_data_input,
            &self.public_form.shield_amount_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.clear_trezor_app_passphrase_input(window, cx);
        self.clear_trezor_pin_matrix_prompt(cx);
        self.public_form.send_error = None;
        self.public_form.public_send_kind = PublicSendKind::Transfer;
        self.public_form.advanced_send_estimate = None;
        self.public_form.advanced_send_estimate_invalidated = false;
        self.invalidate_advanced_public_send_estimate();
        self.public_form.shield_error = None;
        if !self.public_form.sending && !self.public_form.shielding {
            self.clear_public_action_progress_state();
        }
    }

    pub(in crate::root) fn invalidate_advanced_public_send_estimate(&mut self) {
        self.public_form.advanced_send_estimate_generation = self
            .public_form
            .advanced_send_estimate_generation
            .wrapping_add(1);
        if self.public_form.advanced_send_estimate.take().is_some() {
            self.public_form.advanced_send_estimate_invalidated = true;
        }
        self.public_form.advanced_send_estimate_pending = false;
        self.public_form.advanced_send_to_error = None;
        self.public_form.advanced_send_value_error = None;
        self.public_form.advanced_send_data_error = None;
    }

    pub(in crate::root) fn estimate_advanced_public_send(&mut self, cx: &mut Context<'_, Self>) {
        if self.public_form.sending
            || self.public_form.advanced_send_estimate_pending
            || !self.public_form.public_send_kind.is_advanced()
            || self.public_form.selected_asset != Some(PublicAssetId::Native)
        {
            return;
        }
        let Some(account) = self.selected_public_account() else {
            self.public_form.send_error = Some(Arc::from("Select a public account first"));
            cx.notify();
            return;
        };
        let from = account.address;
        let public_account_uuid: Arc<str> = Arc::from(account.public_account_uuid.as_str());
        let native_decimals = public_asset_decimals(
            self.selected_chain,
            PublicAssetId::Native,
            Some(&self.effective_token_registry),
        );
        let intent = match parse_advanced_public_send_intent(
            self.public_form.public_send_kind,
            self.public_form
                .advanced_send_to_input
                .read(cx)
                .value()
                .as_ref(),
            self.public_form
                .advanced_send_value_input
                .read(cx)
                .value()
                .as_ref(),
            self.public_form
                .advanced_send_data_input
                .read(cx)
                .value()
                .as_ref(),
            native_decimals,
        ) {
            Ok(intent) => intent,
            Err(error) => {
                self.set_advanced_public_send_validation_error(error);
                cx.notify();
                return;
            }
        };
        let gas_fee = match self.public_action_gas_fee_selection(PublicActionMode::Send, cx) {
            Ok(gas_fee) => gas_fee,
            Err(error) => {
                self.public_form.send_error = Some(Arc::from(error));
                cx.notify();
                return;
            }
        };
        self.invalidate_advanced_public_send_estimate();
        self.public_form.advanced_send_estimate_pending = true;
        self.public_form.send_error = None;
        let generation = self.public_form.advanced_send_estimate_generation;
        let send_kind = self.public_form.public_send_kind;
        let chain_id = self.selected_chain;
        let selected_wallet_id = self.selected_wallet_id.clone();
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let result = estimate_public_advanced_transaction(
                PublicAdvancedTransactionEstimateRequest {
                    chain_id,
                    effective_chain,
                    from,
                    intent,
                    gas_fee,
                    access_list: None,
                },
                &http,
            )
            .await;
            let _ = this.update(cx, |root, cx| {
                if root.selected_wallet_id != selected_wallet_id
                    || root.selected_chain != chain_id
                    || root.public_form.selected_account_uuid.as_deref()
                        != Some(public_account_uuid.as_ref())
                    || root.public_form.advanced_send_estimate_generation != generation
                    || root.public_form.public_send_kind != send_kind
                {
                    return;
                }
                root.public_form.advanced_send_estimate_pending = false;
                match result {
                    Ok(estimate) => {
                        root.public_form.advanced_send_estimate = Some(estimate);
                        root.public_form.advanced_send_estimate_invalidated = false;
                        root.public_form.send_error = None;
                    }
                    Err(error) => {
                        root.public_form.advanced_send_estimate = None;
                        root.public_form.send_error = Some(Arc::from(format!(
                            "Could not estimate advanced transaction: {}",
                            format_report_chain(&error)
                        )));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_advanced_public_send_validation_error(
        &mut self,
        error: AdvancedPublicSendValidationError,
    ) {
        self.public_form.advanced_send_to_error = None;
        self.public_form.advanced_send_value_error = None;
        self.public_form.advanced_send_data_error = None;
        let message = Arc::from(error.message);
        match error.field {
            AdvancedPublicSendField::Destination => {
                self.public_form.advanced_send_to_error = Some(message);
            }
            AdvancedPublicSendField::Value => {
                self.public_form.advanced_send_value_error = Some(message);
            }
            AdvancedPublicSendField::Data => {
                self.public_form.advanced_send_data_error = Some(message);
            }
        }
        self.public_form.send_error = None;
    }

    fn reset_public_action_gas_fee_quotes(&mut self) {
        for gas_fee in [
            &mut self.public_form.send_gas_fee,
            &mut self.public_form.shield_gas_fee,
        ] {
            gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
            gas_fee.quote = None;
            gas_fee.refreshing = false;
            gas_fee.error = None;
            gas_fee.quote_error = None;
        }
        self.public_form.shield_gas_fee_authorization_ceiling = None;
    }

    pub(in crate::root) fn invalidate_public_action_gas_fee_quote(
        &mut self,
        action_mode: PublicActionMode,
    ) {
        let gas_fee = match action_mode {
            PublicActionMode::Shield => &mut self.public_form.shield_gas_fee,
            PublicActionMode::Send => &mut self.public_form.send_gas_fee,
        };
        gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
        gas_fee.refreshing = false;
        gas_fee.quote = None;
        gas_fee.quote_error = None;
        if action_mode == PublicActionMode::Shield {
            self.public_form.shield_gas_fee_authorization_ceiling = None;
        }
    }

    pub(in crate::root) fn set_public_action_mode(
        &mut self,
        mode: PublicActionMode,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.action_mode == mode {
            return;
        }
        self.public_form.action_mode = mode;
        self.public_form.send_error = None;
        self.public_form.shield_error = None;
        self.clear_public_action_progress_state();
        self.refresh_public_action_gas_fee_quote(mode, cx);
        cx.defer_in(window, |root, window, cx| {
            root.focus_public_action_dialog_input(window, cx);
        });
        cx.notify();
    }

    const fn public_action_has_active_progress(&self) -> bool {
        !self.public_form.action_progress.is_empty()
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

    pub(in crate::root) fn set_public_action_gas_fee_mode(
        &mut self,
        action_mode: PublicActionMode,
        mode: Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let submitting = self.public_form.sending || self.public_form.shielding;
        let gas_fee = match action_mode {
            PublicActionMode::Shield => &mut self.public_form.shield_gas_fee,
            PublicActionMode::Send => &mut self.public_form.send_gas_fee,
        };
        if submitting || gas_fee.mode == mode {
            return;
        }
        if mode == Eip1559GasFeeMode::Custom {
            gas_fee.seed_custom_from_auto_if_empty(window, cx);
        }
        gas_fee.mode = mode;
        if action_mode == PublicActionMode::Send {
            self.invalidate_advanced_public_send_estimate();
        }
        self.set_public_action_error(action_mode, None);
        cx.notify();
    }

    pub(in crate::root) fn customize_public_action_gas_fee_from_auto(
        &mut self,
        action_mode: PublicActionMode,
        target: Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.sending || self.public_form.shielding {
            return;
        }
        let gas_fee = match action_mode {
            PublicActionMode::Shield => &mut self.public_form.shield_gas_fee,
            PublicActionMode::Send => &mut self.public_form.send_gas_fee,
        };
        if !gas_fee.overwrite_custom_from_auto(window, cx) {
            return;
        }
        let focus_input = match target {
            Eip1559GasFeeEditTarget::MaxFee => gas_fee.max_fee_input.clone(),
            Eip1559GasFeeEditTarget::MaxTip => gas_fee.max_priority_fee_input.clone(),
        };
        gas_fee.mode = Eip1559GasFeeMode::Custom;
        if action_mode == PublicActionMode::Send {
            self.invalidate_advanced_public_send_estimate();
        }
        self.set_public_action_error(action_mode, None);
        focus_input.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    pub(in crate::root) fn refresh_public_action_gas_fee_quote(
        &mut self,
        action_mode: PublicActionMode,
        cx: &mut Context<'_, Self>,
    ) {
        let submitting = self.public_form.sending || self.public_form.shielding;
        let gas_fee = match action_mode {
            PublicActionMode::Shield => &mut self.public_form.shield_gas_fee,
            PublicActionMode::Send => &mut self.public_form.send_gas_fee,
        };
        if submitting || gas_fee.refreshing {
            return;
        }
        gas_fee.refresh_id = gas_fee.refresh_id.wrapping_add(1);
        gas_fee.refreshing = true;
        gas_fee.quote_error = None;
        let refresh_id = gas_fee.refresh_id;
        let chain_id = self.selected_chain;
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let http = self.http.clone();
        let profile =
            if action_mode == PublicActionMode::Shield && self.public_form.mimic_railway_shield {
                PublicShieldTransactionProfile::Railway
            } else {
                PublicShieldTransactionProfile::Railoxide
            };
        cx.spawn(async move |this, cx| {
            let result = quote_public_action_gas_fee_bundle_with_profile(
                chain_id,
                effective_chain.as_ref(),
                profile,
                &http,
            )
            .await;
            let _ = this.update(cx, |root, cx| {
                let current_refresh_id = match action_mode {
                    PublicActionMode::Shield => root.public_form.shield_gas_fee.refresh_id,
                    PublicActionMode::Send => root.public_form.send_gas_fee.refresh_id,
                };
                if current_refresh_id != refresh_id {
                    return;
                }
                match result {
                    Ok(bundle) => {
                        let standard = bundle.standard;
                        let authorization_ceiling = bundle.authorization_ceiling;
                        match action_mode {
                            PublicActionMode::Shield => {
                                root.public_form.shield_gas_fee.refreshing = false;
                                root.public_form.shield_gas_fee.quote = Some(standard);
                            }
                            PublicActionMode::Send => {
                                root.public_form.send_gas_fee.refreshing = false;
                                root.public_form.send_gas_fee.quote = Some(standard);
                            }
                        }
                        if action_mode == PublicActionMode::Shield {
                            root.public_form.shield_gas_fee_authorization_ceiling =
                                Some(authorization_ceiling);
                        }
                        match action_mode {
                            PublicActionMode::Shield => {
                                root.public_form.shield_gas_fee.quote_error = None;
                            }
                            PublicActionMode::Send => {
                                root.public_form.send_gas_fee.quote_error = None;
                            }
                        }
                        if action_mode == PublicActionMode::Send {
                            root.invalidate_advanced_public_send_estimate();
                        }
                        root.set_public_action_error(action_mode, None);
                    }
                    Err(_error) => {
                        let quote_available = match action_mode {
                            PublicActionMode::Shield => {
                                root.public_form.shield_gas_fee.quote.is_some()
                            }
                            PublicActionMode::Send => root.public_form.send_gas_fee.quote.is_some(),
                        };
                        let error = if quote_available {
                            "Gas quote refresh failed; using the last successful quote."
                        } else {
                            "Gas quote is unavailable."
                        };
                        match action_mode {
                            PublicActionMode::Shield => {
                                root.public_form.shield_gas_fee.refreshing = false;
                                root.public_form.shield_gas_fee.quote_error =
                                    Some(Arc::from(error));
                            }
                            PublicActionMode::Send => {
                                root.public_form.send_gas_fee.refreshing = false;
                                root.public_form.send_gas_fee.quote_error = Some(Arc::from(error));
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
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
        let steps = public_action_progress_steps_for_source(mode, asset, public_account_source);
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

    pub(in crate::root) fn spawn_public_action_progress_listener(
        generation: u64,
        chain_id: u64,
        active_wallet_id: Option<Arc<str>>,
        mut progress_rx: mpsc::UnboundedReceiver<PublicActionProgressUpdate>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(update) = progress_rx.recv().await {
                let _ = this.update(cx, |root, cx| {
                    if root.selected_wallet_id != active_wallet_id
                        || root.selected_chain != chain_id
                    {
                        return;
                    }
                    root.apply_public_action_progress_update(generation, update, cx);
                });
            }
        })
        .detach();
    }

    pub(in crate::root) fn spawn_public_action_session_event_listener(
        generation: u64,
        chain_id: u64,
        active_wallet_id: Option<Arc<str>>,
        mut event_rx: mpsc::UnboundedReceiver<PublicActionSessionEvent>,
        cx: &Context<'_, Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = event_rx.recv().await {
                let _ = this.update(cx, |root, cx| {
                    if root.selected_wallet_id != active_wallet_id
                        || root.selected_chain != chain_id
                    {
                        return;
                    }
                    root.apply_public_action_session_event(generation, event, cx);
                });
            }
        })
        .detach();
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
        let content_max_height = dialog_content_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        window.open_dialog(cx, move |dialog, _window, cx| {
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
                .child(scrollable_dialog_content(
                    content_max_height,
                    content_root
                        .read(cx)
                        .render_public_action_progress_dialog_content(&content_root, content_width),
                ))
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
            &self.public_form.action_progress,
        ) != ProgressFooterAction::Stop
        {
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
        match self.public_form.action_progress_mode {
            PublicActionMode::Shield => self.public_form.shielding = false,
            PublicActionMode::Send => self.public_form.sending = false,
        }
        mark_public_action_active_step_stopped(&mut self.public_form.action_progress);
        cx.notify();
    }

    pub(in crate::root) fn discard_public_action_attempt(&mut self, cx: &mut Context<'_, Self>) {
        if self.public_form.action_command_tx.is_none() {
            return;
        }
        self.public_form.action_generation = self.public_form.action_generation.wrapping_add(1);
        match self.public_form.action_progress_mode {
            PublicActionMode::Shield => self.public_form.shielding = false,
            PublicActionMode::Send => self.public_form.sending = false,
        }
        self.clear_public_action_progress_state();
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
        let mut content =
            div()
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
                    self.public_form.action_generation,
                ));

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
            content = content.child(super::super::vault_ui::render_trezor_pin_matrix_prompt(
                root, prompt,
            ));
        }
        content = content.child(render_public_action_progress_footer(
            root.clone(),
            public_action_progress_footer_action(
                self.public_form.action_stop_available,
                &self.public_form.action_progress,
            ),
        ));
        content
    }

    pub(in crate::root) fn open_public_action_gas_retry_dialog(
        &self,
        generation: u64,
        retry_kind: PublicActionGasRetryKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.action_generation != generation
            || self.public_form.action_command_tx.is_none()
        {
            return;
        }
        let (mut max_fee, mut max_tip) = self.public_form.action_current_gas_fee.unwrap_or((
            PUBLIC_ACTION_RETRY_DEFAULT_FEE_WEI,
            PUBLIC_ACTION_RETRY_DEFAULT_FEE_WEI,
        ));
        if retry_kind == PublicActionGasRetryKind::FeeAuthorization
            && let Some(review) = self.public_form.action_fee_authorization_review.as_ref()
        {
            max_fee = review.max_fee_per_gas;
            max_tip = review.max_priority_fee_per_gas;
        }
        if retry_kind == PublicActionGasRetryKind::SpeedUp {
            max_fee = public_action_replacement_bumped_fee(max_fee);
            max_tip = public_action_replacement_bumped_fee(max_tip);
        }
        let root = cx.entity();
        let content = cx.new(|cx| {
            PublicActionGasRetryDialogContent::new(
                root, generation, retry_kind, max_fee, max_tip, window, cx,
            )
        });
        let dialog_width = (window.viewport_size().width * 0.92).min(px(460.0));
        let dialog_max_height = dialog_max_height(window);
        let content_max_height = dialog_content_max_height(window);
        window.open_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .child(scrollable_dialog_content(
                    content_max_height,
                    content.clone(),
                ))
        });
    }

    pub(in crate::root) fn submit_public_action_gas_retry(
        &mut self,
        generation: u64,
        retry_kind: PublicActionGasRetryKind,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.action_generation != generation {
            return;
        }
        let Some(command_tx) = self.public_form.action_command_tx.as_ref() else {
            return;
        };
        let kind = match retry_kind {
            PublicActionGasRetryKind::RetryStep | PublicActionGasRetryKind::RetryEstimate => {
                PublicActionCommandKind::Retry
            }
            PublicActionGasRetryKind::SpeedUp => PublicActionCommandKind::Replacement,
            PublicActionGasRetryKind::FeeAuthorization => PublicActionCommandKind::Retry,
        };
        let send_result = command_tx.send(PublicActionCommand {
            kind,
            gas_fee: PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            },
        });
        let send_ok = send_result.is_ok();
        self.public_form.action_action_error = send_result
            .err()
            .map(|_| Arc::from("Public action is no longer accepting retry commands."));
        if send_ok && retry_kind == PublicActionGasRetryKind::FeeAuthorization {
            self.public_form.action_fee_authorization_review = None;
        }
        cx.notify();
    }

    pub(in crate::root) fn submit_public_action_step_retry(
        &mut self,
        generation: u64,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.action_generation != generation {
            return;
        }
        let Some(command_tx) = self.public_form.action_command_tx.as_ref() else {
            return;
        };
        let gas_fee = self.public_form.action_current_gas_fee.map_or(
            PublicActionGasFeeSelection::Auto,
            |(max_fee_per_gas, max_priority_fee_per_gas)| PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            },
        );
        let send_result = command_tx.send(PublicActionCommand {
            kind: PublicActionCommandKind::Retry,
            gas_fee,
        });
        self.public_form.action_action_error = send_result
            .err()
            .map(|_| Arc::from("Public action is no longer accepting retry commands."));
        cx.notify();
    }

    pub(in crate::root) fn set_public_action_error_details_open(
        &mut self,
        step: PublicActionProgressStep,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        if open {
            self.public_form.expanded_action_error_steps.insert(step);
        } else {
            self.public_form.expanded_action_error_steps.remove(&step);
        }
        cx.notify();
    }

    pub(in crate::root) fn set_public_action_amount_to_max(
        &mut self,
        mode: PublicActionMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(entry) = self.selected_public_balance_entry() else {
            return;
        };
        let Some(amount) = entry.amount.amount() else {
            return;
        };
        let decimals = entry.asset.decimals;
        if entry.asset.id != PublicAssetId::Native {
            self.set_public_action_amount_input(mode, amount, decimals, window, cx);
            self.set_public_action_error(mode, None);
            cx.notify();
            return;
        }

        let Some(public_account_uuid) = self.public_form.selected_account_uuid.clone() else {
            return;
        };
        let chain_id = self.selected_chain;
        let selected_wallet_id = self.selected_wallet_id.clone();
        let symbol = entry.asset.symbol;
        let http = self.http.clone();
        let steps = public_action_progress_steps(mode, PublicAssetId::Native);
        let profile = if mode == PublicActionMode::Shield && self.public_form.mimic_railway_shield {
            PublicShieldTransactionProfile::Railway
        } else {
            PublicShieldTransactionProfile::Railoxide
        };
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let gas_fee = match self.public_action_gas_fee_selection(mode, cx) {
            Ok(selection) => selection,
            Err(error) => {
                self.set_public_action_error(mode, Some(Arc::from(error)));
                cx.notify();
                return;
            }
        };
        let join = self.runtime.spawn(async move {
            estimate_public_native_action_gas_reserve_with_profile_and_ceiling(
                chain_id,
                &steps,
                profile,
                effective_chain.as_ref(),
                gas_fee,
                &http,
                None,
            )
            .await
        });
        self.set_public_action_error(mode, None);
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.selected_wallet_id != selected_wallet_id
                    || root.selected_chain != chain_id
                    || root.public_form.action_mode != mode
                    || root.public_form.selected_asset != Some(PublicAssetId::Native)
                    || root.public_form.selected_account_uuid.as_deref()
                        != Some(public_account_uuid.as_ref())
                {
                    return;
                }
                match result {
                    Ok(Ok(reserve)) => {
                        match public_action_max_amount_after_reserve(amount, reserve) {
                            Some(max_amount) => {
                                root.set_public_action_amount_input(
                                    mode, max_amount, decimals, window, cx,
                                );
                                root.set_public_action_error(mode, None);
                            }
                            None => root.set_public_action_error(
                                mode,
                                Some(Arc::from(format!(
                                    "Not enough {symbol} balance after estimated gas"
                                ))),
                            ),
                        }
                    }
                    Ok(Err(error)) => root.set_public_action_error(
                        mode,
                        Some(Arc::from(format!(
                            "Could not estimate gas reserve for Max: {}",
                            format_report_chain(&error)
                        ))),
                    ),
                    Err(error) => root.set_public_action_error(
                        mode,
                        Some(Arc::from(format!(
                            "Could not estimate gas reserve for Max: {error}"
                        ))),
                    ),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root) fn set_public_action_amount_input(
        &self,
        mode: PublicActionMode,
        amount: U256,
        decimals: u8,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let value = format_send_amount_input(amount, Some(decimals));
        let input = match mode {
            PublicActionMode::Shield => &self.public_form.shield_amount_input,
            PublicActionMode::Send => &self.public_form.send_amount_input,
        };
        input.update(cx, |input, cx| input.set_value(value, window, cx));
    }

    pub(in crate::root) fn set_public_action_error(
        &mut self,
        mode: PublicActionMode,
        message: Option<Arc<str>>,
    ) {
        match mode {
            PublicActionMode::Shield => self.public_form.shield_error = message,
            PublicActionMode::Send => self.public_form.send_error = message,
        }
    }

    pub(in crate::root) fn public_action_gas_fee_selection(
        &self,
        mode: PublicActionMode,
        cx: &Context<'_, Self>,
    ) -> Result<PublicActionGasFeeSelection, String> {
        match mode {
            PublicActionMode::Shield => self.public_form.shield_gas_fee.selection(cx),
            PublicActionMode::Send => self.public_form.send_gas_fee.selection(cx),
        }
    }

    fn public_action_authorized_gas_fee_selection(
        &self,
        mode: PublicActionMode,
        profile: PublicShieldTransactionProfile,
        cx: &App,
    ) -> Result<PublicActionGasFeeSelection, String> {
        let (selection, quote) = match mode {
            PublicActionMode::Shield => (
                self.public_form.shield_gas_fee.selection(cx)?,
                self.public_form.shield_gas_fee.quote,
            ),
            PublicActionMode::Send => (
                self.public_form.send_gas_fee.selection(cx)?,
                self.public_form.send_gas_fee.quote,
            ),
        };
        authorized_public_action_gas_fee_selection(selection, quote, profile, self.selected_chain)
    }

    pub(in crate::root) fn public_action_fee_display(
        &self,
        mode: PublicActionMode,
        amount: Option<U256>,
        cx: &App,
    ) -> PublicActionFeeDisplay {
        let Some(asset) = self.public_form.selected_asset else {
            return PublicActionFeeDisplay {
                gas_limit: None,
                expected_gas_cost: None,
                maximum_gas_cost: None,
                show_maximum_gas_cost: false,
                protocol_fee: None,
            };
        };
        let gas_cost = self.public_action_estimated_gas_cost(mode, asset, cx);
        let format_gas_cost = |native_gas_cost| {
            let token_value =
                format_native_token_amount_for_display(self.selected_chain, native_gas_cost);
            let usd_micro_value = self
                .public_broadcaster_anchor_cache
                .cached_native_usd_micro_value(self.selected_chain, native_gas_cost);
            format_value_with_usd_label(
                token_value,
                native_gas_cost,
                Some(18),
                usd_micro_value,
                false,
            )
        };
        let expected_gas_cost = gas_cost.map(|cost| format_gas_cost(cost.expected_cost));
        let maximum_gas_cost = gas_cost.map(|cost| format_gas_cost(cost.maximum_cost));
        let show_maximum_gas_cost = gas_cost.is_some_and(|cost| {
            public_action_maximum_gas_cost_is_significant(cost.expected_cost, cost.maximum_cost)
        });
        let protocol_fee = if mode == PublicActionMode::Shield {
            amount.map(|amount| {
                let fee_amount = public_shield_protocol_fee_amount(amount);
                let token_value = match asset {
                    PublicAssetId::Native => {
                        format_native_token_amount_for_display(self.selected_chain, fee_amount)
                    }
                    PublicAssetId::Erc20(token) => format_token_amount_for_display(
                        self.selected_chain,
                        token,
                        fee_amount,
                        Some(&self.effective_token_registry),
                    ),
                };
                let usd_micro_value = match asset {
                    PublicAssetId::Native => self
                        .public_broadcaster_anchor_cache
                        .cached_native_usd_micro_value(self.selected_chain, fee_amount),
                    PublicAssetId::Erc20(token) => self
                        .public_broadcaster_anchor_cache
                        .cached_token_usd_micro_value(self.selected_chain, token, fee_amount),
                };
                format_value_with_usd_label(
                    token_value,
                    fee_amount,
                    public_asset_decimals(
                        self.selected_chain,
                        asset,
                        Some(&self.effective_token_registry),
                    ),
                    usd_micro_value,
                    false,
                )
            })
        } else {
            None
        };
        PublicActionFeeDisplay {
            gas_limit: (mode == PublicActionMode::Shield
                && asset == PublicAssetId::Native
                && self.public_form.mimic_railway_shield)
                .then_some(6_000_000),
            expected_gas_cost,
            maximum_gas_cost,
            show_maximum_gas_cost,
            protocol_fee,
        }
    }

    fn public_action_estimated_gas_cost(
        &self,
        mode: PublicActionMode,
        asset: PublicAssetId,
        cx: &App,
    ) -> Option<Eip1559GasCostProjection> {
        let (kind, gas_fee) = match mode {
            PublicActionMode::Shield => {
                (PublicActionKind::Shield, &self.public_form.shield_gas_fee)
            }
            PublicActionMode::Send => (PublicActionKind::Send, &self.public_form.send_gas_fee),
        };
        let profile = if mode == PublicActionMode::Shield && self.public_form.mimic_railway_shield {
            PublicShieldTransactionProfile::Railway
        } else {
            PublicShieldTransactionProfile::Railoxide
        };
        let gas_fee_mode = match gas_fee.mode {
            Eip1559GasFeeMode::Auto => PublicActionGasFeeMode::Auto,
            Eip1559GasFeeMode::Custom => PublicActionGasFeeMode::Custom,
        };
        let authorization_ceiling =
            public_action_uses_railway_authorization_ceiling(mode, profile, asset, gas_fee_mode)
                .then_some(self.public_form.shield_gas_fee_authorization_ceiling)
                .flatten();
        estimate_public_action_gas_cost_with_profile_and_ceiling(
            self.selected_chain,
            self.effective_chain_configs.get(&self.selected_chain),
            kind,
            asset,
            profile,
            gas_fee.selection(cx).ok()?,
            gas_fee.quote,
            authorization_ceiling,
        )
        .ok()
    }

    pub(in crate::root) fn public_action_initial_gas_values(
        &self,
        mode: PublicActionMode,
        selection: &PublicActionGasFeeSelection,
    ) -> Option<(u128, u128)> {
        match selection {
            PublicActionGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } => Some((*max_fee_per_gas, *max_priority_fee_per_gas)),
            PublicActionGasFeeSelection::Auto => {
                let quote = match mode {
                    PublicActionMode::Shield => self.public_form.shield_gas_fee.quote,
                    PublicActionMode::Send => self.public_form.send_gas_fee.quote,
                }?;
                Some((
                    quote.suggested_max_fee_per_gas,
                    quote.suggested_max_priority_fee_per_gas,
                ))
            }
        }
    }

    pub(in crate::root) fn submit_public_send_from_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.sending {
            return;
        }
        self.clear_public_action_progress_state();
        let Some(draft) = self.public_send_draft(cx) else {
            return;
        };
        let summary = public_send_authorization_summary(&draft);
        let hardware_derived = draft.public_account_source == PublicAccountSource::HardwareDerived;
        let intent = SpendAuthorizationIntent::PublicSend(Box::new(draft));
        if hardware_derived {
            Self::open_hardware_public_action_authorization_dialog(intent, summary, window, cx);
        } else {
            self.request_spend_authorization(intent, summary, window, cx);
        }
    }

    pub(in crate::root) fn public_send_draft(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<PublicSendDraft> {
        let Some(asset) = self.public_form.selected_asset else {
            self.public_form.send_error = Some(Arc::from("Select an asset to send"));
            cx.notify();
            return None;
        };
        let Some(public_account_uuid) = self.public_form.selected_account_uuid.clone() else {
            self.public_form.send_error = Some(Arc::from("Select a public account first"));
            cx.notify();
            return None;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.send_error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return None;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.public_form.send_error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return None;
        };
        let chain_id = self.selected_chain;
        let asset_decimals =
            public_asset_decimals(chain_id, asset, Some(&self.effective_token_registry));
        let asset_label =
            public_action_asset_label(chain_id, asset, Some(&self.effective_token_registry));
        let asset_icon_path =
            public_asset_icon_path(chain_id, asset, Some(&self.effective_token_registry));
        let Some(public_account) = self.public_account_for_uuid(Some(public_account_uuid.as_ref()))
        else {
            self.public_form.send_error = Some(Arc::from("Selected public account was not found"));
            cx.notify();
            return None;
        };
        let public_account_label = public_account_display_label(public_account)
            .unwrap_or_else(|| short_address(&public_account.address));
        let public_account_source = public_account.source;
        let (amount, recipient, intent, advanced_estimate, gas_fee, fee_display) =
            if self.public_form.public_send_kind.is_advanced() {
                if asset != PublicAssetId::Native {
                    self.public_form.send_error = Some(Arc::from(
                        "Advanced transactions are available only from the native asset",
                    ));
                    cx.notify();
                    return None;
                }
                let parsed = parse_advanced_public_send_intent(
                    self.public_form.public_send_kind,
                    self.public_form
                        .advanced_send_to_input
                        .read(cx)
                        .value()
                        .as_ref(),
                    self.public_form
                        .advanced_send_value_input
                        .read(cx)
                        .value()
                        .as_ref(),
                    self.public_form
                        .advanced_send_data_input
                        .read(cx)
                        .value()
                        .as_ref(),
                    asset_decimals,
                );
                let intent = match parsed {
                    Ok(intent) => intent,
                    Err(error) => {
                        self.set_advanced_public_send_validation_error(error);
                        cx.notify();
                        return None;
                    }
                };
                let Some(estimate) = self.public_form.advanced_send_estimate.clone() else {
                    self.public_form.send_error =
                        Some(Arc::from(advanced_public_send_estimate_required_message(
                            self.public_form.advanced_send_estimate_invalidated,
                        )));
                    cx.notify();
                    return None;
                };
                let PublicTransactionIntent::Raw { to, value, .. } = &intent else {
                    unreachable!("advanced parser returns a raw intent")
                };
                let expected_token_value =
                    format_native_token_amount_for_display(chain_id, estimate.expected_gas_cost);
                let expected_usd_micro_value = self
                    .public_broadcaster_anchor_cache
                    .cached_native_usd_micro_value(chain_id, estimate.expected_gas_cost);
                let maximum_token_value =
                    format_native_token_amount_for_display(chain_id, estimate.max_gas_cost);
                let maximum_usd_micro_value = self
                    .public_broadcaster_anchor_cache
                    .cached_native_usd_micro_value(chain_id, estimate.max_gas_cost);
                let fee_display = PublicActionFeeDisplay {
                    gas_limit: None,
                    expected_gas_cost: Some(format_value_with_usd_label(
                        expected_token_value,
                        estimate.expected_gas_cost,
                        Some(18),
                        expected_usd_micro_value,
                        false,
                    )),
                    maximum_gas_cost: Some(format_value_with_usd_label(
                        maximum_token_value,
                        estimate.max_gas_cost,
                        Some(18),
                        maximum_usd_micro_value,
                        false,
                    )),
                    show_maximum_gas_cost: public_action_maximum_gas_cost_is_significant(
                        estimate.expected_gas_cost,
                        estimate.max_gas_cost,
                    ),
                    protocol_fee: None,
                };
                (
                    *value,
                    to.unwrap_or_default(),
                    intent,
                    Some(estimate.clone()),
                    PublicActionGasFeeSelection::Custom {
                        max_fee_per_gas: estimate.max_fee_per_gas,
                        max_priority_fee_per_gas: estimate.max_priority_fee_per_gas,
                    },
                    fee_display,
                )
            } else {
                let amount_input = self
                    .public_form
                    .send_amount_input
                    .read(cx)
                    .value()
                    .to_string();
                let amount = match parse_send_amount(&amount_input, asset_decimals) {
                    Ok(amount) if !amount.is_zero() => amount,
                    Ok(_) => {
                        self.public_form.send_error =
                            Some(Arc::from("Amount must be greater than zero"));
                        cx.notify();
                        return None;
                    }
                    Err(error) => {
                        self.public_form.send_error = Some(Arc::from(error.to_string()));
                        cx.notify();
                        return None;
                    }
                };
                let Some(recipient) = parse_address(
                    self.public_form
                        .send_recipient_input
                        .read(cx)
                        .value()
                        .as_ref(),
                ) else {
                    self.public_form.send_error =
                        Some(Arc::from("Enter a valid EVM recipient address"));
                    cx.notify();
                    return None;
                };
                let gas_fee = match self.public_action_authorized_gas_fee_selection(
                    PublicActionMode::Send,
                    PublicShieldTransactionProfile::Railoxide,
                    cx,
                ) {
                    Ok(selection) => selection,
                    Err(error) => {
                        self.public_form.send_error = Some(Arc::from(error));
                        cx.notify();
                        return None;
                    }
                };
                let fee_display =
                    self.public_action_fee_display(PublicActionMode::Send, Some(amount), cx);
                if fee_display.expected_gas_cost.is_none() || fee_display.maximum_gas_cost.is_none()
                {
                    self.public_form.send_error = Some(Arc::from(
                        "Wait for an estimated gas cost before authorizing the send",
                    ));
                    cx.notify();
                    return None;
                }
                (
                    amount,
                    recipient,
                    PublicTransactionIntent::Transfer {
                        asset,
                        amount,
                        recipient,
                    },
                    None,
                    gas_fee,
                    fee_display,
                )
            };
        Some(PublicSendDraft {
            chain_id,
            asset,
            asset_label,
            asset_icon_path,
            asset_decimals,
            public_account_uuid,
            public_account_label,
            public_account_source,
            view_session,
            vault_store,
            amount,
            recipient,
            intent,
            advanced_estimate,
            gas_fee,
            fee_display,
        })
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn submit_public_send_authorized(
        &mut self,
        draft: PublicSendDraft,
        vault_password: Zeroizing<String>,
        protected_software_seed_session: Option<
            Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        >,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.sending {
            return;
        }
        let PublicSendDraft {
            chain_id,
            asset,
            asset_label,
            public_account_uuid,
            public_account_source,
            view_session,
            vault_store,
            intent,
            advanced_estimate,
            gas_fee,
            ..
        } = draft;
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase = view_session.hardware_profile_session().and_then(|session| {
            self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
        });
        #[cfg(not(feature = "hardware"))]
        let trezor_app_passphrase = None;
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider =
            if matches!(&public_account_source, PublicAccountSource::HardwareDerived) {
                Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
            } else {
                None
            };
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_provider = None;
        self.public_form.sending = true;
        self.public_form.send_error = None;
        let http = self.http.clone();
        let active_wallet_id = self.selected_wallet_id.clone();
        let icon_path =
            public_asset_icon_path(chain_id, asset, Some(&self.effective_token_registry));
        let initial_gas_fee =
            self.public_action_initial_gas_values(PublicActionMode::Send, &gas_fee);
        let advanced_submission = advanced_estimate.is_some();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let generation = self.start_public_action_progress(
            PublicActionMode::Send,
            asset,
            asset_label,
            icon_path,
            public_account_source,
            Some(command_tx),
            initial_gas_fee,
            advanced_submission,
        );
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        Self::spawn_public_action_progress_listener(
            generation,
            chain_id,
            active_wallet_id.clone(),
            progress_rx,
            cx,
        );
        Self::spawn_public_action_session_event_listener(
            generation,
            chain_id,
            active_wallet_id.clone(),
            event_rx,
            cx,
        );
        Self::show_public_action_progress_dialog_after_close(window, cx);
        let request = PublicSendRequest {
            chain_id,
            effective_chain: self.effective_chain_configs.get(&chain_id).cloned(),
            view_session,
            vault_store,
            vault_password,
            protected_software_seed_session,
            trezor_app_passphrase,
            trezor_pin_matrix_provider,
            public_account_uuid: public_account_uuid.to_string(),
            intent,
            advanced_authorization: advanced_estimate.map(|estimate| {
                PublicAdvancedTransactionAuthorization {
                    payload_fingerprint: estimate.payload_fingerprint,
                    gas_limit: estimate.gas_limit,
                }
            }),
            gas_fee,
            command_rx: Some(command_rx),
            event_tx: Some(event_tx),
        };
        let submitted_public_account_uuid = Arc::clone(&public_account_uuid);
        let join = self.runtime.spawn(async move {
            submit_public_send_with_progress(request, &http, move |update| {
                let _ = progress_tx.send(update);
            })
            .await
        });
        self.public_form.action_task_abort_handle = Some(join.abort_handle());
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |root, cx| {
                if root.selected_wallet_id != active_wallet_id || root.selected_chain != chain_id {
                    return;
                }
                if !public_action_accepts_update(
                    root.public_form.action_generation,
                    generation,
                    root.public_form.action_stopped,
                ) {
                    return;
                }
                root.public_form.sending = false;
                root.public_form.action_task_abort_handle = None;
                match result {
                    Ok(Ok(result)) => {
                        root.public_form.action_command_tx = None;
                        root.public_form.action_action_error = None;
                        root.public_form.action_contract_address =
                            result.tx.contract_address.as_deref().map(Arc::from);
                        match root
                            .public_account_for_uuid(Some(submitted_public_account_uuid.as_ref()))
                            .map(|account| account.status)
                        {
                            Some(PublicAccountStatus::Inactive) => {
                                root.schedule_inactive_public_balance_refresh(cx);
                            }
                            _ => root.schedule_public_balance_refresh(cx),
                        }
                    }
                    Ok(Err(error)) => {
                        let message = format_report_chain(&error);
                        if is_spend_authorization_failure_error(&message) {
                            root.clear_spend_authorization(cx);
                        }
                        root.discard_active_trezor_session_if_stale(&message, cx);
                        root.public_form.action_command_tx = None;
                        if advanced_submission {
                            root.invalidate_advanced_public_send_estimate();
                        }
                        root.fail_public_action_progress(generation, message.clone(), cx);
                        root.public_form.send_error = Some(Arc::from(message));
                    }
                    Err(error) => {
                        let message = format!("Public send task failed: {error}");
                        root.public_form.action_command_tx = None;
                        if advanced_submission {
                            root.invalidate_advanced_public_send_estimate();
                        }
                        root.fail_public_action_progress(generation, message.clone(), cx);
                        root.public_form.send_error = Some(Arc::from(message));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root) fn submit_public_shield_from_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.shielding {
            return;
        }
        self.clear_public_action_progress_state();
        let Some(draft) = self.public_shield_draft(cx) else {
            return;
        };
        let summary = public_shield_authorization_summary(&draft);
        let hardware_derived = draft.public_account_source == PublicAccountSource::HardwareDerived;
        let intent = SpendAuthorizationIntent::PublicShield(Box::new(draft));
        if hardware_derived {
            Self::open_hardware_public_action_authorization_dialog(intent, summary, window, cx);
        } else {
            self.request_spend_authorization(intent, summary, window, cx);
        }
    }

    pub(in crate::root) fn public_shield_draft(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Option<PublicShieldDraft> {
        let Some(asset) = self.public_form.selected_asset else {
            self.public_form.shield_error = Some(Arc::from("Select an asset to shield"));
            cx.notify();
            return None;
        };
        let Some(public_account_uuid) = self.public_form.selected_account_uuid.clone() else {
            self.public_form.shield_error = Some(Arc::from("Select a public account first"));
            cx.notify();
            return None;
        };
        let Some(view_session) = self.view_session.clone() else {
            self.public_form.shield_error = Some(Arc::from("Wallet vault is locked"));
            cx.notify();
            return None;
        };
        let Some(vault_store) = self.vault_store.clone() else {
            self.public_form.shield_error = Some(Arc::from("Wallet vault storage is unavailable"));
            cx.notify();
            return None;
        };
        let chain_id = self.selected_chain;
        let asset_decimals =
            public_asset_decimals(chain_id, asset, Some(&self.effective_token_registry));
        let asset_label =
            public_action_asset_label(chain_id, asset, Some(&self.effective_token_registry));
        let asset_icon_path =
            public_asset_icon_path(chain_id, asset, Some(&self.effective_token_registry));
        let Some(public_account) = self.public_account_for_uuid(Some(public_account_uuid.as_ref()))
        else {
            self.public_form.shield_error =
                Some(Arc::from("Selected public account was not found"));
            cx.notify();
            return None;
        };
        let public_account_label = public_account_display_label(public_account)
            .unwrap_or_else(|| short_address(&public_account.address));
        let public_account_source = public_account.source;
        let amount_input = self
            .public_form
            .shield_amount_input
            .read(cx)
            .value()
            .to_string();
        let amount = match parse_send_amount(&amount_input, asset_decimals) {
            Ok(amount) if !amount.is_zero() => amount,
            Ok(_) => {
                self.public_form.shield_error = Some(Arc::from("Amount must be greater than zero"));
                cx.notify();
                return None;
            }
            Err(error) => {
                self.public_form.shield_error = Some(Arc::from(error.to_string()));
                cx.notify();
                return None;
            }
        };
        let profile = if self.public_form.mimic_railway_shield {
            PublicShieldTransactionProfile::Railway
        } else {
            PublicShieldTransactionProfile::Railoxide
        };
        let gas_fee_mode = match self.public_form.shield_gas_fee.mode {
            Eip1559GasFeeMode::Auto => PublicActionGasFeeMode::Auto,
            Eip1559GasFeeMode::Custom => PublicActionGasFeeMode::Custom,
        };
        let gas_fee = match self.public_action_authorized_gas_fee_selection(
            PublicActionMode::Shield,
            profile,
            cx,
        ) {
            Ok(selection) => selection,
            Err(error) => {
                self.public_form.shield_error = Some(Arc::from(error));
                cx.notify();
                return None;
            }
        };
        let authorized_fee_ceiling = if public_action_uses_railway_authorization_ceiling(
            PublicActionMode::Shield,
            profile,
            asset,
            gas_fee_mode,
        ) {
            let Some(ceiling) = self.public_form.shield_gas_fee_authorization_ceiling else {
                self.public_form.shield_error =
                    Some(Arc::from("Wait for the Railway authorization ceiling"));
                cx.notify();
                return None;
            };
            ceiling
        } else {
            gas_fee
        };
        let fee_display =
            self.public_action_fee_display(PublicActionMode::Shield, Some(amount), cx);
        if fee_display.expected_gas_cost.is_none()
            || fee_display.maximum_gas_cost.is_none()
            || fee_display.protocol_fee.is_none()
        {
            self.public_form.shield_error = Some(Arc::from(
                "Wait for fee estimates before authorizing the shield",
            ));
            cx.notify();
            return None;
        }
        Some(PublicShieldDraft {
            chain_id,
            asset,
            asset_label,
            asset_icon_path,
            asset_decimals,
            public_account_uuid,
            public_account_label,
            public_account_source,
            view_session,
            vault_store,
            amount,
            profile,
            gas_fee,
            gas_fee_mode,
            authorized_fee_ceiling,
            fee_display,
        })
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(in crate::root) fn submit_public_shield_authorized(
        &mut self,
        draft: PublicShieldDraft,
        vault_password: Zeroizing<String>,
        protected_software_seed_session: Option<
            Arc<wallet_ops::vault::ProtectedSoftwareSeedSession>,
        >,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.public_form.shielding {
            return;
        }
        let PublicShieldDraft {
            chain_id,
            asset,
            asset_label,
            public_account_uuid,
            public_account_source,
            view_session,
            vault_store,
            amount,
            profile,
            gas_fee,
            gas_fee_mode,
            authorized_fee_ceiling,
            ..
        } = draft;
        #[cfg(feature = "hardware")]
        let trezor_app_passphrase = view_session.hardware_profile_session().and_then(|session| {
            self.read_trezor_app_passphrase_for_hardware_session(session, window, cx)
        });
        #[cfg(not(feature = "hardware"))]
        let trezor_app_passphrase = None;
        #[cfg(feature = "hardware")]
        let trezor_pin_matrix_provider =
            if matches!(&public_account_source, PublicAccountSource::HardwareDerived) {
                Some(self.trezor_pin_matrix_provider_for_operation(window, cx))
            } else {
                None
            };
        #[cfg(not(feature = "hardware"))]
        let trezor_pin_matrix_provider = None;
        self.public_form.shielding = true;
        self.public_form.shield_error = None;
        let http = self.http.clone();
        let active_wallet_id = self.selected_wallet_id.clone();
        let icon_path =
            public_asset_icon_path(chain_id, asset, Some(&self.effective_token_registry));
        let initial_gas_fee =
            self.public_action_initial_gas_values(PublicActionMode::Shield, &gas_fee);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let generation = self.start_public_action_progress(
            PublicActionMode::Shield,
            asset,
            asset_label,
            icon_path,
            public_account_source,
            Some(command_tx),
            initial_gas_fee,
            false,
        );
        let (progress_tx, progress_rx) = mpsc::unbounded_channel();
        Self::spawn_public_action_progress_listener(
            generation,
            chain_id,
            active_wallet_id.clone(),
            progress_rx,
            cx,
        );
        Self::spawn_public_action_session_event_listener(
            generation,
            chain_id,
            active_wallet_id.clone(),
            event_rx,
            cx,
        );
        Self::show_public_action_progress_dialog_after_close(window, cx);
        let request = PublicShieldRequest {
            chain_id,
            effective_chain: self.effective_chain_configs.get(&chain_id).cloned(),
            view_session,
            vault_store,
            vault_password,
            protected_software_seed_session,
            trezor_app_passphrase,
            trezor_pin_matrix_provider,
            public_account_uuid: public_account_uuid.to_string(),
            asset,
            amount,
            profile,
            gas_fee,
            gas_fee_mode,
            authorized_fee_ceiling,
            command_rx: Some(command_rx),
            event_tx: Some(event_tx),
        };
        let submitted_public_account_uuid = Arc::clone(&public_account_uuid);
        let join = self.runtime.spawn(async move {
            submit_public_shield_with_progress(request, &http, move |update| {
                let _ = progress_tx.send(update);
            })
            .await
        });
        self.public_form.action_task_abort_handle = Some(join.abort_handle());
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let _ = this.update(cx, |root, cx| {
                if root.selected_wallet_id != active_wallet_id || root.selected_chain != chain_id {
                    return;
                }
                if !public_action_accepts_update(
                    root.public_form.action_generation,
                    generation,
                    root.public_form.action_stopped,
                ) {
                    return;
                }
                root.public_form.shielding = false;
                root.public_form.action_task_abort_handle = None;
                match result {
                    Ok(Ok(_result)) => {
                        root.public_form.action_command_tx = None;
                        root.public_form.action_action_error = None;
                        root.public_form.mimic_railway_shield =
                            root.mimic_railway_shields_by_default;
                        match root
                            .public_account_for_uuid(Some(submitted_public_account_uuid.as_ref()))
                            .map(|account| account.status)
                        {
                            Some(PublicAccountStatus::Inactive) => {
                                root.schedule_inactive_public_balance_refresh(cx);
                            }
                            _ => root.schedule_public_balance_refresh(cx),
                        }
                    }
                    Ok(Err(error)) => {
                        let message = format_report_chain(&error);
                        if is_spend_authorization_failure_error(&message) {
                            root.clear_spend_authorization(cx);
                        }
                        root.discard_active_trezor_session_if_stale(&message, cx);
                        root.public_form.action_command_tx = None;
                        root.fail_public_action_progress(generation, message.clone(), cx);
                        root.public_form.shield_error = Some(Arc::from(message));
                    }
                    Err(error) => {
                        let message = format!("Public shield task failed: {error}");
                        root.public_form.action_command_tx = None;
                        root.fail_public_action_progress(generation, message.clone(), cx);
                        root.public_form.shield_error = Some(Arc::from(message));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}
