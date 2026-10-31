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
    /// Whether the intermediate is the destination token's own asset. Always true for Across.
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

/// Destinations of Across `routes` from the swap's chain to `destination_chain` whose
/// destination token is configured there. A route that starts from `sell_token` is skipped.
/// When several routes reach one destination token, the one whose origin symbol matches the
/// destination symbol is kept, otherwise the first in the provider's order.
#[must_use]
pub fn across_destination_tokens(
    routes: &[AcrossRoute],
    sell_token: Address,
    registry: &EffectiveTokenRegistry,
    destination_chain: u64,
) -> Vec<BridgeDestination> {
    let mut destinations: Vec<(BridgeDestination, bool)> = Vec::new();
    for route in routes {
        if route.origin_token == sell_token || route.origin_token == Address::ZERO {
            continue;
        }
        let Some(configured) = registry.get(destination_chain, &route.destination_token) else {
            continue;
        };
        let matched = route
            .origin_symbol
            .eq_ignore_ascii_case(&route.destination_symbol);
        let destination = BridgeDestination {
            destination_token: route.destination_token,
            intermediate: route.origin_token,
            symbol: configured.symbol.clone(),
            same_asset: true,
            near: None,
        };
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

/// Destinations of Across `routes` from a Public account's chain to `destination_chain` whose
/// destination token is configured there. A route that starts from the sell token, or from
/// the wrapped token of a native sell asset, is a direct deposit. Any other route takes an
/// order, offered only when the chain can swap (`can_swap`) and the sell asset is an ERC-20: a
/// native asset is never the sell side of an order. When several routes reach one destination
/// token, a deposit is kept over an order, then an order whose origin symbol matches the
/// destination symbol, otherwise the first in the provider's order.
#[must_use]
pub fn public_across_destination_tokens(
    routes: &[AcrossRoute],
    sell: PublicSellAsset,
    can_swap: bool,
    registry: &EffectiveTokenRegistry,
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
        let Some(configured) = registry.get(destination_chain, &route.destination_token) else {
            continue;
        };
        let destination = PublicBridgeDestination {
            destination: BridgeDestination {
                destination_token: route.destination_token,
                intermediate: route.origin_token,
                symbol: configured.symbol.clone(),
                same_asset: true,
                near: None,
            },
            path,
        };
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

/// Equal 1Click symbols, with a native symbol `X` the same asset as `WX`: ETH and WETH, BNB and
/// WBNB, POL and WPOL.
fn same_asset(a: &str, b: &str) -> bool {
    let wraps = |wrapped: &str, native: &str| {
        wrapped
            .strip_prefix(['W', 'w'])
            .is_some_and(|rest| rest.eq_ignore_ascii_case(native))
    };
    a.eq_ignore_ascii_case(b) || wraps(a, b) || wraps(b, a)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;
    use crate::settings::{
        WalletSettings, build_effective_chain_configs, build_effective_token_registry,
    };

    const ARB_WETH: Address = address!("82af49447d8a07e3bd95bd0d56f35241523fbab1");
    const ARB_USDC: Address = address!("af88d065e77c8cc2239327c5edb3a432268e5831");
    const ARB_USDC_E: Address = address!("ff970a61a04b1ca14834a43f5de4533ebddb5cc8");
    const ARB_USDT: Address = address!("fd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9");
    const POL_USDC: Address = address!("3c499c542cEF5E3811e1192ce70d8cC03d5c3359");
    const POL_USDC_E: Address = address!("2791bca1f2de4661ed88a30c99a7a9449aa84174");
    const POL_USDT: Address = address!("c2132d05d31c914a87c6611c10748aeb04b58e8f");
    const POL_WETH: Address = address!("7ceB23fD6bC0adD59E62ac25578270cFf1b9f619");
    const BSC_USDC: Address = address!("8ac76a51cc950d9822d68b83fe1ad97b32cd580d");
    const BSC_ETH: Address = address!("2170Ed0880ac9A755fd29B2688956BD959F933F8");

    fn route(origin: Address, symbol: &str, destination: Address, to: &str) -> AcrossRoute {
        AcrossRoute {
            origin_token: origin,
            destination_token: destination,
            origin_symbol: symbol.into(),
            destination_symbol: to.into(),
        }
    }

    fn one_click(blockchain: &str, symbol: &str, contract: Option<Address>) -> OneClickToken {
        OneClickToken {
            asset_id: format!("{blockchain}:{symbol}"),
            blockchain: blockchain.into(),
            symbol: symbol.into(),
            decimals: 18,
            contract_address: contract,
            contract: contract.as_ref().map(ToString::to_string),
            price: None,
        }
    }

    fn destination_tokens(destinations: &[BridgeDestination]) -> Vec<(Address, Address)> {
        destinations
            .iter()
            .map(|destination| (destination.destination_token, destination.intermediate))
            .collect()
    }

    #[test]
    fn across_skips_routes_from_the_sell_token_and_keeps_other_routes_to_a_token() {
        let registry = build_effective_token_registry(&WalletSettings::default()).unwrap();
        // Arbitrum One to Polygon, as in `fixtures/across_routes_arb_pol.json`.
        let routes = [
            // Another origin for a destination that a later route reaches from its own asset.
            route(ARB_USDC_E, "USDC.e", POL_USDC, "USDC"),
            route(ARB_USDC, "USDC", POL_USDC, "USDC"),
            route(ARB_USDC, "USDC", POL_USDC_E, "USDC.e"),
            route(ARB_USDT, "USDT", POL_USDT, "USDT"),
            route(ARB_WETH, "WETH", POL_WETH, "WETH"),
            // Not in Polygon's configured list.
            route(ARB_WETH, "WETH", Address::repeat_byte(7), "XYZ"),
            // Nor is this address, whatever symbol the provider gives it.
            route(ARB_USDT, "USDT", Address::repeat_byte(8), "USDT"),
        ];

        // WETH to WETH would bridge the sell token itself.
        assert_eq!(
            destination_tokens(&across_destination_tokens(
                &routes, ARB_WETH, &registry, 137
            )),
            [
                (POL_USDC, ARB_USDC),
                (POL_USDC_E, ARB_USDC),
                (POL_USDT, ARB_USDT)
            ]
        );
        // Selling USDC.e keeps USDC's routes, including the one to USDC.e.
        assert_eq!(
            destination_tokens(&across_destination_tokens(
                &routes, ARB_USDC_E, &registry, 137
            )),
            [
                (POL_USDC, ARB_USDC),
                (POL_USDC_E, ARB_USDC),
                (POL_USDT, ARB_USDT),
                (POL_WETH, ARB_WETH)
            ]
        );
    }

    #[test]
    fn public_across_keeps_the_sell_tokens_routes_as_deposits_and_orders_the_rest() {
        use PublicBridgePath::{Deposit, Order};
        let registry = build_effective_token_registry(&WalletSettings::default()).unwrap();
        let routes = [
            // Another origin for a destination the sell token reaches itself.
            route(ARB_USDC_E, "USDC.e", POL_USDC, "USDC"),
            route(ARB_USDC, "USDC", POL_USDC, "USDC"),
            route(ARB_USDC, "USDC", POL_USDC_E, "USDC.e"),
            route(ARB_USDT, "USDT", POL_USDT, "USDT"),
            route(ARB_WETH, "WETH", POL_WETH, "WETH"),
        ];
        let offered = |sell, can_swap| {
            public_across_destination_tokens(&routes, sell, can_swap, &registry, 137)
                .into_iter()
                .map(|offer| {
                    (
                        offer.destination.destination_token,
                        offer.destination.intermediate,
                        offer.path,
                    )
                })
                .collect::<Vec<_>>()
        };

        // Selling USDC: its own routes are deposits, and the deposit to USDC replaces the
        // earlier order from USDC.e.
        assert_eq!(
            offered(PublicSellAsset::Erc20(ARB_USDC), true),
            [
                (POL_USDC, ARB_USDC, Deposit),
                (POL_USDC_E, ARB_USDC, Deposit),
                (POL_USDT, ARB_USDT, Order),
                (POL_WETH, ARB_WETH, Order)
            ]
        );
        // A chain with only an origin profile has no orders.
        assert_eq!(
            offered(PublicSellAsset::Erc20(ARB_USDC), false),
            [
                (POL_USDC, ARB_USDC, Deposit),
                (POL_USDC_E, ARB_USDC, Deposit)
            ]
        );
        // Selling WETH: an order buys USDC, the origin whose symbol matches the destination's.
        assert_eq!(
            offered(PublicSellAsset::Erc20(ARB_WETH), true),
            [
                (POL_USDC, ARB_USDC, Order),
                (POL_USDC_E, ARB_USDC, Order),
                (POL_USDT, ARB_USDT, Order),
                (POL_WETH, ARB_WETH, Deposit)
            ]
        );
        // The native asset is deposited as WETH and is never the sell side of an order.
        assert_eq!(
            offered(PublicSellAsset::Native { wrapped: ARB_WETH }, true),
            [(POL_WETH, ARB_WETH, Deposit)]
        );
    }

    #[test]
    fn near_offers_native_assets_and_never_uses_the_sell_token_as_intermediate() {
        let registry = build_effective_token_registry(&WalletSettings::default()).unwrap();
        let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let arbitrum = chains.get(42161).unwrap().bridge_profile().unwrap();
        let bnb = chains.get(56).unwrap().bridge_destination().unwrap();
        let tokens = [
            one_click("arb", "ETH", None),
            one_click("arb", "WETH", Some(ARB_WETH)),
            one_click("arb", "USDC", Some(ARB_USDC)),
            one_click("arb", "USDT", Some(ARB_USDT)),
            one_click("bsc", "BNB", None),
            one_click("bsc", "USDC", Some(BSC_USDC)),
            one_click("bsc", "ETH", Some(BSC_ETH)),
            // Not in BNB Chain's configured list.
            one_click("bsc", "XYZ", Some(Address::repeat_byte(7))),
            // Nor is this address, whatever symbol the provider gives it.
            one_click("bsc", "USDT", Some(Address::repeat_byte(8))),
        ];

        // Selling USDC: USDC on BNB Chain is the same asset. BNB has no asset match on
        // Arbitrum, and USDC is the sell token, so USDT carries it. ETH does too: 1Click has no
        // liquidity for WETH.
        let from_usdc = near_destination_tokens(&tokens, &arbitrum, &bnb, ARB_USDC, &registry);
        assert_eq!(
            destination_tokens(&from_usdc),
            [(Address::ZERO, ARB_USDT), (BSC_ETH, ARB_USDT)]
        );
        assert_eq!(from_usdc[0].symbol, "BNB");
        // Quotes convert between the chosen tokens' 1Click assets.
        assert_eq!(
            from_usdc
                .iter()
                .map(|destination| {
                    let near = destination.near.as_ref().unwrap();
                    (
                        destination.same_asset,
                        near.origin_asset.as_str(),
                        near.destination_asset.as_str(),
                    )
                })
                .collect::<Vec<_>>(),
            [
                (false, "arb:USDT", "bsc:BNB"),
                (false, "arb:USDT", "bsc:ETH")
            ]
        );
        // Selling WETH: ETH on BNB Chain is the same asset.
        assert_eq!(
            destination_tokens(&near_destination_tokens(
                &tokens, &arbitrum, &bnb, ARB_WETH, &registry
            )),
            [(Address::ZERO, ARB_USDC), (BSC_USDC, ARB_USDC)]
        );

        // Across has no native BNB route, so only NEAR Intents offers it.
        let across = [route(ARB_USDC, "USDC", BSC_USDC, "USDC")];
        assert!(
            across_destination_tokens(&across, ARB_WETH, &registry, 56)
                .iter()
                .all(|destination| destination.destination_token != Address::ZERO)
        );
    }

    #[test]
    fn near_non_evm_offers_every_listed_token_once_with_the_sell_tokens_asset() {
        const ARB_WBTC: Address = address!("2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f");
        let registry = build_effective_token_registry(&WalletSettings::default()).unwrap();
        let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let arbitrum = chains.get(42161).unwrap().bridge_profile().unwrap();
        let listed = |blockchain: &str, symbol: &str, asset_id: &str, contract: Option<&str>| {
            OneClickToken {
                asset_id: asset_id.into(),
                contract: contract.map(str::to_owned),
                ..one_click(blockchain, symbol, None)
            }
        };
        let tokens = [
            one_click("arb", "WBTC", Some(ARB_WBTC)),
            one_click("arb", "USDC", Some(ARB_USDC)),
            one_click("arb", "USDT", Some(ARB_USDT)),
            // 1Click lists native BTC under two asset ids.
            listed("btc", "BTC", "nep141:btc.omft.near", None),
            listed("btc", "BTC(OMNI)", "1cs_v1:btc:native:coin", None),
            listed(
                "sol",
                "USDC",
                "sol:USDC",
                Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            ),
            // In no token registry.
            listed(
                "sol",
                "BONK",
                "sol:BONK",
                Some("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"),
            ),
        ];
        let offered = |network| {
            near_non_evm_destination_tokens(&tokens, &arbitrum, network, ARB_WBTC, &registry)
                .into_iter()
                .map(|destination| {
                    (
                        destination.contract,
                        destination.intermediate,
                        destination.same_asset,
                        destination.near.destination_asset,
                    )
                })
                .collect::<Vec<_>>()
        };

        // Selling WBTC: BTC is offered once through USDC, under the first listing's symbol and
        // quoted under the `1cs_v1:` id, whose quotes 1Click signs verifiably.
        assert_eq!(
            offered(NonEvmNetwork::Bitcoin),
            [(None, ARB_USDC, false, "1cs_v1:btc:native:coin".to_owned())]
        );
        assert_eq!(
            near_non_evm_destination_tokens(
                &tokens,
                &arbitrum,
                NonEvmNetwork::Bitcoin,
                ARB_WBTC,
                &registry
            )[0]
            .symbol,
            "BTC"
        );
        assert_eq!(
            offered(NonEvmNetwork::Solana),
            [
                (
                    Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_owned()),
                    ARB_USDC,
                    true,
                    "sol:USDC".to_owned()
                ),
                (
                    Some("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263".to_owned()),
                    ARB_USDC,
                    false,
                    "sol:BONK".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn near_offers_nothing_for_a_destination_without_a_one_click_name() {
        let registry = build_effective_token_registry(&WalletSettings::default()).unwrap();
        let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
        let arbitrum = chains.get(42161).unwrap().bridge_profile().unwrap();
        // The wallet ships no 1Click name for Linea, so NEAR Intents offers nothing there, even
        // if the token list holds tokens under a name for it.
        let linea = chains.get(59144).unwrap().bridge_destination().unwrap();
        let tokens = [
            one_click("arb", "USDC", Some(ARB_USDC)),
            one_click("linea", "ETH", None),
        ];
        assert_eq!(
            near_destination_tokens(&tokens, &arbitrum, &linea, ARB_WETH, &registry),
            [] as [BridgeDestination; 0]
        );
    }
}
