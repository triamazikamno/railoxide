//! Private presentation is projected from the desktop's current wallet and chain owners.
use gpui::{Context, Window};
use gpui_component::WindowExt as _;
use wallet_ops::{
    gateway::{
        GatewayPrivateAsset, GatewayPrivateChainState, GatewayPrivateCommand, GatewayPrivateView,
        GatewayPrivateWallet,
    },
    hardware::HardwareDeviceKind,
    vault::WalletMetadataBundle,
};

use super::{
    ChainUtxoState, VaultState, WalletRoot,
    chain_load::{SyncStatusContext, loading_summary, sync_status_labels},
    private_assets::{FormattedTokenTotal, should_show_pending_amount},
    vault::{
        hardware_device_kind_from_wallet_select_value, visible_wallet_metadata,
        wallet_select_items_from_metadata, wallet_select_value_for_selected_wallet,
    },
};

pub(super) fn private_wallet_choices(
    metadata: &[WalletMetadataBundle],
    revealed_wallet: Option<&str>,
) -> Vec<GatewayPrivateWallet> {
    wallet_select_items_from_metadata(&visible_wallet_metadata(metadata, revealed_wallet))
        .into_iter()
        .map(|item| {
            let mut choice = GatewayPrivateWallet::default();
            choice.hardware = hardware_device_kind_from_wallet_select_value(&item.wallet_id).map(
                |kind| match kind {
                    HardwareDeviceKind::Ledger => "ledger".into(),
                    HardwareDeviceKind::Trezor => "trezor".into(),
                },
            );
            choice.wallet_id = item.wallet_id.to_string();
            choice.label = item.label.to_string();
            choice
        })
        .collect()
}

pub(super) fn private_asset_presentation(asset: &FormattedTokenTotal) -> GatewayPrivateAsset {
    let mut row = GatewayPrivateAsset::default();
    row.asset = asset
        .token
        .map_or_else(|| asset.label.clone(), |token| token.to_string());
    row.symbol.clone_from(&asset.label);
    row.amount.clone_from(&asset.amount);
    row.usd.clone_from(&asset.usd_amount);
    row.icon = match &asset.icon_path {
        Some(crate::assets::WalletIconSource::Embedded(path))
            if path.starts_with("railgun-ui/") =>
        {
            Some(path.clone())
        }
        _ => None,
    };
    row.pending_verification = should_show_pending_amount(asset.pending_poi_total)
        .then(|| asset.pending_poi_amount.clone());
    row.pending_incoming = should_show_pending_amount(asset.pending_incoming_total)
        .then(|| asset.pending_incoming_amount.clone());
    row.pending_outgoing = should_show_pending_amount(asset.pending_outgoing_total)
        .then(|| asset.pending_outgoing_amount.clone());
    row
}

impl WalletRoot {
    pub(super) fn gateway_private_view(&self) -> GatewayPrivateView {
        let mut presentation = GatewayPrivateView::default();
        presentation.wallets = private_wallet_choices(
            &self.wallet_metadata,
            self.revealed_passphrase_context_id.as_deref(),
        );
        if let Some(wallet_id) = self.selected_wallet_id.as_ref()
            && self
                .view_session
                .as_ref()
                .is_some_and(|view| view.wallet_id() == wallet_id.as_ref())
        {
            let choice = wallet_select_value_for_selected_wallet(
                wallet_id,
                &visible_wallet_metadata(
                    &self.wallet_metadata,
                    self.revealed_passphrase_context_id.as_deref(),
                ),
            );
            if presentation
                .wallets
                .iter()
                .any(|wallet| wallet.wallet_id == choice.as_ref())
            {
                presentation.selected_wallet = Some(wallet_id.to_string());
                presentation.selected_wallet_choice = Some(choice.to_string());
                presentation.receive_address = self
                    .view_session
                    .as_ref()
                    .and_then(|view| view.receive_address().ok());
            }
        }
        presentation.selected_chain = Some(self.selected_chain);
        if self.unwrap_unshields_by_default {
            presentation.default_unwrap_asset = self
                .effective_chain_configs
                .get(self.selected_chain)
                .and_then(|chain| chain.wrapped_native_token)
                .map(|token| token.to_string());
        }
        if !self.selected_chain_has_railgun() {
            presentation.receive_address = None;
            presentation.selection_message = Some(
                "Private balances and Shield are unavailable on public-only chains".to_owned(),
            );
            return presentation;
        }
        presentation.selection_message = self.gateway.private_selection_message.map(str::to_owned);
        if self.wallet_switch_loading_generation == Some(self.wallet_switch_generation) {
            presentation.selection_message = Some("Switching wallet…".into());
        }
        #[cfg(feature = "hardware")]
        if self.gateway.private_hardware_selection_pending
            && self.hardware_profile_unlock.error.is_some()
        {
            presentation.selection_message =
                Some("Could not open the hardware wallet. Continue in the desktop app.".into());
        }
        if presentation.selected_wallet.is_none() {
            presentation.message = Some("Choose a wallet to continue".into());
            return presentation;
        }
        let state = self.chain_states.get(&self.selected_chain);
        presentation.forms_available =
            state.is_some_and(ChainUtxoState::private_action_forms_available);
        presentation.generation_ready =
            state.is_some_and(ChainUtxoState::private_action_generation_ready);
        let sync_labels = match state {
            Some(ChainUtxoState::Loading { progress }) => {
                presentation.state = GatewayPrivateChainState::Loading;
                presentation.message = Some(loading_summary(*progress));
                Some(sync_status_labels(SyncStatusContext::Loading, *progress))
            }
            Some(ChainUtxoState::Syncing { progress, .. }) => {
                presentation.state = GatewayPrivateChainState::Syncing;
                presentation.message = Some(loading_summary(*progress));
                Some(sync_status_labels(SyncStatusContext::Syncing, *progress))
            }
            Some(ChainUtxoState::Ready { .. }) => {
                presentation.state = GatewayPrivateChainState::Ready;
                None
            }
            Some(ChainUtxoState::Error { .. }) => {
                presentation.state = GatewayPrivateChainState::Error;
                // Native diagnostics can include local paths or endpoints.
                presentation.message = Some(
                    "Could not load private balances. Open the desktop app for details.".into(),
                );
                None
            }
            Some(ChainUtxoState::Idle) | None => {
                presentation.message = Some("Select a chain to load private balances".into());
                None
            }
        };
        if state
            .and_then(ChainUtxoState::snapshot)
            .is_some_and(|snapshot| snapshot.chain_id != self.selected_chain)
        {
            presentation.state = GatewayPrivateChainState::Loading;
            presentation.message = Some("Waiting for current network balances.".into());
            presentation.forms_available = false;
            presentation.generation_ready = false;
            return presentation;
        }
        if let Some(labels) = sync_labels {
            presentation.stage_label = Some(labels.title);
            presentation.percent = Some(labels.percent);
        }
        if let Some(snapshot) = state
            .and_then(ChainUtxoState::snapshot)
            .filter(|snapshot| snapshot.chain_id == self.selected_chain)
        {
            let assets = self.private_asset_presentation(snapshot);
            presentation.total.clone_from(&assets.total);
            presentation.assets = assets.rows.iter().map(private_asset_presentation).collect();
            presentation.pending = self.gateway_private_pending(snapshot, &assets.rows);
        }
        presentation
    }

    pub(super) fn apply_gateway_private_command(
        &mut self,
        command: GatewayPrivateCommand,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let GatewayPrivateCommand::SelectWallet { wallet_id } = command;
        let eligible = private_wallet_choices(
            &self.wallet_metadata,
            self.revealed_passphrase_context_id.as_deref(),
        )
        .iter()
        .any(|wallet| wallet.wallet_id == wallet_id);
        if !eligible
            || !matches!(self.vault_state, VaultState::ViewUnlocked)
            || self.view_session.is_none()
            || window.has_active_dialog(cx)
            || self.wallet_switch_loading_generation == Some(self.wallet_switch_generation)
            || self.public_sync_cache_resetting
            || self.gateway.wallet_switch_in_progress()
            || !self.destructive_cache_reset_is_allowed()
        {
            self.gateway.private_selection_message =
                Some("Wallet selection is busy or unavailable. Open the desktop app to continue.");
            self.publish_gateway_desktop_state();
            return;
        }
        let hardware = hardware_device_kind_from_wallet_select_value(&wallet_id).is_some();
        if hardware && !cfg!(feature = "hardware") {
            self.gateway.private_selection_message =
                Some("Hardware wallet support is unavailable in this desktop build.");
            self.publish_gateway_desktop_state();
            return;
        }
        self.gateway.private_selection_message = if hardware {
            Some("Continue wallet selection in the desktop app.")
        } else {
            None
        };
        if self.selected_wallet_id.as_deref() == Some(wallet_id.as_str()) {
            self.gateway.private_selection_message = None;
        }
        self.gateway.private_hardware_selection_pending = hardware;
        self.select_wallet(&wallet_id, window, cx);
        self.publish_gateway_desktop_state();
        cx.notify();
    }
}
