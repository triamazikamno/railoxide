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
        // An origin not in Arbitrum's configured list, whatever symbol the provider gives it.
        route(Address::repeat_byte(9), "USDC", POL_USDC, "USDC"),
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
            &routes, ARB_WETH, &registry, 42161, 137
        )),
        [
            (POL_USDC, ARB_USDC),
            (POL_USDC_E, ARB_USDC),
            (POL_USDT, ARB_USDT)
        ]
    );
    // Selling USDC.e keeps USDC's routes, including the one to USDC.e, which is another
    // asset by its configured symbols.
    let from_usdc_e = across_destination_tokens(&routes, ARB_USDC_E, &registry, 42161, 137);
    assert_eq!(
        destination_tokens(&from_usdc_e),
        [
            (POL_USDC, ARB_USDC),
            (POL_USDC_E, ARB_USDC),
            (POL_USDT, ARB_USDT),
            (POL_WETH, ARB_WETH)
        ]
    );
    assert_eq!(
        from_usdc_e
            .iter()
            .map(|destination| destination.same_asset)
            .collect::<Vec<_>>(),
        [true, false, true, true]
    );
    // The provider can pair any configured origin with a destination: WETH to USDC is
    // offered, but not as the same asset.
    let paired = across_destination_tokens(
        &[route(ARB_WETH, "USDC", POL_USDC, "USDC")],
        ARB_USDC,
        &registry,
        42161,
        137,
    );
    assert_eq!(
        (destination_tokens(&paired), paired[0].same_asset),
        (vec![(POL_USDC, ARB_WETH)], false)
    );
}

#[test]
fn public_across_keeps_the_sell_tokens_routes_as_deposits_and_orders_the_rest() {
    use PublicBridgePath::{Deposit, Order};
    let registry = build_effective_token_registry(&WalletSettings::default()).unwrap();
    let routes = [
        // An origin not in Arbitrum's configured list is never bought by an order.
        route(Address::repeat_byte(9), "USDT", POL_USDT, "USDT"),
        // Another origin for a destination the sell token reaches itself.
        route(ARB_USDC_E, "USDC.e", POL_USDC, "USDC"),
        route(ARB_USDC, "USDC", POL_USDC, "USDC"),
        route(ARB_USDC, "USDC", POL_USDC_E, "USDC.e"),
        route(ARB_USDT, "USDT", POL_USDT, "USDT"),
        route(ARB_WETH, "WETH", POL_WETH, "WETH"),
    ];
    let offers = |sell, can_swap| {
        public_across_destination_tokens(&routes, sell, can_swap, &registry, 42161, 137)
    };
    let offered = |sell, can_swap| {
        offers(sell, can_swap)
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
    // USDC to USDC.e stays offered as another asset.
    assert_eq!(
        offers(PublicSellAsset::Erc20(ARB_WETH), true)
            .iter()
            .map(|offer| offer.destination.same_asset)
            .collect::<Vec<_>>(),
        [true, false, true, true]
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
        across_destination_tokens(&across, ARB_WETH, &registry, 42161, 56)
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
    let listed =
        |blockchain: &str, symbol: &str, asset_id: &str, contract: Option<&str>| OneClickToken {
            asset_id: asset_id.into(),
            contract: contract.map(str::to_owned),
            ..one_click(blockchain, symbol, None)
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
