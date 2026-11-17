use super::*;
use crate::settings::{
    CustomTokenSettings, TokenKey, WalletSettings, build_effective_chain_configs,
    build_effective_token_registry,
};

#[test]
fn enabled_sepolia_admits_private_executor_but_no_swaps_or_bridges() {
    let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    let chain = chains.get_mut(11_155_111).unwrap();
    chain.enabled = true;
    assert!(chain.require_railgun().is_ok());
    assert!(chain.accepted_executor_profile().is_some());
    assert!(chain.swap_profile().is_none());
    assert!(chain.public_swap_profile().is_none());
    assert!(chain.bridge_profile().is_none());
    assert!(chain.bridge_origin_profile().is_none());
    assert!(chain.bridge_destination().is_none());
}

#[test]
fn swap_profiles_require_accepted_built_in_executors() {
    let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    for (chain_id, api, guard) in [
        (
            1,
            "mainnet",
            address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"),
        ),
        (
            56,
            "bnb",
            address!("B971eF87ede563556b2ED4b1C0b0019111Dd85d2"),
        ),
        (
            137,
            "polygon",
            address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"),
        ),
        (
            42161,
            "arbitrum_one",
            address!("68b3465833fb72A70ecDF485E0e4C7bD8665Fc45"),
        ),
        (
            8453,
            "base",
            address!("2626664c2603336E57B271c5C0b26F421741e481"),
        ),
    ] {
        let chain = chains.get_mut(chain_id).unwrap();
        let profile = chain.swap_profile().expect("supported chain");
        assert_eq!(profile.chain_id(), chain_id);
        assert_eq!(
            profile.orderbook_api_base(),
            format!("https://api.cow.fi/{api}")
        );
        assert_eq!(profile.deadline_guard(), guard);
        assert_eq!(
            profile.settlement(),
            address!("9008D19f58AAbD9eD0D60971565AA8510560ab41")
        );
        assert_eq!(
            profile.vault_relayer(),
            address!("C92E8bdf79f0507f65a392b0ab4667716BFE0110")
        );
        assert_eq!(
            profile.hooks_trampoline(),
            address!("60Bf78233f48eC42eE3F101b9a05eC7878728006")
        );

        chain.built_in = false;
        assert!(chain.swap_profile().is_none());
        chain.built_in = true;
        let deployment = &mut chain.railgun.as_mut().unwrap().deployment;
        let delegate = deployment.relay_adapt_7702_contract;
        deployment.relay_adapt_7702_contract = Address::repeat_byte(7);
        assert!(chain.swap_profile().is_none());
        chain
            .railgun
            .as_mut()
            .unwrap()
            .deployment
            .relay_adapt_7702_contract = delegate;
        chain.enabled = false;
        assert!(chain.swap_profile().is_none());
    }
}

#[test]
fn bridge_profiles_cover_every_swap_chain_only() {
    let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    let mut unsupported_built_in = 0;
    let mut origin_only = Vec::new();
    for chain in chains.values() {
        let bridge = chain.bridge_profile();
        if chain.swap_profile().is_some() {
            let bridge = bridge.expect("every swap chain can bridge");
            assert_eq!(bridge.chain_id(), chain.chain_id);
            assert_ne!(bridge.spoke_pool(), Address::ZERO);
            let handler = if chain.chain_id == 56 {
                address!("AC537C12fE8f544D712d71ED4376a502EEa944d7")
            } else {
                address!("924a9f036260DdD5808007E1AA95f08eD08aA569")
            };
            assert_eq!(bridge.multicall_handler(), handler);
            // The public destination role names the same pool and 1Click chain.
            let destination = chain.bridge_destination().expect("every swap chain");
            assert_eq!(destination.chain_id(), chain.chain_id);
            assert_eq!(destination.spoke_pool(), Some(bridge.spoke_pool()));
            assert_eq!(
                destination.one_click_blockchain(),
                Some(bridge.one_click_blockchain())
            );
            // So does the origin role of a deposit a Public account sends itself.
            let origin = chain.bridge_origin_profile().expect("every swap chain");
            assert_eq!(origin.chain_id(), chain.chain_id);
            assert_eq!(origin.spoke_pool(), bridge.spoke_pool());
            assert_eq!(origin.across_api_base(), bridge.across_api_base());
        } else if chain.built_in && ![1, 56, 137, 42161, 8453].contains(&chain.chain_id) {
            assert!(bridge.is_none(), "chain {}", chain.chain_id);
            // A chain with a pinned pool and no Public swap profile is a deposit origin
            // only.
            if let Some(origin) = chain.bridge_origin_profile() {
                assert_ne!(origin.spoke_pool(), Address::ZERO);
                assert_eq!(
                    chain.bridge_destination().and_then(|d| d.spoke_pool()),
                    None,
                    "chain {}",
                    chain.chain_id
                );
                if chain.public_swap_profile().is_none() {
                    origin_only.push(chain.chain_id);
                }
            }
            unsupported_built_in += 1;
        }
    }
    assert!(unsupported_built_in > 0);
    let base = chains
        .get(8453)
        .unwrap()
        .bridge_profile()
        .expect("base bridge");
    assert_eq!(
        base.spoke_pool(),
        address!("09aea4b2242abC8bb4BB78D537A67a245A7bEC64")
    );
    assert_eq!(base.one_click_blockchain(), "base");
    origin_only.sort_unstable();
    assert_eq!(origin_only, [10, 130, 143, 480, 999, 4326, 4663, 9745]);
    // Across serves Arc and Tempo, which did not pass the fork check.
    for chain_id in [5042, 4217, 100] {
        assert!(
            chains
                .get(chain_id)
                .unwrap()
                .bridge_origin_profile()
                .is_none(),
            "chain {chain_id}"
        );
    }

    // A destination chain needs only RPC, so no executor or enabled check applies here.
    let chain = chains.get_mut(137).unwrap();
    chain.enabled = false;
    assert!(chain.swap_profile().is_none());
    assert!(chain.bridge_profile().is_some());
    assert!(chain.bridge_origin_profile().is_some());
    chain.built_in = false;
    assert!(chain.bridge_profile().is_none());
    assert!(chain.bridge_origin_profile().is_none());
}

#[test]
fn public_swap_profiles_need_no_accepted_executor() {
    let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    for chain_id in [1, 56, 137, 42161, 8453] {
        let chain = chains.get_mut(chain_id).unwrap();
        let profile = chain.public_swap_profile().expect("supported chain");
        assert_eq!(profile.chain_id(), chain_id);
        let swap = chain.swap_profile().expect("swap chain");
        assert_eq!(profile.settlement(), swap.settlement());
        assert_eq!(profile.vault_relayer(), swap.vault_relayer());
        assert_eq!(profile.orderbook_api_base(), swap.orderbook_api_base());
        assert_eq!(profile.app_code(), swap.app_code());
        assert_eq!(profile.app_data_byte_budget(), swap.app_data_byte_budget());
        assert_eq!(profile.valid_to_window(), swap.valid_to_window());
        assert_eq!(profile.anchor_deviation_bps(), swap.anchor_deviation_bps());
        assert_eq!(
            profile.cow_shed_factory(),
            address!("0a654985c5856ab562237286f36d55c0ff637213")
        );
        assert_eq!(
            profile.cow_shed_implementation(),
            address!("F0D586aB0017fDfE2ACf4AB008B3Ddb2CF50bB09")
        );
        assert_eq!(
            profile.weiroll(),
            address!("9585c3062Df1C247d5E373Cfca9167F7dC2b5963")
        );
        assert_eq!(profile.math(), SWAP_MATH_ADDRESS);

        chain
            .railgun
            .as_mut()
            .unwrap()
            .deployment
            .relay_adapt_7702_contract = Address::repeat_byte(7);
        assert!(chain.swap_profile().is_none());
        assert_eq!(chain.public_swap_profile(), Some(profile));
        chain.enabled = false;
        assert!(chain.public_swap_profile().is_none());
        chain.enabled = true;
        chain.built_in = false;
        assert!(chain.public_swap_profile().is_none());
    }
}

/// Avalanche, Linea and Ink take orders from a Public account without being swap or bridge
/// chains of the stealth flow.
#[test]
fn public_swap_profiles_cover_chains_without_a_swap_profile() {
    let mut chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    let mainnet = chains.get(1).unwrap().public_swap_profile().unwrap();
    for (chain_id, api) in [(43114, "avalanche"), (57073, "ink"), (59144, "linea")] {
        let chain = chains.get_mut(chain_id).unwrap();
        assert!(chain.swap_profile().is_none(), "chain {chain_id}");
        assert!(chain.bridge_profile().is_none(), "chain {chain_id}");
        assert!(chain.bridge_origin_profile().is_some(), "chain {chain_id}");
        let profile = chain.public_swap_profile().expect("order chain");
        assert_eq!(profile.chain_id(), chain_id);
        assert_eq!(
            profile.orderbook_api_base(),
            format!("https://api.cow.fi/{api}")
        );
        // Everything but the chain and its orderbook is as on the swap chains.
        assert_eq!(
            PublicSwapProfile {
                chain_id: mainnet.chain_id,
                orderbook_api_base: mainnet.orderbook_api_base,
                ..profile
            },
            mainnet
        );

        chain.enabled = false;
        assert!(chain.public_swap_profile().is_none());
        chain.enabled = true;
        chain.built_in = false;
        assert!(chain.public_swap_profile().is_none());
    }
    // A pinned pool alone isn't enough: `CoW` runs no orderbook on Optimism, and Plasma has
    // no weiroll executor.
    for chain_id in [10, 9745] {
        let chain = chains.get(chain_id).unwrap();
        assert!(chain.bridge_origin_profile().is_some(), "chain {chain_id}");
        assert!(chain.public_swap_profile().is_none(), "chain {chain_id}");
    }
}

#[test]
fn bridge_destinations_cover_every_enabled_chain_with_rpc() {
    const CUSTOM: u64 = 31337;
    let mut settings = WalletSettings::default();
    settings
        .chains
        .custom
        .insert(CUSTOM, crate::settings::tests::custom_evm_chain());
    let mut chains = build_effective_chain_configs(&settings).unwrap();
    // Built-in chains without Railgun, and a chain the user added: Avalanche has a 1Click
    // name, and none of them a destination pool.
    for (chain_id, one_click) in [(43114, Some("avax")), (59144, None), (CUSTOM, None)] {
        let chain = chains.get(chain_id).unwrap();
        assert!(chain.railgun.is_none(), "chain {chain_id}");
        assert!(chain.bridge_profile().is_none(), "chain {chain_id}");
        let destination = chain.bridge_destination().expect("enabled chain with RPC");
        assert_eq!(destination.chain_id(), chain_id);
        assert_eq!(destination.one_click_blockchain(), one_click);
        assert_eq!(destination.spoke_pool(), None);
    }

    let chain = chains.get_mut(43114).unwrap();
    chain.enabled = false;
    assert!(chain.bridge_destination().is_none());
}

/// Across names a chain's wrapped native token as the destination of its native delivery,
/// and only listed tokens are offered, so a chain whose native asset is ETH lists its WETH.
#[test]
fn eth_native_chains_list_their_wrapped_native_token() {
    let settings = WalletSettings::default();
    let chains = build_effective_chain_configs(&settings).unwrap();
    let registry = build_effective_token_registry(&settings).unwrap();
    let mut checked = 0;
    for chain in chains.values() {
        let Some(wrapped) = chain
            .wrapped_native_token
            .filter(|_| chain.built_in && chain.native_currency.symbol == "ETH")
        else {
            continue;
        };
        assert!(
            registry.get(chain.chain_id, &wrapped).is_some(),
            "chain {}",
            chain.chain_id
        );
        checked += 1;
    }
    assert!(checked > 2);
}

#[test]
fn swap_tokens_follow_the_configured_registry() {
    let profile = MAINNET_SWAP_PROFILE;
    let weth = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let steth = address!("ae7ab96520DE3A18E5e111B5EaAb095312D7fE84");
    let custom = Address::repeat_byte(7);
    let other_chain = Address::repeat_byte(8);
    let mut settings = WalletSettings::default();
    for (chain_id, token) in [(1, custom), (56, other_chain)] {
        settings.tokens.custom_tokens.push(CustomTokenSettings {
            chain_id,
            token_address: token.to_string(),
            symbol: "CUSTOM".into(),
            ..Default::default()
        });
    }
    settings.tokens.built_in_tombstones.push(TokenKey {
        chain_id: 1,
        token_address: usdc.to_string(),
    });
    let registry = build_effective_token_registry(&settings).unwrap();
    let reshield = SwapDelivery::Reshield;
    let external = SwapDelivery::External {
        receiver: Address::repeat_byte(9),
    };
    let offered = swap_destination_tokens(&registry, &profile)
        .into_iter()
        .map(|token| token.token_address.parse::<Address>().unwrap())
        .collect::<Vec<_>>();
    assert!(offered.contains(&custom));
    assert!(offered.contains(&steth));
    assert!(!offered.contains(&other_chain));
    assert!(!offered.contains(&usdc));

    // The backend admits the same tokens in either direction, without a second list.
    for (sell, buy) in [(weth, custom), (custom, weth), (steth, weth)] {
        assert_eq!(
            profile.pair_eligibility(sell, buy, &reshield),
            SwapTokenEligibility::Eligible
        );
    }
    let ineligible = SwapTokenEligibility::Ineligible;
    assert_eq!(
        profile.pair_eligibility(usdc, Address::ZERO, &external),
        SwapTokenEligibility::Eligible
    );
    for (sell, buy, delivery) in [
        (usdc, Address::ZERO, &reshield),
        (Address::ZERO, usdc, &reshield),
        (Address::ZERO, usdc, &external),
    ] {
        assert_eq!(
            profile.pair_eligibility(sell, buy, delivery),
            ineligible(SwapIneligibility::NativeAsset)
        );
    }
    assert_eq!(
        profile.pair_eligibility(weth, weth, &reshield),
        ineligible(SwapIneligibility::SameToken)
    );
}

#[test]
fn external_receivers_exclude_the_swaps_own_addresses() {
    let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    let chain = chains.get(1).unwrap();
    let profile = chain.swap_profile().unwrap();
    let railgun = chain.require_railgun().unwrap().deployment.contract;
    let executor = Address::repeat_byte(2);
    for (receiver, rejection) in [
        (Address::ZERO, SwapReceiverRejection::ZeroAddress),
        (executor, SwapReceiverRejection::Executor),
        (railgun, SwapReceiverRejection::Railgun),
        (profile.settlement(), SwapReceiverRejection::Settlement),
        (profile.vault_relayer(), SwapReceiverRejection::VaultRelayer),
        (
            profile.hooks_trampoline(),
            SwapReceiverRejection::HooksTrampoline,
        ),
    ] {
        assert_eq!(
            profile.check_receiver(railgun, executor, receiver),
            Err(rejection)
        );
    }
    // Another of the wallet's stealth accounts may receive.
    assert_eq!(
        profile.check_receiver(railgun, executor, Address::repeat_byte(3)),
        Ok(())
    );
}

#[test]
fn bridge_receivers_exclude_the_destination_chains_protocol_contracts() {
    let chains = build_effective_chain_configs(&WalletSettings::default()).unwrap();
    let destination = chains.get(137).unwrap();
    let bridge = destination.bridge_destination().unwrap();
    let railgun = destination.require_railgun().unwrap().deployment.contract;
    let spoke_pool = bridge.spoke_pool().unwrap();
    for (receiver, rejection) in [
        (Address::ZERO, BridgeReceiverRejection::ZeroAddress),
        (railgun, BridgeReceiverRejection::Railgun),
        (spoke_pool, BridgeReceiverRejection::SpokePool),
    ] {
        assert_eq!(
            bridge.check_receiver(Some(railgun), receiver),
            Err(rejection)
        );
    }
    assert_eq!(
        bridge.check_receiver(Some(railgun), Address::repeat_byte(3)),
        Ok(())
    );

    // A chain without Railgun and without a pinned pool rejects only the zero address:
    // contracts that are protocol contracts on Polygon are ordinary addresses there.
    let base = chains.get(8453).unwrap().bridge_destination().unwrap();
    assert_eq!(
        base.check_receiver(None, Address::ZERO),
        Err(BridgeReceiverRejection::ZeroAddress)
    );
    for receiver in [railgun, spoke_pool, Address::repeat_byte(3)] {
        assert_eq!(base.check_receiver(None, receiver), Ok(()));
    }
}
