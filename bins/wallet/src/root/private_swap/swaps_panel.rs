#[allow(clippy::wildcard_imports)]
use super::*;

impl WalletRoot {
    pub(in crate::root) fn ensure_private_swaps(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let origin_chain_id = self.last_evm_chain;
        let Some(chain) = self.effective_chain_configs.get(origin_chain_id) else {
            self.clear_private_swaps(cx);
            return;
        };
        if self.view_session.is_none()
            || (chain.swap_profile().is_none() && chain.bridge_origin_profile().is_none())
        {
            self.clear_private_swaps(cx);
            return;
        }
        let session = self.stealth_session().filter(|session| {
            session.chain_id == origin_chain_id && chain.swap_profile().is_some()
        });
        // Public drafts and destination execution do not depend on the origin's private
        // session. Attach one when it arrives without replacing their view or capabilities.
        if let Some(panel) = self
            .private_swaps
            .as_mut()
            .filter(|panel| panel.origin_chain_id == origin_chain_id && panel.session.is_none())
            && let Some(session) = session.as_ref()
        {
            let owner = session.executor_owner();
            panel.session = Some(session.clone());
            panel.view.update(cx, |view, cx| {
                view.session = Some(session.clone());
                view.owner = owner;
                view.origin_changes = PrivateSwapsView::watch_origin_changes(
                    view.session.as_ref(),
                    view.owner.as_ref(),
                    cx,
                );
                view.reload_records();
                cx.notify();
            });
            return;
        }
        if self.private_swaps.as_ref().is_some_and(|panel| {
            panel.origin_chain_id == origin_chain_id
                && match (&panel.session, &session) {
                    (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
                    (None, None) => true,
                    _ => false,
                }
        }) {
            return;
        }
        self.clear_private_swaps(cx);
        let owner = session
            .as_ref()
            .and_then(|session| session.executor_owner());
        let root = cx.entity().downgrade();
        let runtime = self.runtime.clone();
        let view = cx.new(|cx| {
            PrivateSwapsView::new_for_origin(
                root,
                origin_chain_id,
                session.clone(),
                owner,
                runtime,
                window,
                cx,
            )
        });
        cx.observe(&view, |_, _, cx| cx.notify()).detach();
        let refresh = view.downgrade();
        cx.defer(move |cx| {
            let _ = refresh.update(cx, |view, cx| {
                view.refresh_public_swap_records(cx);
                cx.notify();
            });
        });
        self.private_swaps = Some(PrivateSwapsPanel {
            origin_chain_id,
            session,
            view,
        });
    }

    pub(in crate::root) fn clear_private_swaps(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(panel) = self.private_swaps.take() {
            panel.view.update(cx, |view, cx| {
                view.active = false;
                if let Some(job) = view.job.take() {
                    job.abort.abort();
                }
                view.job_revision = view.job_revision.wrapping_add(1);
                view.dialog = None;
                view.form = None;
                view.setup_retry = None;
                view.cancel = None;
                view.pending_authorization = None;
                view.reapproval = None;
                view.cancelled = None;
                view.tracking.clear();
                view.destinations.clear();
                view.attributions.clear();
                view.speed_up_refusals.clear();
                view.records.clear();
                view.public_records.clear();
                view.public_tracking.clear();
                view.public_destination_balances.clear();
                view.public_tracking_failures.clear();
                view.public_permit_used_up.clear();
                view.public_authorization = None;
                view.public_execution = None;
                view.public_job = None;
                view.public_running = None;
                cx.notify();
            });
        }
    }

    /// The swap view of the current executor session.
    pub(in crate::root) fn private_swaps_view(&self) -> Option<Entity<PrivateSwapsView>> {
        self.private_swaps
            .as_ref()
            .filter(|panel| {
                Some(panel.origin_chain_id) == self.selected_evm_chain()
                    && self.view_session.is_some()
                    && panel
                        .session
                        .as_ref()
                        .is_none_or(|session| self.stealth_session_is_current(session))
            })
            .map(|panel| panel.view.clone())
    }

    /// Read the swap form's balances again once a private snapshot of `chain_id` replaced the
    /// root's. The swap view hears of the same observation on its own and may read the root
    /// before this update, so it reads again after it.
    pub(in crate::root) fn refresh_private_swap_assets(
        &self,
        chain_id: u64,
        cx: &mut Context<'_, Self>,
    ) {
        if self
            .private_swaps
            .as_ref()
            .is_none_or(|panel| panel.origin_chain_id != chain_id)
        {
            return;
        }
        let Some(view) = self.private_swaps_view() else {
            return;
        };
        let view = view.downgrade();
        cx.defer(move |cx| {
            let _ = view.update(cx, |view, cx| {
                if view.form.is_some() {
                    view.refresh_form_assets(cx);
                    cx.notify();
                }
            });
        });
    }

    /// The selected chain's swap profile, when this wallet can run executor operations there.
    pub(in crate::root) fn private_swap_profile(&self) -> Option<SwapProfile> {
        self.private_swaps_view()?;
        self.effective_chain_configs
            .get(self.last_evm_chain)?
            .swap_profile()
    }

    pub(in crate::root) fn open_public_swap_form(
        &mut self,
        public_account_uuid: &str,
        sell: Option<Address>,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(account) = self
            .public_accounts
            .iter()
            .find(|account| {
                account.public_account_uuid == public_account_uuid
                    && account.status == wallet_ops::vault::PublicAccountStatus::Active
                    && account.scope != wallet_ops::vault::PublicAccountScope::Global
                    && !matches!(
                        account.source,
                        wallet_ops::vault::PublicAccountSource::ExecutorDerived(_)
                    )
            })
            .cloned()
        else {
            return;
        };
        self.ensure_private_swaps(window, cx);
        let Some(view) = self.private_swaps_view() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            view.update(cx, |view, cx| {
                view.open_public_form(account, sell.unwrap_or(Address::ZERO), window, cx);
            });
        });
    }

    pub(in crate::root) fn open_private_swap_form(
        &self,
        sell: Address,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(view) = self.private_swaps_view() else {
            return;
        };
        window.defer(cx, move |window, cx| {
            view.update(cx, |view, cx| view.open_new_form(sell, window, cx));
        });
    }
}
