use super::*;

impl WalletRoot {
    /// After a Public action succeeds, only the account that submitted it changed.
    pub(in crate::root) fn refresh_public_action_submitter(
        &self,
        chain_id: u64,
        public_account_uuid: &str,
        cx: &mut Context<'_, Self>,
    ) {
        self.schedule_account_public_balance_refresh(chain_id, [public_account_uuid], None, cx);
    }

    pub(in crate::root) fn open_public_action_dialog(
        &mut self,
        public_account_uuid: Arc<str>,
        asset: PublicAssetId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_all_dialogs(cx);
        self.set_public_selected_balance(public_account_uuid, asset, window, cx);
        self.public_form.action_mode = if self.selected_chain_has_railgun() {
            PublicActionMode::Shield
        } else {
            PublicActionMode::Send
        };
        self.reset_public_action_gas_fee_quotes();
        self.clear_public_action_dialog_inputs(window, cx);
        #[cfg(debug_assertions)]
        if self.open_public_shield_error_ui_fixture(asset, window, cx) {
            return;
        }
        let root = cx.entity();
        let dialog_width = (window.viewport_size().width * 0.92).min(PUBLIC_ACTION_DIALOG_WIDTH);
        let dialog_max_height = dialog_max_height(window);
        let content_width = secondary_dialog_content_width(dialog_width);
        let asset_label = public_asset_label(
            self.effective_chain_configs.get(self.last_evm_chain),
            asset,
            Some(&self.effective_token_registry),
        );
        let icon_path = public_asset_icon_path(
            self.last_evm_chain,
            asset,
            Some(&self.effective_token_registry),
        );
        window.open_wallet_dialog(cx, move |dialog, _window, cx| {
            let close_root = root.clone();
            let content_root = root.clone();
            dialog
                .w(dialog_width)
                .max_h(dialog_max_height)
                .title(public_action_title_row(
                    asset_label.clone(),
                    icon_path.clone(),
                ))
                .on_ok(|_, _, _| false)
                .on_close(move |_event, window, cx| {
                    close_root.update(cx, |root, cx| {
                        root.clear_public_action_dialog_inputs(window, cx);
                    });
                })
                .child(content_root.read(cx).render_public_action_dialog_content(
                    content_root.clone(),
                    content_width,
                    cx,
                ))
        });
        self.refresh_public_action_gas_fee_quote(self.public_form.action_mode, cx);
        cx.defer_in(window, |root, window, cx| {
            root.focus_public_action_dialog_input(window, cx);
        });
    }

    /// `shield:estimate-error`, `shield:funding-error`, `shield:native-pending` and
    /// `shield:fallback-pending` stage an inert progress dialog when a Public asset dialog is
    /// opened on a scratch profile. The other `shield:*` fixtures stage the native Shield form
    /// instead, and `hints:*` an ERC-20 Shield form. No action is submitted and no record is
    /// written.
    #[cfg(debug_assertions)]
    pub(in crate::root) fn open_public_shield_error_ui_fixture(
        &mut self,
        asset: PublicAssetId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let funding_error = match public_shield_ui_fixture() {
            None => return false,
            Some(PublicShieldUiFixture::EstimateError) => false,
            Some(PublicShieldUiFixture::FundingError) => true,
            Some(
                fixture @ (PublicShieldUiFixture::NativePending
                | PublicShieldUiFixture::FallbackPending),
            ) => {
                self.open_native_shield_pending_ui_fixture(fixture, window, cx);
                return true;
            }
            Some(
                fixture @ (PublicShieldUiFixture::MimicPrepared
                | PublicShieldUiFixture::NativePrepared
                | PublicShieldUiFixture::MimicUnavailable
                | PublicShieldUiFixture::Fallback),
            ) => {
                self.stage_native_shield_form_ui_fixture(fixture, window, cx);
                return false;
            }
            Some(PublicShieldUiFixture::Hints) => {
                self.stage_shield_hints_ui_fixture(window, cx);
                return false;
            }
        };
        let asset = self
            .effective_chain_configs
            .get(self.last_evm_chain)
            .and_then(|chain| chain.wrapped_native_token)
            .map_or(asset, PublicAssetId::Erc20);
        let message = Self::shield_error_ui_fixture_message(funding_error);
        let states = vec![
            PublicActionStepState {
                step: PublicActionProgressStep::Approve,
                status: if funding_error {
                    PublicActionStepStatus::Error
                } else {
                    PublicActionStepStatus::Done
                },
                tx_hash: None,
                message: funding_error.then(|| Arc::from(message)),
                interval: None,
            },
            PublicActionStepState {
                step: PublicActionProgressStep::Shield,
                status: if funding_error {
                    PublicActionStepStatus::NotStarted
                } else {
                    PublicActionStepStatus::Error
                },
                tx_hash: None,
                message: (!funding_error).then(|| Arc::from(message)),
                interval: None,
            },
        ];
        self.start_public_action_progress_with_states(
            PublicActionMode::Shield,
            states,
            public_asset_label(
                self.effective_chain_configs.get(self.last_evm_chain),
                asset,
                Some(&self.effective_token_registry),
            ),
            public_asset_icon_path(
                self.last_evm_chain,
                asset,
                Some(&self.effective_token_registry),
            ),
            PublicAccountSource::Derived,
            None,
            None,
            false,
        );
        self.public_form.action_stop_available = false;
        self.show_public_action_progress_dialog(window, cx);
        cx.notify();
        true
    }

    /// The error the `shield:funding-error` or `shield:estimate-error` progress dialog shows.
    #[cfg(debug_assertions)]
    const fn shield_error_ui_fixture_message(funding_error: bool) -> &'static str {
        if funding_error {
            "insufficient native balance for public shield estimated network fees"
        } else {
            "all public action query RPC attempts failed: estimate public action gas: server returned an error response: error code 3: execution reverted, data: \"0x\""
        }
    }

    #[cfg(debug_assertions)]
    pub(in crate::root) fn public_shield_error_ui_fixture_active() -> bool {
        public_shield_ui_fixture().is_some()
    }

    pub(in crate::root) fn focus_public_action_dialog_input(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match self.public_form.action_mode {
            PublicActionMode::Shield => self
                .public_form
                .shield_amount_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            PublicActionMode::Send => {
                let input = match self.public_form.public_send_kind {
                    PublicSendKind::Transfer => &self.public_form.send_recipient_input,
                    PublicSendKind::ContractCall => &self.public_form.advanced_send_to_input,
                    PublicSendKind::Deploy => &self.public_form.advanced_send_value_input,
                };
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
            PublicActionMode::Wrap | PublicActionMode::Unwrap => self
                .public_form
                .send_amount_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
        }
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
            &self.public_form.shield_amount_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.public_form
            .advanced_send_data_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.clear_trezor_app_passphrase_input(window, cx);
        self.clear_trezor_pin_matrix_prompt(cx);
        self.public_form.send_error = None;
        self.public_form.public_send_kind = PublicSendKind::Transfer;
        self.public_form.advanced_send_estimate = None;
        self.public_form.advanced_send_estimate_invalidated = false;
        self.invalidate_advanced_public_send_estimate();
        self.public_form.shield_error = None;
        self.public_form.native_shield_preparation.invalidate();
        // The dialog's operation ends; a submitted draft keeps its own clone.
        self.public_form.operation = None;
        if !self.public_form.sending && !self.public_form.shielding {
            self.clear_public_action_progress_state();
        }
    }

    /// The Public action dialog's operation client for `account`. Another acting account
    /// replaces it.
    pub(in crate::root) fn public_form_operation(
        &mut self,
        account: &Arc<str>,
    ) -> OperationHttpClientSlot {
        match &self.public_form.operation {
            Some((owner, operation)) if owner == account => operation.clone(),
            _ => {
                let operation = OperationHttpClientSlot::new(self.http.clone());
                self.public_form.operation = Some((Arc::clone(account), operation.clone()));
                operation
            }
        }
    }

    pub(in crate::root) fn set_public_action_mode(
        &mut self,
        mode: PublicActionMode,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if mode == PublicActionMode::Shield && !self.selected_chain_has_railgun() {
            return;
        }
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
}
