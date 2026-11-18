use super::*;

#[gpui::test]
fn swap_receiver_uses_unshield_suggestions_and_saves_to_the_public_address_book(
    cx: &mut TestAppContext,
) {
    use alloy::primitives::address;

    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
    let saved = Address::repeat_byte(0x51);
    let unsaved = Address::repeat_byte(0x7b);
    with_swap_view(cx, |root, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                root.effective_token_registry =
                    wallet_ops::settings::build_effective_token_registry(
                        &wallet_ops::settings::WalletSettings::default(),
                    )
                    .unwrap();
                root.public_address_book = vec![cold_wallet_entry(saved)];
            });
            swaps.update(cx, |swaps, cx| {
                swaps.open_form(
                    None,
                    usdc,
                    Some(dai),
                    Some(U256::from(1_000_000)),
                    None,
                    SwapDelivery::Reshield,
                    window,
                    cx,
                );
                swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(form.delivery.is_err());
                assert!(
                    matches!(form.quote, QuoteState::Loading),
                    "the swap is quoted before its receiver is entered"
                );
                let revision = form.quote_revision;
                // Private Unshield's suggestions: active public accounts and the address book.
                let options = swaps.receiver_options(cx);
                let offered = |address: Address| {
                    options
                        .iter()
                        .any(|option| parse_address(&option.address) == Some(address))
                };
                assert!(offered(Address::repeat_byte(1)) && offered(saved));
                assert!(
                    !offered(Address::repeat_byte(14)),
                    "inactive public accounts aren't suggested"
                );

                let form = swaps.form.as_mut().unwrap();
                form.price_acknowledged = true;
                form.high_costs_acknowledged = true;
                swaps.receiver_picker_event(
                    &RecipientPickerEvent::Select(saved.to_checksum(None).into()),
                    window,
                    cx,
                );
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    form.receiver_input.read(cx).value().to_string(),
                    saved.to_checksum(None)
                );
                assert_eq!(
                    form.delivery,
                    Ok(SwapDelivery::External { receiver: saved })
                );
                assert!(
                    matches!(form.quote, QuoteState::Loading) && form.quote_revision == revision,
                    "a picked receiver, like a typed one, keeps the quote in flight"
                );
                assert!(
                    !form.price_acknowledged && !form.high_costs_acknowledged,
                    "acceptance for another receiver doesn't carry over"
                );

                set_receiver_text(swaps, &unsaved.to_checksum(None), window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let save = cx.debug_bounds("swap-save-receiver").unwrap();
        cx.simulate_click(save.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("Trading desk");
        cx.dispatch_action(gpui_component::dialog::Confirm { secondary: false });
        cx.run_until_parked();
        root.read_with(cx, |root, _| {
            assert!(
                root.public_address_book
                    .iter()
                    .any(|entry| entry.address == unsaved && entry.label == "Trading desk"),
                "the receiver is saved to the public address book"
            );
        });
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(
                form.receiver_input.read(cx).value().to_string(),
                unsaved.to_checksum(None),
                "saving keeps the entered receiver"
            );
            assert_eq!(
                form.delivery,
                Ok(SwapDelivery::External { receiver: unsaved })
            );
        });
    });
}

#[gpui::test]
fn swap_receiver_rejects_addresses_that_would_lose_the_proceeds(cx: &mut TestAppContext) {
    use alloy::primitives::address;

    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        reusable_account(executors, operation);
        let (railgun, profile) = root.read_with(cx, |root, _| {
            let chain = root.effective_chain_configs.get(1).unwrap();
            (
                chain.require_railgun().unwrap().deployment.contract,
                chain.swap_profile().unwrap(),
            )
        });
        let rejected = [
            (String::new(), ENTER_RECEIVER),
            ("0x1234".to_owned(), INVALID_RECEIVER),
            (
                Address::ZERO.to_string(),
                receiver_rejection_message(SwapReceiverRejection::ZeroAddress),
            ),
            (
                Address::repeat_byte(3).to_string(),
                receiver_rejection_message(SwapReceiverRejection::Executor),
            ),
            (
                railgun.to_string(),
                receiver_rejection_message(SwapReceiverRejection::Railgun),
            ),
            (
                profile.settlement().to_string(),
                receiver_rejection_message(SwapReceiverRejection::Settlement),
            ),
            (
                profile.vault_relayer().to_string(),
                receiver_rejection_message(SwapReceiverRejection::VaultRelayer),
            ),
            (
                profile.hooks_trampoline().to_string(),
                receiver_rejection_message(SwapReceiverRejection::HooksTrampoline),
            ),
        ];
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                root.effective_token_registry =
                    wallet_ops::settings::build_effective_token_registry(
                        &wallet_ops::settings::WalletSettings::default(),
                    )
                    .unwrap();
            });
            swaps.update(cx, |swaps, cx| {
                swaps.open_form(
                    None,
                    usdc,
                    Some(dai),
                    Some(U256::from(1_000_000)),
                    None,
                    SwapDelivery::Reshield,
                    window,
                    cx,
                );
                // The swap's own stealth account can't receive: reuse a set-up one.
                swaps.select_form_account(Some(operation), window, cx);
                swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                for (entered, problem) in &rejected {
                    set_receiver_text(swaps, entered, window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(
                        form.delivery,
                        Err(DeliveryProblem::Receiver((*problem).into())),
                        "{entered}"
                    );
                    assert_eq!(
                        form.quote_delivery(),
                        Some(SwapDelivery::External {
                            receiver: Address::ZERO
                        }),
                        "{entered} never reaches a quote"
                    );
                }
            });
            window.draw(cx).clear(cx);
        });
        assert!(
            cx.debug_bounds("swap-receiver-problem").is_some(),
            "the reason shows under the receiver"
        );
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                // Another address, even another account of the wallet, can receive.
                set_receiver_text(swaps, &Address::repeat_byte(4).to_string(), window, cx);
                assert!(swaps.form.as_ref().unwrap().delivery.is_ok());
            });
        });
    });
}

/// A Public address can take the chain's native asset instead of its wrapped native token,
/// from its own Buy entry or Private Unshield's output switch, and the order then buys the
/// native marker. Private balance lists no native entry, and neither does a list without the
/// wrapped token.
#[gpui::test]
fn native_output_is_a_switch_on_wrapped_native_for_a_public_address(cx: &mut TestAppContext) {
    use alloy::primitives::address;

    let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
    let receiver = Address::repeat_byte(4);
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        let offers_native = |swaps: &PrivateSwapsView, cx: &App| {
            swaps
                .buy_picker_items(swaps.form.as_ref().unwrap(), cx)
                .iter()
                .any(|item| item.asset.token == Address::ZERO)
        };
        let shown = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            (
                cx.debug_bounds("swap-native-output").is_some(),
                cx.debug_bounds("swap-native-payout-note").is_some(),
            )
        };
        let orderbook = stub_orderbook(&stubs, runtime);
        let weth = cx.update(|window, cx| {
            let weth = root.update(cx, |root, _| {
                root.effective_token_registry =
                    wallet_ops::settings::build_effective_token_registry(
                        &wallet_ops::settings::WalletSettings::default(),
                    )
                    .unwrap();
                root.effective_chain_configs
                    .get(1)
                    .unwrap()
                    .wrapped_native_token
                    .unwrap()
            });
            swaps.update(cx, |swaps, cx| {
                swaps
                    .private_owner()
                    .unwrap()
                    .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
                swaps.open_form(
                    None,
                    STUB_USDC,
                    Some(weth),
                    Some(U256::from(1_000_000)),
                    None,
                    SwapDelivery::Reshield,
                    window,
                    cx,
                );
                swaps.form.as_mut().unwrap().orderbook = Some(orderbook);
                assert!(!offers_native(swaps, cx));
                // The wrapped native token leads the Buy list; the rest follow by symbol.
                let items = swaps.buy_picker_items(swaps.form.as_ref().unwrap(), cx);
                assert!(items.len() > 2);
                assert_eq!(items[0].asset.token, weth);
                assert!(
                    items[1..]
                        .windows(2)
                        .all(|pair| pair[0].asset.label <= pair[1].asset.label)
                );
            });
            weth
        });
        assert_eq!(
            shown(cx),
            (false, false),
            "Private balance has no output choice"
        );

        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
                set_receiver_text(swaps, &receiver.to_checksum(None), window, cx);
                let items = swaps.buy_picker_items(swaps.form.as_ref().unwrap(), cx);
                assert_eq!(
                    (items[0].asset.token, items[1].asset.token),
                    (Address::ZERO, weth),
                    "ETH leads the Buy list, WETH next"
                );
            });
        });
        assert_eq!(
            shown(cx),
            (true, false),
            "WETH to a Public address offers ETH"
        );

        // The picker's ETH entry, and then its WETH entry, set the switch, and reopening the
        // picker selects the entry in effect.
        let selected = |swaps: &PrivateSwapsView, cx: &App| {
            let tokens = swaps.form.as_ref().unwrap().picker.tokens.read(cx);
            let row = tokens.selected_index().unwrap().row;
            tokens.delegate().listed().nth(row)
        };
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_buy_picker(window, cx);
                swaps.pick_buy_token(Address::ZERO, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.buy, Some(weth));
                assert!(form.native_output);
                assert_eq!(form.order_buy(), Some(Address::ZERO));
                swaps.open_buy_picker(window, cx);
                assert_eq!(selected(swaps, cx), Some(Address::ZERO));
                swaps.close_buy_picker(cx);
                window.close_dialog(cx);
            });
        });
        let review = ready_review(swaps, runtime, cx);
        assert_eq!(review.plan().buy_token(), Address::ZERO);
        assert_eq!(
            stubs.quotes().last().unwrap()["buyToken"]
                .as_str()
                .and_then(|value| value.parse::<Address>().ok()),
            Some(broadcaster_core::contracts::cow::BUY_NATIVE_TOKEN)
        );
        assert_eq!(
            shown(cx),
            (true, true),
            "native output explains the contract wallet limit"
        );

        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_buy_picker(window, cx);
                swaps.pick_buy_token(weth, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.buy, Some(weth));
                assert!(!form.native_output);
                swaps.open_buy_picker(window, cx);
                assert_eq!(selected(swaps, cx), Some(weth));
                swaps.close_buy_picker(cx);
                window.close_dialog(cx);
            });
        });
        assert_eq!(ready_review(swaps, runtime, cx).plan().buy_token(), weth);
        assert_eq!(
            stubs.quotes().last().unwrap()["buyToken"]
                .as_str()
                .and_then(|value| value.parse::<Address>().ok()),
            Some(weth),
            "picking WETH requotes it"
        );
        assert_eq!(shown(cx), (true, false));

        // A WETH sell token leaves WETH, and so ETH, out of the Buy list.
        cx.update(|_, cx| {
            swaps.update(cx, |swaps, cx| {
                let sell = std::mem::replace(&mut swaps.form.as_mut().unwrap().sell, weth);
                assert!(!offers_native(swaps, cx));
                swaps.form.as_mut().unwrap().sell = sell;
            });
        });

        // Another Buy asset has no native form, and choosing WETH again starts wrapped.
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_form_buy(dai, window, cx);
                assert!(!swaps.form.as_ref().unwrap().native_output);
            });
        });
        assert_eq!(shown(cx), (false, false));
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_form_buy(weth, window, cx);
                assert!(!swaps.form.as_ref().unwrap().native_output);
                swaps.set_native_output(true, window, cx);
                swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.buy, Some(weth), "Private balance keeps WETH");
                assert_eq!(form.order_buy(), Some(weth));
            });
        });
        assert_eq!(shown(cx), (false, false));
    });
}

/// The form's Public address quote is ready before a receiver is entered, and can't be reviewed.
/// Entering one keeps that quote, asks neither `CoW` nor a bridge provider, and admits the
/// review of a delivery to it.
fn receiver_joins_the_ready_quote(
    swaps: &Entity<PrivateSwapsView>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    cx: &mut gpui::VisualTestContext,
) {
    let receiver = Address::repeat_byte(4);
    let quoted = ready_review(swaps, runtime, cx);
    let requests = || (stubs.quotes().len(), stubs.bridge_requests().len());
    let asked = requests();
    swaps.read_with(cx, |swaps, _| {
        let form = swaps.form.as_ref().unwrap();
        let Err(DeliveryProblem::Receiver(problem)) = &form.delivery else {
            panic!("no receiver is entered");
        };
        assert_eq!(form.review_problem(&quoted), Some(problem.clone()));
    });
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| {
            set_receiver_text(swaps, &receiver.to_checksum(None), window, cx);
        });
    });
    // Past the debounce, after which a quote or a bridge refresh would make its request.
    cx.executor().advance_clock(QUOTE_DEBOUNCE);
    cx.run_until_parked();
    runtime.block_on(tokio::time::sleep(Duration::from_millis(50)));
    cx.run_until_parked();
    assert_eq!(requests(), asked, "the receiver asks nothing");
    cx.update(|_, cx| {
        swaps.update(cx, |swaps, _| {
            let form = swaps.form.as_mut().unwrap();
            let QuoteState::Ready(review) = &form.quote else {
                panic!("the quote stays ready");
            };
            let review = Arc::clone(review);
            assert_eq!(form.quote_receiver(), receiver);
            assert_eq!(form.delivery, Ok(review.plan().delivery()));
            assert_eq!(
                review.with_receiver(Address::ZERO).plan(),
                quoted.plan(),
                "only the receiver differs"
            );
            assert_eq!(
                (review.suggested_private_minimum(), review.bridge()),
                (quoted.suggested_private_minimum(), quoted.bridge())
            );
            form.price_acknowledged = true;
            form.high_costs_acknowledged = true;
            assert!(form.review_problem(&review).is_none());
        });
    });
}

#[gpui::test]
fn public_address_quote_is_ready_before_its_receiver_and_takes_it_locally(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        let orderbook = stub_orderbook(&stubs, runtime);
        cx.update(|window, cx| {
            let weth = root.update(cx, |root, _| {
                root.effective_token_registry =
                    wallet_ops::settings::build_effective_token_registry(
                        &wallet_ops::settings::WalletSettings::default(),
                    )
                    .unwrap();
                root.effective_chain_configs
                    .get(1)
                    .unwrap()
                    .wrapped_native_token
                    .unwrap()
            });
            swaps.update(cx, |swaps, cx| {
                swaps
                    .private_owner()
                    .unwrap()
                    .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
                swaps.open_form(
                    None,
                    STUB_USDC,
                    Some(weth),
                    Some(U256::from(1_000_000)),
                    None,
                    SwapDelivery::Reshield,
                    window,
                    cx,
                );
                swaps.form.as_mut().unwrap().orderbook = Some(orderbook);
                swaps.set_receive_to(ReceiveTo::PublicAddress, window, cx);
            });
        });
        receiver_joins_the_ready_quote(swaps, &stubs, runtime, cx);
    });
}

#[gpui::test]
fn bridge_quote_is_ready_before_its_receiver_and_takes_it_locally(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                set_receiver_text(swaps, "", window, cx);
                swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
            });
        });
        receiver_joins_the_ready_quote(swaps, &stubs, runtime, cx);
    });
}

#[gpui::test]
fn external_review_names_the_receiver_and_warns_for_own_public_accounts(cx: &mut TestAppContext) {
    use alloy::primitives::address;

    let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
    let own = Address::repeat_byte(1);
    let saved = Address::repeat_byte(0x51);
    let unknown = Address::repeat_byte(0x7b);
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                root.effective_token_registry =
                    wallet_ops::settings::build_effective_token_registry(
                        &wallet_ops::settings::WalletSettings::default(),
                    )
                    .unwrap();
                root.public_address_book = vec![cold_wallet_entry(saved)];
            });
        });
        swaps.read_with(cx, |swaps, cx| {
            for (receiver, own_account) in [(own, true), (saved, false), (unknown, false)] {
                let warning = swaps.own_account_warning(receiver, &swaps.token_symbol(dai, cx), cx);
                // Only the wallet's own account becomes linked to it, and the review says so.
                assert_eq!(warning.is_some(), own_account);
                if let Some(warning) = warning {
                    assert!(warning.starts_with("Account 01 ") && warning.contains("DAI"));
                }
            }
        });

        // The review of a quoted swap of USDC for ETH, delivered to the wallet's own account.
        let orderbook = stub_orderbook(&stubs, runtime);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps
                    .private_owner()
                    .unwrap()
                    .plan_swaps_from_note_for_tests(STUB_USDC, U256::from(10_000_000));
                swaps.open_form(
                    None,
                    STUB_USDC,
                    Some(Address::ZERO),
                    Some(U256::from(1_000_000)),
                    None,
                    SwapDelivery::External { receiver: own },
                    window,
                    cx,
                );
                swaps.form.as_mut().unwrap().orderbook = Some(orderbook);
                swaps.schedule_quote(window, cx);
            });
        });
        for (receiver, label, own_account) in [
            (own, "Account 01 · your Public account", true),
            (saved, "Cold wallet", false),
        ] {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    set_receiver_text(swaps, &receiver.to_checksum(None), window, cx);
                });
            });
            let review = ready_review(swaps, runtime, cx);
            assert_eq!(
                review.plan().delivery(),
                SwapDelivery::External { receiver }
            );
            let (summary, unshield) = swaps.read_with(cx, |swaps, cx| {
                (
                    swaps.swap_summary(&review, None, None, None, None, cx),
                    swaps.token_amount(
                        STUB_USDC,
                        review.plan().amount() - review.sell_amount(),
                        cx,
                    ),
                )
            });
            assert_eq!(
                summary.receiver_for_test(),
                Some((receiver.to_checksum(None), Some(label.to_owned()))),
                "the Receive card copies and shows the approved address in full"
            );
            // No shield: only the unshield fee applies.
            let details = summary.details_for_test();
            assert!(details.contains(&("Railgun unshield".to_owned(), unshield)));
            assert!(
                details
                    .iter()
                    .all(|(label, value)| label != "Railgun shield" && !value.contains("shield")),
                "{details:?}"
            );
            // The disclosure warns only for the wallet's own account, which its card names.
            let (_, public, warns) = summary.disclosure_for_test().unwrap();
            assert!(public.contains(EXTERNAL_DELIVERY_DISCLOSURE), "{public}");
            assert_eq!(warns, own_account);
            assert_eq!(
                public.contains("Account 01 becomes publicly linked to this swap"),
                own_account,
                "{public}"
            );
        }
        // Quotes never carry the receiver.
        for quote in stubs.quotes() {
            let body = quote.to_string().to_ascii_lowercase();
            for receiver in [own, saved] {
                assert!(!body.contains(&format!("{receiver:x}")), "{body}");
            }
        }
    });
}

#[gpui::test]
fn external_swaps_show_their_delivery_and_recover_only_the_sell_token(cx: &mut TestAppContext) {
    use wallet_ops::vault::{SwapObservation, SwapOrderObservations, SwapTradeAmounts};

    let receiver = Address::repeat_byte(0x51);
    let (sell, buy) = (Address::repeat_byte(1), Address::repeat_byte(2));
    with_approved_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        cx.update(|_, cx| {
            root.update(cx, |root, _| {
                root.public_address_book = vec![cold_wallet_entry(receiver)];
            });
        });
        let delivery = SwapDelivery::External { receiver };
        let (uid, observed) = placed_swap(executors, operation, delivery.clone());
        let seen = SwapObservation {
            block: observed.block(),
            transaction_hash: Some(alloy::primitives::B256::repeat_byte(40)),
        };
        let no_private_balance = |steps: &[model::SwapStep], card: &model::SwapCardLine| {
            steps
                .iter()
                .flat_map(|step| [step.label.as_str(), step.detail.as_str()])
                .chain([card.title.as_str(), card.detail.as_str()])
                .all(|text| !text.contains("private balance"))
        };

        // The unshield ran and the order expired unfilled: only the sell token can be in the
        // stealth account, so recovery starts with it, as for a Private swap.
        executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    pre_hook_executed: Some(seen),
                    expired: Some(seen),
                    ..Default::default()
                },
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                let stage = swaps.stage(record);
                assert_eq!(
                    stage,
                    SwapStage::Order(SwapOrderState::PreHookOnly { expired: true })
                );
                assert!(model::swap_actions(stage, false).recover);
                assert_eq!(swap_delivery(record), delivery);
                for stage in [
                    stage,
                    SwapStage::Order(SwapOrderState::Traded),
                    SwapStage::Order(SwapOrderState::NotDelivered),
                ] {
                    assert_eq!(
                        swap_recovery_token(stage, &delivery, sell, buy),
                        sell,
                        "the stealth account never holds the bought token: {stage:?}"
                    );
                }
                let labels = swaps.labels(record, cx);
                assert_eq!(labels.receiver.as_deref(), Some("Cold wallet"));
                assert!(no_private_balance(
                    &model::swap_steps(stage, &labels),
                    &model::swap_card_line(stage, &labels)
                ));
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-detail-receiver").is_some());

        // The trade paid the receiver: the swap is delivered, not back in a private balance.
        executors
            .record_swap_observations(
                operation,
                uid,
                SwapOrderObservations {
                    pre_hook_executed: Some(seen),
                    traded: Some(seen),
                    delivered: Some(seen),
                    trade_amounts: Some(SwapTradeAmounts {
                        sell_amount: U256::from(100),
                        buy_amount: U256::from(99),
                        fee_amount: U256::ZERO,
                        settlement_gas_used: None,
                        settlement_effective_gas_price: None,
                        executed_fee: None,
                        executed_fee_token: None,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let record = swaps.record(operation).unwrap();
                let stage = swaps.stage(record);
                assert_eq!(stage, SwapStage::Order(SwapOrderState::Done));
                let labels = swaps.labels(record, cx);
                let received = swaps.token_amount(buy, U256::from(99), cx);
                assert_eq!(labels.received.as_ref(), Some(&received));
                let steps = model::swap_steps(stage, &labels);
                assert_eq!(
                    steps
                        .iter()
                        .map(|step| step.label.as_str())
                        .collect::<Vec<_>>(),
                    [
                        "Stealth account set up",
                        "Order open",
                        "Delivered to Cold wallet"
                    ]
                );
                assert!(steps[2].detail.contains(&received));
                let card = model::swap_card_line(stage, &labels);
                assert_eq!(card.detail, format!("Delivered {received} to Cold wallet"));
                assert!(no_private_balance(&steps, &card));
                // My orders names the receiver, and the swap ends Delivered rather than Filled.
                let rows = swaps.order_rows_for_test(cx);
                assert!(
                    rows.iter().any(|(meta, status)| {
                        meta.ends_with(" · to Cold wallet") && status == "Delivered"
                    }),
                    "{rows:?}"
                );
                swaps.show_detail(operation, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        assert!(cx.debug_bounds("swap-outcome").is_some());
        assert!(cx.debug_bounds("swap-detail-receiver").is_some());
    });
}

/// After a restart before its first order, an External swap requotes with the delivery saved
/// with its setup. The quote still names only the stealth account, and the confirm step shows
/// the receiver.
#[gpui::test]
fn restarted_external_swap_requotes_its_approved_delivery(cx: &mut TestAppContext) {
    let receiver = Address::repeat_byte(0x51);
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, _, executors, operation, runtime, cx| {
            let setup = approved_swap(
                root,
                executors,
                operation,
                external_approval(receiver, Some(U256::MAX)),
                cx,
            );
            let swaps = restarted_swaps(root, &stubs, runtime, operation, setup, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.place_approved_order(operation, window, cx);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.job.is_none())
            });
            swaps.read_with(cx, |swaps, cx| {
                let pending = swaps
                    .pending_authorization
                    .as_ref()
                    .unwrap_or_else(|| panic!("{:?}", swaps.tracking[&operation].error));
                let SwapAction::Order(approval) = &pending.action else {
                    panic!("the approved order's confirm step");
                };
                assert!(!approval.full_review);
                let plan = approval.review.plan();
                assert_eq!(plan.delivery(), SwapDelivery::External { receiver });
                assert_eq!(plan.buy_token(), Address::ZERO);
                assert_eq!(
                    swaps
                        .place_summary(approval, cx)
                        .unwrap()
                        .receiver_for_test()
                        .map(|(address, _)| address),
                    Some(receiver.to_checksum(None))
                );
            });
            let quotes = stubs.quotes();
            assert_eq!(quotes.len(), 1);
            let address = |field: &str| {
                quotes[0][field]
                    .as_str()
                    .and_then(|value| value.parse::<Address>().ok())
            };
            // CoW sees the stealth account as receiver, and its native buy address.
            assert_eq!(address("receiver"), Some(Address::repeat_byte(3)));
            assert_eq!(
                address("buyToken"),
                Some(broadcaster_core::contracts::cow::BUY_NATIVE_TOKEN)
            );
            assert!(
                !quotes[0]
                    .to_string()
                    .to_ascii_lowercase()
                    .contains(&format!("{receiver:x}"))
            );
        },
    );
}

/// A changed hook cost reopens a restarted External swap's form with its approved delivery,
/// receiver and native Buy asset. The native pair keeps its Public address, but another
/// receiver can be typed, and the review then names the delivery change, not the hook cost.
#[gpui::test]
fn reopened_external_swap_keeps_its_delivery_and_names_a_receiver_change(cx: &mut TestAppContext) {
    let (receiver, moved) = (Address::repeat_byte(0x51), Address::repeat_byte(0x7b));
    let stubs = SwapStubs::start();
    with_approved_swap_view_and_rpc(
        cx,
        Some(stubs.rpc()),
        |root, _, executors, operation, runtime, cx| {
            // An approval saved before gas shares needs review again.
            let setup = approved_swap(
                root,
                executors,
                operation,
                external_approval(receiver, None),
                cx,
            );
            let swaps = restarted_swaps(root, &stubs, runtime, operation, setup, cx);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.place_approved_order(operation, window, cx);
                });
            });
            drive_until(cx, runtime, |cx| {
                swaps.read_with(cx, |swaps, _| swaps.form.is_some())
            });
            swaps.read_with(cx, |swaps, cx| {
                assert_eq!(
                    swaps.reapproval,
                    Some((operation, SwapReviewChange::GasShare))
                );
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.operation, Some(operation));
                assert_eq!(form.receive_to, ReceiveTo::PublicAddress);
                assert_eq!(
                    form.receiver_input.read(cx).value().to_string(),
                    receiver.to_checksum(None)
                );
                assert_eq!(form.delivery, Ok(SwapDelivery::External { receiver }));
                // The native Buy comes back as WETH with native output.
                let weth = root
                    .read(cx)
                    .effective_chain_configs
                    .get(1)
                    .unwrap()
                    .wrapped_native_token;
                assert!(weth.is_some());
                assert_eq!(form.buy, weth);
                assert!(form.native_output);
            });
            // The approved native pair can only pay a Public address.
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                    let form = swaps.form.as_ref().unwrap();
                    assert_eq!(form.receive_to, ReceiveTo::PublicAddress);
                    assert!(form.native_output);
                });
            });
            let review = ready_review(&swaps, runtime, cx);
            assert_eq!(
                review.plan().delivery(),
                SwapDelivery::External { receiver }
            );
            assert_eq!(review.plan().buy_token(), Address::ZERO);

            // Typed, not set: a disabled receiver input would refuse the keystrokes.
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                swaps.update(cx, |swaps, cx| {
                    let input = swaps.form.as_ref().unwrap().receiver_input.clone();
                    input.update(cx, |input, cx| input.focus(window, cx));
                });
                window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
            });
            cx.simulate_input(&moved.to_checksum(None));
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    form.delivery,
                    Ok(SwapDelivery::External { receiver: moved })
                );
                assert!(form.native_output);
            });
            let review = ready_review(&swaps, runtime, cx);
            assert_eq!(review.plan().buy_token(), Address::ZERO);
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    assert_eq!(
                        swaps.first_order_change(operation, &review),
                        Some(SwapReviewChange::Delivery),
                        "the delivery change comes before the hook cost change"
                    );
                    swaps.form.as_mut().unwrap().price_acknowledged = true;
                    swaps.form_primary(window, cx);
                    let pending = swaps.pending_authorization.as_ref().expect("a full review");
                    let SwapAction::Order(approval) = &pending.action else {
                        panic!("the order's review");
                    };
                    assert!(approval.full_review);
                    assert_eq!(
                        approval.review.plan().delivery(),
                        SwapDelivery::External { receiver: moved }
                    );
                    assert!(swaps.reapproval.is_none());
                });
            });
        },
    );
}

/// An approval saved with a swap's setup, of 1 USDC for ETH delivered to `receiver`. Every term
/// the stub quote leads to is within it, so an unchanged requote is confirm-only. Without a
/// `gas_allowance`, it was saved before gas shares and the order needs review again.
fn external_approval(receiver: Address, gas_allowance: Option<U256>) -> SwapApproval {
    let mut approval = test_approval();
    let bounds = &mut approval.bounds;
    bounds.sell_amount = U256::from(997_500);
    bounds.unshield_amount = Some(U256::from(1_000_000));
    bounds.unshield_fee_bps = wallet_ops::RAILGUN_PROTOCOL_FEE_BPS;
    bounds.buy_amount = U256::ONE;
    bounds.private_minimum = U256::ONE;
    bounds.shield_fee_bps = U256::ZERO;
    bounds.pre_hook_gas_limit = u64::MAX;
    bounds.post_hook_gas_limit = None;
    bounds.gas_share_bps = gas_allowance.map(|_| wallet_ops::cow::GAS_SHARE_BALANCED_BPS);
    bounds.gas_allowance = gas_allowance;
    bounds.valid_for_secs = Some(600);
    approval.delivery = SwapDelivery::External { receiver };
    approval.tokens = Some(wallet_ops::vault::SwapApprovalTokens {
        sell: STUB_USDC,
        buy: Address::ZERO,
    });
    approval
}

/// `operation`'s stealth account at `Address::repeat_byte(3)`, set up with `approval` and no
/// order yet. Returns the setup's observation.
fn approved_swap(
    root: &Entity<WalletRoot>,
    executors: &ExecutorStore,
    operation: ExecutorOperationId,
    approval: SwapApproval,
    cx: &gpui::VisualTestContext,
) -> wallet_ops::SwapSetupStatus {
    use alloy::eips::{BlockNumHash, eip7702::constants::EIP7702_DELEGATION_DESIGNATOR};
    use alloy::primitives::{B256, Bytes};
    use wallet_ops::vault::{
        ExecutorNonceObservation, ExecutorPayloadContext, ExecutorPayloadPurpose,
        IssuedExecutorPayload,
    };
    let profile = root.read_with(cx, |root, _| {
        root.effective_chain_configs
            .get(1)
            .unwrap()
            .accepted_executor_profile()
            .unwrap()
    });
    executors
        .bind_address(operation, Address::repeat_byte(3))
        .unwrap();
    let observed =
        ExecutorNonceObservation::new(BlockNumHash::new(10, B256::repeat_byte(10)), U256::ZERO);
    executors.record_account_read(operation, observed).unwrap();
    let payload = B256::repeat_byte(4);
    executors
        .record_issued(
            operation,
            IssuedExecutorPayload::new(
                U256::ZERO,
                profile.delegate(),
                payload,
                ExecutorPayloadPurpose::Operation,
                ExecutorPayloadContext::new(Bytes::from_static(b"setup"), observed, Vec::new()),
            ),
        )
        .unwrap();
    let confirmed = BlockNumHash::new(12, B256::repeat_byte(12));
    let record = executors
        .record_account_read(
            operation,
            ExecutorNonceObservation::new(confirmed, U256::ONE),
        )
        .unwrap();
    executors
        .record_swap_approval(operation, SwapUseId::first(operation), approval)
        .unwrap();
    let code = [
        EIP7702_DELEGATION_DESIGNATOR.as_slice(),
        profile.delegate().as_slice(),
    ]
    .concat();
    wallet_ops::swap_setup_status(&record, confirmed, &code, profile)
}
