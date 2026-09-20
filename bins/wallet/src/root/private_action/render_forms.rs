use super::{
    Alert, ButtonVariants, DeliveryFormKind, DeliveryMode, Disableable, Entity, ParentElement,
    Pixels, PublicBroadcasterResultKind, SendResult, Sizable, SponsoredSelfBroadcastSessionOutcome,
    Styled, UnshieldAssetKey, UnshieldResult, WalletRoot, app_button, app_muted_text,
    app_strong_text, delivery_element_id, div, effective_self_broadcast_funding_mode,
    fee_policy_eligible_public_broadcasters, format_form_error_for_asset,
    is_effective_wrapped_native_token, labeled_field, private_action_amount_input,
    private_broadcaster_closed_active_progress, public_broadcaster_cost_status,
    public_broadcaster_fee_token_warning, public_broadcaster_submit_disabled_for_fee_token_options,
    px, render_delivery_selector, render_fee_mode_toggle, render_private_action_metrics,
    render_private_broadcaster_status_notice, render_private_self_broadcast_status_notice,
    render_private_submission_active_status_notice, render_public_broadcaster_cost_estimate,
    render_public_broadcaster_cost_status, render_public_broadcaster_settings,
    render_recipient_picker, render_self_broadcast_settings, render_send_result,
    render_sponsored_funding_estimate, render_unshield_generating_status,
    render_unshield_native_top_up_control, render_unshield_output_toggle, render_unshield_result,
    selected_broadcaster_fee_warning, send_element_id,
    should_render_public_broadcaster_cost_preview, sponsored_estimate_allows_submission,
    sponsored_funding_choice_visible, sponsored_funding_enabled,
    sponsored_self_broadcast_availability_reason, unshield_element_id,
};

impl WalletRoot {
    pub(in crate::root) fn render_send_form(
        &self,
        root: Entity<Self>,
        key: UnshieldAssetKey,
        content_width: Pixels,
    ) -> gpui::Div {
        let Some(form) = self.send_forms.get(&key) else {
            return div();
        };
        let asset = &form.asset;
        let unit_hint = if asset.decimals.is_some() {
            format!("{} amount", asset.label)
        } else {
            "Raw base units for this unknown token".to_string()
        };
        let delivery_root = root.clone();
        let metrics_root = root.clone();
        let chooser_root = root.clone();
        let estimate_root = root.clone();
        let progress_root = root.clone();
        let recipient_root = root.clone();
        let delivery_submit_root = root.clone();
        let amount_submit_root = root.clone();
        let submit_root = root;
        let self_broadcast_accounts = self.active_self_broadcast_gas_payer_accounts();
        let effective_chain = self.effective_chain_configs.get(asset.chain_id);
        let show_sponsored_funding_choice = sponsored_funding_choice_visible(effective_chain);
        let sponsorship_unavailable_reason = show_sponsored_funding_choice
            .then(|| sponsored_self_broadcast_availability_reason(effective_chain))
            .flatten();
        let sponsorship_enabled = sponsored_funding_enabled(
            show_sponsored_funding_choice,
            sponsorship_unavailable_reason,
        );
        let mut public_broadcaster_submit_disabled = false;
        let mut self_broadcast_submit_disabled = false;
        let mut sponsored_funding_estimate = None;
        let generation_ready = self.private_action_generation_ready(asset.chain_id);
        let public_broadcaster_submitted = matches!(
            form.result.as_ref(),
            Some(SendResult::PublicBroadcaster(result))
                if matches!(result.result, PublicBroadcasterResultKind::Submitted { .. })
        );
        let self_broadcast_submitted =
            matches!(form.result.as_ref(), Some(SendResult::SelfBroadcast(_)));
        let sponsored_submitted = matches!(
            form.result.as_ref(),
            Some(SendResult::Sponsored(result))
                if matches!(
                    result.outcome,
                    SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(ref receipt)
                        if receipt.status
                )
        );
        let submitted =
            public_broadcaster_submitted || self_broadcast_submitted || sponsored_submitted;
        let recipient_options = self.private_send_recipient_options();

        let mut card =
            div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .child(render_private_action_metrics(
                    metrics_root,
                    key,
                    DeliveryFormKind::Send,
                    asset,
                    form.generating,
                    content_width,
                ));

        if asset.total > asset.max_batched {
            card = card.child(
                ui::private_action::warning(
                    send_element_id(key, "spend-capacity-warning"),
                    ui::private_action::spend_capacity_warning(false),
                )
                .small(),
            );
        }

        card = card.child(render_delivery_selector(
            delivery_root,
            key,
            DeliveryFormKind::Send,
            form.delivery_mode,
            form.generating || form.gateway_execution.is_some(),
            !self_broadcast_accounts.is_empty(),
            sponsorship_enabled,
        ));
        if form.delivery_mode == DeliveryMode::PublicBroadcaster {
            let policy = self.public_broadcaster_fee_policy(form.allow_suspicious_broadcasters);
            let trust_filter =
                self.public_broadcaster_trust_filter(form.favorites_only_broadcasters);
            let fee_rows = self.monitor_fee_rows();
            let fee_token_options = self.current_public_broadcaster_fee_token_options(
                asset.chain_id,
                false,
                false,
                form.favorites_only_broadcasters,
                policy,
            );
            public_broadcaster_submit_disabled =
                public_broadcaster_submit_disabled_for_fee_token_options(
                    &fee_token_options,
                    form.selected_fee_token,
                );
            let candidates = self.current_public_broadcaster_candidates(
                asset.chain_id,
                form.selected_fee_token,
                false,
                false,
                form.favorites_only_broadcasters,
                policy,
            );
            let visible_candidates = fee_policy_eligible_public_broadcasters(&candidates, policy);
            if let Some(warning) = public_broadcaster_fee_token_warning(
                &fee_rows,
                asset.chain_id,
                &fee_token_options,
                form.selected_fee_token,
                &trust_filter,
            ) {
                card = card.child(
                    ui::private_action::warning(
                        delivery_element_id(key, DeliveryFormKind::Send, "fee-token-warning"),
                        warning,
                    )
                    .small(),
                );
            }
            card = card.child(render_public_broadcaster_settings(
                chooser_root.clone(),
                key,
                DeliveryFormKind::Send,
                form.allow_suspicious_broadcasters,
                form.favorites_only_broadcasters,
                asset.token,
                form.fee_mode,
                &form.broadcaster_choice,
                &visible_candidates,
                &fee_token_options,
                form.selected_fee_token,
                form.generating,
            ));
            if let Some(warning) = selected_broadcaster_fee_warning(
                &form.broadcaster_choice,
                &candidates,
                form.allow_suspicious_broadcasters,
            ) {
                card = card.child(
                    ui::private_action::warning(
                        delivery_element_id(key, DeliveryFormKind::Send, "fee-policy-warning"),
                        warning,
                    )
                    .small(),
                );
            }
        } else if form.delivery_mode == DeliveryMode::SelfBroadcast {
            sponsored_funding_estimate.clone_from(&form.sponsored_funding_estimate);
            let effective_funding =
                effective_self_broadcast_funding_mode(effective_chain, form.self_broadcast_funding);
            let sponsored_estimate_ready = sponsored_estimate_allows_submission(
                effective_funding,
                sponsored_funding_estimate.as_ref(),
                form.sponsored_estimate_pending,
            );
            self_broadcast_submit_disabled = form
                .self_broadcast_gas_payer_uuid
                .as_deref()
                .and_then(|uuid| self.selected_self_broadcast_gas_payer_account(Some(uuid)))
                .is_none()
                || !sponsored_estimate_ready;
        }

        let submit_disabled = !generation_ready
            || form.generating
            || public_broadcaster_submit_disabled
            || self_broadcast_submit_disabled
            || submitted;

        if form.delivery_mode == DeliveryMode::SelfBroadcast {
            card = card.child(render_self_broadcast_settings(
                chooser_root,
                key,
                DeliveryFormKind::Send,
                &self_broadcast_accounts,
                form.self_broadcast_gas_payer_uuid.as_deref(),
                self.public_balance_snapshot.as_deref(),
                &form.self_broadcast_gas_payer_select,
                &form.self_broadcast_gas_fee,
                form.self_broadcast_funding,
                form.sponsored_incentive,
                &form.sponsored_custom_incentive_input,
                show_sponsored_funding_choice,
                sponsorship_unavailable_reason,
                form.generating,
                !submit_disabled,
                move |window, cx| {
                    delivery_submit_root.update(cx, |root, cx| {
                        root.generate_send_calldata_from_form(key, window, cx);
                    });
                },
            ));
        }

        card = card.child(
            div()
                .flex()
                .items_end()
                .gap_3()
                .child(
                    labeled_field(
                        "Recipient 0zk address",
                        render_recipient_picker(
                            recipient_root,
                            key,
                            DeliveryFormKind::Send,
                            &form.recipient_input,
                            &form.recipient_value,
                            form.recipient_suggestions_open,
                            form.recipient_suggestion_index,
                            &form.recipient_suggestions_scroll,
                            &recipient_options,
                            form.generating,
                        ),
                    )
                    .flex_1()
                    .min_w(px(0.0)),
                )
                .child(
                    labeled_field(
                        unit_hint,
                        private_action_amount_input(
                            &form.amount_input,
                            form.generating,
                            !submit_disabled,
                            move |window, cx| {
                                amount_submit_root.update(cx, |root, cx| {
                                    root.generate_send_calldata_from_form(key, window, cx);
                                });
                            },
                        ),
                    )
                    .w(px(220.0)),
                ),
        );
        if let Some(estimate_state) = sponsored_funding_estimate {
            card = card.child(render_sponsored_funding_estimate(
                estimate_root.clone(),
                key,
                DeliveryFormKind::Send,
                &self.sponsored_funding_estimate_display(&estimate_state),
                form.sponsored_funding_breakdown_open,
            ));
        }
        card = card.child(
            div().flex().items_end().gap_3().justify_end().child(
                app_button(
                    send_element_id(key, "generate"),
                    if form.generating {
                        "Preparing..."
                    } else if submitted {
                        "Submitted"
                    } else if form.delivery_mode == DeliveryMode::PublicBroadcaster {
                        "Submit via broadcaster"
                    } else if form.delivery_mode == DeliveryMode::SelfBroadcast {
                        "Self-broadcast"
                    } else {
                        "Generate calldata"
                    },
                )
                .primary()
                .loading(form.generating)
                .disabled(submit_disabled)
                .tooltip(if generation_ready {
                    "Prepare private transaction"
                } else {
                    "Generation is available after wallet sync finishes"
                })
                .on_click(move |_event, window, cx| {
                    submit_root.update(cx, |root, cx| {
                        root.generate_send_calldata_from_form(key, window, cx);
                    });
                }),
            ),
        );

        if form.delivery_mode == DeliveryMode::PublicBroadcaster
            && form.custom_fee_amount.is_some()
            && (form.cost_estimate.is_none() || form.error.is_some())
            && !form.generating
            && form.gateway_execution.is_none()
            && form.result.is_none()
        {
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(app_muted_text("Custom transaction fee"))
                    .child(super::fee_editor::fee_edit_button(
                        estimate_root.clone(),
                        DeliveryFormKind::Send,
                        key,
                    )),
            );
        }

        if should_render_public_broadcaster_cost_preview(
            form.delivery_mode,
            form.result.is_some(),
            form.error.is_some(),
        ) {
            if let Some(estimate) = form.cost_estimate.as_ref() {
                let anchor_rate = self
                    .public_broadcaster_anchor_cache
                    .cached_rate(asset.chain_id, estimate.fee_token);
                card = card.child(render_public_broadcaster_cost_estimate(
                    estimate_root,
                    key,
                    DeliveryFormKind::Send,
                    asset,
                    estimate,
                    anchor_rate,
                    Some(&self.effective_token_registry),
                    &self.public_broadcaster_anchor_cache,
                    form.transaction_fee_breakdown_open,
                    form.estimating_cost,
                    form.custom_fee_amount,
                    !form.generating && form.gateway_execution.is_none(),
                ));
            } else if let Some(status) =
                public_broadcaster_cost_status(form.cost_estimate_pending, form.estimating_cost)
            {
                card = card.child(render_public_broadcaster_cost_status(
                    self.unshield_spinner_tick,
                    status,
                ));
            }
        }

        if form.generating
            && matches!(
                form.delivery_mode,
                DeliveryMode::PublicBroadcaster | DeliveryMode::SelfBroadcast
            )
            && let Some(active) = private_broadcaster_closed_active_progress(
                self.private_broadcaster_progress.as_ref(),
                DeliveryFormKind::Send,
                key,
                form.generation_id,
            )
        {
            card = card.child(render_private_submission_active_status_notice(
                progress_root.clone(),
                key,
                DeliveryFormKind::Send,
                &active,
            ));
        }

        if form.generating && form.delivery_mode == DeliveryMode::ManualCalldata {
            card = card.child(render_unshield_generating_status(
                self.unshield_spinner_tick,
                form.generation_stage,
            ));
        }

        if let Some(error) = form.error.as_ref() {
            card = card.child(
                Alert::error(
                    send_element_id(key, "form-error"),
                    format_form_error_for_asset(
                        error,
                        asset,
                        form.selected_fee_token,
                        Some(&self.effective_token_registry),
                    ),
                )
                .small(),
            );
        }

        if let Some(result) = form.result.as_ref() {
            match result {
                SendResult::Manual(result) => {
                    card = card.child(render_send_result(key, result));
                }
                SendResult::PublicBroadcaster(result) => {
                    card = card.child(render_private_broadcaster_status_notice(
                        progress_root,
                        key,
                        DeliveryFormKind::Send,
                        &result.result,
                    ));
                }
                SendResult::SelfBroadcast(result) => {
                    card = card.child(div().flex().flex_col().gap_2().child(
                        render_private_self_broadcast_status_notice(
                            progress_root,
                            key,
                            DeliveryFormKind::Send,
                            result,
                        ),
                    ));
                }
                SendResult::Sponsored(result) => {
                    card = card.child(render_sponsored_self_broadcast_result(result));
                }
            }
        }

        card
    }

    pub(in crate::root) fn render_unshield_form(
        &self,
        root: Entity<Self>,
        key: UnshieldAssetKey,
        content_width: Pixels,
    ) -> gpui::Div {
        let Some(form) = self.unshield_forms.get(&key) else {
            return div();
        };
        let asset = &form.asset;
        let unwrap_supported = is_effective_wrapped_native_token(
            &self.effective_chain_configs,
            asset.chain_id,
            asset.token,
        );
        let unit_hint = if asset.decimals.is_some() {
            format!("{} amount", asset.label)
        } else {
            "Raw base units for this unknown token".to_string()
        };
        let delivery_root = root.clone();
        let fee_mode_root = root.clone();
        let metrics_root = root.clone();
        let chooser_root = root.clone();
        let output_root = root.clone();
        let estimate_root = root.clone();
        let progress_root = root.clone();
        let recipient_root = root.clone();
        let top_up_root = root.clone();
        let delivery_submit_root = root.clone();
        let amount_submit_root = root.clone();
        let submit_root = root;
        let self_broadcast_accounts = self.active_self_broadcast_gas_payer_accounts();
        let effective_chain = self.effective_chain_configs.get(asset.chain_id);
        let show_sponsored_funding_choice = sponsored_funding_choice_visible(effective_chain);
        let sponsorship_unavailable_reason = show_sponsored_funding_choice
            .then(|| sponsored_self_broadcast_availability_reason(effective_chain))
            .flatten();
        let sponsorship_enabled = sponsored_funding_enabled(
            show_sponsored_funding_choice,
            sponsorship_unavailable_reason,
        );
        let mut public_broadcaster_submit_disabled = false;
        let mut self_broadcast_submit_disabled = false;
        let mut sponsored_funding_estimate = None;
        let generation_ready = self.private_action_generation_ready(asset.chain_id);
        let public_broadcaster_submitted = matches!(
            form.result.as_ref(),
            Some(UnshieldResult::PublicBroadcaster(result))
                if matches!(result.result, PublicBroadcasterResultKind::Submitted { .. })
        );
        let self_broadcast_submitted =
            matches!(form.result.as_ref(), Some(UnshieldResult::SelfBroadcast(_)));
        let sponsored_submitted = matches!(
            form.result.as_ref(),
            Some(UnshieldResult::Sponsored(result))
                if matches!(
                    result.outcome,
                    SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(ref receipt)
                        if receipt.status
                )
        );
        let submitted =
            public_broadcaster_submitted || self_broadcast_submitted || sponsored_submitted;
        let recipient_options = self.private_unshield_recipient_options();

        let mut card =
            div()
                .w(content_width)
                .flex()
                .flex_col()
                .gap_3()
                .child(render_private_action_metrics(
                    metrics_root,
                    key,
                    DeliveryFormKind::Unshield,
                    asset,
                    form.generating,
                    content_width,
                ));

        if form.native_top_up_enabled {
            card = card.child(ui::private_action::warning(
                unshield_element_id(key, "top-up-linkage"),
                ui::private_action::NATIVE_TOP_UP_LINKAGE_WARNING,
            ));
        }

        if asset.total > asset.max_batched {
            card = card.child(
                ui::private_action::warning(
                    unshield_element_id(key, "spend-capacity-warning"),
                    ui::private_action::spend_capacity_warning(true),
                )
                .small(),
            );
        }

        card = card.child(render_delivery_selector(
            delivery_root,
            key,
            DeliveryFormKind::Unshield,
            form.delivery_mode,
            form.generating || form.gateway_execution.is_some(),
            !self_broadcast_accounts.is_empty(),
            sponsorship_enabled,
        ));
        if form.delivery_mode == DeliveryMode::PublicBroadcaster {
            let policy = self.public_broadcaster_fee_policy(form.allow_suspicious_broadcasters);
            let trust_filter =
                self.public_broadcaster_trust_filter(form.favorites_only_broadcasters);
            let fee_rows = self.monitor_fee_rows();
            let native_top_up = form.native_top_up_enabled && form.native_top_up.is_some();
            let fee_token_options = self.current_public_broadcaster_fee_token_options(
                asset.chain_id,
                form.unwrap,
                native_top_up,
                form.favorites_only_broadcasters,
                policy,
            );
            public_broadcaster_submit_disabled =
                public_broadcaster_submit_disabled_for_fee_token_options(
                    &fee_token_options,
                    form.selected_fee_token,
                );
            let candidates = self.current_public_broadcaster_candidates(
                asset.chain_id,
                form.selected_fee_token,
                form.unwrap,
                native_top_up,
                form.favorites_only_broadcasters,
                policy,
            );
            let visible_candidates = fee_policy_eligible_public_broadcasters(&candidates, policy);
            if let Some(warning) = public_broadcaster_fee_token_warning(
                &fee_rows,
                asset.chain_id,
                &fee_token_options,
                form.selected_fee_token,
                &trust_filter,
            ) {
                card = card.child(
                    ui::private_action::warning(
                        delivery_element_id(key, DeliveryFormKind::Unshield, "fee-token-warning"),
                        warning,
                    )
                    .small(),
                );
            }
            card = card.child(render_public_broadcaster_settings(
                chooser_root.clone(),
                key,
                DeliveryFormKind::Unshield,
                form.allow_suspicious_broadcasters,
                form.favorites_only_broadcasters,
                asset.token,
                form.fee_mode,
                &form.broadcaster_choice,
                &visible_candidates,
                &fee_token_options,
                form.selected_fee_token,
                form.generating,
            ));
            if let Some(warning) = selected_broadcaster_fee_warning(
                &form.broadcaster_choice,
                &candidates,
                form.allow_suspicious_broadcasters,
            ) {
                card = card.child(
                    ui::private_action::warning(
                        delivery_element_id(key, DeliveryFormKind::Unshield, "fee-policy-warning"),
                        warning,
                    )
                    .small(),
                );
            }
        } else if form.delivery_mode == DeliveryMode::SelfBroadcast {
            sponsored_funding_estimate.clone_from(&form.sponsored_funding_estimate);
            let effective_funding =
                effective_self_broadcast_funding_mode(effective_chain, form.self_broadcast_funding);
            let sponsored_estimate_ready = sponsored_estimate_allows_submission(
                effective_funding,
                sponsored_funding_estimate.as_ref(),
                form.sponsored_estimate_pending,
            );
            self_broadcast_submit_disabled = form
                .self_broadcast_gas_payer_uuid
                .as_deref()
                .and_then(|uuid| self.selected_self_broadcast_gas_payer_account(Some(uuid)))
                .is_none()
                || !sponsored_estimate_ready;
        }

        let submit_disabled = !generation_ready
            || form.generating
            || public_broadcaster_submit_disabled
            || self_broadcast_submit_disabled
            || submitted;

        if form.delivery_mode == DeliveryMode::SelfBroadcast {
            card = card.child(render_self_broadcast_settings(
                chooser_root,
                key,
                DeliveryFormKind::Unshield,
                &self_broadcast_accounts,
                form.self_broadcast_gas_payer_uuid.as_deref(),
                self.public_balance_snapshot.as_deref(),
                &form.self_broadcast_gas_payer_select,
                &form.self_broadcast_gas_fee,
                form.self_broadcast_funding,
                form.sponsored_incentive,
                &form.sponsored_custom_incentive_input,
                show_sponsored_funding_choice,
                sponsorship_unavailable_reason,
                form.generating,
                !submit_disabled,
                move |window, cx| {
                    delivery_submit_root.update(cx, |root, cx| {
                        root.generate_unshield_calldata_from_form(key, window, cx);
                    });
                },
            ));
        }

        card = card.child(render_fee_mode_toggle(
            fee_mode_root,
            key,
            DeliveryFormKind::Unshield,
            form.delivery_mode,
            form.fee_mode,
            form.generating,
        ));

        card = card
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_3()
                    .child(
                        labeled_field(
                            "Recipient",
                            render_recipient_picker(
                                recipient_root,
                                key,
                                DeliveryFormKind::Unshield,
                                &form.recipient_input,
                                &form.recipient_value,
                                form.recipient_suggestions_open,
                                form.recipient_suggestion_index,
                                &form.recipient_suggestions_scroll,
                                &recipient_options,
                                form.generating,
                            ),
                        )
                        .flex_1()
                        .min_w(px(0.0)),
                    )
                    .children(unwrap_supported.then(|| {
                        render_unshield_output_toggle(
                            output_root.clone(),
                            key,
                            asset.chain_id,
                            form.unwrap,
                            form.generating,
                        )
                    }))
                    .child(
                        labeled_field(
                            unit_hint,
                            private_action_amount_input(
                                &form.amount_input,
                                form.generating,
                                !submit_disabled,
                                move |window, cx| {
                                    amount_submit_root.update(cx, |root, cx| {
                                        root.generate_unshield_calldata_from_form(key, window, cx);
                                    });
                                },
                            ),
                        )
                        .w(px(220.0)),
                    ),
            )
            .child(render_unshield_native_top_up_control(
                top_up_root,
                key,
                form.native_top_up.as_ref(),
                form.native_top_up_enabled,
                form.generating,
                Some(&self.effective_token_registry),
            ));
        if let Some(estimate_state) = sponsored_funding_estimate {
            card = card.child(render_sponsored_funding_estimate(
                estimate_root.clone(),
                key,
                DeliveryFormKind::Unshield,
                &self.sponsored_funding_estimate_display(&estimate_state),
                form.sponsored_funding_breakdown_open,
            ));
        }
        card = card.child(
            div().flex().items_end().gap_3().justify_end().child(
                app_button(
                    unshield_element_id(key, "generate"),
                    if form.generating {
                        "Preparing..."
                    } else if submitted {
                        "Submitted"
                    } else if form.delivery_mode == DeliveryMode::PublicBroadcaster {
                        "Submit via broadcaster"
                    } else if form.delivery_mode == DeliveryMode::SelfBroadcast {
                        "Self-broadcast"
                    } else {
                        "Generate calldata"
                    },
                )
                .primary()
                .loading(form.generating)
                .disabled(submit_disabled)
                .tooltip(if generation_ready {
                    "Prepare private transaction"
                } else {
                    "Generation is available after wallet sync finishes"
                })
                .on_click(move |_event, window, cx| {
                    submit_root.update(cx, |root, cx| {
                        root.generate_unshield_calldata_from_form(key, window, cx);
                    });
                }),
            ),
        );

        if form.delivery_mode == DeliveryMode::PublicBroadcaster
            && form.custom_fee_amount.is_some()
            && (form.cost_estimate.is_none() || form.error.is_some())
            && !form.generating
            && form.gateway_execution.is_none()
            && form.result.is_none()
        {
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(app_muted_text("Custom transaction fee"))
                    .child(super::fee_editor::fee_edit_button(
                        estimate_root.clone(),
                        DeliveryFormKind::Unshield,
                        key,
                    )),
            );
        }

        if should_render_public_broadcaster_cost_preview(
            form.delivery_mode,
            form.result.is_some(),
            form.error.is_some(),
        ) {
            if let Some(estimate) = form.cost_estimate.as_ref() {
                let anchor_rate = self
                    .public_broadcaster_anchor_cache
                    .cached_rate(asset.chain_id, estimate.fee_token);
                card = card.child(render_public_broadcaster_cost_estimate(
                    estimate_root,
                    key,
                    DeliveryFormKind::Unshield,
                    asset,
                    estimate,
                    anchor_rate,
                    Some(&self.effective_token_registry),
                    &self.public_broadcaster_anchor_cache,
                    form.transaction_fee_breakdown_open,
                    form.estimating_cost,
                    form.custom_fee_amount,
                    !form.generating && form.gateway_execution.is_none(),
                ));
            } else if let Some(status) =
                public_broadcaster_cost_status(form.cost_estimate_pending, form.estimating_cost)
            {
                card = card.child(render_public_broadcaster_cost_status(
                    self.unshield_spinner_tick,
                    status,
                ));
            }
        }

        if form.generating
            && matches!(
                form.delivery_mode,
                DeliveryMode::PublicBroadcaster | DeliveryMode::SelfBroadcast
            )
            && let Some(active) = private_broadcaster_closed_active_progress(
                self.private_broadcaster_progress.as_ref(),
                DeliveryFormKind::Unshield,
                key,
                form.generation_id,
            )
        {
            card = card.child(render_private_submission_active_status_notice(
                progress_root.clone(),
                key,
                DeliveryFormKind::Unshield,
                &active,
            ));
        }

        if form.generating && form.delivery_mode == DeliveryMode::ManualCalldata {
            card = card.child(render_unshield_generating_status(
                self.unshield_spinner_tick,
                form.generation_stage,
            ));
        }

        if let Some(error) = form.error.as_ref() {
            card = card.child(
                Alert::error(
                    unshield_element_id(key, "form-error"),
                    format_form_error_for_asset(
                        error,
                        asset,
                        form.selected_fee_token,
                        Some(&self.effective_token_registry),
                    ),
                )
                .small(),
            );
        }

        if let Some(result) = form.result.as_ref() {
            match result {
                UnshieldResult::Manual(result) => {
                    card = card.child(render_unshield_result(key, asset, result));
                }
                UnshieldResult::PublicBroadcaster(result) => {
                    card = card.child(render_private_broadcaster_status_notice(
                        progress_root,
                        key,
                        DeliveryFormKind::Unshield,
                        &result.result,
                    ));
                }
                UnshieldResult::SelfBroadcast(result) => {
                    card = card.child(div().flex().flex_col().gap_2().child(
                        render_private_self_broadcast_status_notice(
                            progress_root,
                            key,
                            DeliveryFormKind::Unshield,
                            result,
                        ),
                    ));
                }
                UnshieldResult::Sponsored(result) => {
                    card = card.child(render_sponsored_self_broadcast_result(result));
                }
            }
        }

        card
    }
}

fn render_sponsored_self_broadcast_result(
    result: &wallet_ops::DesktopSponsoredSelfBroadcastResult,
) -> gpui::Div {
    let authorization = result.prepared.authorization;
    let status = match &result.outcome {
        SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt) if receipt.status => {
            "Sponsored transaction confirmed"
        }
        SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(_) => {
            "Sponsored transaction reverted"
        }
        SponsoredSelfBroadcastSessionOutcome::Stopped { .. } => "Sponsored retries stopped locally",
    };
    let mut content = div()
        .flex()
        .flex_col()
        .gap_1()
        .p(px(10.0))
        .child(app_strong_text(status))
        .child(app_muted_text(format!(
            "Builder payment: {} native base units",
            authorization.builder_payment
        )))
        .child(app_muted_text(format!(
            "Private wrapped-native spend: {} (protocol fee {})",
            authorization.gross_wrapped_native_spend, authorization.protocol_fee
        )))
        .child(app_muted_text(format!(
            "Outer gas limit: {} · signer: {}",
            authorization.transaction_gas_limit,
            authorization.signer.to_checksum(None)
        )))
        .child(app_muted_text(
            "Priority fee is paid to the block builder in addition to the private builder payment.",
        ));
    if let SponsoredSelfBroadcastSessionOutcome::CanonicalReceipt(receipt) = &result.outcome {
        content = content
            .child(app_muted_text(format!("Transaction: {}", receipt.tx_hash)))
            .child(app_muted_text(format!("Block: {}", receipt.block_number)));
    }
    content
}
