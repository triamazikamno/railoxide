#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn request_spend_authorization(
        &mut self,
        intent: SpendAuthorizationIntent,
        summary: SpendAuthorizationSummary,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let summary = if self.selected_wallet_source().is_hardware_derived()
            && intent.hardware_executor_action(self).is_some()
        {
            summary.requiring_explicit_review()
        } else {
            summary
        };
        let summary = if intent.gateway_execution().is_some()
            || matches!(
                &intent,
                SpendAuthorizationIntent::PublicSwap(..)
                    | SpendAuthorizationIntent::PublicSwapSource { .. }
            ) {
            summary.requiring_explicit_review()
        } else {
            summary
        };
        if self.selected_wallet_source().is_hardware_derived()
            && (intent.uses_private_wallet() || intent.hardware_executor_action(self).is_some())
        {
            intent.private_attention(
                "Approve on your hardware wallet",
                "Use the desktop app to approve this private spend on your device.",
            );
            self.clear_spend_authorization(cx);
            let key = match &intent {
                SpendAuthorizationIntent::PrepareExecutorUnshield(key, ..)
                | SpendAuthorizationIntent::ExecutorUnshield(key, ..) => Some(*key),
                _ => None,
            };
            let payer = if let SpendAuthorizationIntent::PublicShield(draft) = &intent
                && draft.native_operation.is_some()
            {
                // A stealth-route native shield's payer sends the stealth account's execution,
                // as a gas payer does. The draft admits only a software or imported payer here.
                Some(draft.public_account_uuid.to_string())
            } else if let Some(key) = key
                && let Some(draft) = self.unshield_spend_draft(key, cx)
                && draft.delivery_mode == DeliveryMode::SelfBroadcast
            {
                let Some(payer) = self.selected_self_broadcast_gas_payer_account(
                    draft.self_broadcast_public_account_uuid.as_deref(),
                ) else {
                    return;
                };
                if !matches!(
                    payer.source,
                    PublicAccountSource::Derived | PublicAccountSource::Imported
                ) {
                    self.set_unshield_form_error(key, "Select a software or imported gas payer, or a broadcaster, for this executor action.", cx);
                    return;
                }
                Some(payer.public_account_uuid.clone())
            } else {
                None
            };
            if let Some(payer) = payer {
                self.open_spend_authorization_dialog(
                    SpendAuthorizationIntent::ExecutorGasPassword {
                        intent: Box::new(intent),
                        summary: Box::new(summary.clone()),
                        payer,
                    },
                    summary.requiring_explicit_review(),
                    window,
                    cx,
                );
                return;
            }
            self.open_hardware_spend_authorization_dialog(
                HardwareSpendAuthorizationCompletion::Continue(intent),
                summary,
                window,
                cx,
            );
            return;
        }
        if spend_authorization_can_use_cached_password(&summary)
            && let Some(password) = self.valid_spend_authorization_password(cx)
        {
            match self.desktop_spend_authorization(password) {
                Ok(authorization) => {
                    if !intent.approve_gateway_review(self) {
                        return;
                    }
                    self.continue_authorized_spend(intent, authorization, window, cx);
                }
                Err(message) => self.set_vault_error(message, cx),
            }
            return;
        }

        intent.private_attention(
            "Authorize in the desktop app",
            "Enter your vault password in the desktop app to continue.",
        );
        self.open_spend_authorization_dialog(intent, summary, window, cx);
    }
}
