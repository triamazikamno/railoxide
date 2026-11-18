use super::*;

impl WalletRoot {
    /// The broadcaster's refusal and the form's choice, while that form can still retry it.
    pub(super) fn refused_public_broadcaster_retry(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
    ) -> Option<(&PublicBroadcasterSubmissionResult, &BroadcasterChoice)> {
        let progress = self.private_broadcaster_progress.as_ref()?;
        if progress.kind != kind
            || progress.key != key
            || progress.generation_id != generation_id
            || progress.flow != PrivateSubmissionProgressFlow::PublicBroadcaster
            || progress.gateway_execution.is_some()
            // Closing the dialog abandons a retry, so a late estimate never opens authorization.
            || !progress.dialog_open
        {
            return None;
        }
        let result = progress
            .result
            .as_ref()
            .filter(|result| matches!(result.result, PublicBroadcasterResultKind::Failed { .. }))?;
        let (generating, gateway, form_generation_id, choice) = match kind {
            DeliveryFormKind::Send => {
                let form = self.send_forms.get(&key)?;
                (
                    form.generating,
                    form.gateway_execution.is_some(),
                    form.generation_id,
                    &form.broadcaster_choice,
                )
            }
            DeliveryFormKind::Unshield => {
                let form = self.unshield_forms.get(&key)?;
                (
                    form.generating,
                    form.gateway_execution.is_some(),
                    form.generation_id,
                    &form.broadcaster_choice,
                )
            }
        };
        (!generating && !gateway && form_generation_id == generation_id).then_some((result, choice))
    }

    pub(super) fn try_another_public_broadcaster(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((result, BroadcasterChoice::Random)) =
            self.refused_public_broadcaster_retry(kind, key, generation_id)
        else {
            return;
        };
        let address = result.broadcaster.railgun_address.clone();
        match kind {
            DeliveryFormKind::Send => {
                let form = self.send_forms.get_mut(&key).expect("validated form");
                if !form.refused_broadcasters.contains(&address) {
                    form.refused_broadcasters.push(address);
                }
                form.shown_broadcaster = None;
                form.cost_estimate = None;
                form.result = None;
                form.error = None;
            }
            DeliveryFormKind::Unshield => {
                let form = self.unshield_forms.get_mut(&key).expect("validated form");
                if !form.refused_broadcasters.contains(&address) {
                    form.refused_broadcasters.push(address);
                }
                form.shown_broadcaster = None;
                form.cost_estimate = None;
                form.result = None;
                form.error = None;
            }
        }
        self.start_refused_broadcaster_retry(kind, key, generation_id, None, window, cx);
    }

    /// Whether a submission the form's note pin stopped can still continue without the pin.
    pub(super) fn input_pin_conflict_retryable(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
    ) -> bool {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return false;
        };
        if !progress.input_pin_conflict
            || progress.kind != kind
            || progress.key != key
            || progress.generation_id != generation_id
            || progress.gateway_execution.is_some()
            || !progress.dialog_open
        {
            return false;
        }
        // Not the form's conflict flag: estimates rewrite it without seeing note overlap.
        let (generating, form_generation_id) = match kind {
            DeliveryFormKind::Send => {
                let Some(form) = self.send_forms.get(&key) else {
                    return false;
                };
                (form.generating, form.generation_id)
            }
            DeliveryFormKind::Unshield => {
                let Some(form) = self.unshield_forms.get(&key) else {
                    return false;
                };
                (form.generating, form.generation_id)
            }
        };
        !generating && form_generation_id == generation_id
    }

    /// Once the user accepts the double-payment risk, overrides the pin for one attempt and
    /// submits again from the form.
    pub(super) fn continue_despite_input_pin(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.input_pin_conflict_retryable(kind, key, generation_id) {
            return;
        }
        Self::confirm_double_payment_risk(window, cx, move |root, window, cx| {
            if !root.input_pin_conflict_retryable(kind, key, generation_id) {
                return;
            }
            let pin = match kind {
                DeliveryFormKind::Send => root.send_forms.get_mut(&key).and_then(|form| {
                    form.input_pin_conflict = false;
                    form.input_pin.as_mut()
                }),
                DeliveryFormKind::Unshield => root.unshield_forms.get_mut(&key).and_then(|form| {
                    form.input_pin_conflict = false;
                    form.input_pin.as_mut()
                }),
            };
            if let Some(pin) = pin {
                pin.overridden = true;
            }
            root.close_private_broadcaster_progress_dialog(window, cx);
            match kind {
                DeliveryFormKind::Send => {
                    root.generate_send_calldata_from_form(key, window, cx);
                }
                DeliveryFormKind::Unshield => {
                    root.generate_unshield_calldata_from_form(key, window, cx);
                }
            }
        });
    }

    pub(super) fn retry_public_broadcaster_with_custom_fee(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((result, _)) = self.refused_public_broadcaster_retry(kind, key, generation_id)
        else {
            return;
        };
        let retry = BroadcasterFeeRetry {
            generation_id,
            fee_amount: result.fee_amount,
            broadcaster: result.broadcaster.clone(),
        };
        // The editor opens over the progress dialog, which stays until authorization opens.
        self.open_broadcaster_fee_editor(kind, key, Some(retry), window, cx);
    }

    /// Fetches a refused submission's new estimate, then opens spend authorization for it.
    /// `expected_broadcaster` fails the retry if the estimate lands on a different broadcaster.
    pub(in crate::root) fn start_refused_broadcaster_retry(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        expected_broadcaster: Option<String>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .refused_public_broadcaster_retry(kind, key, generation_id)
            .is_none()
        {
            return;
        }
        let input = self.broadcaster_retry_estimate_input(kind, key, cx);
        let request = match input.map_or(Ok(None), |input| {
            self.prepare_private_broadcaster_estimate(&input)
        }) {
            Ok(Some(request)) => request,
            Ok(None) => {
                self.fail_refused_broadcaster_retry(
                    "No broadcaster can take this retry right now.".into(),
                    cx,
                );
                return;
            }
            Err(error) => {
                self.fail_refused_broadcaster_retry(error.to_string(), cx);
                return;
            }
        };
        if let Some(progress) = self.private_broadcaster_progress.as_mut() {
            progress.refusal_retry_pending = true;
            progress.refusal_retry_error = None;
        }
        // Claiming the form's estimate id makes a background estimate that lands meanwhile stale.
        self.cost_estimate_seq = self.cost_estimate_seq.wrapping_add(1);
        let estimate_id = self.cost_estimate_seq;
        if let Some((id, estimating)) = self.public_broadcaster_estimate_claim(kind, key) {
            *id = estimate_id;
            *estimating = true;
        }
        cx.notify();

        let http = self.http.clone();
        let join = self
            .runtime
            .spawn(async move { request.estimate(&http).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = match join.await {
                Ok(result) => result,
                Err(error) => Err(eyre::eyre!("retry cost estimate task failed: {error}")),
            };
            let result = result.and_then(|estimate| match &expected_broadcaster {
                Some(expected) if estimate.broadcaster.railgun_address != *expected => {
                    Err(eyre::eyre!(
                        "The broadcaster that refused is no longer available. Try another broadcaster."
                    ))
                }
                _ => Ok(estimate),
            });
            let _ = this.update_in(cx, |root, window, cx| {
                let retryable = root
                    .refused_public_broadcaster_retry(kind, key, generation_id)
                    .is_some();
                let estimate = match result {
                    Ok(estimate) if retryable => estimate,
                    result => {
                        let released = root
                            .public_broadcaster_estimate_claim(kind, key)
                            .filter(|(id, _)| **id == estimate_id)
                            .map(|(id, estimating)| {
                                *id = 0;
                                *estimating = false;
                            })
                            .is_some();
                        if let Err(error) = result
                            && retryable
                        {
                            root.fail_refused_broadcaster_retry(format_report_chain(&error), cx);
                        } else {
                            // The dialog closed or moved on; nothing is submitted. A newer
                            // submission's progress keeps its own pending state.
                            if let Some(progress) = root.private_broadcaster_progress.as_mut()
                                && progress.accepts_update(kind, key, generation_id)
                            {
                                progress.refusal_retry_pending = false;
                            }
                            cx.notify();
                        }
                        // The form keeps a current estimate, unless a newer estimate already
                        // claimed it.
                        if released {
                            root.schedule_public_broadcaster_cost_estimate(kind, key, cx);
                        }
                        return;
                    }
                };
                match kind {
                    DeliveryFormKind::Send => {
                        let form = root.send_forms.get_mut(&key).expect("validated form");
                        if form.broadcaster_choice == BroadcasterChoice::Random {
                            form.shown_broadcaster =
                                Some(estimate.broadcaster.railgun_address.clone());
                        }
                        form.cost_estimate = Some(estimate);
                        form.cost_estimate_pending = false;
                        form.estimating_cost = false;
                        form.estimate_id = 0;
                    }
                    DeliveryFormKind::Unshield => {
                        let form = root.unshield_forms.get_mut(&key).expect("validated form");
                        if form.broadcaster_choice == BroadcasterChoice::Random {
                            form.shown_broadcaster =
                                Some(estimate.broadcaster.railgun_address.clone());
                        }
                        form.cost_estimate = Some(estimate);
                        form.cost_estimate_pending = false;
                        form.estimating_cost = false;
                        form.estimate_id = 0;
                    }
                }
                root.close_private_broadcaster_progress_dialog(window, cx);
                match kind {
                    DeliveryFormKind::Send => {
                        root.generate_send_calldata_from_form(key, window, cx);
                    }
                    DeliveryFormKind::Unshield => {
                        root.generate_unshield_calldata_from_form(key, window, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// The form's estimate id and in-flight flag.
    fn public_broadcaster_estimate_claim(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
    ) -> Option<(&mut u64, &mut bool)> {
        match kind {
            DeliveryFormKind::Send => self
                .send_forms
                .get_mut(&key)
                .map(|form| (&mut form.estimate_id, &mut form.estimating_cost)),
            DeliveryFormKind::Unshield => self
                .unshield_forms
                .get_mut(&key)
                .map(|form| (&mut form.estimate_id, &mut form.estimating_cost)),
        }
    }

    fn fail_refused_broadcaster_retry(&mut self, error: String, cx: &mut Context<'_, Self>) {
        if let Some(progress) = self.private_broadcaster_progress.as_mut() {
            progress.refusal_retry_pending = false;
            progress.refusal_retry_error = Some(Arc::from(error));
        }
        cx.notify();
    }

    pub(in crate::root) fn open_self_broadcast_gas_retry_dialog(
        &self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        retry_kind: SelfBroadcastGasRetryKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id)
            || progress.self_broadcast_command_tx.is_none()
        {
            return;
        }
        let Some((mut max_fee, mut max_tip)) = progress.self_broadcast_current_gas_fee else {
            return;
        };
        if retry_kind == SelfBroadcastGasRetryKind::SpeedUp {
            max_fee = self_broadcast_replacement_bumped_fee(max_fee);
            max_tip = self_broadcast_replacement_bumped_fee(max_tip);
        }
        let root = cx.entity();
        let content = cx.new(|cx| {
            SelfBroadcastGasRetryDialogContent::new(
                root,
                kind,
                key,
                generation_id,
                retry_kind,
                max_fee,
                max_tip,
                window,
                cx,
            )
        });
        let dialog_width =
            (window.viewport_size().width * 0.92).min(SELF_BROADCAST_GAS_RETRY_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        window.open_wallet_dialog(cx, move |dialog, _window, _cx| {
            let submit_content = content.clone();
            dialog
                .on_ok(move |_event, window, cx| {
                    submit_content.update(cx, |content, cx| content.submit(window, cx));
                    false
                })
                .w(dialog_width)
                .max_h(dialog_max_height)
                .child(content.clone())
        });
    }

    pub(super) fn submit_self_broadcast_gas_retry(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        retry_kind: SelfBroadcastGasRetryKind,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        let Some(command_tx) = progress.self_broadcast_command_tx.as_ref() else {
            return;
        };
        let command_kind = match retry_kind {
            SelfBroadcastGasRetryKind::RetryStep | SelfBroadcastGasRetryKind::RetryEstimate => {
                SelfBroadcastCommandKind::Retry
            }
            SelfBroadcastGasRetryKind::SpeedUp => SelfBroadcastCommandKind::Replacement,
        };
        let send_result = command_tx.send(SelfBroadcastCommand {
            kind: command_kind,
            gas_fee: SelfBroadcastGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            },
        });
        if let Some(progress) = self.private_broadcaster_progress.as_mut() {
            progress.self_broadcast_action_error = send_result.err().map(|_| {
                Arc::from("Self-broadcast session is no longer accepting retry commands.")
            });
        }
        cx.notify();
    }

    pub(super) fn submit_self_broadcast_step_retry(
        &mut self,
        kind: DeliveryFormKind,
        key: UnshieldAssetKey,
        generation_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(progress) = self.private_broadcaster_progress.as_ref() else {
            return;
        };
        if !progress.accepts_update(kind, key, generation_id) {
            return;
        }
        let Some(command_tx) = progress.self_broadcast_command_tx.as_ref() else {
            return;
        };
        let gas_fee = progress.self_broadcast_current_gas_fee.map_or(
            SelfBroadcastGasFeeSelection::Auto,
            |(max_fee_per_gas, max_priority_fee_per_gas)| SelfBroadcastGasFeeSelection::Custom {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            },
        );
        let send_result = command_tx.send(SelfBroadcastCommand {
            kind: SelfBroadcastCommandKind::Retry,
            gas_fee,
        });
        if let Some(progress) = self.private_broadcaster_progress.as_mut() {
            progress.self_broadcast_action_error = send_result.err().map(|_| {
                Arc::from("Self-broadcast session is no longer accepting retry commands.")
            });
        }
        cx.notify();
    }
}
