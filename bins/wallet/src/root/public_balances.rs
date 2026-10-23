use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::U256;
use gpui::Context;
use railgun_ui::{
    format_scaled_amount, format_token_amount, format_usd_micro_value,
    native_currency_icon_asset_path, short_address,
};
use rand::RngExt as _;
use wallet_ops::{
    PublicAccountBalance, PublicAssetId, PublicBalanceAmount, PublicBalanceEntry,
    PublicBalanceRefreshTicket, PublicBalanceSnapshot, TokenAnchorRateCache,
    refresh_public_balances_at_least, settings::EffectiveTokenRegistry, vault::PublicAccountStatus,
};

use super::{WalletRoot, format_report_chain, token_display_metadata};

use crate::assets::WalletIconSource;

pub(super) fn public_asset_label(
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    asset: PublicAssetId,
    registry: Option<&EffectiveTokenRegistry>,
) -> String {
    let Some(chain) = chain else {
        return "Unavailable".to_owned();
    };
    match asset {
        PublicAssetId::Native => chain.native_currency.symbol.clone(),
        PublicAssetId::Erc20(token) => token_display_metadata(registry, chain.chain_id, &token)
            .map_or_else(|| short_address(&token), |info| info.symbol),
    }
}

pub(super) fn public_asset_decimals(
    chain: Option<&wallet_ops::settings::EffectiveChainConfig>,
    asset: PublicAssetId,
    registry: Option<&EffectiveTokenRegistry>,
) -> Option<u8> {
    let chain = chain?;
    match asset {
        PublicAssetId::Native => Some(chain.native_currency.decimals),
        PublicAssetId::Erc20(token) => {
            token_display_metadata(registry, chain.chain_id, &token).map(|info| info.decimals)
        }
    }
}

pub(super) fn public_asset_icon_path(
    chain_id: u64,
    asset: PublicAssetId,
    registry: Option<&EffectiveTokenRegistry>,
) -> Option<WalletIconSource> {
    match asset {
        PublicAssetId::Native => {
            native_currency_icon_asset_path(chain_id).map(WalletIconSource::embedded)
        }
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
    public_balance_usd_value(chain_id, asset, amount, anchor_cache).map(format_usd_micro_value)
}

pub(super) fn public_balance_usd_value(
    chain_id: u64,
    asset: PublicAssetId,
    amount: &PublicBalanceAmount,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> Option<U256> {
    let PublicBalanceAmount::Available(amount) = amount else {
        return None;
    };
    let cache = anchor_cache?;
    match asset {
        PublicAssetId::Native => cache.cached_native_usd_micro_value(chain_id, *amount),
        PublicAssetId::Erc20(token) => cache.cached_token_usd_micro_value(chain_id, token, *amount),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct PublicUsdTotal {
    pub(super) value: Option<U256>,
    pub(super) partial: bool,
}

pub(super) fn public_balances_usd_total(
    balances: &[PublicBalanceEntry],
    chain_id: u64,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> PublicUsdTotal {
    let mut total = PublicUsdTotal::default();
    for entry in balances {
        if !matches!(entry.amount, PublicBalanceAmount::Available(amount) if !amount.is_zero()) {
            continue;
        }
        if let Some(value) =
            public_balance_usd_value(chain_id, entry.asset.id, &entry.amount, anchor_cache)
        {
            total.value = Some(total.value.unwrap_or_default().saturating_add(value));
        } else {
            total.partial = true;
        }
    }
    total
}

pub(super) fn public_active_usd_total(
    snapshot: Option<&PublicBalanceSnapshot>,
    accounts: &[wallet_ops::vault::PublicAccountMetadata],
    chain_id: u64,
    anchor_cache: Option<&TokenAnchorRateCache>,
) -> PublicUsdTotal {
    let mut total = PublicUsdTotal::default();
    let Some(snapshot) = snapshot.filter(|snapshot| snapshot.chain_id == chain_id) else {
        return total;
    };
    for account in accounts.iter().filter(|account| {
        account.status == PublicAccountStatus::Active && account.is_available_on_chain(chain_id)
    }) {
        let balances = public_account_visible_balances_for_chain(
            Some(snapshot),
            chain_id,
            &account.public_account_uuid,
            account.status,
        );
        let account_total = public_balances_usd_total(&balances, chain_id, anchor_cache);
        if let Some(value) = account_total.value {
            total.value = Some(total.value.unwrap_or_default().saturating_add(value));
        }
        total.partial |= account_total.partial;
    }
    total
}

/// Whole-dollar USD label for the compact tile line, such as `$494` or `<$1`.
pub(super) fn public_balance_compact_usd(value: U256) -> String {
    const USD_MICRO_PER_DOLLAR: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);
    let dollars = (value + USD_MICRO_PER_DOLLAR / U256::from(2)) / USD_MICRO_PER_DOLLAR;
    if dollars.is_zero() {
        return "<$1".to_owned();
    }
    let mut label = format_usd_micro_value(dollars * USD_MICRO_PER_DOLLAR);
    if label.ends_with(".00") {
        label.truncate(label.len() - 3);
    }
    label
}

/// Compact list presentation only; transaction inputs retain full precision.
pub(super) fn public_balance_tile_amount(amount: &PublicBalanceAmount, decimals: u8) -> String {
    let PublicBalanceAmount::Available(amount) = amount else {
        return "unavailable".to_owned();
    };
    let full = format_scaled_amount(*amount, decimals);
    let (whole, fraction) = full.split_once('.').unwrap_or((&full, ""));
    if !amount.is_zero() && whole == "0" && fraction.starts_with("0000") {
        return "<0.0001".to_owned();
    }
    // Keep up to four decimal places, reducing precision before abbreviating large integers.
    if whole.len() > 8 {
        return format!("{}.{}e{}", &whole[..1], &whole[1..4], whole.len() - 1);
    }
    let precision = 4
        .min(8_usize.saturating_sub(whole.len() + 1))
        .min(fraction.len());
    let fraction = fraction[..precision].trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    }
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

/// Every balance the snapshot read for the account on `chain_id`, zero and unreadable ones
/// included. `None` while the snapshot doesn't hold the account there.
pub(super) fn public_account_balances_for_chain<'a>(
    snapshot: Option<&'a PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
    status: PublicAccountStatus,
) -> Option<&'a [PublicBalanceEntry]> {
    public_account_row_for_chain(snapshot, chain_id, public_account_uuid, status)
        .map(|account| account.balances.as_slice())
}

fn public_account_row_for_chain<'a>(
    snapshot: Option<&'a PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
    status: PublicAccountStatus,
) -> Option<&'a PublicAccountBalance> {
    snapshot
        .filter(|snapshot| snapshot.chain_id == chain_id)?
        .accounts
        .iter()
        .find(|account| {
            account.account.public_account_uuid.as_str() == public_account_uuid
                && account.account.status == status
        })
}

/// The account's row keeps balances from an earlier read, but its latest refresh failed or
/// was partial, which clears the observation.
pub(super) fn public_account_stale_for_chain(
    snapshot: Option<&PublicBalanceSnapshot>,
    chain_id: u64,
    public_account_uuid: &str,
    status: PublicAccountStatus,
) -> bool {
    public_account_row_for_chain(snapshot, chain_id, public_account_uuid, status)
        .is_some_and(public_row_stale)
}

const fn public_row_stale(row: &PublicAccountBalance) -> bool {
    !row.balances.is_empty() && row.observed_at.is_none()
}

#[derive(Default)]
pub(super) struct PublicSectionFreshness {
    /// Accounts with a selected-chain row, so counted in the section's total.
    pub(super) counted: usize,
    /// Accounts with no selected-chain row.
    pub(super) not_refreshed: usize,
    /// Some counted row is stale.
    pub(super) stale: bool,
    /// Some account with no selected-chain row failed its latest refresh.
    pub(super) failed: bool,
}

pub(super) fn public_section_freshness(
    snapshot: Option<&PublicBalanceSnapshot>,
    accounts: &[wallet_ops::vault::PublicAccountMetadata],
    chain_id: u64,
    status: PublicAccountStatus,
    failed: &BTreeSet<String>,
) -> PublicSectionFreshness {
    let mut freshness = PublicSectionFreshness::default();
    for account in accounts
        .iter()
        .filter(|account| account.status == status && account.is_available_on_chain(chain_id))
    {
        if let Some(row) =
            public_account_row_for_chain(snapshot, chain_id, &account.public_account_uuid, status)
        {
            freshness.counted += 1;
            freshness.stale |= public_row_stale(row);
        } else {
            freshness.not_refreshed += 1;
            freshness.failed |= failed.contains(&account.public_account_uuid);
        }
    }
    freshness
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
        self.public_inactive_balance_error = None;
        self.public_form.selected_asset = None;
        self.reset_public_asset_focus();
        self.clear_public_action_progress_state();
        self.public_form.send_error = None;
        self.public_form.shield_error = None;
    }

    /// Shows the balances already read this session for the selected chain, without RPC.
    pub(super) fn restore_public_balance_snapshot(&mut self) {
        self.public_balance_snapshot = self
            .current_public_balance_scope(self.selected_chain)
            .and_then(|scope| self.public_balance_cache.snapshot(&scope))
            .map(Arc::new);
    }

    /// Selected-chain accounts of `status` that a global refresh reads.
    pub(super) fn public_global_refresh_ids(&self, status: PublicAccountStatus) -> Vec<String> {
        self.public_accounts
            .iter()
            .filter(|account| {
                account.status == status
                    && account.is_available_on_chain(self.selected_chain)
                    && !self
                        .ui_state
                        .public_refresh_excluded
                        .contains(&account.public_account_uuid)
            })
            .map(|account| account.public_account_uuid.clone())
            .collect()
    }

    /// Selected-chain account ids in the in-flight refresh or queued behind it.
    pub(super) fn public_refreshing_accounts(&self) -> BTreeSet<String> {
        self.current_public_balance_scope(self.selected_chain)
            .map(|scope| self.public_balance_cache.refreshing_accounts(&scope))
            .unwrap_or_default()
    }

    /// Selected-chain account ids whose latest refresh failed without returning their row.
    pub(super) fn public_failed_accounts(&self) -> BTreeSet<String> {
        self.current_public_balance_scope(self.selected_chain)
            .map(|scope| self.public_balance_cache.failed_accounts(&scope))
            .unwrap_or_default()
    }

    pub(super) fn public_section_refreshing(&self, status: PublicAccountStatus) -> bool {
        let pending = self.public_refreshing_accounts();
        self.public_accounts.iter().any(|account| {
            account.status == status && pending.contains(&account.public_account_uuid)
        })
    }

    pub(super) fn set_public_refresh_excluded(
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

    /// Timer, view, chain, settings and reload triggers: only with automatic refresh on.
    pub(super) fn schedule_automatic_public_balance_refresh(&self, cx: &mut Context<'_, Self>) {
        if self.ui_state.public_auto_refresh {
            self.schedule_global_public_balance_refresh(PublicAccountStatus::Active, cx);
        }
    }

    /// The section's included accounts on the selected chain, whatever the toggle says.
    pub(super) fn schedule_global_public_balance_refresh(
        &self,
        status: PublicAccountStatus,
        cx: &mut Context<'_, Self>,
    ) {
        let accounts = self.public_global_refresh_ids(status);
        self.schedule_account_public_balance_refresh(self.selected_chain, accounts, None, cx);
    }

    /// Exactly the named accounts, ignoring the toggle and exclusions.
    pub(super) fn schedule_account_public_balance_refresh<I, S>(
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

    pub(super) fn run_public_balance_refresh(
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
            let mut completion = Some((ticket, result));
            let (sync_window, focus_window) = this
                .update(cx, |root, cx| {
                    let (ticket, result) = completion.take().expect("refresh completion available");
                    let (sync_selects, restore_focus) =
                        root.apply_public_balance_refresh_result(ticket, result, window, cx);
                    let target = window.or_else(|| cx.windows().first().copied());
                    (
                        target.filter(|_| sync_selects),
                        target.filter(|_| restore_focus),
                    )
                })
                .unwrap_or_default();
            if let Some((ticket, _)) = completion {
                let _ = cache.finish_refresh(ticket, None);
                // The owner has gone away; discard any admitted follow-up as well.
                cache.clear();
            }
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
    fn run_public_balance_follow_up(
        &self,
        ticket: PublicBalanceRefreshTicket,
        window: Option<gpui::AnyWindowHandle>,
        cx: &Context<'_, Self>,
    ) {
        let delay = Duration::from_millis(rand::rng().random_range(500..=3_000));
        let cache = self.public_balance_cache.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let mut ticket = Some(ticket);
            let _ = this.update(cx, |root, cx| {
                let ticket = ticket.take().expect("follow-up ticket available");
                root.run_public_balance_refresh(ticket, window, cx);
            });
            if let Some(ticket) = ticket {
                let _ = cache.finish_refresh(ticket, None);
                // The owner has gone away; discard any admitted follow-up as well.
                cache.clear();
            }
        })
        .detach();
    }

    pub(super) fn apply_public_balance_refresh_result(
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
        let selected = scope.chain_id() == self.selected_chain;
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
