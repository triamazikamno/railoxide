use gpui::{Context, Window};
use wallet_ops::{
    PublicAssetId,
    gateway::{
        GatewayAccountBalances, GatewayAssetBalance, GatewayPublicCommand, GatewayPublicView,
    },
    vault::PublicAccountStatus,
};

use super::{
    WalletRoot,
    public_balances::{
        public_account_usd_total_label_for_chain, public_asset_icon_path,
        public_balance_amount_label, public_balance_usd_label,
    },
};

impl WalletRoot {
    pub(super) fn gateway_public_view(&self) -> GatewayPublicView {
        let Some(view) = self.view_session.as_ref() else {
            return GatewayPublicView::default();
        };
        let mut presentation = GatewayPublicView::default();
        presentation.drafts = self.gateway.drafts.borrow().views(self);
        presentation.selected_account = self
            .selected_public_account()
            .filter(|account| {
                account.is_active_for_wallet(view.wallet_id())
                    && account.is_available_on_chain(self.selected_chain)
            })
            .map(|account| account.public_account_uuid.clone());
        presentation.selected_chain = Some(self.selected_chain);
        presentation.refreshing = self.public_balance_refreshing;
        presentation.balance_error = self.public_balance_error.is_some();
        let snapshot = self
            .public_balance_snapshot
            .as_deref()
            .filter(|snapshot| snapshot.chain_id == self.selected_chain);
        for account in self.public_accounts.iter().filter(|account| {
            account.is_active_for_wallet(view.wallet_id())
                && account.is_available_on_chain(self.selected_chain)
        }) {
            let mut balances = GatewayAccountBalances::default();
            balances
                .account_uuid
                .clone_from(&account.public_account_uuid);
            balances.total = public_account_usd_total_label_for_chain(
                snapshot,
                self.selected_chain,
                &account.public_account_uuid,
                PublicAccountStatus::Active,
                Some(&self.public_broadcaster_anchor_cache),
            );
            if let Some(observation) = snapshot.and_then(|snapshot| {
                snapshot
                    .accounts
                    .iter()
                    .find(|value| value.account == *account)
            }) {
                balances.assets = observation
                    .balances
                    .iter()
                    .filter(|entry| !entry.amount.is_zero())
                    .map(|entry| {
                        let mut asset = GatewayAssetBalance::default();
                        asset.asset = match entry.asset.id {
                            PublicAssetId::Native => "native".to_owned(),
                            PublicAssetId::Erc20(address) => address.to_string(),
                        };
                        asset.symbol.clone_from(&entry.asset.symbol);
                        asset.amount =
                            public_balance_amount_label(&entry.amount, entry.asset.decimals);
                        asset.max_amount = match entry.asset.id {
                            PublicAssetId::Erc20(_) => entry.amount.amount().map(|amount| {
                                super::format_send_amount_input(amount, Some(entry.asset.decimals))
                            }),
                            PublicAssetId::Native => None,
                        };
                        asset.usd = public_balance_usd_label(
                            self.selected_chain,
                            entry.asset.id,
                            &entry.amount,
                            Some(&self.public_broadcaster_anchor_cache),
                        );
                        asset.icon = match public_asset_icon_path(
                            self.selected_chain,
                            entry.asset.id,
                            Some(&self.effective_token_registry),
                        ) {
                            Some(crate::assets::WalletIconSource::Embedded(path))
                                if path.starts_with("railgun-ui/") =>
                            {
                                Some(path)
                            }
                            _ => None,
                        };
                        asset
                    })
                    .collect();
            }
            presentation.balances.push(balances);
        }
        presentation
    }

    pub(super) fn apply_gateway_public_command(
        &mut self,
        peer_id: &str,
        command: GatewayPublicCommand,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(view) = self.view_session.as_ref() else {
            return;
        };
        match command {
            GatewayPublicCommand::Draft { command } => {
                self.apply_gateway_draft_command(peer_id, *command, window, cx);
                return;
            }
            GatewayPublicCommand::SelectAccount {
                public_account_uuid,
            } => {
                if self.public_accounts.iter().any(|account| {
                    account.public_account_uuid == public_account_uuid
                        && (account.is_active_for_wallet(view.wallet_id())
                            && account.is_available_on_chain(self.selected_chain))
                }) {
                    self.set_public_selected_balance(
                        public_account_uuid.into(),
                        PublicAssetId::Native,
                        window,
                        cx,
                    );
                }
            }
            GatewayPublicCommand::SelectChain { chain_id } => {
                if self.effective_chain_configs.contains_key(&chain_id) {
                    self.select_chain(chain_id, window, cx);
                }
            }
            GatewayPublicCommand::RefreshBalances => {
                // Coalesce explicit refreshes in the existing desktop balance owner.
                if !self.public_balance_refreshing {
                    self.schedule_public_balance_refresh(cx);
                }
            }
            _ => return,
        }
        self.publish_gateway_desktop_state();
    }
}
