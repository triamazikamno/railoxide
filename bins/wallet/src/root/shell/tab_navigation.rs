#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// The tab a wallet opens on: Private, or Public where the selected chain has no Railgun.
    pub(in crate::root) fn default_wallet_tab(&self) -> WalletTab {
        if self.selected_chain_has_railgun() {
            WalletTab::default()
        } else {
            WalletTab::Public
        }
    }

    pub(in crate::root) fn select_wallet_tab(
        &mut self,
        tab: WalletTab,
        cx: &mut Context<'_, Self>,
    ) {
        if tab.needs_railgun() && !self.selected_chain_has_railgun() {
            return;
        }
        if self.active_wallet_tab == tab {
            return;
        }
        self.active_wallet_tab = tab;
        self.focus_utxo_table_on_render = should_focus_utxo_table(
            self.active_activity,
            self.active_wallet_tab,
            self.chain_states.get(&self.last_evm_chain),
        );
        if tab == WalletTab::Public {
            self.focus_public_account_search_on_render = true;
            self.schedule_automatic_public_balance_refresh(cx);
        }
        if tab == WalletTab::Activity {
            // Reading again picks up address-book labels and accounts changed since.
            self.load_wallet_activity(cx);
            self.mark_wallet_activity_seen(cx);
            self.wallet_activity.request_list_focus();
        }
        cx.notify();
    }

    /// Selects the next (or previous) wallet tab with wrap-around, skipping tabs
    /// the selected chain cannot show.
    pub(in crate::root) fn cycle_wallet_tab(&mut self, forward: bool, cx: &mut Context<'_, Self>) {
        let tabs = WalletTab::ALL;
        let count = tabs.len();
        let current = tabs
            .iter()
            .position(|tab| *tab == self.active_wallet_tab)
            .unwrap_or(0);
        let has_railgun = self.selected_chain_has_railgun();
        let next = (1..count)
            .map(|step| {
                if forward {
                    tabs[(current + step) % count]
                } else {
                    tabs[(current + count - step) % count]
                }
            })
            .find(|tab| !tab.needs_railgun() || has_railgun);
        if let Some(tab) = next {
            self.select_wallet_tab(tab, cx);
        }
    }

    /// Keeps wallet shortcuts reachable when the focused element disappears, such as the
    /// vault password input after unlock or the public search input after a tab switch.
    pub(in crate::root) fn focus_wallet_view_after_focus_loss(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.unlocked_wallet_activity_is_active()
            && should_apply_background_focus(window.has_active_dialog(cx))
        {
            self.wallet_focus.focus(window, cx);
        }
    }

    fn unlocked_wallet_activity_is_active(&self) -> bool {
        matches!(self.vault_state, VaultState::ViewUnlocked)
            && self.active_activity == Activity::Wallet
    }

    pub(in crate::root) fn focus_public_account_search_if_requested(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.focus_public_account_search_on_render || !self.public_wallet_tab_is_active() {
            return;
        }

        self.public_form
            .search_input
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        self.focus_public_account_search_on_render = false;
    }

    fn public_wallet_tab_is_active(&self) -> bool {
        self.active_activity == Activity::Wallet && self.active_wallet_tab == WalletTab::Public
    }
}
