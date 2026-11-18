use super::*;

/// A new swap of 1 USDC with the stub providers on its route and Polygon enabled with the
/// stub's RPC. Polygon's session is still loading, at 62% of indexing its UTXOs and 92% of the
/// whole sync, so opening the picker for Private balance starts no session.
fn open_form_beside_syncing_polygon(
    root: &Entity<WalletRoot>,
    swaps: &Entity<PrivateSwapsView>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) {
    let orderbook = stub_orderbook(stubs, runtime);
    cx.update(|window, cx| {
        root.update(cx, |root, _| {
            root.effective_token_registry = wallet_ops::settings::build_effective_token_registry(
                &wallet_ops::settings::WalletSettings::default(),
            )
            .unwrap();
            enable_stub_chain(root, stubs, 137);
            let mut progress = ChainSyncProgress::default();
            progress.observe(Some(wallet_ops::SyncProgressUpdate::new(
                wallet_ops::SyncProgressStage::IndexingUtxos,
                0,
                62,
                100,
            )));
            root.chain_states
                .insert(137, ChainUtxoState::Loading { progress });
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
            form.bridge_clients = Some(stub_bridge_clients(stubs, &orderbook));
            form.orderbook = Some(orderbook);
        });
        window.draw(cx).clear(cx);
    });
}

/// The picker's switch and the form's Receive to are one setting, and it decides which
/// networks can be picked. Switching the form to Private balance keeps a network picked for a
/// Public address: the form says under Receive to why it can't quote, and the picker still
/// shows that network while its sync is under way.
#[gpui::test]
fn buy_picker_switch_is_receive_to_and_a_kept_network_explains_itself(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_form_beside_syncing_polygon(root, swaps, &stubs, runtime, cx);
        let click = |selector: &'static str, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let bounds = cx.debug_bounds(selector).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        };
        click("swap-buy-selector", cx);
        swaps.read_with(cx, |swaps, cx| {
            assert!(swaps.form.as_ref().unwrap().picker.open);
            assert_eq!(
                picker_polygon(swaps, cx),
                NetworkAvailability::Syncing(Some(92)),
                "Private balance waits for Polygon's sync"
            );
        });

        // The picker's switch is the form's Receive to.
        click("swap-buy-picker-public", cx);
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(form.receive_to, ReceiveTo::PublicAddress);
            assert!(form.picker.open, "the switch keeps the picker open");
            assert_eq!(
                picker_polygon(swaps, cx),
                NetworkAvailability::Available,
                "a Public address needs only RPC endpoints"
            );
        });
        cx.update(|window, cx| {
            let picker = &swaps.read(cx).form.as_ref().unwrap().picker;
            assert!(
                picker
                    .networks
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx),
                "the Public switch moves the focus to the network search"
            );
        });

        // Pick USDT on Polygon: the network and the token arrive together.
        click("swap-buy-picker-network-137", cx);
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                form.bridge.routes.contains_key(&(STUB_USDC, 137))
            })
        });
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(
                (form.picker.network, form.network),
                (BuyNetworkKey::Chain(137), None)
            );
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    (form.network, form.buy, form.picker.open),
                    (Some(137), Some(STUB_POLYGON_USDT), false)
                );

                // The form's Receive to keeps them, and says why Private balance can't quote.
                swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    (form.network, form.buy),
                    (Some(137), Some(STUB_POLYGON_USDT))
                );
                assert!(matches!(
                    form.delivery,
                    Err(DeliveryProblem::Network { syncing: true, .. })
                ));
                assert!(form.quote_delivery().is_none());
                assert!(
                    matches!(form.quote, QuoteState::Idle) && form.quote_task.is_none(),
                    "nothing is quoted, so Review stays unavailable"
                );
            });
            window.draw(cx).clear(cx);
        });
        // The sync's progress shows where the quote will, not as a problem under Receive to.
        assert!(cx.debug_bounds("swap-destination-progress").is_some());
        assert!(cx.debug_bounds("swap-receive-to-problem").is_none());

        // The picker follows the form's Receive to, and shows the form's network.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_buy_picker(window, cx);
                let form = swaps.form.as_ref().unwrap();
                let content = swaps.buy_picker_content(form, cx);
                assert_eq!(content.receive_to, ReceiveTo::PrivateBalance);
                assert_eq!(
                    (content.network, form.network),
                    (BuyNetworkKey::Chain(137), Some(137))
                );
                assert_eq!(
                    picker_polygon(swaps, cx),
                    NetworkAvailability::Syncing(Some(92))
                );
            });
        });
    });
}

/// Each list picks through its selection. Enter in the network search shows the selected
/// network's tokens, and typing there doesn't change the shown network. A network the search
/// hides can't be picked. Enter in the token search picks the selected token.
#[gpui::test]
fn buy_picker_lists_pick_their_selection(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        let (tokens, networks) = swaps.read_with(cx, |swaps, _| {
            let picker = &swaps.form.as_ref().unwrap().picker;
            (picker.tokens.clone(), picker.networks.clone())
        });
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
        };
        let shown = |cx: &mut gpui::VisualTestContext| {
            swaps.read_with(cx, |swaps, _| {
                swaps
                    .form
                    .as_ref()
                    .unwrap()
                    .picker
                    .network
                    .chain_id()
                    .unwrap()
            })
        };
        let listed = |cx: &mut gpui::VisualTestContext| {
            draw(cx);
            networks.read_with(cx, |list, _| {
                list.delegate()
                    .listed()
                    .filter_map(BuyNetworkKey::chain_id)
                    .collect::<Vec<_>>()
            })
        };
        draw(cx);
        cx.update(|window, cx| networks.read(cx).focus_handle(cx).focus(window, cx));
        draw(cx);
        assert_eq!(shown(cx), 137);
        cx.simulate_input("ETH");
        assert_eq!(listed(cx), [1]);
        assert_eq!(shown(cx), 137, "typing doesn't change the shown network");
        cx.simulate_keystrokes("enter");
        assert_eq!(shown(cx), 1);

        // Nothing matches: no rows, and Enter does nothing.
        cx.update(|window, cx| {
            networks.update(cx, |list, cx| list.set_query("POLYGONE", window, cx));
        });
        assert_eq!(listed(cx), Vec::<u64>::new());
        cx.simulate_keystrokes("enter");
        assert_eq!(shown(cx), 1);

        // Polygon is listed again, under Ethereum, the swap's own network.
        cx.update(|window, cx| {
            networks.update(cx, |list, cx| {
                list.set_query("", window, cx);
                list.set_selected_index(Some(gpui_component::IndexPath::new(1)), window, cx);
            });
        });
        assert_eq!(listed(cx), [1, 137]);
        cx.simulate_keystrokes("enter");
        assert_eq!(shown(cx), 137);

        // A search matching a non-EVM network alone selects it, not a heading, so Enter shows it.
        cx.update(|window, cx| {
            networks.update(cx, |list, cx| list.set_query("sol", window, cx));
        });
        draw(cx);
        cx.simulate_keystrokes("enter");
        assert_eq!(
            swaps.read_with(cx, |swaps, _| swaps.form.as_ref().unwrap().picker.network),
            BuyNetworkKey::NonEvm(NonEvmNetwork::Solana)
        );

        // POL, USDC, then USDT.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.show_buy_picker_network(137, window, cx);
            });
        });
        draw(cx);
        cx.update(|window, cx| {
            tokens.update(cx, |list, cx| {
                list.set_selected_index(Some(gpui_component::IndexPath::new(2)), window, cx);
            });
            tokens.read(cx).focus_handle(cx).focus(window, cx);
        });
        draw(cx);
        cx.simulate_keystrokes("enter");
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(
                (form.network, form.buy, form.picker.open),
                (Some(137), Some(STUB_POLYGON_USDT), false)
            );
        });
    });
}

/// An Across review names the destination, provider, bridge fee and surplus, delivers exactly
/// the deposit's output on Polygon, and discloses the cross-chain link. The confirm-only step
/// repeats the terms the approval binds, with the approved minimum and no bridge fee.
#[gpui::test]
fn across_review_and_confirm_step_show_the_bound_destination_terms(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, operation, runtime, cx| {
            open_bridge_form(root, swaps, &stubs, runtime, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            let bridge = *review.bridge().unwrap();
            let receiver = Address::repeat_byte(4).to_checksum(None);
            swaps.read_with(cx, |swaps, cx| {
                assert_eq!(
                    swaps.bridge_total_usd_value(&review, cx),
                    None,
                    "missing destination prices must not reuse the source quote's dollars"
                );
            });
            root.read_with(cx, |root, _| {
                let cache = &root.public_broadcaster_anchor_cache;
                cache.store_rate(1, STUB_USDT, U256::from(3_000_000_000_u64));
                cache.store_native_usd_rate(1, U256::from(3_000_000_000_u64), 18);
            });
            swaps.read_with(cx, |swaps, cx| {
                // Across delivers the same asset. Its fixed deposit less the fee can be
                // valued on the source chain even before destination prices are loaded.
                let payout = review.suggested_private_minimum() - bridge.fee.unwrap();
                assert_eq!(swaps.bridge_usd_value(&review, cx), Some(payout));
                assert_eq!(
                    swaps.bridge_total_usd_value(&review, cx),
                    Some(payout + review.estimated_source_surplus().unwrap())
                );
                // The Minimum field shows a destination amount, which has no fallback.
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    swaps.strip_usd_label(form, &review, review.suggested_private_minimum(), cx),
                    None
                );
            });
            root.read_with(cx, |root, _| {
                let cache = &root.public_broadcaster_anchor_cache;
                // A destination price, when present, takes precedence over the fallback.
                cache.store_rate(137, STUB_POLYGON_USDT, U256::from(500_000));
                cache.store_native_usd_rate(137, U256::from(1_000_000), 18);
            });
            // The stub's 0.1% bridge fee leaves the costs low.
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert!(cx.debug_bounds("swap-high-costs").is_none());
            let source = cx.debug_bounds("swap-source-return").unwrap();
            let total = cx.debug_bounds("swap-total-received").unwrap();
            assert!(total.left() >= source.left() && total.right() <= source.right());
            assert!(total.bottom() <= source.bottom());
            assert!(cx.debug_bounds("swap-destination-usd").is_some());
            swaps.read_with(cx, |swaps, cx| {
                let received = |summary: &SpendAuthorizationSummary| {
                    summary
                        .receive_card_for_test()
                        .map(|(label, amount, _)| (label, amount))
                };
                let exactly = |amount| {
                    Some((
                        "Receive on Polygon, exactly".to_owned(),
                        swaps.network_token_amount(137, STUB_POLYGON_USDT, amount, cx),
                    ))
                };
                let surplus = review.estimated_source_surplus().unwrap();
                let destination_usd = bridge.expected_output * U256::from(2);
                let total_usd = destination_usd + surplus;
                assert_eq!(swaps.bridge_usd_value(&review, cx), Some(destination_usd));
                assert_eq!(swaps.bridge_total_usd_value(&review, cx), Some(total_usd));
                // The Minimum field's dollars are its destination amount's, after the bridge
                // fee, also for a share whose bridge leg isn't quoted yet.
                let form = swaps.form.as_ref().unwrap();
                let pending = review.with_gas_share(5_000).unwrap();
                assert!(pending.bridge().is_none());
                for (minimum, destination) in [
                    (review.suggested_private_minimum(), bridge.destination_minimum),
                    (
                        pending.suggested_private_minimum(),
                        StripScale::of(&review).show(pending.suggested_private_minimum()),
                    ),
                ] {
                    assert_eq!(
                        swaps.strip_usd_label(form, &review, minimum, cx),
                        Some(format!(
                            "≈ {}",
                            railgun_ui::format_usd_micro_value(destination * U256::from(2))
                        ))
                    );
                }
                let summary = swaps.swap_summary(&review, None, None, None, None, cx);
                assert_eq!(
                    received(&summary),
                    exactly(bridge.destination_minimum),
                    "Across delivers exactly its output"
                );
                assert_eq!(
                    summary.send_card_for_test().map(|(label, _)| label),
                    Some(format!("Sell on {}", network_name(swaps.origin_chain_id))),
                    "a swap to another network names its own"
                );
                assert_eq!(
                    summary.card_networks_for_test(),
                    Some([Some(swaps.origin_chain_id), Some(137)]),
                    "each card's icon carries its own network"
                );
                assert_eq!(
                    summary.receiver_for_test().map(|(address, _)| address),
                    Some(receiver.clone())
                );
                assert_eq!(
                    summary.rows_for_test()[..3],
                    [
                        gas_row(swaps, &review, cx),
                        (
                            "Returned".to_owned(),
                            swaps.with_usd(format!("≈ {}", swaps.token_amount(STUB_USDT, surplus, cx)),
                                STUB_USDT, surplus, cx)
                        ),
                        (
                            "Bridge".to_owned(),
                            format!(
                                "Across · fee {}",
                                swaps.token_amount(STUB_USDT, bridge.fee.unwrap(), cx)
                            )
                        ),
                    ]
                );
                let deposit = swaps.token_amount(STUB_USDT, review.suggested_private_minimum(), cx);
                let details = summary.details_for_test();
                let unshield = review.plan().amount() - review.sell_amount();
                for row in [
                    ("Railgun unshield", swaps.token_amount(STUB_USDC, unshield, cx)),
                    ("Price tolerance", format_bps_percent(u64::from(DEFAULT_SLIPPAGE_BPS))),
                    ("Deposit to Across", format!("{deposit} on Ethereum")),
                    // The bridge quote needs the profile's window.
                    ("Order valid for", "10 minutes".to_owned()),
                ] {
                    assert!(
                        details.contains(&(row.0.to_owned(), row.1)),
                        "{details:?}"
                    );
                }
                // The surplus choice and the total are behind the Returned row's info button,
                // and the refund path behind the Bridge row's.
                let returned = summary.row_hint_for_test("Returned").unwrap();
                assert!(
                    returned.contains(&format!(
                        "above the {deposit} deposit is reshielded to your private balance"
                    )) && returned.ends_with(&format!(
                        "Estimated total received ≈ {}",
                        railgun_ui::format_usd_micro_value(total_usd)
                    )),
                    "{returned}"
                );
                let terms = summary.row_hint_for_test("Bridge").unwrap();
                assert!(
                    terms.contains(&format!(
                        "If the deposit isn't filled before it expires, Across refunds the {deposit} to the stealth account on Ethereum"
                    )),
                    "{terms}"
                );
                let (_, public, _) = summary.disclosure_for_test().unwrap();
                assert!(
                    public.contains("The deposit names the receiver, Polygon and the amounts, so the receiver's funds on Polygon can be traced to this swap."),
                    "{public}"
                );
                assert!(!public.contains(EXTERNAL_DELIVERY_DISCLOSURE));
                // A Bridge order signs its deposit, which the Deposit row already shows.
                assert!(details.iter().all(|(label, _)| label != "Signed minimum"));
                assert_eq!(summary.card_deltas_for_test(), [None, None]);
                assert_eq!(summary.row_delta_for_test("Gas"), None);

                // Reopened against a saved approval that bound a higher destination minimum
                // and allowed less gas, both amounts show an adverse change.
                let mut saved = test_approval().bounds;
                saved.unshield_amount = Some(review.plan().amount());
                saved.destination_minimum = Some(bridge.destination_minimum * U256::from(2));
                saved.gas_allowance = Some(review.gas_allowance() / U256::from(2));
                let reopened = swaps.swap_summary(
                    &review,
                    None,
                    Some(SwapReviewChange::DestinationMinimum {
                        approved: bridge.destination_minimum * U256::from(2),
                        current: bridge.destination_minimum,
                    }),
                    Some(&saved),
                    None,
                    cx,
                );
                let adverse = |delta: Option<(String, bool)>| delta.map(|(_, adverse)| adverse);
                let [sell, receive] = reopened.card_deltas_for_test();
                assert_eq!(sell, None, "the same sell amount");
                assert_eq!(adverse(receive), Some(true));
                assert_eq!(adverse(reopened.row_delta_for_test("Gas")), Some(true));

                // The approval saved with the setup binds a lower minimum than this requote.
                let approved = bridge.destination_minimum / U256::from(2);
                let approval = OrderApproval {
                    operation,
                    review: Arc::clone(&review),
                    private_minimum: review.suggested_private_minimum(),
                    price_acknowledged: true,
                    orderbook: swaps.form.as_ref().unwrap().orderbook.clone().unwrap(),
                    bridge: None,
                    destination_minimum: Some(approved),
                    continuing_minimum: None,
                    full_review: false,
                    swap_use: None,
                    pair_destination: None,
                    speed_up: None,
                };
                let place = swaps.place_summary(&approval, cx).unwrap();
                assert_eq!(received(&place), exactly(approved), "the bound minimum");
                assert_eq!(
                    place.receiver_for_test().map(|(address, _)| address),
                    Some(receiver)
                );
                assert_eq!(
                    place.rows_for_test(),
                    [("Bridge".to_owned(), "Across".to_owned())],
                    "without a bridge fee"
                );
                assert!(
                    place
                        .row_hint_for_test("Bridge")
                        .is_some_and(|terms| terms.contains("surplus is reshielded on Ethereum"))
                );
            });
        },
    );
}
