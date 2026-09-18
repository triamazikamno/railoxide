use super::{
    Context, DesktopPrivateSpendAuthorization, ExecutorOperationId, SpendAuthorizationSummary,
    SpendAuthorizationSummaryRow, StealthAccountsView, StealthAction, Window,
};
use gpui_component::WindowExt as _;
use wallet_ops::vault::{PublicAccountMetadata, PublicAccountStatus};

#[cfg(test)]
mod tests;

impl StealthAccountsView {
    pub(super) fn open_public_account(
        &mut self,
        operation: ExecutorOperationId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.job.is_some() || !self.session_is_current(cx) {
            return;
        }
        let Some(record) = self
            .records
            .iter()
            .find(|record| record.operation() == operation)
        else {
            return;
        };
        let Some(address) = record.address() else {
            return;
        };
        let existing = record.public_account_uuid().and_then(|id| {
            self.root.upgrade().and_then(|root| {
                root.read(cx)
                    .public_accounts
                    .iter()
                    .find(|account| {
                        account.public_account_uuid == id
                            && account.status == PublicAccountStatus::Active
                    })
                    .cloned()
            })
        });
        if let Some(account) = existing {
            self.select_public_account(&account, window, cx);
            return;
        }
        self.request_authorization(StealthAction::AddToPublic { operation },
            SpendAuthorizationSummary::new("Add to Public", "This address becomes available for normal Public actions and balance refreshes on this chain. Adding it moves no funds and does not cancel pending operations or give sites access.", vec![
                SpendAuthorizationSummaryRow::new("Account", format!("#{}", record.index())),
                SpendAuthorizationSummaryRow::new("Address", address.to_checksum(None)).with_shortened_copyable(),
                SpendAuthorizationSummaryRow::new("Chain", railgun_ui::chain_name(self.session.chain_id).map_or_else(|| self.session.chain_id.to_string(), str::to_owned)),
            ]), window, cx);
    }

    pub(super) fn continue_public_registration(
        &mut self,
        operation: ExecutorOperationId,
        authorization: DesktopPrivateSpendAuthorization,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.job.is_some() {
            return;
        }
        self.error = None;
        self.job_revision = self.job_revision.wrapping_add(1);
        let revision = self.job_revision;
        let owner = self.owner.clone();
        let join = self.runtime.spawn(async move {
            owner
                .register_public_account(operation, &authorization)
                .await
        });
        self.job = Some(join.abort_handle());
        cx.spawn_in(window, async move |this, cx| {
            let result = join.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.job_revision != revision || !this.session_is_current(cx) {
                    return;
                }
                this.job = None;
                this.reload_records();
                match result {
                    Ok(Ok(account)) => this.select_public_account(&account, window, cx),
                    Ok(Err(error)) => this.error = Some(error.to_string()),
                    Err(error) if !error.is_cancelled() => {
                        this.error =
                            Some("Adding this account stopped. Retry Add to Public.".into());
                    }
                    Err(_) => {}
                }
                this.refresh_visible(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn select_public_account(
        &self,
        account: &PublicAccountMetadata,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        window.close_sheet(cx);
        let _ = self.root.update(cx, |root, cx| {
            if !root.stealth_session_is_current(&self.session) {
                return;
            }
            root.reload_public_accounts(window, cx);
            root.set_public_selected_balance(
                account.public_account_uuid.clone().into(),
                wallet_ops::PublicAssetId::Native,
                window,
                cx,
            );
            if let Some(panel) = &mut root.stealth_accounts {
                panel.open = false;
            }
            root.focus_public_account_search_on_render = true;
            cx.notify();
        });
    }
}
