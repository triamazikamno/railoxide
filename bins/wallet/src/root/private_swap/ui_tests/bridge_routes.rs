use super::*;

/// Another network's Buy list holds what either provider delivers. Across is the default
/// where it delivers; a token only NEAR Intents delivers switches to it and quotes again; and
/// the sell token's own asset offers no provider. A chosen provider has its hint button.
#[gpui::test]
fn bridge_buy_token_picks_the_provider_and_explains_a_same_token_pair(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        let shown = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            (
                cx.debug_bounds("swap-provider-hint-trigger").is_some(),
                cx.debug_bounds("swap-bridge-same-token").is_some(),
            )
        };
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        assert!(
            !root.read_with(cx, |root, _| root.active_rpc_chains.contains(&137)),
            "browsing a destination does not activate its background RPC work"
        );
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            let items = swaps
                .buy_picker_items(form, cx)
                .into_iter()
                .map(|item| (item.asset.token, item.near_only))
                .collect::<Vec<_>>();
            assert_eq!(
                items,
                [
                    (Address::ZERO, true),
                    (STUB_POLYGON_USDC, false),
                    (STUB_POLYGON_USDT, false),
                ],
                "native POL first with its NEAR Intents tag; USDC stays listed"
            );
        });

        // Both deliver USDT: Across.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(matches!(
                    form.bridge_state(),
                    BridgeState::Ready {
                        provider: BridgeProvider::Across,
                        near: true,
                        switched: false,
                        ..
                    }
                ));
                assert_eq!(
                    form.provider_select.read(cx).selected_value(),
                    Some(&BridgeProvider::Across)
                );
            });
        });
        assert_eq!(shown(cx), (true, false));
        assert!(
            root.read_with(cx, |root, _| root.active_rpc_chains.contains(&137)),
            "committing the destination starts its background RPC work"
        );

        // Only NEAR Intents delivers POL: it switches and quotes again.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_mut().unwrap();
                form.price_acknowledged = true;
                form.high_costs_acknowledged = true;
                swaps.set_form_buy(Address::ZERO, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(matches!(
                    form.bridge_state(),
                    BridgeState::Ready {
                        provider: BridgeProvider::NearIntents,
                        switched: true,
                        ..
                    }
                ));
                assert!(matches!(form.quote, QuoteState::Loading));
                assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                assert!(matches!(
                    form.delivery,
                    Ok(SwapDelivery::Bridge(BridgeDelivery {
                        provider: BridgeProvider::NearIntents,
                        destination_chain: 137,
                        destination_token: Address::ZERO,
                        surplus: BridgeSurplus::BridgedByProvider,
                        ..
                    }))
                ));
            });
        });
        assert_eq!(shown(cx), (true, false));
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| {
                !matches!(swaps.form.as_ref().unwrap().quote, QuoteState::Loading)
            })
        });
        assert!(
            stubs
                .bridge_requests()
                .iter()
                .any(|path| path.starts_with("/near/v0/quote")),
            "the swap is quoted with NEAR Intents"
        );

        // USDT goes back to Across; USDC is the sell token's asset.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_form_buy(STUB_POLYGON_USDT, window, cx);
                assert!(matches!(
                    swaps.form.as_ref().unwrap().bridge_state(),
                    BridgeState::Ready {
                        provider: BridgeProvider::Across,
                        ..
                    }
                ));
                swaps.set_form_buy(STUB_POLYGON_USDC, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(matches!(form.bridge_state(), BridgeState::SameToken));
                assert!(form.provider_select.read(cx).selected_value().is_none());
                assert!(
                    matches!(form.quote, QuoteState::Idle) && form.quote_task.is_none(),
                    "a same-token pair isn't quoted"
                );
            });
        });
        assert_eq!(shown(cx), (false, true));
    });
}

/// While one provider can't list its tokens, the other's can be chosen and quoted, beside a
/// warning button whose popover has Retry. With neither listing, the error takes the Buy line,
/// and its Retry brings back both lists without the warning. Private delivery requires Across,
/// so its failure must also offer Retry when NEAR still answers.
#[gpui::test]
fn bridge_routes_stay_usable_while_one_provider_is_unreachable(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        let shown = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            (
                cx.debug_bounds("swap-bridge-partial").is_some(),
                cx.debug_bounds("swap-price-error").is_some(),
            )
        };
        // Retry asks on a fresh route, which the fixture points at the stubs again. The warning
        // button's Retry is in its popover.
        let retry = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            if let Some(notice) = cx.debug_bounds("swap-bridge-notice-trigger") {
                cx.simulate_click(notice.center(), gpui::Modifiers::none());
                cx.run_until_parked();
                cx.update(|window, cx| window.draw(cx).clear(cx));
            }
            let retry = cx.debug_bounds("swap-bridge-routes-retry").unwrap();
            cx.simulate_click(retry.center(), gpui::Modifiers::none());
            let orderbook = stub_orderbook(&stubs, runtime);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    assert!(form.orderbook.is_none(), "Retry must request a fresh route");
                    assert!(!form.bridge.notice_open, "Retry closes the popover");
                    form.bridge.routes_tasks.clear();
                    form.bridge_clients = Some(stub_bridge_clients(&stubs, &orderbook));
                    form.orderbook = Some(orderbook);
                    swaps.load_bridge_routes(window, cx);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| {
                    let form = swaps.form.as_ref().unwrap();
                    form.bridge.routes.contains_key(&(STUB_USDC, 137))
                })
            });
        };

        stubs.set_failing(&["/near/v0/tokens"]);
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                assert!(matches!(
                    swaps.form.as_ref().unwrap().bridge_state(),
                    BridgeState::Ready {
                        provider: BridgeProvider::Across,
                        near: false,
                        ..
                    }
                ));
            });
        });
        ready_review(swaps, runtime, cx);
        assert_eq!(shown(cx), (true, false));

        stubs.set_failing(&["/near/v0/tokens", "/across/available-routes"]);
        retry(cx);
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert!(matches!(form.bridge_state(), BridgeState::Failed(_)));
            assert!(swaps.buy_picker_items(form, cx).is_empty());
        });
        assert_eq!(shown(cx), (false, true));

        stubs.set_failing(&[]);
        retry(cx);
        swaps.read_with(cx, |swaps, _| {
            assert!(matches!(
                swaps.form.as_ref().unwrap().bridge_state(),
                BridgeState::Ready {
                    provider: BridgeProvider::Across,
                    near: true,
                    ..
                }
            ));
        });
        assert_eq!(shown(cx), (false, false));

        stubs.set_failing(&["/across/available-routes"]);
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                // Keep the destination shown while testing the provider's list independently
                // of the destination wallet's setup funding.
                swaps.form.as_mut().unwrap().receive_to = ReceiveTo::PrivateBalance;
                let form = swaps.form.as_ref().unwrap();
                assert!(matches!(
                    swaps.buy_picker_content(form, cx).tokens,
                    buy_picker::BuyPickerTokens::Failed(_)
                ));
                cx.notify();
            });
        });
        stubs.set_failing(&[]);
        retry(cx);
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            let buy_picker::BuyPickerTokens::Listed(rows) =
                swaps.buy_picker_content(form, cx).tokens
            else {
                panic!("Retry must restore the private delivery token list");
            };
            assert!(
                rows.iter()
                    .any(|row| row.item.asset.token == STUB_POLYGON_USDT)
            );
        });
    });
}

/// On Arbitrum One, Across's WETH route is listed as ETH, which its `SpokePool` pays wallets,
/// together with NEAR Intents' native ETH. Picking ETH bridges WETH with Across.
#[gpui::test]
fn across_weth_to_arbitrum_is_listed_and_bridged_as_eth(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|window, cx| {
            root.update(cx, |root, _| enable_stub_chain(root, &stubs, 42161));
            swaps.update(cx, |swaps, cx| {
                swaps.show_buy_picker_network(42161, window, cx);
            });
        });
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                form.bridge.routes.contains_key(&(STUB_USDC, 42161))
            })
        });
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            let items = swaps
                .buy_picker_items(form, cx)
                .into_iter()
                .map(|item| {
                    (
                        item.asset.token,
                        item.asset.label.to_string(),
                        item.near_only,
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                items,
                [(Address::ZERO, "ETH".to_owned(), false)],
                "one ETH item both providers deliver, and no separate WETH"
            );
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_buy_token(Address::ZERO, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(matches!(
                    form.bridge_state(),
                    BridgeState::Ready {
                        provider: BridgeProvider::Across,
                        across: true,
                        near: true,
                        switched: false,
                        ..
                    }
                ));
                assert!(matches!(
                    form.delivery,
                    Ok(SwapDelivery::Bridge(BridgeDelivery {
                        provider: BridgeProvider::Across,
                        destination_chain: 42161,
                        destination_token: STUB_ARBITRUM_WETH,
                        ..
                    }))
                ));
            });
        });

        // Private balance shields the same route's token as WETH, and lists it so. Arbitrum's
        // session is still loading here, so nothing starts one.
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                root.chain_states.insert(
                    42161,
                    ChainUtxoState::Loading {
                        progress: ChainSyncProgress::default(),
                    },
                );
            });
            swaps.update(cx, |swaps, cx| {
                swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    (form.network, form.buy),
                    (Some(42161), Some(STUB_ARBITRUM_WETH))
                );
                assert!(matches!(
                    form.bridge_state(),
                    BridgeState::Ready {
                        provider: BridgeProvider::Across,
                        near: false,
                        ..
                    }
                ));
                let items = swaps
                    .buy_items_on(form, form.network, cx)
                    .into_iter()
                    .map(|item| (item.asset.token, item.asset.label.to_string()))
                    .collect::<Vec<_>>();
                assert_eq!(items, [(STUB_ARBITRUM_WETH, "WETH".to_owned())]);

                swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                assert_eq!(swaps.form.as_ref().unwrap().buy, Some(Address::ZERO));
            });
        });
    });
}

/// Across's route to a chain's configured wrapped native token is listed as the native asset
/// on any chain, here Base. On a chain without that setting the token is listed as itself.
#[gpui::test]
fn across_lists_the_configured_wrapped_native_token_as_the_native_asset(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        // List Base's tokens once its routes are fetched for `wrapped` as its setting.
        let base_items = |wrapped: Option<Address>, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                root.update(cx, |root, _| {
                    let mut base = root.effective_chain_configs.get(8453).unwrap().clone();
                    base.wrapped_native_token = wrapped;
                    put_chain(root, base);
                });
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.bridge.routes.remove(&(STUB_USDC, 8453));
                    swaps.show_buy_picker_network(8453, window, cx);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| {
                    let form = swaps.form.as_ref().unwrap();
                    form.bridge.routes.contains_key(&(STUB_USDC, 8453))
                })
            });
            swaps.read_with(cx, |swaps, cx| {
                swaps
                    .buy_picker_items(swaps.form.as_ref().unwrap(), cx)
                    .into_iter()
                    .map(|item| (item.asset.token, item.asset.label.to_string()))
                    .collect::<Vec<_>>()
            })
        };
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                enable_stub_chain(root, &stubs, 8453);
            });
        });
        assert_eq!(
            base_items(Some(STUB_BASE_WETH), cx),
            [(Address::ZERO, "ETH".to_owned())],
            "outside Ethereum and Arbitrum One, the setting decides too"
        );
        assert_eq!(
            base_items(None, cx),
            [(STUB_BASE_WETH, "WETH".to_owned())],
            "without the setting, the form doesn't promise the native asset"
        );
    });
}

/// Private balance can deliver on another network only when it is enabled with RPC endpoints,
/// has an accepted swap profile, is synced in this session, and holds private funds a setup
/// broadcaster accepts or a set-up account to deliver to. The first condition that fails is
/// the reason the picker shows. While it isn't known whether the funds there can pay for a
/// setup, the setup fee reads as being checked. A network that is syncing or checking can be
/// picked ahead of it, and one with a set-up account says that an existing account is needed
/// only once setup is known to be unfunded.
#[test]
fn private_balance_needs_a_synced_and_funded_network() {
    let funded = PrivateNetworkFacts {
        rpc: true,
        swap_profile: true,
        sync: NetworkSync::Ready,
        funds: SetupFunds::Funded,
        reusable: false,
    };
    let unavailable = NetworkAvailability::Unavailable;
    for (facts, availability) in [
        (funded, NetworkAvailability::Available),
        (
            PrivateNetworkFacts {
                funds: SetupFunds::Unfunded,
                ..funded
            },
            unavailable(NetworkUnavailable::Unfunded),
        ),
        (
            PrivateNetworkFacts {
                funds: SetupFunds::Unfunded,
                reusable: true,
                ..funded
            },
            NetworkAvailability::ReuseOnly,
        ),
        (
            PrivateNetworkFacts {
                funds: SetupFunds::Unknown,
                ..funded
            },
            NetworkAvailability::CheckingFee,
        ),
        // A set-up account doesn't settle what the funds there can pay.
        (
            PrivateNetworkFacts {
                funds: SetupFunds::Unknown,
                reusable: true,
                ..funded
            },
            NetworkAvailability::CheckingFee,
        ),
        // A set-up account doesn't stand in for the sync, which reads it.
        (
            PrivateNetworkFacts {
                sync: NetworkSync::Loading(None),
                funds: SetupFunds::Unfunded,
                reusable: true,
                ..funded
            },
            NetworkAvailability::Syncing(None),
        ),
        // Funds can't be read before the sync is ready, so a loading chain says so first.
        (
            PrivateNetworkFacts {
                sync: NetworkSync::Loading(Some(62)),
                funds: SetupFunds::Unfunded,
                ..funded
            },
            NetworkAvailability::Syncing(Some(62)),
        ),
        (
            PrivateNetworkFacts {
                sync: NetworkSync::Failed,
                ..funded
            },
            unavailable(NetworkUnavailable::SyncFailed),
        ),
        (
            PrivateNetworkFacts {
                swap_profile: false,
                ..funded
            },
            unavailable(NetworkUnavailable::PublicOnly),
        ),
        (
            PrivateNetworkFacts {
                rpc: false,
                swap_profile: false,
                ..funded
            },
            unavailable(NetworkUnavailable::Rpc),
        ),
    ] {
        assert_eq!(private_network_availability(facts), availability);
    }
    let reuse = NetworkAvailability::ReuseOnly;
    let checking = NetworkAvailability::CheckingFee;
    assert!(reuse.is_pickable(), "its routes stay selectable");
    assert!(
        NetworkAvailability::Syncing(None).is_pickable(),
        "the form waits for the sync"
    );
    assert!(!unavailable(NetworkUnavailable::Unfunded).is_pickable());
    // The row of a network with a set-up account has no note until setup is known to be
    // unfunded.
    assert!(checking.list_reason("Polygon", true).is_none());
    assert!(checking.list_reason("Polygon", false).is_some());
    assert!(reuse.list_reason("Polygon", true).is_some());
}

/// The picker lists every other chain enabled with RPC endpoints, built in or added by the
/// user, also without Railgun, in the network selector's order. Private balance can't deliver
/// on such a chain, so it leaves those out and counts them, and opening the picker starts no
/// session there. A Public address can pick one until its fetched routes show that no provider
/// serves it, which a provider that couldn't be asked doesn't show.
#[gpui::test]
fn buy_picker_lists_chains_without_railgun_for_a_public_address(cx: &mut TestAppContext) {
    const ADDED: u64 = 777_777;
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        let orderbook = stub_orderbook(&stubs, runtime);
        let listed = |swaps: &PrivateSwapsView, receive_to, cx: &App| {
            swaps
                .network_items(receive_to, cx)
                .into_iter()
                .filter(|network| !network.this_network)
                // The chains; the non-EVM networks after them have tests of their own.
                .filter_map(|network| Some((network.key.chain_id()?, network.availability)))
                .collect::<Vec<_>>()
        };
        let unavailable = NetworkAvailability::Unavailable;
        let fetched = |cx: &mut gpui::VisualTestContext| {
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| {
                    let form = swaps.form.as_ref().unwrap();
                    form.bridge.routes.contains_key(&(STUB_USDC, ADDED))
                })
            });
        };
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                enable_stub_chain(root, &stubs, 100);
                // A Railgun chain with a higher id than Gnosis, which the selector lists first.
                enable_stub_chain(root, &stubs, 42161);
                // A chain the user added has no Railgun deployment and no 1Click name, and the
                // stub Across has no route to it.
                let mut added = root.effective_chain_configs.get(100).unwrap().clone();
                added.chain_id = ADDED;
                added.built_in = false;
                added.rpc_route = wallet_ops::RpcChainRoute::new(ADDED, vec![stubs.rpc()]);
                put_chain(root, added);
                let mut disabled = built_in_chain(10);
                disabled.enabled = false;
                put_chain(root, disabled);
            });
            swaps.update(cx, |swaps, cx| {
                swaps
                    .private_owner()
                    .unwrap()
                    .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
                swaps.open_form(
                    None,
                    STUB_USDC,
                    None,
                    Some(U256::from(1_000_000)),
                    None,
                    SwapDelivery::Reshield,
                    window,
                    cx,
                );
                let form = swaps.form.as_mut().unwrap();
                form.bridge_clients = Some(stub_bridge_clients(&stubs, &orderbook));
                form.orderbook = Some(orderbook);
                swaps.open_buy_picker(window, cx);
                let chains = |swaps: &PrivateSwapsView, receive_to, cx: &App| {
                    swaps
                        .network_items(receive_to, cx)
                        .into_iter()
                        .map(|network| network.key)
                        .collect::<Vec<_>>()
                };
                let chain = BuyNetworkKey::Chain;
                let non_evm = NonEvmNetwork::ALL.map(BuyNetworkKey::NonEvm);
                assert_eq!(
                    chains(swaps, ReceiveTo::PrivateBalance, cx),
                    [chain(1), chain(42161)],
                    "only the chains with a private balance, and disabled Optimism and the \
                     non-EVM networks aren't listed"
                );
                assert_eq!(
                    chains(swaps, ReceiveTo::PublicAddress, cx),
                    [chain(1), chain(42161), chain(100), chain(ADDED)]
                        .into_iter()
                        .chain(non_evm)
                        .collect::<Vec<_>>(),
                    "the same order, with the Public-address-only chains after, then the \
                     non-EVM networks last"
                );
                let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
                assert_eq!(content.receive_to, ReceiveTo::PrivateBalance);
                assert_eq!(
                    content.network_counts,
                    (2, 8),
                    "Gnosis, the added chain and the four non-EVM networks take a Public \
                     address only"
                );
            });
            let states = &root.read(cx).chain_states;
            assert!(
                !states.contains_key(&100) && !states.contains_key(&ADDED),
                "no session starts on a chain without Railgun"
            );
            swaps.update(cx, |swaps, cx| {
                swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                set_receiver_text(swaps, &Address::repeat_byte(4).to_string(), window, cx);
                assert_eq!(
                    listed(swaps, ReceiveTo::PublicAddress, cx),
                    [
                        (42161, NetworkAvailability::Available),
                        (100, NetworkAvailability::Available),
                        (ADDED, NetworkAvailability::Available),
                    ],
                    "a chain can be picked before its routes are fetched"
                );
                let content = swaps.buy_picker_content(swaps.form.as_ref().unwrap(), cx);
                assert_eq!(
                    content.network_counts,
                    (2, 8),
                    "the counts don't depend on the delivery kind"
                );

                // The network search filters what the picker lists, by name or by the start
                // of the chain id, and leaves the form's own list alone.
                let chain = BuyNetworkKey::Chain;
                let non_evm = NonEvmNetwork::ALL.map(BuyNetworkKey::NonEvm);
                let networks = swaps.form.as_ref().unwrap().picker.networks.clone();
                let mut matches = |query: &'static str, cx: &mut Context<'_, PrivateSwapsView>| {
                    networks.update(cx, |list, cx| {
                        list.set_query(query, window, cx);
                        let delegate = list.delegate();
                        (
                            delegate.listed().collect::<Vec<_>>(),
                            delegate.heading_row(),
                        )
                    })
                };
                assert_eq!(matches(" ARBITRUM ", cx), (vec![chain(42161)], None));
                assert_eq!(matches("7777", cx), (vec![chain(ADDED)], None));
                assert_eq!(listed(swaps, ReceiveTo::PublicAddress, cx).len(), 3);
                // The non-EVM networks follow the chains under their heading, which only
                // parts them from the chains: a search matching none of the chains lists no
                // heading, which would take the row-0 selection and leave Enter doing nothing.
                assert_eq!(
                    matches("", cx),
                    (
                        [chain(1), chain(42161), chain(100), chain(ADDED)]
                            .into_iter()
                            .chain(non_evm)
                            .collect(),
                        Some(4)
                    )
                );
                assert_eq!(
                    matches("sol", cx),
                    (vec![BuyNetworkKey::NonEvm(NonEvmNetwork::Solana)], None)
                );
            });
        });

        // Across couldn't be asked, which doesn't show that it has no route there.
        stubs.set_failing(&["/across/available-routes"]);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.show_buy_picker_network(ADDED, window, cx);
            });
        });
        fetched(cx);
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert!(matches!(
                form.bridge.routes.get(&(STUB_USDC, ADDED)),
                Some(Err(_))
            ));
            assert_eq!(
                listed(swaps, ReceiveTo::PublicAddress, cx)[2],
                (ADDED, NetworkAvailability::Available)
            );
        });

        // Asked again, Across has no route there, and the chain has no 1Click name.
        stubs.set_failing(&[]);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_mut().unwrap();
                form.bridge.routes.remove(&(STUB_USDC, ADDED));
                swaps.load_bridge_routes(window, cx);
            });
        });
        fetched(cx);
        swaps.read_with(cx, |swaps, cx| {
            assert_eq!(
                listed(swaps, ReceiveTo::PublicAddress, cx),
                [
                    (42161, NetworkAvailability::Available),
                    (100, NetworkAvailability::Available),
                    (ADDED, unavailable(NetworkUnavailable::NoBridge)),
                ]
            );
        });

        // A form left on that network has no provider for any token, so nothing is quoted.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_mut().unwrap();
                form.network = Some(ADDED);
                form.buy = Some(Address::repeat_byte(9));
                swaps.bridge_choices_changed(window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(matches!(form.delivery, Err(DeliveryProblem::Bridge)));
                assert!(form.quote_delivery().is_none());
                assert!(
                    matches!(form.quote, QuoteState::Idle) && form.quote_task.is_none(),
                    "nothing is quoted, so Review stays unavailable"
                );
            });
        });
    });
}

#[gpui::test]
fn private_network_funding_checks_the_setup_fee_against_spendable_notes(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        let (session, owner) = swaps.read_with(cx, |swaps, _| {
            (
                Arc::clone(swaps.private_session().unwrap()),
                Arc::clone(swaps.private_owner().unwrap()),
            )
        });
        let http = root.read_with(cx, |root, _| root.http.clone());
        let delegate = root.read_with(cx, |root, _| {
            root.effective_chain_configs
                .get(1)
                .unwrap()
                .accepted_executor_profile()
                .unwrap()
                .delegate()
        });
        let candidate = PublicBroadcasterCandidate {
            chain_id: 1,
            railgun_address: "setup-fee-test".into(),
            identifier: None,
            token: STUB_USDC,
            fee: U256::from(10).pow(U256::from(18)),
            fees_id: "setup-fee".into(),
            fee_expiration: std::time::SystemTime::now() + Duration::from_secs(60),
            reliability: 0.9,
            available_wallets: 1,
            version: "8.2.3".into(),
            relay_adapt: None,
            relay_adapt_7702: Some(delegate),
            required_poi_list_keys: Vec::new(),
            viewing_public_key: [1; 32],
            address_data: broadcaster_core::crypto::railgun::AddressData {
                master_public_key: U256::ONE,
                viewing_public_key: [1; 32],
            },
            fee_policy_status: wallet_ops::BroadcasterFeePolicyStatus::UnknownAnchor,
        };
        for (balance, expected) in [
            (
                U256::ONE,
                NetworkAvailability::Unavailable(NetworkUnavailable::Unfunded),
            ),
            (
                U256::from(10).pow(U256::from(20)),
                NetworkAvailability::Available,
            ),
        ] {
            owner.plan_swaps_from_note_for_tests(STUB_USDC, balance);
            assert_eq!(
                runtime.block_on(buy_picker::estimate_network_funding(
                    &session,
                    vec![candidate.clone()],
                    &http,
                )),
                expected,
                "a compatible fee token alone is insufficient",
            );
        }
        // Offers that haven't arrived yet don't show the funds can't pay.
        assert_eq!(
            runtime.block_on(buy_picker::estimate_network_funding(
                &session,
                Vec::new(),
                &http,
            )),
            NetworkAvailability::CheckingFee,
        );
        let invalid = PublicBroadcasterCandidate {
            relay_adapt_7702: None,
            ..candidate
        };
        assert_eq!(
            runtime.block_on(buy_picker::estimate_network_funding(
                &session,
                vec![invalid],
                &http,
            )),
            NetworkAvailability::Unavailable(NetworkUnavailable::SetupFee),
            "an unusable offer must not be reported as insufficient funds",
        );
    });
}

#[gpui::test]
fn private_bridge_quote_survives_background_funding_checks(cx: &mut TestAppContext) {
    enum Refresh {
        Periodic,
        RenewedOffer,
        OtherTokenOffer(std::time::SystemTime),
        /// A periodic check whose estimate can't reach the RPC.
        FailedPeriodic,
    }

    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            let session = start_polygon_session(root, &stubs, runtime, store, cx);
            let mut offer = crate::root::tests::fee_row(137, STUB_POLYGON_USDT, "funding-offer");
            offer.relay_adapt_7702 = root.read_with(cx, |root, _| {
                root.effective_chain_configs
                    .get(137)
                    .unwrap()
                    .accepted_executor_profile()
                    .map(wallet_ops::settings::ExecutorProfile::delegate)
            });
            session
                .executor_owner()
                .unwrap()
                .plan_swaps_from_note_for_tests(STUB_POLYGON_USDT, U256::from(100_000_000));
            root.update(cx, |root, _| {
                let ChainUtxoState::Ready { snapshot, .. } =
                    root.chain_states.get_mut(&137).unwrap()
                else {
                    panic!("ready destination session")
                };
                *snapshot = Arc::new(wallet_ops::ListUtxosOutput {
                    chain_id: 137,
                    cache_key: "funding-refresh-test".into(),
                    utxo_count: 2,
                    unspent_count: 2,
                    spent_count: 0,
                    local_pending_spent_count: 0,
                    utxos: vec![
                        crate::root::tests::unshield_utxo_output(
                            STUB_POLYGON_USDT,
                            100_000_000,
                            0,
                            1,
                        ),
                        crate::root::tests::unshield_utxo_output(STUB_POLYGON_USDC, 1, 0, 2),
                    ],
                    totals: vec![
                        wallet_ops::TokenTotal {
                            token: STUB_POLYGON_USDT.to_string(),
                            total: "100000000".into(),
                            poi_verified_total: "100000000".into(),
                        },
                        wallet_ops::TokenTotal {
                            token: STUB_POLYGON_USDC.to_string(),
                            total: "1".into(),
                            poi_verified_total: "1".into(),
                        },
                    ],
                });
                root.monitor_state.write().upsert_fee(offer.clone());
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    swaps.refresh_destination_network(window, cx);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, cx| {
                    let form = swaps.form.as_ref().unwrap();
                    if let QuoteState::Failed(error) = &form.quote {
                        panic!("initial funding quote failed: {error:#}");
                    }
                    assert!(
                        !matches!(
                            swaps.network_setup_availability(137, cx),
                            NetworkAvailability::Unavailable(_)
                        ),
                        "initial funding: {:?}, delivery: {:?}",
                        swaps.network_setup_availability(137, cx),
                        form.delivery
                    );
                    matches!(form.quote, QuoteState::Ready(_))
                })
            });
            let review = ready_review(swaps, runtime, cx);
            swaps.update(cx, |swaps, _| {
                let form = swaps.form.as_mut().unwrap();
                form.price_acknowledged = true;
                form.high_costs_acknowledged = true;
            });
            let assert_same_quote = |swaps: &PrivateSwapsView| {
                let form = swaps.form.as_ref().unwrap();
                assert!(
                    matches!(&form.quote, QuoteState::Ready(current) if Arc::ptr_eq(current, &review)),
                    "a background funding check must retain the usable quote"
                );
                assert!(form.price_acknowledged && form.high_costs_acknowledged);
            };
            // Keep the quote while each kind of refresh is pending and after it completes.
            // The current-thread runtime holds the estimate pending until drive_until runs it.
            for refresh in [
                Refresh::Periodic,
                Refresh::RenewedOffer,
                Refresh::OtherTokenOffer(std::time::SystemTime::now() + Duration::from_secs(60)),
                Refresh::OtherTokenOffer(std::time::UNIX_EPOCH),
                Refresh::FailedPeriodic,
            ] {
                let fails = matches!(refresh, Refresh::FailedPeriodic);
                match refresh {
                    Refresh::FailedPeriodic => {
                        stubs.set_failing(&["/rpc"]);
                        swaps.update(cx, |swaps, _| {
                            swaps
                                .form
                                .as_mut()
                                .unwrap()
                                .picker
                                .expire_funding_for_tests(137);
                        });
                    }
                    Refresh::Periodic => swaps.update(cx, |swaps, _| {
                        swaps
                            .form
                            .as_mut()
                            .unwrap()
                            .picker
                            .expire_funding_for_tests(137);
                    }),
                    Refresh::RenewedOffer => {
                        offer.fees_id = "renewed-offer".into();
                        offer.fee_expiration += Duration::from_secs(60);
                        root.update(cx, |root, _| {
                            root.monitor_state.write().upsert_fee(offer.clone());
                        });
                    }
                    // An offer for USDC comes and goes, but both balances stay unchanged
                    // and USDT still pays the setup fee.
                    Refresh::OtherTokenOffer(expiration) => root.update(cx, |root, _| {
                        root.monitor_state
                            .write()
                            .upsert_fee(broadcaster_monitor::FeeRow {
                                token_address: STUB_POLYGON_USDC,
                                fee_expiration: expiration,
                                ..offer.clone()
                            });
                    }),
                }
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        assert_eq!(
                            swaps.network_setup_availability(137, cx),
                            NetworkAvailability::Available,
                            "broadcaster availability must not look like a balance change"
                        );
                        swaps.refresh_destination_network(window, cx);
                        assert_same_quote(swaps);
                        assert!(
                            !swaps
                                .form
                                .as_ref()
                                .unwrap()
                                .picker
                                .funding_checked_for_tests(137)
                        );
                    });
                });
                drive_until(cx, runtime, |cx| {
                    swaps.read_with(cx, |swaps, _| {
                        swaps
                            .form
                            .as_ref()
                            .unwrap()
                            .picker
                            .funding_checked_for_tests(137)
                    })
                });
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.refresh_destination_network(window, cx);
                        assert_same_quote(swaps);
                        // A check that couldn't run keeps the last answer.
                        assert_eq!(
                            swaps
                                .form
                                .as_ref()
                                .unwrap()
                                .picker
                                .funding_failed_for_tests(137),
                            fails
                        );
                        assert_eq!(
                            swaps.network_setup_availability(137, cx),
                            NetworkAvailability::Available
                        );
                    });
                });
            }
            stubs.set_failing(&[]);
            // A completed check that can no longer afford the fee must still stop review.
            offer.fee = U256::from(10).pow(U256::from(24));
            root.update(cx, |root, _| {
                root.monitor_state.write().upsert_fee(offer);
            });
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.refresh_destination_network(window, cx);
                    assert_same_quote(swaps);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| {
                    matches!(swaps.form.as_ref().unwrap().quote, QuoteState::Idle)
                })
            });
            swaps.read_with(cx, |swaps, cx| {
                assert_eq!(
                    swaps.network_setup_availability(137, cx),
                    NetworkAvailability::Unavailable(NetworkUnavailable::Unfunded)
                );
                assert!(swaps.form.as_ref().unwrap().delivery.is_err());
            });
            runtime.block_on(session.stop()).unwrap();
        },
    );
}
