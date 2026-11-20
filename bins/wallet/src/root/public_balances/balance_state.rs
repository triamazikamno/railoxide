#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn selected_public_balance_entry(&self) -> Option<PublicBalanceEntry> {
        let public_account_uuid = self.public_form.selected_account_uuid.as_deref()?;
        let asset = self.public_form.selected_asset?;
        let status = self
            .public_account_for_uuid(Some(public_account_uuid))?
            .status;
        self.public_balance_entry(public_account_uuid, asset, status)
    }

    pub(in crate::root) fn public_balance_entry(
        &self,
        public_account_uuid: &str,
        asset: PublicAssetId,
        status: PublicAccountStatus,
    ) -> Option<PublicBalanceEntry> {
        public_balance_entry_for_chain(
            self.public_balance_snapshot.as_deref(),
            self.last_evm_chain,
            public_account_uuid,
            asset,
            status,
        )
    }

    pub(in crate::root) fn clear_public_chain_balance_state(&mut self) {
        self.public_balance_snapshot = None;
        self.public_balance_error = None;
        self.public_inactive_balance_error = None;
        self.public_form.selected_asset = None;
        self.reset_public_asset_focus();
        // A handed-off Tron Send isn't this chain's; aborting it would lose its outcome.
        if !self.claim_tron_send_stop_unless_handed_off() {
            // Reject the aborted task's late callbacks and release admission for the next action.
            self.public_form.action_generation = self.public_form.action_generation.wrapping_add(1);
            self.public_form.sending = false;
            self.public_form.shielding = false;
            self.clear_public_action_progress_state();
        }
        self.public_form.send_error = None;
        self.public_form.shield_error = None;
    }

    /// Shows the balances already read this session for the selected chain, without RPC.
    pub(in crate::root) fn restore_public_balance_snapshot(&mut self) {
        self.public_balance_snapshot = self
            .current_public_balance_scope(self.last_evm_chain)
            .and_then(|scope| self.public_balance_cache.snapshot(&scope))
            .map(Arc::new);
    }

    /// The listed network's accounts of `status` that a global refresh reads.
    pub(in crate::root) fn public_global_refresh_ids(
        &self,
        status: PublicAccountStatus,
    ) -> Vec<String> {
        let chain_id = self.listed_public_chain();
        self.listed_public_accounts()
            .iter()
            .filter(|account| {
                account.status == status
                    && account.is_available_on_chain(chain_id)
                    && !self
                        .ui_state
                        .public_refresh_excluded
                        .contains(&account.public_account_uuid)
            })
            .map(|account| account.public_account_uuid.clone())
            .collect()
    }

    /// Selected-chain account ids in the in-flight refresh or queued behind it.
    pub(in crate::root) fn public_refreshing_accounts(&self) -> BTreeSet<String> {
        self.current_public_balance_scope(self.last_evm_chain)
            .map(|scope| self.public_balance_cache.refreshing_accounts(&scope))
            .unwrap_or_default()
    }

    /// Selected-chain account ids whose latest refresh failed without returning their row.
    pub(in crate::root) fn public_failed_accounts(&self) -> BTreeSet<String> {
        self.current_public_balance_scope(self.last_evm_chain)
            .map(|scope| self.public_balance_cache.failed_accounts(&scope))
            .unwrap_or_default()
    }

    pub(in crate::root) fn public_section_refreshing(&self, status: PublicAccountStatus) -> bool {
        let pending = self.listed_refreshing_accounts();
        self.listed_public_accounts().iter().any(|account| {
            account.status == status && pending.contains(&account.public_account_uuid)
        })
    }

    pub(in crate::root) fn set_public_refresh_excluded(
        &mut self,
        public_account_uuid: &str,
        excluded: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let exclusions = &mut self.ui_state.public_refresh_excluded;
        let changed = if excluded {
            exclusions.insert(public_account_uuid.to_owned())
        } else {
            exclusions.remove(public_account_uuid)
        };
        if changed {
            self.save_ui_state();
            cx.notify();
        }
    }
}
