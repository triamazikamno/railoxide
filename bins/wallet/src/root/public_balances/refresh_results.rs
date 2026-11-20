#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    /// Runs `apply` with `ticket` on the root. If the root has gone, `cache` finishes the
    /// ticket instead, and the result is `R::default()`.
    pub(in crate::root) fn apply_balance_ticket<R: Default>(
        this: &gpui::WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
        cache: &PublicBalanceCache,
        ticket: PublicBalanceRefreshTicket,
        apply: impl FnOnce(&mut Self, PublicBalanceRefreshTicket, &mut Context<'_, Self>) -> R,
    ) -> R {
        let mut ticket = Some(ticket);
        let applied = this.update(cx, |root, cx| {
            apply(root, ticket.take().expect("balance ticket available"), cx)
        });
        if let Some(ticket) = ticket {
            let _ = cache.finish_refresh(ticket, None);
            // The owner has gone away; discard any admitted follow-up as well.
            cache.clear();
        }
        applied.unwrap_or_default()
    }

    pub(in crate::root) fn apply_public_balance_refresh_result(
        &mut self,
        ticket: PublicBalanceRefreshTicket,
        result: Result<Result<PublicBalanceSnapshot, eyre::Report>, tokio::task::JoinError>,
        window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<'_, Self>,
    ) -> (bool, bool) {
        let scope = ticket.scope().clone();
        let covers = |status| {
            self.public_accounts.iter().any(|account| {
                account.status == status && ticket.includes_account(&account.public_account_uuid)
            })
        };
        let active = covers(PublicAccountStatus::Active);
        let inactive = covers(PublicAccountStatus::Inactive);
        let (snapshot, error, join_failed) = match result {
            Ok(Ok(snapshot)) => (Some(snapshot), None, false),
            Ok(Err(error)) => (None, Some(format_report_chain(&error)), false),
            Err(error) => (None, Some(error.to_string()), true),
        };
        let succeeded = snapshot.is_some();
        let completion = self.public_balance_cache.finish_refresh(ticket, snapshot);
        let selected = scope.chain_id() == self.last_evm_chain;
        let mut restore_focus = false;
        if completion.accepted && selected {
            // Keep the focused tile and any open asset menu across a background
            // refresh; dropping a focused menu would strand keyboard focus.
            let focused_asset = self.public_focused_asset();
            let previous = self.public_balance_snapshot.clone();
            self.public_balance_snapshot = self.public_balance_cache.snapshot(&scope).map(Arc::new);
            restore_focus = self.revalidate_public_asset_focus(focused_asset);
            // Keep the latest failure; rows decide whether it still applies, so another
            // account's success doesn't clear it.
            if let Some(error) = &error {
                if active {
                    self.public_balance_error = Some(Arc::from(if join_failed {
                        format!("Public balance refresh failed: {error}")
                    } else {
                        error.clone()
                    }));
                }
                if inactive {
                    self.public_inactive_balance_error = Some(Arc::from(if join_failed {
                        format!("Inactive public balance refresh failed: {error}")
                    } else {
                        error.clone()
                    }));
                }
            }
            if active
                && succeeded
                && let Some(snapshot) = self.public_balance_snapshot.clone()
            {
                self.revalidate_sponsored_estimates_for_public_balance_change(
                    previous.as_deref(),
                    &snapshot,
                    cx,
                );
            }
        }
        if let Some(follow_up) = completion.follow_up {
            self.run_public_balance_follow_up(follow_up, window, cx);
        }
        self.publish_gateway_desktop_state();
        cx.notify();
        (
            completion.accepted && selected && active && succeeded,
            restore_focus,
        )
    }
}
