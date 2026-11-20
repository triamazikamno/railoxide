#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// Timer, view, chain, settings and reload triggers: only with automatic refresh on.
    pub(in crate::root) fn schedule_automatic_public_balance_refresh(
        &self,
        cx: &mut Context<'_, Self>,
    ) {
        if self.ui_state.public_auto_refresh {
            self.schedule_global_public_balance_refresh(PublicAccountStatus::Active, cx);
        }
    }

    /// The section's included accounts on the selected network, whatever the toggle says. With
    /// Tron selected these are Tron accounts; EVM accounts are read again on an EVM chain.
    pub(in crate::root) fn schedule_global_public_balance_refresh(
        &self,
        status: PublicAccountStatus,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.public_global_refresh_ids(status);
        if self.tron_selected() {
            self.schedule_tron_balance_refresh(accounts, cx);
        } else {
            self.schedule_account_public_balance_refresh(self.last_evm_chain, accounts, None, cx);
        }
    }

    /// Exactly the named accounts, ignoring the toggle and exclusions.
    pub(in crate::root) fn schedule_account_public_balance_refresh<I, S>(
        &self,
        chain_id: u64,
        accounts: I,
        window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<'_, Self>,
    ) where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let accounts = accounts
            .into_iter()
            .map(Into::into)
            .collect::<Vec<String>>();
        if accounts.is_empty() {
            return;
        }
        self.publish_gateway_desktop_state();
        let Some(scope) = self.current_public_balance_scope(chain_id) else {
            return;
        };
        if let Some(ticket) = self.public_balance_cache.begin_refresh(&scope, accounts) {
            self.run_public_balance_refresh(ticket, window, cx);
        }
        self.publish_gateway_desktop_state();
        cx.notify();
    }

    pub(in crate::root) fn run_public_balance_refresh(
        &self,
        ticket: PublicBalanceRefreshTicket,
        window: Option<gpui::AnyWindowHandle>,
        cx: &Context<'_, Self>,
    ) {
        // Follow-up tickets recapture current authority and accounts before starting RPC.
        self.publish_gateway_desktop_state();
        if !self.public_balance_cache.is_current_scope(ticket.scope()) {
            let _ = self.public_balance_cache.finish_refresh(ticket, None);
            return;
        }
        let chain_id = ticket.scope().chain_id();
        // The read skips an account that is unavailable on this chain.
        let account = self
            .public_accounts
            .iter()
            .find(|account| ticket.includes_account(&account.public_account_uuid))
            .cloned();
        let http = self.http.clone();
        let (Some(account), Ok(effective_chain)) = (
            account,
            self.effective_chain_configs.enabled(chain_id).cloned(),
        ) else {
            // Finish the ticket so its id stops showing as pending.
            if let Some(follow_up) = self
                .public_balance_cache
                .finish_refresh(ticket, None)
                .follow_up
            {
                self.run_public_balance_follow_up(follow_up, window, cx);
            }
            return;
        };
        let effective_token_registry = self.effective_token_registry.clone();
        let minimum = ticket.minimum_block();
        let join = self.runtime.spawn(async move {
            refresh_public_balances_at_least(
                chain_id,
                &account,
                &effective_chain,
                Some(&effective_token_registry),
                &http,
                minimum,
            )
            .await
        });
        let cache = self.public_balance_cache.clone();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let (sync_window, focus_window) =
                Self::apply_balance_ticket(&this, cx, &cache, ticket, |root, ticket, cx| {
                    let (sync_selects, restore_focus) =
                        root.apply_public_balance_refresh_result(ticket, result, window, cx);
                    let target = window.or_else(|| cx.windows().first().copied());
                    (
                        target.filter(|_| sync_selects),
                        target.filter(|_| restore_focus),
                    )
                });
            if let Some(window) = focus_window {
                let _ = window.update(cx, |_, window, cx| {
                    let _ = this.update(cx, |root, cx| {
                        root.public_form.list_focus.focus(window, cx);
                    });
                });
            }
            if let Some(window) = sync_window {
                let _ = window.update(cx, |_, window, cx| {
                    let _ = this.update(cx, |root, cx| {
                        root.sync_self_broadcast_gas_payer_selects(window, cx);
                    });
                });
            }
        })
        .detach();
    }

    /// Runs a follow-up after a random 0.5-3 s, so successive accounts' reads aren't
    /// timed together. A scope change meanwhile finishes it without reads.
    pub(super) fn run_public_balance_follow_up(
        &self,
        ticket: PublicBalanceRefreshTicket,
        window: Option<gpui::AnyWindowHandle>,
        cx: &Context<'_, Self>,
    ) {
        Self::run_balance_follow_up(
            self.public_balance_cache.clone(),
            ticket,
            cx,
            move |root, ticket, cx| root.run_public_balance_refresh(ticket, window, cx),
        );
    }

    /// Runs `refresh` with a follow-up `ticket` from `cache` after a random 0.5-3 s. If the
    /// root has gone by then, `cache` finishes the ticket instead.
    pub(in crate::root) fn run_balance_follow_up<F>(
        cache: PublicBalanceCache,
        ticket: PublicBalanceRefreshTicket,
        cx: &Context<'_, Self>,
        refresh: F,
    ) where
        F: FnOnce(&mut Self, PublicBalanceRefreshTicket, &mut Context<'_, Self>) + 'static,
    {
        let delay = isolated_read_jitter();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            Self::apply_balance_ticket(&this, cx, &cache, ticket, refresh);
        })
        .detach();
    }
}
