use alloy::primitives::{Address, U256};
use railgun_ui::{
    format_scaled_amount, format_token_amount, format_usd_micro_value, lookup_token,
    non_redundant_usd_micro_value, short_address, token_icon_asset_path,
};
use wallet_ops::settings::{EffectiveChainRegistry, EffectiveTokenInfo, EffectiveTokenRegistry};

use crate::assets::WalletIconSource;

#[derive(Clone)]
pub(super) struct TokenDisplayMetadata {
    pub(super) symbol: String,
    pub(super) decimals: u8,
    pub(super) icon_path: Option<WalletIconSource>,
}

pub(super) fn token_display_metadata(
    registry: Option<&EffectiveTokenRegistry>,
    chain_id: u64,
    token: &Address,
) -> Option<TokenDisplayMetadata> {
    if let Some(registry) = registry {
        return registry
            .get(chain_id, token)
            .map(|info| token_display_metadata_from_effective(info, token));
    }

    lookup_token(chain_id, token).map(|info| TokenDisplayMetadata {
        symbol: info.symbol.to_owned(),
        decimals: info.decimals,
        icon_path: token_icon_asset_path(chain_id, token).map(WalletIconSource::embedded),
    })
}

fn token_display_metadata_from_effective(
    info: &EffectiveTokenInfo,
    token: &Address,
) -> TokenDisplayMetadata {
    let icon_path = info
        .icon_path
        .as_ref()
        .map(WalletIconSource::file)
        .or_else(|| {
            info.built_in
                .then(|| {
                    token_icon_asset_path(info.chain_id, token).map(WalletIconSource::embedded)
                })
                .flatten()
        });
    TokenDisplayMetadata {
        symbol: info.symbol.clone(),
        decimals: info.decimals,
        icon_path,
    }
}

pub(super) fn token_display_label(
    chain_id: u64,
    token: Address,
    registry: Option<&EffectiveTokenRegistry>,
) -> String {
    token_display_metadata(registry, chain_id, &token)
        .map_or_else(|| short_address(&token), |info| info.symbol)
}

pub(super) fn format_exact_token_amount_for_display(
    chain_id: u64,
    token: Address,
    amount: U256,
    registry: Option<&EffectiveTokenRegistry>,
) -> String {
    token_display_metadata(registry, chain_id, &token).map_or_else(
        || format!("{} raw token units ({})", amount, short_address(&token)),
        |info| {
            format!(
                "{} {}",
                format_send_amount_input(amount, Some(info.decimals)),
                info.symbol
            )
        },
    )
}

pub(super) fn format_token_amount_for_display(
    chain_id: u64,
    token: Address,
    amount: U256,
    registry: Option<&EffectiveTokenRegistry>,
) -> String {
    token_display_metadata(registry, chain_id, &token).map_or_else(
        || format!("{} raw token units ({})", amount, short_address(&token)),
        |info| {
            format!(
                "{} {}",
                format_token_amount(amount, info.decimals),
                info.symbol
            )
        },
    )
}

pub(super) fn format_token_amount_ceiling_for_display(
    chain_id: u64,
    token: Address,
    amount: U256,
    registry: Option<&EffectiveTokenRegistry>,
) -> String {
    token_display_metadata(registry, chain_id, &token).map_or_else(
        || format!("{} raw token units ({})", amount, short_address(&token)),
        |info| {
            format!(
                "{} {}",
                railgun_ui::format_token_amount_ceiling(amount, info.decimals),
                info.symbol
            )
        },
    )
}

pub(super) fn format_value_with_usd_label(
    token_value: String,
    amount: U256,
    decimals: Option<u8>,
    usd_micro_value: Option<U256>,
    negative: bool,
) -> String {
    let Some(usd_micro_value) = usd_micro_value else {
        return format!("{token_value} · USD unavailable");
    };
    if decimals.is_some_and(|decimals| {
        non_redundant_usd_micro_value(amount, decimals, usd_micro_value).is_none()
    }) {
        return token_value;
    }
    let usd_value = format_usd_micro_value(usd_micro_value);
    format!(
        "{token_value} · {}",
        if negative {
            format!("-{usd_value}")
        } else {
            usd_value
        }
    )
}

pub(super) const fn native_token_display_label(chain_id: u64) -> &'static str {
    match native_wrapped_output_labels(chain_id) {
        Some((native_label, _wrapped_label)) => native_label,
        None => "base token",
    }
}

pub(super) fn format_native_token_amount_for_display(chain_id: u64, amount: U256) -> String {
    format!(
        "{} {}",
        format_token_amount(amount, 18),
        native_token_display_label(chain_id)
    )
}

pub(super) fn format_native_token_amount_ceiling_for_display(
    chain_id: u64,
    amount: U256,
) -> String {
    format!(
        "{} {}",
        railgun_ui::format_token_amount_ceiling(amount, 18),
        native_token_display_label(chain_id)
    )
}

pub(super) fn format_native_top_up_recipient_suffix(chain_id: u64, amount: U256) -> String {
    format!(
        "+ {} (gas top-up)",
        format_native_token_amount_for_display(chain_id, amount)
    )
}

pub(super) fn format_recipient_amount_with_native_top_up(
    recipient_amount: &str,
    chain_id: u64,
    native_amount: U256,
) -> String {
    format!(
        "{} {}",
        recipient_amount,
        format_native_top_up_recipient_suffix(chain_id, native_amount)
    )
}

pub(super) fn format_unshield_amount_input(amount: U256, decimals: Option<u8>) -> String {
    decimals.map_or_else(
        || amount.to_string(),
        |decimals| format_scaled_amount(amount, decimals),
    )
}

pub(super) fn format_send_amount_input(amount: U256, decimals: Option<u8>) -> String {
    format_unshield_amount_input(amount, decimals)
}

pub(super) const fn native_wrapped_output_labels(
    chain_id: u64,
) -> Option<(&'static str, &'static str)> {
    match chain_id {
        1 | 42161 => Some(("ETH", "WETH")),
        56 => Some(("BNB", "WBNB")),
        137 => Some(("MATIC", "WMATIC")),
        _ => None,
    }
}

pub(super) fn parse_address(raw: &str) -> Option<Address> {
    raw.parse().ok()
}

pub(super) fn is_effective_wrapped_native_token(
    effective_chain_configs: &EffectiveChainRegistry,
    chain_id: u64,
    token: Address,
) -> bool {
    effective_chain_configs
        .get(chain_id)
        .and_then(|chain| chain.wrapped_native_token)
        .is_some_and(|wrapped| wrapped == token)
}
