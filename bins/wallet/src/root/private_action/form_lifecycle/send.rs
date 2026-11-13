use super::*;

impl WalletRoot {
    pub(in crate::root) fn close_send_form(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(form) = self.send_forms.remove(&key)
            && let Some(execution) = form.gateway_execution
        {
            execution.cancel_review();
        }
        if self
            .private_action_form
            .as_ref()
            .is_some_and(|form| form.kind == DeliveryFormKind::Send && form.key == key)
        {
            self.private_action_form = None;
            self.broadcaster_picker = None;
        }
        if self
            .private_broadcaster_progress
            .as_ref()
            .is_some_and(|progress| progress.kind == DeliveryFormKind::Send && progress.key == key)
        {
            self.clear_private_broadcaster_progress_state();
        }
        cx.notify();
    }

    pub(in crate::root) fn open_send_form(
        &mut self,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        // Debug UI fixture: a `broadcaster-progress:` mode opens its progress dialog in place of
        // the form, and `broadcaster-review` its review.
        #[cfg(debug_assertions)]
        {
            if self.open_ui_fixture_broadcaster_progress(&asset, window, cx) {
                return;
            }
        }
        let key = UnshieldAssetKey::from_asset(&asset);
        self.initialize_send_form(asset, window, cx);
        let focus_recipient_input = self.send_forms[&key].recipient_input.clone();
        self.private_action_form = Some(PrivateActionFormState {
            focus: cx.focus_handle(),
            kind: DeliveryFormKind::Send,
            key,
        });
        self.refresh_public_broadcaster_anchor(DeliveryFormKind::Send, key, cx);
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
        Self::open_private_action_dialog(DeliveryFormKind::Send, key, "Send", window, cx);
        cx.defer_in(window, move |root, window, cx| {
            if root
                .private_action_form
                .as_ref()
                .is_some_and(|form| form.kind == DeliveryFormKind::Send && form.key == key)
                && window.has_active_dialog(cx)
            {
                focus_recipient_input
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
        });
        cx.notify();
    }

    pub(in crate::root) fn initialize_send_form(
        &mut self,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let key = UnshieldAssetKey::from_asset(&asset);
        let amount_input = new_text_input(window, cx, "amount");
        let recipient_input = new_text_input(window, cx, "0zk recipient");
        let (asset_select, asset_select_items) = self.new_private_action_asset_select(
            DeliveryFormKind::Send,
            key.chain_id,
            key.token,
            window,
            cx,
        );
        let self_broadcast_gas_payer_uuid = self.default_self_broadcast_gas_payer_uuid();
        let gas_payer_select = self.new_self_broadcast_gas_payer_select(
            key.chain_id,
            self_broadcast_gas_payer_uuid.as_deref(),
            window,
            cx,
        );
        let gas_fee_editor = Eip1559GasFeeEditorState::new(window, cx);
        let sponsored_custom_incentive_input =
            new_prefilled_input(window, cx, "incentive %", "25".to_owned());
        cx.subscribe_in(
            &recipient_input,
            window,
            move |this, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    if let Some(form) = this.send_forms.get_mut(&key) {
                        if form.recipient_value.as_ref() == input.read(cx).value().as_ref() {
                            return;
                        }
                        form.recipient_value = Arc::from(input.read(cx).value().as_ref());
                    }
                    this.update_recipient_suggestions_for_input_change(
                        DeliveryFormKind::Send,
                        key,
                        cx,
                    );
                    this.clear_send_form_text_edit_state(key, cx);
                    this.debounce_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
                    this.debounce_sponsored_funding_estimate(DeliveryFormKind::Send, key, cx);
                } else if matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_selected_recipient_suggestion(
                        DeliveryFormKind::Send,
                        key,
                        window,
                        cx,
                    );
                }
            },
        )
        .detach();
        cx.subscribe(
            &amount_input,
            move |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    if this.consume_programmatic_amount_input_change(
                        DeliveryFormKind::Send,
                        key,
                        cx,
                    ) {
                        return;
                    }
                    this.clear_send_form_text_edit_state(key, cx);
                    this.debounce_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
                    this.debounce_sponsored_funding_estimate(DeliveryFormKind::Send, key, cx);
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &asset_select,
            window,
            move |this,
                  _select,
                  event: &SelectEvent<SearchableVec<PrivateActionAssetSelectItem>>,
                  window,
                  cx| {
                if let SelectEvent::Confirm(Some(token)) = event {
                    this.set_private_action_asset_token(
                        DeliveryFormKind::Send,
                        key,
                        *token,
                        window,
                        cx,
                    );
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &gas_payer_select,
            window,
            move |this,
                  _select,
                  event: &SelectEvent<FullWidthSelectItems<SelfBroadcastGasPayerSelectItem>>,
                  window,
                  cx| {
                if let SelectEvent::Confirm(Some(uuid)) = event {
                    this.set_self_broadcast_gas_payer(
                        DeliveryFormKind::Send,
                        key,
                        Some(Arc::clone(uuid)),
                        window,
                        cx,
                    );
                }
            },
        )
        .detach();
        cx.subscribe(
            &gas_fee_editor.max_fee_input,
            move |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.sync_self_broadcast_custom_gas_fee_error(DeliveryFormKind::Send, key, cx);
                    this.clear_send_form_text_edit_state(key, cx);
                    this.debounce_sponsored_funding_estimate(DeliveryFormKind::Send, key, cx);
                }
            },
        )
        .detach();
        cx.subscribe(
            &gas_fee_editor.max_priority_fee_input,
            move |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.sync_self_broadcast_custom_gas_fee_error(DeliveryFormKind::Send, key, cx);
                    this.clear_send_form_text_edit_state(key, cx);
                    this.debounce_sponsored_funding_estimate(DeliveryFormKind::Send, key, cx);
                }
            },
        )
        .detach();
        cx.subscribe(
            &sponsored_custom_incentive_input,
            move |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.set_sponsored_custom_incentive_from_text(
                        DeliveryFormKind::Send,
                        key,
                        input.read(cx).value().as_ref(),
                        cx,
                    );
                }
            },
        )
        .detach();
        self.send_forms.clear();
        self.unshield_forms.clear();
        self.clear_private_broadcaster_progress_state();
        self.broadcaster_picker = None;
        let selected_fee_token =
            self.default_public_broadcaster_fee_token(key.chain_id, key.token, false, false);
        self.send_forms.insert(
            key,
            SendFormState {
                custom_fee_amount: None,
                gateway_execution: None,
                gateway_estimated_at: None,
                asset,
                recipient_input,
                recipient_value: Arc::from(""),
                recipient_suggestions_open: false,
                recipient_suggestion_index: None,
                recipient_suggestions_scroll: ScrollHandle::new(),
                amount_input,
                asset_select,
                asset_select_items,
                delivery_mode: DeliveryMode::PublicBroadcaster,
                self_broadcast_gas_payer_uuid,
                self_broadcast_gas_payer_select: gas_payer_select,
                self_broadcast_gas_fee: gas_fee_editor,
                self_broadcast_funding: SelfBroadcastFundingMode::PublicBalance,
                sponsored_incentive: SponsoredIncentive::Standard,
                sponsored_custom_incentive_input,
                sponsored_funding_estimate: None,
                sponsored_funding_breakdown_open: false,
                sponsored_estimate_id: 0,
                sponsored_estimate_pending: false,
                self_broadcast_estimated_native_gas_cost: None,
                selected_fee_token,
                broadcaster_choice: BroadcasterChoice::Random,
                fee_mode: FeeHandlingMode::DeductFromAmount,
                allow_suspicious_broadcasters: self.default_allow_suspicious_broadcasters,
                favorites_only_broadcasters: false,
                transaction_fee_breakdown_open: true,
                pending_programmatic_amount_input: None,
                cost_estimate_pending: false,
                estimating_cost: false,
                cost_estimate: None,
                shown_broadcaster: None,
                refused_broadcasters: Vec::new(),
                input_pin: None,
                input_pin_conflict: false,
                estimate_id: 0,
                generation_id: 0,
                generating: false,
                generation_stage: TransactionGenerationStage::default(),
                error: None,
                result: None,
            },
        );
    }

    pub(in crate::root) fn clear_send_form_text_edit_state(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        if form.generating
            || (form.result.is_none()
                && form.error.is_none()
                && form.cost_estimate.is_none()
                && !form.cost_estimate_pending
                && !form.estimating_cost)
        {
            return;
        }
        form.result = None;
        form.error = None;
        form.cost_estimate = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        cx.notify();
    }

    pub(in crate::root) fn set_send_asset(
        &mut self,
        key: UnshieldAssetKey,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((
            current_token,
            selected_fee_token,
            choice,
            fee_mode,
            allow_suspicious,
            favorites_only,
            delivery_mode,
        )) = self.send_forms.get(&key).map(|form| {
            (
                form.asset.token,
                form.selected_fee_token,
                form.broadcaster_choice.clone(),
                form.fee_mode,
                form.allow_suspicious_broadcasters,
                form.favorites_only_broadcasters,
                form.delivery_mode,
            )
        })
        else {
            return;
        };
        if current_token == asset.token {
            return;
        }

        let policy = self.public_broadcaster_fee_policy(allow_suspicious);
        let asset_options =
            self.private_action_asset_options(DeliveryFormKind::Send, asset.chain_id);
        let fee_token_options = self.current_public_broadcaster_fee_token_options(
            asset.chain_id,
            false,
            false,
            favorites_only,
            policy,
        );
        let selected_fee_token = resolve_selected_public_broadcaster_fee_token(
            selected_fee_token,
            asset.token,
            &fee_token_options,
        );
        let candidates = self.current_public_broadcaster_candidates(
            asset.chain_id,
            selected_fee_token,
            false,
            false,
            favorites_only,
            policy,
        );
        let broadcaster_choice =
            if broadcaster_choice_supported_by_candidates(&choice, &candidates, policy) {
                choice
            } else {
                BroadcasterChoice::Random
            };
        let fee_mode = if selected_fee_token == asset.token {
            fee_mode
        } else {
            FeeHandlingMode::AddToAmount
        };
        // Switching assets clears the amount; the user must enter it deliberately.
        let amount = String::new();

        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        if form.generating {
            return;
        }
        form.asset = asset;
        form.selected_fee_token = selected_fee_token;
        form.broadcaster_choice = broadcaster_choice;
        form.custom_fee_amount = None;
        form.fee_mode = fee_mode;
        form.pending_programmatic_amount_input = Some(amount.clone());
        form.self_broadcast_estimated_native_gas_cost = None;
        form.error = None;
        form.result = None;
        form.cost_estimate = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        set_programmatic_amount_input_value(&form.amount_input, amount, window, cx);
        form.asset_select_items = private_action_asset_select_items(&asset_options);
        sync_private_action_asset_select_entity(
            &form.asset_select,
            &asset_options,
            form.asset.token,
            window,
            cx,
        );

        self.clear_private_broadcaster_progress_state();
        cx.notify();
        self.refresh_public_broadcaster_anchor(DeliveryFormKind::Send, key, cx);
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
        if delivery_mode == DeliveryMode::SelfBroadcast {
            self.refresh_self_broadcast_gas_fee_quote(DeliveryFormKind::Send, key, cx);
        }
        self.debounce_sponsored_funding_estimate(DeliveryFormKind::Send, key, cx);
    }

    pub(in crate::root) fn set_send_delivery_mode(
        &mut self,
        key: UnshieldAssetKey,
        mode: DeliveryMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let self_broadcast_gas_payer_uuid = if mode == DeliveryMode::SelfBroadcast {
            let default = self.default_self_broadcast_gas_payer_uuid();
            if default.is_none() && self.active_self_broadcast_gas_payer_accounts().is_empty() {
                return;
            }
            default
        } else {
            None
        };
        let Some(form) = self.send_forms.get(&key) else {
            return;
        };
        if form.generating || form.delivery_mode == mode || form.gateway_execution.is_some() {
            return;
        }
        if self.confirm_leaving_input_pin(
            DeliveryFormKind::Send,
            key,
            window,
            cx,
            move |root, window, cx| root.set_send_delivery_mode(key, mode, window, cx),
        ) {
            return;
        }
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        let old_max = send_form_max_entered_amount(form, form.delivery_mode, form.fee_mode);
        let new_max = send_form_max_entered_amount(form, mode, form.fee_mode);
        let adjusted =
            amount_adjustment_for_max_change(&form.amount_input, &form.asset, old_max, new_max, cx);
        form.delivery_mode = mode;
        form.custom_fee_amount = None;
        if mode == DeliveryMode::SelfBroadcast {
            form.self_broadcast_gas_payer_uuid
                .clone_from(&self_broadcast_gas_payer_uuid);
        }
        form.self_broadcast_estimated_native_gas_cost = None;
        form.error = None;
        form.result = None;
        if mode == DeliveryMode::PublicBroadcaster || adjusted.is_some() {
            form.cost_estimate = None;
        }
        if let Some(adjusted) = adjusted {
            form.pending_programmatic_amount_input = Some(adjusted.clone());
            set_programmatic_amount_input_value(&form.amount_input, adjusted, window, cx);
        }
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        cx.notify();
        if mode == DeliveryMode::PublicBroadcaster {
            self.refresh_public_broadcaster_anchor(DeliveryFormKind::Send, key, cx);
        } else if mode == DeliveryMode::SelfBroadcast {
            self.schedule_account_public_balance_refresh(
                key.chain_id,
                self_broadcast_gas_payer_uuid.as_deref(),
                Some(window.window_handle()),
                cx,
            );
            self.refresh_self_broadcast_gas_fee_quote(DeliveryFormKind::Send, key, cx);
        }
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
        self.debounce_sponsored_funding_estimate(DeliveryFormKind::Send, key, cx);
    }

    pub(in crate::root) fn set_send_broadcaster_choice(
        &mut self,
        key: UnshieldAssetKey,
        choice: BroadcasterChoice,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        if form.generating || form.broadcaster_choice == choice {
            return;
        }
        form.broadcaster_choice = choice;
        // A new choice, including going back to Random, starts a fresh pick.
        form.shown_broadcaster = None;
        form.custom_fee_amount = None;
        form.error = None;
        form.result = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        cx.notify();
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
    }

    pub(in crate::root) fn set_send_fee_token(
        &mut self,
        key: UnshieldAssetKey,
        fee_token: Address,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((
            chain_id,
            action_token,
            current_choice,
            generating,
            allow_suspicious,
            favorites_only,
        )) = self.send_forms.get(&key).map(|form| {
            (
                form.asset.chain_id,
                form.asset.token,
                form.broadcaster_choice.clone(),
                form.generating,
                form.allow_suspicious_broadcasters,
                form.favorites_only_broadcasters,
            )
        })
        else {
            return;
        };
        if generating {
            return;
        }
        let policy = self.public_broadcaster_fee_policy(allow_suspicious);
        let candidates = self.current_public_broadcaster_candidates(
            chain_id,
            fee_token,
            false,
            false,
            favorites_only,
            policy,
        );
        let reset_specific =
            !broadcaster_choice_supported_by_candidates(&current_choice, &candidates, policy);
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        if form.selected_fee_token == fee_token && !reset_specific {
            return;
        }
        form.selected_fee_token = fee_token;
        form.custom_fee_amount = None;
        if fee_token != action_token {
            form.fee_mode = FeeHandlingMode::AddToAmount;
        }
        if reset_specific {
            form.broadcaster_choice = BroadcasterChoice::Random;
        }
        form.error = None;
        form.result = None;
        form.cost_estimate = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        cx.notify();
        self.refresh_public_broadcaster_anchor(DeliveryFormKind::Send, key, cx);
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
    }

    pub(in crate::root) fn set_send_allow_suspicious_broadcasters(
        &mut self,
        key: UnshieldAssetKey,
        allow: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((
            chain_id,
            fee_token,
            choice,
            generating,
            current_allow,
            favorites_only,
            resolved_random_broadcaster,
            random_estimate_in_flight,
        )) = self.send_forms.get(&key).map(|form| {
            (
                form.asset.chain_id,
                form.selected_fee_token,
                form.broadcaster_choice.clone(),
                form.generating,
                form.allow_suspicious_broadcasters,
                form.favorites_only_broadcasters,
                form.cost_estimate
                    .as_ref()
                    .map(|estimate| estimate.broadcaster.railgun_address.clone()),
                form.cost_estimate_pending || form.estimating_cost,
            )
        })
        else {
            return;
        };
        if generating || current_allow == allow {
            return;
        }
        let policy = self.public_broadcaster_fee_policy(allow);
        let candidates = self.current_public_broadcaster_candidates(
            chain_id,
            fee_token,
            false,
            false,
            favorites_only,
            policy,
        );
        let preserve_estimate = should_preserve_estimate_after_broadcaster_policy_change(
            &choice,
            resolved_random_broadcaster.as_deref(),
            random_estimate_in_flight,
            &candidates,
            policy,
        );
        let reset_specific =
            matches!(choice, BroadcasterChoice::Specific { .. }) && !preserve_estimate;
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        form.allow_suspicious_broadcasters = allow;
        if reset_specific {
            form.broadcaster_choice = BroadcasterChoice::Random;
            form.custom_fee_amount = None;
        }
        let should_reestimate = !preserve_estimate;
        if should_reestimate {
            form.error = None;
            form.result = None;
            form.estimate_id = 0;
            form.cost_estimate_pending = false;
            form.estimating_cost = false;
        }
        cx.notify();
        if should_reestimate {
            self.invalidate_broadcaster_picker_fee_estimate(DeliveryFormKind::Send, key, cx);
            self.refresh_public_broadcaster_anchor(DeliveryFormKind::Send, key, cx);
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
            self.schedule_broadcaster_picker_fee_estimate(DeliveryFormKind::Send, key, cx);
        }
    }

    pub(super) fn set_send_favorites_only_broadcasters(
        &mut self,
        key: UnshieldAssetKey,
        enabled: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((
            chain_id,
            fee_token,
            choice,
            generating,
            allow_suspicious,
            current_enabled,
            resolved_random_broadcaster,
            random_estimate_in_flight,
        )) = self.send_forms.get(&key).map(|form| {
            (
                form.asset.chain_id,
                form.selected_fee_token,
                form.broadcaster_choice.clone(),
                form.generating,
                form.allow_suspicious_broadcasters,
                form.favorites_only_broadcasters,
                form.cost_estimate
                    .as_ref()
                    .map(|estimate| estimate.broadcaster.railgun_address.clone()),
                form.cost_estimate_pending || form.estimating_cost,
            )
        })
        else {
            return;
        };
        if generating || current_enabled == enabled {
            return;
        }
        let policy = self.public_broadcaster_fee_policy(allow_suspicious);
        let candidates = self.current_public_broadcaster_candidates(
            chain_id, fee_token, false, false, enabled, policy,
        );
        let preserve_estimate = should_preserve_estimate_after_broadcaster_policy_change(
            &choice,
            resolved_random_broadcaster.as_deref(),
            random_estimate_in_flight,
            &candidates,
            policy,
        );
        let reset_specific =
            matches!(choice, BroadcasterChoice::Specific { .. }) && !preserve_estimate;
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        form.favorites_only_broadcasters = enabled;
        if reset_specific {
            form.broadcaster_choice = BroadcasterChoice::Random;
            form.custom_fee_amount = None;
        }
        let should_reestimate = !preserve_estimate;
        if should_reestimate {
            form.error = None;
            form.result = None;
            form.estimate_id = 0;
            form.cost_estimate_pending = false;
            form.estimating_cost = false;
        }
        cx.notify();
        if should_reestimate {
            self.invalidate_broadcaster_picker_fee_estimate(DeliveryFormKind::Send, key, cx);
            self.refresh_public_broadcaster_anchor(DeliveryFormKind::Send, key, cx);
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
            self.schedule_broadcaster_picker_fee_estimate(DeliveryFormKind::Send, key, cx);
        }
    }

    pub(in crate::root) fn set_send_fee_mode(
        &mut self,
        key: UnshieldAssetKey,
        fee_mode: FeeHandlingMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        if form.generating
            || form.selected_fee_token != form.asset.token
            || form.fee_mode == fee_mode
        {
            return;
        }
        let old_max = send_form_max_entered_amount(form, form.delivery_mode, form.fee_mode);
        let new_max = send_form_max_entered_amount(form, form.delivery_mode, fee_mode);
        let adjusted =
            amount_adjustment_for_max_change(&form.amount_input, &form.asset, old_max, new_max, cx);
        form.fee_mode = fee_mode;
        form.error = None;
        form.result = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        if let Some(adjusted) = adjusted {
            form.pending_programmatic_amount_input = Some(adjusted.clone());
            set_programmatic_amount_input_value(&form.amount_input, adjusted, window, cx);
        }
        cx.notify();
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Send, key, cx);
    }

    pub(in crate::root) fn set_send_form_error(
        &mut self,
        key: UnshieldAssetKey,
        message: impl Into<Arc<str>>,
        cx: &mut Context<'_, Self>,
    ) {
        let message = message.into();
        if let Some(form) = self.send_forms.get_mut(&key) {
            form.generating = false;
            if form_error_clears_public_broadcaster_cost_estimate(
                DeliveryFormKind::Send,
                message.as_ref(),
            ) {
                form.cost_estimate = None;
            }
            form.cost_estimate_pending = false;
            form.estimating_cost = false;
            form.estimate_id = 0;
            form.result = None;
            form.error = Some(message);
            cx.notify();
        }
    }
}
