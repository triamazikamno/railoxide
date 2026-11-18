#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn set_public_action_amount_to_max(
        &mut self,
        mode: PublicActionMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(entry) = self.selected_public_balance_entry() else {
            return;
        };
        let Some(amount) = entry.amount.amount() else {
            return;
        };
        let decimals = entry.asset.decimals;
        if entry.asset.id != PublicAssetId::Native {
            self.set_public_action_amount_input(mode, amount, decimals, window, cx);
            self.set_public_action_error(mode, None);
            cx.notify();
            return;
        }
        if self.native_shield_stealth_applies(mode) {
            self.set_native_shield_amount_to_max(amount, decimals, window, cx);
            return;
        }

        let Some(public_account_uuid) = self.public_form.selected_account_uuid.clone() else {
            return;
        };
        let chain_id = self.last_evm_chain;
        let selected_wallet_id = self.selected_wallet_id.clone();
        let symbol = entry.asset.symbol;
        let http = self.http.clone();
        // Outside the stealth route a native shield wraps; reserve its approval too.
        let steps = public_action_progress_steps_for_source(
            mode,
            PublicAssetId::Native,
            PublicAccountSource::Derived,
            true,
        );
        let profile = if mode == PublicActionMode::Shield && self.mimic_railway_shield_effective() {
            PublicShieldTransactionProfile::Railway
        } else {
            PublicShieldTransactionProfile::Railoxide
        };
        let Ok(effective_chain) = self.effective_chain_configs.enabled(chain_id).cloned() else {
            return;
        };
        let gas_fee = match self.public_action_gas_fee_selection(mode, cx) {
            Ok(selection) => selection,
            Err(error) => {
                self.set_public_action_error(mode, Some(Arc::from(error)));
                cx.notify();
                return;
            }
        };
        let join = self.runtime.spawn(async move {
            estimate_public_native_action_gas_reserve_with_profile_and_ceiling(
                chain_id,
                &steps,
                profile,
                &effective_chain,
                gas_fee,
                &http,
                None,
            )
            .await
        });
        self.set_public_action_error(mode, None);
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |root, window, cx| {
                if root.selected_wallet_id != selected_wallet_id
                    || root.last_evm_chain != chain_id
                    || root.public_form.action_mode != mode
                    || root.public_form.selected_asset != Some(PublicAssetId::Native)
                    || root.public_form.selected_account_uuid.as_deref()
                        != Some(public_account_uuid.as_ref())
                {
                    return;
                }
                match result {
                    Ok(Ok(reserve)) => {
                        match public_action_max_amount_after_reserve(amount, reserve) {
                            Some(max_amount) => {
                                root.set_public_action_amount_input(
                                    mode, max_amount, decimals, window, cx,
                                );
                                root.set_public_action_error(mode, None);
                            }
                            None => root.set_public_action_error(
                                mode,
                                Some(Arc::from(format!(
                                    "Not enough {symbol} balance after estimated gas"
                                ))),
                            ),
                        }
                    }
                    Ok(Err(error)) => root.set_public_action_error(
                        mode,
                        Some(Arc::from(format!(
                            "Could not estimate gas reserve for Max: {}",
                            format_report_chain(&error)
                        ))),
                    ),
                    Err(error) => root.set_public_action_error(
                        mode,
                        Some(Arc::from(format!(
                            "Could not estimate gas reserve for Max: {error}"
                        ))),
                    ),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::root) fn set_public_action_amount_input(
        &self,
        mode: PublicActionMode,
        amount: U256,
        decimals: u8,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let value = format_send_amount_input(amount, Some(decimals));
        let input = match mode {
            PublicActionMode::Shield => &self.public_form.shield_amount_input,
            PublicActionMode::Send | PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                &self.public_form.send_amount_input
            }
        };
        input.update(cx, |input, cx| input.set_value(value, window, cx));
    }

    pub(in crate::root) fn set_public_action_error(
        &mut self,
        mode: PublicActionMode,
        message: Option<Arc<str>>,
    ) {
        match mode {
            PublicActionMode::Shield => self.public_form.shield_error = message,
            PublicActionMode::Send | PublicActionMode::Wrap | PublicActionMode::Unwrap => {
                self.public_form.send_error = message;
            }
        }
    }
}
