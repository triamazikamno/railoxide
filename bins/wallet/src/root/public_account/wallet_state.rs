#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn clear_public_wallet_runtime_state(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) {
        self.clear_stealth_accounts(cx);
        self.clear_private_swaps(cx);
        self.clear_tron_wallet_state();
        self.public_balance_cache.clear();
        self.public_accounts.clear();
        self.public_balance_snapshot = None;
        self.public_balance_error = None;
        self.public_inactive_balance_error = None;
        self.public_form.selected_account_uuid = None;
        self.public_form.editing_account_uuid = None;
        self.public_form.selected_asset = None;
        self.public_form.mimic_railway_shield = self.mimic_railway_shields_by_default;
        self.public_form.shield_approval_mode = wallet_ops::PublicShieldApprovalMode::Exact;
        self.public_form.native_shield_preparation.invalidate();
        self.public_form.operation = None;
        self.public_form.public_send_kind = PublicSendKind::Transfer;
        self.public_form.advanced_send_estimate = None;
        self.public_form.advanced_send_estimate_invalidated = false;
        self.invalidate_advanced_public_send_estimate();
        self.clear_public_action_progress_state();
        self.public_form.next_derived_index = None;
        self.public_form.derive_preview.reset();
        self.public_form.next_account_label_number = 1;
        self.public_form.error = None;
        self.public_form.send_error = None;
        self.public_form.shield_error = None;
        self.walletconnect.clear_runtime();
        self.sync_platform_attention();
        self.public_form.importing_account = false;
        self.public_form.sending = false;
        self.public_form.shielding = false;
        self.reset_public_asset_focus();
        self.public_form.open_section = PublicAccountStatus::Active;
    }

    pub(in crate::root) fn reset_public_wallet_state(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.clear_public_wallet_runtime_state(cx);
        for input in [
            &self.public_form.add_password_input,
            &self.public_form.import_label_input,
            &self.public_form.import_private_key_input,
            &self.public_form.import_password_input,
            &self.public_form.edit_label_input,
            &self.public_form.send_recipient_input,
            &self.public_form.send_amount_input,
            &self.public_form.advanced_send_to_input,
            &self.public_form.advanced_send_value_input,
            &self.public_form.shield_amount_input,
            &self.walletconnect.uri_input,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.public_form
            .advanced_send_data_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.public_form.import_global = false;
        self.public_form.action_mode = PublicActionMode::Shield;
        self.public_form.public_send_kind = PublicSendKind::Transfer;
        self.public_form.advanced_send_estimate = None;
        self.public_form.advanced_send_estimate_invalidated = false;
        self.invalidate_advanced_public_send_estimate();
    }
}
