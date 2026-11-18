use super::*;

#[gpui::test]
fn swap_cost_confirmation_wraps_and_is_cleared_when_terms_change(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(Address::repeat_byte(1), window, cx);
            });
            window.close_all_dialogs(cx);
            let swaps = swaps.clone();
            // Mount the production warning with an authorized-cost message. Quote creation is
            // exercised elsewhere; this checks the control and its consent lifetime.
            window.open_wallet_dialog(cx, move |dialog, _, cx| {
                let warning = swaps.update(cx, |swaps, cx| {
                    PrivateSwapsView::render_cost_acknowledgement(
                        swaps.form.as_ref().unwrap(),
                        Some(AuthorizedCostWarning {
                            headline: "37% of this swap may go to costs.".to_owned(),
                            details: "Up to $0.03 for source gas, a $0.10 bridge fee, and $0.85 for destination gas and shielding.".to_owned(),
                        }),
                        true,
                        cx,
                    )
                });
                dialog.w(gpui::px(360.)).children(warning)
            });
            window.set_rem_size(gpui::px(22.));
            window.draw(cx).clear(cx);
        });
        let alert = cx.debug_bounds("swap-high-costs").unwrap();
        let message = cx.debug_bounds("swap-high-cost-message").unwrap();
        let details = cx.debug_bounds("swap-high-cost-details").unwrap();
        let checkbox = cx.debug_bounds("swap-costs-acknowledged").unwrap();
        assert!(alert.size.width <= gpui::px(360.));
        assert!(
            details.top() >= message.bottom(),
            "cost details follow the headline"
        );
        assert!(
            details.left() >= alert.left() && details.right() <= alert.right(),
            "cost details wrap inside the alert"
        );
        assert!(
            checkbox.top() >= details.bottom(),
            "confirmation follows the full warning"
        );
        assert!(
            checkbox.bottom() <= alert.bottom()
                && checkbox.left() >= alert.left()
                && checkbox.right() <= alert.right(),
            "confirmation stays inside the alert"
        );
        assert!(!swaps.read_with(cx, |swaps, _| {
            swaps.form.as_ref().unwrap().high_costs_acknowledged
        }));
        cx.simulate_click(checkbox.center(), gpui::Modifiers::none());
        assert!(swaps.read_with(cx, |swaps, _| {
            swaps.form.as_ref().unwrap().high_costs_acknowledged
        }));
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.set_slippage(100, window, cx);
                assert!(
                    !swaps.form.as_ref().unwrap().high_costs_acknowledged,
                    "consent to the previous quote cannot carry across changed terms"
                );
                // Another gas share changes the costs the user authorizes.
                swaps.form.as_mut().unwrap().high_costs_acknowledged = true;
                swaps.set_gas_share(wallet_ops::cow::GAS_SHARE_LOOSE_BPS, false, window, cx);
                assert!(!swaps.form.as_ref().unwrap().high_costs_acknowledged);
            });
        });
    });
}

#[gpui::test]
fn swap_max_starts_a_quote_for_the_exact_available_amount(cx: &mut TestAppContext) {
    use alloy::primitives::address;

    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
    let available = U256::from(305_133);
    with_swap_view(cx, |root, swaps, _, _, _, cx| {
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
                    None,
                    None,
                    SwapDelivery::Reshield,
                    window,
                    cx,
                );
                assert!(matches!(
                    swaps.form.as_ref().unwrap().quote,
                    QuoteState::Idle
                ));
            });
            window.close_all_dialogs(cx);
            let swaps = swaps.clone();
            // Feed the real Sell panel a spendable balance without starting private sync.
            window.open_wallet_dialog(cx, move |dialog, _, cx| {
                let panel = swaps.update(cx, |swaps, cx| {
                    swaps.render_sell_panel(
                        swaps.form.as_ref().unwrap(),
                        &[UnshieldAsset {
                            chain_id: 1,
                            token: usdc,
                            label: "USDC".into(),
                            decimals: Some(6),
                            total: available,
                            poi_verified_total: available,
                            max_batched: available,
                            icon_path: None,
                        }],
                        true,
                        false,
                        cx,
                    )
                });
                dialog.child(panel)
            });
            window.draw(cx).clear(cx);
        });
        let max_button = cx.debug_bounds("swap-amount-max").unwrap();
        cx.simulate_click(max_button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            let form = swaps.form.as_ref().unwrap();
            assert_eq!(swaps.form_amount(form, cx).unwrap(), available);
            assert!(
                matches!(form.quote, QuoteState::Loading),
                "Max starts quoting"
            );
            assert!(form.quote_task.is_some());
        });
    });
}

#[gpui::test]
fn buy_asset_picker_searches_and_keeps_selection_in_sync(cx: &mut TestAppContext) {
    use alloy::primitives::address;

    let usdc = address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let dai = address!("6b175474e89094c44da98b954eedeac495271d0f");
    with_swap_view(cx, |root, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            root.update(cx, |root, _| {
                root.effective_token_registry =
                    wallet_ops::settings::build_effective_token_registry(
                        &wallet_ops::settings::WalletSettings::default(),
                    )
                    .unwrap();
            });
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(usdc, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let previous_focus = cx.update(|window, cx| window.focused(cx).unwrap());
        let trigger = cx.debug_bounds("swap-buy-selector").unwrap();
        cx.simulate_click(trigger.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-buy-picker").is_some());
        assert!(
            cx.debug_bounds("dialog-1").is_some(),
            "the picker is a modal above the form"
        );
        for (width, height, rem) in [(1600., 900., 16.), (720., 600., 20.)] {
            cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(height)));
            cx.update(|window, cx| {
                window.set_rem_size(gpui::px(rem));
                window.draw(cx).clear(cx);
            });
            let picker = cx.debug_bounds("swap-buy-picker").unwrap();
            let tokens = cx.debug_bounds("swap-buy-picker-tokens").unwrap();
            let networks = cx.debug_bounds("swap-buy-picker-networks").unwrap();
            assert!((picker.center().x - gpui::px(width / 2.)).abs() < gpui::px(1.));
            assert!(picker.left() >= gpui::px(0.) && picker.right() <= gpui::px(width));
            assert!(picker.bottom() <= gpui::px(height));
            for pane in [tokens, networks] {
                assert!(pane.size.height > gpui::px(0.) && pane.bottom() <= picker.bottom());
                assert!(pane.left() >= picker.left() && pane.right() <= picker.right());
            }
        }
        cx.simulate_input("DAI");
        cx.run_until_parked();
        swaps.read_with(cx, |swaps, cx| {
            let tokens = swaps.form.as_ref().unwrap().picker.tokens.read(cx);
            assert_eq!(tokens.delegate().listed().next(), Some(dai));
            assert_eq!(
                tokens.selected_index(),
                Some(gpui_component::IndexPath::default()),
                "the first match"
            );
        });
        // Enter in the search picks the selected token and closes the picker.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("swap-buy-picker").is_none());
        assert!(cx.update(|window, _| previous_focus.is_focused(window)));
        // Escape from either search closes only the picker, leaving the swap form and its
        // selected asset intact. The dialog restores the previous focus.
        for from_networks in [false, true] {
            let trigger = cx.debug_bounds("swap-buy-selector").unwrap();
            cx.simulate_click(trigger.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                let picker = &swaps.read(cx).form.as_ref().unwrap().picker;
                assert!(
                    picker.tokens.read(cx).delegate().listed().count() > 1,
                    "the search starts empty"
                );
                if from_networks {
                    let focus = picker.networks.read(cx).focus_handle(cx);
                    focus.focus(window, cx);
                } else {
                    assert!(picker.tokens.read(cx).focus_handle(cx).is_focused(window));
                }
            });
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(previous_focus.is_focused(window));
                assert!(window.has_active_dialog(cx), "the swap dialog stays open");
            });
            assert!(cx.debug_bounds("swap-buy-picker").is_none());
            swaps.read_with(cx, |swaps, _| {
                let form = swaps.form.as_ref().unwrap();
                assert!(!form.picker.open);
                assert_eq!(form.buy, Some(dai));
            });
        }
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!((form.network, form.buy), (None, Some(dai)));
                assert!(!form.picker.open);

                swaps.flip_tokens(window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!((form.sell, form.buy), (dai, Some(usdc)));

                swaps.set_form_sell(usdc, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(form.buy.is_none());

                // The picker lists what the wallet holds first, by value, then the rest by
                // symbol. WETH has a price, which a zero balance doesn't show.
                let cache = &root.read(cx).public_broadcaster_anchor_cache;
                cache.store_native_usd_rate(1, U256::from(3_000_000_000_u64), 18);
                cache.store_rate(1, dai, U256::from(3_000_000_000_000_000_000_000_u128));
                swaps.form.as_mut().unwrap().assets.totals = vec![
                    (STUB_USDT, U256::from(5_000_000)),
                    (dai, U256::from(1_000_000_000_000_000_000_000_u128)),
                ];
                let form = swaps.form.as_ref().unwrap();
                let buy_picker::BuyPickerTokens::Listed(rows) =
                    swaps.buy_picker_content(form, cx).tokens
                else {
                    panic!("the swap's own network lists its tokens");
                };
                assert_eq!(
                    rows.iter()
                        .take(2)
                        .map(|row| (row.item.asset.token, row.usd.is_some()))
                        .collect::<Vec<_>>(),
                    [(dai, true), (STUB_USDT, false)],
                    "a priced balance, then a balance without a price"
                );
                // Of the tokens the wallet doesn't hold, the network's own asset and its
                // wrapped token lead, and the rest follow by symbol.
                let wrapped = root
                    .read(cx)
                    .effective_chain_configs
                    .get(1)
                    .and_then(|chain| chain.wrapped_native_token);
                let rest = rows[2..]
                    .iter()
                    .map(|row| {
                        let rank = match row.item.asset.token {
                            Address::ZERO => 0_u8,
                            token if Some(token) == wrapped => 1,
                            _ => 2,
                        };
                        (rank, row.item.asset.label.to_lowercase())
                    })
                    .collect::<Vec<_>>();
                assert!(rest.is_sorted(), "{rest:?}");
                assert!(rest.first().is_some_and(|(rank, _)| *rank < 2), "{rest:?}");
                assert!(
                    rows[2..]
                        .iter()
                        .any(|row| row.item.asset.token == STUB_WETH && row.usd.is_none())
                );
            });
            window.draw(cx).clear(cx);
        });
    });
}

#[gpui::test]
fn swap_quote_error_wraps_within_its_column(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(Address::repeat_byte(1), window, cx);
            });
        });
        for error in [
            eyre::eyre!(
                "The CoW orderbook quote request to https://api.cow.fi/ failed: the connection closed before a response was received. Check your connection and try again."
            ),
            eyre::Report::from(wallet_ops::cow::OrderLimitError::HookCostExceedsOutput {
                buy_token: Address::repeat_byte(2),
                gas_estimate: U256::from(60_000_000),
                best_case: U256::from(49_500_000),
            })
            .wrap_err("Could not price the order"),
        ] {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let revision = swaps.form.as_ref().unwrap().quote_revision;
                    swaps.apply_quote(
                        None,
                        revision,
                        Some(QuoteResult {
                            orderbook: None,
                            bridge_clients: None,
                            bridge: None,
                            outcome: Err(error),
                        }),
                        window,
                        cx,
                    );
                });
            });
            for (width, font_size) in [(700., 16.), (600., 22.)] {
                cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(1100.)));
                cx.update(|window, cx| {
                    window.set_rem_size(gpui::px(font_size));
                    window.refresh();
                    window.draw(cx).clear(cx);
                });
                let status = cx.debug_bounds("swap-price-status").unwrap();
                let error = cx.debug_bounds("swap-price-error").unwrap();
                let retry = cx.debug_bounds("swap-price-retry").unwrap();
                assert!(
                    error.right() <= status.right(),
                    "the error must stay out of the balance column: {error:?}, {status:?}"
                );
                assert!(
                    retry.top() >= error.top() && retry.bottom() <= error.bottom(),
                    "Retry is inside the error: {retry:?}, {error:?}"
                );
                assert!(error.bottom() <= status.bottom());
            }
        }
    });
}

/// A pricing failure as the quote paths report it: the cause under the quote's values.
fn pricing_failure() -> eyre::Report {
    eyre::Report::from(OrderLimitError::QuotePriceNotDecimal).wrap_err(
        wallet_ops::SwapQuoteDiagnostics {
            chain_id: 56,
            sell_token: Address::repeat_byte(1),
            buy_token: Address::repeat_byte(2),
            sell_amount: U256::from(100),
            buy_amount: U256::from(99),
            sell_token_price: "abc".to_owned(),
            gas_amount: "163218".to_owned(),
            gas_price: "50000000".to_owned(),
            anchor: wallet_ops::SwapQuoteAnchor::NotConfigured,
        },
    )
}

/// A failed pricing quote offers its Details collapsed. Opened, they hold the quote's values
/// above Retry, and a retry closes them for the next failure.
#[gpui::test]
fn failed_pricing_quote_offers_details_that_a_retry_collapses(cx: &mut TestAppContext) {
    with_swap_view(cx, |_, swaps, _, _, _, cx| {
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(Address::repeat_byte(1), window, cx);
                let revision = swaps.form.as_ref().unwrap().quote_revision;
                swaps.apply_quote(
                    None,
                    revision,
                    Some(QuoteResult {
                        orderbook: None,
                        bridge_clients: None,
                        bridge: None,
                        outcome: Err(pricing_failure()),
                    }),
                    window,
                    cx,
                );
            });
            window.draw(cx).clear(cx);
        });
        let toggle = cx
            .debug_bounds("swap-price-error-details-toggle")
            .expect("a pricing failure offers Details");
        assert!(
            cx.debug_bounds("swap-price-error-details").is_none(),
            "Details start collapsed"
        );
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let error = cx.debug_bounds("swap-price-error").unwrap();
        let details = cx.debug_bounds("swap-price-error-details").unwrap();
        let copy = cx.debug_bounds("swap-price-error-details-copy").unwrap();
        let retry = cx.debug_bounds("swap-price-retry").unwrap();
        assert!(
            details.left() >= error.left() && details.right() <= error.right(),
            "the details wrap inside the alert: {details:?}, {error:?}"
        );
        assert!(
            copy.top() >= details.bottom() && retry.top() >= copy.bottom(),
            "Retry stays under the open details: {details:?}, {copy:?}, {retry:?}"
        );
        swaps.read_with(cx, |swaps, cx| {
            let QuoteState::Failed(error) = &swaps.form.as_ref().unwrap().quote else {
                panic!("the quote stays failed");
            };
            let shown = swaps.quote_failure(error, cx).details.unwrap();
            for field in [
                "cause: ",
                "chain: 56",
                "sell token: ",
                "buy token: ",
                "sell amount: 100",
                "buy amount: 99",
                "sell token price: abc",
                "gas amount: 163218",
                "gas price: 50000000",
                "price anchor: not configured",
            ] {
                assert!(shown.contains(field), "{field}: {shown}");
            }
        });
        cx.simulate_click(retry.center(), gpui::Modifiers::none());
        assert!(!swaps.read_with(cx, |swaps, _| {
            swaps.form.as_ref().unwrap().quote_details_open
        }));
    });
}

/// The Public form offers a pricing failure's Details too. An amount too small to bridge keeps
/// its own message, which has nothing behind it to show.
#[gpui::test]
fn public_quote_failure_offers_details_except_for_an_amount_too_small(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, _, _, _, cx| {
        let source = root.read_with(cx, |root, _| root.public_accounts[1].clone());
        let too_low = || {
            eyre::Report::from(BridgeApiError::AmountTooLow).wrap_err("Could not quote the bridge")
        };
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.open_public_form(source, STUB_USDT, window, cx);
            });
        });
        for (error, details) in [(pricing_failure(), true), (too_low(), false)] {
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    let form = swaps.form.as_mut().unwrap();
                    form.quote_task = None;
                    form.quote = QuoteState::Failed(error);
                    cx.notify();
                });
                window.draw(cx).clear(cx);
            });
            assert!(cx.debug_bounds("public-swap-quote-error").is_some());
            assert_eq!(
                cx.debug_bounds("public-swap-quote-error-details-toggle")
                    .is_some(),
                details
            );
        }
        let failure = public_source::public_quote_failure(&too_low(), Some(137), |_, amount| {
            amount.to_string()
        });
        assert_eq!(
            failure.message,
            format!(
                "This amount is too small for Across to bridge to {}. Try a larger amount.",
                network_name(137)
            )
        );
    });
}

#[gpui::test]
fn quote_retry_discards_the_old_route_and_ignores_its_late_response(cx: &mut TestAppContext) {
    with_swap_view(cx, |root, swaps, _, operation, runtime, cx| {
        let owner = swaps.read_with(cx, |swaps, _| Arc::clone(swaps.private_owner().unwrap()));
        let http = root.read_with(cx, |root, _| root.http.clone());
        let route = runtime.block_on(http.operation_http_client()).unwrap();
        let old_client = owner.swap_orderbook_client(&route).unwrap();
        for operation in [None, Some(operation)] {
            let mut old_revision = 0;
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    swaps.open_form(
                        operation,
                        Address::repeat_byte(1),
                        Some(Address::repeat_byte(2)),
                        Some(U256::ONE),
                        None,
                        SwapDelivery::Reshield,
                        window,
                        cx,
                    );
                    old_revision = swaps.form.as_ref().unwrap().quote_revision;
                    swaps.apply_quote(
                        operation,
                        old_revision,
                        Some(QuoteResult {
                            orderbook: Some(old_client.clone()),
                            bridge_clients: None,
                            bridge: None,
                            outcome: Err(eyre::eyre!("connection failed")),
                        }),
                        window,
                        cx,
                    );
                });
                window.draw(cx).clear(cx);
            });
            let retry = cx.debug_bounds("swap-price-retry").unwrap();
            cx.simulate_click(retry.center(), gpui::Modifiers::none());
            cx.update(|window, cx| {
                swaps.update(cx, |swaps, cx| {
                    // A superseded response cannot put the failed route back into either
                    // cache, including the operation cache used when resuming a swap.
                    swaps.apply_quote(
                        operation,
                        old_revision,
                        Some(QuoteResult {
                            orderbook: Some(old_client.clone()),
                            bridge_clients: None,
                            bridge: None,
                            outcome: Err(eyre::eyre!("late connection failure")),
                        }),
                        window,
                        cx,
                    );
                    let form = swaps.form.as_ref().unwrap();
                    assert!(matches!(form.quote, QuoteState::Loading));
                    assert!(form.orderbook.is_none(), "Retry must request a fresh route");
                    if let Some(operation) = operation {
                        assert!(
                            swaps.tracking[&operation].orderbook.is_none(),
                            "Retry must not fall back to the tracked route"
                        );
                    }
                });
            });
        }
    });
}

#[gpui::test]
fn private_swap_progress_preserves_other_dialogs_and_can_restart_a_retired_setup(
    cx: &mut TestAppContext,
) {
    with_swap_view(cx, |_, swaps, executors, operation, _, cx| {
        let rendered = Rc::new(Cell::new(false));
        cx.update(|window, cx| {
            let rendered = rendered.clone();
            window.open_wallet_dialog(cx, move |dialog, _, _| {
                rendered.set(true);
                dialog.title("Unrelated form").child("Keep this form open")
            });
            window.draw(cx).clear(cx);
        });
        rendered.set(false);
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.finish_order(
                    operation,
                    wallet_ops::SwapOrderOutcome::Replan {
                        byte_budget: 1_000,
                        attempt_recorded: true,
                    },
                    window,
                    cx,
                );
            });
            window.refresh();
            window.draw(cx).clear(cx);
            assert!(window.has_active_dialog(cx));
        });
        assert!(
            rendered.get(),
            "the unrelated dialog still renders after the swap completes"
        );
        // A preparation may retire an account after detecting prior chain activity. A retry
        // must open a fresh swap review instead of resubmitting that unusable reservation.
        // This swap has no saved approval for a new account to continue under.
        executors.retire(operation).unwrap();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                swaps.reload_records();
                let stage = swaps.stage(swaps.record(operation).unwrap());
                assert_eq!(stage, model::SwapStage::SetupRetired);
                let actions = model::swap_actions(stage, false);
                assert!(actions.resume.is_some_and(|available| available.is_ok()));
                // Ended, it leaves the Private tab by itself.
                assert!(!actions.recover && !actions.dismiss);
                swaps.resume_swap(operation, window, cx);
                assert!(swaps.setup_retry.is_none());
                assert!(swaps.form.as_ref().unwrap().operation().is_none());
                assert!(swaps.record(operation).unwrap().is_retired());
            });
            window.draw(cx).clear(cx);
        });
    });
}

#[gpui::test]
fn selected_account_swap_allows_changing_both_tokens_without_starting_setup(
    cx: &mut TestAppContext,
) {
    use alloy::eips::BlockNumHash;
    use alloy::primitives::B256;
    use wallet_ops::vault::ExecutorNonceObservation;
    with_swap_view(cx, |root, swaps, executors, operation, _, cx| {
        pending_setup(executors, operation);
        // The setup won its nonce, so a new swap can offer this account.
        executors
            .record_account_read(
                operation,
                ExecutorNonceObservation::new(
                    BlockNumHash::new(12, B256::repeat_byte(12)),
                    U256::ONE,
                ),
            )
            .unwrap();
        cx.update(|window, cx| {
            let target = crate::root::stealth_accounts::StealthAccountTarget::new(
                swaps.read(cx).private_session().unwrap(),
                operation,
            );
            root.update(cx, |root, cx| {
                root.open_stealth_account(&target, window, cx);
            });
            window.draw(cx).clear(cx);
        });
        let menu = cx
            .debug_bounds(format!("stealth-row-menu-{}", operation.opaque_id()).leak())
            .unwrap();
        cx.simulate_click(menu.center(), gpui::Modifiers::none());
        // The menu starts with Add to Public, then Use for swap.
        cx.simulate_keystrokes("down down enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps
                    .form
                    .as_ref()
                    .expect("the selected account opens a swap form");
                assert_eq!(form.operation, Some(operation));
                assert!(form.reuse_account);
                assert_eq!(swaps.form_mode(form), FormMode::Order);
                assert!(swaps.job.is_none());
                let select = form.account_select.as_ref().unwrap();
                assert_eq!(select.read(cx).selected_value(), Some(&Some(operation)));
                let sell = alloy::primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
                let buy = alloy::primitives::address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
                swaps.set_form_sell(sell, window, cx);
                swaps.set_form_buy(buy, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert_eq!((form.sell, form.buy), (sell, Some(buy)));
                assert_eq!(form.operation, Some(operation));
            });
            window.draw(cx).clear(cx);
            window.close_all_dialogs(cx);
            swaps.update(cx, |swaps, cx| {
                swaps.open_new_form(Address::repeat_byte(1), window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(form.operation.is_none() && !form.reuse_account);
                assert_eq!(swaps.form_mode(form), FormMode::Setup { resume: false });
                // A normally opened swap offers the set-up account.
                let select = form.account_select.clone().unwrap();
                select.update(cx, |select, cx| {
                    select.set_selected_value(&Some(operation), window, cx);
                    assert_eq!(select.selected_value(), Some(&Some(operation)));
                    cx.emit(
                        SelectEvent::<SearchableVec<SwapAccountSelectItem>>::Confirm(Some(Some(
                            operation,
                        ))),
                    );
                });
            });
            window.draw(cx).clear(cx);
        });
        cx.update(|window, cx| {
            swaps.update(cx, |swaps, cx| {
                let form = swaps.form.as_ref().unwrap();
                assert_eq!(form.operation, Some(operation));
                assert!(form.reuse_account);
                assert_eq!(swaps.form_mode(form), FormMode::Order);
                assert!(swaps.job.is_none());
                // Back to a new account: the swap is no longer bound to the chosen one.
                swaps.select_form_account(None, window, cx);
                let form = swaps.form.as_ref().unwrap();
                assert!(form.operation.is_none() && !form.reuse_account);
                assert_eq!(swaps.form_mode(form), FormMode::Setup { resume: false });
                assert_eq!(
                    form.account_select
                        .as_ref()
                        .unwrap()
                        .read(cx)
                        .selected_value(),
                    Some(&None)
                );
            });
        });
    });
}
