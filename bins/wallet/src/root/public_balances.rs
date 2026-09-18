use std::sync::Arc;

use alloy::primitives::U256;
use gpui::{Context, Window};
use railgun_ui::{
    chain_icon_asset_path, chain_name, format_token_amount, format_usd_micro_value, short_address,
};
use wallet_ops::{
    PublicAssetId, PublicBalanceAmount, PublicBalanceEntry, PublicBalanceRefreshTicket,
    PublicBalanceSnapshot, TokenAnchorRateCache, refresh_public_balances_at_least,
    settings::EffectiveTokenRegistry, vault::PublicAccountStatus,
};

use super::{WalletRoot, format_report_chain, token_display_metadata};

use crate::assets::WalletIconSource;

pub(super) fn public_asset_label(
    chain_id: u64,
    asset: PublicAssetId,
    registry: Option<&EffectiveTokenRegistry>,
) -> String {
    match asset {
        PublicAssetId::Native => chain_name(chain_id).map_or_else(
            || "Native".to_string(),
            |name| match chain_id {
                56 => "BNB".to_string(),
                137 => "MATIC".to_string(),
                _ => format!("{name} native"),
            },
        ),
        PublicAssetId::Erc20(token) => token_display_metadata(registry, chain_id, &token)
            .map_or_else(|| short_address(&token), |info| info.symbol),
    }
}

pub(super) fn public_asset_decimals(
    chain_id: u64,
    asset: PublicAssetId,
    registry: Option<&EffectiveTokenRegistry>,
) -> Option<u8> {
    match asset {
        PublicAssetId::Native => Some(18),
        PublicAssetId::Erc20(token) => {
            token_display_metadata(registry, chain_id, &token).map(|info| info.decimals)
        }
    }
}

pub(super) fn public_asset_icon_path(
    chain_id: u64,
    asset: PublicAssetId,
    registry: Option<&EffectiveTokenRegistry>,
) -> Option<WalletIconSource> {
    match asset {
        PublicAssetId::Native => chain_icon_asset_path(chain_id).map(WalletIconSource::embedded),
        PublicAssetId::Erc20(token) => {
            token_display_metadata(registry, chain_id, &token).and_then(|info| info.icon_path)
        }
    }
}

pub(super) fn public_balance_amount_label(amount: &PublicBalanceAmount, decimals: u8) -> String {
    match amount {
        PublicBalanceAmount::Available(amount) => format_token_amount(*amount, decimals),
        PublicBalanceAmount::Unavailable => "unavailable".to_string(),
    }
}

pub(super) fn public_balance_usd_label(
    chain_id: u64,
    asset: PublicAssetId,
    amount: &PublicBalanceAmount,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> Option<String> {
    let PublicBalanceAmount::Available(amount) = amount else {
        return None;
    };
    let cache = anchor_cache?;
    let usd_micro_value = match asset {
        PublicAssetId::Native => cache.cached_native_usd_micro_value(chain_id, *amount),
        PublicAssetId::Erc20(token) => cache.cached_token_usd_micro_value(chain_id, token, *amount),
    }?;
    Some(format_usd_micro_value(usd_micro_value))
}

pub(super) fn public_account_usd_total_label_for_chain(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
    status: PublicAccountStatus,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> Option<String> {
    let cache = anchor_cache?;
    let snapshot = snapshot.filter(|snapshot| snapshot.chain_id == chain_id)?;
    let account = snapshot.accounts.iter().find(|account| {
        account.account.public_account_uuid.as_str() == public_account_uuid
            && account.account.status == status
    })?;

    let mut total = U256::ZERO;
    let mut has_priced_balance = false;
    for entry in &account.balances {
        let PublicBalanceAmount::Available(amount) = entry.amount else {
            continue;
        };
        let usd_micro_value = match entry.asset.id {
            PublicAssetId::Native => cache.cached_native_usd_micro_value(chain_id, amount),
            PublicAssetId::Erc20(token) => {
                cache.cached_token_usd_micro_value(chain_id, token, amount)
            }
        };
        if let Some(usd_micro_value) = usd_micro_value {
            total = total.saturating_add(usd_micro_value);
            has_priced_balance = true;
        }
    }

    has_priced_balance.then(|| format_usd_micro_value(total))
}

pub(super) fn public_balance_entry_for_chain(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
    asset: PublicAssetId,
    status: PublicAccountStatus,
) -> Option<PublicBalanceEntry> {
    let snapshot = snapshot.filter(|snapshot| snapshot.chain_id == chain_id)?;
    snapshot
        .accounts
        .iter()
        .find(|account| {
            account.account.public_account_uuid.as_str() == public_account_uuid
                && account.account.status == status
        })?
        .balances
        .iter()
        .find(|entry| entry.asset.id == asset)
        .cloned()
}

pub(super) fn public_account_visible_balances_for_chain(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
    status: PublicAccountStatus,
) -> Vec<PublicBalanceEntry> {
    let Some(snapshot) = snapshot.filter(|snapshot| snapshot.chain_id == chain_id) else {
        return Vec::new();
    };
    snapshot
        .accounts
        .iter()
        .find(|account| {
            account.account.public_account_uuid.as_str() == public_account_uuid
                && account.account.status == status
        })
        .map_or_else(Vec::new, |account| {
            account
                .balances
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.amount,
                        PublicBalanceAmount::Available(amount) if !amount.is_zero()
                    )
                })
                .cloned()
                .collect()
        })
}

impl WalletRoot {
    pub(super) fn selected_public_balance_entry(&self) -> Option<PublicBalanceEntry> {
        let public_account_uuid = self.public_form.selected_account_uuid.as_deref()?;
        let asset = self.public_form.selected_asset?;
        let status = self
            .public_account_for_uuid(Some(public_account_uuid))?
            .status;
        self.public_balance_entry(public_account_uuid, asset, status)
    }

    fn public_balance_entry(
        &self,
        public_account_uuid: &str,
        asset: PublicAssetId,
        status: PublicAccountStatus,
    ) -> Option<PublicBalanceEntry> {
        public_balance_entry_for_chain(
            self.public_balance_snapshot.as_deref(),
            self.selected_chain,
            public_account_uuid,
            asset,
            status,
        )
    }

    pub(super) fn clear_public_chain_balance_state(&mut self) {
        self.public_balance_snapshot = None;
        self.public_balance_error = None;
        self.public_balance_refreshing = false;
        self.public_inactive_balance_error = None;
        self.public_inactive_balance_refreshing = false;
        self.public_form.selected_asset = None;
        self.clear_public_action_progress_state();
        self.public_form.send_error = None;
        self.public_form.shield_error = None;
    }

    pub(super) fn schedule_public_balance_refresh(&mut self, cx: &mut Context<'_, Self>) {
        self.schedule_public_balance_refresh_for_chain(
            self.selected_chain,
            PublicAccountStatus::Active,
            None,
            cx,
        );
    }

    pub(super) fn schedule_self_broadcast_public_balance_refresh(
        &mut self,
        window: &Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.schedule_public_balance_refresh_for_chain(
            self.selected_chain,
            PublicAccountStatus::Active,
            Some(window.window_handle()),
            cx,
        );
    }

    pub(super) fn schedule_inactive_public_balance_refresh(&mut self, cx: &mut Context<'_, Self>) {
        self.schedule_public_balance_refresh_for_chain(
            self.selected_chain,
            PublicAccountStatus::Inactive,
            None,
            cx,
        );
    }

    pub(super) fn schedule_public_balance_refresh_for_chain(
        &mut self,
        chain_id: u64,
        status: PublicAccountStatus,
        window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<'_, Self>,
    ) {
        self.publish_gateway_desktop_state();
        let Some(scope) = self.current_public_balance_scope(chain_id) else {
            return;
        };
        if !self
            .public_accounts
            .iter()
            .any(|account| account.status == status)
        {
            return;
        }
        if chain_id == self.selected_chain {
            self.public_balance_snapshot = self.public_balance_cache.snapshot(&scope).map(Arc::new);
            match status {
                PublicAccountStatus::Active => {
                    self.public_balance_refreshing = true;
                    self.public_balance_error = None;
                }
                PublicAccountStatus::Inactive => {
                    self.public_inactive_balance_refreshing = true;
                    self.public_inactive_balance_error = None;
                }
            }
        }
        if let Some(ticket) = self.public_balance_cache.begin_refresh(&scope, status) {
            self.run_public_balance_refresh(ticket, window, cx);
        }
        self.publish_gateway_desktop_state();
        cx.notify();
    }

    pub(super) fn run_public_balance_refresh(
        &mut self,
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
        let accounts = self
            .public_accounts
            .iter()
            .filter(|account| {
                ticket.includes_status(account.status) && account.is_available_on_chain(chain_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        let http = self.http.clone();
        let effective_chain = self.effective_chain_configs.get(&chain_id).cloned();
        let effective_token_registry = self.effective_token_registry.clone();
        let minimum = ticket.minimum_block();
        if chain_id == self.selected_chain {
            if ticket.includes_status(PublicAccountStatus::Active) {
                self.public_balance_refreshing = true;
                self.public_balance_error = None;
            }
            if ticket.includes_status(PublicAccountStatus::Inactive) {
                self.public_inactive_balance_refreshing = true;
                self.public_inactive_balance_error = None;
            }
        }
        let join = self.runtime.spawn(async move {
            refresh_public_balances_at_least(
                chain_id,
                &accounts,
                effective_chain.as_ref(),
                Some(&effective_token_registry),
                &http,
                minimum,
            )
            .await
        });
        let cache = self.public_balance_cache.clone();
        cx.spawn(async move |this, cx| {
            let result = join.await;
            let mut completion = Some((ticket, result));
            let sync_window = this
                .update(cx, |root, cx| {
                    let (ticket, result) = completion.take().expect("refresh completion available");
                    let sync_selects =
                        root.apply_public_balance_refresh_result(ticket, result, window, cx);
                    sync_selects
                        .then(|| window.or_else(|| cx.windows().first().copied()))
                        .flatten()
                })
                .ok()
                .flatten();
            if let Some((ticket, _)) = completion {
                let _ = cache.finish_refresh(ticket, None);
                // The owner has gone away; discard any admitted follow-up as well.
                cache.clear();
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

    fn apply_public_balance_refresh_result(
        &mut self,
        ticket: PublicBalanceRefreshTicket,
        result: Result<Result<PublicBalanceSnapshot, eyre::Report>, tokio::task::JoinError>,
        window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let scope = ticket.scope().clone();
        let active = ticket.includes_status(PublicAccountStatus::Active);
        let inactive = ticket.includes_status(PublicAccountStatus::Inactive);
        let (snapshot, error, join_failed) = match result {
            Ok(Ok(snapshot)) => (Some(snapshot), None, false),
            Ok(Err(error)) => (None, Some(format_report_chain(&error)), false),
            Err(error) => (None, Some(error.to_string()), true),
        };
        let succeeded = snapshot.is_some();
        let completion = self.public_balance_cache.finish_refresh(ticket, snapshot);
        let selected = scope.chain_id() == self.selected_chain;
        if completion.accepted && selected {
            let previous = self.public_balance_snapshot.clone();
            self.public_balance_snapshot = self.public_balance_cache.snapshot(&scope).map(Arc::new);
            if active {
                self.public_balance_refreshing = false;
                self.public_balance_error = error.as_ref().map(|error| {
                    Arc::from(if join_failed {
                        format!("Public balance refresh failed: {error}")
                    } else {
                        error.clone()
                    })
                });
                if succeeded && let Some(snapshot) = self.public_balance_snapshot.clone() {
                    self.revalidate_sponsored_estimates_for_public_balance_change(
                        previous.as_deref(),
                        &snapshot,
                        cx,
                    );
                }
            }
            if inactive {
                self.public_inactive_balance_refreshing = false;
                self.public_inactive_balance_error = error.as_ref().map(|error| {
                    Arc::from(if join_failed {
                        format!("Inactive public balance refresh failed: {error}")
                    } else {
                        error.clone()
                    })
                });
            }
        }
        if let Some(follow_up) = completion.follow_up {
            self.run_public_balance_refresh(follow_up, window, cx);
        }
        self.publish_gateway_desktop_state();
        cx.notify();
        completion.accepted && selected && active && succeeded
    }
}
