#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
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
            self.effective_chain_configs.get(self.last_evm_chain),
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
        let chain_id = self.last_evm_chain;
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        self.invalidate_advanced_public_send_estimate();
        self.public_form.advanced_send_estimate_pending = true;
        self.public_form.send_error = None;
        let generation = self.public_form.advanced_send_estimate_generation;
        let send_kind = self.public_form.public_send_kind;
        let selected_wallet_id = self.selected_wallet_id.clone();
        let http = self.http.clone();
        let operation = self.public_form_operation(&public_account_uuid);
        cx.spawn(async move |this, cx| {
            let result = match operation.client().await {
                Ok(client) => {
                    estimate_public_advanced_transaction(
                        PublicAdvancedTransactionEstimateRequest {
                            chain_id,
                            effective_chain,
                            from,
                            intent,
                            gas_fee,
                            access_list: None,
                        },
                        &http,
                        &client,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let _ = this.update(cx, |root, cx| {
                if root.selected_wallet_id != selected_wallet_id
                    || root.last_evm_chain != chain_id
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

    pub(super) fn set_advanced_public_send_validation_error(
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
}
