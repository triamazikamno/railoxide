use super::*;

/// A private Bridge review delivers to the private balance on Polygon for either failure
/// choice: the Receive card names the network and no receiver, Pay now sums both setup fees,
/// the Bridge row adds the delivery allowance, a row states the failure choice, and the
/// disclosure informs without warning. Changing the choice quotes again and clears the
/// acknowledgements. The confirm-only step repeats the bound terms.
#[gpui::test]
fn private_bridge_review_shows_both_setups_and_the_failure_choice(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, operation, runtime, cx| {
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            cx.update(|window, cx| {
                // Polygon's session is still loading, so Private balance starts none.
                root.update(cx, |root, _| {
                    root.chain_states.insert(
                        137,
                        ChainUtxoState::Loading {
                            progress: ChainSyncProgress::default(),
                        },
                    );
                });
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    // Without a ready session there, a new swap can't shield on Polygon.
                    assert!(matches!(
                        swaps.form.as_ref().unwrap().delivery,
                        Err(DeliveryProblem::Network { .. })
                    ));
                    // A started swap met Private balance's conditions when it was approved.
                    swaps.form.as_mut().unwrap().operation = Some(operation);
                    swaps.bridge_choices_changed(window, cx);
                });
            });
            let fees = [
                SetupFee {
                    chain_id: 1,
                    token: STUB_USDC,
                    maximum: U256::from(420_000),
                    broadcaster: "0zk1origin".to_owned(),
                },
                SetupFee {
                    chain_id: 137,
                    token: STUB_POLYGON_USDT,
                    maximum: U256::from(50_000),
                    broadcaster: "0zk1destination".to_owned(),
                },
            ];
            for (failure, choice) in [
                (BridgeShieldFailure::RefundOnOrigin, "Refund on Ethereum"),
                (BridgeShieldFailure::KeepOnDestination, "Keep on Polygon"),
            ] {
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        if swaps.form.as_ref().unwrap().bridge.shield_failure != failure {
                            swaps.set_form_shield_failure(failure, window, cx);
                            let form = swaps.form.as_ref().unwrap();
                            assert!(
                                matches!(form.quote, QuoteState::Loading),
                                "the choice is part of the delivery, so it is quoted again"
                            );
                            assert!(!form.price_acknowledged && !form.high_costs_acknowledged);
                        }
                    });
                });
                let review = ready_review(swaps, runtime, cx);
                // Accepted for this quote. The next choice clears both.
                cx.update(|_, cx| {
                    swaps.update(cx, |swaps, _| {
                        let form = swaps.form.as_mut().unwrap();
                        form.price_acknowledged = true;
                        form.high_costs_acknowledged = true;
                        form.blocked_shield_acknowledged = true;
                    });
                });
                let bridge = *review.bridge().unwrap();
                let private = bridge.private.unwrap();
                assert!(!private.delivery_allowance.is_zero());
                assert!(!review.gas_allowance().is_zero());
                assert!(review.estimated_source_surplus().unwrap() > U256::ZERO);
                assert_eq!(
                    bridge.destination_minimum,
                    private.quoted_output - private.delivery_allowance
                );
                // The form shows the choice and its rows.
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert!(cx.debug_bounds("swap-shield-failure-refund").is_some());
                assert!(cx.debug_bounds("swap-shield-failure-keep").is_some());
                swaps.read_with(cx, |swaps, cx| {
                    let form = swaps.form.as_ref().unwrap();
                    let SwapDelivery::Bridge(delivery) = review.plan().delivery() else {
                        panic!("a Bridge delivery");
                    };
                    assert_eq!(
                        delivery.private.map(|private| private.on_shield_failure),
                        Some(failure)
                    );
                    assert_eq!(form.review_problem(&review), None);
                    assert_eq!(swaps.destination_setup_chain(form), Some(137));

                    let registry = root.read(cx).effective_token_registry.clone();
                    let limit = |fee: &SetupFee| {
                        format_token_amount_ceiling_for_display(
                            fee.chain_id,
                            fee.token,
                            fee.maximum,
                            Some(&registry),
                        )
                    };
                    let summary = swaps.swap_summary(&review, Some(&fees), None, None, None, cx);
                    let received = swaps.network_token_amount(
                        137,
                        STUB_POLYGON_USDT,
                        bridge.received_minimum(),
                        cx,
                    );
                    let (label, amount, lines) = summary.receive_card_for_test().unwrap();
                    assert_eq!(
                        (label.as_str(), amount),
                        ("Receive on Polygon, at least", received)
                    );
                    assert_eq!(lines, ["to your private balance"]);
                    assert_eq!(summary.receiver_for_test(), None, "no receiver is named");
                    let rows = summary.rows_for_test();
                    let value = |label: &str| {
                        rows.iter()
                            .find(|(row, _)| row == label)
                            .map(|(_, value)| value.clone())
                    };
                    let surplus = review.estimated_source_surplus().unwrap();
                    assert_eq!(
                        value("Returned"),
                        Some(swaps.with_usd(
                            format!("≈ {}", swaps.token_amount(STUB_USDT, surplus, cx)),
                            STUB_USDT,
                            surplus,
                            cx,
                        ))
                    );
                    assert_eq!(
                        value("Pay now"),
                        Some(format!(
                            "up to {} + {} · not refunded",
                            limit(&fees[0]),
                            limit(&fees[1])
                        ))
                    );
                    // The cost rows sit under one collapsed line, which names what isn't
                    // refunded.
                    let (title, collapsed, costs) = summary.row_group_for_test().unwrap();
                    assert_eq!(title, "Costs");
                    assert_eq!(costs, ["Pay now", "Gas", "Returned", "Bridge"]);
                    assert_eq!(
                        collapsed,
                        format!(
                            "{} · up to {} + {} now, not refunded",
                            swaps.costs_label(&review, "", cx),
                            limit(&fees[0]),
                            limit(&fees[1])
                        )
                    );
                    let setup = summary.row_hint_for_test("Pay now").unwrap();
                    assert!(
                        setup.contains(&format!("{} on Ethereum", limit(&fees[0])))
                            && setup.contains(&format!("{} on Polygon", limit(&fees[1]))),
                        "{setup}"
                    );
                    assert_eq!(
                        value("Bridge"),
                        Some(format!(
                            "Across · {} + ≈ {} delivery",
                            swaps.token_amount(STUB_USDT, bridge.fee.unwrap(), cx),
                            swaps.network_bare_amount(
                                137,
                                STUB_POLYGON_USDT,
                                private.delivery_allowance,
                                cx
                            )
                        ))
                    );
                    assert_eq!(value("If the shield fails").as_deref(), Some(choice));
                    assert_eq!(value("Receiver"), None);
                    // Before either account is derived, each is a new account on its network.
                    for account in ["Source · Ethereum", "Destination · Polygon"] {
                        assert_eq!(value(account).as_deref(), Some("New account"), "{account}");
                    }
                    assert!(
                        summary
                            .warnings_for_test()
                            .iter()
                            .all(|warning| !warning.starts_with("Reusing")),
                        "new accounts link nothing"
                    );
                    let (public, card, warns) = summary.disclosure_for_test().unwrap();
                    assert!(!card.contains("reuses"), "{card}");
                    assert_eq!(public, "this swap, and that it shields on Polygon");
                    assert!(!warns, "the informational alert");
                    assert!(
                        card.contains(
                            "The deposit names a stealth account on Polygon and the shield it runs."
                        ) && card.contains("stay private"),
                        "{card}"
                    );
                    assert_eq!(
                        card.contains("publicly visible"),
                        failure == BridgeShieldFailure::KeepOnDestination,
                        "{card}"
                    );
                    let zero = Address::ZERO.to_checksum(None);
                    assert!(
                        !card.contains(&zero)
                            && rows.iter().all(|(_, value)| !value.contains(&zero)),
                        "no address appears"
                    );

                    // The confirm-only step binds a lower approved minimum than this requote.
                    let approved = bridge.destination_minimum / U256::from(2);
                    let approval = OrderApproval {
                        operation,
                        review: Arc::clone(&review),
                        private_minimum: review.suggested_private_minimum(),
                        price_acknowledged: true,
                        orderbook: form.orderbook.clone().unwrap(),
                        bridge: None,
                        destination_minimum: Some(approved),
                        continuing_minimum: None,
                        full_review: false,
                        swap_use: None,
                        pair_destination: None,
                        speed_up: None,
                    };
                    assert_eq!(
                        approval
                            .private_delivery()
                            .map(|delivery| delivery.destination_chain),
                        Some(137)
                    );
                    let place = swaps.place_summary(&approval, cx).unwrap();
                    let credited = SwapBridgeQuote {
                        destination_minimum: approved,
                        ..bridge
                    }
                    .received_minimum();
                    let (label, amount, lines) = place.receive_card_for_test().unwrap();
                    assert_eq!(
                        (label.as_str(), amount),
                        (
                            "Receive on Polygon, at least",
                            swaps.network_token_amount(137, STUB_POLYGON_USDT, credited, cx)
                        )
                    );
                    assert_eq!(lines, ["to your private balance"]);
                    assert_eq!(place.receiver_for_test(), None);
                    assert_eq!(
                        place.rows_for_test(),
                        [
                            ("Bridge".to_owned(), "Across".to_owned()),
                            ("If the shield fails".to_owned(), choice.to_owned()),
                            ("Source · Ethereum".to_owned(), "New account".to_owned()),
                            ("Destination · Polygon".to_owned(), "New account".to_owned()),
                        ]
                    );
                });
            }
        },
    );
}

#[gpui::test]
fn private_bridge_approval_expires_when_the_destination_session_ends(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, operation, runtime, store, cx| {
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    swaps.form.as_mut().unwrap().operation = Some(operation);
                    swaps.bridge_choices_changed(window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            let session = start_polygon_session(root, &stubs, runtime, store, cx);
            let command = cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let approval = OrderApproval {
                        operation,
                        private_minimum: review.suggested_private_minimum(),
                        destination_minimum: Some(review.bridge().unwrap().destination_minimum),
                        continuing_minimum: None,
                        review: Arc::clone(&review),
                        price_acknowledged: true,
                        orderbook: swaps.form.as_ref().unwrap().orderbook.clone().unwrap(),
                        bridge: None,
                        full_review: true,
                        swap_use: None,
                        pair_destination: None,
                        speed_up: None,
                    };
                    let summary = swaps.swap_summary(&review, None, None, None, None, cx);
                    swaps.request_authorization(
                        SwapAction::Order(Box::new(approval)),
                        summary,
                        window,
                        cx,
                    );
                    swaps.pending_authorization.clone().unwrap()
                })
            });
            let intent = SpendAuthorizationIntent::PrivateSwap(swaps.clone(), command);
            root.update(cx, |root, _| {
                assert!(intent.approve_gateway_review(root));
                root.chain_states.remove(&137);
                assert!(
                    root.stealth_session().is_some(),
                    "the origin session remains ready"
                );
                assert!(
                    !intent.approve_gateway_review(root),
                    "the ended destination session invalidates the review"
                );
            });
            runtime.block_on(async {
                session.stop().await.unwrap();
            });
        },
    );
}

/// A private Bridge swap's two setups report by themselves: a problem names its network, and
/// one setup's problem doesn't stand for the other. A swap with one setup keeps its message.
#[test]
fn setup_problems_name_the_network_of_each_setup() {
    let networks = ("Ethereum".to_owned(), "Polygon".to_owned());
    let problem = |text: &str| Some(text.to_owned());
    assert_eq!(
        setup_problems(None, problem("timed out"), Some(&networks)),
        problem("Polygon: timed out")
    );
    assert_eq!(
        setup_problems(problem("rejected"), problem("timed out"), Some(&networks)),
        problem("Ethereum: rejected Polygon: timed out")
    );
    assert_eq!(setup_problems(None, None, Some(&networks)), None);
    assert_eq!(
        setup_problems(problem("rejected"), None, None),
        problem("rejected")
    );
}

/// A NEAR Intents review of native POL shows 1Click's estimate in POL's own decimals, no
/// Returned row, a fee it can't value without anchors, its disclaimer, and what 1Click learns.
#[gpui::test]
fn near_intents_review_shows_the_estimate_and_what_1click_learns(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, operation, runtime, cx| {
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(Address::ZERO, window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            swaps.read_with(cx, |swaps, cx| {
            let summary = swaps.swap_summary(&review, None, None, None, None, cx);
            let pol = swaps.network_token_symbol(137, Address::ZERO, cx);
            let (label, amount, lines) = summary.receive_card_for_test().unwrap();
            assert_eq!(
                (label, amount),
                (
                    "Receive on Polygon, at least".to_owned(),
                    format!("12.5 {pol}")
                )
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("about ") && line.ends_with(" expected")),
                "{lines:?}"
            );
            assert_eq!(
                summary.receiver_for_test().map(|(address, _)| address),
                Some(Address::repeat_byte(4).to_checksum(None))
            );
            let rows = summary.rows_for_test();
            assert_eq!(
                rows[..2],
                [
                    gas_row(swaps, &review, cx),
                    (
                        "Bridge".to_owned(),
                        "NEAR Intents · fee included".to_owned()
                    ),
                ]
            );
            assert!(rows.iter().all(|(label, _)| label != "Returned"), "{rows:?}");
            let deposit = swaps.token_amount(STUB_USDT, review.suggested_private_minimum(), cx);
            assert!(
                summary
                    .details_for_test()
                    .contains(&("Deposit to 1Click".to_owned(), format!("{deposit} on Ethereum")))
            );
            let warnings = summary.warnings_for_test();
            assert!(
                warnings.iter().any(|warning| warning == NEAR_INTENTS_DISCLAIMER)
                    && warnings.iter().all(|warning| !warning.contains("Across")),
                "{warnings:?}"
            );
            let (_, public, _) = summary.disclosure_for_test().unwrap();
            assert!(
                public.contains("1Click learns the receiver when the order is signed, so the receiver's funds on Polygon can be traced to this swap."),
                "{public}"
            );

            // The confirm-only step shows the estimate only for the deposit it was quoted
            // for, and only when it reaches the approved destination minimum.
            let bridge = *review.bridge().unwrap();
            let shows_estimate = |private_minimum, destination_minimum| {
                let approval = OrderApproval {
                    operation,
                    review: Arc::clone(&review),
                    private_minimum,
                    price_acknowledged: true,
                    orderbook: swaps.form.as_ref().unwrap().orderbook.clone().unwrap(),
                    bridge: None,
                    destination_minimum: Some(destination_minimum),
                    continuing_minimum: None,
                    full_review: false,
                    swap_use: None,
                    pair_destination: None,
                    speed_up: None,
                };
                let (_, _, lines) = swaps
                    .place_summary(&approval, cx)
                    .unwrap()
                    .receive_card_for_test()
                    .unwrap();
                lines.iter().any(|line| line.ends_with(" expected"))
            };
            let deposit = review.suggested_private_minimum();
            let (below, above) = (
                bridge.expected_output - U256::ONE,
                bridge.expected_output + U256::ONE,
            );
            assert!(shows_estimate(deposit, below));
            assert!(!shows_estimate(deposit, above));
            // A raised deposit has no estimate, whichever side of the minimum the old one is.
            for approved in [below, above] {
                assert!(!shows_estimate(deposit + U256::ONE, approved), "{approved}");
            }
        });
        },
    );
}

fn non_evm_entry(
    label: &str,
    network: NonEvmNetwork,
    address: &str,
) -> wallet_ops::vault::NonEvmAddressBookEntry {
    wallet_ops::vault::NonEvmAddressBookEntry {
        entry_uuid: format!("{label}-entry"),
        label: label.into(),
        network,
        address: address.into(),
        display_order: 0,
    }
}

/// Show the open Buy picker's `network` and wait for 1Click's tokens there.
fn show_non_evm_network(
    swaps: &Entity<PrivateSwapsView>,
    network: NonEvmNetwork,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) {
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            swaps.show_buy_picker_network(network, window, cx);
        });
    });
    drive_until(cx, runtime, |cx| {
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            form.bridge
                .non_evm_routes
                .contains_key(&(STUB_USDC, network))
        })
    });
}

/// Showing and picking a non-EVM network starts no chain session. Solana lists 1Click's tokens
/// there with their mints, so look-alike symbols each show their own, and a pasted mint finds
/// its token. The receiver must then be a Solana address, its suggestions are the saved Solana
/// entries alone, and Private balance isn't available there.
#[gpui::test]
fn non_evm_networks_list_1click_tokens_and_take_their_own_receivers(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        let sessions = |cx: &mut gpui::VisualTestContext| {
            root.read_with(cx, |root, _| {
                let mut chains = root.chain_states.keys().copied().collect::<Vec<_>>();
                chains.sort_unstable();
                chains
            })
        };
        let before = sessions(cx);
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                root.public_address_book = vec![cold_wallet_entry(Address::repeat_byte(0x51))];
                root.non_evm_address_book = vec![
                    non_evm_entry("Phantom", NonEvmNetwork::Solana, SOLANA_RECEIVER),
                    non_evm_entry(
                        "Cold storage",
                        NonEvmNetwork::Bitcoin,
                        "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
                    ),
                ];
            });
        });
        show_non_evm_network(swaps, NonEvmNetwork::Solana, runtime, cx);
        let tokens = swaps.read_with(cx, |swaps, _| {
            swaps.form.as_ref().unwrap().picker.tokens.clone()
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| {
            tokens.update(cx, |list, cx| {
                assert_eq!(
                    list.delegate().listed_contracts(),
                    [
                        ("SOL".to_owned(), None),
                        ("sUSDC".to_owned(), Some(STUB_SOLANA_SUSDC.to_owned())),
                        ("USDC".to_owned(), Some(STUB_SOLANA_USDC.to_owned())),
                    ],
                    "the native asset first, and each look-alike with its own mint"
                );
                list.set_query(STUB_SOLANA_SUSDC, window, cx);
                assert_eq!(
                    list.delegate().listed_contracts(),
                    [("sUSDC".to_owned(), Some(STUB_SOLANA_SUSDC.to_owned()))]
                );
            });
            swaps.update(cx, |swaps, cx| {
                swaps.pick_non_evm_token(Some(STUB_SOLANA_USDC.to_owned()), window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    (form.network, form.non_evm, form.buy),
                    (None, Some(NonEvmNetwork::Solana), None)
                );
                // The Polygon swap's 0x receiver isn't a Solana address.
                assert_eq!(
                    form.delivery,
                    Err(DeliveryProblem::Receiver(
                        "The receiver must be a Solana address.".into()
                    ))
                );
                let options = swaps.receiver_options(cx);
                assert_eq!(
                    options
                        .iter()
                        .map(|option| (option.label.as_ref(), option.address.as_ref()))
                        .collect::<Vec<_>>(),
                    [("Phantom", SOLANA_RECEIVER)],
                    "no public account, EVM entry or other network's entry is suggested"
                );
                set_receiver_text(swaps, SOLANA_RECEIVER, window, cx);
                assert!(matches!(
                    &swaps.form.as_ref().unwrap().delivery,
                    Ok(SwapDelivery::NonEvmBridge(delivery))
                        if delivery.receiver == SOLANA_RECEIVER
                            && delivery.contract.as_deref() == Some(STUB_SOLANA_USDC)
                ));
                swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(
                    matches!(
                        &form.delivery,
                        Err(DeliveryProblem::Network { problem, syncing: false })
                            if problem.starts_with("Private balance isn't available on Solana.")
                    ),
                    "{:?}",
                    form.delivery
                );
                assert!(form.quote_delivery().is_none(), "nothing is quoted");
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-receive-to-problem").is_some());
        assert_eq!(
            sessions(cx),
            before,
            "a non-EVM network has no chain session"
        );
    });
}

/// A Bitcoin receiver is checked as a mainnet address, and a new one is saved to the non-EVM
/// address book with network Bitcoin while the field keeps it. Zcash explains its own rules.
#[gpui::test]
fn new_bitcoin_receiver_saves_to_the_non_evm_address_book(cx: &mut TestAppContext) {
    const RECEIVER: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        show_non_evm_network(swaps, NonEvmNetwork::Zcash, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_non_evm_token(None, window, cx);
                for (receiver, message) in [
                    // A `t1` address with its last character changed.
                    (
                        "t1TD3bn8rTPyM8DJjMCM3vs1dhrtrspoDmi",
                        "This isn't a valid Zcash address. Check it, including its checksum.",
                    ),
                    (
                        "zs1z7rejlpsa98s2rrrfkwmaxu53e4ue0ulcrw0h4x5g8jl04tak0d3mm47vdtahatqrlkngh9slya",
                        "The bridge can't pay a Sapling-only address. Enter a t1… address, or a \
                         unified address (u1…) with an Orchard or transparent receiver.",
                    ),
                ] {
                    set_receiver_text(swaps, receiver, window, cx);
                    assert_eq!(
                        swaps.form.as_ref().unwrap().delivery,
                        Err(DeliveryProblem::Receiver(message.into()))
                    );
                }
                swaps.open_buy_picker(window, cx);
            });
        });
        show_non_evm_network(swaps, NonEvmNetwork::Bitcoin, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_non_evm_token(None, window, cx);
                set_receiver_text(
                    swaps,
                    "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx",
                    window,
                    cx,
                );
                assert_eq!(
                    swaps.form.as_ref().unwrap().delivery,
                    Err(DeliveryProblem::Receiver(
                        "Only mainnet addresses can receive the swap. Enter a mainnet address."
                            .into()
                    ))
                );
                set_receiver_text(swaps, RECEIVER, window, cx);
                assert!(matches!(
                    &swaps.form.as_ref().unwrap().delivery,
                    Ok(SwapDelivery::NonEvmBridge(delivery))
                        if delivery.network == NonEvmNetwork::Bitcoin && delivery.contract.is_none()
                ));
            });
            window.draw(cx).clear(cx);
        });
        let save = cx.debug_bounds("swap-save-receiver").unwrap();
        cx.simulate_click(save.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("Cold storage");
        cx.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
        cx.run_until_parked();
        root.read_with(cx, |root, _| {
            assert_eq!(
                root.non_evm_address_book
                    .iter()
                    .map(|entry| (entry.label.as_str(), entry.network, entry.address.as_str()))
                    .collect::<Vec<_>>(),
                [("Cold storage", NonEvmNetwork::Bitcoin, RECEIVER)]
            );
            assert!(
                root.public_address_book
                    .iter()
                    .all(|entry| entry.label != "Cold storage"),
                "nothing is saved to the public address book"
            );
        });
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(form.receiver_input.read(cx).value().to_string(), RECEIVER);
        });
    });
}

/// A Solana review names the network, the receiver with its saved Solana label, and the
/// token's mint, with NEAR Intents as its bridge and the only provider the form shows. No
/// anchor prices the look-alike sUSDC, so the review asks for the unverified-price
/// acknowledgement, names the token without one, and values it only by 1Click's estimate.
#[gpui::test]
fn non_evm_review_names_the_network_receiver_and_mint(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                root.non_evm_address_book = vec![non_evm_entry(
                    "Phantom",
                    NonEvmNetwork::Solana,
                    SOLANA_RECEIVER,
                )];
            });
        });
        show_non_evm_network(swaps, NonEvmNetwork::Solana, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_non_evm_token(Some(STUB_SOLANA_SUSDC.to_owned()), window, cx);
                set_receiver_text(swaps, SOLANA_RECEIVER, window, cx);
            });
        });
        let review = ready_review(swaps, runtime, cx);
        swaps.read_with(cx, |swaps, cx| {
            let SwapDelivery::NonEvmBridge(delivery) = review.plan().delivery() else {
                panic!("a non-EVM review");
            };
            assert_eq!(
                (delivery.symbol.as_str(), delivery.contract.as_deref()),
                ("sUSDC", Some(STUB_SOLANA_SUSDC))
            );
            let summary = swaps.swap_summary(&review, None, None, None, None, cx);
            let (label, amount, _) = summary.receive_card_for_test().unwrap();
            assert_eq!(
                (label, amount),
                (
                    "Receive on Solana, at least".to_owned(),
                    non_evm_amount(&delivery, review.bridge().unwrap().destination_minimum)
                )
            );
            // Only 1Click's quote values the token, at its expected output's rate.
            assert_eq!(
                summary.receive_usd_for_test().as_deref(),
                Some("≈ $12.50 · 1Click estimate")
            );
            assert_eq!(
                summary.receiver_for_test(),
                Some((SOLANA_RECEIVER.to_owned(), Some("Phantom".to_owned())))
            );
            let rows = summary.rows_for_test();
            for row in [
                ("Bridge", "NEAR Intents · fee included"),
                ("Mint", STUB_SOLANA_SUSDC),
            ] {
                assert!(
                    rows.contains(&(row.0.to_owned(), row.1.to_owned())),
                    "{row:?} in {rows:?}"
                );
            }
            // The mint shows in the short form the form and the order detail give it.
            let mint = (
                "Mint".to_owned(),
                railgun_ui::short_non_evm(STUB_SOLANA_SUSDC),
            );
            let shown = summary.shown_rows_for_test();
            assert!(shown.contains(&mint), "{mint:?} in {shown:?}");
            let warnings = summary.warnings_for_test();
            assert!(
                warnings.contains(&format!(
                    "{UNVERIFIED_PRICE_WARNING} No independent price is known for sUSDC on Solana."
                )) && warnings
                    .iter()
                    .any(|warning| warning == NEAR_INTENTS_DISCLAIMER),
                "{warnings:?}"
            );
            assert_eq!(
                swaps.form.as_ref().unwrap().review_problem(&review),
                Some("Accept the unverified price before you review the swap.".into())
            );
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-provider-near-intents").is_some());
        assert!(cx.debug_bounds("swap-buy-contract").is_some());
    });
}

/// A non-EVM swap's history comes from its record: no 1Click client is set up, so no token
/// list can be fetched, and My orders and the detail still name the network, the receiver by
/// its saved label, the token and its amounts, and 1Click's Solana transaction as it reported
/// it.
#[gpui::test]
fn non_evm_history_renders_from_the_record_without_the_token_list(cx: &mut TestAppContext) {
    const SIGNATURE: &str =
        "2zf1D3JpcScRk9TTCWSQmbKr8hB9nfiLK9iA7mHvNuxqSrDDLCRFY7mb48txGysNHWns8autYb6meqyhRAPs23qT";
    use wallet_ops::vault::{NonEvmDelivery, SwapBridgeOutcome};
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                root.non_evm_address_book = vec![non_evm_entry(
                    "Phantom",
                    NonEvmNetwork::Solana,
                    SOLANA_RECEIVER,
                )];
            });
        });
        let delivery = NonEvmDelivery {
            network: NonEvmNetwork::Solana,
            receiver: SOLANA_RECEIVER.to_owned(),
            asset_id: "nep141:sol-JUP".to_owned(),
            contract: Some("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN".to_owned()),
            symbol: "JUP".to_owned(),
            decimals: 6,
        };
        let (uid, observed) = placed_swap(
            executors,
            operation,
            SwapDelivery::NonEvmBridge(delivery.clone()),
        );
        executors
            .record_swap_observations(
                operation,
                uid,
                bridge_observations(
                    observed,
                    None,
                    Some(SwapBridgeOutcome::DeliveredReported {
                        amount_out: Some(U256::from(181_250_000_u32)),
                        transaction_hash: None,
                        destination_transaction: Some(SIGNATURE.to_owned()),
                    }),
                ),
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                let stage = swaps.stage(record);
                assert_eq!(stage, SwapStage::Order(SwapOrderState::Done));
                let labels = swaps.labels(record, cx);
                // Its name gives the network, as its symbol can match the sold token's.
                assert!(
                    labels.pair.ends_with(" for JUP on Solana"),
                    "{}",
                    labels.pair
                );
                let bridge = labels.bridge.clone().unwrap();
                assert_eq!(
                    (
                        bridge.network.as_str(),
                        bridge.token.as_str(),
                        bridge.receiver.as_str(),
                        bridge.minimum.clone(),
                        labels.received,
                        bridge.destination_transaction.as_deref(),
                    ),
                    (
                        "Solana",
                        "JUP",
                        "Phantom",
                        Some(non_evm_amount(&delivery, BRIDGE_MINIMUM)),
                        // Amounts of 100 or more show no decimals.
                        Some("181 JUP".to_owned()),
                        Some(SIGNATURE),
                    )
                );
                let rows = swaps.order_rows_for_test(cx);
                assert!(
                    rows.iter().any(|(meta, shown)| {
                        // NEAR Intents' deliveries are all reported, so the status needn't say.
                        meta.ends_with(" · to Phantom on Solana · NEAR Intents")
                            && shown == "Delivered"
                    }),
                    "{rows:?}"
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        for selector in [
            "swap-bridge-facts",
            "swap-detail-receiver",
            "swap-destination-transaction",
            "swap-step-destination-transaction",
        ] {
            assert!(cx.debug_bounds(selector).is_some(), "{selector}");
        }
    });
}

/// Each attempt of a non-EVM swap shows its own approved minimum in the destination token, not
/// the intermediate the order buys.
#[gpui::test]
fn non_evm_attempts_show_their_destination_minimums(cx: &mut TestAppContext) {
    use wallet_ops::vault::{
        NonEvmDelivery, SwapObservation, SwapPreHookDeath, SwapPreHookDeathCause,
    };
    with_approved_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let delivery = NonEvmDelivery {
            network: NonEvmNetwork::Solana,
            receiver: SOLANA_RECEIVER.to_owned(),
            asset_id: "nep141:sol.omft.near".to_owned(),
            contract: None,
            symbol: "SOL".to_owned(),
            decimals: 9,
        };
        let (uid, observed) = placed_swap(
            executors,
            operation,
            SwapDelivery::NonEvmBridge(delivery.clone()),
        );
        let higher = BRIDGE_MINIMUM + U256::from(10_000_000);
        let expired = SwapPreHookDeath {
            cause: SwapPreHookDeathCause::Expired,
            observation: SwapObservation {
                block: observed.block(),
                transaction_hash: None,
            },
        };
        retried_and_traded(executors, operation, uid, observed, expired, |bounds| {
            bounds.destination_minimum = Some(higher);
        });
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                let latest = record.swap().unwrap().orders().last().unwrap();
                let attempts = model::SwapAttempts::ending_at(record, latest);
                let rows = swaps.attempt_rows(record, &attempts, cx);
                for (row, minimum) in rows.iter().zip([BRIDGE_MINIMUM, higher]) {
                    let expected = format!("Minimum {} · ", non_evm_amount(&delivery, minimum));
                    assert!(row.detail.starts_with(&expected), "{}", row.detail);
                }
                assert_eq!(rows.len(), 2);
            });
        });
    });
}
