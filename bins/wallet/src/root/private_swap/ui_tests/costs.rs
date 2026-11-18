use super::*;

/// Provider fees or private destination costs can each push the approved costs above 20%.
/// Both must appear in the total and require Swap anyway.
#[gpui::test]
fn high_bridge_costs_require_swap_anyway(cx: &mut TestAppContext) {
    for private in [false, true] {
        let stubs = SwapStubs::start();
        stubs.set_across_fee_bps(if private { 300 } else { 2_000 });
        if private {
            stubs
                .across_relayer_gas_bps
                .store(200, std::sync::atomic::Ordering::Relaxed);
        }
        with_swap_view_and_rpc(
            cx,
            Some(stubs.rpc()),
            |root, swaps, _, operation, runtime, cx| {
                open_bridge_form(root, swaps, &stubs, runtime, cx);
                cx.update(|window, cx| {
                    swaps.update(cx, |swaps, cx| {
                        swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
                        if private {
                            swaps.set_receive_to(ReceiveTo::PrivateBalance, window, cx);
                            // An existing swap has already admitted its destination setup.
                            swaps.form.as_mut().unwrap().operation = Some(operation);
                            swaps.bridge_choices_changed(window, cx);
                        }
                    });
                });
                let review = ready_review(swaps, runtime, cx);
                let bridge = review.bridge().unwrap();
                let destination_cost = bridge.private.map_or(U256::ZERO, |delivery| {
                    delivery.quoted_output - bridge.received_minimum()
                });
                if private {
                    assert!(
                        authorized_cost_bps(
                            review.best_case(),
                            review.plan().amount(),
                            review.sell_amount(),
                            review.suggested_private_minimum() - bridge.fee.unwrap(),
                        ) < AUTHORIZED_COST_WARNING_BPS
                    );
                    assert!(destination_cost > review.best_case() / U256::from(5));
                }
                let before_delivery = expected_payout(&review);
                let retained = before_delivery
                    - shield_fee_on(&review, before_delivery)
                    - bridge.fee.unwrap()
                    - destination_cost;
                assert_eq!(total_cost(&review), swap_fees_to(&review, retained));
                cx.update(|window, cx| window.draw(cx).clear(cx));
                assert!(cx.debug_bounds("swap-high-costs").is_some());
                let bps =
                    authorized_high_cost(&review).expect("bridge costs take the total past 20%");
                cx.update(|_, cx| {
                    swaps.update(cx, |swaps, cx| {
                        let warning = swaps.authorized_cost_warning(&review, bps, cx);
                        let message = warning.message();
                        assert!(warning.details.contains("bridge fee"), "{message}");
                        assert!(
                            swaps
                                .swap_summary(&review, None, None, None, None, cx)
                                .warnings_for_test()
                                .contains(&message)
                        );
                        if private {
                            assert!(warning.details.contains("destination"), "{message}");
                            let cache = &root.read(cx).public_broadcaster_anchor_cache;
                            cache.store_rate(1, STUB_USDT, U256::from(1_000_000));
                            cache.store_native_usd_rate(1, U256::from(1_000_000), 18);
                            // Without a destination price, value the net credit in source
                            // units; with one, value that same credit on the destination.
                            let fallback =
                                swaps.usd_micro_value(STUB_USDT, bridge.received_minimum(), cx);
                            assert!(fallback.is_some());
                            assert_eq!(swaps.bridge_usd_value(&review, cx), fallback);
                            cache.store_rate(137, STUB_POLYGON_USDT, U256::from(2_000_000));
                            cache.store_native_usd_rate(137, U256::from(1_000_000), 18);
                            assert_eq!(
                                swaps.bridge_usd_value(&review, cx),
                                swaps.network_usd_micro_value(
                                    137,
                                    STUB_POLYGON_USDT,
                                    bridge.received_minimum(),
                                    cx
                                )
                            );
                        }
                        let form = swaps.form.as_mut().unwrap();
                        form.price_acknowledged = true;
                        assert_eq!(
                            form.review_problem(&review)
                                .map(|problem| problem.to_string()),
                            Some("Confirm Swap anyway to accept the high swap costs.".to_owned())
                        );
                        form.high_costs_acknowledged = true;
                        form.blocked_shield_acknowledged = true;
                        assert!(form.review_problem(&review).is_none());
                    });
                });
            },
        );
    }
}

/// A new swap's gas share is Balanced, and a preset prices the same quote again without a `CoW`
/// request. Order validity quotes again, and the share stays. While a quote loads, the strip's
/// row keeps the Buy panel's height, and a preset chosen there prices the quote that lands.
#[gpui::test]
fn balanced_is_the_default_and_presets_reprice_without_a_new_quote(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        let review = ready_review(swaps, runtime, cx);
        assert_eq!(review.gas_share_bps(), GAS_SHARE_BALANCED_BPS);
        assert_eq!(stubs.quotes().len(), 1);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let tight = cx.debug_bounds("swap-gas-tight").unwrap();
        cx.simulate_click(tight.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            let QuoteState::Ready(repriced) = &form.quote else {
                panic!("the quote priced at Tight");
            };
            assert_eq!(
                (form.gas_share_bps, form.gas_custom),
                (GAS_SHARE_TIGHT_BPS, false)
            );
            assert_eq!(repriced.gas_share_bps(), GAS_SHARE_TIGHT_BPS);
            let tight = review.with_gas_share(GAS_SHARE_TIGHT_BPS).unwrap();
            assert_eq!(
                repriced.suggested_private_minimum(),
                tight.suggested_private_minimum()
            );
            assert!(repriced.suggested_private_minimum() > review.suggested_private_minimum());
            assert!(form.quote_task.is_none());
            assert_eq!(
                slider_percent(form.gas_slider.read(cx).value().end()),
                90,
                "the knob moves to Tight"
            );
        });
        assert_eq!(stubs.quotes().len(), 1, "a preset makes no CoW request");

        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_valid_for(Duration::from_mins(30), window, cx);
            });
        });
        let review = ready_review(swaps, runtime, cx);
        assert_eq!(review.valid_for(), Duration::from_mins(30));
        assert_eq!(review.gas_share_bps(), GAS_SHARE_TIGHT_BPS);
        assert_eq!(stubs.quotes().len(), 2);

        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| swaps.schedule_quote(window, cx));
            window.draw(cx).clear(cx);
        });
        swaps.read_with(cx, |swaps, _| {
            assert!(matches!(
                swaps.form.as_ref().unwrap().quote,
                QuoteState::Loading
            ));
        });
        assert!(cx.debug_bounds("swap-gas-strip").is_some());
        let edit = cx.debug_bounds("swap-gas-edit-minimum").unwrap();
        let loose = cx.debug_bounds("swap-gas-loose").unwrap();
        cx.simulate_click(edit.center(), gpui::Modifiers::none());
        cx.simulate_click(loose.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert!(matches!(form.quote, QuoteState::Loading));
            assert!(
                !form.gas_minimum_editing,
                "the edit button waits for a quote"
            );
            assert_eq!(
                (form.gas_share_bps, form.gas_custom),
                (GAS_SHARE_LOOSE_BPS, false)
            );
        });
        // The card apart from its status line, which the stub's unverified price wraps.
        let card = |cx: &mut gpui::VisualTestContext| {
            cx.debug_bounds("swap-buy-panel").unwrap().size.height
                - cx.debug_bounds("swap-price-status").unwrap().size.height
        };
        let loading = card(cx);
        let review = ready_review(swaps, runtime, cx);
        assert_eq!(review.gas_share_bps(), GAS_SHARE_LOOSE_BPS);
        assert_eq!(stubs.quotes().len(), 3, "the share makes no CoW request");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(card(cx), loading);
    });
}

/// A Bridge swap's bridge leg is quoted for the order amount, so another gas share quotes that
/// leg again, without a `CoW` request, and a release at the dragged share asks nothing more.
/// Its validity stays at the profile's window.
#[gpui::test]
fn bridge_gas_share_requotes_only_the_bridge_leg_and_keeps_the_profile_validity(
    cx: &mut TestAppContext,
) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
            });
        });
        let review = ready_review(swaps, runtime, cx);
        let quotes = stubs.quotes().len();
        let fees = across_fee_amounts(&stubs).len();
        move_gas_slider(swaps, 50., false, cx);
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!((form.gas_share_bps, form.gas_custom), (5_000, true));
            assert!(
                matches!(&form.quote, QuoteState::Ready(quoted) if Arc::ptr_eq(quoted, &review)),
                "the quote stays while its bridge leg is quoted again"
            );
        });
        move_gas_slider(swaps, 50., true, cx);
        let requoted = refreshed_review(swaps, runtime, cx);
        assert_eq!(requoted.gas_share_bps(), 5_000);
        assert_eq!(
            across_fee_amounts(&stubs)[fees..],
            [requoted.suggested_private_minimum()],
            "the drag and its release ask Across once"
        );
        assert_eq!(stubs.quotes().len(), quotes, "no CoW request");
        assert_eq!(requoted.quote(), review.quote());

        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let before = swaps.form.as_ref().unwrap().valid_for;
                swaps.set_valid_for(Duration::from_mins(60), window, cx);
                let form = swaps.form.as_mut().unwrap();
                assert_eq!(form.valid_for, before);
                assert!(matches!(form.quote, QuoteState::Ready(_)));
                form.details_open = true;
            });
            window.draw(cx).clear(cx);
        });
        assert_eq!(requoted.valid_for(), review.valid_for());
        assert!(cx.debug_bounds("swap-validity-locked").is_some());
    });
}

/// Editing a Bridge swap's minimum keeps the quote and the strip in view: typing, the bar's
/// keys and a preset clicked from the focused Minimum field each price the strip locally,
/// where the swap can't be reviewed, and then ask the provider once, for the last share's
/// order amount. No edit asks `CoW`.
#[gpui::test]
fn bridge_share_edits_keep_the_strip_and_quote_the_bridge_leg_once(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
            });
        });
        let review = ready_review(swaps, runtime, cx);
        let quotes = stubs.quotes().len();
        // Blur is only reported in an active window.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            swaps.update(cx, |swaps, cx| swaps.edit_gas_minimum(window, cx));
            window.draw(cx).clear(cx);
        });

        // The form's share after an edit: the quote stays, estimated and unreviewable, and
        // nothing is requested before the debounce.
        let previewed = |cx: &mut gpui::VisualTestContext, fees: usize, edit: &str| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert_eq!(stubs.quotes().len(), quotes, "{edit}");
            assert_eq!(across_fee_amounts(&stubs).len(), fees, "{edit}");
            assert!(cx.debug_bounds("swap-gas-strip").is_some(), "{edit}");
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                let QuoteState::Ready(quoted) = &form.quote else {
                    panic!("{edit} keeps the quote");
                };
                assert_ne!(quoted.gas_share_bps(), form.gas_share_bps, "{edit}");
                assert_eq!(
                    form.review_problem(quoted)
                        .map(|problem| problem.to_string()),
                    Some("Updating the bridge quote…".to_owned()),
                    "{edit}"
                );
                form.gas_share_bps
            })
        };
        // The quote after the debounce: one Across request, for the last share's order amount,
        // gave the review its destination minimum, and the swap can be reviewed.
        let settled = |cx: &mut gpui::VisualTestContext, fees: usize, share: u16| {
            let requoted = refreshed_review(swaps, runtime, cx);
            let amount = requoted.suggested_private_minimum();
            assert_eq!(requoted.gas_share_bps(), share);
            assert_eq!(across_fee_amounts(&stubs)[fees..], [amount]);
            assert_eq!(
                requoted.bridge().unwrap().destination_minimum,
                amount - amount * U256::from(10) / U256::from(10_000)
            );
            assert_eq!(stubs.quotes().len(), quotes);
            cx.update(|_, cx| {
                swaps.update(cx, |swaps, _| {
                    let form = swaps.form.as_mut().unwrap();
                    assert!(
                        !form.price_acknowledged && !form.high_costs_acknowledged,
                        "consent doesn't carry over to the new terms"
                    );
                    form.price_acknowledged = true;
                    form.high_costs_acknowledged = true;
                    assert!(form.review_problem(&requoted).is_none());
                });
            });
        };

        let mut fees = across_fee_amounts(&stubs).len();
        let mut shares = vec![review.gas_share_bps()];
        let mut typed = String::new();
        for share in [5_000, 3_000] {
            typed = swaps.read_with(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                format_unshield_amount_input(
                    StripScale::of(&review).show(
                        review
                            .with_gas_share(share)
                            .unwrap()
                            .suggested_private_minimum(),
                    ),
                    swaps.strip_decimals(form, &review, cx),
                )
            });
            cx.update(|window, cx| {
                window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
            });
            cx.simulate_input(&typed);
            cx.run_until_parked();
            let share = previewed(cx, fees, "typing");
            assert!(cx.debug_bounds("swap-gas-minimum-field").is_some());
            assert!(!shares.contains(&share), "each minimum is another share");
            shares.push(share);
        }
        settled(cx, fees, *shares.last().unwrap());
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(
                form.gas_minimum_input.read(cx).value().to_string(),
                typed,
                "the reply leaves the focused field as typed"
            );
        });

        fees += 1;
        focus_gas_bar(swaps, cx);
        for _ in 0..2 {
            cx.simulate_keystrokes("left");
            cx.run_until_parked();
            let share = previewed(cx, fees, "a bar key");
            assert!(cx.debug_bounds("swap-gas-bar").is_some());
            assert!(!shares.contains(&share), "each key moves the share");
            shares.push(share);
        }
        settled(cx, fees, *shares.last().unwrap());

        // A preset clicked while the Minimum field has the focus: the field's blur leaves the
        // strip in place, so the click lands.
        fees += 1;
        cx.update(|window, cx| {
            let input = swaps
                .read(cx)
                .form
                .as_ref()
                .unwrap()
                .gas_minimum_input
                .clone();
            input.update(cx, |input, cx| input.focus(window, cx));
            window.draw(cx).clear(cx);
        });
        cx.run_until_parked();
        let tight = cx.debug_bounds("swap-gas-tight").unwrap();
        cx.simulate_click(tight.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(previewed(cx, fees, "a preset"), GAS_SHARE_TIGHT_BPS);
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert!(!form.gas_custom && !form.gas_minimum_editing);
        });
        settled(cx, fees, GAS_SHARE_TIGHT_BPS);
    });
}

/// A bridge refresh that another share superseded never replaces the shown terms. A failed one
/// keeps the quote and the chosen share, shows why, and blocks the review until another share
/// is quoted.
#[gpui::test]
fn a_superseded_or_failed_bridge_refresh_keeps_the_quote(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_bridge_form(root, swaps, &stubs, runtime, cx);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.pick_buy_token(STUB_POLYGON_USDT, window, cx);
            });
        });
        ready_review(swaps, runtime, cx);
        let quotes = stubs.quotes().len();
        let fees = across_fee_amounts(&stubs).len();

        // Across holds its answer for the first share until after the second share's.
        stubs.set_across_delay(Duration::from_millis(300));
        move_gas_slider(swaps, 50., false, cx);
        drive_until(cx, runtime, |_| across_fee_amounts(&stubs).len() > fees);
        stubs.set_across_delay(Duration::ZERO);
        move_gas_slider(swaps, 30., false, cx);
        let current = refreshed_review(swaps, runtime, cx);
        let amount = current.suggested_private_minimum();
        let asked = across_fee_amounts(&stubs);
        assert_eq!(asked.len(), fees + 2);
        assert_ne!(asked[fees], amount);
        assert_eq!(asked[fees + 1], amount);
        runtime.block_on(tokio::time::sleep(Duration::from_millis(400)));
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(form.gas_share_bps, current.gas_share_bps());
            assert!(
                matches!(&form.quote, QuoteState::Ready(quoted) if Arc::ptr_eq(quoted, &current)),
                "the first share's late answer changes nothing"
            );
        });

        // Across leaves nothing to receive, which fails the bridge quote.
        stubs.set_across_fee_bps(10_000);
        move_gas_slider(swaps, 70., false, cx);
        refreshed_review(swaps, runtime, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-gas-strip").is_some());
        assert!(cx.debug_bounds("swap-bridge-quote-error").is_some());
        let failed = swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert!(
                matches!(&form.quote, QuoteState::Ready(quoted) if Arc::ptr_eq(quoted, &current)),
                "a failed refresh keeps the quote"
            );
            assert_ne!(form.gas_share_bps, current.gas_share_bps());
            let error = form.bridge_quote_error.clone().expect("the refresh failed");
            assert_eq!(form.review_problem(&current), Some(error));
            form.gas_share_bps
        });

        stubs.set_across_fee_bps(10);
        move_gas_slider(swaps, 50., false, cx);
        let recovered = refreshed_review(swaps, runtime, cx);
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert!(![failed, current.gas_share_bps()].contains(&recovered.gas_share_bps()));
            assert_eq!(recovered.gas_share_bps(), form.gas_share_bps);
            assert!(form.bridge_quote_error.is_none());
            assert!(!form.gas_share_pending(&recovered));
        });
        assert_eq!(stubs.quotes().len(), quotes, "no refresh asks CoW");
    });
}

/// The bar and the Minimum field stay closed until the edit button opens them, and it closes
/// them again while a preset is selected. Dragging the bar selects Custom and prices its share,
/// which keeps them open; the edit button then focuses the field. A typed minimum moves the
/// knob to its share, a requote at another gas price keeps that share, not the typed amount,
/// and a preset closes them.
#[gpui::test]
fn the_gas_bar_and_custom_minimum_set_a_share_that_a_requote_keeps(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        let review = ready_review(swaps, runtime, cx);
        let bar_open = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let bar = cx.debug_bounds("swap-gas-bar").is_some();
            assert_eq!(cx.debug_bounds("swap-gas-minimum-field").is_some(), bar);
            bar
        };
        assert!(!bar_open(cx), "a preset's bar starts closed");
        for open in [true, false] {
            let edit = cx.debug_bounds("swap-gas-edit-minimum").unwrap();
            cx.simulate_click(edit.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            assert_eq!(bar_open(cx), open, "the edit button toggles the bar");
        }
        move_gas_slider(swaps, 50., false, cx);
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!((form.gas_share_bps, form.gas_custom), (5_000, true));
            let QuoteState::Ready(repriced) = &form.quote else {
                panic!("the quote at the knob's share");
            };
            assert_eq!(
                repriced.suggested_private_minimum(),
                review
                    .with_gas_share(5_000)
                    .unwrap()
                    .suggested_private_minimum()
            );
        });

        assert!(bar_open(cx), "a custom share shows its bar");

        let typed = review
            .with_gas_share(3_000)
            .unwrap()
            .suggested_private_minimum();
        let edit = cx.debug_bounds("swap-gas-edit-minimum").unwrap();
        cx.simulate_click(edit.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(bar_open(cx), "a custom share keeps its bar open");
        cx.update(|window, cx| {
            let input = swaps
                .read(cx)
                .form
                .as_ref()
                .unwrap()
                .gas_minimum_input
                .clone();
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
            window.dispatch_action(Box::new(gpui_component::input::SelectAll), cx);
        });
        cx.simulate_input(&format_unshield_amount_input(typed, Some(6)));
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(form.gas_share_bps, 3_000);
            assert_eq!(slider_percent(form.gas_slider.read(cx).value().end()), 70);
        });

        stubs.set_gas_price_wei(2);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.focus_form_amount(window, cx);
                swaps.schedule_quote(window, cx);
            });
        });
        let requoted = ready_review(swaps, runtime, cx);
        assert_eq!(requoted.gas_share_bps(), 3_000);
        assert!(requoted.suggested_private_minimum() < typed, "the gas rose");
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert!(form.gas_custom);
            assert_eq!(
                form.gas_minimum_input.read(cx).value().to_string(),
                railgun_ui::format_token_amount(requoted.suggested_private_minimum(), 6)
            );
        });

        assert!(bar_open(cx));
        let tight = cx.debug_bounds("swap-gas-tight").unwrap();
        cx.simulate_click(tight.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(!bar_open(cx), "a preset closes the bar");
    });
}

/// With the bar focused, the arrows move the share by 5% of the gas, and Home and End go to
/// the bar's ends.
#[gpui::test]
fn gas_bar_keys_step_the_share_by_five_percent(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        ready_review(swaps, runtime, cx);
        focus_gas_bar(swaps, cx);
        for (keys, share) in [
            ("left", 3_000),
            ("right right", 2_000),
            ("end", 0),
            ("home", GAS_SHARE_LOOSE_BPS),
        ] {
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(
                    (form.gas_share_bps, form.gas_custom),
                    (share, true),
                    "{keys}"
                );
            });
        }
    });
}

/// When the gas estimate exceeds the swap, the bar starts at zero, Loose is unavailable, and
/// the bar's start is the most gas that leaves a positive minimum.
#[gpui::test]
fn the_gas_bar_starts_at_zero_when_gas_exceeds_the_swap(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        let first = ready_review(swaps, runtime, cx);
        let review = requote_at_gas(swaps, &stubs, runtime, gas_price_for(&first, 15_000), cx);
        let bar = GasBar::of(&review);
        assert!(bar.gas_exceeds());
        assert_eq!(review.gas_share_bps(), GAS_SHARE_BALANCED_BPS);
        assert!(review.with_gas_share(GAS_SHARE_LOOSE_BPS).is_err());
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            let (start, label, _) = swaps.gas_bar_ends(form, &review, &review, cx);
            assert_eq!(
                (start, label),
                (
                    swaps.bare_amount(STUB_USDT, U256::ZERO, cx),
                    "gas exceeds the swap"
                )
            );
        });
        focus_gas_bar(swaps, cx);
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(form.gas_share_bps, bar.max_share_bps());
            assert!(bar.max_share_bps() < GAS_SHARE_LOOSE_BPS);
            assert!(matches!(&form.quote, QuoteState::Ready(start)
                    if start.gas_share_bps() == bar.max_share_bps()
                        && !start.suggested_private_minimum().is_zero()));
        });
    });
}

/// The spec's 10 USDC swap: Balanced's 8.45 USDT minimum stays below the warning, and
/// Loose's 4.04 needs Swap anyway.
#[test]
fn authorized_costs_count_everything_the_minimum_gives_up() {
    let cost = |minimum: u64| {
        authorized_cost_bps(
            U256::from(9_958_600),
            U256::from(10_000_000),
            U256::from(9_975_000),
            U256::from(minimum),
        )
    };
    assert!(cost(8_450_000) < AUTHORIZED_COST_WARNING_BPS, "Balanced");
    assert!(cost(4_040_000) >= AUTHORIZED_COST_WARNING_BPS, "Loose");
}

/// A USD value is left out only when it reads as the amount beside it. Whole-number zeros are
/// digits, not padding.
#[test]
fn a_usd_value_repeats_only_the_amount_it_reads_as() {
    for (usd, amount, repeats) in [
        ("≈ $9.63", "9.63", true),
        ("≈ $10.00", "10", true),
        ("≈ $1,234.50", "1234.5", true),
        ("≈ $9.61", "9.63", false),
        ("≈ $9.63", "0.0039", false),
        ("≈ $100.00", "1", false),
    ] {
        assert_eq!(
            usd_repeats_amount(usd, amount),
            repeats,
            "{usd} beside {amount}"
        );
    }
}

/// The details header's costs are measured from the best case, which counts `CoW`'s network
/// fee as output: the unshield fee at the best-case rate, the gas allowance and the shield fee.
/// A payout above the quote's `buyAmount` hides none of them.
#[gpui::test]
fn total_cost_counts_every_deduction_from_the_best_case(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    // Of the 997,500 sold after the unshield fee, which leaves a quoted sell amount of 900,000.
    stubs.set_fee_amount(97_500);
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        let first = ready_review(swaps, runtime, cx);
        let review = requote_at_gas(swaps, &stubs, runtime, gas_price_for(&first, 1_000), cx);
        // buyAmount + floor(97,500 * buyAmount / 900,000).
        let best = U256::from(443_333_333_333_333_u64);
        assert_eq!(review.best_case(), best);
        assert_eq!(review.plan().amount(), U256::from(1_000_000));
        let unshield_fee = U256::from(2_500) * best / U256::from(997_500);
        for review in [review.with_gas_share(0).unwrap(), (*review).clone()] {
            let allowance = review.gas_allowance();
            assert_eq!(allowance.is_zero(), review.gas_share_bps() == 0);
            let payout = best - allowance;
            assert!(payout > U256::from(STUB_BUY_AMOUNT));
            let shield_fee = review.shield_fee_on_output(payout);
            assert!(!shield_fee.is_zero());
            assert_eq!(total_cost(&review), unshield_fee + allowance + shield_fee);
        }
    });
}

/// With gas at 40% of the swap, Balanced authorizes little and Loose needs Swap anyway, which
/// another share clears.
#[gpui::test]
fn loose_needs_swap_anyway_where_balanced_does_not(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        let first = ready_review(swaps, runtime, cx);
        let review = requote_at_gas(swaps, &stubs, runtime, gas_price_for(&first, 4_000), cx);
        assert!(authorized_high_cost(&review).is_none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-high-costs").is_none());
        let loose = cx.debug_bounds("swap-gas-loose").unwrap();
        cx.simulate_click(loose.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-high-costs").is_some());
        let loose = swaps.update(cx, |swaps, _| {
            let form = swaps.form.as_mut().unwrap();
            form.price_acknowledged = true;
            let QuoteState::Ready(loose) = &form.quote else {
                panic!("the quote priced at Loose");
            };
            let loose = Arc::clone(loose);
            assert!(authorized_high_cost(&loose).is_some());
            assert_eq!(
                form.review_problem(&loose)
                    .map(|problem| problem.to_string()),
                Some("Confirm Swap anyway to accept the high swap costs.".to_owned())
            );
            loose
        });
        let checkbox = cx.debug_bounds("swap-costs-acknowledged").unwrap();
        cx.simulate_click(checkbox.center(), gpui::Modifiers::none());
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                assert!(
                    swaps
                        .form
                        .as_ref()
                        .unwrap()
                        .review_problem(&loose)
                        .is_none()
                );
                swaps.set_gas_preset(GasPreset::Balanced, window, cx);
                assert!(!swaps.form.as_ref().unwrap().high_costs_acknowledged);
            });
        });
    });
}

/// With gas at six times the swap, Balanced leaves no minimum and the quote is priced at Tight.
/// The form keeps Balanced, selects no preset and refuses the review, and a quote at lower gas
/// is Balanced again. Choosing Higher makes the Tight quote the user's, with its Swap anyway.
#[gpui::test]
fn a_quote_that_falls_back_to_tight_waits_for_a_chosen_minimum(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        let first = ready_review(swaps, runtime, cx);
        let high = gas_price_for(&first, 60_000);
        let fell_back = |cx: &mut gpui::VisualTestContext| {
            let review = requote_at_gas(swaps, &stubs, runtime, high, cx);
            assert_eq!(review.gas_share_bps(), GAS_SHARE_TIGHT_BPS);
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert!(cx.debug_bounds("swap-gas-too-high").is_some());
            assert!(cx.debug_bounds("swap-high-costs").is_none());
            swaps.update(cx, |swaps, _| {
                let form = swaps.form.as_mut().unwrap();
                form.price_acknowledged = true;
                assert_eq!(
                    (form.gas_share_bps, form.gas_custom),
                    (GAS_SHARE_BALANCED_BPS, false)
                );
                assert_eq!(form.selected_gas_preset(Some(&review)), None);
                assert!(form.review_problem(&review).is_some());
            });
        };
        fell_back(cx);

        let low = gas_price_for(&first, 1_000);
        let review = requote_at_gas(swaps, &stubs, runtime, low, cx);
        assert_eq!(review.gas_share_bps(), GAS_SHARE_BALANCED_BPS);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-gas-too-high").is_none());
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(
                form.selected_gas_preset(Some(&review)),
                Some(GasPreset::Balanced)
            );
        });

        fell_back(cx);
        let tight = cx.debug_bounds("swap-gas-tight").unwrap();
        cx.simulate_click(tight.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-gas-too-high").is_none());
        let checkbox = cx.debug_bounds("swap-costs-acknowledged").unwrap();
        cx.simulate_click(checkbox.center(), gpui::Modifiers::none());
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            let QuoteState::Ready(review) = &form.quote else {
                panic!("the quote stays ready");
            };
            assert_eq!(
                form.selected_gas_preset(Some(review)),
                Some(GasPreset::Tight)
            );
            assert!(form.review_problem(review).is_none());
        });
    });
}

/// When even Tight leaves no positive minimum, quoting stops with the sell-amount error, and
/// the chosen share stays for the next quote.
#[gpui::test]
fn every_preset_leaving_nothing_shows_the_sell_amount_error(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        let first = ready_review(swaps, runtime, cx);
        stubs.set_gas_price_wei(gas_price_for(&first, 150_000));
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| swaps.schedule_quote(window, cx));
        });
        drive_until(cx, runtime, |cx| {
            swaps.read_with(cx, |swaps, _| {
                !matches!(swaps.form.as_ref().unwrap().quote, QuoteState::Loading)
            })
        });
        swaps.read_with(cx, |swaps, _| {
            let form = swaps.form.as_ref().unwrap();
            let QuoteState::Failed(error) = &form.quote else {
                panic!("the quote fails");
            };
            assert!(matches!(
                error.downcast_ref::<OrderLimitError>(),
                Some(OrderLimitError::HookCostExceedsOutput { gas_estimate, best_case, .. })
                    if *gas_estimate > *best_case * U256::from(10)
            ));
            assert_eq!(form.gas_share_bps, GAS_SHARE_BALANCED_BPS);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-price-error").is_some());
    });
}

/// The gas strip's info button opens its explanation from the keyboard; Escape closes it and
/// returns focus to the button. The explanation has the bar's legend only while the bar shows.
#[gpui::test]
fn gas_help_opens_from_its_button_and_escape_returns_focus(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        ready_review(swaps, runtime, cx);
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(!swaps.read_with(cx, |swaps, _| swaps.form.as_ref().unwrap().gas_bar_open()));
        let info = cx.debug_bounds("swap-gas-help-trigger").unwrap();
        cx.simulate_click(info.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-gas-help-content").is_some());
        assert!(cx.debug_bounds("swap-gas-help-legend").is_none());
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-gas-help-content").is_none());
        focus_gas_bar(swaps, cx);
        // The info button, the three presets and the edit button precede the bar in the tab
        // order.
        cx.update(|window, cx| {
            for _ in 0..5 {
                window.focus_prev(cx);
            }
            window.draw(cx).clear(cx);
        });
        let trigger = cx.update(|window, cx| window.focused(cx)).unwrap();
        swaps.read_with(cx, |swaps, _| {
            assert_ne!(trigger, swaps.form.as_ref().unwrap().gas_bar_focus);
        });
        cx.simulate_keystrokes("enter");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-gas-help-content").is_some());
        assert!(cx.debug_bounds("swap-gas-help-legend").is_some());
        assert!(swaps.read_with(cx, |swaps, _| swaps.form.as_ref().unwrap().gas_help_open));
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-gas-help-content").is_none());
        assert!(!swaps.read_with(cx, |swaps, _| swaps.form.as_ref().unwrap().gas_help_open));
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(trigger));
        assert!(
            swaps.read_with(cx, |swaps, _| swaps.form.is_some()),
            "the form stays open"
        );
    });
}

/// A Private delivery review leads with the guaranteed minimum, states the gas in money, and
/// shows the signed buy amount and the gas price in its order terms. What the order publishes,
/// when it expires unfilled and what a triggered unshield costs to recover stay in the review.
#[gpui::test]
fn private_review_leads_with_the_minimum_and_states_the_gas(cx: &mut TestAppContext) {
    let stubs = SwapStubs::start();
    with_swap_view_and_rpc(cx, Some(stubs.rpc()), |root, swaps, _, _, runtime, cx| {
        open_private_form(root, swaps, &stubs, runtime, cx);
        let first = ready_review(swaps, runtime, cx);
        let review = requote_at_gas(swaps, &stubs, runtime, gas_price_for(&first, 1_000), cx);
        swaps.read_with(cx, |swaps, cx| {
            let summary = swaps.swap_summary(&review, None, None, None, None, cx);
            let minimum = review.suggested_private_minimum();
            let (label, amount, lines) = summary.receive_card_for_test().unwrap();
            assert_eq!(
                (label, amount),
                (
                    "Receive at least".to_owned(),
                    swaps.token_amount(STUB_USDT, minimum, cx)
                )
            );
            assert_eq!(
                summary.send_card_for_test().map(|(label, _)| label),
                Some("Sell".to_owned()),
                "a swap on one network doesn't name it"
            );
            assert_eq!(
                lines.last().map(String::as_str),
                Some("to your private balance")
            );
            assert_eq!(summary.receiver_for_test(), None);
            assert_eq!(summary.rows_for_test()[0], gas_row(swaps, &review, cx));
            assert!(
                summary.row_group_for_test().is_none(),
                "a single cost row isn't grouped"
            );
            let gas = summary.row_hint_for_test("Gas").unwrap();
            assert!(
                gas.contains(
                    "If no solver covers the rest within 10 minutes, the order expires and nothing is swapped."
                ),
                "{gas}"
            );
            assert!(summary.details_note_for_test().is_some_and(|note| {
                note.contains("recovering the tokens costs the unshield and shield fees")
            }));
            let (_, public, warns) = summary.disclosure_for_test().unwrap();
            assert!(
                !warns && public.starts_with("Placing the order publishes its tokens, amounts"),
                "{public}"
            );
            let signed = review.buy_amount_for(minimum).unwrap();
            assert!(
                signed > minimum,
                "the order signs the amount before the shield fee"
            );
            let details = summary.details_for_test();
            for row in [
                (
                    "Signed minimum",
                    format!(
                        "{}, before the shield fee",
                        swaps.token_amount(STUB_USDT, signed, cx)
                    ),
                ),
                (
                    "Railgun unshield",
                    swaps.token_amount(
                        STUB_USDC,
                        review.plan().amount() - review.sell_amount(),
                        cx,
                    ),
                ),
                (
                    "Gas estimate",
                    format!(
                        "≈ {} at {} gwei",
                        swaps.gas_money(STUB_USDT, review.gas_estimate(), cx),
                        format_gwei(review.gas_price_wei())
                    ),
                ),
                ("Price tolerance", "0.5%".to_owned()),
                ("Order valid for", "10 minutes".to_owned()),
            ] {
                assert!(details.contains(&(row.0.to_owned(), row.1)), "{details:?}");
            }
            // The stub quote states no protocol fee.
            assert!(details.iter().all(|(label, _)| label != "CoW fee"));
        });
    });
}

/// The stub RPC gas price at which the gas estimate is about `fraction_bps` of the best case,
/// from a `review` quoted at 1 wei, whose cushioned price of 2 wei values one buy-token base
/// unit at one wei.
fn gas_price_for(review: &SwapReview, fraction_bps: u64) -> u64 {
    let units = review.gas_estimate() / U256::from(2);
    let target = review.best_case() * U256::from(fraction_bps) / U256::from(10_000);
    // A price of 4k wei is cushioned to 5k.
    (target / (units * U256::from(5))).saturating_to::<u64>() * 4
}

/// The form's quote again, once the stub RPC reports `wei` as its gas price.
fn requote_at_gas(
    swaps: &Entity<PrivateSwapsView>,
    stubs: &SwapStubs,
    runtime: &tokio::runtime::Runtime,
    wei: u64,
    cx: &mut gpui::VisualTestContext,
) -> Arc<SwapReview> {
    stubs.set_gas_price_wei(wei);
    cx.update(|window, cx| {
        swaps.update(cx, |swaps, cx| swaps.schedule_quote(window, cx));
    });
    ready_review(swaps, runtime, cx)
}

/// Open the gas bar with the edit button unless it is open, focus it, as a click or Tab does,
/// and draw the form.
fn focus_gas_bar(swaps: &Entity<PrivateSwapsView>, cx: &mut gpui::VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    if !swaps.read_with(cx, |swaps, _| swaps.form.as_ref().unwrap().gas_bar_open()) {
        let edit = cx.debug_bounds("swap-gas-edit-minimum").unwrap();
        cx.simulate_click(edit.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        let focus = swaps.read(cx).form.as_ref().unwrap().gas_bar_focus.clone();
        focus.focus(window, cx);
        window.draw(cx).clear(cx);
    });
}
