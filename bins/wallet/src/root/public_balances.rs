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
    PublicAccountBalance, PublicAssetId, PublicBalanceAmount, PublicBalanceCache,
    PublicBalanceEntry, PublicBalanceRefreshTicket, PublicBalanceSnapshot, TokenAnchorRateCache,
    refresh_public_balances_at_least, settings::EffectiveTokenRegistry, vault::PublicAccountStatus,
};

use super::{WalletRoot, format_report_chain, token_display_metadata};

use crate::assets::WalletIconSource;

mod balance_state;
mod refresh_results;
mod refresh_runs;

/// A random 0.5-3 s wait before the next account's isolated reads, so successive accounts'
/// reads aren't timed together.
pub(super) fn isolated_read_jitter() -> Duration {
    Duration::from_millis(rand::rng().random_range(500..=3_000))
}

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
