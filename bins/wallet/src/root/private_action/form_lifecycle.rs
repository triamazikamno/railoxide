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

    pub(in crate::root) fn close_unshield_form(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(form) = self.unshield_forms.remove(&key)
            && let Some(execution) = form.gateway_execution
        {
            execution.cancel_review();
        }
        if self
            .private_action_form
            .as_ref()
            .is_some_and(|form| form.kind == DeliveryFormKind::Unshield && form.key == key)
        {
            self.private_action_form = None;
            self.broadcaster_picker = None;
        }
        if self
            .private_broadcaster_progress
            .as_ref()
            .is_some_and(|progress| {
                progress.kind == DeliveryFormKind::Unshield && progress.key == key
            })
        {
            self.clear_private_broadcaster_progress_state();
        }
        cx.notify();
    }

    pub(in crate::root) fn open_private_action_dialog(
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        title_action: &'static str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let root = cx.entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            let dialog_width = (window.viewport_size().width * 0.92).min(PRIVATE_ASSET_LIST_WIDTH);
            let content_width = secondary_dialog_content_width(dialog_width);
            let max_height = window.viewport_size().height * 0.88;
            let close_root = root.clone();
            let content_root = root.clone();
            root.update(cx, |root, cx| {
                root.sync_private_action_asset_select_for_dialog(kind, key, window, cx);
            });
            let content = content_root.read(cx);
            let (asset_label, icon_path) =
                content.private_action_dialog_asset(kind, key).map_or_else(
                    || ("asset".to_string(), None),
                    |asset| (asset.label.clone(), asset.icon_path.clone()),
                );
            let (asset_select, asset_select_disabled) =
                content.private_action_dialog_asset_select(kind, key);
            let child = match kind {
                DeliveryFormKind::Send => {
                    content.render_send_form(content_root.clone(), key, content_width)
                }
                DeliveryFormKind::Unshield => {
                    content.render_unshield_form(content_root.clone(), key, content_width)
                }
            };
            dialog
                .w(dialog_width)
                .on_ok(|_, _, _| false)
                .max_h(max_height)
                .title(private_action_title_row(
                    title_action,
                    &asset_label,
                    icon_path,
                    asset_select,
                    asset_select_disabled,
                ))
                .on_close(move |_event, _window, cx| {
                    close_root.update(cx, |root, cx| match kind {
                        DeliveryFormKind::Send => root.close_send_form(key, cx),
                        DeliveryFormKind::Unshield => root.close_unshield_form(key, cx),
                    });
                })
                .child(
                    div()
                        .when_some(content.private_action_form.as_ref(), |this, form| {
                            this.track_focus(&form.focus)
                        })
                        .child(child),
                )
        });
    }

    pub(in crate::root) fn private_action_dialog_asset(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> Option<&UnshieldAsset> {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).map(|form| &form.asset),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).map(|form| &form.asset),
        }
    }

    pub(in crate::root) fn private_action_dialog_asset_select(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> (
        Option<Entity<SelectState<SearchableVec<PrivateActionAssetSelectItem>>>>,
        bool,
    ) {
        match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get(&key) else {
                    return (None, false);
                };
                let show = self
                    .private_action_asset_options(kind, form.asset.chain_id)
                    .len()
                    > 1;
                let disabled = form.generating || send_form_submitted(form);
                (show.then(|| form.asset_select.clone()), disabled)
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get(&key) else {
                    return (None, false);
                };
                let show = self
                    .private_action_asset_options(kind, form.asset.chain_id)
                    .len()
                    > 1;
                let disabled = form.generating || unshield_form_submitted(form);
                (show.then(|| form.asset_select.clone()), disabled)
            }
        }
    }

    pub(in crate::root) fn open_send_form(
        &mut self,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
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

    pub(in crate::root) fn consume_programmatic_amount_input_change(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &Context<'_, Self>,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                let Some(expected) = form.pending_programmatic_amount_input.take() else {
                    return false;
                };
                form.amount_input.read(cx).value().as_ref() == expected
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                let Some(expected) = form.pending_programmatic_amount_input.take() else {
                    return false;
                };
                form.amount_input.read(cx).value().as_ref() == expected
            }),
        }
    }

    pub(in crate::root) fn set_private_action_metric_amount(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        amount: U256,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = self.set_programmatic_amount_input(kind, key, amount, window, cx);
        if changed {
            if kind == DeliveryFormKind::Unshield {
                self.refresh_unshield_native_top_up_state(key, cx);
            }
            self.schedule_public_broadcaster_cost_estimate(kind, key, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn set_programmatic_amount_input(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        amount: U256,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating {
                    return false;
                }
                let value = format_send_amount_input(amount, form.asset.decimals);
                form.pending_programmatic_amount_input = Some(value.clone());
                form.error = None;
                form.result = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                set_programmatic_amount_input_value(&form.amount_input, value, window, cx);
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating {
                    return false;
                }
                let value = format_unshield_amount_input(amount, form.asset.decimals);
                form.pending_programmatic_amount_input = Some(value.clone());
                form.error = None;
                form.result = None;
                form.estimate_id = 0;
                form.cost_estimate_pending = false;
                form.estimating_cost = false;
                set_programmatic_amount_input_value(&form.amount_input, value, window, cx);
                true
            }),
        }
    }

    pub(in crate::root) fn set_private_action_asset(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => self.set_send_asset(key, asset, window, cx),
            DeliveryFormKind::Unshield => self.set_unshield_asset(key, asset, window, cx),
        }
    }

    pub(in crate::root) fn set_private_action_asset_token(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        token: Address,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(asset) = self
            .private_action_asset_options(kind, key.chain_id)
            .into_iter()
            .find(|asset| asset.token == token)
        else {
            return;
        };
        self.set_private_action_asset(kind, key, asset, window, cx);
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

    pub(in crate::root) fn set_unshield_asset(
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
            unwrap,
        )) = self.unshield_forms.get(&key).map(|form| {
            (
                form.asset.token,
                form.selected_fee_token,
                form.broadcaster_choice.clone(),
                form.fee_mode,
                form.allow_suspicious_broadcasters,
                form.favorites_only_broadcasters,
                form.delivery_mode,
                form.unwrap,
            )
        })
        else {
            return;
        };
        if current_token == asset.token {
            return;
        }

        let unwrap_supported = is_effective_wrapped_native_token(
            &self.effective_chain_configs,
            asset.chain_id,
            asset.token,
        );
        let unwrap = unwrap && unwrap_supported;
        let policy = self.public_broadcaster_fee_policy(allow_suspicious);
        let asset_options =
            self.private_action_asset_options(DeliveryFormKind::Unshield, asset.chain_id);
        let fee_token_options = self.current_public_broadcaster_fee_token_options(
            asset.chain_id,
            unwrap,
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
            unwrap,
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
        // Switching assets clears the amount; the user must enter it deliberately.
        let amount = String::new();

        let Some(form) = self.unshield_forms.get_mut(&key) else {
            return;
        };
        if form.generating {
            return;
        }
        form.asset = asset;
        form.unwrap = unwrap;
        form.native_top_up = None;
        form.native_top_up_enabled = false;
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
        self.refresh_unshield_native_top_up_state(key, cx);
        self.refresh_public_broadcaster_anchor(DeliveryFormKind::Unshield, key, cx);
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
        if delivery_mode == DeliveryMode::SelfBroadcast {
            self.refresh_self_broadcast_gas_fee_quote(DeliveryFormKind::Unshield, key, cx);
        }
        self.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
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
        let Some(form) = self.send_forms.get_mut(&key) else {
            return;
        };
        if form.generating || form.delivery_mode == mode || form.gateway_execution.is_some() {
            return;
        }
        let old_max = send_form_max_entered_amount(form, form.delivery_mode, form.fee_mode);
        let new_max = send_form_max_entered_amount(form, mode, form.fee_mode);
        let adjusted =
            amount_adjustment_for_max_change(&form.amount_input, &form.asset, old_max, new_max, cx);
        form.delivery_mode = mode;
        form.custom_fee_amount = None;
        if mode == DeliveryMode::SelfBroadcast {
            form.self_broadcast_gas_payer_uuid = self_broadcast_gas_payer_uuid;
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
            self.schedule_self_broadcast_public_balance_refresh(window, cx);
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

    pub(in crate::root) fn set_favorites_only_broadcasters(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        enabled: bool,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => self.set_send_favorites_only_broadcasters(key, enabled, cx),
            DeliveryFormKind::Unshield => {
                self.set_unshield_favorites_only_broadcasters(key, enabled, cx);
            }
        }
    }

    fn set_send_favorites_only_broadcasters(
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

    pub(in crate::root) fn set_allow_suspicious_broadcasters(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        allow: bool,
        cx: &mut Context<'_, Self>,
    ) {
        match kind {
            DeliveryFormKind::Send => self.set_send_allow_suspicious_broadcasters(key, allow, cx),
            DeliveryFormKind::Unshield => {
                self.set_unshield_allow_suspicious_broadcasters(key, allow, cx);
            }
        }
    }

    pub(in crate::root) fn set_transaction_fee_breakdown_open(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.transaction_fee_breakdown_open == open {
                    false
                } else {
                    form.transaction_fee_breakdown_open = open;
                    true
                }
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.transaction_fee_breakdown_open == open {
                    false
                } else {
                    form.transaction_fee_breakdown_open = open;
                    true
                }
            }),
        };
        if changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn set_sponsored_funding_breakdown_open(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        open: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.sponsored_funding_breakdown_open == open {
                    false
                } else {
                    form.sponsored_funding_breakdown_open = open;
                    true
                }
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.sponsored_funding_breakdown_open == open {
                    false
                } else {
                    form.sponsored_funding_breakdown_open = open;
                    true
                }
            }),
        };
        if changed {
            cx.notify();
        }
    }

    pub(in crate::root) fn set_self_broadcast_gas_payer(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        public_account_uuid: Option<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.self_broadcast_gas_payer_uuid == public_account_uuid {
                    return false;
                }
                form.self_broadcast_gas_payer_uuid = public_account_uuid;
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.self_broadcast_gas_payer_uuid == public_account_uuid {
                    return false;
                }
                form.self_broadcast_gas_payer_uuid = public_account_uuid;
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
        };
        if changed {
            self.sync_self_broadcast_gas_payer_select(kind, key, window, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn choose_random_self_broadcast_gas_payer(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.active_self_broadcast_gas_payer_accounts();
        let (selected_uuid, funding) = match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).map_or(
                (None, SelfBroadcastFundingMode::PublicBalance),
                |form| {
                    (
                        form.self_broadcast_gas_payer_uuid.clone(),
                        form.self_broadcast_funding,
                    )
                },
            ),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).map_or(
                (None, SelfBroadcastFundingMode::PublicBalance),
                |form| {
                    (
                        form.self_broadcast_gas_payer_uuid.clone(),
                        form.self_broadcast_funding,
                    )
                },
            ),
        };
        let Some(account_uuid) = random_self_broadcast_gas_payer_uuid_for_funding(
            &accounts,
            selected_uuid.as_deref(),
            key.chain_id,
            self.public_balance_snapshot.as_deref(),
            funding,
        ) else {
            return;
        };
        self.set_self_broadcast_gas_payer(kind, key, Some(account_uuid), window, cx);
    }

    pub(in crate::root) fn set_self_broadcast_gas_fee_mode(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        mode: Eip1559GasFeeMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.self_broadcast_gas_fee.mode == mode {
                    return false;
                }
                if mode == Eip1559GasFeeMode::Custom {
                    form.self_broadcast_gas_fee
                        .seed_custom_from_auto_if_empty(window, cx);
                }
                form.self_broadcast_gas_fee.mode = mode;
                if mode == Eip1559GasFeeMode::Auto {
                    form.self_broadcast_gas_fee.error = None;
                }
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating || form.self_broadcast_gas_fee.mode == mode {
                    return false;
                }
                if mode == Eip1559GasFeeMode::Custom {
                    form.self_broadcast_gas_fee
                        .seed_custom_from_auto_if_empty(window, cx);
                }
                form.self_broadcast_gas_fee.mode = mode;
                if mode == Eip1559GasFeeMode::Auto {
                    form.self_broadcast_gas_fee.error = None;
                }
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
        };
        if changed {
            self.sync_self_broadcast_custom_gas_fee_error(kind, key, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    fn sync_self_broadcast_custom_gas_fee_error(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &App,
    ) {
        let error = match kind {
            DeliveryFormKind::Send => self.send_forms.get(&key).and_then(|form| {
                (form.self_broadcast_gas_fee.mode == Eip1559GasFeeMode::Custom)
                    .then(|| form.self_broadcast_gas_fee.selection(cx).err())
                    .flatten()
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get(&key).and_then(|form| {
                (form.self_broadcast_gas_fee.mode == Eip1559GasFeeMode::Custom)
                    .then(|| form.self_broadcast_gas_fee.selection(cx).err())
                    .flatten()
            }),
        }
        .map(Arc::<str>::from);
        match kind {
            DeliveryFormKind::Send => {
                if let Some(form) = self.send_forms.get_mut(&key)
                    && form.self_broadcast_gas_fee.mode == Eip1559GasFeeMode::Custom
                {
                    form.self_broadcast_gas_fee.error = error;
                }
            }
            DeliveryFormKind::Unshield => {
                if let Some(form) = self.unshield_forms.get_mut(&key)
                    && form.self_broadcast_gas_fee.mode == Eip1559GasFeeMode::Custom
                {
                    form.self_broadcast_gas_fee.error = error;
                }
            }
        }
    }

    pub(in crate::root) fn customize_self_broadcast_gas_fee_from_auto(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        target: Eip1559GasFeeEditTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mut focus_input: Option<Entity<InputState>> = None;
        let changed = match kind {
            DeliveryFormKind::Send => self.send_forms.get_mut(&key).is_some_and(|form| {
                if form.generating
                    || !form
                        .self_broadcast_gas_fee
                        .overwrite_custom_from_auto(window, cx)
                {
                    return false;
                }
                focus_input = Some(match target {
                    Eip1559GasFeeEditTarget::MaxFee => {
                        form.self_broadcast_gas_fee.max_fee_input.clone()
                    }
                    Eip1559GasFeeEditTarget::MaxTip => {
                        form.self_broadcast_gas_fee.max_priority_fee_input.clone()
                    }
                });
                form.self_broadcast_gas_fee.mode = Eip1559GasFeeMode::Custom;
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
            DeliveryFormKind::Unshield => self.unshield_forms.get_mut(&key).is_some_and(|form| {
                if form.generating
                    || !form
                        .self_broadcast_gas_fee
                        .overwrite_custom_from_auto(window, cx)
                {
                    return false;
                }
                focus_input = Some(match target {
                    Eip1559GasFeeEditTarget::MaxFee => {
                        form.self_broadcast_gas_fee.max_fee_input.clone()
                    }
                    Eip1559GasFeeEditTarget::MaxTip => {
                        form.self_broadcast_gas_fee.max_priority_fee_input.clone()
                    }
                });
                form.self_broadcast_gas_fee.mode = Eip1559GasFeeMode::Custom;
                form.self_broadcast_estimated_native_gas_cost = None;
                form.error = None;
                form.result = None;
                true
            }),
        };
        if changed {
            if let Some(input) = focus_input {
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
            self.sync_self_broadcast_custom_gas_fee_error(kind, key, cx);
            self.debounce_sponsored_funding_estimate(kind, key, cx);
        }
    }

    pub(in crate::root) fn refresh_self_broadcast_gas_fee_quote(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let chain_id = key.chain_id;
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let refresh_id = match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get_mut(&key) else {
                    return;
                };
                if form.generating || form.self_broadcast_gas_fee.refreshing {
                    return;
                }
                form.self_broadcast_gas_fee.refresh_id =
                    form.self_broadcast_gas_fee.refresh_id.wrapping_add(1);
                form.self_broadcast_gas_fee.refreshing = true;
                form.self_broadcast_gas_fee.quote_error = None;
                form.self_broadcast_gas_fee.refresh_id
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get_mut(&key) else {
                    return;
                };
                if form.generating || form.self_broadcast_gas_fee.refreshing {
                    return;
                }
                form.self_broadcast_gas_fee.refresh_id =
                    form.self_broadcast_gas_fee.refresh_id.wrapping_add(1);
                form.self_broadcast_gas_fee.refreshing = true;
                form.self_broadcast_gas_fee.quote_error = None;
                form.self_broadcast_gas_fee.refresh_id
            }
        };
        let http = self.http.clone();
        cx.spawn(async move |this, cx| {
            let result =
                quote_desktop_self_broadcast_gas_fee(chain_id, effective_chain.as_ref(), &http)
                    .await;
            let _ = this.update(cx, |root, cx| {
                let gas_fee = match kind {
                    DeliveryFormKind::Send => root
                        .send_forms
                        .get_mut(&key)
                        .map(|form| &mut form.self_broadcast_gas_fee),
                    DeliveryFormKind::Unshield => root
                        .unshield_forms
                        .get_mut(&key)
                        .map(|form| &mut form.self_broadcast_gas_fee),
                };
                let Some(gas_fee) = gas_fee else {
                    return;
                };
                if gas_fee.refresh_id != refresh_id {
                    return;
                }
                gas_fee.refreshing = false;
                let quote_updated = match result {
                    Ok(quote) => {
                        gas_fee.quote = Some(quote);
                        gas_fee.quote_error = None;
                        true
                    }
                    Err(_error) => {
                        gas_fee.quote_error = Some(Arc::from(if gas_fee.quote.is_some() {
                            "Gas quote refresh failed; using the last successful quote."
                        } else {
                            "Gas quote is unavailable."
                        }));
                        false
                    }
                };
                root.sync_self_broadcast_custom_gas_fee_error(kind, key, cx);
                if quote_updated {
                    root.revalidate_sponsored_funding_estimate(kind, key, cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
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

    pub(in crate::root) fn open_unshield_form(
        &mut self,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        let key = UnshieldAssetKey::from_asset(&asset);
        self.initialize_unshield_form(asset, window, cx);
        let focus_recipient_input = self.unshield_forms[&key].recipient_input.clone();
        self.private_action_form = Some(PrivateActionFormState {
            focus: cx.focus_handle(),
            kind: DeliveryFormKind::Unshield,
            key,
        });
        self.refresh_public_broadcaster_anchor(DeliveryFormKind::Unshield, key, cx);
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
        self.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
        Self::open_private_action_dialog(DeliveryFormKind::Unshield, key, "Unshield", window, cx);
        cx.defer_in(window, move |root, window, cx| {
            if root
                .private_action_form
                .as_ref()
                .is_some_and(|form| form.kind == DeliveryFormKind::Unshield && form.key == key)
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

    pub(in crate::root) fn initialize_unshield_form(
        &mut self,
        asset: UnshieldAsset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let key = UnshieldAssetKey::from_asset(&asset);
        let amount_input = new_text_input(window, cx, "amount");
        let recipient_input = new_text_input(window, cx, "0x recipient");
        let (asset_select, asset_select_items) = self.new_private_action_asset_select(
            DeliveryFormKind::Unshield,
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
                    if let Some(form) = this.unshield_forms.get_mut(&key) {
                        if form.recipient_value.as_ref() == input.read(cx).value().as_ref() {
                            return;
                        }
                        form.recipient_value = Arc::from(input.read(cx).value().as_ref());
                    }
                    this.clear_unshield_form_text_edit_state(key, cx);
                    this.refresh_unshield_native_top_up_state(key, cx);
                    this.update_recipient_suggestions_for_input_change(
                        DeliveryFormKind::Unshield,
                        key,
                        cx,
                    );
                    this.debounce_public_broadcaster_cost_estimate(
                        DeliveryFormKind::Unshield,
                        key,
                        cx,
                    );
                    this.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
                } else if matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_selected_recipient_suggestion(
                        DeliveryFormKind::Unshield,
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
                        DeliveryFormKind::Unshield,
                        key,
                        cx,
                    ) {
                        this.refresh_unshield_native_top_up_state(key, cx);
                        return;
                    }
                    this.clear_unshield_form_text_edit_state(key, cx);
                    this.refresh_unshield_native_top_up_state(key, cx);
                    this.debounce_public_broadcaster_cost_estimate(
                        DeliveryFormKind::Unshield,
                        key,
                        cx,
                    );
                    this.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
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
                        DeliveryFormKind::Unshield,
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
                        DeliveryFormKind::Unshield,
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
                    this.sync_self_broadcast_custom_gas_fee_error(
                        DeliveryFormKind::Unshield,
                        key,
                        cx,
                    );
                    this.clear_unshield_form_text_edit_state(key, cx);
                    this.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
                }
            },
        )
        .detach();
        cx.subscribe(
            &gas_fee_editor.max_priority_fee_input,
            move |this, _input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.sync_self_broadcast_custom_gas_fee_error(
                        DeliveryFormKind::Unshield,
                        key,
                        cx,
                    );
                    this.clear_unshield_form_text_edit_state(key, cx);
                    this.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
                }
            },
        )
        .detach();
        cx.subscribe(
            &sponsored_custom_incentive_input,
            move |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.set_sponsored_custom_incentive_from_text(
                        DeliveryFormKind::Unshield,
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
        self.unshield_forms.insert(
            key,
            UnshieldFormState {
                custom_fee_amount: None,
                executor_operation: None,
                executor_review: None,
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
                unwrap: false,
                native_top_up: None,
                native_top_up_enabled: false,
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
                estimate_id: 0,
                generation_id: 0,
                generating: false,
                generation_stage: TransactionGenerationStage::default(),
                error: None,
                result: None,
            },
        );
    }

    pub(in crate::root) fn set_unshield_unwrap(
        &mut self,
        key: UnshieldAssetKey,
        unwrap: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let unwrap_supported = self.unshield_forms.get(&key).is_some_and(|form| {
            is_effective_wrapped_native_token(
                &self.effective_chain_configs,
                form.asset.chain_id,
                form.asset.token,
            )
        });
        let Some(form) = self.unshield_forms.get_mut(&key) else {
            return;
        };
        if !unwrap_supported || form.generating || form.unwrap == unwrap {
            return;
        }
        form.unwrap = unwrap;
        form.error = None;
        form.result = None;
        form.cost_estimate = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        let delivery_mode = form.delivery_mode;
        cx.notify();
        self.refresh_unshield_native_top_up_state(key, cx);
        if delivery_mode == DeliveryMode::PublicBroadcaster {
            self.refresh_public_broadcaster_anchor(DeliveryFormKind::Unshield, key, cx);
        }
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
        self.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
    }

    pub(in crate::root) fn set_unshield_fee_mode(
        &mut self,
        key: UnshieldAssetKey,
        fee_mode: FeeHandlingMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.unshield_forms.get_mut(&key) else {
            return;
        };
        if form.generating || form.fee_mode == fee_mode {
            return;
        }
        let old_max = unshield_form_max_entered_amount(form, form.delivery_mode, form.fee_mode);
        let new_max = unshield_form_max_entered_amount(form, form.delivery_mode, fee_mode);
        let adjusted =
            amount_adjustment_for_max_change(&form.amount_input, &form.asset, old_max, new_max, cx);
        form.fee_mode = fee_mode;
        form.error = None;
        form.result = None;
        form.cost_estimate = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        if let Some(adjusted) = adjusted {
            form.pending_programmatic_amount_input = Some(adjusted.clone());
            set_programmatic_amount_input_value(&form.amount_input, adjusted, window, cx);
        }
        let delivery_mode = form.delivery_mode;
        cx.notify();
        self.refresh_unshield_native_top_up_state(key, cx);
        if delivery_mode == DeliveryMode::PublicBroadcaster {
            self.refresh_public_broadcaster_anchor(DeliveryFormKind::Unshield, key, cx);
        }
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
        self.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
    }

    pub(in crate::root) fn clear_unshield_form_text_edit_state(
        &mut self,
        key: UnshieldAssetKey,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.unshield_forms.get_mut(&key) else {
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

    pub(in crate::root) fn set_unshield_delivery_mode(
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
        let Some(form) = self.unshield_forms.get_mut(&key) else {
            return;
        };
        if form.generating || form.delivery_mode == mode || form.gateway_execution.is_some() {
            return;
        }
        let old_max = unshield_form_max_entered_amount(form, form.delivery_mode, form.fee_mode);
        let new_max = unshield_form_max_entered_amount(form, mode, form.fee_mode);
        let adjusted =
            amount_adjustment_for_max_change(&form.amount_input, &form.asset, old_max, new_max, cx);
        form.delivery_mode = mode;
        form.custom_fee_amount = None;
        if mode == DeliveryMode::SelfBroadcast {
            form.self_broadcast_gas_payer_uuid = self_broadcast_gas_payer_uuid;
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
            self.refresh_public_broadcaster_anchor(DeliveryFormKind::Unshield, key, cx);
        } else if mode == DeliveryMode::SelfBroadcast {
            self.schedule_self_broadcast_public_balance_refresh(window, cx);
            self.refresh_self_broadcast_gas_fee_quote(DeliveryFormKind::Unshield, key, cx);
        }
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
        self.debounce_sponsored_funding_estimate(DeliveryFormKind::Unshield, key, cx);
    }

    pub(in crate::root) fn set_unshield_broadcaster_choice(
        &mut self,
        key: UnshieldAssetKey,
        choice: BroadcasterChoice,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(form) = self.unshield_forms.get_mut(&key) else {
            return;
        };
        if form.generating || form.broadcaster_choice == choice {
            return;
        }
        form.broadcaster_choice = choice;
        form.custom_fee_amount = None;
        form.error = None;
        form.result = None;
        form.estimate_id = 0;
        form.cost_estimate_pending = false;
        form.estimating_cost = false;
        cx.notify();
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
    }

    pub(in crate::root) fn set_unshield_fee_token(
        &mut self,
        key: UnshieldAssetKey,
        fee_token: Address,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((
            chain_id,
            unwrap,
            native_top_up,
            current_choice,
            generating,
            allow_suspicious,
            favorites_only,
        )) = self.unshield_forms.get(&key).map(|form| {
            (
                form.asset.chain_id,
                form.unwrap,
                form.native_top_up_enabled && form.native_top_up.is_some(),
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
            unwrap,
            native_top_up,
            favorites_only,
            policy,
        );
        let reset_specific =
            !broadcaster_choice_supported_by_candidates(&current_choice, &candidates, policy);
        let Some(form) = self.unshield_forms.get_mut(&key) else {
            return;
        };
        if form.selected_fee_token == fee_token && !reset_specific {
            return;
        }
        form.selected_fee_token = fee_token;
        form.custom_fee_amount = None;
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
        self.refresh_public_broadcaster_anchor(DeliveryFormKind::Unshield, key, cx);
        self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
    }

    pub(in crate::root) fn set_unshield_allow_suspicious_broadcasters(
        &mut self,
        key: UnshieldAssetKey,
        allow: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((
            chain_id,
            fee_token,
            unwrap,
            native_top_up,
            choice,
            generating,
            current_allow,
            favorites_only,
            resolved_random_broadcaster,
            random_estimate_in_flight,
        )) = self.unshield_forms.get(&key).map(|form| {
            (
                form.asset.chain_id,
                form.selected_fee_token,
                form.unwrap,
                form.native_top_up_enabled && form.native_top_up.is_some(),
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
            unwrap,
            native_top_up,
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
        let Some(form) = self.unshield_forms.get_mut(&key) else {
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
            self.invalidate_broadcaster_picker_fee_estimate(DeliveryFormKind::Unshield, key, cx);
            self.refresh_public_broadcaster_anchor(DeliveryFormKind::Unshield, key, cx);
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
            self.schedule_broadcaster_picker_fee_estimate(DeliveryFormKind::Unshield, key, cx);
        }
    }

    fn set_unshield_favorites_only_broadcasters(
        &mut self,
        key: UnshieldAssetKey,
        enabled: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((
            chain_id,
            fee_token,
            unwrap,
            native_top_up,
            choice,
            generating,
            allow_suspicious,
            current_enabled,
            resolved_random_broadcaster,
            random_estimate_in_flight,
        )) = self.unshield_forms.get(&key).map(|form| {
            (
                form.asset.chain_id,
                form.selected_fee_token,
                form.unwrap,
                form.native_top_up_enabled && form.native_top_up.is_some(),
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
            chain_id,
            fee_token,
            unwrap,
            native_top_up,
            enabled,
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
        let Some(form) = self.unshield_forms.get_mut(&key) else {
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
            self.invalidate_broadcaster_picker_fee_estimate(DeliveryFormKind::Unshield, key, cx);
            self.refresh_public_broadcaster_anchor(DeliveryFormKind::Unshield, key, cx);
            self.schedule_public_broadcaster_cost_estimate(DeliveryFormKind::Unshield, key, cx);
            self.schedule_broadcaster_picker_fee_estimate(DeliveryFormKind::Unshield, key, cx);
        }
    }

    pub(in crate::root) fn set_unshield_form_error(
        &mut self,
        key: UnshieldAssetKey,
        message: impl Into<Arc<str>>,
        cx: &mut Context<'_, Self>,
    ) {
        let message = message.into();
        if let Some(form) = self.unshield_forms.get_mut(&key) {
            form.generating = false;
            if form_error_clears_public_broadcaster_cost_estimate(
                DeliveryFormKind::Unshield,
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
