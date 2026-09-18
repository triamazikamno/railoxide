use gpui::{
    App, Context, IntoElement as _, ParentElement as _, SharedString, Styled as _, Window, div,
};
use gpui_component::select::{SearchableVec, SelectItem};
use wallet_ops::{ExecutorAsset, PublicAssetId};

use super::super::StealthAccountsView;
use crate::assets::WalletIconSource;
use crate::root::public_balances::public_asset_icon_path;

#[derive(Clone, PartialEq, Eq)]
pub(in crate::root::stealth_accounts) struct RecoveryAssetItem {
    asset: ExecutorAsset,
    label: String,
    balance: String,
    icon: Option<WalletIconSource>,
}

impl RecoveryAssetItem {
    fn row(&self) -> gpui::Div {
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(ui::private_action::asset_row(
                self.label.clone(),
                self.icon.clone().map(Into::into),
            ))
            .child(div().flex_none().child(self.balance.clone()))
    }
}

impl SelectItem for RecoveryAssetItem {
    type Value = ExecutorAsset;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }
    fn value(&self) -> &Self::Value {
        &self.asset
    }
    fn display_title(&self) -> Option<gpui::AnyElement> {
        Some(self.row().into_any_element())
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl gpui::IntoElement {
        self.row()
    }
    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_ascii_lowercase();
        self.label.to_ascii_lowercase().contains(&query)
            || super::super::asset_label(self.asset)
                .to_ascii_lowercase()
                .contains(&query)
    }
}

impl StealthAccountsView {
    pub(in crate::root::stealth_accounts) fn sync_recovery_assets(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(record) = self
            .records
            .iter()
            .find(|record| Some(record.operation()) == self.selected)
        else {
            return;
        };
        let root = self.root.upgrade();
        let registry = root
            .as_ref()
            .map(|root| &root.read(cx).effective_token_registry);
        let items = self
            .assets_for(record)
            .into_iter()
            .map(|asset| {
                let public_asset = match asset {
                    ExecutorAsset::Native => PublicAssetId::Native,
                    ExecutorAsset::Erc20(token) => PublicAssetId::Erc20(token),
                    ExecutorAsset::Erc721 { collection, .. } => PublicAssetId::Erc20(collection),
                };
                RecoveryAssetItem {
                    asset,
                    label: self.asset_name(asset, cx),
                    balance: self.balance_value_lines(record.operation(), asset, cx).0,
                    icon: public_asset_icon_path(self.session.chain_id, public_asset, registry),
                }
            })
            .collect::<Vec<_>>();
        if self.recovery.asset_items == items {
            return;
        }
        self.recovery.asset_items.clone_from(&items);
        self.recovery.asset_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(items), window, cx);
            if let Some(asset) = self.recovery.asset {
                select.set_selected_value(&asset, window, cx);
            }
            cx.notify();
        });
        cx.notify();
    }

    pub(in crate::root::stealth_accounts) fn recovery_has_native_balance(&self) -> bool {
        self.selected
            .and_then(|operation| self.observations.get(&operation))
            .and_then(|observations| observations.assets.get(&ExecutorAsset::Native))
            .and_then(|balance| balance.value)
            .is_some_and(|balance| !balance.amount.is_zero())
    }
}
