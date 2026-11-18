use super::*;

impl WalletRoot {
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
}
