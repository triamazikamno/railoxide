//! What a Bridge swap can deliver on its destination chain, and the token it buys on its own
//! chain to hand to the provider.
//!
//! On an EVM chain both lists are limited to the destination chain's configured tokens, plus the
//! native asset where NEAR Intents delivers it. A provider's route never adds a token there. A
//! swap paid from the private balance leaves out a destination that is the same asset as the
//! sell token: same-token bridging isn't supported there. A swap paid from a Public account
//! keeps it as a direct deposit. On a [`NonEvmNetwork`] every token 1Click lists is offered,
//! including the sell token's own asset.

use alloy::primitives::Address;

use super::{AcrossRoute, NonEvmNetwork, OneClickToken};
use crate::settings::{BridgeDestinationProfile, BridgeProfile, EffectiveTokenRegistry};

/// A token the provider can deliver on the destination chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeDestination {
    /// `Address::ZERO` for the destination chain's native asset.
    pub destination_token: Address,
    /// The ERC-20 the order buys on the swap's chain and hands to the provider.
    pub intermediate: Address,
    /// The configured token's symbol, or the provider's for a native asset.
    pub symbol: String,
    /// Whether the intermediate is the destination token's own asset. For Across, whether both
    /// tokens' configured symbols name one asset.
    pub same_asset: bool,
    /// 1Click's assets for a NEAR Intents destination, `None` for Across.
    pub near: Option<NearAssets>,
}

/// A token NEAR Intents can deliver on a [`NonEvmNetwork`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonEvmBridgeDestination {
    /// The mint or contract as 1Click lists it, `None` for the network's native asset.
    pub contract: Option<String>,
    /// The ERC-20 the order buys on the swap's chain and hands to 1Click.
    pub intermediate: Address,
    /// 1Click's symbol.
    pub symbol: String,
    /// Whether the intermediate is the destination token's own asset.
    pub same_asset: bool,
    /// The assets the quote converts between. The destination asset and decimals are the
    /// token's.
    pub near: NearAssets,
}

/// The 1Click assets a NEAR Intents quote converts between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NearAssets {
    /// The intermediate's asset id on the swap's chain.
    pub origin_asset: String,
    /// The destination token's asset id on the destination chain.
    pub destination_asset: String,
    pub origin_decimals: u8,
    pub destination_decimals: u8,
}

/// Destinations of Across `routes` from `origin_chain`, the swap's chain, to
/// `destination_chain` whose origin token is configured on the swap's chain and whose
/// destination token is configured there. A route that starts from `sell_token` is skipped.
/// When several routes reach one destination token, the one whose origin symbol matches the
/// destination symbol is kept, otherwise the first in the provider's order.
#[must_use]
pub fn across_destination_tokens(
    routes: &[AcrossRoute],
    sell_token: Address,
    registry: &EffectiveTokenRegistry,
    origin_chain: u64,
    destination_chain: u64,
) -> Vec<BridgeDestination> {
    let mut destinations: Vec<(BridgeDestination, bool)> = Vec::new();
    for route in routes {
        if route.origin_token == sell_token || route.origin_token == Address::ZERO {
            continue;
        }
        let Some(destination) =
            across_destination(route, registry, origin_chain, destination_chain, true)
        else {
            continue;
        };
        let matched = route
            .origin_symbol
            .eq_ignore_ascii_case(&route.destination_symbol);
        match destinations
            .iter_mut()
            .find(|(known, _)| known.destination_token == route.destination_token)
        {
            Some(known) if !known.1 && matched => *known = (destination, matched),
            Some(_) => {}
            None => destinations.push((destination, matched)),
        }
    }
    destinations
        .into_iter()
        .map(|(destination, _)| destination)
        .collect()
}

/// How a swap paid from a Public account reaches a destination token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicBridgePath {
    /// The destination token is the Across counterpart of the Sell token: the account deposits
    /// it itself.
    Deposit,
    /// Another token: a `CoW` order buys the route's origin token and its post-hook deposits it.
    Order,
}

/// A destination a swap paid from a Public account can deliver, and the path that reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicBridgeDestination {
    pub destination: BridgeDestination,
    pub path: PublicBridgePath,
}

/// What the sell side of a Public-paid swap is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicSellAsset {
    Erc20(Address),
    /// The chain's native asset, deposited as this wrapped native token.
    Native {
        wrapped: Address,
    },
}

/// The destination of an Across `route` whose destination token is configured on
/// `destination_chain`. An `order` buys the origin token, so it must be configured on
/// `origin_chain`; a direct deposit's origin is the account's own sell token. The API pairs the
/// tokens, so only their configured symbols say whether they are one asset.
fn across_destination(
    route: &AcrossRoute,
    registry: &EffectiveTokenRegistry,
    origin_chain: u64,
    destination_chain: u64,
    order: bool,
) -> Option<BridgeDestination> {
    let origin = registry.get(origin_chain, &route.origin_token);
    if order && origin.is_none() {
        return None;
    }
    let configured = registry.get(destination_chain, &route.destination_token)?;
    Some(BridgeDestination {
        destination_token: route.destination_token,
        intermediate: route.origin_token,
        symbol: configured.symbol.clone(),
        same_asset: origin.is_some_and(|origin| same_asset(&origin.symbol, &configured.symbol)),
        near: None,
    })
}

/// Destinations of Across `routes` from `origin_chain`, a Public account's chain, to
/// `destination_chain` whose destination token is configured there, and whose origin token,
/// when an order buys it, is configured on the account's chain. A route that starts from the
/// sell token, or from the wrapped token of a native sell asset, is a direct deposit. Any other
/// route takes an order, offered only when the chain can swap (`can_swap`) and the sell asset
/// is an ERC-20: a native asset is never the sell side of an order. When several routes reach
/// one destination token, a deposit is kept over an order, then an order whose origin symbol
/// matches the destination symbol, otherwise the first in the provider's order.
#[must_use]
pub fn public_across_destination_tokens(
    routes: &[AcrossRoute],
    sell: PublicSellAsset,
    can_swap: bool,
    registry: &EffectiveTokenRegistry,
    origin_chain: u64,
    destination_chain: u64,
) -> Vec<PublicBridgeDestination> {
    let (source, orders) = match sell {
        PublicSellAsset::Erc20(token) => (token, can_swap),
        PublicSellAsset::Native { wrapped } => (wrapped, false),
    };
    // Each destination with its route's preference: a deposit, then a symbol-matched order.
    let mut destinations: Vec<(PublicBridgeDestination, u8)> = Vec::new();
    for route in routes {
        if route.origin_token == Address::ZERO {
            continue;
        }
        let (path, preference) = if route.origin_token == source {
            (PublicBridgePath::Deposit, 2)
        } else if !orders {
            continue;
        } else if route
            .origin_symbol
            .eq_ignore_ascii_case(&route.destination_symbol)
        {
            (PublicBridgePath::Order, 1)
        } else {
            (PublicBridgePath::Order, 0)
        };
        let order = path == PublicBridgePath::Order;
        let Some(destination) =
            across_destination(route, registry, origin_chain, destination_chain, order)
        else {
            continue;
        };
        let destination = PublicBridgeDestination { destination, path };
        match destinations
            .iter_mut()
            .find(|(known, _)| known.destination.destination_token == route.destination_token)
        {
            Some(known) if known.1 < preference => *known = (destination, preference),
            Some(_) => {}
            None => destinations.push((destination, preference)),
        }
    }
    destinations
        .into_iter()
        .map(|(destination, _)| destination)
        .collect()
}

/// Intermediate tokens NEAR Intents is asked to convert, in order of preference after the
/// destination's own token. 1Click lists WETH but has no liquidity for it on any bridge
/// chain, so WETH never carries a swap, not even to ETH.
const NEAR_INTERMEDIATES: [&str; 2] = ["USDC", "USDT"];

/// The prefix of 1Click's newer asset id scheme.
const NEWER_ASSET_ID_PREFIX: &str = "1cs_v1:";

/// Destinations 1Click lists on `destination`'s chain: its configured ERC-20s and its native
/// asset. Each takes as intermediate the first token 1Click lists on `origin`'s chain with a
/// configured ERC-20 contract other than `sell_token`, preferring a token with the
/// destination's symbol, then USDC and USDT. Destinations without an intermediate are left
/// out. A `destination` without a 1Click name has none: NEAR Intents isn't offered there.
#[must_use]
pub fn near_destination_tokens(
    tokens: &[OneClickToken],
    origin: &BridgeProfile,
    destination: &BridgeDestinationProfile,
    sell_token: Address,
    registry: &EffectiveTokenRegistry,
) -> Vec<BridgeDestination> {
    let Some(destination_blockchain) = destination.one_click_blockchain() else {
        return Vec::new();
    };
    let on_origin = |token: &&OneClickToken| token.blockchain == origin.one_click_blockchain();
    let sell_symbol = tokens
        .iter()
        .filter(on_origin)
        .find(|token| token.contract_address == Some(sell_token))
        .map(|token| token.symbol.as_str());
    let intermediates = near_intermediates(tokens, origin, sell_token, registry);
    let mut destinations: Vec<BridgeDestination> = Vec::new();
    for token in tokens
        .iter()
        .filter(|token| token.blockchain == destination_blockchain)
    {
        let (destination_token, symbol) = match token.contract_address {
            None => (Address::ZERO, token.symbol.clone()),
            Some(contract) => match registry.get(destination.chain_id(), &contract) {
                Some(configured) if contract != Address::ZERO => {
                    (contract, configured.symbol.clone())
                }
                _ => continue,
            },
        };
        if sell_symbol.is_some_and(|sell| same_asset(sell, &token.symbol))
            || destinations
                .iter()
                .any(|known| known.destination_token == destination_token)
        {
            continue;
        }
        if let Some((candidate, matched)) = near_intermediate(&intermediates, &token.symbol)
            && let Some(intermediate) = candidate.contract_address
        {
            destinations.push(BridgeDestination {
                destination_token,
                intermediate,
                symbol,
                same_asset: matched,
                near: Some(NearAssets {
                    origin_asset: candidate.asset_id.clone(),
                    destination_asset: token.asset_id.clone(),
                    origin_decimals: candidate.decimals,
                    destination_decimals: token.decimals,
                }),
            });
        }
    }
    destinations
}

/// Destinations 1Click lists on `network`: every token there, whether or not the wallet knows
/// it. Intermediates are chosen as in [`near_destination_tokens`]. A destination that is the
/// sell token's own asset, such as BTC when selling WBTC, is kept: its intermediate is never the
/// sell token. An asset 1Click lists more than once, by contract and `None` for the native
/// asset, is shown once with its first listing's symbol, and quoted under its newer `1cs_v1:`
/// asset id when it has one: 1Click's quotes for `nep141:btc.omft.near` carry signatures that
/// don't verify, while those for `1cs_v1:btc:native:coin` do.
#[must_use]
pub fn near_non_evm_destination_tokens(
    tokens: &[OneClickToken],
    origin: &BridgeProfile,
    network: NonEvmNetwork,
    sell_token: Address,
    registry: &EffectiveTokenRegistry,
) -> Vec<NonEvmBridgeDestination> {
    let intermediates = near_intermediates(tokens, origin, sell_token, registry);
    let mut destinations: Vec<NonEvmBridgeDestination> = Vec::new();
    for token in tokens
        .iter()
        .filter(|token| token.blockchain == network.one_click_blockchain())
    {
        if let Some(known) = destinations
            .iter_mut()
            .find(|known| known.contract == token.contract)
        {
            if token.asset_id.starts_with(NEWER_ASSET_ID_PREFIX)
                && !known
                    .near
                    .destination_asset
                    .starts_with(NEWER_ASSET_ID_PREFIX)
            {
                known.near.destination_asset.clone_from(&token.asset_id);
                known.near.destination_decimals = token.decimals;
            }
            continue;
        }
        if let Some((candidate, matched)) = near_intermediate(&intermediates, &token.symbol)
            && let Some(intermediate) = candidate.contract_address
        {
            destinations.push(NonEvmBridgeDestination {
                contract: token.contract.clone(),
                intermediate,
                symbol: token.symbol.clone(),
                same_asset: matched,
                near: NearAssets {
                    origin_asset: candidate.asset_id.clone(),
                    destination_asset: token.asset_id.clone(),
                    origin_decimals: candidate.decimals,
                    destination_decimals: token.decimals,
                },
            });
        }
    }
    destinations
}

/// Tokens 1Click lists on `origin`'s chain with a configured ERC-20 contract other than
/// `sell_token`.
fn near_intermediates<'a>(
    tokens: &'a [OneClickToken],
    origin: &BridgeProfile,
    sell_token: Address,
    registry: &EffectiveTokenRegistry,
) -> Vec<&'a OneClickToken> {
    tokens
        .iter()
        .filter(|token| token.blockchain == origin.one_click_blockchain())
        .filter(|token| {
            token.contract_address.is_some_and(|contract| {
                contract != sell_token
                    && contract != Address::ZERO
                    && registry.get(origin.chain_id(), &contract).is_some()
            })
        })
        .collect()
}

/// The intermediate for a destination with 1Click `symbol`, and whether it is the same asset:
/// one with that symbol, then USDC, then USDT.
fn near_intermediate<'a>(
    intermediates: &[&'a OneClickToken],
    symbol: &str,
) -> Option<(&'a OneClickToken, bool)> {
    intermediates
        .iter()
        .find(|candidate| candidate.symbol.eq_ignore_ascii_case(symbol))
        .map(|candidate| (*candidate, true))
        .or_else(|| {
            NEAR_INTERMEDIATES.iter().find_map(|preferred| {
                intermediates
                    .iter()
                    .find(|candidate| candidate.symbol == *preferred)
                    .map(|candidate| (*candidate, false))
            })
        })
}

/// Equal 1Click or configured symbols, with a native symbol `X` the same asset as `WX`: ETH and
/// WETH, BNB and WBNB, POL and WPOL.
pub(crate) fn same_asset(a: &str, b: &str) -> bool {
    let wraps = |wrapped: &str, native: &str| {
        wrapped
            .strip_prefix(['W', 'w'])
            .is_some_and(|rest| rest.eq_ignore_ascii_case(native))
    };
    a.eq_ignore_ascii_case(b) || wraps(a, b) || wraps(b, a)
}

#[cfg(test)]
mod tests;
