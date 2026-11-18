use super::*;

impl WalletRoot {
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
                    self.effective_chain_configs.get(self.last_evm_chain),
                    asset,
                    Some(&self.effective_token_registry),
                )
            },
        );
        let disabled = account.is_none() || selected_asset.is_none();
        let submitting = self.public_action_in_flight();
        let mode_root = root.clone();
        let submit_root = root.clone();
        let gas_fee_root = root.clone();
        let progress_root = root.clone();
        let advanced_mode_root = root.clone();
        let advanced_estimate_root = root.clone();
        let advanced_submit_root = root.clone();
        let keyboard_submit_root = root.clone();
        let max_root = root;
        let show_form_errors = !self.public_action_has_active_progress();
        let amount_hint = format!("{asset_label} amount");
        let send_amount_hint = selected_asset.map_or_else(
            || "Amount".to_string(),
            |asset| {
                format!(
                    "Amount ({})",
                    public_action_asset_label(
                        self.effective_chain_configs.get(self.last_evm_chain),
                        asset,
                        Some(&self.effective_token_registry),
                    )
                )
            },
        );
        let max_gas_reserve = selected_asset
            .and_then(|asset| self.public_action_estimated_gas_cost(mode, asset, cx))
            .map(|projection| projection.maximum_cost);
        let native_stealth = self.native_shield_stealth_applies(mode);
        let max_label = balance_entry.as_ref().and_then(|entry| {
            // Stealth-route Max prepares its own reserve; before that only the balance is known.
            if native_stealth && max_gas_reserve.is_none() {
                return entry.amount.amount().map(|_| {
                    format!(
                        "{} {} before gas",
                        public_balance_amount_label(&entry.amount, entry.asset.decimals),
                        entry.asset.symbol,
                    )
                });
            }
            public_action_max_label(entry, max_gas_reserve)
        });
        let fee_amount = selected_asset.and_then(|asset| {
            let input = match mode {
                PublicActionMode::Shield => &self.public_form.shield_amount_input,
                PublicActionMode::Send | PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                    &self.public_form.send_amount_input
                }
            };
            let decimals = public_asset_decimals(
                self.effective_chain_configs.get(self.last_evm_chain),
                asset,
                Some(&self.effective_token_registry),
            );
            parse_send_amount(input.read(cx).value().as_ref(), decimals)
                .ok()
                .filter(|amount| !amount.is_zero())
        });
        let fee_display = self.public_action_fee_display(mode, fee_amount, cx);
        let gas_pending = match mode {
            PublicActionMode::Shield => {
                self.public_form.shield_gas_fee.refreshing
                    || (native_stealth && self.public_form.native_shield_preparation.pending)
            }
            PublicActionMode::Send | PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                self.public_form.gas_fee(mode).refreshing
            }
        };
        let fee_ready = fee_display.expected_gas_cost.is_some();
        let unwrap_gas_error = selected_asset
            .filter(|_| mode == PublicActionMode::Unwrap)
            .and_then(|asset| self.selected_public_unwrap_gas_error(asset, cx));
        let submit_disabled = disabled
            || submitting
            || match mode {
                PublicActionMode::Shield => {
                    !fee_ready || self.native_shield_unavailable(mode).is_some()
                }
                PublicActionMode::Send if self.public_form.public_send_kind.is_advanced() => {
                    self.public_form.advanced_send_estimate_pending
                        || self.public_form.advanced_send_estimate.is_none()
                }
                PublicActionMode::Send | PublicActionMode::Wrap => !fee_ready,
                PublicActionMode::Unwrap => !fee_ready || unwrap_gas_error.is_some(),
            };
        let wrap_mode = matches!(mode, PublicActionMode::Wrap | PublicActionMode::Unwrap);
        let mut content = public_action_form(content_width, submit_disabled, move |window, cx| {
            keyboard_submit_root.update(cx, |root, cx| match mode {
                PublicActionMode::Shield => root.submit_public_shield_from_form(window, cx),
                PublicActionMode::Send | PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                    root.submit_public_send_from_form(window, cx);
                }
            });
        })
        .children(account.map(|account| {
            app_muted_text(format!("From {}", short_address(&account.address)))
                .font_family(APP_MONO_FONT_FAMILY)
        }))
        // Wrap and Unwrap are opened from their asset and never switch to Shield or Send.
        .when(!wrap_mode, |content| {
            content
                .child(ui::controls::public_action_mode_group(
                    "wallet-public-action-mode-toggle",
                    mode == PublicActionMode::Shield,
                    submitting || !self.selected_chain_has_railgun(),
                    move |shield, window, cx| {
                        let mode = if *shield {
                            PublicActionMode::Shield
                        } else {
                            PublicActionMode::Send
                        };
                        mode_root.update(cx, |root, cx| {
                            root.set_public_action_mode(mode, window, cx);
                        });
                    },
                ))
                .when(!self.selected_chain_has_railgun(), |content| {
                    content.child(app_muted_text(
                        "Shield is unavailable: this chain has no Railgun deployment.",
                    ))
                })
        });

        if account.is_some_and(|account| {
            matches!(account.source, PublicAccountSource::ExecutorDerived(_))
        }) && self.selected_wallet_source().is_hardware_derived()
        {
            content = content.child(app_muted_text("Signing requires fresh hardware derivation. This app then uses a temporary software EVM key; the device does not sign the EVM transaction.").whitespace_normal());
        }

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
                        self.mimic_railway_shield_effective(),
                        self.public_form.shielding,
                        self.mimic_railway_native_unavailable(),
                    ))
                    .when(self.public_shield_may_approve(), |content| {
                        content.child(render_public_shield_approval_control(
                            &submit_root,
                            self.public_form.shield_approval_mode,
                            self.mimic_railway_shield_effective(),
                            self.public_form.shielding,
                        ))
                    })
                    .children(self.render_native_shield_status(cx))
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
                        .disabled(submit_disabled)
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
                                    self.public_form.sending,
                                ))
                                .child(public_send_kind_segment_button(
                                    "wallet-public-send-contract-call".into(),
                                    "Contract call",
                                    send_kind == PublicSendKind::ContractCall,
                                    self.public_form.sending,
                                ))
                                .child(public_send_kind_segment_button(
                                    "wallet-public-send-deploy".into(),
                                    "Deploy",
                                    send_kind == PublicSendKind::Deploy,
                                    self.public_form.sending,
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
                    content =
                        content
                            .child(
                                labeled_field(
                                    format!(
                                        "Value ({})",
                                        self.configured_native_symbol(self.last_evm_chain)
                                    ),
                                    app_input(&self.public_form.advanced_send_value_input)
                                        .disabled(disabled || self.public_form.sending),
                                )
                                .children(
                                    self.public_form.advanced_send_value_error.as_ref().map(
                                        |error| {
                                            Alert::error(
                                                "wallet-public-send-advanced-value-error",
                                                error.to_string(),
                                            )
                                            .small()
                                        },
                                    ),
                                ),
                            )
                            .child(
                                labeled_field(
                                    data_label,
                                    gpui_component::input::Textarea::new(
                                        &self.public_form.advanced_send_data_input,
                                    )
                                    .bg(rgb(theme::SURFACE))
                                    .px(px(8.0))
                                    .w_full()
                                    .min_w(px(0.0))
                                    .font_family(APP_MONO_FONT_FAMILY)
                                    .text_size(APP_TEXT_SIZE)
                                    .disabled(disabled || self.public_form.sending),
                                )
                                .w_full()
                                .min_w(px(0.0))
                                .children(
                                    self.public_form.advanced_send_data_error.as_ref().map(
                                        |error| {
                                            Alert::error(
                                                "wallet-public-send-advanced-data-error",
                                                error.to_string(),
                                            )
                                            .small()
                                        },
                                    ),
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
                                    self.effective_chain_configs.get(self.last_evm_chain),
                                    estimate,
                                    self.public_broadcaster_anchor_cache
                                        .cached_native_usd_micro_value(
                                            self.last_evm_chain,
                                            estimate.expected_gas_cost,
                                        ),
                                    self.public_broadcaster_anchor_cache
                                        .cached_native_usd_micro_value(
                                            self.last_evm_chain,
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
                                        .disabled(submit_disabled)
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
                            .disabled(submit_disabled)
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
            PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                let (button_id, label, pending_label) = if mode == PublicActionMode::Wrap {
                    ("wallet-public-wrap", "Wrap", "Wrapping...")
                } else {
                    ("wallet-public-unwrap", "Unwrap", "Unwrapping...")
                };
                content = content
                    .child(render_public_action_amount_input(
                        max_root,
                        mode,
                        &self.public_form.send_amount_input,
                        send_amount_hint,
                        max_label,
                        disabled || self.public_form.sending,
                    ))
                    .child(render_eip1559_gas_fee_editor(
                        gas_fee_root,
                        &Eip1559GasFeeTarget::Public { mode },
                        self.public_form.gas_fee(mode),
                        disabled || self.public_form.sending,
                    ))
                    .child(render_public_action_fee_estimate(&fee_display, gas_pending))
                    .child(
                        app_button(
                            button_id,
                            if self.public_form.sending {
                                pending_label
                            } else {
                                label
                            },
                        )
                        .primary()
                        .small()
                        .loading(self.public_form.sending)
                        .disabled(submit_disabled)
                        .on_click(move |_event, window, cx| {
                            submit_root.update(cx, |root, cx| {
                                root.submit_public_send_from_form(window, cx);
                            });
                        }),
                    );
                if show_form_errors
                    && let Some(error) = self
                        .public_form
                        .send_error
                        .as_deref()
                        .or(unwrap_gas_error.as_deref())
                {
                    content = content
                        .child(Alert::error("wallet-public-wrap-error", error.to_string()).small());
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
                self.tron_send_pending_detail(),
            ));
        }

        content
    }
}
