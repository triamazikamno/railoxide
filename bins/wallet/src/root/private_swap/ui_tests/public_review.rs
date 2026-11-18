use super::*;

#[gpui::test]
fn public_amounts_use_selected_balance_metadata_through_review_max_and_restore(
    cx: &mut TestAppContext,
) {
    use gpui_kit::test::TestWindowExt;
    use wallet_ops::vault::{PublicSwapClaim, SwapAccountChoice, SwapApprovedAccount};

    let stubs = SwapStubs::start();
    stubs.enable_public_reviews();
    let sell = Address::repeat_byte(42);
    let entered = U256::from(7_000_000_000_000_000_000_u64);
    let available = U256::from(10_000_000_000_000_000_123_u64);
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    enable_stub_chain(root, &stubs, 1);
                    enable_stub_chain(root, &stubs, 137);
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    assert!(root.effective_token_registry.get(1, &sell).is_none());
                    configure_public_review_assets(root);
                    for account in
                        &mut Arc::make_mut(root.public_balance_snapshot.as_mut().unwrap()).accounts
                    {
                        for balance in &mut account.balances {
                            if balance.asset.id == wallet_ops::PublicAssetId::Erc20(STUB_USDC) {
                                balance.asset.id = wallet_ops::PublicAssetId::Erc20(sell);
                                balance.asset.symbol = "SNAP".into();
                                balance.asset.decimals = 18;
                                balance.amount =
                                    wallet_ops::PublicBalanceAmount::Available(available);
                            }
                        }
                    }
                });
            });
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let (destination_store, operation) = reusable_polygon_account(root, cx);
            let destination = destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == operation)
                .unwrap();
            let account = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(account, sell, window, cx);
                    swaps
                        .form
                        .as_ref()
                        .unwrap()
                        .amount_input
                        .update(cx, |input, cx| {
                            input.set_value("7", window, cx);
                        });
                });
            });
            choose_swap_source(cx, "swap-source-public-account-2");
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.sell, sell);
                assert_eq!(form.amount_input.read(cx).value().as_ref(), "7");
                assert_eq!(swaps.form_amount(form, cx).unwrap(), entered);
                assert_eq!(swaps.form_sell_amount(form, entered, cx), "7 SNAP");
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let max = cx.debug_bounds("swap-amount-max").unwrap();
            cx.simulate_click(max.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(swaps.form_amount(form, cx).unwrap(), available);
                assert_eq!(
                    form.amount_input.read(cx).value().as_ref(),
                    "10.000000000000000123"
                );
            });
            choose_swap_source(cx, "swap-source-public-account-1");
            let (review, across, orderbook, route) =
                public_review_fixture(root, &polygon, &stubs, runtime, sell, entered, false, cx);
            assert_eq!(review.sell_amount(), entered);
            assert!(across_fee_amounts(&stubs).contains(&entered));
            let approval = review
                .approval(
                    SwapApprovedAccount {
                        address: destination.address(),
                        setup: false,
                    },
                    None,
                    true,
                )
                .unwrap();
            assert_eq!(approval.bounds.sell_amount, entered);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.use_amount(entered, window, cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.network = Some(137);
                    form.buy = Some(STUB_POLYGON_USDC);
                    form.destination_account = Some(DestinationAccount {
                        chain_id: 137,
                        operation,
                        index: destination.index(),
                        address: destination.address().unwrap(),
                    });
                    form.public
                        .as_mut()
                        .unwrap()
                        .routes
                        .insert((sell, 137), vec![route]);
                    swaps.install_public_review_for_tests(review, across, Some(orderbook), cx);
                    let form = swaps.form.as_mut().unwrap();
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    form.blocked_shield_acknowledged = true;
                    swaps.request_public_review(window, cx);
                });
            });
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, _| {
                assert_eq!(
                    swaps
                        .public_authorization
                        .as_ref()
                        .unwrap()
                        .public_authorization_summary()
                        .send_card_for_test(),
                    Some((format!("Send on {}", network_name(1)), "7 SNAP".into()))
                );
            });
            cx.update(|window, cx| {
                window.click(SharedString::from("wallet-spend-auth-cancel"), cx);
            });
            cx.run_until_parked();
            let account = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let id = SwapUseId::random().unwrap();
            destination_store
                .claim_public_swap(PublicSwapClaim {
                    id,
                    origin_chain: 1,
                    source: account.address,
                    source_scope: account.scope,
                    account: SwapAccountChoice::Existing(operation),
                    delegate: destination.delegate(),
                    destination_token: STUB_POLYGON_USDC,
                    bridged_token: sell,
                    order: false,
                    approval,
                    now: now_unix(),
                })
                .unwrap();
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    assert!(swaps.public_authorization.is_none());
                    swaps.refresh_public_swap_records(cx);
                    swaps.perform_public_swap_action(
                        model::SwapIdentity {
                            operation,
                            swap_use: id,
                        },
                        crate::root::private_swap::public_progress::PublicSwapAction::Continue,
                        window,
                        cx,
                    );
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(form.amount_input.read(cx).value().as_ref(), "7");
                    assert_eq!(swaps.form_amount(form, cx).unwrap(), entered);
                    assert_eq!(swaps.form_sell_amount(form, entered, cx), "7 SNAP");
                    swaps.close_swap_dialog(window, cx);
                });
            });
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// The form and the review of a swap paid from a Public account, for a same-token deposit and
/// for an order, both to an existing destination account. The form's Buy card and details
/// follow the path. The review is the compact one: the network chip, the account on the Sell
/// or Send card, a Pay now row with the account's gas and what it sends, and the reused
/// account, its address shortened. An order's terms name the proxy it goes through. Only the
/// deposit, which waits on nothing, has no stepper.
#[gpui::test]
fn public_form_and_review_follow_the_path_and_name_the_paying_account(cx: &mut TestAppContext) {
    use gpui_kit::test::TestWindowExt;
    let stubs = SwapStubs::start();
    stubs.enable_public_reviews();
    with_swap_view_and_store(
        cx,
        Some(stubs.rpc()),
        |root, swaps, _, _, runtime, store, cx| {
            cx.update(|_, cx| {
                root.update(cx, |root, _| {
                    enable_stub_chain(root, &stubs, 1);
                    enable_stub_chain(root, &stubs, 137);
                    root.effective_token_registry =
                        wallet_ops::settings::build_effective_token_registry(
                            &wallet_ops::settings::WalletSettings::default(),
                        )
                        .unwrap();
                    configure_public_review_assets(root);
                });
            });
            let polygon = start_polygon_session(root, &stubs, runtime, store, cx);
            let (destination_store, destination_operation) = reusable_polygon_account(root, cx);
            let destination_record = destination_store
                .records()
                .unwrap()
                .into_iter()
                .find(|record| record.operation() == destination_operation)
                .unwrap();
            let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
            let label = public_source_label(&source);
            let (origin, destination) = (network_name(1), network_name(137));
            let reused = format!(
                "Reuses #{} · {}",
                destination_record.index(),
                spend_authorization_recipient_display(
                    &destination_record.address().unwrap().to_checksum(None)
                )
            );
            // A form started on the native asset suggests wrapping before a token is picked, and
            // the suggestion alone doesn't keep Review unavailable.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_public_form(source.clone(), Address::ZERO, window, cx);
                    swaps.stub_public_orders_for_tests(true, cx);
                    let reason = swaps
                        .public_form_reason(swaps.form.as_ref().unwrap(), cx)
                        .expect("a native Sell asset suggests wrapping");
                    assert!(!reason.blocks_review);
                    assert!(
                        reason
                            .text
                            .contains(" can't be swapped from a Public account. Wrap it first"),
                        "{}",
                        reason.text
                    );
                    swaps.close_swap_dialog(window, cx);
                });
            });
            cx.run_until_parked();
            for order in [false, true] {
                let sell = if order { STUB_USDT } else { STUB_USDC };
                let (review, across, orderbook, route) = public_review_fixture(
                    root,
                    &polygon,
                    &stubs,
                    runtime,
                    sell,
                    U256::from(7_000_000),
                    order,
                    cx,
                );
                assert_eq!(review.intent().order, order);
                assert!(
                    !review.gas_plan().max_gas_cost.is_zero(),
                    "the review accounts for gas paid by the Public account"
                );
                let proxy = review.proxy();
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.open_public_form(source.clone(), sell, window, cx);
                        swaps.stub_public_orders_for_tests(true, cx);
                        let fresh = swaps.form.as_ref().unwrap();
                        assert!(
                            receive_to_problem(fresh).is_none()
                                && swaps.public_form_reason(fresh, cx).is_none(),
                            "an untouched form prompts for a network without an error"
                        );
                        let form = swaps.form.as_mut().unwrap();
                        assert_eq!(
                            form.sell, sell,
                            "each scenario opens a fresh source-token draft"
                        );
                        form.network = Some(137);
                        form.buy = Some(STUB_POLYGON_USDC);
                        form.amount_input
                            .update(cx, |input, cx| input.set_value("7", window, cx));
                        form.destination_account = Some(DestinationAccount {
                            chain_id: 137,
                            operation: destination_operation,
                            index: destination_record.index(),
                            address: destination_record.address().unwrap(),
                        });
                        form.public
                            .as_mut()
                            .unwrap()
                            .routes
                            .insert((sell, 137), vec![route]);
                        swaps.install_public_review_for_tests(
                            review.clone(),
                            across,
                            Some(orderbook),
                            cx,
                        );
                        let form = swaps.form.as_ref().unwrap();
                        assert_eq!(
                            PrivateSwapsView::public_buy_label(form, true),
                            if order {
                                format!("Buy on {destination}, at least")
                            } else {
                                format!("Receive on {destination}")
                            }
                        );
                        let details = swaps
                            .public_details(form, &review, cx)
                            .into_iter()
                            .map(|row| row.label)
                            .collect::<Vec<_>>();
                        let bridge_costs = [
                            "Across fee".to_owned(),
                            format!("Delivery on {destination}"),
                            format!("Railgun shield on {destination}"),
                        ];
                        if order {
                            // The stubbed pair has no anchors, so there is no price check row.
                            assert!(
                                details[0].starts_with("Gas at ") && details[0].ends_with(" gwei")
                            );
                            assert_eq!(details[1], "Gas you pay");
                            assert_eq!(details[2..5], bridge_costs);
                            assert_eq!(
                                details[5..],
                                [
                                    format!("Approval from {label}"),
                                    "Price tolerance".to_owned()
                                ]
                            );
                        } else {
                            assert_eq!(details[..3], bridge_costs);
                            assert_eq!(
                                details[3..],
                                [format!("Approval and deposit from {label}")],
                                "a deposit has no order, so no gas share, price or tolerance rows"
                            );
                        }
                        let form = swaps.form.as_mut().unwrap();
                        form.price_acknowledged = true;
                        form.blocked_shield_acknowledged = true;
                        swaps.request_public_review(window, cx);
                        assert!(
                            swaps.public_authorization.is_some(),
                            "{:?}",
                            swaps.form.as_ref().unwrap().error
                        );
                    });
                });
                cx.run_until_parked();
                let summary = swaps.read_with(cx, |swaps, _| {
                    swaps
                        .public_authorization
                        .as_ref()
                        .unwrap()
                        .public_authorization_summary()
                });
                assert!(summary.compact_rows_for_test());
                assert_eq!(
                    summary.title_for_test(),
                    (
                        if order {
                            "Swap to private balance".to_owned()
                        } else {
                            format!("Shield on {destination}")
                        },
                        None
                    ),
                    "an existing account's review claims no setup"
                );
                assert_eq!(
                    summary.send_card_for_test().unwrap().0,
                    format!("{} on {origin}", if order { "Sell" } else { "Send" })
                );
                assert_eq!(
                    summary.card_networks_for_test(),
                    Some([Some(1), Some(137)]),
                    "each card's icon carries its own network"
                );
                assert_eq!(
                    summary.send_account_for_test(),
                    Some((label.clone(), source.address.to_checksum(None)))
                );
                let (receive, _, lines) = summary.receive_card_for_test().unwrap();
                assert_eq!(
                    receive,
                    if order {
                        format!("Receive on {destination}, at least")
                    } else {
                        format!("Receive on {destination}")
                    }
                );
                // An order's card shows its best case too, in the destination token.
                let best = swaps.read_with(cx, |swaps, cx| {
                    review.best_case().map(|best| {
                        format!(
                            "up to {} if solvers pay all gas",
                            swaps.network_bare_amount(137, STUB_POLYGON_USDC, best, cx)
                        )
                    })
                });
                assert_eq!(best.is_some(), order, "only an order has a best case");
                assert_eq!(
                    lines,
                    best.into_iter()
                        .chain(["to your private balance".to_owned()])
                        .collect::<Vec<_>>()
                );
                let rows = summary.shown_rows_for_test();
                let labels = rows
                    .iter()
                    .map(|(label, _)| label.as_str())
                    .collect::<Vec<_>>();
                let account = format!("Account on {destination}");
                // The account's gas and what it sends, with the account named behind the info
                // button.
                let max_gas = swaps.read_with(cx, |swaps, cx| {
                    swaps.token_amount(Address::ZERO, review.gas_plan().max_gas_cost, cx)
                });
                let sends = rows[0]
                    .1
                    .strip_prefix(&format!("up to {max_gas} · "))
                    .unwrap_or_else(|| panic!("{}", rows[0].1));
                assert!(
                    if order {
                        matches!(sends, "one approval" | "two approvals")
                    } else {
                        sends.ends_with("deposit")
                    },
                    "{sends}"
                );
                let pays = summary.row_hint_for_test("Pay now").unwrap();
                assert!(pays.starts_with(&format!("{label} sends ")), "{pays}");
                // The cost rows sit under one collapsed line, which says the account's gas
                // isn't refunded.
                let (title, collapsed, costs) = summary.row_group_for_test().unwrap();
                assert_eq!(title, "Costs");
                assert!(
                    collapsed.ends_with(&format!("up to {max_gas} now, not refunded")),
                    "{collapsed}"
                );
                if order {
                    assert_eq!(costs, ["Pay now", "Gas", "Bridge"]);
                    assert_eq!(
                        labels,
                        [
                            "Pay now",
                            "Gas",
                            "Bridge",
                            account.as_str(),
                            "If the shield fails"
                        ]
                    );
                    assert!(
                        summary.details_for_test().contains(&(
                            "Your CoW proxy".to_owned(),
                            proxy.unwrap().to_checksum(None)
                        )),
                        "the order's terms show the proxy's address in full"
                    );
                    assert!(summary.details_note_for_test().is_some_and(|note| {
                        note.contains(&format!("only {label} controls"))
                            && note.contains("stays in the proxy")
                    }));
                    assert_eq!(
                        summary.steps_for_test(),
                        Some((
                            2,
                            vec![
                                format!("Account on {destination} ready"),
                                "Approve and place order".to_owned()
                            ]
                        ))
                    );
                } else {
                    assert_eq!(costs, ["Pay now", "Bridge"]);
                    assert_eq!(
                        labels,
                        ["Pay now", "Bridge", account.as_str(), "If the shield fails"]
                    );
                    assert!(
                        summary.details_for_test().is_empty(),
                        "a deposit has no order terms and no proxy"
                    );
                    assert!(
                        summary.steps_for_test().is_none(),
                        "a direct deposit into an existing account uses a single review"
                    );
                }
                assert_eq!(rows[labels.len() - 2].1, reused);
                assert_eq!(rows[labels.len() - 1].1, format!("Refund to {label}"));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert_eq!(
                    cx.debug_bounds("wallet-spend-auth-steps-hint").is_some(),
                    order,
                    "request_spend_authorization renders the review's step choice"
                );
                cx.update(|window, cx| {
                    window.click(SharedString::from("wallet-spend-auth-cancel"), cx);
                });
                cx.run_until_parked();
                swaps.read_with(cx, |swaps, _| {
                    assert!(
                        swaps.public_authorization.is_none(),
                        "Cancel releases the reviewed authority before another scenario"
                    );
                });
                if order {
                    public_order_setup_review_and_signatures(
                        swaps,
                        &source,
                        destination_operation,
                        &review,
                        cx,
                    );
                    // Last, as it empties the account: one that can't pay its own gas has the
                    // form say how much it needs in the Sell card, and Review is unavailable.
                    cx.update(|window, cx| {
                        root.update(cx, |root, _| {
                            let snapshot =
                                Arc::make_mut(root.public_balance_snapshot.as_mut().unwrap());
                            for balance in snapshot
                                .accounts
                                .iter_mut()
                                .flat_map(|account| &mut account.balances)
                            {
                                if balance.asset.id == wallet_ops::PublicAssetId::Native {
                                    balance.amount =
                                        wallet_ops::PublicBalanceAmount::Available(U256::ZERO);
                                }
                            }
                        });
                        swaps.update(cx, |swaps, cx| {
                            let reason = swaps
                                .public_form_reason(swaps.form.as_ref().unwrap(), cx)
                                .expect("a gas shortfall is a reason");
                            assert!(reason.blocks_review);
                            assert!(
                                reason.text.starts_with(&format!("{label} needs "))
                                    && reason
                                        .text
                                        .contains(&format!(" on {origin} for gas and has ")),
                                "{}",
                                reason.text
                            );
                            swaps.request_public_review(window, cx);
                            assert!(swaps.public_authorization.is_none());
                            cx.notify();
                        });
                        window.draw(cx).clear(cx);
                    });
                    let pay_from = cx.debug_bounds("swap-pay-from").unwrap();
                    let reason = cx.debug_bounds("swap-public-reason").unwrap();
                    let buy = cx.debug_bounds("swap-buy-panel").unwrap();
                    assert!(
                        reason.top() > pay_from.bottom() && reason.bottom() <= buy.top(),
                        "the reason sits in the Sell card, above the Buy card"
                    );
                    assert!(
                        cx.debug_bounds("swap-price-status").is_some(),
                        "the Buy card shows the order's best case under its minimum"
                    );

                    // A preset prices the reviewed quote at its share and previews the bridge
                    // leg again: the review stays, and CoW isn't asked for another quote.
                    let quotes = stubs.quotes().len();
                    cx.update(|window, cx| {
                        swaps.update(cx, |swaps, cx| {
                            let reviewed = |swaps: &PrivateSwapsView| {
                                let form = swaps.form.as_ref().unwrap();
                                form.public.as_ref().unwrap().review.clone().unwrap()
                            };
                            let before = reviewed(swaps);
                            swaps.set_gas_preset(GasPreset::Tight, window, cx);
                            assert!(Arc::ptr_eq(&reviewed(swaps), &before));
                            assert_eq!(
                                public_strip(swaps.form.as_ref().unwrap())
                                    .unwrap()
                                    .share_bps,
                                GAS_SHARE_TIGHT_BPS
                            );
                        });
                    });
                    assert_eq!(stubs.quotes().len(), quotes);

                    // Another open swap of the account that buys the same token is named
                    // under the Sell card before Review is clicked, and keeps it unavailable.
                    polygon
                        .executor_owner()
                        .unwrap()
                        .claim_public_swap(wallet_ops::PublicSwapUseClaim {
                            id: SwapUseId::random().unwrap(),
                            origin_chain: 1,
                            source: source.address,
                            source_scope: source.scope.clone(),
                            account: SwapAccountChoice::New(ExecutorOperationId::random().unwrap()),
                            destination_token: STUB_POLYGON_USDC,
                            intent: review.intent(),
                            approval: review
                                .approval(
                                    SwapApprovedAccount {
                                        address: None,
                                        setup: true,
                                    },
                                    Some(SwapSetupFeeLimit {
                                        token: STUB_POLYGON_USDC,
                                        amount: U256::from(50_000),
                                    }),
                                    true,
                                )
                                .unwrap(),
                        })
                        .unwrap();
                    cx.update(|window, cx| {
                        swaps.update(cx, |swaps, cx| {
                            swaps.refresh_public_swap_records(cx);
                            let reason = swaps
                                .public_form_reason(swaps.form.as_ref().unwrap(), cx)
                                .expect("an open swap is a reason");
                            assert!(reason.blocks_review);
                            assert!(
                                reason.text.starts_with(&format!(
                                    "{label} already has an open swap that buys "
                                )),
                                "{}",
                                reason.text
                            );
                            swaps.request_public_review(window, cx);
                            assert!(swaps.public_authorization.is_none());
                            cx.notify();
                        });
                        window.draw(cx).clear(cx);
                    });
                    assert!(
                        cx.debug_bounds("swap-public-open-swap").is_some(),
                        "the reason offers to open the swap it names"
                    );

                    // A token the routes don't deliver, and a network whose private balance
                    // isn't synced, each say why and keep Review unavailable.
                    cx.update(|_, cx| {
                        swaps.update(cx, |swaps, cx| {
                            for (network, expected) in [
                                (137, "Across doesn't deliver "),
                                (42_161, "Private balance needs "),
                            ] {
                                let form = swaps.form.as_mut().unwrap();
                                form.network = Some(network);
                                form.buy = Some(Address::repeat_byte(0x55));
                                let reason = swaps
                                    .public_form_reason(swaps.form.as_ref().unwrap(), cx)
                                    .expect("a pair that can't be quoted has a reason");
                                assert!(reason.blocks_review, "{}", reason.text);
                                assert!(
                                    reason.text.starts_with(expected)
                                        && reason.text.contains(&network_name(network)),
                                    "{}",
                                    reason.text
                                );
                            }
                        });
                    });
                }
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| swaps.close_swap_dialog(window, cx));
                });
                cx.run_until_parked();
            }
            runtime.block_on(polygon.stop()).unwrap();
        },
    );
}

/// An order to a new destination account, on the open form of an order: its review has the
/// mockup's two steps, and one Pay now row for the setup fee and the account's gas, whose hint
/// names the network the fee is paid on and what the account sends. A hardware account
/// then sees what its device will sign as two groups of decoded, formatted terms, and as
/// three when the order carries a permit, whose group comes first.
fn public_order_setup_review_and_signatures(
    swaps: &Entity<PrivateSwapsView>,
    source: &wallet_ops::vault::PublicAccountMetadata,
    destination: ExecutorOperationId,
    review: &wallet_ops::PublicSwapReview,
    cx: &gpui::VisualTestContext,
) {
    let network = network_name(137);
    let label = public_source_label(source);
    let proxy = review.proxy().unwrap();
    let bought = review.buy_amount().unwrap();
    swaps.read_with(cx, |swaps, cx| {
        let form = swaps.form.as_ref().unwrap();
        let approval = review
            .approval(
                SwapApprovedAccount {
                    address: None,
                    setup: true,
                },
                Some(SwapSetupFeeLimit {
                    token: STUB_POLYGON_USDC,
                    amount: U256::from(50_000),
                }),
                true,
            )
            .unwrap();
        let fee = SetupFee {
            chain_id: 137,
            token: STUB_POLYGON_USDC,
            maximum: U256::from(50_000),
            broadcaster: "0zk1…test".to_owned(),
        };
        let summary = swaps.public_review_summary(form, review, &approval, Some(&fee), cx);
        assert_eq!(summary.title_for_test().0, "Set up account and swap");
        assert_eq!(
            summary.steps_for_test(),
            Some((
                1,
                vec![
                    format!("Set up account on {network}"),
                    "Approve and place order".to_owned()
                ]
            ))
        );
        let (pay_now, value) = summary.rows_for_test().remove(0);
        assert_eq!(pay_now, "Pay now");
        let fees = value
            .strip_prefix("up to ")
            .and_then(|value| value.strip_suffix(" · not refunded"))
            .and_then(|fees| fees.split_once(" + "))
            .unwrap_or_else(|| panic!("{value}"));
        assert!(!fees.0.is_empty(), "{value}");
        assert_eq!(
            fees.1,
            swaps.token_amount(Address::ZERO, approval.max_gas_cost, cx)
        );
        // The collapsed Costs line names the same amounts as not refunded.
        let (title, collapsed, costs) = summary.row_group_for_test().unwrap();
        assert_eq!(title, "Costs");
        assert_eq!(costs.first().map(String::as_str), Some("Pay now"));
        assert!(
            collapsed.ends_with(&format!("up to {} + {} now, not refunded", fees.0, fees.1)),
            "{collapsed}"
        );
        let pays = summary.row_hint_for_test("Pay now").unwrap();
        assert!(
            pays.contains(&format!(
                "Paid to broadcaster 0zk1…test from your private balance on {network}"
            )) && pays.contains(&format!("{label} sends ")),
            "{pays}"
        );
        assert!(
            !summary
                .rows_for_test()
                .iter()
                .any(|(label, _)| label.starts_with("Account on ")),
            "a new account has no address to name yet"
        );

        let batch = wallet_ops::PublicSwapBatchTerms {
            proxy,
            guard_token: STUB_USDC,
            guard_amount: bought,
            depositor: source.address,
            recipient: Address::repeat_byte(9),
            input_token: STUB_USDC,
            output_token: STUB_POLYGON_USDC,
            destination_chain: 137,
            scale_numerator: review.bridge().destination_minimum,
            scale_denominator: bought,
            deadline: 1_700_000_000,
            nonce: alloy::primitives::B256::ZERO,
        };
        let mut groups = swaps.public_signature_groups(
            source,
            destination,
            review,
            STUB_USDT,
            None,
            &batch,
            1_700_000_000,
            cx,
        );
        assert_eq!(groups.len(), 2, "an order without a permit signs twice");
        let (order, order_rows) = groups.pop().unwrap();
        let (instructions, batch_rows) = groups.pop().unwrap();
        assert_eq!(instructions, "1 · Bridge instructions for your CoW proxy");
        assert_eq!(order, "2 · CoW order");
        // The device signs the permit first, so its group leads and the others move down.
        let relayer = Address::repeat_byte(0xc0);
        let permit = wallet_ops::PublicSwapPermitTerms {
            token: STUB_USDT,
            spender: relayer,
            amount: review.sell_amount(),
            deadline: 1_700_000_000,
        };
        let with_permit = swaps.public_signature_groups(
            source,
            destination,
            review,
            STUB_USDT,
            Some(&permit),
            &batch,
            1_700_000_000,
            cx,
        );
        assert_eq!(
            with_permit
                .iter()
                .map(|(title, _)| title.as_str())
                .collect::<Vec<_>>(),
            [
                "1 · Approval for CoW",
                "2 · Bridge instructions for your CoW proxy",
                "3 · CoW order"
            ]
        );
        let permit_rows = &with_permit[0].1;
        assert_eq!(
            permit_rows
                .iter()
                .map(|row| (row.label.as_str(), row.value.clone(), row.address.clone()))
                .collect::<Vec<_>>()[..3],
            [
                (
                    "Token",
                    swaps.token_symbol(STUB_USDT, cx),
                    Some(railgun_ui::short_address(&STUB_USDT))
                ),
                (
                    "Spender",
                    "CoW's vault relayer".to_owned(),
                    Some(railgun_ui::short_address(&relayer))
                ),
                (
                    "Amount",
                    swaps.form_sell_amount(form, review.sell_amount(), cx),
                    None
                ),
            ]
        );
        assert_eq!(permit_rows[3].label, "Valid until");
        assert_eq!(
            batch_rows
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            [
                "Runs only if the proxy holds",
                "Deposits into Across",
                "Refunds go to",
                format!("Delivered on {network}").as_str(),
                "Recipient",
                "Valid until"
            ]
        );
        assert_eq!(
            batch_rows[0].value,
            format!("at least {}", swaps.token_amount(STUB_USDC, bought, cx))
        );
        assert_eq!(
            (batch_rows[2].value.clone(), batch_rows[2].address.clone()),
            (label, Some(railgun_ui::short_address(&source.address)))
        );
        assert_eq!(
            order_rows
                .iter()
                .map(|row| (row.label.as_str(), row.value.clone()))
                .collect::<Vec<_>>()[..3],
            [
                (
                    "Sell",
                    swaps.form_sell_amount(form, review.sell_amount(), cx)
                ),
                ("Buy at least", swaps.token_amount(STUB_USDC, bought, cx)),
                ("Receiver", "Your CoW proxy".to_owned()),
            ]
        );
        assert_eq!(
            order_rows[2].address,
            Some(railgun_ui::short_address(&proxy))
        );
        for row in batch_rows.iter().chain(&order_rows).chain(permit_rows) {
            assert!(
                !row.value.contains("1700000000")
                    && !row.value.contains(&bought.to_string())
                    && !row.value.contains("0x"),
                "{} shows raw data: {}",
                row.label,
                row.value
            );
        }
        assert_eq!(
            public_source::public_sign_label("Ledger"),
            "Sign on Ledger…"
        );
    });
}
